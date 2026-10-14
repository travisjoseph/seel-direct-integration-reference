using System.Security.Cryptography;
using System.Text;

namespace Seel.Client;

/// <summary>Webhook signature verification.</summary>
public static class SeelWebhooks
{
    public const string SignatureHeader = "X-Seel-Hmac-SHA256";

    /// <summary>
    /// Verify Seel's X-Seel-Hmac-SHA256 header: Base64(HMAC-SHA256(body, secret)).
    /// </summary>
    /// <remarks>
    /// Webhook delivery is at-least-once: ACK with HTTP 200 within 10 seconds,
    /// dedupe on the payload's id + type (retries reuse the same outer id).
    ///
    /// An empty secret always fails. It is a valid HMAC key, so accepting it
    /// would authenticate anyone who signs with "".
    /// </remarks>
    /// <param name="body">The raw request body, exactly as received.</param>
    /// <param name="signature">The header value, or null when it was absent.</param>
    /// <param name="secret">The webhook secret Seel issued with the API key.</param>
    public static bool VerifySignature(ReadOnlySpan<byte> body, string? signature, string secret)
    {
        if (string.IsNullOrEmpty(secret) || string.IsNullOrEmpty(signature))
        {
            return false;
        }
        var expected = Encoding.ASCII.GetBytes(
            Convert.ToBase64String(HMACSHA256.HashData(Encoding.UTF8.GetBytes(secret), body)));
        return CryptographicOperations.FixedTimeEquals(expected, Encoding.UTF8.GetBytes(signature));
    }
}
