# Seel Direct Integration - Reference Implementation

A reference implementation of Seel's direct ("ecommerce") integration. It is
for SaaS platforms whose retailers offer Worry-Free Purchase through an API
rather than an app store. Shopify retailers install Seel's Shopify app
instead and need none of this.

It wraps the public `/v1/ecommerce/*` APIs and follows the flow in Seel's
[SaaS Platform Integration Quickstart](https://developer.seel.com/docs/platform-direct-integration).
The self-built opt-in widget on the Quote API is Seel's documented Option B.

The API calls the program WFP (Worry-Free Purchase), so element IDs and API
fields say `wfp` even where the product is sold as Worry-Free Delivery.

## Layout

| Path | What it is |
|---|---|
| `widget/seel-widget.js` | Storefront widget. Vanilla JS, no build step. Exposes `window.SeelSDK` (`createQuote`, `onCheck`, `onUncheck`, `configure`) and renders the Quote API response into `#seel-wfp-widget-root`. |
| `widget/demo.html` | Offline demo with a mocked quote. Open it in a browser. |
| `server/python/` | Stdlib only. `python3 example_server.py`. The primary copy. |
| `server/node/` | Node 18+ built-ins. `node example-server.js` |
| `server/rust/` | Small crate: ureq client, hyper example server. `cargo run` |
| `server/java/` | JDK 17+ only. `javac *.java && java ExampleServer`. See [Java port](#java-port). |
| `server/validation-cases.json` | Shared test fixture all four ports run. |

Each server port has a client for every `/v1/ecommerce/*` endpoint and an
example server. The example server is a quote proxy that keeps the API key
out of the browser, plus order and fulfillment routes and a webhook endpoint
that checks HMAC signatures. Node, Rust and Java are ports of the Python copy.

## Quickstart

The offline demo needs no keys:

```bash
open widget/demo.html
```

Against sandbox, start a server, then point the demo at it. The example
server does not serve the demo page.

```bash
cd server/python   # or server/node, server/rust, server/java
SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... SEEL_MERCHANT_ID=... SEEL_QUOTE_TYPE=... python3 example_server.py
# then, in widget/demo.html, replace the mock quoteFetcher with
#   configure({ quoteEndpoint: "http://localhost:8787/v1/ecommerce/quotes" })
```

The server listens on `127.0.0.1:8787`. Set `HOST` and `PORT` to change
either. It rejects request bodies over 1 MiB with a `413`. Each port reads the process
environment directly and never loads a `.env` file. `.env.example` lists
the variables to set. Never commit credentials.

## Program configuration

Nothing is hardcoded to one program. Seel provides four values at
onboarding:

| Value | Where it goes |
|---|---|
| API key | `SEEL_API_KEY`, server-side only |
| Webhook secret | `SEEL_WEBHOOK_SECRET`, server-side only |
| Merchant ID (per retailer) | `SEEL_MERCHANT_ID`, or per request |
| Quote type (per program, e.g. `acme-wfp`) | `SEEL_QUOTE_TYPE`, or per request |

With `SEEL_MERCHANT_ID` and `SEEL_QUOTE_TYPE` set, the proxy stamps them
into every quote request. The storefront embed is then identical for every
program and retailer, and only the backend environment differs.

## Deployment shapes

The same code covers two setups. They differ in who runs the example server
and what the retailer holds.

**Single retailer.** The retailer's backend holds the API key and calls
Seel directly. The quote proxy keeps the key out of the browser. Orders and
fulfillments usually go straight through `SeelClient` from the order
pipeline, so the order routes are optional.

```
storefront widget ──▶ retailer backend ──▶ Seel
```

**Platform proxy.** The platform holds one API key for every retailer on it.
Retailers call the platform instead of Seel and hold no Seel credentials,
at most their merchant ID. The platform creates each merchant, resolves
which merchant each request belongs to, and receives every webhook, so it
sees all contracts and claims across its retailers.

```
storefront widget ──▶ platform proxy ──▶ Seel
retailer backend  ──▶ platform proxy ──▶ Seel
```

The proxy serves Seel's own paths, prefix included. A retailer already
written against Seel moves onto a platform by setting `SEEL_BASE_URL` to the
platform and changing nothing else.

A platform starts by replacing two functions in the example server:

| Function | Replace it with |
|---|---|
| `authenticate_caller` | Your own retailer authentication. Here it returns true for everyone, which is fine only because the server holds a sandbox key and binds `127.0.0.1` by default. Set `HOST` to expose it only once this function is real, or anyone who can reach the port can use the key. Never give a retailer the Seel API key. One key covers every retailer, so any holder could act as any other. |
| `resolve_merchant_id` | A lookup from the authenticated caller to that retailer's merchant ID. Here it falls back to the request body, which a real platform must not do. |

Leave `SEEL_MERCHANT_ID` unset on a platform. It stamps one merchant onto
every request, which only fits the single-retailer case.

The platform also has to check order ownership. The proxy takes `order_id`
from the URL and never checks it against the caller, so one retailer could
cancel another's order by knowing its ID. The platform needs its own record
of which retailer owns which order and must check it before proxying. That
mapping belongs to the platform, so this reference does not invent one.
Read this code for the request shapes and the lifecycle, not as a hardened
gateway.

## Proxy routes

All routes are POST.

| Route | Calls |
|---|---|
| `/v1/ecommerce/quotes` | `create_quote` |
| `/v1/ecommerce/orders` | `create_order` |
| `/v1/ecommerce/orders/{order_id}` | `update_order` |
| `/v1/ecommerce/orders/{order_id}/cancel` | `cancel_order` |
| `/v1/ecommerce/orders/{order_id}/fulfillments` | `create_fulfillment` |
| `/v1/ecommerce/orders/{order_id}/fulfillments/{fulfillment_id}` | `update_fulfillment` |
| `/webhooks/seel` | HMAC-verified webhook receiver |

Each failure source gets its own status, so you can tell your bug from
Seel's answer from a network problem:

| Status | Meaning |
|---|---|
| Seel's own status | Seel rejected the request. The proxy passes Seel's error body through unchanged, so you see the field it objected to. |
| `400` | The proxy rejected the request before sending. Validation failures carry a `problems` array listing every fault. Other rejections, such as malformed JSON, carry a plain `error`. |
| `409` | Seel accepted the order but minted no contract, including a 2xx whose body is not JSON. The order exists upstream, so do not retry it. |
| `408` | Rust only. The request body did not arrive within 30 seconds. |
| `413` | The request body is over 1 MiB. |
| `502` | The proxy got no usable response from Seel. Usually Seel was unreachable and nothing was processed. If the connection dropped partway through Seel's answer, it may have processed the request, so look an order up before retrying it. |
| `504` | Seel did not answer within 15 seconds. It may have processed the request, so look the order up before retrying. |

A complete 2xx from Seel stays a 2xx. An empty body comes back as `{}`, and
a body that is not JSON comes back as `{"seel_raw_body": "<text>"}`.

The proxy also returns `400` for an ID that would escape its path segment.
A decoded `/` would otherwise reach a different endpoint than the route
names. It rejects `batch` as an order ID for the same reason, so the
update-order route cannot reach the batch endpoint.

The proxy forwards only the write path. Merchant onboarding
(`create_merchant`, `create_orders_batch`) runs from your onboarding flow,
not a retailer request, so it has no route. Neither do claims or the `GET`
lookups. The clients expose all of them for you to call directly.

## Integration flow

1. **Onboard.** When a retailer enables the program, call
   `create_merchant()`, then `create_orders_batch()` with at least 30 days
   of order history so Seel can run risk analysis.
2. **Checkout.** The storefront embeds `seel-widget.js` and the mount div.
   The widget quotes through the proxy and renders the offer. Re-quote on
   address change, discount or item removal. The widget drops out-of-order
   responses and remembers a shopper's opt-out across re-quotes.
3. **Order placed.** Call `create_order()` for every order, opted in or
   not. On opt-in, include `seel_services` with the `quote_id` and quoted
   `price`. That call mints the contract and fires `contract.created`.
4. **Fulfillment.** Call `create_fulfillment()` with tracking and carrier
   on ship, and `update_fulfillment()` when delivery status changes.
5. **Cancellation.** Call `cancel_order()`, which cancels coverage too. On
   the `contract.cancelled` webhook, refund the WFP fee and its tax to the
   shopper if you have not already.
6. **Claims.** When a shopper files in the platform's returns flow, call
   `create_claim()`.
   - Delivery-issue claims use `claim_type` `loss`, `damage`, `theft` or
     `delay`, with `claim_details` and attachments.
   - Return-shipping claims use `claim_type` `return_shipping`, with the RMA
     number, the return shipment (carrier, tracking, label cost) and the
     return addresses.

   Each program sets who decides the outcome. Either the platform
   adjudicates and submits its decision through `update_claim()` (accept or
   reject, with a `reject_reason` code and shopper-facing details on a
   rejection), or Seel adjudicates. Either way Seel fires `claim.accepted`
   or `claim.rejected`.
7. **Webhooks.** Use one endpoint. Check the `X-Seel-Hmac-SHA256`
   signature, answer 200 within 10 seconds, and dedupe on the payload's
   `id` plus `type`, because delivery is at-least-once.

`onCheck` and `onUncheck` fire only when the opt-in state changes. A
re-quote passes the new price to the `createQuote` callback, so update the
displayed fee there if the price moved while the shopper stayed opted in.

## Environments

| Environment | Base URL |
|---|---|
| Sandbox | `https://api-test.seel.com` |
| Production | `https://api.seel.com` |

The base URL comes from `SEEL_BASE_URL` or the client constructor, so
switching environments needs no code change. Each client also defines
`SANDBOX_BASE_URL` and `PRODUCTION_BASE_URL`. Python, Node and Java default
to sandbox when you pass no base URL. Rust requires one.

Four values change between environments, not just the URL:

| Value | Why it differs |
|---|---|
| `SEEL_BASE_URL` | It is the environment. |
| `SEEL_API_KEY` | Seel issues keys per environment. A sandbox key fails against production. |
| `SEEL_WEBHOOK_SECRET` | Seel issues it with the key. |
| `SEEL_MERCHANT_ID` | Merchants are created per environment, so a retailer has a different ID in each. |

`SEEL_QUOTE_TYPE` is usually the same in both. Confirm it with your Seel
contact.

Seel configures two more things per environment, and neither carries over:

- **Webhook URLs.** Seel registers them by hand. A sandbox endpoint does
  nothing for production.
- **Rates and market eligibility.** Seel sets these per merchant. A
  currency priced in sandbox may be unpriced in production, and the other
  way round. See [unpriced markets](#unpriced-markets-do-not-error).

Before the first real order, confirm with Seel that webhooks and rates are
set up on the target environment.

Every request carries `X-Seel-Api-Key` and `X-Seel-Api-Version`. The
pinned version is the `API_VERSION` constant in each client and is the same
in both environments. [developer.seel.com](https://developer.seel.com/reference/introduction)
lists the latest. Browser code always goes through the proxy and never
holds the key.

## Validation rules

Measured against sandbox on 2026-09-10 for Create Quote, Create Order and
Create Merchant, by removing one field at a time from a known-good payload.
An earlier version of this README also listed `retail_price`, `category_3`,
`category_4` and `shipping_origin.state` as required. They are optional.

These rules describe a newly provisioned account. As an integration
matures, Seel's implementation team relaxes validation to match the fields
a merchant can actually supply, so an established account may accept less.
Sending the full set is never wrong. That is why the clients treat
validation as advisory and let you turn it off.

### Required on Create Quote and Create Order

Top level, both calls: `merchant_id`, `session_id`, `device_category`,
`device_platform`, `customer`, `shipping_address`, `line_items`.
Quote also needs `type` and `is_default_on`. Order also needs `order_id`,
`order_number` and `created_ts`.

- `customer`: `customer_id`, `email`
- `shipping_address`: `address_1`, `city`, `state`, `zipcode`, `country`
- `line_items[]`: `line_item_id`, `product_id`, `product_title`, `quantity`,
  `price`, `allocated_discounts`, `sales_tax`, `final_price`, `currency`,
  `requires_shipping`, `image_urls`, `category_1`, `category_2`,
  `is_final_sale`, `shipping_origin`
- `line_items[].shipping_origin`: `country` only

Optional: `cart_id`, `device_id`, `client_ip`, `customer.first_name`,
`last_name` and `phone`, `shipping_address.address_2`, and line-item
`variant_id`, `sku`, `brand_name`, `retail_price`, `product_url`,
`category_3`, `category_4`, `condition`, and every `shipping_origin` field
except `country`.

Seel expects each line item to satisfy
`price + sales_tax - allocated_discounts = final_price`. The clients do not
check this, so compute `final_price` from the other three.

Each program has its own quote `type`, provided with your credentials. Seel
rejects any other type with an error naming the allowed ones.

### Unpriced markets do not error

Seel configures eligibility per merchant, market and currency. For an
unconfigured market the quote still returns `accepted`, with `price: 0.0`
and the coverage missing from `coverages[]`. That looks like a working
integration until someone notices no offer ever showed. Check that
`coverages[]` is non-empty before treating a quote as an offer. If a market
you expect comes back empty, ask your Seel contact whether rates are set
for that currency.

### Required on Create Merchant

`shop_id`, `admin_domain`, `shop_domain`, `shop_platform`, `shop_currency`,
`shop_name`, `contact_name`, `contact_email`, and `seel_services` with
`type` on each entry.

Every `seel_services` entry also needs a `coverages` key. Without it Seel
returns `500 system error`, not a validation error. An empty array passes,
which suggests a server-side null check rather than a data requirement. The
clients check for the key before sending so you get a readable error
instead of a 500.

### Failed attaches

Two of these three mistakes return HTTP 200 and attach nothing, so an
integration can look healthy while covering nothing:

| Mistake | Response |
|---|---|
| `quote_id` at the top level instead of inside `seel_services` | `200`, `seel_services: null`, no error |
| `seel_services` sent as an object instead of an array | `500` from the JSON parser |
| `seel_services` entry with no `price` | `200`, `contract_id: null`, `error: "system error"` inside the entry |

The Python, Node and Rust clients check every `create_order` response for a
real `contract_id` to catch this.

Seel does not check that the attached `price` matches the quoted premium,
or that the order's line items match the quoted cart. It mints a contract
either way, so your code has to check both.

### Turning validation off

The Python, Node and Rust clients run two checks, each with its own switch,
both on by default:

| Switch | What it does | Turn it off when |
|---|---|---|
| `validate` | Checks the payload against the rules above before sending, and raises with every problem at once | Your account has relaxed fields, or you want the API to be the only judge |
| `check_contract` | Checks that `create_order` actually minted a contract | You inspect `seel_services[].contract_id` yourself |

The switches are separate on purpose. `validate` is an opinion about
required fields and can be wrong for your account. `check_contract` is not
an opinion. A `contract_id: null` on a 200 is a real failure, and turning
off validation should not hide it.

```python
SeelClient(api_key, base_url, validate=False, check_contract=True)   # Python
```
```javascript
new SeelClient(apiKey, baseUrl, apiVersion, timeoutMs, false, true)  // Node
```
```rust
SeelClient::new(&key, &url).without_validation()                     // Rust
SeelClient::new(&key, &url).without_contract_check()                 // Rust
```

### Errors

Seel returns errors as JSON with an `error` message and a `trace_id`. Each
client exposes both on its API error type, and the proxy forwards them to
the browser. Include the `trace_id` when you raise an issue with Seel.

## Java port

`SeelClient.java` takes and returns raw JSON strings, so the client
neither validates payloads nor checks for a minted contract.
`SeelValidation.java` has a small JSON parser and writer that the example
server uses to stamp, validate and check each request.

- Validation lives in `SeelValidation.java`. Call `validateQuote`,
  `validateOrder` or `validateMerchant` on the `Map` your JSON library
  produces, before you serialize it.
- `SeelValidation.contractNotMintedReason` checks a `create_order`
  response. The example server calls it. `SeelClient` does not, so call it
  yourself.

## Tests

All four ports run the same cases from `server/validation-cases.json`. The
fixture catches drift between ports. It checks that all four make the same
accept or reject decision and report every expected field path, not that
any one port is correct. Each suite uses only its runtime's standard
library.

The fixture covers payload validation and the contract-minted check,
including when that check runs. It does not cover routing, path-parameter
decoding or the proxy's error mapping, which differ per port. Rust has
local tests for routing and path decoding. Nothing tests the error
mapping.

```bash
python3 -m unittest discover server/python
node --test server/node/test-validation.js
(cd server/rust && cargo test)
(cd server/java && javac *.java && java SeelValidationTest)
```

The Java suite reads the fixture relative to its own directory, hence the
subshell. Pass Node the file, not the directory: Node 24 treats
`node --test server/node` as a module path and fails.

When you change a required field, update `validation-cases.json` too.
Changing one port alone fails the fixture. Changing all four and skipping
the fixture leaves it passing on a rule no port applies.

## Notes

- The widget matches the `window.SeelSDK` interface of Seel's hosted
  bundle, so switching to that bundle later means changing the script
  `src`. Only `configure()` is specific to this build.
- Style the widget to Seel's
  [design guidelines](https://developer.seel.com/docs/design-guideline)
  before production.
- The platform owns tax on the WFP fee. Add it to the order total on
  opt-in and refund it with the fee on cancellation.
