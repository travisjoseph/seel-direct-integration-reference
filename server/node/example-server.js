/**
 * Example backend for the direct-integration path. Node stdlib only.
 *
 * Routes:
 *   POST /v1/ecommerce/quotes  - browser quote proxy: attaches the server-side API
 *                           key and forwards to Seel's Quote API (the widget
 *                           never sees the key)
 *
 *   POST /v1/ecommerce/orders                                - create order
 *   POST /v1/ecommerce/orders/{orderId}                      - update order
 *   POST /v1/ecommerce/orders/{orderId}/cancel               - cancel order
 *   POST /v1/ecommerce/orders/{orderId}/fulfillments         - create fulfillment
 *   POST /v1/ecommerce/orders/{orderId}/fulfillments/{fid}   - update fulfillment
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
 *   Partner platform proxy - the partner platform holds one API key for every retailer on it,
 *   retailers point at the partner platform instead of at Seel, and the partner platform
 *   resolves which merchant each request belongs to. Retailers hold no Seel
 *   credentials at all. See authenticateCaller() and resolveMerchantId()
 *   below, which are where a partner platform starts, and SEEL_RETAILER_MERCHANTS,
 *   which binds each retailer storefront's origin to its merchant. They are
 *   not the whole job: order_id comes off the URL and is never checked
 *   against the caller, so nothing here stops one retailer touching
 *   another's order. That mapping belongs to the partner platform - see the README.
 *
 * Run:
 *   SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... node example-server.js
 *
 * Listens on 127.0.0.1:8787. Set HOST (and PORT) to bind elsewhere.
 *
 * To drive the widget demo against a live sandbox, two steps - this server
 * doesn't serve the demo page:
 *   1. run this server
 *   2. in widget/demo.html, replace the mock quoteFetcher with
 *      configure({ quoteEndpoint: "http://localhost:8787/v1/ecommerce/quotes" })
 */

"use strict";

const http = require("node:http");

const {
  SANDBOX_BASE_URL,
  SeelAPIError,
  SeelClient,
  SeelContractNotMintedError,
  SeelValidationError,
  verifyWebhookSignature,
} = require("./seel-client");

const PORT = parseInt(process.env.PORT || "8787", 10);
// Loopback by default: this demo authenticates nobody, so it must not be
// reachable from the network unless the operator asks for that.
const HOST = process.env.HOST || "127.0.0.1";
// Quote and order payloads run to a few KB; anything near this is not one.
const MAX_BODY_BYTES = 1048576;
const API_KEY = process.env.SEEL_API_KEY || "";
const WEBHOOK_SECRET = process.env.SEEL_WEBHOOK_SECRET || "";
const BASE_URL = process.env.SEEL_BASE_URL || SANDBOX_BASE_URL;
// Program values from Seel onboarding. When set, the proxy stamps them into
// every quote request, so storefront code stays identical across programs.
const QUOTE_TYPE = process.env.SEEL_QUOTE_TYPE || "";

function normalizeOrigin(origin) {
  return origin.trim().toLowerCase().replace(/\/+$/, "");
}

/**
 * Parse SEEL_RETAILER_MERCHANTS into a Map of storefront origin to
 * merchant_id.
 *
 * Throws on anything malformed. A bad table must stop startup: running on
 * with a partial or empty one would refuse every retailer, or worse, bind a
 * storefront to the wrong merchant.
 */
function parseRetailerMerchants(raw) {
  let table;
  try {
    table = JSON.parse(raw);
  } catch (exc) {
    throw new Error(`SEEL_RETAILER_MERCHANTS is not valid JSON: ${exc.message}`);
  }
  if (table === null || typeof table !== "object" || Array.isArray(table)) {
    throw new Error("SEEL_RETAILER_MERCHANTS must be a JSON object of origin to merchant_id");
  }
  // A Map, not an object: an Origin of "constructor" must not find
  // Object.prototype.constructor.
  const parsed = new Map();
  for (const [origin, merchantId] of Object.entries(table)) {
    if (typeof merchantId !== "string" || !merchantId) {
      throw new Error(`SEEL_RETAILER_MERCHANTS[${JSON.stringify(origin)}] must be a non-empty string`);
    }
    const key = normalizeOrigin(origin);
    if (parsed.has(key) && parsed.get(key) !== merchantId) {
      throw new Error(`SEEL_RETAILER_MERCHANTS maps ${JSON.stringify(key)} to two merchants`);
    }
    parsed.set(key, merchantId);
  }
  return parsed;
}

// Mutable so the tests can rebind it per case without reloading the module.
const merchantConfig = {
  merchantId: process.env.SEEL_MERCHANT_ID || "",
  retailerMerchants: process.env.SEEL_RETAILER_MERCHANTS
    ? parseRetailerMerchants(process.env.SEEL_RETAILER_MERCHANTS)
    : null,
};
// Both set is a contradiction, not a precedence question: SEEL_MERCHANT_ID
// would silently stamp one merchant onto every retailer in the table.
if (merchantConfig.merchantId && merchantConfig.retailerMerchants !== null) {
  throw new Error("set SEEL_MERCHANT_ID or SEEL_RETAILER_MERCHANTS, not both");
}

const client = new SeelClient(API_KEY, BASE_URL);

// Routes mirror Seel's real paths, prefix included, so a caller already
// written against Seel moves onto a partner platform by changing the base URL and
// nothing else. The clients build "<base>/v1/ecommerce/...", so anything
// shorter would 404 every call.
const QUOTE_PATH = "/v1/ecommerce/quotes";
const WEBHOOK_PATH = "/webhooks/seel";
const ORDERS_PATH = /^\/v1\/ecommerce\/orders$/;
const ORDER_PATH = /^\/v1\/ecommerce\/orders\/([^/]+)$/;
const ORDER_CANCEL_PATH = /^\/v1\/ecommerce\/orders\/([^/]+)\/cancel$/;
const FULFILLMENTS_PATH = /^\/v1\/ecommerce\/orders\/([^/]+)\/fulfillments$/;
const FULFILLMENT_PATH = /^\/v1\/ecommerce\/orders\/([^/]+)\/fulfillments\/([^/]+)$/;

/**
 * Percent-decode one path segment, or return null if it escapes.
 *
 * A segment arrives encoded and is interpolated into the upstream URL, so a
 * decoded "/" would reach a different endpoint than the route implies:
 * "orders/ORD1%2Fcancel" matches the update-order route and would perform a
 * cancel. Control characters are refused for the same reason. A malformed
 * escape is rejected rather than thrown, which would otherwise surface as a
 * 500. With one API key shared across retailers this is a privilege
 * boundary, not a cosmetic check.
 */
// Segments that are Seel endpoints in their own right and so can never be
// an order id. Seel's own collection endpoints live alongside order ids, so
// an id that equals one of them would reach the collection instead.
// "batch" is POST /v1/ecommerce/orders/batch, the order-history backfill:
// routed as an order id it would proxy an unstamped, unvalidated batch
// write.
const RESERVED_PATH_SEGMENTS = new Set(["batch"]);

function safePathParam(raw) {
  let decoded;
  try {
    decoded = decodeURIComponent(raw);
  } catch {
    return null; // malformed percent-escape
  }
  if (decoded.includes("/")) return null;
  if (RESERVED_PATH_SEGMENTS.has(decoded)) return null;
  // A segment that is only dots is refused too: a decoded ".." is not a
  // slash, but fetch normalizes it away, so "orders/%2E%2E/cancel" leaves
  // this proxy as a request to /v1/ecommerce/cancel - a different endpoint
  // than the route names.
  if (/^\.+$/.test(decoded)) return null;
  for (const ch of decoded) {
    const code = ch.codePointAt(0);
    if (code < 0x20 || code === 0x7f) return null;
  }
  return decoded;
}

/** The request cannot be bound to a merchant; the handler answers 403. */
class MerchantBindingError extends Error {}

/**
 * Identify the caller as { origin }, or return null to answer 401. origin is
 * the normalized Origin header, or null.
 *
 * Browser quote requests come from a retailer's storefront, and the browser
 * sets Origin itself; page script cannot change it. That makes the origin
 * the caller's identity for the quote route, and resolveMerchantId() looks
 * it up in SEEL_RETAILER_MERCHANTS. A non-browser client can send any Origin
 * it likes, so this binds what a shopper's browser can do, not what a
 * script with the proxy URL can.
 *
 * Server-to-server routes (orders, fulfillments, cancel) have no Origin.
 * This demo accepts them from anyone, which is only safe because it holds a
 * sandbox key and listens on localhost. A partner platform authenticates its
 * retailers' backends with its own credentials, returns the retailer
 * identity here, and maps it to a merchant the same way the origin is
 * mapped. Retailers never receive a Seel API key: one key covers every
 * retailer on the partner platform and would let any holder act as any other.
 */
function authenticateCaller(req) {
  const origin = req.headers.origin;
  return { origin: origin ? normalizeOrigin(origin) : null };
}

/**
 * Return the merchant ID to stamp, or throw MerchantBindingError.
 *
 * Single retailer: SEEL_MERCHANT_ID is set once in the environment and
 * stamped onto everything, so storefront and pipeline code carry no
 * program-specific values.
 *
 * Partner platform proxy: SEEL_RETAILER_MERCHANTS maps each storefront origin to its
 * merchant. The merchant comes from the caller, never the body, which is
 * what stops one retailer quoting or ordering against another's merchant
 * ID. A body merchant_id that disagrees is refused rather than overwritten,
 * so a misconfigured storefront fails loudly. The origin is the only
 * identity this demo knows, so with a table set a backend call without one,
 * such as create order, is refused until the partner platform maps its own retailer
 * credentials here too.
 *
 * With neither set, the body value is forwarded so the unconfigured demo
 * still runs. Any caller can then name any merchant.
 */
function resolveMerchantId(caller, params) {
  const { merchantId: single, retailerMerchants } = merchantConfig;
  if (single) return single;
  if (retailerMerchants === null) return params.merchant_id || "";
  const merchantId = retailerMerchants.get(caller.origin);
  if (merchantId === undefined) {
    throw new MerchantBindingError("request origin is not a registered storefront");
  }
  if (Object.hasOwn(params, "merchant_id") && params.merchant_id !== merchantId) {
    throw new MerchantBindingError(
      "merchant_id in the request body does not match the merchant for this storefront"
    );
  }
  return merchantId;
}

/**
 * The Access-Control-Allow-Origin value for this request, or null.
 *
 * With a retailer table, only a registered storefront may read responses,
 * so the header echoes that origin and is omitted for any other.
 */
function allowedOrigin(req) {
  const { retailerMerchants } = merchantConfig;
  if (retailerMerchants === null) return "*";
  const origin = normalizeOrigin(req.headers.origin || "");
  return retailerMerchants.has(origin) ? origin : null;
}

/**
 * Stamp the resolved merchant into params, or answer 403 and return false.
 */
function stampMerchant(res, caller, params) {
  let merchantId;
  try {
    merchantId = resolveMerchantId(caller, params);
  } catch (exc) {
    if (!(exc instanceof MerchantBindingError)) throw exc;
    respond(res, 403, { error: exc.message });
    return false;
  }
  if (merchantId) params.merchant_id = merchantId;
  return true;
}

/**
 * Call Seel and mirror the result back to the caller.
 */
async function forward(res, label, call) {
  try {
    respond(res, 200, await call());
  } catch (exc) {
    if (exc instanceof SeelValidationError) {
      // The request never left this process, so this is the caller's bug.
      // Hand back every problem at once.
      respond(res, 400, { error: exc.message, problems: exc.problems });
    } else if (exc instanceof SeelContractNotMintedError) {
      // Seel accepted the order and minted no contract. 502 would be wrong
      // twice over: the upstream call succeeded, and a retry would
      // duplicate the order.
      console.log(`[proxy] ${label}: ${exc.message}`);
      respond(res, 409, { error: exc.message, seel_response: exc.response });
    } else if (exc instanceof SeelAPIError) {
      // Forward Seel's status and error body - it names the offending
      // field.
      const errBody =
        exc.body !== null && typeof exc.body === "object" ? exc.body : { error: exc.message };
      respond(res, exc.status, errBody);
    } else if (exc.name === "TimeoutError") {
      // Seel may have processed the request before the deadline, so a
      // blind retry can duplicate an order. 504 says so; 502 would read
      // as "nothing happened".
      console.log(`[proxy] ${label} timed out after ${client.timeoutMs}ms`);
      respond(res, 504, {
        error: "timed out waiting for Seel; the request may have been processed, check before retrying",
      });
    } else {
      // Log before answering: a bare 502 leaves the operator unable to tell
      // a timeout from a bug in this handler.
      console.log(`[proxy] ${label} failed: ${exc}`);
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
 * Before any of this fires, the endpoint has to be registered with Seel.
 * Seel has no self-serve way to register this URL. There is no webhook field on
 * Create or Update Merchant and no registration endpoint - ask your Seel contact
 * to configure it, and tell them which events you want. Do it once per
 * environment: a sandbox registration does not carry over to production.
 *
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
    "Content-Length": data.length,
  });
  res.end(data);
}

/**
 * Read the request body, or answer 413 and resolve null once it passes the
 * cap. Content-Length is checked up front; the running count is what
 * catches a chunked body, which declares no length.
 */
function readBody(req, res) {
  const tooLarge = () => {
    respond(res, 413, { error: `request body exceeds ${MAX_BODY_BYTES} bytes` });
    // Discard the rest instead of closing on it. A close with bytes still
    // unread resets the connection, and a client still sending sees the
    // reset before it reads the 413 (RFC 9112 section 9.6). Nothing is
    // buffered, and server.requestTimeout cuts off a client that never
    // finishes.
    req.resume();
  };
  if (Number(req.headers["content-length"]) > MAX_BODY_BYTES) {
    tooLarge();
    return Promise.resolve(null);
  }
  return new Promise((resolve, reject) => {
    const chunks = [];
    let received = 0;
    req.on("data", (chunk) => {
      if (received > MAX_BODY_BYTES) return; // already answered
      received += chunk.length;
      if (received > MAX_BODY_BYTES) {
        tooLarge();
        resolve(null);
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

// Set on res up front so every response, including the 413 from readBody
// and the 500 fallback, carries the same CORS headers.
function setCorsHeaders(req, res) {
  const origin = allowedOrigin(req);
  if (origin) res.setHeader("Access-Control-Allow-Origin", origin);
  if (merchantConfig.retailerMerchants !== null) res.setHeader("Vary", "Origin");
}

async function handleRequest(req, res) {
  setCorsHeaders(req, res);
  if (req.method === "OPTIONS") {
    // CORS preflight for storefront pages
    res.writeHead(204, {
      "Access-Control-Allow-Headers": "Content-Type",
      "Access-Control-Allow-Methods": "POST, OPTIONS",
    });
    res.end();
    return;
  }

  const body = await readBody(req, res);
  if (body === null) return;
  // Match on the path only. Ports that keep the query string here would 404
  // a request the others route.
  const path = req.url.split("?")[0];

  if (req.method !== "POST") {
    respond(res, 404, { error: "not found" });
    return;
  }

  let caller = null;
  if (path !== WEBHOOK_PATH) {
    caller = authenticateCaller(req);
    if (caller === null) {
      respond(res, 401, { error: "unauthorized" });
      return;
    }
  }

  if (path === QUOTE_PATH) {
    const params = jsonBody(res, body);
    if (params === null) return;
    if (!stampMerchant(res, caller, params)) return;
    if (QUOTE_TYPE) params.type = QUOTE_TYPE;
    await forward(res, "quote", () => client.createQuote(params));
    return;
  }

  // Sync every order, opted in or not. On opt-in the body carries
  // seel_services with the quote_id and price, which mints the contract
  // and fires contract.created.
  if (ORDERS_PATH.test(path)) {
    const params = jsonBody(res, body);
    if (params === null) return;
    if (!stampMerchant(res, caller, params)) return;
    await forward(res, "order", () => client.createOrder(params));
    return;
  }

  // Cancel carries no body.
  let match = ORDER_CANCEL_PATH.exec(path);
  if (match) {
    const orderId = safePathParam(match[1]);
    if (orderId === null) {
      respond(res, 400, { error: "invalid order id in path" });
      return;
    }
    await forward(res, "order cancel", () => client.cancelOrder(orderId));
    return;
  }

  match = FULFILLMENT_PATH.exec(path);
  if (match) {
    const orderId = safePathParam(match[1]);
    const fulfillmentId = safePathParam(match[2]);
    if (orderId === null || fulfillmentId === null) {
      respond(res, 400, { error: "invalid id in path" });
      return;
    }
    const params = jsonBody(res, body);
    if (params === null) return;
    await forward(res, "fulfillment update", () =>
      client.updateFulfillment(orderId, fulfillmentId, params)
    );
    return;
  }

  match = FULFILLMENTS_PATH.exec(path);
  if (match) {
    const orderId = safePathParam(match[1]);
    if (orderId === null) {
      respond(res, 400, { error: "invalid order id in path" });
      return;
    }
    const params = jsonBody(res, body);
    if (params === null) return;
    await forward(res, "fulfillment", () => client.createFulfillment(orderId, params));
    return;
  }

  match = ORDER_PATH.exec(path);
  if (match) {
    const orderId = safePathParam(match[1]);
    if (orderId === null) {
      respond(res, 400, { error: "invalid order id in path" });
      return;
    }
    const params = jsonBody(res, body);
    if (params === null) return;
    await forward(res, "order update", () => client.updateOrder(orderId, params));
    return;
  }

  if (path === WEBHOOK_PATH) {
    const signature = req.headers["x-seel-hmac-sha256"] || "";
    // An empty secret is a valid HMAC key, so without this check an
    // unconfigured server authenticates anyone who signs with "".
    if (!WEBHOOK_SECRET || !verifyWebhookSignature(body, signature, WEBHOOK_SECRET)) {
      respond(res, 401, { error: "invalid signature" });
      return;
    }
    // ACK and flush before doing any work: Seel retries anything not
    // answered with a 200 within 10 seconds.
    respond(res, 200, { ok: true });
    let event;
    try {
      event = JSON.parse(body.toString());
    } catch {
      // The parser quotes the offending text in its message, and the
      // payload must not reach the log.
      console.log("[webhook] body is not JSON");
      return;
    }
    try {
      handleWebhookEvent(event);
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
  if (!merchantConfig.merchantId && merchantConfig.retailerMerchants === null) {
    console.log(
      "warning: neither SEEL_MERCHANT_ID nor SEEL_RETAILER_MERCHANTS set - " +
        "merchant_id is taken from the request body, which only a demo may do"
    );
  }
  const server = http.createServer((req, res) => {
    handleRequest(req, res).catch((exc) => {
      console.error(`[server] unhandled error: ${exc}`);
      if (!res.headersSent) {
        respond(res, 500, { error: "internal server error" });
      }
    });
  });
  server.listen(PORT, HOST, () => {
    const { address, port } = server.address();
    console.log(`listening on http://${address}:${port}`);
  });
}

module.exports = { client, handleRequest, merchantConfig, parseRetailerMerchants };
