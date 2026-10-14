// Example backend for the direct-integration path. A port of
// server/python/example_server.py.
//
// Routes (all POST):
//   /v1/ecommerce/quotes                                 browser quote proxy: attaches the
//                                                        server-side API key (the widget
//                                                        never sees it) and forwards to Seel
//   /v1/ecommerce/orders                                 create order
//   /v1/ecommerce/orders/{order_id}                      update order
//   /v1/ecommerce/orders/{order_id}/cancel               cancel order
//   /v1/ecommerce/orders/{order_id}/fulfillments         create fulfillment
//   /v1/ecommerce/orders/{order_id}/fulfillments/{fid}   update fulfillment
//   /webhooks/seel                                       contract.* and claim.* events:
//                                                        verifies HMAC, ACKs 200, then
//                                                        hands off for internal fan-out
//
// The order and fulfillment routes mirror Seel's own path shape, so a caller
// already written against Seel's API moves over by changing the base URL and
// nothing else.
//
// Two deployments use these routes differently:
//
//   Single retailer: the retailer's own backend holds the API key and calls
//   Seel directly. The order and fulfillment routes are optional here; call
//   SeelClient from your order pipeline instead if that fits better.
//
//   Partner platform proxy: the partner platform holds one API key for every retailer on it,
//   retailers point at the partner platform instead of at Seel, and the partner platform
//   resolves which merchant each request belongs to. Retailers hold no Seel
//   credentials at all. IRetailerResolver is where a partner platform starts, and
//   SEEL_RETAILER_MERCHANTS binds each retailer storefront's origin to its
//   merchant. They are not the whole job: order_id comes off the URL and is
//   never checked against the caller, so nothing here stops one retailer
//   touching another's order. That mapping belongs to the partner platform; see the
//   README.
//
// Run:
//   SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... dotnet run --project Seel.ExampleServer

using Seel.ExampleServer;

var config = ServerConfig.FromEnvironment(Environment.GetEnvironmentVariable);
var app = ExampleServer.Build(config);
if (config.Seel.ApiKey.Length == 0)
{
    app.Logger.LogWarning("SEEL_API_KEY not set - quote proxy will fail");
}
if (config.Binding is MerchantBinding.FromBody)
{
    app.Logger.LogWarning("neither SEEL_MERCHANT_ID nor SEEL_RETAILER_MERCHANTS set - "
                          + "merchant_id is taken from the request body, which only a demo may do");
}
app.Run();
