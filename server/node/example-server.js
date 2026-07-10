/**
 * Example backend for the direct-integration path. Node stdlib only.
 *
 * Two routes:
 *   POST /api/seel/quote  - browser quote proxy: attaches the server-side API
 *                           key and forwards to Seel's Quote API (the widget
 *                           never sees the key)
 *   POST /webhooks/seel   - single webhook endpoint for contract.* and claim.*
 *                           events: verifies HMAC, ACKs 200 fast, then hands
 *                           off for internal fan-out
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

  if (req.method === "POST" && req.url === "/api/seel/quote") {
    let params;
    try {
      params = JSON.parse(body.toString());
    } catch {
      respond(res, 400, { error: "request body must be JSON" });
      return;
    }
    if (MERCHANT_ID) params.merchant_id = MERCHANT_ID;
    if (QUOTE_TYPE) params.type = QUOTE_TYPE;
    try {
      respond(res, 200, await client.createQuote(params));
    } catch (exc) {
      if (exc instanceof SeelAPIError) {
        // Forward Seel's status and error body - it names the offending
        // field.
        const errBody =
          exc.body !== null && typeof exc.body === "object"
            ? exc.body
            : { error: exc.message };
        respond(res, exc.status, errBody);
      } else {
        respond(res, 502, { error: "upstream quote request failed" });
      }
    }
    return;
  }

  if (req.method === "POST" && req.url === "/webhooks/seel") {
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
