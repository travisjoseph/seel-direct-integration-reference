using System.Text.Json.Nodes;

namespace Seel.ExampleServer;

/// <summary>Who is calling. Origin is the normalized Origin header, or null.</summary>
public sealed record Caller(string? Origin);

/// <summary>The request cannot be bound to a merchant; the proxy answers 403.</summary>
public sealed class MerchantBindingException(string message) : Exception(message);

/// <summary>
/// The two seams a partner platform replaces. Register its own implementation in DI
/// in place of <see cref="OriginRetailerResolver"/>.
/// </summary>
public interface IRetailerResolver
{
    /// <summary>Identify the caller, or return null to answer 401.</summary>
    Caller? AuthenticateCaller(HttpRequest request);

    /// <summary>
    /// Return the merchant ID to stamp into the body, or null to leave the
    /// body's own value alone.
    /// </summary>
    /// <exception cref="MerchantBindingException">The request cannot be bound to a merchant.</exception>
    string? ResolveMerchantId(Caller caller, JsonObject body);
}

/// <summary>The demo's resolver: the Origin header is the caller's identity.</summary>
public sealed class OriginRetailerResolver(MerchantBinding binding) : IRetailerResolver
{
    /// <summary>Identify the caller, or return null to answer 401.</summary>
    /// <remarks>
    /// Browser quote requests come from a retailer's storefront, and the
    /// browser sets Origin itself; page script cannot change it. That makes
    /// the origin the caller's identity for the quote route, and
    /// <see cref="ResolveMerchantId"/> looks it up in SEEL_RETAILER_MERCHANTS.
    /// A non-browser client can send any Origin it likes, so this binds what a
    /// shopper's browser can do, not what a script with the proxy URL can.
    ///
    /// Server-to-server routes (orders, fulfillments, cancel) have no Origin.
    /// This demo accepts them from anyone, which is only safe because it holds
    /// a sandbox key and listens on localhost. A partner platform authenticates its
    /// retailers' backends with its own credentials, returns the retailer
    /// identity here, and maps it to a merchant the same way the origin is
    /// mapped. Retailers never receive a Seel API key: one key covers every
    /// retailer on the partner platform and would let any holder act as any other.
    /// </remarks>
    public Caller? AuthenticateCaller(HttpRequest request)
    {
        var origin = request.Headers.Origin.FirstOrDefault();
        return new Caller(string.IsNullOrEmpty(origin) ? null : MerchantBinding.NormalizeOrigin(origin));
    }

    /// <summary>Return the merchant ID to stamp, or throw <see cref="MerchantBindingException"/>.</summary>
    /// <remarks>
    /// Single retailer: SEEL_MERCHANT_ID is set once in the environment and
    /// stamped onto everything, so storefront and pipeline code carry no
    /// program-specific values.
    ///
    /// Partner platform proxy: SEEL_RETAILER_MERCHANTS maps each storefront origin to
    /// its merchant. The merchant comes from the caller, never the body, which
    /// is what stops one retailer quoting or ordering against another's
    /// merchant ID. A body merchant_id that disagrees is refused rather than
    /// overwritten, so a misconfigured storefront fails loudly. The origin is
    /// the only identity this demo knows, so with a table set a backend call
    /// without one, such as create order, is refused until the partner platform maps
    /// its own retailer credentials here too.
    ///
    /// With neither set, the body value is forwarded so the unconfigured demo
    /// still runs. Any caller can then name any merchant.
    /// </remarks>
    public string? ResolveMerchantId(Caller caller, JsonObject body)
    {
        switch (binding)
        {
            case MerchantBinding.SingleRetailer single:
                return single.MerchantId;
            case MerchantBinding.RetailerTable table:
                if (caller.Origin is null || !table.Merchants.TryGetValue(caller.Origin, out var merchantId))
                {
                    throw new MerchantBindingException("request origin is not a registered storefront");
                }
                if (body.TryGetPropertyValue("merchant_id", out var claimed)
                    && !(claimed is JsonValue value && value.TryGetValue<string>(out var text) && text == merchantId))
                {
                    throw new MerchantBindingException(
                        "merchant_id in the request body does not match the merchant for this storefront");
                }
                return merchantId;
            default:
                return null;
        }
    }
}
