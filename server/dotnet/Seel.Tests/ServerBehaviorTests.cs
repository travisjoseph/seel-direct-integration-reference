using System.Net;
using System.Net.Http.Headers;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json.Nodes;
using Seel.ExampleServer;

namespace Seel.Tests;

public sealed class ServerFixture : IAsyncLifetime
{
    public StubSeel Seel { get; private set; } = null!;

    public RunningServer Server { get; private set; } = null!;

    public async Task InitializeAsync()
    {
        Seel = await StubSeel.StartAsync();
        Server = await RunningServer.StartAsync(
            RunningServer.Config(Seel.BaseUrl, quoteType: "acme-wfp", timeout: TimeSpan.FromSeconds(0.5)));
    }

    public async Task DisposeAsync()
    {
        await Server.DisposeAsync();
        await Seel.DisposeAsync();
    }
}

/// <summary>
/// The example server against a local stub of Seel: body cap, error mapping,
/// path-parameter rejection and the webhook receiver.
/// </summary>
public class ServerBehaviorTests(ServerFixture fixture) : IClassFixture<ServerFixture>
{
    private const int Cap = 1048576;

    private RunningServer Server => fixture.Server;

    private static string Sign(byte[] body) =>
        Convert.ToBase64String(HMACSHA256.HashData(Encoding.UTF8.GetBytes(RunningServer.Secret), body));

    private static async Task<(int Status, JsonNode? Body)> Read(HttpResponseMessage response)
    {
        var text = await response.Content.ReadAsStringAsync();
        return ((int)response.StatusCode, text.Length > 0 ? JsonNode.Parse(text) : null);
    }

    private async Task<(int Status, JsonNode? Body)> Post(string path, string? body = null, string? signature = null)
    {
        using var request = new HttpRequestMessage(HttpMethod.Post, path)
        {
            Content = body is null ? null : new StringContent(body, Encoding.UTF8, "application/json"),
        };
        if (signature is not null)
        {
            request.Headers.Add("X-Seel-Hmac-SHA256", signature);
        }
        using var response = await Server.Http.SendAsync(request);
        return await Read(response);
    }

    private static JsonObject Error(string message) => new() { ["error"] = message };

    // -- webhook receiver -------------------------------------------------

    [Fact]
    public async Task SignedWebhookIsAcked()
    {
        var body = """{"type":"contract.created","id":"evt_1"}""";
        var (status, reply) = await Post("/webhooks/seel", body, Sign(Encoding.UTF8.GetBytes(body)));
        Assert.Equal((200, """{"ok":true}"""), (status, reply!.ToJsonString()));
    }

    [Theory]
    [InlineData(null)]
    [InlineData("")]
    [InlineData("AAAA")]
    public async Task UnsignedOrWronglySignedWebhookIs401(string? signature)
    {
        var (status, reply) = await Post("/webhooks/seel", """{"type":"contract.created"}""", signature);
        Assert.Equal((401, Error("invalid signature").ToJsonString()), (status, reply!.ToJsonString()));
    }

    [Fact]
    public async Task ASignatureOverADifferentBodyIs401()
    {
        var (status, _) = await Post("/webhooks/seel", """{"id":"evt_2"}""", Sign("""{"id":"evt_1"}"""u8.ToArray()));
        Assert.Equal(401, status);
    }

    // An empty secret is a valid HMAC key, so without the check an
    // unconfigured server would authenticate anyone who signs with "".
    [Fact]
    public async Task AnEmptyWebhookSecretRefusesEverySignature()
    {
        await using var unconfigured = await RunningServer.StartAsync(
            RunningServer.Config(fixture.Seel.BaseUrl, webhookSecret: ""));
        var body = "{}"u8.ToArray();
        var emptyKeySignature = Convert.ToBase64String(HMACSHA256.HashData(Array.Empty<byte>(), body));
        using var request = new HttpRequestMessage(HttpMethod.Post, "/webhooks/seel") { Content = new ByteArrayContent(body) };
        request.Headers.Add("X-Seel-Hmac-SHA256", emptyKeySignature);
        using var response = await unconfigured.Http.SendAsync(request);
        Assert.Equal(HttpStatusCode.Unauthorized, response.StatusCode);
    }

    [Fact]
    public async Task ChunkedWebhookBodyIsVerified()
    {
        var body = """{"type":"contract.created","id":"evt_1"}"""u8.ToArray();
        using var request = new HttpRequestMessage(HttpMethod.Post, "/webhooks/seel")
        {
            Content = new StreamContent(new MemoryStream(body)),
        };
        request.Headers.TransferEncodingChunked = true;
        request.Headers.Add("X-Seel-Hmac-SHA256", Sign(body));
        using var response = await Server.Http.SendAsync(request);
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
    }

    // -- body cap ---------------------------------------------------------

    [Fact]
    public async Task ContentLengthOverCapIs413()
    {
        var (status, reply) = await Post("/v1/ecommerce/quotes", new string('x', Cap + 1));
        Assert.Equal((413, Error($"request body exceeds {Cap} bytes").ToJsonString()), (status, reply!.ToJsonString()));
    }

    [Fact]
    public async Task ContentLengthAtCapIsRead()
    {
        var payload = "{\"a\": \"" + new string('x', Cap - 9) + "\"}";
        Assert.Equal(Cap, payload.Length);
        var (status, reply) = await Post("/v1/ecommerce/quotes", payload);
        Assert.Equal(400, status); // read in full, then failed validation
        Assert.Contains("missing required field merchant_id", reply!["problems"]!.AsArray().Select(p => (string)p!));
    }

    [Fact]
    public async Task ChunkedBodyOverCapIs413()
    {
        using var request = new HttpRequestMessage(HttpMethod.Post, "/v1/ecommerce/quotes")
        {
            Content = new StreamContent(new MemoryStream(new byte[Cap + 1])),
        };
        request.Headers.TransferEncodingChunked = true;
        using var response = await Server.Http.SendAsync(request);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, response.StatusCode);
    }

    [Fact]
    public async Task MalformedChunkSizeIs400()
    {
        var status = await Server.SendBytesAsync(Encoding.ASCII.GetBytes(
            "POST /v1/ecommerce/quotes HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n{}\r\n0\r\n\r\n"));
        Assert.Equal(400, status);
    }

    // -- request parsing and stamping ------------------------------------

    [Theory]
    [InlineData("{\"a\":", "request body must be JSON")]
    [InlineData("{\"a\":1,\"a\":2}", "request body must be JSON")]
    [InlineData("{\"customer\":{\"email\":\"a\",\"email\":\"b\"}}", "request body must be JSON")]
    [InlineData("[1]", "request body must be a JSON object")]
    public async Task UnusableJsonIs400(string body, string error)
    {
        var (status, reply) = await Post("/v1/ecommerce/orders/O-1", body);
        Assert.Equal((400, Error(error).ToJsonString()), (status, reply!.ToJsonString()));
    }

    [Fact]
    public async Task QuoteTypeIsStampedOverTheBody()
    {
        var quote = Fixtures.CompletePayload("quote");
        quote["type"] = "other-program";
        var (status, reply) = await Post("/v1/ecommerce/quotes", quote.ToJsonString());
        Assert.Equal(200, status);
        Assert.Equal("acme-wfp", (string)reply!["type"]!);
    }

    [Fact]
    public async Task OtherVerbsAre404Json()
    {
        using var response = await Server.Http.GetAsync("/v1/ecommerce/quotes");
        Assert.Equal((404, Error("not found").ToJsonString()), ((int)response.StatusCode, (await Read(response)).Body!.ToJsonString()));
    }

    // -- upstream outcome mapping ----------------------------------------

    [Fact]
    public async Task Empty2xxBodyIsAnEmptyObject()
    {
        var (status, reply) = await Post("/v1/ecommerce/orders/O-EMPTY/cancel");
        Assert.Equal((200, "{}"), (status, reply!.ToJsonString()));
    }

    [Fact]
    public async Task NonJson2xxBodyPassesStatusWithRawText()
    {
        var (status, reply) = await Post("/v1/ecommerce/orders/O-TEXT/cancel");
        Assert.Equal((200, """{"seel_raw_body":"OK"}"""), (status, reply!.ToJsonString()));
    }

    [Fact]
    public async Task SeelStatusAndErrorBodyPassThrough()
    {
        var (status, reply) = await Post("/v1/ecommerce/orders/O-REJECT/cancel");
        Assert.Equal((422, """{"error":"order not found","trace_id":"trace-1"}"""), (status, reply!.ToJsonString()));
    }

    [Fact]
    public async Task SeelNonJsonErrorKeepsItsStatus()
    {
        var (status, reply) = await Post("/v1/ecommerce/orders/O-HTML/cancel");
        Assert.Equal((500, Error("Seel API 500: <html>oops</html>").ToJsonString()), (status, reply!.ToJsonString()));
    }

    [Fact]
    public async Task ValidationFailureIs400WithEveryProblem()
    {
        var quote = Fixtures.CompletePayload("quote");
        quote.Remove("customer");
        quote.Remove("line_items");
        var before = fixture.Seel.Received.Count;
        var (status, reply) = await Post("/v1/ecommerce/quotes", quote.ToJsonString());
        Assert.Equal(400, status);
        Assert.Equal(
            ["missing required field customer", "missing required field line_items"],
            reply!["problems"]!.AsArray().Select(p => (string)p!));
        Assert.Equal(before, fixture.Seel.Received.Count);
    }

    [Fact]
    public async Task CreateOrderWithListBodyIs409NotMinted()
    {
        var order = AttachedOrder("m-1");
        var (status, reply) = await Post("/v1/ecommerce/orders", order.ToJsonString());
        Assert.Equal(409, status);
        Assert.Equal("[]", reply!["seel_response"]!.ToJsonString());
        Assert.Contains("no contract was minted", (string)reply["error"]!);
    }

    [Fact]
    public async Task CreateOrderWithNonJson2xxIs409NotMinted()
    {
        var (status, reply) = await Post("/v1/ecommerce/orders", AttachedOrder("TEXT").ToJsonString());
        Assert.Equal(409, status);
        Assert.Equal("""{"seel_raw_body":"OK"}""", reply!["seel_response"]!.ToJsonString());
    }

    [Fact]
    public async Task CreateOrderThatMintsIs200()
    {
        var (status, reply) = await Post("/v1/ecommerce/orders", AttachedOrder("MINT").ToJsonString());
        Assert.Equal(200, status);
        Assert.Equal("ct_1", (string)reply!["seel_services"]![0]!["contract_id"]!);
    }

    [Fact]
    public async Task TimeoutIs504()
    {
        var (status, reply) = await Post("/v1/ecommerce/orders/O-SLOW/cancel");
        Assert.Equal(
            (504, Error("timed out waiting for Seel; the request may have been processed, check before retrying").ToJsonString()),
            (status, reply!.ToJsonString()));
    }

    [Fact]
    public async Task UnreachableSeelIs502()
    {
        // Port 9 (discard) is closed on loopback, so the connect is refused.
        await using var server = await RunningServer.StartAsync(RunningServer.Config("http://127.0.0.1:9"));
        using var response = await server.Http.PostAsync("/v1/ecommerce/orders/O-1/cancel", null);
        var (status, reply) = await Read(response);
        Assert.Equal((502, Error("upstream order cancel request failed").ToJsonString()), (status, reply!.ToJsonString()));
    }

    // -- path parameters --------------------------------------------------

    [Theory]
    [InlineData("/v1/ecommerce/orders/ORD1%2Fcancel")]
    [InlineData("/v1/ecommerce/orders/ORD1%2fcancel/fulfillments")]
    [InlineData("/v1/ecommerce/orders/%2E%2E/cancel")]
    [InlineData("/v1/ecommerce/orders/./cancel")]
    [InlineData("/v1/ecommerce/orders/a%zz/cancel")]
    [InlineData("/v1/ecommerce/orders/a%2/cancel")]
    [InlineData("/v1/ecommerce/orders/a%ff/cancel")]
    [InlineData("/v1/ecommerce/orders/a%01b/cancel")]
    [InlineData("/v1/ecommerce/orders/a%7Fb/cancel")]
    [InlineData("/v1/ecommerce/orders/batch")]
    [InlineData("/v1/ecommerce/orders/O-1/fulfillments/%2F")]
    public async Task IdThatEscapesItsSegmentIs400AndNeverForwarded(string path)
    {
        var before = fixture.Seel.Received.Count;
        using var response = await Server.PostRawPathAsync(path, "{}");
        var (status, reply) = await Read(response);
        Assert.Equal(400, status);
        Assert.StartsWith("invalid", (string)reply!["error"]!);
        Assert.Equal(before, fixture.Seel.Received.Count);
    }

    // Kestrel refuses a request target that decodes to NUL before the
    // handler runs, so this 400 has no JSON body.
    [Fact]
    public async Task EncodedNulIsRefusedByKestrel()
    {
        using var response = await Server.PostRawPathAsync("/v1/ecommerce/orders/a%00b/cancel");
        Assert.Equal(HttpStatusCode.BadRequest, response.StatusCode);
    }

    [Fact]
    public async Task EscapedIdIsDecodedThenReEscapedUpstream()
    {
        using var response = await Server.PostRawPathAsync("/v1/ecommerce/orders/O%201/cancel");
        Assert.Equal(HttpStatusCode.OK, response.StatusCode);
        Assert.Contains(fixture.Seel.Received, r => r.RawTarget == "/v1/ecommerce/orders/O%201/cancel");
    }

    [Theory]
    [InlineData("O-1", "O-1")]
    [InlineData("O%201", "O 1")]
    [InlineData("caf%C3%A9", "café")]
    [InlineData("a.b", "a.b")]
    [InlineData("batches", "batches")]
    [InlineData("ORD1%2Fcancel", null)]
    [InlineData("%2e%2e", null)]
    [InlineData("...", null)]
    [InlineData("%", null)]
    [InlineData("%4", null)]
    [InlineData("%g1", null)]
    [InlineData("%C3", null)]
    [InlineData("%0A", null)]
    [InlineData("%7f", null)]
    [InlineData("batch", null)]
    [InlineData("%62atch", null)]
    public void SafeDecode(string raw, string? expected) => Assert.Equal(expected, PathParams.SafeDecode(raw));

    private static JsonObject AttachedOrder(string merchantId)
    {
        var order = Fixtures.CompletePayload("order");
        order["merchant_id"] = merchantId;
        order["seel_services"] = new JsonArray(new JsonObject { ["type"] = "acme-wfp", ["quote_id"] = "q-1", ["price"] = 0.98 });
        return order;
    }
}
