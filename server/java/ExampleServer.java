/*
 * Example backend for the direct-integration path. JDK 17+ standard library
 * only, no dependencies.
 *
 * Routes:
 *   POST /api/seel/quote  - browser quote proxy: attaches the server-side API
 *                           key and forwards to Seel's Quote API (the widget
 *                           never sees the key)
 *
 *   POST /api/seel/orders                                - create order
 *   POST /api/seel/orders/{orderId}                      - update order
 *   POST /api/seel/orders/{orderId}/cancel               - cancel order
 *   POST /api/seel/orders/{orderId}/fulfillments         - create fulfillment
 *   POST /api/seel/orders/{orderId}/fulfillments/{fid}   - update fulfillment
 *
 *   POST /webhooks/seel   - single webhook endpoint for contract.* and claim.*
 *                           events: verifies HMAC, ACKs 200 fast, then hands
 *                           off for internal fan-out
 *
 * The order and fulfillment routes mirror Seel's own path shape, so a caller
 * already written against Seel's API moves over by changing the base URL and
 * nothing else.
 *
 * Two deployments use these routes differently:
 *
 *   Single retailer - the retailer's own backend holds the API key and calls
 *   Seel directly. The order and fulfillment routes are optional here; call
 *   SeelClient from your order pipeline instead if that fits better.
 *
 *   Platform proxy - the platform holds one API key for every retailer on it,
 *   retailers point at the platform instead of at Seel, and the platform
 *   resolves which merchant each request belongs to. Retailers hold no Seel
 *   credentials at all. See authenticateCaller() and resolveMerchantId()
 *   below - those two methods are the whole of what a platform must replace.
 *
 * Compile and run:
 *   javac SeelClient.java ExampleServer.java
 *   SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... java ExampleServer
 *
 * To drive the widget demo against a live sandbox, two steps - this server
 * doesn't serve the demo page:
 *   1. run this server
 *   2. in widget/demo.html, replace the mock quoteFetcher with
 *      configure({ quoteEndpoint: "http://localhost:8787/api/seel/quote" })
 *
 * Like SeelClient, this example works in raw JSON strings to stay
 * dependency-free. In a real backend, use your own JSON library (Jackson,
 * Gson, ...) instead of the string splicing demoed here.
 */

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.concurrent.Executors;

public class ExampleServer {

    static final int PORT = Integer.parseInt(env("PORT", "8787"));
    static final String API_KEY = env("SEEL_API_KEY", "");
    static final String WEBHOOK_SECRET = env("SEEL_WEBHOOK_SECRET", "");
    static final String BASE_URL = env("SEEL_BASE_URL", SeelClient.SANDBOX_BASE_URL);
    // Program values from Seel onboarding. When set, the proxy stamps them
    // into every quote request, so storefront code stays identical across
    // programs.
    static final String MERCHANT_ID = env("SEEL_MERCHANT_ID", "");
    static final String QUOTE_TYPE = env("SEEL_QUOTE_TYPE", "");

    static final SeelClient client = new SeelClient(API_KEY, BASE_URL);

    /** A call out to Seel, for {@link #forward}. */
    @FunctionalInterface
    interface SeelCall {
        String call() throws Exception;
    }

    /**
     * Decide whether the caller may use this proxy.
     *
     * <p>This demo accepts everyone, which is only safe because it holds a
     * sandbox key and listens on localhost.
     *
     * <p>A platform MUST replace this. Retailers authenticate to the platform
     * with platform credentials - they never receive a Seel API key, because
     * one key covers every retailer on the platform and would let any holder
     * act as any other. Return the caller's identity from here and pass it to
     * {@link #resolveMerchantId} so a retailer can only ever touch its own
     * orders.
     */
    static boolean authenticateCaller(HttpExchange exchange) {
        return true;
    }

    /**
     * Return the merchant ID this request belongs to.
     *
     * <p>Single retailer: SEEL_MERCHANT_ID is set once in the environment and
     * stamped onto everything, so storefront and pipeline code carry no
     * program-specific values.
     *
     * <p>Platform proxy: leave SEEL_MERCHANT_ID unset and look the merchant up
     * from the authenticated caller instead. Deriving it from the caller
     * rather than trusting the request body is what stops one retailer
     * quoting or ordering against another's merchant ID.
     */
    static String resolveMerchantId(HttpExchange exchange) {
        return MERCHANT_ID;
    }

    private static String env(String name, String fallback) {
        String value = System.getenv(name);
        return (value == null || value.isEmpty()) ? fallback : value;
    }

    /**
     * Before any of this fires, the endpoint has to be registered with Seel.
     * Seel has no self-serve way to register this URL. There is no webhook field on
     * Create or Update Merchant and no registration endpoint - ask your Seel contact
     * to configure it, and tell them which events you want. Do it once per
     * environment: a sandbox registration does not carry over to production.
     *
     * <p>Internal fan-out. Parse the payload with your JSON library, map
     * merchant_id/order_id to your own retailer code and route to your
     * systems. Dedupe on id + type first, since delivery is at-least-once.
     * In production, queue this work off the request thread instead of
     * processing inline.
     */
    static void handleWebhookEvent(String rawPayload) {
        System.out.println("[webhook] received: " + rawPayload);
    }

    private static void respond(HttpExchange exchange, int status, String jsonBody)
            throws IOException {
        byte[] data = jsonBody.getBytes(StandardCharsets.UTF_8);
        exchange.getResponseHeaders().set("Content-Type", "application/json");
        // demo only; lock down in prod
        exchange.getResponseHeaders().set("Access-Control-Allow-Origin", "*");
        exchange.sendResponseHeaders(status, data.length);
        // Closing the response body stream (try-with-resources) flushes the
        // full response to the client.
        try (OutputStream out = exchange.getResponseBody()) {
            out.write(data);
        }
    }

    /** Minimal JSON string escaping for the few strings this demo emits. */
    private static String jsonEscape(String s) {
        StringBuilder sb = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"' -> sb.append("\\\"");
                case '\\' -> sb.append("\\\\");
                case '\n' -> sb.append("\\n");
                case '\r' -> sb.append("\\r");
                case '\t' -> sb.append("\\t");
                default -> {
                    if (c < 0x20) {
                        sb.append(String.format("\\u%04x", (int) c));
                    } else {
                        sb.append(c);
                    }
                }
            }
        }
        return sb.toString();
    }

    private static void handle(HttpExchange exchange) throws IOException {
        String method = exchange.getRequestMethod();
        String path = exchange.getRequestURI().getPath();

        if (method.equalsIgnoreCase("OPTIONS")) { // CORS preflight for the demo page
            // demo only; lock down in prod
            exchange.getResponseHeaders().set("Access-Control-Allow-Origin", "*");
            exchange.getResponseHeaders().set("Access-Control-Allow-Headers", "Content-Type");
            exchange.getResponseHeaders().set("Access-Control-Allow-Methods", "POST, OPTIONS");
            exchange.sendResponseHeaders(204, -1); // -1 = no response body
            exchange.close();
            return;
        }

        byte[] body = exchange.getRequestBody().readAllBytes();

        if (!method.equalsIgnoreCase("POST")) {
            respond(exchange, 404, "{\"error\": \"not found\"}");
            return;
        }

        if (path.equals("/webhooks/seel")) {
            handleWebhook(exchange, body);
            return;
        }

        if (!authenticateCaller(exchange)) {
            respond(exchange, 401, "{\"error\": \"unauthorized\"}");
            return;
        }

        if (path.equals("/api/seel/quote")) {
            handleQuote(exchange, body);
            return;
        }

        // Routes mirroring Seel's own paths under /api/seel.
        String[] seg = segments(path);
        if (seg.length >= 3 && seg[0].equals("api") && seg[1].equals("seel")
                && seg[2].equals("orders")) {
            // Sync every order, opted in or not. On opt-in the body carries
            // seel_services with the quote_id and price, which mints the
            // contract and fires contract.created.
            if (seg.length == 3) {
                String params = spliceFields(exchange, body, merchantIdField(exchange));
                if (params == null) {
                    return;
                }
                forward(exchange, "order", () -> client.createOrder(params));
                return;
            }
            String orderId = decode(seg[3]);
            if (seg.length == 4) {
                String params = spliceFields(exchange, body, "");
                if (params == null) {
                    return;
                }
                forward(exchange, "order update", () -> client.updateOrder(orderId, params));
                return;
            }
            // Cancel carries no body.
            if (seg.length == 5 && seg[4].equals("cancel")) {
                forward(exchange, "order cancel", () -> client.cancelOrder(orderId));
                return;
            }
            if (seg.length == 5 && seg[4].equals("fulfillments")) {
                String params = spliceFields(exchange, body, "");
                if (params == null) {
                    return;
                }
                forward(exchange, "fulfillment",
                        () -> client.createFulfillment(orderId, params));
                return;
            }
            if (seg.length == 6 && seg[4].equals("fulfillments")) {
                String fulfillmentId = decode(seg[5]);
                String params = spliceFields(exchange, body, "");
                if (params == null) {
                    return;
                }
                forward(exchange, "fulfillment update",
                        () -> client.updateFulfillment(orderId, fulfillmentId, params));
                return;
            }
        }

        respond(exchange, 404, "{\"error\": \"not found\"}");
    }

    /** Split a path into its non-empty segments. */
    private static String[] segments(String path) {
        return Arrays.stream(path.split("/")).filter(s -> !s.isEmpty()).toArray(String[]::new);
    }

    /**
     * Percent-decode one path segment. "+" is escaped first because
     * URLDecoder treats it as a space, which is a query-string rule and
     * wrong for a path.
     */
    private static String decode(String segment) {
        return URLDecoder.decode(segment.replace("+", "%2B"), StandardCharsets.UTF_8);
    }

    /** The merchant_id JSON pair to splice in, or "" when none is set. */
    private static String merchantIdField(HttpExchange exchange) {
        String merchantId = resolveMerchantId(exchange);
        return merchantId.isEmpty()
                ? ""
                : "\"merchant_id\":\"" + jsonEscape(merchantId) + "\"";
    }

    /**
     * Validate a JSON object body and splice extra pairs in after the
     * opening "{" - a demo-only shortcut for having no JSON library. The
     * pairs are always inserted, so if the caller also sent those keys the
     * object gets duplicate keys, and which value Seel uses isn't guaranteed
     * (parsers differ on duplicate-key precedence). In a real backend, parse
     * the body and set the fields with your JSON library.
     *
     * <p>Answers 400 and returns null when the body isn't a JSON object.
     */
    private static String spliceFields(HttpExchange exchange, byte[] body, String injected)
            throws IOException {
        String trimmed = new String(body, StandardCharsets.UTF_8).strip();
        if (trimmed.isEmpty() || !trimmed.startsWith("{")) {
            respond(exchange, 400, "{\"error\": \"request body must be JSON\"}");
            return null;
        }
        if (injected.isEmpty()) {
            return trimmed;
        }
        String rest = trimmed.substring(1).strip();
        // "{}" body: no trailing comma after the injected pairs.
        return rest.equals("}") ? "{" + injected + "}" : "{" + injected + "," + rest;
    }

    /** Call Seel and mirror the result back to the caller. */
    private static void forward(HttpExchange exchange, String label, SeelCall call)
            throws IOException {
        try {
            respond(exchange, 200, call.call());
        } catch (SeelApiException e) {
            // Forward Seel's status and error body - it names the
            // offending field.
            String errorBody = e.getBody() == null ? "" : e.getBody().strip();
            if (errorBody.startsWith("{")) {
                respond(exchange, e.getStatus(), errorBody);
            } else {
                respond(exchange, e.getStatus(),
                        "{\"error\": \"" + jsonEscape(e.getMessage()) + "\"}");
            }
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            respond(exchange, 502,
                    "{\"error\": \"upstream " + jsonEscape(label) + " request failed\"}");
        } catch (Exception e) {
            respond(exchange, 502,
                    "{\"error\": \"upstream " + jsonEscape(label) + " request failed\"}");
        }
    }

    private static void handleQuote(HttpExchange exchange, byte[] body) throws IOException {
        StringBuilder injected = new StringBuilder(merchantIdField(exchange));
        if (!QUOTE_TYPE.isEmpty()) {
            if (injected.length() > 0) {
                injected.append(",");
            }
            injected.append("\"type\":\"").append(jsonEscape(QUOTE_TYPE)).append("\"");
        }
        // Storefront code should omit merchant_id and type, as the README
        // describes - spliceFields always inserts them.
        String params = spliceFields(exchange, body, injected.toString());
        if (params == null) {
            return;
        }
        forward(exchange, "quote", () -> client.createQuote(params));
    }

    private static void handleWebhook(HttpExchange exchange, byte[] body) throws IOException {
        String signature = exchange.getRequestHeaders().getFirst("X-Seel-Hmac-SHA256");
        if (signature == null) {
            signature = "";
        }
        // An unset secret can never verify (and the JCE rejects empty HMAC
        // keys), so reject up front.
        if (WEBHOOK_SECRET.isEmpty()
                || !SeelClient.verifyWebhookSignature(body, signature, WEBHOOK_SECRET)) {
            respond(exchange, 401, "{\"error\": \"invalid signature\"}");
            return;
        }
        // ACK before doing any work: Seel retries anything not answered
        // with a 200 within 10 seconds. respond() closes the body stream,
        // which flushes the response to the client.
        respond(exchange, 200, "{\"ok\": true}");
        try {
            handleWebhookEvent(new String(body, StandardCharsets.UTF_8));
        } catch (Exception e) { // already ACKed; never let this escape
            System.out.println("[webhook] processing error: " + e);
        }
    }

    public static void main(String[] args) throws IOException {
        if (API_KEY.isEmpty()) {
            System.out.println("warning: SEEL_API_KEY not set - quote proxy will fail");
        }
        HttpServer server = HttpServer.create(new InetSocketAddress(PORT), 0);
        server.createContext("/", ExampleServer::handle);
        // Thread pool so webhook deliveries don't queue behind each other.
        server.setExecutor(Executors.newFixedThreadPool(8));
        System.out.println("listening on http://localhost:" + PORT);
        server.start();
    }
}
