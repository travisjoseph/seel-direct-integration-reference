using System.Text.Json.Nodes;

namespace Seel.Client;

/// <summary>
/// Thrown on any non-2xx response. Carries the status and Seel's JSON error
/// body: the message names the offending field, and the trace_id is what Seel
/// support will ask for.
/// </summary>
/// <remarks>
/// Also thrown on a 2xx whose body is not JSON, with the status passed through
/// and the text under <c>Body["seel_raw_body"]</c>, so a caller can tell "Seel
/// accepted it and said something unparseable" from a failure.
/// </remarks>
public sealed class SeelApiException(int status, JsonNode? body, string rawBody)
    : Exception($"Seel API {status}: {DescribeError(body, rawBody)}")
{
    public int Status { get; } = status;

    /// <summary>The parsed response body, or null when it was not JSON.</summary>
    public JsonNode? Body { get; } = body;

    /// <summary>The response body as text.</summary>
    public string RawBody { get; } = rawBody;

    public string? Error => (Body as JsonObject)?["error"]?.ToString();

    public string? TraceId => (Body as JsonObject)?["trace_id"]?.ToString();

    private static string DescribeError(JsonNode? body, string rawBody) => body switch
    {
        JsonObject obj when obj.TryGetPropertyValue("error", out var error) => error?.ToString() ?? "null",
        JsonNode node => node.ToJsonString(),
        null => rawBody,
    };
}

/// <summary>
/// Thrown when Seel did not answer within the timeout.
/// </summary>
/// <remarks>
/// Distinct from a connection failure on purpose: the request may have reached
/// Seel and been processed, so a blind retry can duplicate an order. Look the
/// order up before retrying.
/// </remarks>
public sealed class SeelTimeoutException(TimeSpan timeout, Exception? inner = null)
    : Exception($"Seel did not answer within {timeout.TotalSeconds:0.###}s", inner);

/// <summary>
/// Thrown before any request when a payload is missing fields Seel requires.
/// Carries the full list, so one call reports every problem instead of the API
/// reporting them one 400 at a time.
/// </summary>
public sealed class SeelValidationException(string operation, IReadOnlyList<string> problems)
    : Exception($"{operation}: {string.Join("; ", problems)}. "
                + "Set SeelClientOptions.Validate to false to skip these checks.")
{
    public string Operation { get; } = operation;

    public IReadOnlyList<string> Problems { get; } = problems;
}

/// <summary>
/// Thrown when create_order returns 2xx but no contract was created.
/// </summary>
/// <remarks>
/// Seel reports a failed attach as <c>contract_id: null</c> on an otherwise
/// successful response. There is no error status code. Without this check an
/// integration looks healthy while covering nothing.
/// </remarks>
public sealed class SeelContractNotMintedException(JsonNode? response, string detail)
    : Exception($"order created but no contract was minted: {detail}")
{
    /// <summary>Seel's response to the create_order call.</summary>
    public JsonNode? Response { get; } = response;
}
