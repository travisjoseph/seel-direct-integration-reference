using System.Net;
using System.Text.Json.Nodes;
using Seel.ExampleServer;

namespace Seel.Tests;

/// <summary>
/// Runs server/binding-cases.json, which every port reads. Each binding case
/// goes through the real request handler to a stub upstream that records what
/// it received, so a port that forwards the body merchant_id fails on the
/// merchant the stub saw, not on a unit-level return value.
/// </summary>
public class BindingCasesTests
{
    private static readonly Dictionary<string, string> Paths = new()
    {
        ["quote"] = "/v1/ecommerce/quotes",
        ["order"] = "/v1/ecommerce/orders",
    };

    public static TheoryData<string> ParseCases => Fixtures.Names(Fixtures.Binding, "parse_cases");

    public static TheoryData<string> BindingCases => Fixtures.Names(Fixtures.Binding, "binding_cases");

    [Fact]
    public void FixtureIsPopulated()
    {
        Assert.True(ParseCases.Count >= 5);
        Assert.True(BindingCases.Count >= 5);
    }

    [Theory]
    [MemberData(nameof(ParseCases))]
    public void ParseCase(string name)
    {
        var testCase = Fixtures.Case(Fixtures.Binding, "parse_cases", name);
        var raw = (string)testCase["raw"]!;
        if (testCase["error"]?.GetValue<bool>() == true)
        {
            Assert.Throws<FormatException>(() => MerchantBinding.ParseRetailerMerchants(raw));
            return;
        }
        var expected = testCase["expected"]!.AsObject().ToDictionary(p => p.Key, p => (string)p.Value!);
        Assert.Equal(expected, MerchantBinding.ParseRetailerMerchants(raw));
    }

    [Fact]
    public void BothMerchantSettingsStopStartup() =>
        Assert.Throws<InvalidOperationException>(
            () => MerchantBinding.From("m-solo", """{"https://dealer-a.example":"m-a"}"""));

    [Fact]
    public void NeitherMerchantSettingFallsBackToTheBody() =>
        Assert.IsType<MerchantBinding.FromBody>(MerchantBinding.From(null, null));

    [Theory]
    [MemberData(nameof(BindingCases))]
    public async Task BindingCase(string name)
    {
        var testCase = Fixtures.Case(Fixtures.Binding, "binding_cases", name);
        await using var seel = await StubSeel.StartAsync();
        await using var server = await RunningServer.StartAsync(RunningServer.Config(seel.BaseUrl, Binding(testCase)));

        var route = (string)testCase["route"]!;
        var payload = Fixtures.CompletePayload(route);
        payload.Remove("merchant_id");
        if (testCase["body_merchant_id"] is { } bodyMerchant)
        {
            payload["merchant_id"] = bodyMerchant.DeepClone();
        }
        using var request = new HttpRequestMessage(HttpMethod.Post, Paths[route])
        {
            Content = new StringContent(payload.ToJsonString(), System.Text.Encoding.UTF8, "application/json"),
        };
        if (testCase["origin"] is { } origin)
        {
            request.Headers.Add("Origin", (string)origin!);
        }

        using var response = await server.Http.SendAsync(request);
        var body = JsonNode.Parse(await response.Content.ReadAsStringAsync());

        Assert.Equal(testCase["status"]!.GetValue<int>(), (int)response.StatusCode);
        Assert.Equal((string?)testCase["allow_origin"], AllowOrigin(response));
        if (response.StatusCode == HttpStatusCode.OK)
        {
            var received = Assert.Single(seel.Received);
            Assert.Equal((string)testCase["merchant_id"]!, (string)JsonNode.Parse(received.Body)!["merchant_id"]!);
        }
        else
        {
            Assert.Equal(new JsonObject { ["error"] = (string)testCase["error"]! }.ToJsonString(), body!.ToJsonString());
            Assert.Empty(seel.Received);
        }
    }

    [Fact]
    public async Task PreflightEchoesOnlyARegisteredOrigin()
    {
        var table = new MerchantBinding.RetailerTable(new Dictionary<string, string> { ["https://dealer-a.example"] = "m-a" });
        await using var server = await RunningServer.StartAsync(RunningServer.Config("http://127.0.0.1:9", table));

        using var known = await Preflight(server, "https://dealer-a.example");
        using var unknown = await Preflight(server, "https://other.example");

        Assert.Equal(HttpStatusCode.NoContent, known.StatusCode);
        Assert.Equal("https://dealer-a.example", AllowOrigin(known));
        Assert.Equal("Origin", Assert.Single(known.Headers.Vary));
        Assert.Null(AllowOrigin(unknown));
    }

    [Fact]
    public async Task WithoutATableEveryOriginGetsStarAndNoVary()
    {
        await using var server = await RunningServer.StartAsync(RunningServer.Config("http://127.0.0.1:9"));
        using var response = await Preflight(server, "https://anywhere.example");
        Assert.Equal("*", AllowOrigin(response));
        Assert.Empty(response.Headers.Vary);
    }

    private static Task<HttpResponseMessage> Preflight(RunningServer server, string origin)
    {
        var request = new HttpRequestMessage(HttpMethod.Options, "/v1/ecommerce/quotes");
        request.Headers.Add("Origin", origin);
        return server.Http.SendAsync(request);
    }

    private static string? AllowOrigin(HttpResponseMessage response) =>
        response.Headers.TryGetValues("Access-Control-Allow-Origin", out var values) ? values.Single() : null;

    private static MerchantBinding Binding(JsonObject testCase)
    {
        if (testCase["single_merchant_id"] is { } single)
        {
            return new MerchantBinding.SingleRetailer((string)single!);
        }
        return testCase["retailer_merchants"] is JsonObject table
            ? new MerchantBinding.RetailerTable(table.ToDictionary(p => p.Key, p => (string)p.Value!))
            : new MerchantBinding.FromBody();
    }
}
