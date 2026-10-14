using System.Collections.Concurrent;
using System.Net.Sockets;
using System.Text;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Hosting;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Http.Features;
using Microsoft.Extensions.Logging;
using Seel.Client;
using Seel.ExampleServer;

namespace Seel.Tests;

public sealed record ReceivedRequest(string Method, string RawTarget, string Body, string? ApiKey, string? ApiVersion);

/// <summary>
/// A stand-in for Seel on a free loopback port. It records every request and
/// answers from the path, so each test names the upstream behavior it wants.
/// </summary>
public sealed class StubSeel : IAsyncDisposable
{
    private readonly WebApplication _app;

    private StubSeel(WebApplication app) => _app = app;

    public ConcurrentQueue<ReceivedRequest> Received { get; } = new();

    public string BaseUrl => _app.Urls.Single();

    public static async Task<StubSeel> StartAsync()
    {
        var builder = WebApplication.CreateSlimBuilder();
        builder.WebHost.UseUrls("http://127.0.0.1:0");
        builder.Logging.ClearProviders();
        var app = builder.Build();
        var stub = new StubSeel(app);
        app.Run(stub.AnswerAsync);
        await app.StartAsync();
        return stub;
    }

    private async Task AnswerAsync(HttpContext context)
    {
        var request = context.Request;
        var body = await new StreamReader(request.Body).ReadToEndAsync();
        var target = context.Features.GetRequiredFeature<IHttpRequestFeature>().RawTarget;
        Received.Enqueue(new ReceivedRequest(request.Method, target, body,
            request.Headers["X-Seel-Api-Key"], request.Headers["X-Seel-Api-Version"]));

        var (status, contentType, text) = (request.Method, target) switch
        {
            ("GET", _) => (200, "application/json", $$"""{"method":"GET","path":"{{target}}"}"""),
            (_, var t) when t.EndsWith("/O-EMPTY/cancel") => (200, "application/json", ""),
            (_, var t) when t.EndsWith("/O-TEXT/cancel") => (200, "text/plain", "OK"),
            (_, var t) when t.EndsWith("/O-REJECT/cancel") =>
                (422, "application/json", """{"error":"order not found","trace_id":"trace-1"}"""),
            (_, var t) when t.EndsWith("/O-HTML/cancel") => (500, "text/html", "<html>oops</html>"),
            (_, var t) when t.EndsWith("/O-SLOW/cancel") => await Slow(),
            (_, "/v1/ecommerce/orders") when body.Contains("\"merchant_id\":\"TEXT\"") => (200, "text/plain", "OK"),
            (_, "/v1/ecommerce/orders") when body.Contains("\"merchant_id\":\"MINT\"") =>
                (200, "application/json", """{"seel_services":[{"type":"acme-wfp","contract_id":"ct_1"}]}"""),
            (_, "/v1/ecommerce/orders") => (200, "application/json", "[]"),
            _ => (200, "application/json", body.Length > 0 ? body : "{}"),
        };
        context.Response.StatusCode = status;
        context.Response.ContentType = contentType;
        await context.Response.WriteAsync(text);
    }

    private static async Task<(int, string, string)> Slow()
    {
        await Task.Delay(TimeSpan.FromSeconds(1.5));
        return (200, "application/json", "{}");
    }

    public ValueTask DisposeAsync() => _app.DisposeAsync();
}

/// <summary>The example server, started in-process on a free loopback port.</summary>
public sealed class RunningServer : IAsyncDisposable
{
    public const string Secret = "test-secret";

    private readonly WebApplication _app;

    private RunningServer(WebApplication app, HttpClient http)
    {
        _app = app;
        Http = http;
    }

    public HttpClient Http { get; }

    public Uri BaseUri => new(_app.Urls.Single());

    public static ServerConfig Config(
        string seelBaseUrl,
        MerchantBinding? binding = null,
        string webhookSecret = Secret,
        string? quoteType = null,
        TimeSpan? timeout = null) =>
        new("127.0.0.1", 0,
            new SeelClientOptions { ApiKey = "dummy", BaseUrl = seelBaseUrl, Timeout = timeout ?? TimeSpan.FromSeconds(15) },
            webhookSecret, quoteType, binding ?? new MerchantBinding.FromBody());

    public static async Task<RunningServer> StartAsync(ServerConfig config)
    {
        var app = ExampleServer.ExampleServer.Build(config);
        await app.StartAsync();
        var http = new HttpClient { BaseAddress = new Uri(app.Urls.Single()) };
        return new RunningServer(app, http);
    }

    /// <summary>
    /// POST to a path sent byte for byte. HttpClient would otherwise unescape
    /// and collapse "%2E%2E" before it left the process.
    /// </summary>
    public Task<HttpResponseMessage> PostRawPathAsync(string path, string? body = null)
    {
        var uri = new Uri(BaseUri + path.TrimStart('/'),
            new UriCreationOptions { DangerousDisablePathAndQueryCanonicalization = true });
        return Http.SendAsync(new HttpRequestMessage(HttpMethod.Post, uri)
        {
            Content = body is null ? null : new StringContent(body, Encoding.UTF8, "application/json"),
        });
    }

    /// <summary>Send exact bytes and return the status code from the reply.</summary>
    public async Task<int> SendBytesAsync(byte[] request)
    {
        using var socket = new TcpClient();
        await socket.ConnectAsync(BaseUri.Host, BaseUri.Port);
        var stream = socket.GetStream();
        await stream.WriteAsync(request);
        var buffer = new byte[64];
        var read = await stream.ReadAsync(buffer);
        var statusLine = Encoding.ASCII.GetString(buffer, 0, read);
        return int.Parse(statusLine.Split(' ')[1]);
    }

    public async ValueTask DisposeAsync()
    {
        Http.Dispose();
        await _app.DisposeAsync();
    }
}
