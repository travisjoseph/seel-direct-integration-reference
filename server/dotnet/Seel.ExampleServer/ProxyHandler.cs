using System.Text.Json;
using System.Text.Json.Nodes;
using System.Text.RegularExpressions;
using Microsoft.AspNetCore.Http.Features;
using Seel.Client;

namespace Seel.ExampleServer;

/// <summary>
/// Every request the example server answers: the quote proxy, the order and
/// fulfillment routes, and the webhook receiver.
/// </summary>
/// <remarks>
/// Routing matches the raw request target rather than ASP.NET's decoded
/// <c>Request.Path</c>. The id checks in <see cref="PathParams.SafeDecode"/>
/// have to see each segment exactly as sent: Kestrel decodes most escapes and
/// normalizes dot segments before routing, but leaves %2F encoded, so route
/// values would mix decoded and encoded text.
/// </remarks>
public sealed partial class ProxyHandler(
    SeelClient client, ServerConfig config, IRetailerResolver resolver, ILogger<ProxyHandler> log)
{
    /// <summary>
    /// Request bodies larger than this get a 413 before any parsing. A quote
    /// or order is a few KB; the cap exists so a single caller cannot make the
    /// process buffer arbitrary input.
    /// </summary>
    public const int MaxBodyBytes = 1048576;

    // Routes mirror Seel's real paths, prefix included, so a caller already
    // written against Seel moves onto a partner platform by changing the base URL and
    // nothing else.
    private const string QuotePath = "/v1/ecommerce/quotes";
    private const string OrdersPath = "/v1/ecommerce/orders";
    private const string WebhookPath = "/webhooks/seel";

    [GeneratedRegex("^/v1/ecommerce/orders/([^/]+)$")]
    private static partial Regex OrderPath();

    [GeneratedRegex("^/v1/ecommerce/orders/([^/]+)/cancel$")]
    private static partial Regex OrderCancelPath();

    [GeneratedRegex("^/v1/ecommerce/orders/([^/]+)/fulfillments$")]
    private static partial Regex FulfillmentsPath();

    [GeneratedRegex("^/v1/ecommerce/orders/([^/]+)/fulfillments/([^/]+)$")]
    private static partial Regex FulfillmentPath();

    public async Task HandleAsync(HttpContext context)
    {
        IResult result;
        try
        {
            result = context.Request.Method switch
            {
                "OPTIONS" => Preflight(context.Response),
                "POST" => await PostAsync(context),
                _ => Error(404, "not found"),
            };
        }
        catch (RequestRefused refusal)
        {
            result = Error(refusal.Status, refusal.Message);
        }
        // Kestrel's own verdict on a body it could not read: malformed
        // chunks, or a body that stalls (408).
        catch (BadHttpRequestException exc)
        {
            result = Error(exc.StatusCode, exc.Message);
        }
        var origin = config.Binding.AllowedOrigin(context.Request.Headers.Origin);
        if (origin is not null)
        {
            context.Response.Headers.AccessControlAllowOrigin = origin;
        }
        if (config.Binding.VariesByOrigin)
        {
            context.Response.Headers.Vary = "Origin";
        }
        await result.ExecuteAsync(context);
    }

    private static IResult Preflight(HttpResponse response)
    {
        response.Headers.AccessControlAllowHeaders = "Content-Type";
        response.Headers.AccessControlAllowMethods = "POST, OPTIONS";
        return Results.NoContent();
    }

    private async Task<IResult> PostAsync(HttpContext context)
    {
        var body = await ReadBodyAsync(context.Request);
        var rawTarget = context.Features.GetRequiredFeature<IHttpRequestFeature>().RawTarget;
        // Match on the path only. Ports that keep the query string here would
        // 404 a request the others route.
        var path = rawTarget.Split('?', 2)[0];

        if (path == WebhookPath)
        {
            return ReceiveWebhook(context, body);
        }

        var caller = resolver.AuthenticateCaller(context.Request);
        if (caller is null)
        {
            return Error(401, "unauthorized");
        }

        if (path == QuotePath)
        {
            var quote = StampMerchant(caller, ParseObject(body));
            if (config.QuoteType is not null)
            {
                quote["type"] = config.QuoteType;
            }
            return await ForwardAsync("quote", () => client.CreateQuoteAsync(quote));
        }

        // Sync every order, opted in or not. On opt-in the body carries
        // seel_services with the quote_id and price, which mints the contract
        // and fires contract.created.
        if (path == OrdersPath)
        {
            var order = StampMerchant(caller, ParseObject(body));
            return await ForwardAsync("order", () => client.CreateOrderAsync(order));
        }

        // Cancel carries no body.
        if (OrderCancelPath().Match(path) is { Success: true } cancel)
        {
            var orderId = Id(cancel.Groups[1].Value, "invalid order id in path");
            return await ForwardAsync("order cancel", () => client.CancelOrderAsync(orderId));
        }

        if (FulfillmentPath().Match(path) is { Success: true } fulfillment)
        {
            var orderId = Id(fulfillment.Groups[1].Value, "invalid id in path");
            var fulfillmentId = Id(fulfillment.Groups[2].Value, "invalid id in path");
            var update = ParseObject(body);
            return await ForwardAsync("fulfillment update",
                () => client.UpdateFulfillmentAsync(orderId, fulfillmentId, update));
        }

        if (FulfillmentsPath().Match(path) is { Success: true } fulfillments)
        {
            var orderId = Id(fulfillments.Groups[1].Value, "invalid order id in path");
            var created = ParseObject(body);
            return await ForwardAsync("fulfillment", () => client.CreateFulfillmentAsync(orderId, created));
        }

        if (OrderPath().Match(path) is { Success: true } orderMatch)
        {
            var orderId = Id(orderMatch.Groups[1].Value, "invalid order id in path");
            var update = ParseObject(body);
            return await ForwardAsync("order update", () => client.UpdateOrderAsync(orderId, update));
        }

        return Error(404, "not found");
    }

    private IResult ReceiveWebhook(HttpContext context, byte[] body)
    {
        var signature = context.Request.Headers[SeelWebhooks.SignatureHeader].FirstOrDefault();
        if (!SeelWebhooks.VerifySignature(body, signature, config.WebhookSecret))
        {
            return Error(401, "invalid signature");
        }
        // ACK first: Seel retries anything not answered with a 200 within 10
        // seconds, so the work runs only once the response has gone out.
        context.Response.OnCompleted(() =>
        {
            try
            {
                HandleWebhookEvent(JsonNode.Parse(body));
            }
            catch (Exception exc)
            {
                log.LogError(exc, "[webhook] processing error");
            }
            return Task.CompletedTask;
        });
        return Results.Json(new JsonObject { ["ok"] = true });
    }

    /// <summary>Route a delivered webhook into your own systems.</summary>
    /// <remarks>
    /// Before any of this fires, the endpoint has to be registered with Seel.
    /// Seel has no self-serve way to register this URL. There is no webhook
    /// field on Create or Update Merchant and no registration endpoint: ask
    /// your Seel contact to configure it, and tell them which events you want.
    /// Do it once per environment: a sandbox registration does not carry over
    /// to production.
    ///
    /// Internal fan-out. Map merchant_id/order_id to your own retailer code
    /// here and route to your systems. Dedupe on id + type first, since
    /// delivery is at-least-once. In production, queue this work instead of
    /// processing inline.
    /// </remarks>
    private void HandleWebhookEvent(JsonNode? webhookEvent) =>
        log.LogInformation("[webhook] {Type} id={Id}", webhookEvent?["type"], webhookEvent?["id"]);

    /// <summary>
    /// Call Seel and mirror the result back to the caller.
    /// </summary>
    /// <remarks>
    /// Each failure has to stay distinguishable. Collapsing them into one 502
    /// would report a payload that never left this process as an upstream
    /// outage, and would invite a retry on an order Seel has already accepted.
    ///
    /// The upstream call deliberately ignores the caller disconnecting: an
    /// order cut off mid-send leaves Seel's state unknown, which is worse than
    /// finishing a call nobody reads.
    /// </remarks>
    private async Task<IResult> ForwardAsync(string label, Func<Task<JsonNode?>> call)
    {
        try
        {
            return Results.Json(await call());
        }
        catch (SeelValidationException exc)
        {
            // The request never left this process, so this is the caller's
            // bug. Hand back every problem at once.
            return Results.Json(
                new JsonObject { ["error"] = exc.Message, ["problems"] = new JsonArray([.. exc.Problems.Select(p => JsonValue.Create(p))]) },
                statusCode: 400);
        }
        catch (SeelContractNotMintedException exc)
        {
            // Seel accepted the order and minted no contract. 502 would be
            // wrong twice over: the upstream call succeeded, and a retry would
            // duplicate the order.
            log.LogWarning("[proxy] {Label}: {Message}", label, exc.Message);
            return Results.Json(
                new JsonObject { ["error"] = exc.Message, ["seel_response"] = exc.Response?.DeepClone() },
                statusCode: 409);
        }
        catch (SeelApiException exc)
        {
            // Forward Seel's status and error body: it names the offending
            // field.
            return exc.Body is JsonObject seelBody
                ? Results.Json(seelBody, statusCode: exc.Status)
                : Error(exc.Status, exc.Message);
        }
        catch (SeelTimeoutException)
        {
            // Seel may have processed the request before we gave up, so this
            // must not read as "safe to retry" the way a 502 does.
            log.LogWarning("[proxy] {Label}: timed out waiting for Seel", label);
            return Error(504, "timed out waiting for Seel; the request may have been processed, check before retrying");
        }
        catch (Exception exc)
        {
            // Log before answering: a bare 502 leaves the operator unable to
            // tell a connection failure from a bug in this handler.
            log.LogError(exc, "[proxy] {Label} failed", label);
            return Error(502, $"upstream {label} request failed");
        }
    }

    /// <summary>Stamp the resolved merchant into the body, or refuse with 403.</summary>
    private JsonObject StampMerchant(Caller caller, JsonObject body)
    {
        string? merchantId;
        try
        {
            merchantId = resolver.ResolveMerchantId(caller, body);
        }
        catch (MerchantBindingException exc)
        {
            throw new RequestRefused(403, exc.Message);
        }
        if (merchantId is not null)
        {
            body["merchant_id"] = merchantId;
        }
        return body;
    }

    private static string Id(string rawSegment, string error) =>
        PathParams.SafeDecode(rawSegment) ?? throw new RequestRefused(400, error);

    private static JsonObject ParseObject(byte[] body)
    {
        JsonNode? node;
        try
        {
            // Refuse duplicate keys while parsing. JsonObject otherwise builds
            // each nested dictionary lazily and throws on a duplicate the
            // first time validation touches it, deep inside the upstream call.
            node = JsonNode.Parse(body, documentOptions: StrictJson);
        }
        catch (JsonException)
        {
            throw new RequestRefused(400, "request body must be JSON");
        }
        return node as JsonObject ?? throw new RequestRefused(400, "request body must be a JSON object");
    }

    /// <summary>
    /// Read the request body, refusing with 413 past <see cref="MaxBodyBytes"/>.
    /// </summary>
    /// <remarks>
    /// Content-Length is checked before reading and a chunked body is capped
    /// as it streams, so neither framing can push more than the cap into
    /// memory. The cap is enforced here rather than through Kestrel's
    /// MaxRequestBodySize so it holds under any host.
    /// </remarks>
    private static async Task<byte[]> ReadBodyAsync(HttpRequest request)
    {
        if (request.ContentLength > MaxBodyBytes)
        {
            throw TooLarge();
        }
        using var buffer = new MemoryStream();
        var chunk = new byte[81920];
        int read;
        while ((read = await request.Body.ReadAsync(chunk)) > 0)
        {
            if (buffer.Length + read > MaxBodyBytes)
            {
                throw TooLarge();
            }
            buffer.Write(chunk, 0, read);
        }
        return buffer.ToArray();
    }

    private static RequestRefused TooLarge() => new(413, $"request body exceeds {MaxBodyBytes} bytes");

    private static readonly JsonDocumentOptions StrictJson = new() { AllowDuplicateProperties = false };

    /// <summary>A request refused before anything is sent to Seel.</summary>
    private sealed class RequestRefused(int status, string error) : Exception(error)
    {
        public int Status { get; } = status;
    }

    private static IResult Error(int status, string message) =>
        Results.Json(new JsonObject { ["error"] = message }, statusCode: status);
}
