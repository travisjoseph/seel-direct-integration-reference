// Run: node --test widget/test-widget.js
// Node built-ins only. The DOM stub models just what the widget touches:
// getElementById, innerHTML (elements are created for each id="..." it
// contains), and change/toggle listeners.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const SOURCE = fs.readFileSync(path.join(__dirname, "seel-widget.js"), "utf8");

function makeElement(id) {
  const listeners = {};
  let html = "";
  const el = {
    id,
    checked: false,
    open: false,
    style: {},
    addEventListener(type, fn) { (listeners[type] = listeners[type] || []).push(fn); },
    fire(type) { (listeners[type] || []).forEach((fn) => fn()); },
  };
  Object.defineProperty(el, "innerHTML", {
    get: () => html,
    set(value) {
      html = value;
      el.children = {};
      for (const m of value.matchAll(/<(\w+)[^>]*\sid="([^"]+)"([^>]*)>/g)) {
        const child = makeElement(m[2]);
        child.checked = /\schecked/.test(m[0]);
        el.children[m[2]] = child;
      }
    },
  });
  el.children = {};
  return el;
}

function loadWidget() {
  const mount = makeElement("seel-wfp-widget-root");
  const document = {
    getElementById(id) {
      if (id === mount.id) return mount;
      return mount.children[id] || null;
    },
  };
  const window = {};
  const errors = [];
  const console = { error: (...a) => errors.push(a), warn: () => {}, log: () => {} };
  vm.runInNewContext(SOURCE, { window, document, console, setTimeout, clearTimeout, Promise });
  const sdk = window.SeelSDK;
  const sent = [];
  let respond = () => QUOTE;
  sdk.configure({ quoteFetcher: async (body) => { sent.push(body); return respond(body); } });
  return {
    sdk,
    mount,
    sent,
    errors,
    setResponder(fn) { respond = fn; },
    checkbox: () => mount.children["seel-wfp-checkbox"] || null,
  };
}

const QUOTE = {
  quote_id: "q_1",
  status: "accepted",
  price: "2.99",
  currency: "USD",
  display_amounts: { price: "$2.99" },
  coverages: [{ coverage_type: "gdd", price: 2.99 }],
  eligible_items: [{ line_item_id: "line_1", variant_id: "v_1" }],
  extra_info: { currency_symbol: "$", widget_title: "Worry-Free Delivery", terms_url: "https://example.com/t" },
};

const UNPRICED = { ...QUOTE, quote_id: "q_unpriced", price: 0.0, coverages: [] };

const CART = { cart_id: "cart_1", is_default_on: true, line_items: [] };

function quoteOnce(w, payload = CART) {
  return new Promise((resolve) => {
    w.sdk.createQuote(payload, (...args) => resolve(args));
  });
}

test("createQuote callback receives (quote, checkboxStatus) with bundle field names", async () => {
  const w = loadWidget();
  const [quote, status, err] = await quoteOnce(w);
  assert.equal(err, undefined);
  assert.equal(status, true, "default-on cart with an offer is checked");
  assert.equal(quote.quoteId, "q_1");
  assert.equal(quote.status, "accepted");
  assert.equal(quote.price, "2.99");
  assert.equal(quote.currencyCode, "USD");
  assert.equal(quote.currencySymbol, "$");
  assert.deepEqual({ ...quote.eligibleItems[0] }, { lineItemId: "line_1", variantId: "v_1" });
  assert.equal(quote.extraInfo.termsUrl, "https://example.com/t");
  assert.equal(w.checkbox().checked, true, "rendered checkbox matches the reported status");
});

test("createQuote returns a Promise of the quote", async () => {
  const w = loadWidget();
  const quote = await w.sdk.createQuote(CART);
  assert.equal(quote.quoteId, "q_1");
});

test("error paths call back (null, null, {code, message}) and resolve null", async () => {
  const w = loadWidget();
  const invalid = [];
  const result = await w.sdk.createQuote(null, (...a) => invalid.push(a));
  assert.equal(result, null);
  assert.equal(invalid.length, 1);
  assert.equal(invalid[0][0], null);
  assert.equal(invalid[0][1], null);
  assert.equal(invalid[0][2].code, "INVALID_PAYLOAD");
  assert.equal(typeof invalid[0][2].message, "string");
  assert.equal(w.sent.length, 0, "invalid payload is never sent");

  w.setResponder(() => { throw new Error("Quote request failed: 502"); });
  const [q, status, err] = await quoteOnce(w);
  assert.equal(q, null);
  assert.equal(status, null);
  assert.deepEqual({ ...err }, { code: "QUOTE_FAILED", message: "Quote request failed: 502" });
});

test("a failed re-quote unchecks and clears the widget", async () => {
  const w = loadWidget();
  const changes = [];
  w.sdk.checkboxChange((q, checked) => changes.push(checked));
  await quoteOnce(w);
  w.setResponder(() => { throw new Error("down"); });
  await quoteOnce(w);
  assert.deepEqual(changes, [true, false]);
  assert.equal(w.mount.innerHTML, "");
});

test("setMerchantId stamps merchant_id unless the payload names one", async () => {
  const w = loadWidget();
  await w.sdk.createQuote(CART);
  assert.equal("merchant_id" in w.sent[0], false, "nothing stamped before setMerchantId");

  w.sdk.setMerchantId("m_retailer_7");
  await w.sdk.createQuote(CART);
  assert.equal(w.sent[1].merchant_id, "m_retailer_7");
  assert.equal("merchant_id" in CART, false, "caller's payload is not mutated");

  await w.sdk.createQuote({ ...CART, merchant_id: "m_explicit" });
  assert.equal(w.sent[2].merchant_id, "m_explicit");
});

test("checkboxChange fires only on state change, with (quote, checked)", async () => {
  const w = loadWidget();
  const changes = [];
  w.sdk.checkboxChange((q, checked) => changes.push([q && q.quoteId, checked]));

  await quoteOnce(w);
  await quoteOnce(w);
  assert.deepEqual(changes, [["q_1", true]], "re-quote with unchanged state does not fire");

  const box = w.checkbox();
  box.checked = false;
  box.fire("change");
  box.fire("change");
  assert.deepEqual(changes, [["q_1", true], ["q_1", false]], "repeated toggle event with same value fires once");

  await quoteOnce(w);
  assert.equal(changes.length, 2, "shopper opt-out survives a default-on re-quote");
  assert.equal(w.checkbox().checked, false);
});

test("checkboxChange keeps only the latest handler", async () => {
  const w = loadWidget();
  const first = [];
  const second = [];
  w.sdk.checkboxChange(() => first.push(1));
  w.sdk.checkboxChange(() => second.push(1));
  await quoteOnce(w);
  assert.equal(first.length, 0);
  assert.equal(second.length, 1);
});

test("updateCheckboxStatus corrects state, re-renders the box, and sticks across re-quotes", async () => {
  const w = loadWidget();
  const changes = [];
  w.sdk.checkboxChange((q, checked) => changes.push(checked));
  await quoteOnce(w);

  w.sdk.updateCheckboxStatus(false);
  assert.deepEqual(changes, [true, false]);
  assert.equal(w.checkbox().checked, false);

  w.sdk.updateCheckboxStatus(false);
  assert.deepEqual(changes, [true, false], "no event when already in that state");

  const [, status] = await quoteOnce(w);
  assert.equal(status, false, "platform-set choice overrides is_default_on on re-quote");

  w.sdk.updateCheckboxStatus(true);
  assert.deepEqual(changes, [true, false, true]);
  assert.equal(w.checkbox().checked, true);
});

test("updateCheckboxStatus(true) with no offer leaves coverage off", async () => {
  const w = loadWidget();
  w.setResponder(() => UNPRICED);
  const changes = [];
  w.sdk.checkboxChange((q, checked) => changes.push(checked));
  await quoteOnce(w);
  w.sdk.updateCheckboxStatus(true);
  assert.deepEqual(changes, []);
});

test("stale responses are dropped without a callback", async () => {
  const w = loadWidget();
  let releaseSlow;
  w.setResponder((body) =>
    body.cart_id === "slow"
      ? new Promise((r) => { releaseSlow = () => r({ ...QUOTE, quote_id: "q_stale" }); })
      : { ...QUOTE, quote_id: "q_fresh" });

  const calls = [];
  const slow = w.sdk.createQuote({ ...CART, cart_id: "slow" }, (q) => calls.push(q.quoteId));
  const fresh = await w.sdk.createQuote({ ...CART, cart_id: "fast" }, (q) => calls.push(q.quoteId));
  releaseSlow();
  assert.equal(await slow, null);
  assert.equal(fresh.quoteId, "q_fresh");
  assert.deepEqual(calls, ["q_fresh"]);
});

test("unpriced market (accepted, empty coverages) renders nothing and reports unchecked", async () => {
  const w = loadWidget();
  const changes = [];
  w.sdk.checkboxChange((q, checked) => changes.push(checked));
  w.setResponder(() => UNPRICED);
  const [quote, status] = await quoteOnce(w);
  assert.equal(quote.status, "accepted");
  assert.equal(status, false);
  assert.equal(w.mount.innerHTML, "");
  assert.deepEqual(changes, [], "never checked, so no change event");
});

test("onWidgetEvent receives emitted types from SEEL_WIDGET_EVENT_TYPES", async () => {
  const w = loadWidget();
  const types = w.sdk.SEEL_WIDGET_EVENT_TYPES;
  const events = [];
  w.sdk.onWidgetEvent((e) => events.push(e));
  w.setResponder(() => ({ ...QUOTE, extra_info: { ...QUOTE.extra_info, coverage_details_text: ["Lost"] } }));
  await quoteOnce(w);
  const details = w.mount.children["seel-wfp-details"];
  details.open = true;
  details.fire("toggle");
  const box = w.checkbox();
  box.checked = false;
  box.fire("change");
  assert.deepEqual(
    events.map((e) => [e.type, e.checked, e.quote.quoteId]),
    [
      [types.CARD_CHECKBOX_CHANGE, true, "q_1"],
      [types.CARD_EXPOSURE_VIEW, true, "q_1"],
      [types.CARD_INFO_CLICK, true, "q_1"],
      [types.CARD_CHECKBOX_CHANGE, false, "q_1"],
    ],
  );
  assert.equal(types.CARD_CHECKBOX_CHANGE, "card_checkbox_change", "bundle event value");
});

test("reset drops in-flight quotes and clears state; destroy also drops handlers", async () => {
  const w = loadWidget();
  const changes = [];
  w.sdk.checkboxChange((q, checked) => changes.push(checked));
  await quoteOnce(w);
  w.checkbox().checked = false;
  w.checkbox().fire("change");

  let release;
  w.setResponder(() => new Promise((r) => { release = () => r(QUOTE); }));
  const inflight = w.sdk.createQuote(CART);
  await w.sdk.reset();
  release();
  assert.equal(await inflight, null, "quote started before reset is dropped");
  assert.equal(w.mount.innerHTML, "");

  w.setResponder(() => QUOTE);
  const [, status] = await quoteOnce(w);
  assert.equal(status, true, "reset forgets the old cart's opt-out");
  assert.deepEqual(changes, [true, false, true], "handler survives reset");

  await w.sdk.destroy();
  await quoteOnce(w);
  assert.deepEqual(changes, [true, false, true], "handler dropped by destroy");
});
