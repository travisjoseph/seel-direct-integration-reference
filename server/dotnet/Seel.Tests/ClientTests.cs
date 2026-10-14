using Seel.Client;

namespace Seel.Tests;

public class ClientTests
{
    private static SeelClient Client(StubSeel seel, TimeSpan? timeout = null, bool validate = true) =>
        new(new HttpClient(), new SeelClientOptions
        {
            ApiKey = "dummy",
            BaseUrl = seel.BaseUrl + "/",
            Timeout = timeout ?? TimeSpan.FromSeconds(15),
            Validate = validate,
        });

    [Fact]
    public async Task GetMerchantIsAGetOnTheEscapedMerchantPath()
    {
        await using var seel = await StubSeel.StartAsync();
        var response = await Client(seel).GetMerchantAsync("m/1 x");
        Assert.Equal("""{"method":"GET","path":"/v1/ecommerce/merchants/m%2F1%20x"}""", response!.ToJsonString());
    }

    [Fact]
    public async Task EveryRequestCarriesTheKeyAndPinnedVersion()
    {
        await using var seel = await StubSeel.StartAsync();
        await Client(seel).CancelOrderAsync("O-1");
        var received = Assert.Single(seel.Received);
        Assert.Equal(("POST", "/v1/ecommerce/orders/O-1/cancel", "dummy", "2.6.0"),
            (received.Method, received.RawTarget, received.ApiKey, received.ApiVersion));
    }

    [Fact]
    public async Task ATimeoutIsItsOwnException()
    {
        await using var seel = await StubSeel.StartAsync();
        await Assert.ThrowsAsync<SeelTimeoutException>(
            () => Client(seel, TimeSpan.FromSeconds(0.3)).CancelOrderAsync("O-SLOW"));
    }

    [Fact]
    public async Task CallerCancellationIsNotReportedAsATimeout()
    {
        await using var seel = await StubSeel.StartAsync();
        using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(0.2));
        await Assert.ThrowsAnyAsync<OperationCanceledException>(
            () => Client(seel).CancelOrderAsync("O-SLOW", cts.Token));
    }

    [Fact]
    public async Task ApiErrorExposesStatusErrorAndTraceId()
    {
        await using var seel = await StubSeel.StartAsync();
        var exc = await Assert.ThrowsAsync<SeelApiException>(() => Client(seel).CancelOrderAsync("O-REJECT"));
        Assert.Equal((422, "order not found", "trace-1"), (exc.Status, exc.Error, exc.TraceId));
    }

    [Fact]
    public async Task ValidationRunsBeforeSendingAndCanBeTurnedOff()
    {
        await using var seel = await StubSeel.StartAsync();
        var quote = Fixtures.CompletePayload("quote");
        quote.Remove("merchant_id");

        var exc = await Assert.ThrowsAsync<SeelValidationException>(() => Client(seel).CreateQuoteAsync(quote));
        Assert.Equal(["missing required field merchant_id"], exc.Problems);
        Assert.Empty(seel.Received);

        await Client(seel, validate: false).CreateQuoteAsync(quote);
        Assert.Single(seel.Received);
    }
}
