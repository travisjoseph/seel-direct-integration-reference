/**
 * Seel Worry-Free Purchase (WFP) opt-in widget, self-hosted.
 *
 * The API calls the program WFP, so element IDs and fields say "wfp" even
 * where the product is sold as Worry-Free Delivery.
 *
 * Exposes window.SeelSDK with the interface of Seel's hosted marketplace
 * bundle: createQuote(payload, cb), checkboxChange(cb),
 * updateCheckboxStatus(checked), onWidgetEvent(cb), destroy(), reset() and
 * SEEL_WIDGET_EVENT_TYPES. Two additions are specific to this build:
 * setMerchantId(id) and configure().
 *
 * The API key never reaches the browser. createQuote() POSTs the payload to
 * a backend proxy (config.quoteEndpoint) that attaches the key and forwards
 * to Seel. The hosted bundle calls Seel directly with a key embedded in the
 * script, which is the difference this build exists for. Reference proxies
 * live in ../server/.
 *
 * checkboxChange fires only when the opt-in state changes: a toggle, a
 * default-on first render, updateCheckboxStatus, or coverage turning
 * ineligible and back. Re-quotes hand the fresh price to the createQuote
 * callback, and a shopper's choice, in or out, survives re-quotes.
 */
(function () {
  "use strict";

  var MOUNT_ID = "seel-wfp-widget-root";
  var FETCH_TIMEOUT_MS = 10000;

  // The hosted bundle's names and values, limited to the events this build
  // emits so a listener never waits on one that cannot arrive.
  var SEEL_WIDGET_EVENT_TYPES = Object.freeze({
    CARD_CHECKBOX_CHANGE: "card_checkbox_change",
    CARD_INFO_CLICK: "card_info_click",
    CARD_EXPOSURE_VIEW: "card_exposure_view",
  });

  var config = {
    // Backend proxy that forwards to Seel's Quote API with the server-side key.
    quoteEndpoint: "/v1/ecommerce/quotes",
    // Override for testing: async function (payload) -> quote response object.
    quoteFetcher: null,
  };

  var merchantId = null;
  var checkboxHandler = null;
  var widgetEventHandler = null;

  var state;
  function freshState() {
    return {
      quote: null,
      checked: false,  // coverage currently applied to the order
      optIn: null,     // the shopper's last choice; null until they make one, then is_default_on no longer applies
      requestSeq: state ? state.requestSeq + 1 : 0, // guards against out-of-order quote responses
    };
  }
  state = freshState();

  function configure(opts) {
    opts = opts || {};
    if (opts.quoteEndpoint) config.quoteEndpoint = opts.quoteEndpoint;
    if (opts.quoteFetcher) config.quoteFetcher = opts.quoteFetcher;
  }

  function setMerchantId(id) {
    merchantId = id || null;
  }

  function defaultFetcher(payload) {
    var controller = typeof AbortController !== "undefined" ? new AbortController() : null;
    var timer = controller && setTimeout(function () { controller.abort(); }, FETCH_TIMEOUT_MS);
    return fetch(config.quoteEndpoint, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
      signal: controller ? controller.signal : undefined,
    }).then(function (res) {
      if (!res.ok) throw new Error("Quote request failed: " + res.status);
      return res.json();
    }).then(function (body) {
      if (timer) clearTimeout(timer);
      return body;
    }, function (err) {
      if (timer) clearTimeout(timer);
      throw err;
    });
  }

  function textList(v) {
    if (Array.isArray(v)) return v;
    return typeof v === "string" ? [v] : [];
  }

  function camelKeys(obj) {
    var out = {};
    Object.keys(obj || {}).forEach(function (k) {
      out[k.replace(/_([a-z0-9])/g, function (_, c) { return c.toUpperCase(); })] = obj[k];
    });
    return out;
  }

  /**
   * The quote handed to callbacks. Field names follow the hosted bundle:
   * status, quoteId, price, currencyCode, currencySymbol, eligibleItems and
   * extraInfo, with every extra_info key camelCased. coverages,
   * displayPrice, modalDetails and raw are additions this build renders from.
   */
  function normalizeQuote(q) {
    var extra = q.extra_info || {};
    var copy = q.widget_copy || {};
    var extraInfo = camelKeys(extra);
    extraInfo.displayWidgetText = textList(extra.display_widget_text || copy.widget_text);
    extraInfo.optOutWarningText = extra.opt_out_warning_text || "";
    extraInfo.coverageDetailsText = textList(extra.coverage_details_text);
    extraInfo.termsUrl = extra.terms_url || "";
    extraInfo.privacyPolicyUrl = extra.privacy_policy_url || "";
    extraInfo.isWidgetHidden = !!extra.is_widget_hidden;
    extraInfo.widgetTitle = extra.widget_title || copy.widget_title || "Worry-Free Delivery";
    return {
      status: q.status,
      quoteId: q.quote_id,
      price: q.price,
      currencyCode: q.currency || q.currency_code,
      currencySymbol: extra.currency_symbol || q.currency_symbol,
      i18nConfigTextsJson: extra.i18n_config,
      eligibleItems: (q.eligible_items || []).map(camelKeys),
      extraInfo: extraInfo,
      displayAmounts: q.display_amounts,
      widgetCopy: q.widget_copy,
      coverages: q.coverages || [],
      displayPrice: (q.display_amounts && q.display_amounts.price) || String(q.price),
      modalDetails: Array.isArray(copy.modal_details_text) ? copy.modal_details_text : [],
      raw: q,
    };
  }

  /**
   * Is this quote something to show the shopper?
   *
   * A quote can be "accepted" and still carry no coverage - an unpriced
   * market returns price 0.0 with an empty coverages array rather than a
   * rejection. Checking status alone would render a 0.00 offer.
   */
  function isOffer(quote) {
    return (
      quote.status === "accepted" &&
      !quote.extraInfo.isWidgetHidden &&
      quote.coverages.length > 0
    );
  }

  function safeCall(fn, args, label) {
    if (typeof fn !== "function") return;
    try {
      fn.apply(null, args);
    } catch (e) {
      console.error("[SeelSDK] " + label + " error", e);
    }
  }

  function emit(type) {
    safeCall(widgetEventHandler, [{ type: type, quote: state.quote, checked: state.checked }], "onWidgetEvent");
  }

  function setChecked(checked, byShopper) {
    if (byShopper) state.optIn = checked;
    if (state.checked === checked) return;
    state.checked = checked;
    var box = document.getElementById("seel-wfp-checkbox");
    if (box) box.checked = checked;
    var warning = document.getElementById("seel-wfp-optout-warning");
    if (warning) warning.style.display = checked ? "none" : "block";
    safeCall(checkboxHandler, [state.quote, checked], "checkboxChange callback");
    emit(SEEL_WIDGET_EVENT_TYPES.CARD_CHECKBOX_CHANGE);
  }

  function escapeHtml(s) {
    return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }

  // API-supplied URLs are rendered only with an explicit http(s) scheme.
  function safeUrl(url) {
    return /^https?:\/\//i.test(url) ? url : "";
  }

  function clearMount() {
    var mount = document.getElementById(MOUNT_ID);
    if (mount) mount.innerHTML = "";
  }

  function render(quote, checked) {
    var mount = document.getElementById(MOUNT_ID);
    if (!mount) {
      console.warn("[SeelSDK] mounting div #" + MOUNT_ID + " not found");
      return;
    }
    // An unconfigured market does not come back rejected: the quote is
    // "accepted" with price 0 and coverages empty. Rendering that shows the
    // shopper a free offer that covers nothing, so treat it as no offer.
    if (!isOffer(quote)) {
      mount.innerHTML = "";
      return;
    }

    var detailsRows = quote.modalDetails.length
      ? quote.modalDetails
          .map(function (d) {
            return (
              '<div style="margin:2px 0"><strong>' +
              escapeHtml(d.category) +
              ":</strong> " +
              escapeHtml(d.description) +
              "</div>"
            );
          })
          .join("")
      : quote.extraInfo.coverageDetailsText
          .map(function (t) {
            return '<div style="margin:2px 0">' + escapeHtml(t) + "</div>";
          })
          .join("");

    var termsUrl = safeUrl(quote.extraInfo.termsUrl);
    var privacyUrl = safeUrl(quote.extraInfo.privacyPolicyUrl);
    var links = [];
    if (termsUrl) links.push('<a href="' + escapeHtml(termsUrl) + '" target="_blank" rel="noopener">Terms</a>');
    if (privacyUrl) links.push('<a href="' + escapeHtml(privacyUrl) + '" target="_blank" rel="noopener">Privacy</a>');

    mount.innerHTML =
      '<div style="border:1px solid #d9d9d9;border-radius:8px;padding:12px;font-family:inherit;font-size:14px;line-height:1.4">' +
      '<label style="display:flex;gap:10px;align-items:flex-start;cursor:pointer">' +
      '<input type="checkbox" id="seel-wfp-checkbox"' + (checked ? " checked" : "") + ' style="margin-top:3px">' +
      "<span>" +
      "<strong>" + escapeHtml(quote.extraInfo.widgetTitle) + "</strong> " +
      "<span>" + escapeHtml(quote.displayPrice) + "</span>" +
      '<div style="color:#555">' + quote.extraInfo.displayWidgetText.map(escapeHtml).join("<br>") + "</div>" +
      "</span>" +
      "</label>" +
      (detailsRows
        ? '<details id="seel-wfp-details" style="margin-top:6px;color:#555"><summary style="cursor:pointer">What’s covered</summary>' + detailsRows + "</details>"
        : "") +
      (links.length
        ? '<div style="margin-top:6px;font-size:12px">' + links.join(" · ") + "</div>"
        : "") +
      (quote.extraInfo.optOutWarningText
        ? '<div id="seel-wfp-optout-warning" style="display:' + (checked ? "none" : "block") + ';margin-top:6px;color:#b45309;font-size:12px">' + escapeHtml(quote.extraInfo.optOutWarningText) + "</div>"
        : "") +
      "</div>";

    var box = document.getElementById("seel-wfp-checkbox");
    box.addEventListener("change", function () {
      setChecked(box.checked, true);
    });
    var details = document.getElementById("seel-wfp-details");
    if (details) {
      details.addEventListener("toggle", function () {
        if (details.open) emit(SEEL_WIDGET_EVENT_TYPES.CARD_INFO_CLICK);
      });
    }
  }

  /**
   * payload is the Seel quote payload: line_items, shipping_address,
   * customer, client_ip, is_default_on. Full field reference:
   * https://developer.seel.com/reference/createquote
   *
   * callback(quote, checkboxStatus) on success, where checkboxStatus is
   * whether coverage is applied after this quote. callback(null, null,
   * { code, message }) on failure, code INVALID_PAYLOAD or QUOTE_FAILED.
   * The returned Promise resolves to the quote, or null when the call
   * failed or a newer call superseded it, and never rejects.
   *
   * Call it on cart load and on every cart change - address, discount, item
   * removed. Out-of-order responses are discarded without a callback, so
   * rapid successive calls are safe.
   */
  function createQuote(payload, callback) {
    function reply(quote, checked, error) {
      safeCall(callback, error ? [null, null, error] : [quote, checked], "createQuote callback");
    }
    if (!payload || typeof payload !== "object") {
      reply(null, null, { code: "INVALID_PAYLOAD", message: "createQuote requires a cart payload object" });
      return Promise.resolve(null);
    }
    var body = payload;
    if (merchantId && !payload.merchant_id) {
      body = Object.assign({}, payload, { merchant_id: merchantId });
    }
    var seq = ++state.requestSeq;
    var fetcher = config.quoteFetcher || defaultFetcher;
    return Promise.resolve()
      .then(function () { return fetcher(body); })
      .then(function (resp) {
        if (seq !== state.requestSeq) return null;
        var quote = normalizeQuote(resp);
        state.quote = quote;
        var wantsIn = state.optIn === null ? !!payload.is_default_on : state.optIn;
        var checked = isOffer(quote) && wantsIn;
        render(quote, checked);
        setChecked(checked);
        if (isOffer(quote)) emit(SEEL_WIDGET_EVENT_TYPES.CARD_EXPOSURE_VIEW);
        reply(quote, checked);
        return quote;
      }, function (err) {
        if (seq !== state.requestSeq) return null;
        console.error("[SeelSDK] createQuote failed", err);
        state.quote = null;
        clearMount();
        setChecked(false); // no valid quote means no coverage fee on the order
        reply(null, null, { code: "QUOTE_FAILED", message: String((err && err.message) || err) });
        return null;
      });
  }

  // One handler at a time, replaced on each call, as in the hosted bundle.
  function checkboxChange(handler) {
    checkboxHandler = typeof handler === "function" ? handler : null;
  }

  function onWidgetEvent(handler) {
    widgetEventHandler = typeof handler === "function" ? handler : null;
  }

  /**
   * Set the opt-in from platform code, e.g. restoring a choice saved on the
   * cart. It counts as the shopper's choice, so it survives re-quotes. With
   * no offer on screen, coverage stays off.
   */
  function updateCheckboxStatus(checked) {
    checked = !!checked;
    state.optIn = checked;
    setChecked(checked && !!state.quote && isOffer(state.quote));
  }

  // Clears the widget for a new cart. Handlers and the merchant ID stay.
  function reset() {
    state = freshState();
    clearMount();
    return Promise.resolve();
  }

  function destroy() {
    checkboxHandler = null;
    widgetEventHandler = null;
    merchantId = null;
    return reset();
  }

  window.SeelSDK = {
    createQuote: createQuote,
    checkboxChange: checkboxChange,
    updateCheckboxStatus: updateCheckboxStatus,
    onWidgetEvent: onWidgetEvent,
    destroy: destroy,
    reset: reset,
    SEEL_WIDGET_EVENT_TYPES: SEEL_WIDGET_EVENT_TYPES,
    setMerchantId: setMerchantId,
    configure: configure,
  };
})();
