/**
 * Node.js client for Seel's ecommerce APIs (the direct-integration path).
 *
 * Wraps the public /v1/ecommerce/* APIs and follows the flow in the SaaS
 * Platform Integration Quickstart
 * (https://developer.seel.com/docs/platform-direct-integration):
 *
 *   merchant enables program -> createMerchant (+ batch order-history backfill)
 *   checkout                 -> createQuote (widget renders from the response)
 *   order placed             -> createOrder (all orders, opted in or not;
 *                               seel_services carries quote_id + price on opt-in)
 *   order shipped/delivered  -> createFulfillment / updateFulfillment
 *   order changed/cancelled  -> updateOrder / cancelOrder
 *   return or claim filed    -> createClaim (+ updateClaim with the decision
 *                               when the platform adjudicates)
 *   async status changes     -> webhooks (contract.*, claim.*), HMAC-signed
 *
 * Node 18+ stdlib only (global fetch, node:crypto). Written to be ported.
 */

"use strict";

const crypto = require("node:crypto");

/**
 * Percent-encode one path segment.
 *
 * Ids come from callers and go straight into the upstream URL. Without
 * this, an id containing "/" (or "%2F", which decodes to one) reaches a
 * different endpoint than the method name implies - an updateOrder call
 * with orderId "x/cancel" would cancel instead.
 */
function pathParam(value) {
  return encodeURIComponent(String(value));
}

const SANDBOX_BASE_URL = "https://api-test.seel.com";
const PRODUCTION_BASE_URL = "https://api.seel.com";
const API_VERSION = "2.6.0"; // the pinned API version; all four language ports match

/**
 * Thrown on any non-2xx response. Carries the status and Seel's JSON
 * error body: the message names the offending field, and the trace_id
 * is what Seel support will ask for.
 */
class SeelAPIError extends Error {
  constructor(status, body) {
    const message =
      body !== null && typeof body === "object"
        ? body.error ?? JSON.stringify(body)
        : String(body);
    super(`Seel API ${status}: ${message}`);
    this.name = "SeelAPIError";
    this.status = status;
    this.body = body;
  }
}

/**
 * Raised before any request when a payload is missing fields Seel requires,
 * or uses a shape the API accepts and then fails on. Carries the full list,
 * so one call reports every problem instead of the API reporting them one
 * 400 at a time.
 */
class SeelValidationError extends Error {
  constructor(operation, problems) {
    super(
      `${operation}: ${problems.join("; ")}. ` +
        "Pass validate: false to the client to skip these checks."
    );
    this.name = "SeelValidationError";
    this.operation = operation;
    this.problems = problems;
  }
}

/**
 * Raised when createOrder returns 200 but no contract was created.
 *
 * Seel reports a failed attach as contract_id: null on an otherwise
 * successful response - there is no error status code. Without this check
 * an integration looks healthy while covering nothing.
 */
class SeelContractNotMintedError extends Error {
  constructor(response, detail) {
    super(`order created but no contract was minted: ${detail}`);
    this.name = "SeelContractNotMintedError";
    this.response = response;
  }
}

// Required-field sets, measured against sandbox on 2026-09-10 by removing
// one field per request from a known-good payload and recording the
// response.
//
// Treat these as a starting point, not a fixed contract. A newly
// provisioned account behaves this way; as an integration develops, Seel's
// implementation team works out which fields a merchant journey can
// actually supply and eases the validation accordingly, so an established
// account may accept less. Sending the full set is never wrong, but
// rejecting a payload locally could be, which is why validation is
// advisory and validate: false turns it off.
//
// A "[]" suffix means the rule applies to every element of that array.
const LINE_ITEM_REQUIRED = [
  "line_item_id", "product_id", "product_title", "quantity", "price",
  "allocated_discounts", "sales_tax", "final_price", "currency",
  "requires_shipping", "image_urls", "category_1", "category_2",
  "is_final_sale", "shipping_origin",
];

const QUOTE_REQUIRED = {
  "": ["merchant_id", "session_id", "device_category", "device_platform", "type",
       "is_default_on", "customer", "shipping_address", "line_items"],
  "customer": ["customer_id", "email"],
  "shipping_address": ["address_1", "city", "state", "zipcode", "country"],
  "line_items[]": LINE_ITEM_REQUIRED,
  "line_items[].shipping_origin": ["country"],
};

const ORDER_REQUIRED = {
  "": ["merchant_id", "order_id", "order_number", "created_ts", "session_id",
       "device_category", "device_platform", "customer", "shipping_address", "line_items"],
  "customer": ["customer_id", "email"],
  "shipping_address": ["address_1", "city", "state", "zipcode", "country"],
  "line_items[]": LINE_ITEM_REQUIRED,
  "line_items[].shipping_origin": ["country"],
  // Only checked when seel_services is present - an order with no coverage
  // is a normal sync, not an error.
  "seel_services[]": ["type", "quote_id", "price"],
};

// coverages must be PRESENT but may be an empty array. Omitting it returns
// a 500 rather than a validation error, so catching it locally is the whole
// point of validating this call.
const MERCHANT_REQUIRED = {
  "": ["shop_id", "admin_domain", "shop_domain", "shop_platform", "shop_currency",
       "shop_name", "contact_name", "contact_email", "seel_services"],
  "seel_services[]": ["type", "coverages"],
};

/**
 * Missing means the key is absent, null/undefined, or an empty string.
 * false and 0 are real values - is_default_on, requires_shipping and
 * allocated_discounts all legitimately take them. An empty array is a real
 * value too: merchant coverages: [] is accepted.
 */
// Shape expectations, checked alongside presence. A scalar where an object
// belongs is the archetypal payload mistake, and without this the nested
// rules silently skip it: resolveScope only descends into objects, so
// {customer: "nope"} would report no problems at all.
//
// Each entry is [parent scope, key, kind].
const QUOTE_SHAPES = [
  ["", "customer", "object"],
  ["", "shipping_address", "object"],
  ["", "line_items", "array_nonempty"],
  ["line_items[]", "shipping_origin", "object"],
];
const ORDER_SHAPES = QUOTE_SHAPES.concat([["", "seel_services", "array"]]);
const MERCHANT_SHAPES = [["", "seel_services", "array_nonempty"]];

function typeName(value) {
  if (Array.isArray(value)) return "array";
  return typeof value;
}

function checkShapes(payload, specs) {
  const problems = [];
  for (const [scope, key, kind] of specs) {
    for (const [node, prefix] of resolveScope(payload, scope)) {
      if (!(key in node) || node[key] === null || node[key] === undefined) continue;
      const value = node[key];
      const path = `${prefix}${key}`;
      if (kind === "object" && (typeof value !== "object" || Array.isArray(value))) {
        problems.push(`${path} must be an object, got ${typeName(value)}`);
      } else if (kind.startsWith("array")) {
        if (!Array.isArray(value)) {
          problems.push(`${path} must be an array, got ${typeName(value)}`);
        } else if (kind === "array_nonempty" && value.length === 0) {
          problems.push(`${path} must not be empty`);
        }
      }
    }
  }
  return problems;
}

function isAbsent(value) {
  return value === undefined || value === null || value === "";
}

/** Yield [node, dottedPrefix] pairs a scope selects. */
function resolveScope(payload, scope) {
  let nodes = [[payload, ""]];
  if (!scope) return nodes;
  for (const part of scope.split(".")) {
    const fanOut = part.endsWith("[]");
    const key = fanOut ? part.slice(0, -2) : part;
    const next = [];
    for (const [node, prefix] of nodes) {
      if (node === null || typeof node !== "object") continue;
      const child = node[key];
      if (fanOut && Array.isArray(child)) {
        child.forEach((item, i) => {
          if (item !== null && typeof item === "object" && !Array.isArray(item)) {
            next.push([item, `${prefix}${key}[${i}].`]);
          }
        });
      } else if (!fanOut && child !== null && typeof child === "object" && !Array.isArray(child)) {
        next.push([child, `${prefix}${key}.`]);
      }
    }
    nodes = next;
  }
  return nodes;
}

function collectMissing(payload, rules) {
  const missing = [];
  for (const [scope, fields] of Object.entries(rules)) {
    for (const [node, prefix] of resolveScope(payload, scope)) {
      for (const field of fields) {
        if (!(field in node) || isAbsent(node[field])) missing.push(`${prefix}${field}`);
      }
    }
  }
  return missing;
}

const asProblems = (missing) => missing.map((f) => `missing required field ${f}`);

/** Return the problems with a Create Quote payload. */
function validateQuotePayload(payload) {
  return asProblems(collectMissing(payload, QUOTE_REQUIRED)).concat(
    checkShapes(payload, QUOTE_SHAPES)
  );
}

/**
 * Return the problems with a Create Order payload.
 *
 * seel_services is only checked for completeness when present: syncing an
 * order the shopper did not opt into is normal. Two shape mistakes are
 * checked separately, because the API accepts both and then fails in ways
 * that do not look like failures.
 */
function validateOrderPayload(payload) {
  const rules = { ...ORDER_REQUIRED };
  const services = payload.seel_services;
  if (!services || !Array.isArray(services)) delete rules["seel_services[]"];
  const problems = asProblems(collectMissing(payload, rules)).concat(
    checkShapes(payload, ORDER_SHAPES)
  );

  // Create Order has no top-level quote_id. Sending one is the classic
  // attach mistake: the API returns 200 with seel_services: null and no
  // error, so the integration looks healthy while covering nothing.
  if ("quote_id" in payload) {
    problems.push(
      "quote_id must go inside a seel_services entry, not at the top level - " +
        "a top-level quote_id is ignored and the order attaches no coverage"
    );
  }
  return problems;
}

/** Return the problems with a Create Merchant payload. */
function validateMerchantPayload(payload) {
  return asProblems(collectMissing(payload, MERCHANT_REQUIRED)).concat(
    checkShapes(payload, MERCHANT_SHAPES)
  );
}

class SeelClient {
  constructor(apiKey, baseUrl = SANDBOX_BASE_URL, apiVersion = API_VERSION, timeoutMs = 15000,
              validate = true, checkContract = true) {
    this.apiKey = apiKey;
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.apiVersion = apiVersion;
    this.timeoutMs = timeoutMs;
    // Pre-flight payload validation against the strict profile. Turn it off
    // for an account Seel has relaxed fields for, or to let the API be the
    // only authority on what a valid payload is.
    this.validate = validate;
    // Post-condition check on createOrder: did a contract actually mint?
    // Separate from validate on purpose. It reports a real failure the API
    // returns as a 200, not an opinion about required fields, so turning
    // validation off should not turn this off too.
    this.checkContract = checkContract;
  }

  _check(operation, problems) {
    if (this.validate && problems.length) throw new SeelValidationError(operation, problems);
  }

  async _request(method, path, payload = null) {
    // Header names are case-insensitive per RFC 9110; fetch normalizes
    // their casing on the wire and Seel accepts any casing.
    const resp = await fetch(this.baseUrl + "/v1" + path, {
      method,
      headers: {
        "X-Seel-Api-Key": this.apiKey,
        "X-Seel-Api-Version": this.apiVersion,
        "Content-Type": "application/json",
      },
      body: payload !== null ? JSON.stringify(payload) : undefined,
      signal: AbortSignal.timeout(this.timeoutMs),
    });
    if (!resp.ok) {
      const raw = await resp.text();
      let body;
      try {
        body = JSON.parse(raw);
      } catch {
        body = raw;
      }
      throw new SeelAPIError(resp.status, body);
    }
    return resp.json();
  }

  // -- Merchants ----------------------------------------------------------

  /**
   * Onboard one retailer. Call it when they enable the program;
   * retailers group under your platform organization on Seel's side.
   * Follow with createOrdersBatch and at least 30 days of order
   * history so Seel can run risk analysis.
   */
  createMerchant(payload) {
    this._check("createMerchant", validateMerchantPayload(payload));
    return this._request("POST", "/ecommerce/merchants", payload);
  }

  /**
   * Sync changed protection settings, or disable the program for a
   * retailer - include the reason when disabling.
   */
  updateMerchant(merchantId, payload) {
    return this._request("POST", `/ecommerce/merchants/${pathParam(merchantId)}`, payload);
  }

  // -- Quotes -------------------------------------------------------------

  /**
   * Quote a cart at checkout. The response carries everything the
   * storefront widget renders: price, display_amounts, widget_copy,
   * extra_info. Re-quote whenever the cart changes - address, discount,
   * item removed.
   *
   * The README's validation section and
   * https://developer.seel.com/reference/createquote list the required
   * fields, including
   * price + sales_tax - allocated_discounts == final_price.
   */
  createQuote(payload) {
    this._check("createQuote", validateQuotePayload(payload));
    return this._request("POST", "/ecommerce/quotes", payload);
  }

  getQuote(quoteId) {
    return this._request("GET", `/ecommerce/quotes/${pathParam(quoteId)}`);
  }

  // -- Orders -------------------------------------------------------------

  /**
   * Sync every new order, opted in or not.
   *
   * On opt-in, seel_services must be an ARRAY of entries carrying type,
   * quote_id and price from the latest quote - that mints the contract and
   * fires contract.created. Sending quote_id at the top level instead
   * returns 200 with seel_services: null and no error, which is why this
   * method checks the response as well as the request.
   *
   * Seel does not check the attach against the quote: a price that does not
   * match the quoted premium, or line items that differ from the quoted
   * cart, both still mint a contract. Keeping them consistent is the
   * caller's job.
   */
  async createOrder(payload) {
    this._check("createOrder", validateOrderPayload(payload));
    const response = await this._request("POST", "/ecommerce/orders", payload);
    if (this.checkContract && Array.isArray(payload.seel_services) && payload.seel_services.length) {
      SeelClient._checkContractMinted(payload, response);
    }
    return response;
  }

  /**
   * Fail loudly when an attach silently did not take. A failed attach is
   * contract_id: null on a 200, never a status code, so nothing else in the
   * stack will notice.
   */
  static _checkContractMinted(payload, response) {
    const services = response.seel_services;
    // A non-array is a failure, not something to skip: the ports must agree
    // on this or the check silently does nothing in one of them.
    if (!Array.isArray(services) || !services.length) {
      const n = payload.seel_services.length;
      throw new SeelContractNotMintedError(
        response,
        `sent ${n} seel_services ${n === 1 ? "entry" : "entries"}, response ` +
          `seel_services is ${JSON.stringify(services)}. Check seel_services is an ` +
          "array and quote_id is inside it, not at the top level."
      );
    }
    for (const entry of services) {
      if (entry && typeof entry === "object" && !Array.isArray(entry) && entry.contract_id) continue;
      throw new SeelContractNotMintedError(
        response,
        `service ${JSON.stringify(entry && entry.type)} returned contract_id=null ` +
          `(status=${JSON.stringify(entry && entry.status)}, ` +
          `error=${JSON.stringify(entry && entry.error)})`
      );
    }
  }

  /**
   * Backfill order history at onboarding - at least 30 days.
   */
  createOrdersBatch(payload) {
    return this._request("POST", "/ecommerce/orders/batch", payload);
  }

  /**
   * Sync order changes: line item removed, shipping address updated.
   */
  updateOrder(orderId, payload) {
    return this._request("POST", `/ecommerce/orders/${pathParam(orderId)}`, payload);
  }

  /**
   * Cancel a synced order; its WFP coverage cancels with it.
   * Refunding the WFP fee and tax to the shopper is the platform's
   * job - see Cancellation in the README.
   */
  cancelOrder(orderId) {
    return this._request("POST", `/ecommerce/orders/${pathParam(orderId)}/cancel`);
  }

  // -- Fulfillments -------------------------------------------------------

  /**
   * Send tracking number + carrier when the order ships.
   */
  createFulfillment(orderId, payload) {
    return this._request("POST", `/ecommerce/orders/${pathParam(orderId)}/fulfillments`, payload);
  }

  /**
   * Update tracking/delivery status after fulfillment.
   */
  updateFulfillment(orderId, fulfillmentId, payload) {
    return this._request(
      "POST", `/ecommerce/orders/${pathParam(orderId)}/fulfillments/${pathParam(fulfillmentId)}`, payload
    );
  }

  // -- Claims -------------------------------------------------------------

  /**
   * Register a claim when the shopper files in the platform's returns
   * flow. Delivery-issue claims carry claim_type loss | damage | theft |
   * delay plus claim_details with attachments; return-shipping claims
   * carry claim_type return_shipping plus the RMA number, the return
   * shipment (carrier, tracking, label cost), and the return addresses. Seel opens the claim as pending and fires
   * the claim.created webhook.
   */
  createClaim(payload) {
    return this._request("POST", "/ecommerce/claims", payload);
  }

  /**
   * Submit the adjudication decision on programs where the platform
   * adjudicates: decision accept | reject, with a reject_reason code and
   * shopper-facing details on rejections. Claim items and amounts cannot
   * be changed after creation. Seel records the outcome and fires
   * claim.accepted or claim.rejected.
   */
  updateClaim(claimId, payload) {
    return this._request("POST", `/ecommerce/claims/${pathParam(claimId)}`, payload);
  }

  getClaim(claimId) {
    return this._request("GET", `/ecommerce/claims/${pathParam(claimId)}`);
  }

  // -- Lookups (ad hoc; day-to-day state comes via webhooks) ---------------

  getOrder(orderId) {
    return this._request("GET", `/ecommerce/orders/${pathParam(orderId)}`);
  }

  listContracts(query = "") {
    return this._request("GET", "/ecommerce/contracts" + (query ? `?${query}` : ""));
  }

  listClaims(query = "") {
    return this._request("GET", "/ecommerce/claims" + (query ? `?${query}` : ""));
  }
}

/**
 * Verify Seel's X-Seel-Hmac-SHA256 header: Base64(HMAC-SHA256(body, secret)).
 *
 * Webhook delivery is at-least-once: ACK with HTTP 200 within 10 seconds,
 * dedupe on the payload's id + type (retries reuse the same outer id).
 *
 * @param {Buffer} body - raw request body bytes, exactly as received
 * @param {string} signatureB64 - value of the X-Seel-Hmac-SHA256 header
 * @param {string} webhookSecret
 * @returns {boolean}
 */
function verifyWebhookSignature(body, signatureB64, webhookSecret) {
  const expected = crypto
    .createHmac("sha256", webhookSecret)
    .update(body)
    .digest("base64");
  const a = Buffer.from(expected);
  const b = Buffer.from(String(signatureB64));
  // timingSafeEqual throws on length mismatch, so check first; the length
  // of the expected digest is not secret.
  if (a.length !== b.length) return false;
  return crypto.timingSafeEqual(a, b);
}

module.exports = {
  SANDBOX_BASE_URL,
  PRODUCTION_BASE_URL,
  API_VERSION,
  SeelAPIError,
  SeelValidationError,
  SeelContractNotMintedError,
  SeelClient,
  validateQuotePayload,
  validateOrderPayload,
  validateMerchantPayload,
  verifyWebhookSignature,
};
