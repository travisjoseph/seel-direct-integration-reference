/**
 * Reference Node.js client for Seel's ecommerce partner APIs (direct path).
 *
 * Wraps the public /v1/ecommerce/* APIs documented at
 * https://developer.seel.com - specifically the flow in the SaaS Platform
 * Integration Quickstart
 * (https://developer.seel.com/docs/platform-direct-integration):
 *
 *   merchant enables program -> createMerchant (+ batch order-history backfill)
 *   checkout                 -> createQuote (widget renders from the response)
 *   order placed             -> createOrder (ALL orders, opted-in or not;
 *                               seel_services carries quote_id + price on opt-in)
 *   order shipped/delivered  -> createFulfillment / updateFulfillment
 *   order changed/cancelled  -> updateOrder / cancelOrder
 *   async status changes     -> webhooks (contract.*, claim.*), HMAC-signed
 *
 * Node 18+ standard library only (global fetch, node:crypto), no
 * dependencies - intended to be readable enough to port to any stack.
 */

"use strict";

const crypto = require("node:crypto");

const SANDBOX_BASE_URL = "https://api-test.seel.com";
const PRODUCTION_BASE_URL = "https://api.seel.com";
const API_VERSION = "2.6.0"; // single source of truth for the pinned API version

/**
 * Thrown on any non-2xx response. Carries the HTTP status and Seel's
 * JSON error body, which includes the actionable message (e.g. which
 * required field is missing) and a trace_id to quote in support requests.
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

class SeelClient {
  constructor(apiKey, baseUrl = SANDBOX_BASE_URL, apiVersion = API_VERSION, timeoutMs = 15000) {
    this.apiKey = apiKey;
    this.baseUrl = baseUrl.replace(/\/+$/, "");
    this.apiVersion = apiVersion;
    this.timeoutMs = timeoutMs;
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
   * Onboard one retailer. Called once per retailer when they enable
   * the program (all grouped under your platform organization on Seel's
   * side). Follow with createOrdersBatch for at least 30 days of order
   * history so Seel can run risk analysis.
   */
  createMerchant(payload) {
    return this._request("POST", "/ecommerce/merchants", payload);
  }

  /**
   * Toggle/disable the program for a retailer (include the reason
   * when disabling), or sync changed protection settings.
   */
  updateMerchant(merchantId, payload) {
    return this._request("POST", `/ecommerce/merchants/${merchantId}`, payload);
  }

  // -- Quotes -------------------------------------------------------------

  /**
   * Quote a cart at checkout. The response (price, display_amounts,
   * widget_copy, extra_info) carries all the copy the storefront widget
   * renders. Re-quote on cart changes: address change, discount applied,
   * item removed.
   *
   * See the README "Required fields and validation rules" section and
   * https://developer.seel.com/reference/createquote for the full
   * required-field list, including the constraint
   * price + sales_tax - allocated_discounts == final_price.
   */
  createQuote(payload) {
    return this._request("POST", "/ecommerce/quotes", payload);
  }

  getQuote(quoteId) {
    return this._request("GET", `/ecommerce/quotes/${quoteId}`);
  }

  // -- Orders -------------------------------------------------------------

  /**
   * Sync every new order, whether or not the shopper opted in. When
   * they did, include the seel_services object with the quote_id and
   * price from the latest quote - this is what mints the contract and
   * triggers the contract.created webhook. Line items must match the
   * quoted cart.
   */
  createOrder(payload) {
    return this._request("POST", "/ecommerce/orders", payload);
  }

  /**
   * Backfill historical orders (at least 30 days, at merchant
   * onboarding).
   */
  createOrdersBatch(payload) {
    return this._request("POST", "/ecommerce/orders/batch", payload);
  }

  /**
   * Sync order changes: line item removed, shipping address updated.
   */
  updateOrder(orderId, payload) {
    return this._request("POST", `/ecommerce/orders/${orderId}`, payload);
  }

  /**
   * Cancel a synced order; any WFP coverage on it cancels
   * automatically. Refunding the WFP fee + tax to the shopper is the
   * platform's job (see "Cancellation" in the README integration flow).
   */
  cancelOrder(orderId) {
    return this._request("POST", `/ecommerce/orders/${orderId}/cancel`);
  }

  // -- Fulfillments -------------------------------------------------------

  /**
   * Send tracking number + carrier when the order ships.
   */
  createFulfillment(orderId, payload) {
    return this._request("POST", `/ecommerce/orders/${orderId}/fulfillments`, payload);
  }

  /**
   * Update tracking/delivery status after fulfillment.
   */
  updateFulfillment(orderId, fulfillmentId, payload) {
    return this._request(
      "POST", `/ecommerce/orders/${orderId}/fulfillments/${fulfillmentId}`, payload
    );
  }

  // -- Lookups (ad hoc; day-to-day state comes via webhooks) ---------------

  getOrder(orderId) {
    return this._request("GET", `/ecommerce/orders/${orderId}`);
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
  SeelClient,
  verifyWebhookSignature,
};
