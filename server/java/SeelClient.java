/*
 * Java client for Seel's ecommerce APIs (the direct-integration path).
 *
 * Wraps the public /v1/ecommerce/* APIs and follows the flow in the SaaS
 * Platform Integration Quickstart
 * (https://developer.seel.com/docs/platform-direct-integration):
 *
 *   merchant enables program -> createMerchant (+ batch order-history backfill)
 *   checkout                 -> createQuote (widget renders from the response)
 *   order placed             -> createOrder (all orders, opted in or not;
 *                               seel_services carries quote_id + price on opt-in)
 *   order shipped/delivered  -> createFulfillment / updateFulfillment
 *   order changed/cancelled  -> updateOrder / cancelOrder
 *   return or claim filed    -> createClaim (+ updateClaim with the decision
 *                               when the platform adjudicates)
 *   async status changes     -> webhooks (contract.*, claim.*), HMAC-signed
 *
 * JDK 17+ standard library only, no dependencies.
 *
 * JSON: the JDK ships none, so every method takes a JSON string payload and
 * returns Seel's JSON response as a string. Bring your own JSON library
 * (Jackson, Gson, whatever you already use) to build payloads and parse
 * responses; this file stays dependency-free.
 */

import java.io.IOException;
import java.net.URI;
import java.net.URLEncoder;
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

    /**
     * Percent-encode one path segment.
     *
     * <p>Ids come from callers and go straight into the upstream URL.
     * Without this, an id containing "/" (or "%2F", which decodes to one)
     * reaches a different endpoint than the method name implies: an
     * updateOrder call with order id "x/cancel" would cancel instead.
     *
     * <p>URLEncoder is form-encoding, so "+" and the characters it leaves
     * alone are corrected afterwards to give true path-segment encoding.
     */
    static String pathParam(String value) {
        return URLEncoder.encode(value, StandardCharsets.UTF_8)
                .replace("+", "%20")
                .replace("*", "%2A")
                .replace("%7E", "~");
    }

    public static final String SANDBOX_BASE_URL = "https://api-test.seel.com";
    public static final String PRODUCTION_BASE_URL = "https://api.seel.com";
    /** The pinned API version; all four language ports match. */
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
     * Onboard one retailer. Call it when they enable the program; retailers
     * group under your platform organization on Seel's side. Follow with
     * createOrdersBatch and at least 30 days of order history so Seel can
     * run risk analysis.
     *
     * <p>Each seel_services entry needs a coverages key. Omitting it returns
     * a 500 rather than a validation error - see
     * {@link SeelValidation#validateMerchant}.
     */
    public String createMerchant(String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/merchants", payloadJson);
    }

    /**
     * Sync changed protection settings, or disable the program for a
     * retailer - include the reason when disabling.
     */
    public String updateMerchant(String merchantId, String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/merchants/" + pathParam(merchantId), payloadJson);
    }

    // -- Quotes --------------------------------------------------------------

    /**
     * Quote a cart at checkout. The response carries everything the
     * storefront widget renders: price, display_amounts, widget_copy,
     * extra_info. Re-quote whenever the cart changes - address, discount,
     * item removed.
     *
     * <p>The README's validation section and
     * https://developer.seel.com/reference/createquote list the required
     * fields, including
     * price + sales_tax - allocated_discounts == final_price.
     */
    public String createQuote(String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/quotes", payloadJson);
    }

    public String getQuote(String quoteId)
            throws SeelApiException, IOException, InterruptedException {
        return request("GET", "/ecommerce/quotes/" + pathParam(quoteId), null);
    }

    // -- Orders --------------------------------------------------------------

    /**
     * Sync every new order, opted in or not.
     *
     * <p>On opt-in, seel_services must be an ARRAY of entries carrying type,
     * quote_id and price from the latest quote - that mints the contract and
     * fires contract.created. Sending quote_id at the top level instead
     * returns 200 with seel_services: null and no error, so check the
     * response for a non-null contract_id rather than trusting the status
     * code.
     *
     * <p>Seel does not check the attach against the quote: a price that does
     * not match the quoted premium, or line items that differ from the
     * quoted cart, both still mint a contract. Keeping them consistent is
     * the caller's job.
     *
     * <p>See {@link SeelValidation#validateOrder} to catch these before
     * sending.
     */
    public String createOrder(String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/orders", payloadJson);
    }

    /**
     * Backfill order history at onboarding - at least 30 days.
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
        return request("POST", "/ecommerce/orders/" + pathParam(orderId), payloadJson);
    }

    /**
     * Cancel a synced order; its WFP coverage cancels with it. Refunding the
     * WFP fee and tax to the shopper is the platform's job - see
     * Cancellation in the README.
     */
    public String cancelOrder(String orderId)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/orders/" + pathParam(orderId) + "/cancel", null);
    }

    // -- Fulfillments ----------------------------------------------------------

    /**
     * Send tracking number + carrier when the order ships.
     */
    public String createFulfillment(String orderId, String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/orders/" + pathParam(orderId) + "/fulfillments", payloadJson);
    }

    /**
     * Update tracking/delivery status after fulfillment.
     */
    public String updateFulfillment(String orderId, String fulfillmentId, String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST",
                "/ecommerce/orders/" + pathParam(orderId) + "/fulfillments/" + pathParam(fulfillmentId), payloadJson);
    }

    // -- Claims ----------------------------------------------------------------

    /**
     * Register a claim when the shopper files in the platform's returns
     * flow. Delivery-issue claims carry claim_type loss | damage | theft |
     * delay plus claim_details with attachments; return-shipping claims
     * carry claim_type return_shipping plus the RMA number, the return
     * shipment (carrier, tracking, label cost), and the return addresses. Seel opens the claim as pending and fires
     * the claim.created webhook.
     */
    public String createClaim(String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/claims", payloadJson);
    }

    /**
     * Submit the adjudication decision on programs where the platform
     * adjudicates: decision accept | reject, with a reject_reason code and
     * shopper-facing details on rejections. Claim items and amounts cannot
     * be changed after creation. Seel records the outcome and fires
     * claim.accepted or claim.rejected.
     */
    public String updateClaim(String claimId, String payloadJson)
            throws SeelApiException, IOException, InterruptedException {
        return request("POST", "/ecommerce/claims/" + pathParam(claimId), payloadJson);
    }

    public String getClaim(String claimId)
            throws SeelApiException, IOException, InterruptedException {
        return request("GET", "/ecommerce/claims/" + pathParam(claimId), null);
    }

    // -- Lookups (ad hoc; day-to-day state comes via webhooks) -----------------

    public String getOrder(String orderId)
            throws SeelApiException, IOException, InterruptedException {
        return request("GET", "/ecommerce/orders/" + pathParam(orderId), null);
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
     * <p>MessageDigest.isEqual compares in constant time. Pass a non-empty
     * webhookSecret; the JCE rejects empty HMAC keys.
     */
    public static boolean verifyWebhookSignature(byte[] body, String signatureB64,
                                                 String webhookSecret) {
        try {
            Mac mac = Mac.getInstance("HmacSHA256");
            mac.init(new SecretKeySpec(webhookSecret.getBytes(StandardCharsets.UTF_8), "HmacSHA256"));
            byte[] expected = Base64.getEncoder().encode(mac.doFinal(body));
            return MessageDigest.isEqual(expected, signatureB64.getBytes(StandardCharsets.UTF_8));
        } catch (GeneralSecurityException e) {
            // Every conforming JVM ships HmacSHA256, so this is unreachable.
            throw new IllegalStateException("HmacSHA256 unavailable", e);
        }
    }
}

/**
 * Thrown on any non-2xx response. Carries the status and Seel's raw JSON
 * error body: the message names the offending field, and the trace_id is
 * what Seel support will ask for. Parse the body with your JSON library
 * for the individual fields.
 *
 * <p>Package-private top-level class so a plain
 * {@code javac SeelClient.java ExampleServer.java} works with no package
 * declaration; give it its own file if you adopt packages.
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
