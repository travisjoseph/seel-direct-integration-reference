/**
 * Upstream-outcome mapping, checked against a local mock of Seel.
 *
 *   node --test server/node/test-proxy.js
 *
 * The shared fixture covers what the validators and the contract check
 * decide. This covers what the client and the proxy do with what Seel
 * sends back: an empty 2xx, a non-JSON 2xx, a timeout, a body that is
 * valid JSON but not an object, and a request body over the cap.
 */

"use strict";

const assert = require("node:assert");
const http = require("node:http");
const { after, before, test } = require("node:test");

const { SeelAPIError, SeelClient, SeelContractNotMintedError } = require("./seel-client");

// What the mock answers is keyed on the order id in the path, or on
// merchant_id for create order, so one upstream serves every case.
const upstream = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const path = req.url;
    if (path === "/v1/ecommerce/orders/EMPTY/cancel") return res.writeHead(200).end();
    if (path === "/v1/ecommerce/orders/TEXT/cancel") {
      return res.writeHead(200, { "Content-Type": "text/plain" }).end("OK");
    }
    if (path === "/v1/ecommerce/orders/HANG/cancel") return; // never answers
    if (path === "/v1/ecommerce/orders") {
      const { merchant_id: merchantId } = JSON.parse(Buffer.concat(chunks).toString());
      if (merchantId === "NULL") return res.writeHead(200).end("null");
      if (merchantId === "LIST") return res.writeHead(200).end("[]");
      if (merchantId === "TEXT") return res.writeHead(200, { "Content-Type": "text/plain" }).end("OK");
    }
    res.writeHead(200).end(JSON.stringify({ ok: true }));
  });
});

let proxy;
let proxyClient;
let baseUrl;
let proxyUrl;

before(async () => {
  await new Promise((resolve) => upstream.listen(0, "127.0.0.1", resolve));
  baseUrl = `http://127.0.0.1:${upstream.address().port}`;
  // The example server reads its config at require time.
  process.env.SEEL_BASE_URL = baseUrl;
  process.env.SEEL_API_KEY = "test-key";
  process.env.SEEL_WEBHOOK_SECRET = "test-secret";
  const { client, handleRequest } = require("./example-server");
  proxyClient = client;
  proxy = http.createServer((req, res) => handleRequest(req, res));
  await new Promise((resolve) => proxy.listen(0, "127.0.0.1", resolve));
  proxyUrl = `http://127.0.0.1:${proxy.address().port}`;
});

after(() => {
  upstream.closeAllConnections();
  upstream.close();
  proxy.closeAllConnections();
  proxy.close();
});

// A complete order so the request reaches the upstream instead of failing
// local validation.
const order = {
  merchant_id: "m-1", order_id: "o-1", order_number: "o-1", created_ts: "1789000000000",
  session_id: "s-1", device_category: "desktop", device_platform: "Web",
  customer: { customer_id: "c-1", email: "shopper@example.com" },
  shipping_address: { address_1: "1 Market St", city: "San Francisco", state: "CA",
                      zipcode: "94105", country: "US" },
  line_items: [{
    line_item_id: "li-1", product_id: "p-1", product_title: "Test Shirt", quantity: 1,
    price: 50.0, allocated_discounts: 0.0, sales_tax: 4.13, final_price: 54.13, currency: "USD",
    requires_shipping: true, image_urls: ["https://example.com/shirt.jpg"], category_1: "Apparel",
    category_2: "Tops", is_final_sale: false, shipping_origin: { country: "US" },
  }],
  seel_services: [{ type: "acme-wfp", quote_id: "q-1", price: 2.0 }],
};

async function post(path, body, headers = {}) {
  const resp = await fetch(proxyUrl + path, { method: "POST", body, headers });
  return { status: resp.status, json: await resp.json() };
}

test("client: empty 2xx body is an empty object", async () => {
  const client = new SeelClient("k", baseUrl);
  assert.deepStrictEqual(await client.cancelOrder("EMPTY"), {});
});

test("client: non-JSON 2xx body carries the status and the raw text", async () => {
  const client = new SeelClient("k", baseUrl);
  await assert.rejects(client.cancelOrder("TEXT"), (exc) => {
    assert.ok(exc instanceof SeelAPIError, `unexpected ${exc}`);
    assert.strictEqual(exc.status, 200);
    assert.deepStrictEqual(exc.body, { seel_raw_body: "OK" });
    return true;
  });
});

test("client: timeout is reported as a TimeoutError, not a generic failure", async () => {
  const client = new SeelClient("k", baseUrl, undefined, 100);
  await assert.rejects(client.cancelOrder("HANG"), { name: "TimeoutError" });
});

for (const merchantId of ["NULL", "LIST"]) {
  test(`client: create order answered with ${merchantId} body is not minted`, async () => {
    const client = new SeelClient("k", baseUrl);
    await assert.rejects(
      client.createOrder({ ...order, merchant_id: merchantId }),
      SeelContractNotMintedError
    );
  });
}

test("proxy: empty 2xx on cancel is 200 {}", async () => {
  assert.deepStrictEqual(await post("/v1/ecommerce/orders/EMPTY/cancel"), { status: 200, json: {} });
});

test("proxy: non-JSON 2xx passes the status through with the raw body", async () => {
  assert.deepStrictEqual(await post("/v1/ecommerce/orders/TEXT/cancel"), {
    status: 200,
    json: { seel_raw_body: "OK" },
  });
});

test("proxy: upstream timeout is 504 with a check-before-retry message", async () => {
  const saved = proxyClient.timeoutMs;
  proxyClient.timeoutMs = 100;
  try {
    assert.deepStrictEqual(await post("/v1/ecommerce/orders/HANG/cancel"), {
      status: 504,
      json: { error: "timed out waiting for Seel; the request may have been processed, check before retrying" },
    });
  } finally {
    proxyClient.timeoutMs = saved;
  }
});

test("proxy: create order answered with null is 409, not 500", async () => {
  const { status, json } = await post(
    "/v1/ecommerce/orders", JSON.stringify({ ...order, merchant_id: "NULL" })
  );
  assert.strictEqual(status, 409);
  assert.strictEqual(json.seel_response, null);
});

test("proxy: create order answered with non-JSON 2xx is 409, not a pass-through 200", async () => {
  const { status, json } = await post(
    "/v1/ecommerce/orders", JSON.stringify({ ...order, merchant_id: "TEXT" })
  );
  assert.strictEqual(status, 409);
  assert.deepStrictEqual(json.seel_response, { seel_raw_body: "OK" });
});

const tooBig = Buffer.alloc(1048576 + 1, "a");
const tooBigError = { error: "request body exceeds 1048576 bytes" };

test("proxy: body over the cap with a Content-Length is 413", async () => {
  assert.deepStrictEqual(await post("/v1/ecommerce/quotes", tooBig), { status: 413, json: tooBigError });
});

test("proxy: chunked body over the cap is 413", async () => {
  const body = new ReadableStream({
    start(controller) {
      controller.enqueue(tooBig.subarray(0, 600000));
      controller.enqueue(tooBig.subarray(600000));
      controller.close();
    },
  });
  const resp = await fetch(proxyUrl + "/v1/ecommerce/quotes", { method: "POST", body, duplex: "half" });
  assert.deepStrictEqual({ status: resp.status, json: await resp.json() }, { status: 413, json: tooBigError });
});
