# Seel Direct-Integration SDK (reference implementation)

Reference implementation of Seel's direct ("ecommerce") integration path for
SaaS platform partners: platforms whose retailers offer Seel Worry-Free
Purchase to their shoppers via API, rather than through a storefront
platform's app store. Retailers on Shopify install Seel's Shopify app
instead and need none of this.

Wraps the public `/v1/ecommerce/*` APIs - the flow documented in Seel's
[SaaS Platform Integration Quickstart](https://developer.seel.com/docs/platform-direct-integration).
Building your own opt-in widget on the Quote API is Seel's documented
"Option B" integration path.

Naming note: WFP (Worry-Free Purchase) is the API-level name for the
Worry-Free Delivery program - element IDs and API fields use `wfp`.

## Layout

```
widget/
  seel-widget.js    Self-hosted storefront widget (vanilla JS, no build step).
                    Exposes window.SeelSDK: createQuote() / onCheck() /
                    onUncheck() (matching Seel's hosted widget bundle) plus
                    configure(), specific to this build. Mounts into
                    #seel-wfp-widget-root and renders entirely from the Quote
                    API response (price, widget_copy, terms).
  demo.html         Offline demo with a mocked quote. Open directly in a browser.

server/             Reference backends in four languages - pick yours. Each
                    implements the same two pieces: an API client for the
                    /v1/ecommerce/* endpoints (merchants, quotes, orders,
                    fulfillments, contracts/claims lookups, webhook HMAC
                    verify) and an example server (browser quote proxy that
                    keeps the API key server-side and injects program values,
                    plus an HMAC-verified webhook endpoint).
  python/           Stdlib only.        python3 example_server.py
  node/             Node 18+ built-ins. node example-server.js
  rust/             Small crate.        cargo run
  java/             JDK 17+ only.       javac *.java && java ExampleServer
```

The Python version is the primary reference: it carries the fullest
commentary, and the other languages are faithful ports of it.

## Program configuration

Everything program-specific is a configuration value provided by Seel during
onboarding - nothing in this package is hardcoded to one program:

| Value | Where it goes |
|---|---|
| API key | `SEEL_API_KEY` env var, server-side only |
| Webhook secret | `SEEL_WEBHOOK_SECRET` env var, server-side only |
| Merchant ID (per retailer) | `SEEL_MERCHANT_ID` env var, or per-request |
| Quote type (per program, e.g. `acme-wfp`) | `SEEL_QUOTE_TYPE` env var, or per-request |

When `SEEL_MERCHANT_ID` / `SEEL_QUOTE_TYPE` are set, the quote proxy injects
them into every quote request, so the storefront embed (script tag +
mounting div + `createQuote` call) is identical for every program and every
retailer - only the backend environment differs.

## Integration flow (wired up per retailer)

1. **Onboard** - retailer enables the program: `create_merchant()`, then
   `create_orders_batch()` with at least 30 days of order history (risk
   analysis).
2. **Checkout** - storefront embeds `seel-widget.js` + the mounting div;
   the widget calls the Quote API via the backend proxy and renders the
   offer. Re-quote on address change, discount, or item removal - the widget
   discards out-of-order responses and preserves the shopper's explicit
   opt-out across re-quotes.
3. **Order placed** - `create_order()` for **every** order, opted-in or not.
   On opt-in, include `seel_services` with the `quote_id` and quoted `price` -
   that mints the contract and fires the `contract.created` webhook.
4. **Fulfillment** - `create_fulfillment()` with tracking + carrier on ship;
   `update_fulfillment()` on delivery-status changes.
5. **Cancellation** - `cancel_order()`; coverage auto-cancels. On the
   `contract.cancelled` webhook, refund the WFP fee + tax to the shopper if
   not already refunded.
6. **Webhooks** - one endpoint, HMAC-SHA256 verified (`X-Seel-Hmac-SHA256`),
   ACK 200 within 10s, dedupe on payload `id` + `type` (at-least-once delivery).

Widget callback semantics: `onCheck`/`onUncheck` fire only when the opt-in
state changes; re-quotes deliver the fresh price through the `createQuote`
callback. Update the displayed fee from that callback if the price changed
while the shopper stays opted in.

## Quickstart

Offline widget demo (no keys needed):

```bash
open widget/demo.html
```

Against sandbox (both steps required - the example server does not serve the
demo page):

```bash
cd server/python   # or server/node, server/rust, server/java
SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... SEEL_MERCHANT_ID=... SEEL_QUOTE_TYPE=... python3 example_server.py
# then, in widget/demo.html, replace the mock quoteFetcher with
#   configure({ quoteEndpoint: "http://localhost:8787/api/seel/quote" })
```

Keep credentials in environment variables and never commit them.

## Environments

| Environment | Base URL |
|---|---|
| Sandbox | `https://api-test.seel.com` |
| Production | `https://api.seel.com` |

Auth: `X-Seel-Api-Key` on every request, plus `X-Seel-Api-Version`. The
pinned version is the `API_VERSION` constant in each client (kept identical
across languages; `server/python/seel_client.py` is the primary reference);
see [developer.seel.com](https://developer.seel.com/reference/introduction)
for the latest. The API key is a server-side secret - browser code must go
through a backend proxy (see the example server in your language).

## Required fields and validation rules

The Quote API enforces the following beyond the documented basics
(authoritative schema:
[developer.seel.com/reference/createquote](https://developer.seel.com/reference/createquote)):

- Each program has its own quote `type` (provided by Seel with your
  credentials). Requests with any other type are rejected with an error
  listing the allowed types.
- Required line-item fields: `allocated_discounts`, `sales_tax`,
  `retail_price`, `image_urls`, `category_1`..`category_4`, and
  `shipping_origin` (with `state` and `country`).
- `shipping_address.state` is required.
- Pricing identity: `price + sales_tax - allocated_discounts` must equal
  `final_price`.
- Quote eligibility is configured per program and market. If a quote returns
  `rejected` for a market you expect to be covered, contact your Seel
  integration contact; US/USD payloads work for end-to-end sandbox testing.

API errors return a JSON body with an actionable `error` message and a
`trace_id` - every client surfaces both via its API error type
(`SeelAPIError` and equivalents), and the example quote proxy forwards them
to the browser. Quote the `trace_id` when raising an issue with Seel.

## Design constraints

- The widget intentionally matches the `window.SeelSDK` interface of Seel's
  hosted widget bundle, so code written against
  `createQuote`/`onCheck`/`onUncheck` can switch to a Seel-hosted bundle
  with a script-src change only (`configure()` is specific to this
  self-hosted build).
- Follow Seel's
  [design guidelines](https://developer.seel.com/docs/design-guideline)
  before production styling of the widget.
- Tax on the WFP fee is the platform's responsibility: add it to the order
  total on opt-in, refund it with the fee on cancellation.
