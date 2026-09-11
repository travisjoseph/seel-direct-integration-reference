# Seel Direct Integration - Reference Implementation

A reference implementation of Seel's direct ("ecommerce") integration, for
SaaS platforms whose retailers offer Worry-Free Purchase through an API
rather than an app store. Shopify retailers install Seel's Shopify app
instead and need none of this.

It wraps the public `/v1/ecommerce/*` APIs and follows the flow in Seel's
[SaaS Platform Integration Quickstart](https://developer.seel.com/docs/platform-direct-integration).
A self-built opt-in widget on the Quote API is Seel's documented Option B.

One naming quirk up front: the API calls the program WFP (Worry-Free
Purchase), so element IDs and API fields say `wfp` even where the product
is sold as Worry-Free Delivery.

## Layout

```
widget/
  seel-widget.js    The storefront widget. Vanilla JS, no build step.
                    Exposes window.SeelSDK (createQuote, onCheck, onUncheck,
                    configure) and renders into #seel-wfp-widget-root
                    straight from the Quote API response.
  demo.html         Offline demo with a mocked quote. Open it in a browser.

server/             The same backend in four languages - pick yours. Each
                    has a client for every /v1/ecommerce/* endpoint and an
                    example server: a quote proxy that keeps the API key off
                    the browser, order and fulfillment routes, and a webhook
                    endpoint that checks HMAC signatures. Payloads are
                    validated before sending - see Validation rules.
  python/           Stdlib only.        python3 example_server.py
  node/             Node 18+ built-ins. node example-server.js
  rust/             Small crate.        cargo run
  java/             JDK 17+ only.       javac *.java && java ExampleServer
                    Validation lives in SeelValidation.java here, not in the
                    client - this port has no JSON parser to inspect a
                    payload with.
```

Python is the primary copy; the other three are ports of it.

## Program configuration

Nothing here is hardcoded to one program. Seel provides four values at
onboarding:

| Value | Where it goes |
|---|---|
| API key | `SEEL_API_KEY` env var, server-side only |
| Webhook secret | `SEEL_WEBHOOK_SECRET` env var, server-side only |
| Merchant ID (per retailer) | `SEEL_MERCHANT_ID` env var, or per-request |
| Quote type (per program, e.g. `acme-wfp`) | `SEEL_QUOTE_TYPE` env var, or per-request |

With `SEEL_MERCHANT_ID` and `SEEL_QUOTE_TYPE` set, the proxy stamps them
into every quote request. The storefront embed is then identical for every
program and every retailer; only the backend environment differs.

## Two deployment shapes

The same code covers both; which one you are decides who runs the example
server and what the retailer holds.

**Single retailer.** The retailer's own backend holds the API key and calls
Seel directly. The quote proxy keeps the key off the browser; orders and
fulfillments usually go straight through `SeelClient` from the order
pipeline, so the order routes are optional.

```
storefront widget ──▶ retailer backend ──▶ Seel
```

**Platform proxy.** The platform holds **one** API key covering every
retailer on it, and retailers point at the platform instead of at Seel.
Retailers hold no Seel credentials at all - only their merchant ID, or
nothing. The platform creates each merchant, resolves which merchant an
incoming request belongs to, and receives every webhook, so it has full
visibility into contracts and claims across its retailers.

```
storefront widget ──▶ platform proxy ──▶ Seel
retailer backend  ──▶ platform proxy ──▶ Seel
```

Because the proxy routes mirror Seel's own path shape, a retailer already
written against Seel's API moves onto a platform by changing the base URL
and nothing else.

Two functions in each example server are the whole of what a platform must
replace:

| Function | Replace it with |
|---|---|
| `authenticate_caller` | Your own retailer authentication. Never hand a retailer the Seel API key - one key covers every retailer on the platform, so any holder could act as any other. |
| `resolve_merchant_id` | A lookup from the authenticated caller to that retailer's merchant ID. Deriving it from the caller rather than trusting the request body is what stops one retailer ordering against another's merchant ID. |

Leave `SEEL_MERCHANT_ID` unset when running as a platform - it exists to
stamp a single merchant onto every request, which is the single-retailer
case.

## Proxy routes

The example server answers these. All are POST.

| Route | Calls |
|---|---|
| `/api/seel/quote` | `create_quote` |
| `/api/seel/orders` | `create_order` |
| `/api/seel/orders/{order_id}` | `update_order` |
| `/api/seel/orders/{order_id}/cancel` | `cancel_order` |
| `/api/seel/orders/{order_id}/fulfillments` | `create_fulfillment` |
| `/api/seel/orders/{order_id}/fulfillments/{fulfillment_id}` | `update_fulfillment` |
| `/webhooks/seel` | HMAC-verified webhook receiver |

Seel's status and error body are passed straight back through, so the
caller sees the field Seel objected to. An unreachable Seel answers 502.

Merchant onboarding (`create_merchant`, `create_orders_batch`) has no
route: it runs from your onboarding flow, not from a retailer request.

## Integration flow

1. **Onboard** - when a retailer enables the program, call
   `create_merchant()`, then `create_orders_batch()` with at least 30 days
   of order history so Seel can run risk analysis.
2. **Checkout** - the storefront embeds `seel-widget.js` and the mounting
   div; the widget quotes through the proxy and renders the offer. Re-quote
   on address change, discount, or item removal. The widget drops
   out-of-order responses and remembers a shopper's opt-out across re-quotes.
3. **Order placed** - `create_order()` for every order, opted in or not.
   On opt-in, include `seel_services` with the `quote_id` and quoted `price`.
   That call mints the contract and fires the `contract.created` webhook.
4. **Fulfillment** - `create_fulfillment()` with tracking and carrier on
   ship, `update_fulfillment()` when delivery status changes.
5. **Cancellation** - `cancel_order()`; coverage cancels with it. On the
   `contract.cancelled` webhook, refund the WFP fee and its tax to the
   shopper if that hasn't happened yet.
6. **Claims** - when a shopper files in the platform's returns flow,
   `create_claim()` registers it with Seel. Delivery-issue claims carry
   `claim_type` loss | damage | theft | delay with `claim_details` and
   their attachments; return-shipping claims carry `claim_type`
   return_shipping with the RMA number, the return shipment (carrier,
   tracking, label cost), and the return addresses. Who decides the outcome is set
   per program: either the platform adjudicates and submits its decision
   via `update_claim()` (accept/reject, with a `reject_reason` code and
   shopper-facing details on rejections), or Seel adjudicates. Either way
   Seel fires `claim.accepted` or `claim.rejected`.
7. **Webhooks** - one endpoint. Check the `X-Seel-Hmac-SHA256` signature,
   answer 200 within 10 seconds, and dedupe on the payload's `id` + `type` -
   delivery is at-least-once.

`onCheck` and `onUncheck` fire only when the opt-in state changes.
Re-quotes hand the fresh price to the `createQuote` callback; update the
displayed fee there if the price moved while the shopper stayed opted in.

## Quickstart

The offline demo needs no keys:

```bash
open widget/demo.html
```

Against sandbox, two steps - the example server doesn't serve the demo page:

```bash
cd server/python   # or server/node, server/rust, server/java
SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... SEEL_MERCHANT_ID=... SEEL_QUOTE_TYPE=... python3 example_server.py
# then, in widget/demo.html, replace the mock quoteFetcher with
#   configure({ quoteEndpoint: "http://localhost:8787/api/seel/quote" })
```

Credentials live in environment variables. Never commit them.

## Environments

| Environment | Base URL |
|---|---|
| Sandbox | `https://api-test.seel.com` |
| Production | `https://api.seel.com` |

Nothing in this repo hardcodes a host. The base URL comes from
`SEEL_BASE_URL`, or from the constructor if you pass one, so moving between
environments is a config change and never a code change.

**Four values are environment-scoped, not one.** Swapping only the URL will
fail on the first call:

| Value | Why it differs |
|---|---|
| `SEEL_BASE_URL` | the environment |
| `SEEL_API_KEY` | keys are issued per environment; a sandbox key does not authenticate against production |
| `SEEL_WEBHOOK_SECRET` | issued with the key, so it changes with it |
| `SEEL_MERCHANT_ID` | merchants are created per environment, so a retailer onboarded in sandbox has a different ID in production |

`SEEL_QUOTE_TYPE` is usually the same string in both, but confirm it with
your Seel contact rather than assuming.

Two things that do **not** carry over and are easy to miss, because both are
configured by Seel rather than by you:

- **Webhook URLs.** Registration is manual and per environment. Registering
  a sandbox endpoint does nothing for production.
- **Rates and market eligibility.** These are configured per merchant. A
  currency that is priced in sandbox may not be in production, and the
  reverse. An unpriced market does not error - the quote comes back
  `accepted` with `price: 0.0` and the coverage missing from `coverages[]`,
  which is easy to mistake for a working integration. Check
  `coverages[]` is non-empty before treating a quote as an offer.

So the intended flow is: keep one `.env` per environment, switch which one
is loaded, and confirm with Seel that webhooks and rates are configured on
the target environment before the first real order.

Every request carries `X-Seel-Api-Key` and `X-Seel-Api-Version`. The pinned
version is the `API_VERSION` constant in each client and is the same in both
environments;
[developer.seel.com](https://developer.seel.com/reference/introduction) has
the latest. The key is a server-side secret: browser code goes through the
proxy, never straight to Seel.

## Validation rules

Measured against sandbox on 2026-09-10 by removing one field per request
from a known-good payload and recording the response, for Create Quote,
Create Order and Create Merchant.

**Requiredness is per-account.** Seel validates a strict default profile and
relaxes individual fields for some accounts, so your account may accept less
than this. The sets below are the strict profile: sending them is never
wrong. That is also why the clients treat validation as advisory and let you
switch it off.

### Required on Create Quote and Create Order

Top level, both calls: `merchant_id`, `session_id`, `device_category`,
`device_platform`, `customer`, `shipping_address`, `line_items`.
Quote also needs `type` and `is_default_on`; Order also needs `order_id`,
`order_number` and `created_ts`.

- `customer`: `customer_id`, `email`
- `shipping_address`: `address_1`, `city`, `state`, `zipcode`, `country`
- `line_items[]`: `line_item_id`, `product_id`, `product_title`, `quantity`,
  `price`, `allocated_discounts`, `sales_tax`, `final_price`, `currency`,
  `requires_shipping`, `image_urls`, `category_1`, `category_2`,
  `is_final_sale`, `shipping_origin`
- `line_items[].shipping_origin`: `country` only

Confirmed optional, despite being easy to assume otherwise: `cart_id`,
`device_id`, `client_ip`, `customer.first_name` / `last_name` / `phone`,
`shipping_address.address_2`, and line-item `variant_id`, `sku`,
`brand_name`, `retail_price`, `product_url`, `category_3`, `category_4`,
`condition`, and every `shipping_origin` field except `country`.

`price + sales_tax - allocated_discounts` must equal `final_price`.

Each program has its own quote `type`, provided with your credentials. Any
other type is rejected with an error naming the allowed ones. Eligibility is
configured per program and market: if a market you expect comes back
`rejected`, ask your Seel contact. US/USD payloads work end to end in
sandbox.

### Required on Create Merchant

`shop_id`, `admin_domain`, `shop_domain`, `shop_platform`, `shop_currency`,
`shop_name`, `contact_name`, `contact_email`, and `seel_services` with
`type` on each entry.

**Every `seel_services` entry also needs a `coverages` key.** Omitting it
returns `500 system error`, not a validation error. An empty array is
accepted, so `coverages: []` is enough to get past it, which is the tell
that this is a server-side null-check rather than a data requirement. The
clients check for it before sending so you get a readable error instead of
a 500.

### Three ways Create Order fails while returning 200

A failed attach is reported as `contract_id: null` on an otherwise
successful response. There is no error status code, so an integration can
look healthy while covering nothing.

| Mistake | What you get back |
|---|---|
| `quote_id` at the top level instead of inside `seel_services` | `200`, `seel_services: null`, no error |
| `seel_services` sent as an object rather than an array | `500` from the JSON parser |
| `seel_services` entry with no `price` | `200`, `contract_id: null`, `error: "system error"` nested in the entry |

Two things Seel does **not** enforce, so you have to: the `price` you attach
is never checked against the quoted premium, and the order's line items are
not required to match the quoted cart. Both mint a contract regardless.

### Turning validation off

The Python, Node and Rust clients do two separate things, each with its own
switch, both on by default:

| Switch | What it does | Turn it off when |
|---|---|---|
| `validate` | Checks the payload against the strict profile before sending, and raises with every problem at once | Your account has fields relaxed, or you want the API to be the only authority on what a valid payload is |
| `check_contract` | After `create_order`, checks that a contract actually minted | You inspect `seel_services[].contract_id` yourself |

They are deliberately separate. `validate` encodes an opinion about required
fields, and that opinion can be wrong for your account. `check_contract`
does not: a failed attach really is a failure, Seel just reports it as
`contract_id: null` on a 200. Switching off the opinion should not switch
off the failure detection.

```python
SeelClient(api_key, base_url, validate=False)                  # Python
```
```javascript
new SeelClient(apiKey, baseUrl, apiVersion, timeoutMs, false)  // Node
```
```rust
SeelClient::new(&key, &url).without_validation()               // Rust
```

Rust has `.without_contract_check()` as well; Python and Node take
`check_contract` / `checkContract` alongside `validate`.

The Java client works in raw JSON strings and has no parser, so it cannot do
either automatically. Use `SeelValidation.validateQuote` / `validateOrder` /
`validateMerchant`, which take the `Map` your JSON library produces - call
them before you serialize - and check `contract_id` on the response
yourself.

### Errors

Errors come back as JSON with an `error` message and a `trace_id`. Each
client surfaces both through its API error type, and the proxy forwards them
to the browser. Include the `trace_id` when you raise an issue with Seel.

## Notes

- The widget matches the `window.SeelSDK` interface of Seel's hosted
  bundle, so switching to that bundle later is a script-src change. Only
  `configure()` is specific to this build.
- Style the widget to Seel's
  [design guidelines](https://developer.seel.com/docs/design-guideline)
  before production.
- Tax on the WFP fee is the platform's job: add it to the order total on
  opt-in, refund it with the fee on cancellation.
