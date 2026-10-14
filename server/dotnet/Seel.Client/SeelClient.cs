// Client for Seel's ecommerce APIs (the direct-integration path).
//
// Wraps the public /v1/ecommerce/* APIs and follows the flow in the SaaS
// Platform Integration Quickstart
// (https://developer.seel.com/docs/platform-direct-integration):
//
//   merchant enables program -> CreateMerchant (+ batch order-history backfill)
//   checkout                 -> CreateQuote (widget renders from the response)
//   order placed             -> CreateOrder (all orders, opted in or not;
//                               seel_services carries quote_id + price on opt-in)
//   order shipped/delivered  -> CreateFulfillment / UpdateFulfillment
//   order changed/cancelled  -> UpdateOrder / CancelOrder
//   return or claim filed    -> CreateClaim (+ UpdateClaim with the decision
//                               when the platform adjudicates)
//   async status changes     -> webhooks (contract.*, claim.*), HMAC-signed
//
// A port of server/python/seel_client.py. Payloads are JsonObject and
// responses JsonNode, so callers are not forced into DTOs.

using System.Text;
using System.Text.Json;
using System.Text.Json.Nodes;

namespace Seel.Client;

/// <summary>Settings for <see cref="SeelClient"/>.</summary>
public sealed record SeelClientOptions
{
    public const string SandboxBaseUrl = "https://api-test.seel.com";
    public const string ProductionBaseUrl = "https://api.seel.com";

    /// <summary>The pinned API version; every language port matches.</summary>
    public const string DefaultApiVersion = "2.6.0";

    public required string ApiKey { get; init; }

    public string BaseUrl { get; init; } = SandboxBaseUrl;

    public string ApiVersion { get; init; } = DefaultApiVersion;

    public TimeSpan Timeout { get; init; } = TimeSpan.FromSeconds(15);

    /// <summary>
    /// Pre-flight payload validation against the strict profile. Turn it off
    /// for an account Seel has relaxed fields for, or to let the API be the
    /// only authority on what a valid payload is.
    /// </summary>
    public bool Validate { get; init; } = true;

    /// <summary>
    /// Post-condition check on create_order: did a contract actually mint?
    /// Separate from <see cref="Validate"/> on purpose. It reports a real
    /// failure the API returns as a 200, not an opinion about required fields,
    /// so turning validation off should not turn this off too.
    /// </summary>
    public bool CheckContract { get; init; } = true;
}

/// <summary>
/// One method per /v1/ecommerce/* endpoint. Register it as a typed client
/// (<c>services.AddHttpClient&lt;SeelClient&gt;()</c>) or construct it over
/// any <see cref="HttpClient"/>.
/// </summary>
public sealed class SeelClient(HttpClient http, SeelClientOptions options)
{
    private readonly string _baseUrl = options.BaseUrl.TrimEnd('/');

    public SeelClientOptions Options { get; } = options;

    // -- Merchants ----------------------------------------------------------

    /// <summary>
    /// Onboard one retailer. Call it when they enable the program; retailers
    /// group under your platform organization on Seel's side. Follow with
    /// <see cref="CreateOrdersBatchAsync"/> and at least 30 days of order
    /// history so Seel can run risk analysis.
    /// </summary>
    /// <remarks>
    /// Each seel_services entry needs a coverages key. Omitting it returns a
    /// 500 rather than a validation error, so this checks it first.
    /// </remarks>
    public Task<JsonNode?> CreateMerchantAsync(JsonObject payload, CancellationToken ct = default)
    {
        Check("create_merchant", SeelValidation.ValidateMerchant(payload));
        return SendAsync(HttpMethod.Post, "/ecommerce/merchants", payload, ct);
    }

    /// <summary>
    /// Sync changed protection settings, or disable the program for a
    /// retailer. Include the reason when disabling.
    /// </summary>
    public Task<JsonNode?> UpdateMerchantAsync(string merchantId, JsonObject payload, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Post, $"/ecommerce/merchants/{Segment(merchantId)}", payload, ct);

    public Task<JsonNode?> GetMerchantAsync(string merchantId, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Get, $"/ecommerce/merchants/{Segment(merchantId)}", null, ct);

    // -- Quotes -------------------------------------------------------------

    /// <summary>
    /// Quote a cart at checkout. The response carries everything the
    /// storefront widget renders: price, display_amounts, widget_copy,
    /// extra_info. Re-quote whenever the cart changes: address, discount,
    /// item removed.
    /// </summary>
    /// <remarks>
    /// The README's validation section and
    /// https://developer.seel.com/reference/createquote list the required
    /// fields, including price + sales_tax - allocated_discounts == final_price.
    /// </remarks>
    public Task<JsonNode?> CreateQuoteAsync(JsonObject payload, CancellationToken ct = default)
    {
        Check("create_quote", SeelValidation.ValidateQuote(payload));
        return SendAsync(HttpMethod.Post, "/ecommerce/quotes", payload, ct);
    }

    public Task<JsonNode?> GetQuoteAsync(string quoteId, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Get, $"/ecommerce/quotes/{Segment(quoteId)}", null, ct);

    // -- Orders -------------------------------------------------------------

    /// <summary>Sync every new order, opted in or not.</summary>
    /// <remarks>
    /// On opt-in, seel_services must be an ARRAY of entries carrying type,
    /// quote_id and price from the latest quote. That mints the contract and
    /// fires contract.created. Sending quote_id at the top level instead
    /// returns 200 with seel_services: null and no error, which is why this
    /// method checks the response as well as the request.
    ///
    /// Seel does not check the attach against the quote: a price that does
    /// not match the quoted premium, or line items that differ from the quoted
    /// cart, both still mint a contract. Keeping them consistent is the
    /// caller's job.
    /// </remarks>
    /// <exception cref="SeelContractNotMintedException">The order attached coverage and no contract minted.</exception>
    public async Task<JsonNode?> CreateOrderAsync(JsonObject payload, CancellationToken ct = default)
    {
        Check("create_order", SeelValidation.ValidateOrder(payload));
        var checkContract = Options.CheckContract && SeelValidation.AttachesCoverage(payload);
        JsonNode? response;
        try
        {
            response = await SendAsync(HttpMethod.Post, "/ecommerce/orders", payload, ct).ConfigureAwait(false);
        }
        // A non-JSON 2xx cannot show a contract, so on an attach it is a
        // failed attach (409 at the proxy), not a success to pass through.
        catch (SeelApiException exc) when (checkContract && exc.Status is >= 200 and < 300)
        {
            response = exc.Body;
        }
        if (checkContract)
        {
            SeelValidation.EnsureContractMinted(payload, response);
        }
        return response;
    }

    /// <summary>Backfill order history at onboarding: at least 30 days.</summary>
    public Task<JsonNode?> CreateOrdersBatchAsync(JsonObject payload, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Post, "/ecommerce/orders/batch", payload, ct);

    /// <summary>Sync order changes: line item removed, shipping address updated.</summary>
    public Task<JsonNode?> UpdateOrderAsync(string orderId, JsonObject payload, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Post, $"/ecommerce/orders/{Segment(orderId)}", payload, ct);

    /// <summary>
    /// Cancel a synced order; its WFP coverage cancels with it. Refunding the
    /// WFP fee and tax to the shopper is the platform's job. See Cancellation
    /// in the README.
    /// </summary>
    public Task<JsonNode?> CancelOrderAsync(string orderId, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Post, $"/ecommerce/orders/{Segment(orderId)}/cancel", null, ct);

    // -- Fulfillments -------------------------------------------------------

    /// <summary>Send tracking number and carrier when the order ships.</summary>
    public Task<JsonNode?> CreateFulfillmentAsync(string orderId, JsonObject payload, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Post, $"/ecommerce/orders/{Segment(orderId)}/fulfillments", payload, ct);

    /// <summary>Update tracking or delivery status after fulfillment.</summary>
    public Task<JsonNode?> UpdateFulfillmentAsync(
        string orderId, string fulfillmentId, JsonObject payload, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Post,
            $"/ecommerce/orders/{Segment(orderId)}/fulfillments/{Segment(fulfillmentId)}", payload, ct);

    // -- Claims -------------------------------------------------------------

    /// <summary>
    /// Register a claim when the shopper files in the platform's returns flow.
    /// Delivery-issue claims carry claim_type loss | damage | theft | delay
    /// plus claim_details with attachments; return-shipping claims carry
    /// claim_type return_shipping plus the RMA number, the return shipment
    /// (carrier, tracking, label cost), and the return addresses. Seel opens
    /// the claim as pending and fires the claim.created webhook.
    /// </summary>
    public Task<JsonNode?> CreateClaimAsync(JsonObject payload, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Post, "/ecommerce/claims", payload, ct);

    /// <summary>
    /// Submit the adjudication decision on programs where the platform
    /// adjudicates: decision accept | reject, with a reject_reason code and
    /// shopper-facing details on rejections. Claim items and amounts cannot be
    /// changed after creation. Seel records the outcome and fires
    /// claim.accepted or claim.rejected.
    /// </summary>
    public Task<JsonNode?> UpdateClaimAsync(string claimId, JsonObject payload, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Post, $"/ecommerce/claims/{Segment(claimId)}", payload, ct);

    public Task<JsonNode?> GetClaimAsync(string claimId, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Get, $"/ecommerce/claims/{Segment(claimId)}", null, ct);

    // -- Lookups (ad hoc; day-to-day state comes via webhooks) ---------------

    public Task<JsonNode?> GetOrderAsync(string orderId, CancellationToken ct = default) =>
        SendAsync(HttpMethod.Get, $"/ecommerce/orders/{Segment(orderId)}", null, ct);

    public Task<JsonNode?> ListContractsAsync(string query = "", CancellationToken ct = default) =>
        SendAsync(HttpMethod.Get, "/ecommerce/contracts" + (query.Length > 0 ? "?" + query : ""), null, ct);

    public Task<JsonNode?> ListClaimsAsync(string query = "", CancellationToken ct = default) =>
        SendAsync(HttpMethod.Get, "/ecommerce/claims" + (query.Length > 0 ? "?" + query : ""), null, ct);

    // Ids arrive from callers and go straight into the upstream URL. Without
    // escaping, an id containing "/" (or "%2F", which decodes to one) reaches
    // a different endpoint than the method name implies: UpdateOrder with
    // order id "x/cancel" would cancel instead. EscapeDataString leaves only
    // RFC 3986 unreserved characters, the same set Python's quote(safe="")
    // keeps.
    private static string Segment(string value) => Uri.EscapeDataString(value);

    private void Check(string operation, IReadOnlyList<string> problems)
    {
        if (Options.Validate && problems.Count > 0)
        {
            throw new SeelValidationException(operation, problems);
        }
    }

    private async Task<JsonNode?> SendAsync(HttpMethod method, string path, JsonObject? payload, CancellationToken ct)
    {
        using var request = new HttpRequestMessage(method, _baseUrl + "/v1" + path);
        request.Headers.Add("X-Seel-Api-Key", Options.ApiKey);
        request.Headers.Add("X-Seel-Api-Version", Options.ApiVersion);
        if (payload is not null)
        {
            request.Content = new StringContent(payload.ToJsonString(), Encoding.UTF8, "application/json");
        }

        // Our own deadline rather than HttpClient.Timeout, so a timeout is
        // told apart from the caller cancelling, and so it holds whatever
        // timeout the injected HttpClient carries.
        using var deadline = CancellationTokenSource.CreateLinkedTokenSource(ct);
        deadline.CancelAfter(Options.Timeout);
        int status;
        string raw;
        try
        {
            using var response = await http.SendAsync(request, deadline.Token).ConfigureAwait(false);
            status = (int)response.StatusCode;
            raw = await response.Content.ReadAsStringAsync(deadline.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException exc) when (!ct.IsCancellationRequested)
        {
            throw new SeelTimeoutException(Options.Timeout, exc);
        }

        if (status is < 200 or >= 300)
        {
            throw new SeelApiException(status, TryParse(raw, out var body) ? body : null, raw);
        }
        // A 2xx is an acceptance whatever the body looks like, so none of
        // these may surface as a transport failure.
        if (string.IsNullOrWhiteSpace(raw))
        {
            return new JsonObject();
        }
        if (TryParse(raw, out var parsed))
        {
            return parsed;
        }
        throw new SeelApiException(status, new JsonObject { ["seel_raw_body"] = raw }, raw);
    }

    private static bool TryParse(string raw, out JsonNode? node)
    {
        try
        {
            node = JsonNode.Parse(raw);
            return true;
        }
        catch (JsonException)
        {
            node = null;
            return false;
        }
    }
}
