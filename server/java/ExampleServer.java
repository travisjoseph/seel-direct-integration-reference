/*
 * Example backend for the direct-integration path. JDK 17+ standard library
 * only, no dependencies.
 *
 * Routes:
 *   POST /v1/ecommerce/quotes  - browser quote proxy: attaches the server-side API
 *                           key and forwards to Seel's Quote API (the widget
 *                           never sees the key)
 *
 *   POST /v1/ecommerce/orders                                - create order
 *   POST /v1/ecommerce/orders/{orderId}                      - update order
 *   POST /v1/ecommerce/orders/{orderId}/cancel               - cancel order
 *   POST /v1/ecommerce/orders/{orderId}/fulfillments         - create fulfillment
 *   POST /v1/ecommerce/orders/{orderId}/fulfillments/{fid}   - update fulfillment
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
 *   below, which are where a platform starts. They are not the whole job:
 *   order_id comes off the URL and is never checked against the caller, so
 *   nothing here stops one retailer touching another's order. That mapping
 *   belongs to the platform - see the README.
 *
 * Compile and run:
 *   javac *.java
 *   SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... java ExampleServer
 *
 * To drive the widget demo against a live sandbox, two steps - this server
 * doesn't serve the demo page:
 *   1. run this server
 *   2. in widget/demo.html, replace the mock quoteFetcher with
 *      configure({ quoteEndpoint: "http://localhost:8787/v1/ecommerce/quotes" })
 *
 * SeelClient works in raw JSON strings to stay dependency-free. This
 * example parses each request body with SeelValidation's minimal reader so
 * it can stamp fields and validate, then serializes it back. In a real
 * backend, use your own JSON library (Jackson, Gson, ...) for both.
 */

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.http.HttpConnectTimeoutException;
import java.net.http.HttpTimeoutException;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Executors;

public class ExampleServer {

    // Loopback by default: this demo holds a real API key and accepts every
    // caller, so it must not be reachable from other hosts unless asked.
    static final String HOST = env("HOST", "127.0.0.1");
    static final int PORT = Integer.parseInt(env("PORT", "8787"));
    static final int MAX_BODY_BYTES = 1048576;
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
     * act as any other. A real implementation returns the caller's identity
     * rather than a boolean, and {@link #resolveMerchantId} takes it -
     * changing both signatures is part of the work.
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
        // Unlike the other ports there is no request-body fallback here.
        // When SEEL_MERCHANT_ID is unset nothing is stamped and the
        // caller's own merchant_id passes through untouched, which is the
        // same end result and carries the same warning: on a real
        // platform, derive the merchant from the authenticated caller
        // instead of trusting whatever the body says.
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
     * <p>Internal fan-out. Map merchant_id/order_id to your own retailer
     * code and route to your systems. Dedupe on id + type first, since
     * delivery is at-least-once. In production, queue this work off the
     * request thread instead of processing inline.
     */
    static void handleWebhookEvent(Map<?, ?> event) {
        System.out.println("[webhook] " + event.get("type") + " id=" + event.get("id"));
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

    private static String error(String message) {
        return SeelValidation.toJson(Map.of("error", message));
    }

    /**
     * Every request gets an answer. A Throwable escaping the handler would
     * leave the client waiting on an open connection with nothing sent,
     * which is how a deeply nested body used to hang its caller.
     */
    private static void handle(HttpExchange exchange) throws IOException {
        try {
            route(exchange);
        } catch (Throwable t) {
            System.out.println("[proxy] handler failed: " + t);
            try {
                respond(exchange, 500, error("internal error"));
            } catch (IOException unanswerable) {
                // headers already on the wire, or the client is gone
            }
        } finally {
            exchange.close();
        }
    }

    private static void route(HttpExchange exchange) throws IOException {
        String method = exchange.getRequestMethod();
        // getRawPath, not getPath: getPath is already percent-decoded, and
        // decoding a second time in decode() turns a literal "%" in an id
        // into a malformed escape and throws out of the handler.
        String rawPath = exchange.getRequestURI().getRawPath();
        String path = rawPath == null ? "" : rawPath;
        // getRawPath already excludes the query string; this keeps the
        // intent explicit and matches the other ports.
        int q = path.indexOf('?');
        if (q >= 0) {
            path = path.substring(0, q);
        }

        if (method.equalsIgnoreCase("OPTIONS")) { // CORS preflight for the demo page
            // demo only; lock down in prod
            exchange.getResponseHeaders().set("Access-Control-Allow-Origin", "*");
            exchange.getResponseHeaders().set("Access-Control-Allow-Headers", "Content-Type");
            exchange.getResponseHeaders().set("Access-Control-Allow-Methods", "POST, OPTIONS");
            exchange.sendResponseHeaders(204, -1); // -1 = no response body
            exchange.close();
            return;
        }

        byte[] body = readBody(exchange);
        if (body == null) {
            return;
        }

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

        if (path.equals("/v1/ecommerce/quotes")) {
            handleQuote(exchange, body);
            return;
        }

        // Routes mirror Seel's real paths, prefix included, so a caller
        // already written against Seel moves onto a platform by changing
        // the base URL and nothing else.
        String[] seg = segments(path);
        if (seg.length >= 3 && seg[0].equals("v1") && seg[1].equals("ecommerce")
                && seg[2].equals("orders")) {
            // Sync every order, opted in or not. On opt-in the body carries
            // seel_services with the quote_id and price, which mints the
            // contract and fires contract.created.
            if (seg.length == 3) {
                Map<String, Object> params = jsonBody(exchange, body);
                if (params == null) {
                    return;
                }
                String merchantId = resolveMerchantId(exchange);
                if (!merchantId.isEmpty()) {
                    params.put("merchant_id", merchantId);
                }
                if (!validate(exchange, "create_order", SeelValidation.validateOrder(params))) {
                    return;
                }
                String json = SeelValidation.toJson(params);
                forward(exchange, "order", () -> client.createOrder(json),
                        SeelValidation.carriesCoverage(json));
                return;
            }
            if (seg[3].isEmpty()) {
                respond(exchange, 404, "{\"error\": \"not found\"}");
                return;
            }
            String orderId = decode(seg[3]);
            if (orderId == null) {
                respond(exchange, 400, "{\"error\": \"invalid order id in path\"}");
                return;
            }
            if (seg.length == 4) {
                String params = passthroughBody(exchange, body);
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
                String params = passthroughBody(exchange, body);
                if (params == null) {
                    return;
                }
                forward(exchange, "fulfillment",
                        () -> client.createFulfillment(orderId, params));
                return;
            }
            if (seg.length == 6 && seg[4].equals("fulfillments") && !seg[5].isEmpty()) {
                String fulfillmentId = decode(seg[5]);
                if (fulfillmentId == null) {
                    respond(exchange, 400, "{\"error\": \"invalid id in path\"}");
                    return;
                }
                String params = passthroughBody(exchange, body);
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

    /**
     * Split a path into segments after the leading slash.
     *
     * <p>Empty segments are kept, so "/orders//cancel" does not collapse
     * into a cancel of an order called "cancel". The Python and Node ports
     * use `([^/]+)` regexes, which reject an empty id, and this has to
     * match them.
     */
    private static String[] segments(String path) {
        String trimmed = path.startsWith("/") ? path.substring(1) : path;
        return trimmed.split("/", -1);
    }

    /**
     * Segments that are Seel endpoints in their own right and so can never
     * be an order id. Seel's own collection endpoints live alongside order
     * ids, so an id that equals one of them would reach the collection
     * instead. "batch" is POST /v1/ecommerce/orders/batch, the
     * order-history backfill: routed as an order id it would proxy an
     * unstamped, unvalidated batch write.
     */
    private static final java.util.Set<String> RESERVED_PATH_SEGMENTS = java.util.Set.of("batch");

    /**
     * Percent-decode one path segment, or return null if it escapes.
     *
     * <p>A decoded "/" would reach a different endpoint than the route
     * implies: "orders/ORD1%2Fcancel" matches the update-order route and
     * would perform a cancel. Control characters are refused for the same
     * reason, and a malformed escape is rejected rather than thrown, which
     * would otherwise escape the handler and close the connection with no
     * response. With one API key shared across retailers this is a
     * privilege boundary, not a cosmetic check.
     *
     * <p>"+" is left alone: treating it as a space is a query-string rule
     * and wrong for a path.
     *
     * <p>One case never reaches here: com.sun.net.httpserver rejects a
     * malformed escape such as "%zz" while parsing the request URI and
     * answers its own HTML 400. Same status as the other ports, different
     * body, and not something this handler can intercept.
     */

    private static String decode(String segment) {
        // Decode the escapes to bytes by hand, then insist the result is
        // valid UTF-8. URLDecoder substitutes U+FFFD for invalid sequences
        // rather than failing, which would forward a mangled id upstream
        // where the other ports return a 400.
        byte[] raw = new byte[segment.length()];
        int len = 0;
        for (int i = 0; i < segment.length(); ) {
            char c = segment.charAt(i);
            if (c == '%') {
                if (i + 2 >= segment.length()) {
                    return null; // truncated escape
                }
                int hi = Character.digit(segment.charAt(i + 1), 16);
                int lo = Character.digit(segment.charAt(i + 2), 16);
                if (hi < 0 || lo < 0) {
                    return null; // malformed escape
                }
                raw[len++] = (byte) ((hi << 4) + lo);
                i += 3;
            } else if (c < 0x80) {
                raw[len++] = (byte) c;
                i++;
            } else {
                // Already-decoded non-ASCII: re-encode so the UTF-8 check
                // below sees the same bytes either way.
                byte[] utf8 = String.valueOf(c).getBytes(StandardCharsets.UTF_8);
                if (len + utf8.length > raw.length) {
                    return null;
                }
                System.arraycopy(utf8, 0, raw, len, utf8.length);
                len += utf8.length;
                i++;
            }
        }
        String decoded;
        try {
            decoded = StandardCharsets.UTF_8
                    .newDecoder()
                    .onMalformedInput(java.nio.charset.CodingErrorAction.REPORT)
                    .onUnmappableCharacter(java.nio.charset.CodingErrorAction.REPORT)
                    .decode(java.nio.ByteBuffer.wrap(raw, 0, len))
                    .toString();
        } catch (java.nio.charset.CharacterCodingException e) {
            return null; // not valid UTF-8
        }
        if (decoded.indexOf('/') >= 0 || RESERVED_PATH_SEGMENTS.contains(decoded)) {
            return null;
        }
        // A segment that is only dots is refused too: a decoded ".." is not
        // a slash, but HTTP clients normalize it away, so
        // "orders/%2E%2E/cancel" leaves this proxy as a request to
        // /v1/ecommerce/cancel - a different endpoint than the route names.
        if (!decoded.isEmpty() && decoded.chars().allMatch(c -> c == '.')) {
            return null;
        }
        for (int i = 0; i < decoded.length(); i++) {
            char c = decoded.charAt(i);
            if (c < 0x20 || c == 0x7F) {
                return null;
            }
        }
        return decoded;
    }

    /**
     * Read the request body, or answer 413 and return null when it is over
     * {@link #MAX_BODY_BYTES}, or 400 when its framing cannot be read.
     * Content-Length is checked first so an oversized declared body is
     * refused without reading it, and the read is capped as well because a
     * chunked body declares no length.
     */
    private static byte[] readBody(HttpExchange exchange) throws IOException {
        String declared = exchange.getRequestHeaders().getFirst("Content-Length");
        if (declared != null) {
            try {
                if (Long.parseLong(declared.strip()) > MAX_BODY_BYTES) {
                    respond(exchange, 413, error("request body exceeds " + MAX_BODY_BYTES + " bytes"));
                    return null;
                }
            } catch (NumberFormatException e) {
                // let the server's own framing deal with a malformed header
            }
        }
        byte[] body;
        try {
            body = exchange.getRequestBody().readNBytes(MAX_BODY_BYTES + 1);
        } catch (IOException e) {
            // The JDK server throws here on a bad chunk size and on
            // chunked trailers it does not support, both the caller's to fix.
            respond(exchange, 400, error("malformed request body"));
            return null;
        }
        if (body.length > MAX_BODY_BYTES) {
            respond(exchange, 413, error("request body exceeds " + MAX_BODY_BYTES + " bytes"));
            return null;
        }
        return body;
    }

    /**
     * Parse a JSON object body, or answer 400 and return null. Nesting and
     * number literals past the parser's limits are refused here too, so
     * the body the proxy forwards means the same thing to every reader.
     */
    @SuppressWarnings("unchecked")
    private static Map<String, Object> jsonBody(HttpExchange exchange, byte[] body)
            throws IOException {
        Object parsed = SeelValidation.parseJson(new String(body, StandardCharsets.UTF_8));
        if (parsed == null) {
            respond(exchange, 400, error("request body must be JSON"));
            return null;
        }
        if (!(parsed instanceof Map)) {
            respond(exchange, 400, error("request body must be a JSON object"));
            return null;
        }
        return (Map<String, Object>) parsed;
    }

    /** A JSON object body re-serialized unchanged, or null after a 400. */
    private static String passthroughBody(HttpExchange exchange, byte[] body) throws IOException {
        Map<String, Object> params = jsonBody(exchange, body);
        return params == null ? null : SeelValidation.toJson(params);
    }

    /**
     * Answer 400 with every problem at once when the validators object,
     * matching the other ports. The request never left this process, so
     * this is the caller's bug to fix. Returns false after answering.
     */
    private static boolean validate(HttpExchange exchange, String operation, List<String> problems)
            throws IOException {
        if (problems.isEmpty()) {
            return true;
        }
        Map<String, Object> out = new LinkedHashMap<>();
        out.put("error", operation + ": " + String.join("; ", problems));
        out.put("problems", problems);
        respond(exchange, 400, SeelValidation.toJson(out));
        return false;
    }

    /**
     * Call Seel and mirror the result back to the caller.
     *
     */
    private static void forward(HttpExchange exchange, String label, SeelCall call)
            throws IOException {
        forward(exchange, label, call, false);
    }

    /**
     * As above, but pass checkContract when the request carried a non-empty
     * seel_services array, so a 200 that minted no contract is reported
     * rather than echoed as success. Use
     * {@link SeelValidation#carriesCoverage} to decide - an order with no
     * coverage is a normal sync and must not be flagged.
     *
     * <p>Each failure has to stay distinguishable. Once Seel has answered
     * 2xx the order exists, so nothing past that point may become a 502,
     * which would invite a retry and a duplicate order.
     */
    private static void forward(
            HttpExchange exchange, String label, SeelCall call, boolean checkContract)
            throws IOException {
        try {
            String body = call.call();
            // An empty 2xx body is an empty object; a non-JSON one (a CDN
            // or gateway page in front of Seel) is relayed verbatim under
            // seel_raw_body rather than echoed as if it were Seel's answer.
            Object seelResponse = body.isBlank() ? Map.of() : SeelValidation.parseJson(body);
            boolean verbatim = seelResponse != null && !body.isBlank();
            if (seelResponse == null) {
                System.out.println("[proxy] " + label + ": upstream returned a non-JSON body");
                seelResponse = Map.of("seel_raw_body", body);
            }
            if (checkContract) {
                String reason = SeelValidation.contractNotMintedReason(seelResponse);
                if (reason != null) {
                    // Seel accepted the order and minted no contract. 502
                    // would be wrong twice over: the upstream call
                    // succeeded, and a retry would duplicate the order.
                    System.out.println("[proxy] " + label + ": " + reason);
                    Map<String, Object> out = new LinkedHashMap<>();
                    out.put("error", "order created but no contract was minted: " + reason);
                    out.put("seel_response", seelResponse);
                    respond(exchange, 409, SeelValidation.toJson(out));
                    return;
                }
            }
            respond(exchange, 200, verbatim ? body : SeelValidation.toJson(seelResponse));
        } catch (SeelApiException e) {
            // Forward Seel's status and error body - it names the
            // offending field.
            String errorBody = e.getBody() == null ? "" : e.getBody().strip();
            if (errorBody.startsWith("{")) {
                respond(exchange, e.getStatus(), errorBody);
            } else {
                respond(exchange, e.getStatus(), error(e.getMessage()));
            }
        } catch (HttpConnectTimeoutException e) {
            // Nothing reached Seel, so a retry is safe.
            System.out.println("[proxy] " + label + " failed: " + e);
            respond(exchange, 502, error("upstream " + label + " request failed"));
        } catch (HttpTimeoutException e) {
            // The request went out and no answer came back in time. Seel
            // may well have processed it, so 504 rather than 502: the
            // caller must check before retrying an order.
            System.out.println("[proxy] " + label + " timed out: " + e);
            respond(exchange, 504, error("timed out waiting for Seel; the request may have "
                    + "been processed, check before retrying"));
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            System.out.println("[proxy] " + label + " interrupted");
            respond(exchange, 502, error("upstream " + label + " request failed"));
        } catch (Exception e) {
            // Log before answering: a bare 502 leaves the operator unable
            // to tell a timeout from a bug in this handler.
            System.out.println("[proxy] " + label + " failed: " + e);
            respond(exchange, 502, error("upstream " + label + " request failed"));
        }
    }

    private static void handleQuote(HttpExchange exchange, byte[] body) throws IOException {
        Map<String, Object> params = jsonBody(exchange, body);
        if (params == null) {
            return;
        }
        // Set on the parsed map, so a caller-supplied merchant_id or type
        // is overwritten rather than left to win as a duplicate key.
        String merchantId = resolveMerchantId(exchange);
        if (!merchantId.isEmpty()) {
            params.put("merchant_id", merchantId);
        }
        if (!QUOTE_TYPE.isEmpty()) {
            params.put("type", QUOTE_TYPE);
        }
        if (!validate(exchange, "create_quote", SeelValidation.validateQuote(params))) {
            return;
        }
        String json = SeelValidation.toJson(params);
        forward(exchange, "quote", () -> client.createQuote(json));
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
            Object event = SeelValidation.parseJson(new String(body, StandardCharsets.UTF_8));
            if (!(event instanceof Map)) {
                throw new IllegalArgumentException("payload is not a JSON object");
            }
            handleWebhookEvent((Map<?, ?>) event);
        } catch (Exception e) { // already ACKed; never let this escape
            System.out.println("[webhook] processing error: " + e);
        }
    }

    public static void main(String[] args) throws IOException {
        if (API_KEY.isEmpty()) {
            System.out.println("warning: SEEL_API_KEY not set - quote proxy will fail");
        }
        // A connection that never finishes sending its request is dropped
        // after this many seconds. Read by the JDK server once, at startup,
        // so it has to be set before create().
        System.getProperties().putIfAbsent("sun.net.httpserver.maxReqTime", "30");
        HttpServer server = HttpServer.create(new InetSocketAddress(HOST, PORT), 0);
        server.createContext("/", ExampleServer::handle);
        // One thread per in-flight request. With a fixed pool, a handful
        // of clients holding their request bodies open would occupy every
        // thread and webhooks would miss Seel's 10 second ACK window.
        server.setExecutor(Executors.newCachedThreadPool());
        InetSocketAddress bound = server.getAddress();
        System.out.println("listening on http://" + bound.getHostString() + ":" + bound.getPort());
        server.start();
    }
}
