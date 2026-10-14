using System.Text;

namespace Seel.ExampleServer;

public static class PathParams
{
    // Segments that are Seel endpoints in their own right and so can never be
    // an order id. Seel's own collection endpoints live alongside order ids,
    // so an id that equals one of them would reach the collection instead.
    // "batch" is POST /v1/ecommerce/orders/batch, the order-history backfill:
    // routed as an order id it would proxy an unstamped, unvalidated batch
    // write.
    private static readonly HashSet<string> Reserved = ["batch"];

    private static readonly UTF8Encoding StrictUtf8 = new(encoderShouldEmitUTF8Identifier: false, throwOnInvalidBytes: true);

    /// <summary>
    /// Percent-decode one raw path segment, or return null if it escapes.
    /// </summary>
    /// <remarks>
    /// A segment arrives encoded and is interpolated into the upstream URL, so
    /// a decoded "/" would reach a different endpoint than the route implies:
    /// "orders/ORD1%2Fcancel" matches the update-order route and would perform
    /// a cancel. Control characters are refused for the same reason. With one
    /// API key shared across retailers this is a privilege boundary, not a
    /// cosmetic check.
    /// </remarks>
    public static string? SafeDecode(string raw)
    {
        // Decode by hand: Uri.UnescapeDataString leaves a malformed escape as
        // literal text and replaces invalid UTF-8 with U+FFFD, and the other
        // ports refuse both.
        string decoded;
        try
        {
            decoded = Decode(raw);
        }
        // ArgumentException covers DecoderFallbackException (invalid UTF-8)
        // and EncoderFallbackException (a lone surrogate in the input).
        catch (Exception exc) when (exc is FormatException or ArgumentException)
        {
            return null;
        }
        if (decoded.Any(c => c == '/' || c < 0x20 || c == 0x7F) || Reserved.Contains(decoded))
        {
            return null;
        }
        // A segment that is only dots is refused too: a decoded ".." is not a
        // slash, but HTTP clients normalize it away, so "orders/%2E%2E/cancel"
        // leaves this proxy as a request to /v1/ecommerce/cancel, a different
        // endpoint than the route names.
        return decoded.Trim('.').Length == 0 ? null : decoded;
    }

    private static string Decode(string raw)
    {
        var bytes = new List<byte>(raw.Length);
        var literalStart = 0;
        for (var i = 0; i <= raw.Length; i++)
        {
            if (i < raw.Length && raw[i] != '%')
            {
                continue;
            }
            bytes.AddRange(StrictUtf8.GetBytes(raw[literalStart..i]));
            if (i == raw.Length)
            {
                break;
            }
            if (i + 2 >= raw.Length || !Uri.IsHexDigit(raw[i + 1]) || !Uri.IsHexDigit(raw[i + 2]))
            {
                throw new FormatException("malformed percent escape");
            }
            bytes.Add(Convert.ToByte(raw.Substring(i + 1, 2), 16));
            i += 2;
            literalStart = i + 1;
        }
        return StrictUtf8.GetString(bytes.ToArray());
    }
}
