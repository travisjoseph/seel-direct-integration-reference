using System.Text.Json;
using Seel.Client;

namespace Seel.ExampleServer;

/// <summary>Everything the example server reads from the environment.</summary>
public sealed record ServerConfig(
    string Host,
    int Port,
    SeelClientOptions Seel,
    string WebhookSecret,
    string? QuoteType,
    MerchantBinding Binding)
{
    /// <summary>
    /// Read the process environment. No .env file is loaded, on purpose.
    /// </summary>
    /// <exception cref="FormatException">SEEL_RETAILER_MERCHANTS or PORT is malformed.</exception>
    /// <exception cref="InvalidOperationException">SEEL_MERCHANT_ID and SEEL_RETAILER_MERCHANTS are both set.</exception>
    public static ServerConfig FromEnvironment(Func<string, string?> env)
    {
        string? Read(string name) => env(name) is { Length: > 0 } value ? value : null;
        return new ServerConfig(
            // Loopback by default: this demo authenticates nobody, so it must
            // not be reachable from the network unless someone chooses that.
            Host: Read("HOST") ?? "127.0.0.1",
            Port: int.Parse(Read("PORT") ?? "8787", System.Globalization.CultureInfo.InvariantCulture),
            Seel: new SeelClientOptions
            {
                ApiKey = Read("SEEL_API_KEY") ?? "",
                BaseUrl = Read("SEEL_BASE_URL") ?? SeelClientOptions.SandboxBaseUrl,
            },
            WebhookSecret: Read("SEEL_WEBHOOK_SECRET") ?? "",
            // Program values from Seel onboarding. When set, the proxy stamps
            // them into every quote request, so storefront code stays
            // identical across programs.
            QuoteType: Read("SEEL_QUOTE_TYPE"),
            Binding: MerchantBinding.From(Read("SEEL_MERCHANT_ID"), Read("SEEL_RETAILER_MERCHANTS")));
    }
}

/// <summary>
/// How a request is bound to a Seel merchant. Exactly one mode applies, so
/// "both SEEL_MERCHANT_ID and SEEL_RETAILER_MERCHANTS" cannot be represented.
/// </summary>
public abstract record MerchantBinding
{
    private MerchantBinding()
    {
    }

    /// <summary>SEEL_MERCHANT_ID: one merchant stamped onto every request.</summary>
    public sealed record SingleRetailer(string MerchantId) : MerchantBinding;

    /// <summary>SEEL_RETAILER_MERCHANTS: normalized storefront origin to merchant ID.</summary>
    public sealed record RetailerTable(IReadOnlyDictionary<string, string> Merchants) : MerchantBinding;

    /// <summary>Neither set: the body's merchant_id is forwarded. Only a local demo may run this way.</summary>
    public sealed record FromBody : MerchantBinding;

    /// <exception cref="FormatException">The table is malformed.</exception>
    /// <exception cref="InvalidOperationException">Both values are set.</exception>
    public static MerchantBinding From(string? merchantId, string? rawTable)
    {
        var table = rawTable is null ? null : ParseRetailerMerchants(rawTable);
        // Both set is a contradiction, not a precedence question:
        // SEEL_MERCHANT_ID would silently stamp one merchant onto every
        // retailer in the table.
        if (merchantId is not null && table is not null)
        {
            throw new InvalidOperationException("set SEEL_MERCHANT_ID or SEEL_RETAILER_MERCHANTS, not both");
        }
        if (merchantId is not null)
        {
            return new SingleRetailer(merchantId);
        }
        return table is null ? new FromBody() : new RetailerTable(table);
    }

    /// <summary>
    /// Parse SEEL_RETAILER_MERCHANTS into {storefront origin: merchant_id}.
    /// </summary>
    /// <remarks>
    /// A bad table must stop startup: running on with a partial or empty one
    /// would refuse every retailer, or worse, bind a storefront to the wrong
    /// merchant.
    /// </remarks>
    /// <exception cref="FormatException">Anything malformed.</exception>
    public static IReadOnlyDictionary<string, string> ParseRetailerMerchants(string raw)
    {
        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(raw);
        }
        catch (JsonException exc)
        {
            throw new FormatException($"SEEL_RETAILER_MERCHANTS is not valid JSON: {exc.Message}");
        }
        using (document)
        {
            if (document.RootElement.ValueKind != JsonValueKind.Object)
            {
                throw new FormatException("SEEL_RETAILER_MERCHANTS must be a JSON object of origin to merchant_id");
            }
            var parsed = new Dictionary<string, string>();
            foreach (var entry in document.RootElement.EnumerateObject())
            {
                if (entry.Value.ValueKind != JsonValueKind.String || entry.Value.GetString() is not { Length: > 0 } merchantId)
                {
                    throw new FormatException($"SEEL_RETAILER_MERCHANTS[\"{entry.Name}\"] must be a non-empty string");
                }
                var origin = NormalizeOrigin(entry.Name);
                if (parsed.TryGetValue(origin, out var existing) && existing != merchantId)
                {
                    throw new FormatException($"SEEL_RETAILER_MERCHANTS maps \"{origin}\" to two merchants");
                }
                parsed[origin] = merchantId;
            }
            return parsed;
        }
    }

    public static string NormalizeOrigin(string origin) => origin.Trim().ToLowerInvariant().TrimEnd('/');

    /// <summary>
    /// The Access-Control-Allow-Origin value for a request, or null to omit it.
    /// </summary>
    /// <remarks>
    /// With a retailer table, only a registered storefront may read responses,
    /// so the header echoes that origin and is omitted for any other.
    /// </remarks>
    public string? AllowedOrigin(string? requestOrigin)
    {
        if (this is not RetailerTable table)
        {
            return "*";
        }
        var origin = NormalizeOrigin(requestOrigin ?? "");
        return table.Merchants.ContainsKey(origin) ? origin : null;
    }

    /// <summary>Whether responses vary by Origin and so need Vary: Origin.</summary>
    public bool VariesByOrigin => this is RetailerTable;
}
