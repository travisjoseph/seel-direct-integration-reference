/**
 * Example backend for the direct-integration path. Node stdlib only.
 *
 * Routes:
 *   POST /api/seel/quote  - browser quote proxy: attaches the server-side API
 *                           key and forwards to Seel's Quote API (the widget
 *                           never sees the key)
 *
 *   POST /api/seel/orders                                - create order
 *   POST /api/seel/orders/{orderId}                      - update order
 *   POST /api/seel/orders/{orderId}/cancel               - cancel order
 *   POST /api/seel/orders/{orderId}/fulfillments         - create fulfillment
 *   POST /api/seel/orders/{orderId}/fulfillments/{fid}   - update fulfillment
 *
 *   POST /webhooks/seel   - single webhook endpoint for contract.* and claim.*
 *                           events: verifies HMAC, ACKs 200 fast, then hands
 *                           off for internal fan-out
 *
 * The order and fulfillment routes mirror Seel's own path shape, so a caller
 * already written against Seel's API moves over by changing the base URL and
 * nothing else.
 *
 * Two deployments use these routes differently:
 *
 *   Single retailer - the retailer's own backend holds the API key and calls
 *   Seel directly. The order and fulfillment routes are optional here; call
 *   SeelClient from your order pipeline instead if that fits better.
 *
 *   Platform proxy - the platform holds one API key for every retailer on it,
 *   retailers point at the platform instead of at Seel, and the platform
 *   resolves which merchant each request belongs to. Retailers hold no Seel
 *   credentials at all. See authenticateCaller() and resolveMerchantId()
 *   below - those two functions are the whole of what a platform must replace.
 *
 * Run:
 *   SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... node example-server.js
 *
 * To drive the widget demo against a live sandbox, two steps - this server
 * doesn't serve the demo page:
 *   1. run this server
 *   2. in widget/demo.html, replace the mock quoteFetcher with
 *      configure({ quoteEndpoint: "http://localhost:8787/api/seel/quote" })
 */

"use strict";

const http = require("node:http");

const {
  SANDBOX_BASE_URL,
  SeelAPIError,
  SeelClient,
  verifyWebhookSignature,
} = require("./seel-client");

const PORT = parseInt(process.env.PORT || "8787", 10);
const API_KEY = process.env.SEEL_API_KEY || "";
const WEBHOOK_SECRET = process.env.SEEL_WEBHOOK_SECRET || "";
const BASE_URL = process.env.SEEL_BASE_URL || SANDBOX_BASE_URL;
// Program values from Seel onboarding. When set, the proxy stamps them into
// every quote request, so storefront code stays identical across programs.
const MERCHANT_ID = process.env.SEEL_MERCHANT_ID || "";
const QUOTE_TYPE = process.env.SEEL_QUOTE_TYPE || "";

const client = new SeelClient(API_KEY, BASE_URL);

// Route patterns, mirroring Seel's own paths under an /api/seel prefix.
const QUOTE_PATH = "/api/seel/quote";
const WEBHOOK_PATH = "/webhooks/seel";
const ORDERS_PATH = /^\/api\/seel\/orders$/;
const ORDER_PATH = /^\/api\/seel\/orders\/([^/]+)$/;
const ORDER_CANCEL_PATH = /^\/api\/seel\/orders\/([^/]+)\/cancel$/;
const FULFILLMENTS_PATH = /^\/api\/seel\/orders\/([^/]+)\/fulfillments$/;
const FULFILLMENT_PATH = /^\/api\/seel\/orders\/([^/]+)\/fulfillments\/([^/]+)$/;

/**
 * Decide whether the caller may use this proxy.
 *
 * This demo accepts everyone, which is only safe because it holds a sandbox
 * key and listens on localhost.
 *
 * A platform MUST replace this. Retailers authenticate to the platform with
 * platform credentials - they never receive a Seel API key, because one key
 * covers every retailer on the platform and would let any holder act as any
 * other. Return the caller's identity from here and pass it to
 * resolveMerchantId() so a retailer can only ever touch its own orders.
 */
function authenticateCaller(req) {
  return true;
}

/**
 * Return the merchant ID this request belongs to.
 *
 * Single retailer: SEEL_MERCHANT_ID is set once in the environment and
 * stamped onto everything, so storefront and pipeline code carry no
 * program-specific values.
 *
 * Platform proxy: leave SEEL_MERCHANT_ID unset and look the merchant up from
 * the authenticated caller instead. Deriving it from the caller rather than
 * trusting the request body is what stops one retailer quoting or ordering
 * against another's merchant ID.
 */
function resolveMerchantId(params) {
  if (MERCHANT_ID) return MERCHANT_ID;
  return params.merchant_id || "";
}

/**
 * Call Seel and mirror the result back to the caller.
 */
async function forward(res, label, call) {
  try {
    respond(res, 200, await call());
  } catch (exc) {
    if (exc instanceof SeelAPIError) {
      // Forward Seel's status and error body - it names the offending
      // field.
      const errBody =
        exc.body !== null && typeof exc.body === "object" ? exc.body : { error: exc.message };
      respond(res, exc.status, errBody);
    } else {
      respond(res, 502, { error: `upstream ${label} request failed` });
    }
  }
}

/**
 * Parse a JSON object body, or answer 400 and return null.
 */
function jsonBody(res, body) {
  let params;
  try {
    params = JSON.parse(body.toString());
  } catch {
    respond(res, 400, { error: "request body must be JSON" });
    return null;
  }
  if (params === null || typeof params !== "object" || Array.isArray(params)) {
    respond(res, 400, { error: "request body must be a JSON object" });
    return null;
  }
  return params;
}

/**
 * Internal fan-out. Map merchant_id/order_id to your own retailer code
 * here and route to your systems. Dedupe on id + type first, since
 * delivery is at-least-once. In production, queue this work off the
 * request thread instead of processing inline.
 */
function handleWebhookEvent(event) {
  console.log(`[webhook] ${event.type} id=${event.id}`);
}

function respond(res, status, body) {
  const data = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    "Content-Type": "application/json",
    "Access-Control-Allow-Origin": "*", // demo only; lock down in prod
    "Content-Length": data.length,
  });
  res.end(data);
}

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on("data", (chunk) => chunks.push(chunk));
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

async function handleRequest(req, res) {
  if (req.method === "OPTIONS") {
    // CORS preflight for the demo page
    res.writeHead(204, {
      "Access-Control-Allow-Origin": "*",
      "Access-Control-Allow-Headers": "Content-Type",
      "Access-Control-Allow-Methods": "POST, OPTIONS",
    });
    res.end();
    return;
  }

  const body = await readBody(req);

  if (req.method !== "POST") {
    respond(res, 404, { error: "not found" });
    return;
  }

  if (req.url !== WEBHOOK_PATH && !authenticateCaller(req)) {
    respond(res, 401, { error: "unauthorized" });
    return;
  }

  if (req.url === QUOTE_PATH) {
    const params = jsonBody(res, body);
    if (params === null) return;
    const merchantId = resolveMerchantId(params);
    if (merchantId) params.merchant_id = merchantId;
    if (QUOTE_TYPE) params.type = QUOTE_TYPE;
    await forward(res, "quote", () => client.createQuote(params));
    return;
  }

  // Sync every order, opted in or not. On opt-in the body carries
  // seel_services with the quote_id and price, which mints the contract
  // and fires contract.created.
  if (ORDERS_PATH.test(req.url)) {
    const params = jsonBody(res, body);
    if (params === null) return;
    const merchantId = resolveMerchantId(params);
    if (merchantId) params.merchant_id = merchantId;
    await forward(res, "order", () => client.createOrder(params));
    return;
  }

  // Cancel carries no body.
  let match = ORDER_CANCEL_PATH.exec(req.url);
  if (match) {
    const orderId = decodeURIComponent(match[1]);
    await forward(res, "order cancel", () => client.cancelOrder(orderId));
    return;
  }

  match = FULFILLMENT_PATH.exec(req.url);
  if (match) {
    const orderId = decodeURIComponent(match[1]);
    const fulfillmentId = decodeURIComponent(match[2]);
    const params = jsonBody(res, body);
    if (params === null) return;
    await forward(res, "fulfillment update", () =>
      client.updateFulfillment(orderId, fulfillmentId, params)
    );
    return;
  }

  match = FULFILLMENTS_PATH.exec(req.url);
  if (match) {
    const orderId = decodeURIComponent(match[1]);
    const params = jsonBody(res, body);
    if (params === null) return;
    await forward(res, "fulfillment", () => client.createFulfillment(orderId, params));
    return;
  }

  match = ORDER_PATH.exec(req.url);
  if (match) {
    const orderId = decodeURIComponent(match[1]);
    const params = jsonBody(res, body);
    if (params === null) return;
    await forward(res, "order update", () => client.updateOrder(orderId, params));
    return;
  }

  if (req.url === WEBHOOK_PATH) {
    const signature = req.headers["x-seel-hmac-sha256"] || "";
    if (!verifyWebhookSignature(body, signature, WEBHOOK_SECRET)) {
      respond(res, 401, { error: "invalid signature" });
      return;
    }
    // ACK and flush before doing any work: Seel retries anything not
    // answered with a 200 within 10 seconds.
    respond(res, 200, { ok: true });
    try {
      handleWebhookEvent(JSON.parse(body.toString()));
    } catch (exc) {
      // already ACKed; never let this escape
      console.log(`[webhook] processing error: ${exc}`);
    }
    return;
  }

  respond(res, 404, { error: "not found" });
}

if (require.main === module) {
  if (!API_KEY) {
    console.log("warning: SEEL_API_KEY not set - quote proxy will fail");
  }
  const server = http.createServer((req, res) => {
    handleRequest(req, res).catch((exc) => {
      console.error(`[server] unhandled error: ${exc}`);
      if (!res.headersSent) {
        respond(res, 500, { error: "internal server error" });
      }
    });
  });
  server.listen(PORT, () => {
    console.log(`listening on http://localhost:${PORT}`);
  });
}
