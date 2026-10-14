/*
 * Runs the shared merchant-binding cases against this port.
 *
 *   javac *.java && java SeelBindingTest
 *
 * Exits non-zero on failure, so it works as a CI step.
 *
 * The cases live in ../binding-cases.json and every port runs them. Each
 * binding case goes through the real request handler to a stub upstream
 * that records what it received, so a port that forwards the body
 * merchant_id fails on the merchant the stub saw, not on a unit-level
 * return value.
 */

import com.sun.net.httpserver.HttpExchange;
import com.sun.net.httpserver.HttpServer;
import java.io.IOException;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Objects;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.Executors;

public class SeelBindingTest {

    private static final Map<String, String> PATHS =
            Map.of("quote", "/v1/ecommerce/quotes", "order", "/v1/ecommerce/orders");
    private static final List<Map<String, Object>> received = new CopyOnWriteArrayList<>();
    private static final HttpClient http = HttpClient.newHttpClient();
    private static final List<String> failures = new ArrayList<>();
    private static int passed = 0;
    private static int proxyPort;

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        Map<String, Object> fixture = (Map<String, Object>) SeelValidation.parseJson(
                Files.readString(Path.of("..", "binding-cases.json"), StandardCharsets.UTF_8));
        List<Object> parseCases = (List<Object>) fixture.get("parse_cases");
        List<Object> bindingCases = (List<Object>) fixture.get("binding_cases");

        // The fixture is the single point of failure for cross-port drift,
        // so an emptied or defanged one has to fail rather than pass green.
        if (parseCases.size() < 5 || bindingCases.size() < 5) {
            System.out.println("FAIL  fixture has fewer than 5 parse or binding cases");
            System.exit(1);
        }

        Map<String, Map<String, Object>> payloads = completePayloads();

        HttpServer upstream = serve(SeelBindingTest::record);
        HttpServer proxy = serve(ExampleServer::handle);
        proxyPort = proxy.getAddress().getPort();
        SeelClient client =
                new SeelClient("dummy", "http://127.0.0.1:" + upstream.getAddress().getPort());

        try {
            for (Object entry : parseCases) {
                runParseCase((Map<String, Object>) entry);
            }
            for (Object entry : bindingCases) {
                runBindingCase((Map<String, Object>) entry, payloads, client);
            }
            preflight(client);
            bothConfigured(client);
        } finally {
            proxy.stop(0);
            upstream.stop(0);
        }

        System.out.println(passed + " passed, " + failures.size() + " failed");
        for (String failure : failures) {
            System.out.println("FAIL  " + failure);
        }
        System.exit(failures.isEmpty() ? 0 : 1);
    }

    @SuppressWarnings("unchecked")
    private static Map<String, Map<String, Object>> completePayloads() throws IOException {
        Map<String, Object> doc = (Map<String, Object>) SeelValidation.parseJson(
                Files.readString(Path.of("..", "validation-cases.json"), StandardCharsets.UTF_8));
        Map<String, Map<String, Object>> payloads = new LinkedHashMap<>();
        for (Object entry : (List<Object>) doc.get("cases")) {
            Map<String, Object> c = (Map<String, Object>) entry;
            if (c.get("name").equals("quote: complete payload")
                    || c.get("name").equals("order: complete payload")) {
                payloads.put((String) c.get("operation"), (Map<String, Object>) c.get("payload"));
            }
        }
        return payloads;
    }

    private static HttpServer serve(com.sun.net.httpserver.HttpHandler handler) throws IOException {
        HttpServer server = HttpServer.create(new InetSocketAddress("127.0.0.1", 0), 0);
        server.createContext("/", handler);
        server.setExecutor(Executors.newCachedThreadPool());
        server.start();
        return server;
    }

    @SuppressWarnings("unchecked")
    private static void record(HttpExchange exchange) throws IOException {
        byte[] body = exchange.getRequestBody().readAllBytes();
        received.add((Map<String, Object>) SeelValidation.parseJson(
                new String(body, StandardCharsets.UTF_8)));
        exchange.getResponseHeaders().set("Content-Type", "application/json");
        exchange.sendResponseHeaders(200, body.length);
        try (OutputStream out = exchange.getResponseBody()) {
            out.write(body);
        }
    }

    private static void check(String name, boolean ok, String detail) {
        if (ok) {
            passed++;
        } else {
            failures.add(name + ": " + detail);
        }
    }

    private static void runParseCase(Map<String, Object> c) {
        String name = "parse: " + c.get("name");
        String raw = (String) c.get("raw");
        if (Boolean.TRUE.equals(c.get("error"))) {
            try {
                Map<String, String> table = ExampleServer.parseRetailerMerchants(raw);
                check(name, false, "expected startup to refuse it, parsed " + table);
            } catch (IllegalArgumentException expected) {
                check(name, true, "");
            }
            return;
        }
        try {
            Map<String, String> table = ExampleServer.parseRetailerMerchants(raw);
            check(name, table.equals(c.get("expected")),
                    "expected " + c.get("expected") + ", got " + table);
        } catch (IllegalArgumentException e) {
            check(name, false, "refused a valid table: " + e.getMessage());
        }
    }

    @SuppressWarnings("unchecked")
    private static void runBindingCase(
            Map<String, Object> c, Map<String, Map<String, Object>> payloads, SeelClient client)
            throws Exception {
        String name = "binding: " + c.get("name");
        String single = (String) c.get("single_merchant_id");
        ExampleServer.config = new ExampleServer.Config(client, single == null ? "" : single,
                (Map<String, String>) (Map<?, ?>) c.get("retailer_merchants"));

        Map<String, Object> payload = new LinkedHashMap<>(payloads.get((String) c.get("route")));
        payload.remove("merchant_id");
        if (c.get("body_merchant_id") != null) {
            payload.put("merchant_id", c.get("body_merchant_id"));
        }
        received.clear();

        HttpResponse<String> response = send("POST", PATHS.get((String) c.get("route")),
                SeelValidation.toJson(payload), (String) c.get("origin"));

        int status = ((Number) c.get("status")).intValue();
        check(name + " status", response.statusCode() == status,
                "expected " + status + ", got " + response.statusCode() + " " + response.body());
        String allow = response.headers().firstValue("Access-Control-Allow-Origin").orElse(null);
        check(name + " allow-origin", Objects.equals(allow, c.get("allow_origin")),
                "expected " + c.get("allow_origin") + ", got " + allow);
        if (status == 200) {
            check(name + " upstream", received.size() == 1
                    && Objects.equals(received.get(0).get("merchant_id"), c.get("merchant_id")),
                    "expected one upstream call with merchant_id " + c.get("merchant_id")
                            + ", got " + received);
        } else {
            Object body = SeelValidation.parseJson(response.body());
            check(name + " error", Map.of("error", c.get("error")).equals(body),
                    "expected error " + c.get("error") + ", got " + response.body());
            check(name + " no upstream", received.isEmpty(),
                    "a refused request reached Seel: " + received);
        }
    }

    private static void preflight(SeelClient client) throws Exception {
        ExampleServer.config = new ExampleServer.Config(
                client, "", Map.of("https://dealer-a.example", "m-a"));
        HttpResponse<String> known =
                send("OPTIONS", PATHS.get("quote"), null, "https://dealer-a.example");
        HttpResponse<String> unknown =
                send("OPTIONS", PATHS.get("quote"), null, "https://other.example");
        check("preflight known origin",
                known.headers().firstValue("Access-Control-Allow-Origin")
                        .orElse("").equals("https://dealer-a.example")
                        && known.headers().firstValue("Vary").orElse("").equals("Origin"),
                "got " + known.headers().map());
        check("preflight unknown origin",
                unknown.headers().firstValue("Access-Control-Allow-Origin").isEmpty(),
                "got " + unknown.headers().map());
    }

    private static void bothConfigured(SeelClient client) {
        try {
            new ExampleServer.Config(client, "m-solo", Map.of("https://dealer-a.example", "m-a"));
            check("config with both SEEL_MERCHANT_ID and a table", false, "was accepted");
        } catch (IllegalArgumentException expected) {
            check("config with both SEEL_MERCHANT_ID and a table", true, "");
        }
    }

    private static HttpResponse<String> send(String method, String path, String body, String origin)
            throws Exception {
        HttpRequest.Builder request = HttpRequest.newBuilder(
                        URI.create("http://127.0.0.1:" + proxyPort + path))
                .header("Content-Type", "application/json")
                .method(method, body == null
                        ? HttpRequest.BodyPublishers.noBody()
                        : HttpRequest.BodyPublishers.ofString(body));
        if (origin != null) {
            request.header("Origin", origin);
        }
        return http.send(request.build(), HttpResponse.BodyHandlers.ofString());
    }
}
