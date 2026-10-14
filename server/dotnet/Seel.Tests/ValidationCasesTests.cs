using System.Text.Json.Nodes;
using Seel.Client;

namespace Seel.Tests;

/// <summary>
/// Runs server/validation-cases.json, which every port reads. The cases exist
/// to catch drift: the ports must make the same accept/reject decision and
/// report the same field paths.
/// </summary>
public class ValidationCasesTests
{
    public static TheoryData<string> Cases => Fixtures.Names(Fixtures.Validation, "cases");

    public static TheoryData<string> ContractCases => Fixtures.Names(Fixtures.Validation, "contract_cases");

    public static TheoryData<string> UncoveredRequests => Fixtures.Names(Fixtures.Validation, "uncovered_requests");

    // The fixture is the single point of failure for cross-port drift, so an
    // emptied or defanged one has to fail rather than pass green.
    [Fact]
    public void FixtureIsPopulated()
    {
        Assert.NotEmpty(Cases);
        Assert.NotEmpty(ContractCases);
        Assert.NotEmpty(UncoveredRequests);
        foreach (var testCase in Fixtures.Section(Fixtures.Validation, "cases"))
        {
            if (testCase!["expect_clean"]?.GetValue<bool>() != true)
            {
                Assert.NotEmpty(testCase["expect_contains"]!.AsArray());
            }
        }
    }

    [Theory]
    [MemberData(nameof(Cases))]
    public void SharedCase(string name)
    {
        var testCase = Fixtures.Case(Fixtures.Validation, "cases", name);
        var payload = testCase["payload"]!.AsObject();
        var problems = (string)testCase["operation"]! switch
        {
            "quote" => SeelValidation.ValidateQuote(payload),
            "order" => SeelValidation.ValidateOrder(payload),
            "merchant" => SeelValidation.ValidateMerchant(payload),
            var other => throw new InvalidOperationException($"unknown operation {other}"),
        };
        if (testCase["expect_clean"]?.GetValue<bool>() == true)
        {
            Assert.Empty(problems);
            return;
        }
        Assert.NotEmpty(problems);
        var joined = string.Join("; ", problems);
        foreach (var fragment in testCase["expect_contains"]!.AsArray())
        {
            Assert.Contains((string)fragment!, joined);
        }
    }

    [Theory]
    [MemberData(nameof(ContractCases))]
    public void ContractCase(string name)
    {
        var testCase = Fixtures.Case(Fixtures.Validation, "contract_cases", name);
        var minted = true;
        try
        {
            SeelValidation.EnsureContractMinted(testCase["request"]!.AsObject(), testCase["response"]);
        }
        catch (SeelContractNotMintedException)
        {
            minted = false;
        }
        Assert.Equal(testCase["expect_minted"]!.GetValue<bool>(), minted);
    }

    // Calls the predicate CreateOrderAsync gates the check on, so a change to
    // when the check runs fails here.
    [Theory]
    [MemberData(nameof(UncoveredRequests))]
    public void ContractCheckRunsOnlyWhenCoverageIsAttached(string name)
    {
        var testCase = Fixtures.Case(Fixtures.Validation, "uncovered_requests", name);
        Assert.Equal(
            testCase["expect_checked"]!.GetValue<bool>(),
            SeelValidation.AttachesCoverage(testCase["request"]!.AsObject()));
    }

    [Fact]
    public async Task CreateOrderSkipsTheContractCheckOnAnUncoveredOrder()
    {
        await using var seel = await StubSeel.StartAsync();
        var client = new SeelClient(new HttpClient(), new SeelClientOptions { ApiKey = "k", BaseUrl = seel.BaseUrl });
        var response = await client.CreateOrderAsync(Fixtures.CompletePayload("order"));
        Assert.Equal("[]", response!.ToJsonString());
    }

    [Fact]
    public async Task CreateOrderChecksTheContractOnAnAttach()
    {
        await using var seel = await StubSeel.StartAsync();
        var client = new SeelClient(new HttpClient(), new SeelClientOptions { ApiKey = "k", BaseUrl = seel.BaseUrl });
        var order = Fixtures.CompletePayload("order");
        order["seel_services"] = new JsonArray(new JsonObject { ["type"] = "acme-wfp", ["quote_id"] = "q-1", ["price"] = 0.98 });
        await Assert.ThrowsAsync<SeelContractNotMintedException>(() => client.CreateOrderAsync(order));

        var lenient = new SeelClient(new HttpClient(),
            new SeelClientOptions { ApiKey = "k", BaseUrl = seel.BaseUrl, CheckContract = false });
        Assert.Equal("[]", (await lenient.CreateOrderAsync(order))!.ToJsonString());
    }
}
