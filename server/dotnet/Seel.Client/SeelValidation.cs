using System.Text.Json;
using System.Text.Json.Nodes;

namespace Seel.Client;

/// <summary>
/// Advisory payload validation and the create_order contract-minted check.
/// </summary>
/// <remarks>
/// Required-field sets were measured against sandbox on 2026-09-10 by removing
/// one field per request from a known-good payload and recording the response.
///
/// Treat these as a starting point, not a fixed contract. A newly provisioned
/// account behaves this way; as an integration develops, Seel's implementation
/// team works out which fields a merchant journey can actually supply and eases
/// the validation accordingly, so an established account may accept less.
/// Sending the full set is never wrong, but rejecting a payload locally could
/// be, which is why validation is advisory and
/// <see cref="SeelClientOptions.Validate"/> turns it off.
/// </remarks>
public static class SeelValidation
{
    // A "[]" suffix means the rule applies to every element of that array.
    private static readonly (string Scope, string[] Fields)[] CartRequired =
    [
        ("customer", ["customer_id", "email"]),
        ("shipping_address", ["address_1", "city", "state", "zipcode", "country"]),
        ("line_items[]", ["line_item_id", "product_id", "product_title", "quantity", "price",
                          "allocated_discounts", "sales_tax", "final_price", "currency",
                          "requires_shipping", "image_urls", "category_1", "category_2",
                          "is_final_sale", "shipping_origin"]),
        ("line_items[].shipping_origin", ["country"]),
    ];

    private static readonly (string Scope, string[] Fields)[] QuoteRequired =
    [
        ("", ["merchant_id", "session_id", "device_category", "device_platform", "type",
              "is_default_on", "customer", "shipping_address", "line_items"]),
        .. CartRequired,
    ];

    private static readonly (string Scope, string[] Fields)[] OrderRequired =
    [
        ("", ["merchant_id", "order_id", "order_number", "created_ts", "session_id",
              "device_category", "device_platform", "customer", "shipping_address", "line_items"]),
        .. CartRequired,
    ];

    // Only checked when seel_services is a non-empty array: an order with no
    // coverage is a normal sync, not an error.
    private static readonly (string Scope, string[] Fields) OrderServicesRequired =
        ("seel_services[]", ["type", "quote_id", "price"]);

    // coverages must be PRESENT but may be an empty list. Omitting it returns a
    // 500 rather than a validation error, so catching it locally is the whole
    // point of validating this call.
    private static readonly (string Scope, string[] Fields)[] MerchantRequired =
    [
        ("", ["shop_id", "admin_domain", "shop_domain", "shop_platform", "shop_currency",
              "shop_name", "contact_name", "contact_email", "seel_services"]),
        ("seel_services[]", ["type", "coverages"]),
    ];

    private enum Shape { Object, Array, NonEmptyArray }

    // Shape expectations, checked alongside presence. A scalar where an object
    // belongs is the archetypal payload mistake, and without this the nested
    // rules silently skip it: ResolveScope only descends into objects, so
    // {"customer": "nope"} would report no problems at all. The same goes for a
    // scalar inside an array, so every array here is checked for object entries
    // too.
    private static readonly (string Scope, string Key, Shape Kind)[] QuoteShapes =
    [
        ("", "customer", Shape.Object),
        ("", "shipping_address", Shape.Object),
        ("", "line_items", Shape.NonEmptyArray),
        ("line_items[]", "shipping_origin", Shape.Object),
    ];

    private static readonly (string Scope, string Key, Shape Kind)[] OrderShapes =
        [.. QuoteShapes, ("", "seel_services", Shape.Array)];

    private static readonly (string Scope, string Key, Shape Kind)[] MerchantShapes =
        [("", "seel_services", Shape.NonEmptyArray)];

    /// <summary>Return the problems with a Create Quote payload.</summary>
    public static IReadOnlyList<string> ValidateQuote(JsonObject payload) =>
        [.. Missing(payload, QuoteRequired), .. CheckShapes(payload, QuoteShapes)];

    /// <summary>Return the problems with a Create Order payload.</summary>
    /// <remarks>
    /// seel_services is only checked for completeness when present: syncing an
    /// order the shopper did not opt into is normal. Two shape mistakes are
    /// checked separately, because the API accepts both and then fails in ways
    /// that do not look like failures.
    /// </remarks>
    public static IReadOnlyList<string> ValidateOrder(JsonObject payload)
    {
        var rules = payload["seel_services"] is JsonArray { Count: > 0 }
            ? [.. OrderRequired, OrderServicesRequired]
            : OrderRequired;
        List<string> problems = [.. Missing(payload, rules), .. CheckShapes(payload, OrderShapes)];

        // Create Order has no top-level quote_id. Sending one is the classic
        // attach mistake: the API returns 200 with seel_services: null and no
        // error, so the integration looks healthy while covering nothing.
        if (payload.ContainsKey("quote_id"))
        {
            problems.Add("quote_id must go inside a seel_services entry, not at the top level - "
                         + "a top-level quote_id is ignored and the order attaches no coverage");
        }
        return problems;
    }

    /// <summary>Return the problems with a Create Merchant payload.</summary>
    public static IReadOnlyList<string> ValidateMerchant(JsonObject payload) =>
        [.. Missing(payload, MerchantRequired), .. CheckShapes(payload, MerchantShapes)];

    /// <summary>
    /// Whether create_order checks its response for a minted contract: only
    /// when the request attached coverage, meaning seel_services is a non-empty
    /// array.
    /// </summary>
    public static bool AttachesCoverage(JsonObject request) =>
        request["seel_services"] is JsonArray { Count: > 0 };

    /// <summary>
    /// Fail loudly when an attach silently did not take.
    /// </summary>
    /// <remarks>
    /// Every failed attach observed so far is <c>contract_id: null</c> on a 200
    /// rather than a status code, so nothing else in the stack notices.
    /// </remarks>
    /// <exception cref="SeelContractNotMintedException">No contract was minted.</exception>
    public static void EnsureContractMinted(JsonObject request, JsonNode? response)
    {
        if (response is not JsonObject body)
        {
            throw new SeelContractNotMintedException(
                response, $"response body is {TypeName(response)}, not an object");
        }
        if (body["seel_services"] is not JsonArray { Count: > 0 } services)
        {
            var sent = (request["seel_services"] as JsonArray)?.Count ?? 0;
            throw new SeelContractNotMintedException(
                response,
                $"sent {sent} seel_services {(sent == 1 ? "entry" : "entries")}, response "
                + $"seel_services is {Render(body["seel_services"])}. Check seel_services is a "
                + "list and quote_id is inside it, not at the top level.");
        }
        foreach (var entry in services)
        {
            // A non-object entry is a failure, not something to skip. The
            // ports have to agree here or the check silently does nothing in
            // one of them.
            if (entry is not JsonObject service)
            {
                throw new SeelContractNotMintedException(
                    response, $"seel_services contains a non-object entry: {Render(entry)}");
            }
            // Minted means a non-empty string id. Anything else ([] or {}
            // included) is a failed attach, however truthy it looks.
            var contractId = service["contract_id"];
            if (contractId is JsonValue value && value.GetValueKind() == JsonValueKind.String
                && value.GetValue<string>().Length > 0)
            {
                continue;
            }
            throw new SeelContractNotMintedException(
                response,
                $"service {Render(service["type"])} returned contract_id={Render(contractId)} "
                + $"(status={Render(service["status"])}, error={Render(service["error"])})");
        }
    }

    // One vocabulary for type names across every port, so the same mistake
    // reads the same way whichever one a partner runs.
    private static string TypeName(JsonNode? node) => node?.GetValueKind() switch
    {
        JsonValueKind.Object => "object",
        JsonValueKind.Array => "array",
        JsonValueKind.String => "string",
        JsonValueKind.True or JsonValueKind.False => "boolean",
        JsonValueKind.Number => "number",
        _ => "null",
    };

    private static string Render(JsonNode? node) => node?.ToJsonString() ?? "null";

    // False and 0 are real values: is_default_on, requires_shipping and
    // allocated_discounts all legitimately take them. An empty list is a real
    // value too: merchant coverages: [] is accepted.
    private static bool IsAbsent(JsonNode? value) =>
        value is null
        || (value is JsonValue v && v.GetValueKind() == JsonValueKind.String && v.GetValue<string>().Length == 0);

    private static IEnumerable<string> Missing(JsonObject payload, (string Scope, string[] Fields)[] rules) =>
        from rule in rules
        from scoped in ResolveScope(payload, rule.Scope)
        from field in rule.Fields
        where !scoped.Node.TryGetPropertyValue(field, out var value) || IsAbsent(value)
        select $"missing required field {scoped.Prefix}{field}";

    private static IEnumerable<string> CheckShapes(JsonObject payload, (string Scope, string Key, Shape Kind)[] specs)
    {
        foreach (var (scope, key, kind) in specs)
        {
            foreach (var (node, prefix) in ResolveScope(payload, scope))
            {
                // Absence is the required-field check's job.
                if (node[key] is not { } value)
                {
                    continue;
                }
                var path = prefix + key;
                if (kind == Shape.Object)
                {
                    if (value is not JsonObject)
                    {
                        yield return $"{path} must be an object, got {TypeName(value)}";
                    }
                }
                else if (value is not JsonArray items)
                {
                    yield return $"{path} must be an array, got {TypeName(value)}";
                }
                else if (kind == Shape.NonEmptyArray && items.Count == 0)
                {
                    yield return $"{path} must not be empty";
                }
                else
                {
                    for (var i = 0; i < items.Count; i++)
                    {
                        if (items[i] is not JsonObject)
                        {
                            yield return $"{path}[{i}] must be an object, got {TypeName(items[i])}";
                        }
                    }
                }
            }
        }
    }

    // Scopes are dotted paths where a "[]" suffix fans out over an array. Only
    // objects are descended into; shape problems are reported separately.
    private static List<(JsonObject Node, string Prefix)> ResolveScope(JsonObject payload, string scope)
    {
        List<(JsonObject Node, string Prefix)> nodes = [(payload, "")];
        if (scope.Length == 0)
        {
            return nodes;
        }
        foreach (var part in scope.Split('.'))
        {
            var fanOut = part.EndsWith("[]", StringComparison.Ordinal);
            var key = fanOut ? part[..^2] : part;
            List<(JsonObject, string)> next = [];
            foreach (var (node, prefix) in nodes)
            {
                switch (node[key])
                {
                    case JsonArray items when fanOut:
                        for (var i = 0; i < items.Count; i++)
                        {
                            if (items[i] is JsonObject item)
                            {
                                next.Add((item, $"{prefix}{key}[{i}]."));
                            }
                        }
                        break;
                    case JsonObject child when !fanOut:
                        next.Add((child, $"{prefix}{key}."));
                        break;
                }
            }
            nodes = next;
        }
        return nodes;
    }
}
