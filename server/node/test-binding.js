/**
 * Runs the shared merchant-binding cases against this port.
 *
 *   node --test server/node/test-binding.js
 *
 * The cases live in ../binding-cases.json and every port runs them. Each
 * binding case goes through the real request handler to a stub upstream
 * that records what it received, so a port that forwards the body
 * merchant_id fails on the merchant the stub saw, not on a unit-level
 * return value.
 */

"use strict";

const assert = require("node:assert");
const fs = require("node:fs");
const http = require("node:http");
const path = require("node:path");
const { after, before, test } = require("node:test");

const readFixture = (name) =>
  JSON.parse(fs.readFileSync(path.join(__dirname, "..", name), "utf8"));
const { parse_cases: parseCases, binding_cases: bindingCases } = readFixture("binding-cases.json");
const payloads = Object.fromEntries(
  readFixture("validation-cases.json")
    .cases.filter((c) => ["quote: complete payload", "order: complete payload"].includes(c.name))
    .map((c) => [c.operation, c.payload])
);
const paths = { quote: "/v1/ecommerce/quotes", order: "/v1/ecommerce/orders" };

const received = [];
const upstream = http.createServer((req, res) => {
  const chunks = [];
  req.on("data", (c) => chunks.push(c));
  req.on("end", () => {
    const body = Buffer.concat(chunks);
    received.push(JSON.parse(body.toString()));
    res.writeHead(200, { "Content-Type": "application/json" }).end(body);
  });
});

let proxy;
let merchantConfig;
let parseRetailerMerchants;

before(async () => {
  await new Promise((resolve) => upstream.listen(0, "127.0.0.1", resolve));
  // The example server reads its config at require time.
  process.env.SEEL_BASE_URL = `http://127.0.0.1:${upstream.address().port}`;
  process.env.SEEL_API_KEY = "test-key";
  const server = require("./example-server");
  ({ merchantConfig, parseRetailerMerchants } = server);
  proxy = http.createServer((req, res) => server.handleRequest(req, res));
  await new Promise((resolve) => proxy.listen(0, "127.0.0.1", resolve));
});

after(() => {
  upstream.closeAllConnections();
  upstream.close();
  proxy.closeAllConnections();
  proxy.close();
});

// http.request rather than fetch, so the Origin header is sent exactly as
// the case names it.
function request(method, urlPath, body, origin) {
  const headers = { "Content-Type": "application/json" };
  if (origin !== null) headers.Origin = origin;
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: "127.0.0.1", port: proxy.address().port, method, path: urlPath, headers },
      (res) => {
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => {
          const text = Buffer.concat(chunks).toString();
          resolve({ res, json: text ? JSON.parse(text) : null });
        });
      }
    );
    req.on("error", reject);
    req.end(body ?? undefined);
  });
}

function configure(single, table) {
  merchantConfig.merchantId = single ?? "";
  merchantConfig.retailerMerchants = table === null ? null : new Map(Object.entries(table));
}

test("retailer table parsing", async (t) => {
  assert.ok(parseCases.length >= 5, "parse_cases fixture is empty");
  for (const c of parseCases) {
    await t.test(c.name, () => {
      if (c.error) {
        assert.throws(() => parseRetailerMerchants(c.raw));
      } else {
        assert.deepStrictEqual(Object.fromEntries(parseRetailerMerchants(c.raw)), c.expected);
      }
    });
  }
});

test("shared binding cases", async (t) => {
  assert.ok(bindingCases.length >= 5, "binding_cases fixture is empty");
  for (const c of bindingCases) {
    await t.test(c.name, async () => {
      configure(c.single_merchant_id, c.retailer_merchants);
      const payload = { ...payloads[c.route] };
      delete payload.merchant_id;
      if (c.body_merchant_id !== null) payload.merchant_id = c.body_merchant_id;
      received.length = 0;

      const { res, json } = await request("POST", paths[c.route], JSON.stringify(payload), c.origin);

      assert.strictEqual(res.statusCode, c.status, JSON.stringify(json));
      assert.strictEqual(res.headers["access-control-allow-origin"] ?? null, c.allow_origin);
      if (c.status === 200) {
        assert.strictEqual(received.length, 1);
        assert.strictEqual(received[0].merchant_id, c.merchant_id);
      } else {
        assert.deepStrictEqual(json, { error: c.error });
        assert.deepStrictEqual(received, [], "a refused request reached Seel");
      }
    });
  }
});

test("preflight echoes only a registered origin", async () => {
  configure(null, { "https://shop-a.example": "m-a" });
  const known = await request("OPTIONS", paths.quote, null, "https://shop-a.example");
  const unknown = await request("OPTIONS", paths.quote, null, "https://other.example");
  assert.strictEqual(known.res.headers["access-control-allow-origin"], "https://shop-a.example");
  assert.strictEqual(known.res.headers.vary, "Origin");
  assert.strictEqual(unknown.res.headers["access-control-allow-origin"], undefined);
});
