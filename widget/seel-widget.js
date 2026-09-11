/**
 * Seel Worry-Free Purchase (WFP) opt-in widget, self-hosted.
 *
 * The API calls the program WFP, so element IDs and fields say "wfp" even
 * where the product is sold as Worry-Free Delivery.
 *
 * Exposes window.SeelSDK: createQuote(), onCheck(), onUncheck() - the same
 * interface as Seel's hosted bundle (developer.seel.com) - plus configure(),
 * which is specific to this build. Swapping to a hosted bundle later is a
 * script-src change.
 *
 * The API key never reaches the browser. createQuote() POSTs the params to
 * a backend proxy (config.quoteEndpoint) that attaches the key and forwards
 * to Seel. Reference proxies live in ../server/.
 *
 * onCheck/onUncheck fire only when the opt-in state changes: a toggle, a
 * default-on first render, or coverage turning ineligible. Re-quotes hand
 * the fresh price to the createQuote callback, and a shopper's opt-out
 * survives re-quotes.
 */
(function () {
  "use strict";

  var MOUNT_ID = "seel-wfp-widget-root";
  var FETCH_TIMEOUT_MS = 10000;

  var config = {
    // Backend proxy that forwards to Seel's Quote API with the server-side key.
    quoteEndpoint: "/v1/ecommerce/quotes",
    // Override for testing: async function (params) -> quote response object.
    quoteFetcher: null,
  };

  var state = {
    quote: null,
    checked: false,
    userChose: false, // once the shopper toggles, is_default_on no longer applies
    requestSeq: 0,    // guards against out-of-order quote responses
    checkHandlers: [],
    uncheckHandlers: [],
  };

  function configure(opts) {
    opts = opts || {};
    if (opts.quoteEndpoint) config.quoteEndpoint = opts.quoteEndpoint;
    if (opts.quoteFetcher) config.quoteFetcher = opts.quoteFetcher;
  }

  function defaultFetcher(params) {
    var controller = typeof AbortController !== "undefined" ? new AbortController() : null;
    var timer = controller && setTimeout(function () { controller.abort(); }, FETCH_TIMEOUT_MS);
    return fetch(config.quoteEndpoint, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(params),
      signal: controller ? controller.signal : undefined,
    }).then(function (res) {
      if (timer) clearTimeout(timer);
      if (!res.ok) throw new Error("Quote request failed: " + res.status);
      return res.json();
    }, function (err) {
      if (timer) clearTimeout(timer);
      throw err;
    });
  }

  /**
   * The quoteData passed to callbacks: camelCase keys, raw API response
   * on .raw.
   */
  function normalizeQuote(q) {
    var extra = q.extra_info || {};
    var copy = q.widget_copy || {};
    return {
      quoteId: q.quote_id,
      status: q.status,
      price: q.price,
      currencyCode: q.currency_code || q.currencyCode,
      currencySymbol: q.currency_symbol || q.currencySymbol,
      displayPrice: (q.display_amounts && q.display_amounts.price) || String(q.price),
      eligibleItems: q.eligible_items || [],
      coverages: q.coverages || [],
      extraInfo: {
        displayWidgetText: extra.display_widget_text || copy.widget_text || [],
        optOutWarningText: extra.opt_out_warning_text || "",
        coverageDetailsText: extra.coverage_details_text || [],
        termsUrl: extra.terms_url || "",
        privacyPolicyUrl: extra.privacy_policy_url || "",
        isWidgetHidden: !!extra.is_widget_hidden,
        widgetTitle: extra.widget_title || copy.widget_title || "Worry-Free Delivery",
      },
      modalDetails: copy.modal_details_text || [],
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

  function fire(handlers, quote) {
    handlers.forEach(function (h) {
      try {
        h(quote);
      } catch (e) {
        console.error("[SeelSDK] callback error", e);
      }
    });
  }

  function setChecked(checked, byUser) {
    if (byUser) state.userChose = true;
    if (state.checked === checked) return;
    state.checked = checked;
    fire(checked ? state.checkHandlers : state.uncheckHandlers, state.quote);
    var warning = document.getElementById("seel-wfp-optout-warning");
    if (warning) warning.style.display = checked ? "none" : "block";
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
        ? '<details style="margin-top:6px;color:#555"><summary style="cursor:pointer">What’s covered</summary>' + detailsRows + "</details>"
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
  }

  /**
   * quoteParams is the Seel quote payload: line_items, shipping_address,
   * customer, client_ip, is_default_on. Full field reference:
   * https://developer.seel.com/reference/createquote
   *
   * Call it on cart load and on every cart change - address, discount, item
   * removed. Out-of-order responses are discarded, so rapid successive
   * calls are safe.
   */
  function createQuote(quoteParams, callback) {
    var seq = ++state.requestSeq;
    var fetcher = config.quoteFetcher || defaultFetcher;
    return Promise.resolve(fetcher(quoteParams))
      .then(function (resp) {
        if (seq !== state.requestSeq) return state.quote; // superseded by a newer call
        var quote = normalizeQuote(resp);
        state.quote = quote;
        var eligible = isOffer(quote);
        var checked = eligible && (state.userChose ? state.checked : !!quoteParams.is_default_on);
        render(quote, checked);
        setChecked(checked);
        if (callback) callback(quote);
        return quote;
      })
      .catch(function (err) {
        if (seq !== state.requestSeq) return state.quote; // stale failure, ignore
        console.error("[SeelSDK] createQuote failed", err);
        clearMount();
        setChecked(false); // no valid quote means no coverage fee on the order
        throw err;
      });
  }

  function onCheck(handler) {
    state.checkHandlers.push(handler);
  }

  function onUncheck(handler) {
    state.uncheckHandlers.push(handler);
  }

  window.SeelSDK = {
    configure: configure,
    createQuote: createQuote,
    onCheck: onCheck,
    onUncheck: onUncheck,
  };
})();
