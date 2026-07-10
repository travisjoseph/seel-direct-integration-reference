/*
 * Reference Java client for Seel's ecommerce partner APIs (direct path).
 *
 * Wraps the public /v1/ecommerce/* APIs documented at
 * https://developer.seel.com - specifically the flow in the SaaS Platform
 * Integration Quickstart
 * (https://developer.seel.com/docs/platform-direct-integration):
 *
 *   merchant enables program -> createMerchant (+ batch order-history backfill)
 *   checkout                 -> createQuote (widget renders from the response)
 *   order placed             -> createOrder (ALL orders, opted-in or not;
 *                               seel_services carries quote_id + price on opt-in)
 *   order shipped/delivered  -> createFulfillment / updateFulfillment
 *   order changed/cancelled  -> updateOrder / cancelOrder
 *   async status changes     -> webhooks (contract.*, claim.*), HMAC-signed
 *
 * JDK 17+ standard library only, no dependencies - intended to be readable
 * enough to port to any stack.
 *
 * JSON handling: the JDK ships no JSON library, so this client deliberately
 * works in raw JSON strings - every method accepts a JSON string payload and
 * returns Seel's JSON response body as a string. Bring your own JSON library
 * (Jackson, Gson, whatever your stack already uses) to build payloads and
 * parse responses; this file stays dependency-free.
 */

import java.io.IOException;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.MessageDigest;
import java.time.Duration;
import java.util.Base64;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

public class SeelClient {

    public static final String SANDBOX_BASE_URL = "https://api-test.seel.com";
    public static final String PRODUCTION_BASE_URL = "https://api.seel.com";
    /** Single source of truth for the pinned API version. */
    public static final String API_VERSION = "2.6.0";

    private static final int DEFAULT_TIMEOUT_SECONDS = 15;

    private final String apiKey;
    private final String baseUrl;
    private final String apiVersion;
    private final Duration timeout;
    private final HttpClient httpClient;

    public SeelClient(String apiKey) {
        this(apiKey, SANDBOX_BASE_URL, API_VERSION, DEFAULT_TIMEOUT_SECONDS);
    }

    public SeelClient(String apiKey, String baseUrl) {
        this(apiKey, baseUrl, API_VERSION, DEFAULT_TIMEOUT_SECONDS);
    }

    public SeelClient(String apiKey, String baseUrl, String apiVersion, int timeoutSeconds) {
        this.apiKey = apiKey;
        this.baseUrl = baseUrl.replaceAll("/+$", "");
        this.apiVersion = apiVersion;
        this.timeout = Duration.ofSeconds(timeoutSeconds);
        this.httpClient = HttpClient.newBuilder()
                .connectTimeout(this.timeout)
                .build();
    }

    private String request(String method, String path, String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        // Header names are case-insensitive per RFC 9110; HttpClient sends
        // them as given and Seel accepts any casing.
        HttpRequest.BodyPublisher bodyPublisher = payloadJson != null
                ? HttpRequest.BodyPublishers.ofString(payloadJson, StandardCharsets.UTF_8)
                : HttpRequest.BodyPublishers.noBody();
        HttpRequest request = HttpRequest.newBuilder()
                .uri(URI.create(baseUrl + "/v1" + path))
                .timeout(timeout)
                .header("X-Seel-Api-Key", apiKey)
                .header("X-Seel-Api-Version", apiVersion)
                .header("Content-Type", "application/json")
                .method(method, bodyPublisher)
                .build();
        HttpResponse<String> response =
                httpClient.send(request, HttpResponse.BodyHandlers.ofString(StandardCharsets.UTF_8));
        int status = response.statusCode();
        if (status < 200 || status >= 300) {
            throw new SeelApiException(status, response.body());
        }
        return response.body();
    }

    // -- Merchants -----------------------------------------------------------

    /**
     * Onboard one retailer. Called once per retailer when they enable the
     * program (all grouped under your platform organization on Seel's side).
     * Follow with createOrdersBatch for at least 30 days of order history so
     * Seel can run risk analysis.
     */
    public String createMerchant(String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/merchants", payloadJson);
    }

    /**
     * Toggle/disable the program for a retailer (include the reason when
     * disabling), or sync changed protection settings.
     */
    public String updateMerchant(String merchantId, String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/merchants/" + merchantId, payloadJson);
    }

    // -- Quotes --------------------------------------------------------------

    /**
     * Quote a cart at checkout. The response (price, display_amounts,
     * widget_copy, extra_info) carries all the copy the storefront widget
     * renders. Re-quote on cart changes: address change, discount applied,
     * item removed.
     *
     * <p>See the README "Required fields and validation rules" section and
     * https://developer.seel.com/reference/createquote for the full
     * required-field list, including the constraint
     * price + sales_tax - allocated_discounts == final_price.
     */
    public String createQuote(String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/quotes", payloadJson);
    }

    public String getQuote(String quoteId)
            throws SeelApiException, IOException, InterruptedException {
        return request("GET", "/ecommerce/quotes/" + quoteId, null);
    }

    // -- Orders --------------------------------------------------------------

    /**
     * Sync every new order, whether or not the shopper opted in. When they
     * did, include the seel_services object with the quote_id and price from
     * the latest quote - this is what mints the contract and triggers the
     * contract.created webhook. Line items must match the quoted cart.
     */
    public String createOrder(String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/orders", payloadJson);
    }

    /**
     * Backfill historical orders (at least 30 days, at merchant onboarding).
     */
    public String createOrdersBatch(String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/orders/batch", payloadJson);
    }

    /**
     * Sync order changes: line item removed, shipping address updated.
     */
    public String updateOrder(String orderId, String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/orders/" + orderId, payloadJson);
    }

    /**
     * Cancel a synced order; any WFP coverage on it cancels automatically.
     * Refunding the WFP fee + tax to the shopper is the platform's job (see
     * "Cancellation" in the README integration flow).
     */
    public String cancelOrder(String orderId)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/orders/" + orderId + "/cancel", null);
    }

    // -- Fulfillments ----------------------------------------------------------

    /**
     * Send tracking number + carrier when the order ships.
     */
    public String createFulfillment(String orderId, String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/orders/" + orderId + "/fulfillments", payloadJson);
    }

    /**
     * Update tracking/delivery status after fulfillment.
     */
    public String updateFulfillment(String orderId, String fulfillmentId, String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST",
                "/ecommerce/orders/" + orderId + "/fulfillments/" + fulfillmentId, payloadJson);
    }

    // -- Lookups (ad hoc; day-to-day state comes via webhooks) -----------------

    public String getOrder(String orderId)
            throws SeelApiException, IOException, InterruptedException {
        return request("GET", "/ecommerce/orders/" + orderId, null);
    }

    public String listContracts()
            throws SeelApiException, IOException, InterruptedException {
        return listContracts("");
    }

    public String listContracts(String query)
            throws SeelApiException, IOException, InterruptedException {
        String suffix = (query == null || query.isEmpty()) ? "" : "?" + query;
        return request("GET", "/ecommerce/contracts" + suffix, null);
    }

    public String listClaims()
            throws SeelApiException, IOException, InterruptedException {
        return listClaims("");
    }

    public String listClaims(String query)
            throws SeelApiException, IOException, InterruptedException {
        String suffix = (query == null || query.isEmpty()) ? "" : "?" + query;
        return request("GET", "/ecommerce/claims" + suffix, null);
    }

    // -- Webhooks --------------------------------------------------------------

    /**
     * Verify Seel's X-Seel-Hmac-SHA256 header: Base64(HMAC-SHA256(body, secret)).
     *
     * <p>Webhook delivery is at-least-once: ACK with HTTP 200 within 10
     * seconds, dedupe on the payload's id + type (retries reuse the same
     * outer id).
     *
     * <p>Uses MessageDigest.isEqual for a constant-time comparison. The
     * webhookSecret must be non-empty; the JCE rejects empty HMAC keys.
     */
    public static boolean verifyWebhookSignature(byte[] body, String signatureB64,
                                                 String webhookSecret) {
        try {
            Mac mac = Mac.getInstance("HmacSHA256");
            mac.init(new SecretKeySpec(webhookSecret.getBytes(StandardCharsets.UTF_8), "HmacSHA256"));
            byte[] expected = Base64.getEncoder().encode(mac.doFinal(body));
            return MessageDigest.isEqual(expected, signatureB64.getBytes(StandardCharsets.UTF_8));
        } catch (GeneralSecurityException e) {
            // HmacSHA256 is guaranteed on every conforming JVM, so this is
            // unreachable in practice.
            throw new IllegalStateException("HmacSHA256 unavailable", e);
        }
    }
}

/**
 * Thrown on any non-2xx response. Carries the HTTP status and Seel's raw
 * JSON error body, which includes the actionable message (e.g. which
 * required field is missing) and a trace_id to quote in support requests.
 * Parse the body with your JSON library if you need the individual fields.
 *
 * <p>Package-private top-level class so that a plain
 * {@code javac SeelClient.java ExampleServer.java} works with no package
 * declaration; move it to its own file if you adopt a package structure.
 */
class SeelApiException extends Exception {

    private final int status;
    private final String body;

    public SeelApiException(int status, String body) {
        super("Seel API " + status + ": " + body);
        this.status = status;
        this.body = body;
    }

    public int getStatus() {
        return status;
    }

    public String getBody() {
        return body;
    }
}
