/*
 * Example backend for the direct-integration path. JDK 17+ standard library
 * only, no dependencies.
 *
 * Compile and run:
 *   javac SeelClient.java ExampleServer.java
 *   SEEL_API_KEY=... SEEL_WEBHOOK_SECRET=... java ExampleServer
 *
 * Two routes:
 *   POST /api/seel/quote  - browser quote proxy: attaches the server-side API
 *                           key and forwards to Seel's Quote API (the widget
 *                           never sees the key)
 *   POST /webhooks/seel   - single webhook endpoint for contract.* and claim.*
 *                           events: verifies HMAC, ACKs 200 fast, then hands
 *                           off for internal fan-out
 *
 * To drive the widget demo against a live sandbox, both steps are required
 * (this server does not serve the demo page itself):
 *   1. run this server
 *   2. in widget/demo.html, replace the mock quoteFetcher with
 *      configure({ quoteEndpoint: "http://localhost:8787/api/seel/quote" })
 *
 * JSON handling: like SeelClient, this example works in raw JSON strings so
 * it stays dependency-free. In your real backend, use whatever JSON library
 * your stack already has (Jackson, Gson, ...) instead of the string
 * manipulation demoed here.
 */

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.Executors;

public class ExampleServer {

    static final int PORT = Integer.parseInt(env("PORT", "8787"));
    static final String API_KEY = env("SEEL_API_KEY", "");
    static final String WEBHOOK_SECRET = env("SEEL_WEBHOOK_SECRET", "");
    static final String BASE_URL = env("SEEL_BASE_URL", SeelClient.SANDBOX_BASE_URL);
    // Program-specific values, provided by Seel during onboarding. When set,
    // the quote proxy injects them server-side so storefront code carries no
    // program-specific values and stays identical across programs.
    static final String MERCHANT_ID = env("SEEL_MERCHANT_ID", "");
    static final String QUOTE_TYPE = env("SEEL_QUOTE_TYPE", "");

    static final SeelClient client = new SeelClient(API_KEY, BASE_URL);

    private static String env(String name, String fallback) {
        String value = System.getenv(name);
        return (value == null || value.isEmpty()) ? fallback : value;
    }

    /**
     * Internal fan-out. Parse the payload with your JSON library, map
     * merchant_id/order_id to your own retailer code here and route to your
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
        // try-with-resources closes the response body stream, which flushes
        // the full response to the client.
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

        if (method.equalsIgnoreCase("POST") && path.equals("/api/seel/quote")) {
            handleQuote(exchange, body);
            return;
        }

        if (method.equalsIgnoreCase("POST") && path.equals("/webhooks/seel")) {
            handleWebhook(exchange, body);
            return;
        }

        respond(exchange, 404, "{\"error\": \"not found\"}");
    }

    private static void handleQuote(HttpExchange exchange, byte[] body) throws IOException {
        String trimmed = new String(body, StandardCharsets.UTF_8).strip();
        if (trimmed.isEmpty() || !trimmed.startsWith("{")) {
            respond(exchange, 400, "{\"error\": \"request body must be JSON\"}");
            return;
        }

        // Inject the program values right after the opening "{" of the JSON
        // object. Demo-only convenience: without a JSON library we splice
        // strings instead of modifying a parsed object. The values are ALWAYS
        // inserted; if the storefront sent merchant_id or type as well, the
        // object ends up with duplicate keys and which value Seel uses is NOT
        // guaranteed (JSON parsers differ on duplicate-key precedence), so
        // storefront code should omit these keys entirely, as the README
        // describes. In your real backend, parse the body and set the fields
        // with your JSON library instead.
        StringBuilder injected = new StringBuilder();
        if (!MERCHANT_ID.isEmpty()) {
            injected.append("\"merchant_id\":\"").append(jsonEscape(MERCHANT_ID)).append("\"");
        }
        if (!QUOTE_TYPE.isEmpty()) {
            if (injected.length() > 0) {
                injected.append(",");
            }
            injected.append("\"type\":\"").append(jsonEscape(QUOTE_TYPE)).append("\"");
        }
        String params;
        if (injected.length() == 0) {
            params = trimmed;
        } else {
            String rest = trimmed.substring(1).strip();
            // "{}" body: no trailing comma after the injected pairs.
            params = rest.equals("}") ? "{" + injected + "}" : "{" + injected + "," + rest;
        }

        try {
            respond(exchange, 200, client.createQuote(params));
        } catch (SeelApiException e) {
            // Forward Seel's status + error body: it names the missing or
            // inconsistent field, which is what the integrator needs.
            String errorBody = e.getBody() == null ? "" : e.getBody().strip();
            if (errorBody.startsWith("{")) {
                respond(exchange, e.getStatus(), errorBody);
            } else {
                respond(exchange, e.getStatus(),
                        "{\"error\": \"" + jsonEscape(e.getMessage()) + "\"}");
            }
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            respond(exchange, 502, "{\"error\": \"upstream quote request failed\"}");
        } catch (Exception e) {
            respond(exchange, 502, "{\"error\": \"upstream quote request failed\"}");
        }
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
        // ACK and flush before doing any work: Seel retries anything not
        // answered with a 200 within 10 seconds. respond() closes the
        // response body stream, which flushes the response to the client.
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
        // Thread pool so webhook deliveries do not queue behind each other.
        server.setExecutor(Executors.newFixedThreadPool(8));
        System.out.println("listening on http://localhost:" + PORT);
        server.start();
    }
}
