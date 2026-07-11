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
                    the browser, and a webhook endpoint that checks HMAC
                    signatures.
  python/           Stdlib only.        python3 example_server.py
  node/             Node 18+ built-ins. node example-server.js
  rust/             Small crate.        cargo run
  java/             JDK 17+ only.       javac *.java && java ExampleServer
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
6. **Webhooks** - one endpoint. Check the `X-Seel-Hmac-SHA256` signature,
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

Every request carries `X-Seel-Api-Key` and `X-Seel-Api-Version`. The pinned
version is the `API_VERSION` constant in each client;
[developer.seel.com](https://developer.seel.com/reference/introduction) has
the latest. The key is a server-side secret: browser code goes through the
proxy, never straight to Seel.

## Validation rules

The Quote API enforces more than the schema at
[developer.seel.com/reference/createquote](https://developer.seel.com/reference/createquote)
makes obvious:

- Each program has its own quote `type`, provided with your credentials.
  Any other type is rejected with an error naming the allowed ones.
- These line-item fields are required: `allocated_discounts`, `sales_tax`,
  `retail_price`, `image_urls`, `category_1`..`category_4`, and
  `shipping_origin` (with `state` and `country`). So is
  `shipping_address.state`.
- `price + sales_tax - allocated_discounts` must equal `final_price`.
- Eligibility is configured per program and market. If a market you expect
  comes back `rejected`, ask your Seel contact. US/USD payloads work end to
  end in sandbox.

Errors come back as JSON with an `error` message and a `trace_id`. Each
client surfaces both through its API error type, and the proxy forwards
them to the browser. Include the `trace_id` when you raise an issue with
Seel.

## Notes

- The widget matches the `window.SeelSDK` interface of Seel's hosted
  bundle, so switching to that bundle later is a script-src change. Only
  `configure()` is specific to this build.
- Style the widget to Seel's
  [design guidelines](https://developer.seel.com/docs/design-guideline)
  before production.
- Tax on the WFP fee is the platform's job: add it to the order total on
  opt-in, refund it with the fee on cancellation.
