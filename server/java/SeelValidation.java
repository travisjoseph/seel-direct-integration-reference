/*
 * Pre-flight payload validation for Seel's ecommerce APIs.
 *
 * Required-field sets measured against sandbox on 2026-09-10 by removing one
 * field per request from a known-good payload and recording the response.
 *
 * Treat these as a starting point, not a fixed contract. A newly provisioned
 * account behaves this way; as an integration develops, Seel's implementation
 * team works out which fields a merchant journey can actually supply and eases
 * the validation accordingly, so an established account may accept less.
 * Sending the full set is never wrong, but rejecting a payload locally could
 * be, so treat a reported problem as a warning worth checking rather than
 * proof the API would refuse it.
 *
 * Why this is separate from SeelClient, unlike the Python, Node and Rust
 * ports: SeelClient works in raw JSON strings to stay dependency-free, so it
 * cannot inspect a payload without a parser. Validate before you serialize -
 * parse with your own JSON library (Jackson, Gson, ...) and pass the
 * resulting Map here, or build the Map first and serialize it after.
 */

import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

public final class SeelValidation {

    private SeelValidation() {}

    private static final List<String> LINE_ITEM_REQUIRED = Arrays.asList(
            "line_item_id", "product_id", "product_title", "quantity", "price",
            "allocated_discounts", "sales_tax", "final_price", "currency",
            "requires_shipping", "image_urls", "category_1", "category_2",
            "is_final_sale", "shipping_origin");

    private static final Map<String, List<String>> QUOTE_REQUIRED = rules(
            "", Arrays.asList("merchant_id", "session_id", "device_category", "device_platform",
                    "type", "is_default_on", "customer", "shipping_address", "line_items"),
            "customer", Arrays.asList("customer_id", "email"),
            "shipping_address", Arrays.asList("address_1", "city", "state", "zipcode", "country"),
            "line_items[]", LINE_ITEM_REQUIRED,
            "line_items[].shipping_origin", Arrays.asList("country"));

    private static final Map<String, List<String>> ORDER_REQUIRED = rules(
            "", Arrays.asList("merchant_id", "order_id", "order_number", "created_ts",
                    "session_id", "device_category", "device_platform", "customer",
                    "shipping_address", "line_items"),
            "customer", Arrays.asList("customer_id", "email"),
            "shipping_address", Arrays.asList("address_1", "city", "state", "zipcode", "country"),
            "line_items[]", LINE_ITEM_REQUIRED,
            "line_items[].shipping_origin", Arrays.asList("country"));

    /** Only applied when seel_services is present and non-empty. */
    private static final Map<String, List<String>> ORDER_SERVICE_REQUIRED = rules(
            "seel_services[]", Arrays.asList("type", "quote_id", "price"));

    /**
     * coverages must be PRESENT but may be an empty list. Omitting it returns
     * a 500 rather than a validation error, so catching it locally is the
     * whole point of validating this call.
     */
    private static final Map<String, List<String>> MERCHANT_REQUIRED = rules(
            "", Arrays.asList("shop_id", "admin_domain", "shop_domain", "shop_platform",
                    "shop_currency", "shop_name", "contact_name", "contact_email",
                    "seel_services"),
            "seel_services[]", Arrays.asList("type", "coverages"));

    private static Map<String, List<String>> rules(Object... pairs) {
        Map<String, List<String>> out = new LinkedHashMap<>();
        for (int i = 0; i < pairs.length; i += 2) {
            @SuppressWarnings("unchecked")
            List<String> fields = (List<String>) pairs[i + 1];
            out.put((String) pairs[i], fields);
        }
        return out;
    }

    /**
     * Shape expectations, checked alongside presence. A scalar where an
     * object belongs is the archetypal payload mistake, and without this
     * the nested rules silently skip it: resolveScope only descends into
     * Maps, so {"customer": "nope"} would report no problems at all.
     *
     * <p>Each entry is {parent scope, key, kind}.
     */
    private static final String[][] QUOTE_SHAPES = {
        {"", "customer", "object"},
        {"", "shipping_address", "object"},
        {"", "line_items", "array_nonempty"},
        {"line_items[]", "shipping_origin", "object"},
    };
    private static final String[][] ORDER_EXTRA_SHAPES = {{"", "seel_services", "array"}};
    private static final String[][] MERCHANT_SHAPES = {{"", "seel_services", "array_nonempty"}};

    /**
     * One vocabulary for type names across all four ports, so the same
     * mistake reads the same way whichever one a partner runs.
     */
    private static String typeName(Object value) {
        if (value instanceof Map) return "object";
        if (value instanceof List) return "array";
        if (value instanceof String) return "string";
        if (value instanceof Boolean) return "boolean";
        if (value instanceof Number) return "number";
        return value == null ? "null" : value.getClass().getSimpleName();
    }

    private static List<String> checkShapes(Map<String, Object> payload, String[][] specs) {
        List<String> problems = new ArrayList<>();
        for (String[] spec : specs) {
            String scope = spec[0];
            String key = spec[1];
            String kind = spec[2];
            for (Scoped scoped : resolveScope(payload, scope)) {
                if (!scoped.node.containsKey(key) || scoped.node.get(key) == null) {
                    continue; // absence is the required-field check's job
                }
                Object value = scoped.node.get(key);
                String path = scoped.prefix + key;
                if (kind.equals("object") && !(value instanceof Map)) {
                    problems.add(path + " must be an object, got " + typeName(value));
                } else if (kind.startsWith("array")) {
                    if (!(value instanceof List)) {
                        problems.add(path + " must be an array, got " + typeName(value));
                    } else if (kind.equals("array_nonempty") && ((List<?>) value).isEmpty()) {
                        problems.add(path + " must not be empty");
                    }
                }
            }
        }
        return problems;
    }

    /** Return the problems with a Create Quote payload. Empty means clean. */
    public static List<String> validateQuote(Map<String, Object> payload) {
        List<String> problems = asProblems(collectMissing(payload, QUOTE_REQUIRED));
        problems.addAll(checkShapes(payload, QUOTE_SHAPES));
        return problems;
    }

    /**
     * Return the problems with a Create Order payload.
     *
     * <p>seel_services is only checked for completeness when present: syncing
     * an order the shopper did not opt into is normal. Two shape mistakes are
     * checked separately, because the API accepts both and then fails in ways
     * that do not look like failures.
     */
    public static List<String> validateOrder(Map<String, Object> payload) {
        Map<String, List<String>> combined = new LinkedHashMap<>(ORDER_REQUIRED);
        Object services = payload.get("seel_services");
        if (services instanceof List && !((List<?>) services).isEmpty()) {
            combined.putAll(ORDER_SERVICE_REQUIRED);
        }
        List<String> problems = asProblems(collectMissing(payload, combined));
        problems.addAll(checkShapes(payload, QUOTE_SHAPES));
        problems.addAll(checkShapes(payload, ORDER_EXTRA_SHAPES));

        // Create Order has no top-level quote_id. Sending one is the classic
        // attach mistake: the API returns 200 with seel_services: null and no
        // error, so the integration looks healthy while covering nothing.
        if (payload.containsKey("quote_id")) {
            problems.add("quote_id must go inside a seel_services entry, not at the top "
                    + "level - a top-level quote_id is ignored and the order attaches no "
                    + "coverage");
        }
        return problems;
    }

    /**
     * Parse a JSON document, or return null if it is not valid JSON.
     *
     * <p>SeelClient works in raw JSON strings to stay dependency-free, so
     * this reader exists to let the checks below reason about structure
     * rather than pattern-match text. An earlier version scanned with
     * regexes and got a partial attach wrong in both directions: a response
     * where one service minted and another failed read as healthy, and an
     * unrelated null contract_id elsewhere in the body read as a failure.
     *
     * <p>It is minimal on purpose. A real backend should use its own JSON
     * library rather than copy it.
     */
    static Object parseJson(String text) {
        if (text == null || text.isBlank()) {
            return null;
        }
        try {
            return new Json(text).parse();
        } catch (RuntimeException e) {
            return null;
        }
    }

    /** Does this request attach coverage, and so warrant a contract check? */
    public static boolean carriesCoverage(String requestJson) {
        Object doc = parseJson(requestJson);
        if (!(doc instanceof Map)) {
            return false;
        }
        Object services = ((Map<?, ?>) doc).get("seel_services");
        return services instanceof List && !((List<?>) services).isEmpty();
    }

    /**
     * Return why a Create Order response carries no contract, or null if it
     * does.
     *
     * <p>Seel reports a failed attach as contract_id: null on an otherwise
     * successful 200 - there is no error status code - so a caller that
     * trusts the status code believes it has coverage when it has none.
     *
     * <p>Pair it with {@link #carriesCoverage} on the request: an order
     * with no seel_services, or an empty array, is a normal uncovered sync
     * and must not be reported as a failure.
     */
    public static String contractNotMintedReason(String responseJson) {
        Object doc = parseJson(responseJson);
        if (!(doc instanceof Map)) {
            return "response was not a JSON object";
        }
        Object services = ((Map<?, ?>) doc).get("seel_services");
        if (!(services instanceof List) || ((List<?>) services).isEmpty()) {
            return "response seel_services is " + describe(services)
                    + " - check seel_services is an array and quote_id is inside it, not at "
                    + "the top level";
        }
        for (Object entry : (List<?>) services) {
            if (!(entry instanceof Map)) {
                return "seel_services contains a non-object entry: " + describe(entry);
            }
            Map<?, ?> service = (Map<?, ?>) entry;
            if (!isMinted(service.get("contract_id"))) {
                return "service " + describe(service.get("type")) + " returned contract_id "
                        + describe(service.get("contract_id")) + " (status="
                        + describe(service.get("status")) + ", error="
                        + describe(service.get("error")) + ")";
            }
        }
        return null;
    }

    /**
     * Falsy means not minted, matching the other ports: null, "", 0 and
     * false all mean no contract. A string "012" is a real id.
     */
    private static boolean isMinted(Object contractId) {
        if (contractId == null) return false;
        if (contractId instanceof String) return !((String) contractId).isEmpty();
        if (contractId instanceof Boolean) return (Boolean) contractId;
        if (contractId instanceof Number) return ((Number) contractId).doubleValue() != 0.0;
        return true;
    }

    private static String describe(Object value) {
        if (value == null) return "null";
        if (value instanceof String) return "\"" + value + "\"";
        return String.valueOf(value);
    }

    /** Return the problems with a Create Merchant payload. Empty means clean. */
    public static List<String> validateMerchant(Map<String, Object> payload) {
        List<String> problems = asProblems(collectMissing(payload, MERCHANT_REQUIRED));
        problems.addAll(checkShapes(payload, MERCHANT_SHAPES));
        return problems;
    }

    private static List<String> asProblems(List<String> missing) {
        List<String> out = new ArrayList<>(missing.size());
        for (String field : missing) {
            out.add("missing required field " + field);
        }
        return out;
    }

    /**
     * Missing means the key is absent, null, or an empty string. false and 0
     * are real values - is_default_on, requires_shipping and
     * allocated_discounts all legitimately take them. An empty list is a real
     * value too: merchant coverages: [] is accepted.
     */
    private static boolean isAbsent(Object value) {
        return value == null || "".equals(value);
    }

    private static List<String> collectMissing(
            Map<String, Object> payload, Map<String, List<String>> rules) {
        List<String> missing = new ArrayList<>();
        for (Map.Entry<String, List<String>> rule : rules.entrySet()) {
            for (Scoped scoped : resolveScope(payload, rule.getKey())) {
                for (String field : rule.getValue()) {
                    if (!scoped.node.containsKey(field) || isAbsent(scoped.node.get(field))) {
                        missing.add(scoped.prefix + field);
                    }
                }
            }
        }
        return missing;
    }

    /** A node a scope selected, with the dotted path that reached it. */
    private static final class Scoped {
        final Map<String, Object> node;
        final String prefix;

        Scoped(Map<String, Object> node, String prefix) {
            this.node = node;
            this.prefix = prefix;
        }
    }

    @SuppressWarnings("unchecked")
    private static List<Scoped> resolveScope(Map<String, Object> payload, String scope) {
        List<Scoped> nodes = new ArrayList<>();
        nodes.add(new Scoped(payload, ""));
        if (scope.isEmpty()) {
            return nodes;
        }
        for (String part : scope.split("\\.")) {
            boolean fanOut = part.endsWith("[]");
            String key = fanOut ? part.substring(0, part.length() - 2) : part;
            List<Scoped> next = new ArrayList<>();
            for (Scoped scoped : nodes) {
                Object child = scoped.node.get(key);
                if (fanOut && child instanceof List) {
                    List<?> items = (List<?>) child;
                    for (int i = 0; i < items.size(); i++) {
                        if (items.get(i) instanceof Map) {
                            next.add(new Scoped((Map<String, Object>) items.get(i),
                                    scoped.prefix + key + "[" + i + "]."));
                        }
                    }
                } else if (!fanOut && child instanceof Map) {
                    next.add(new Scoped((Map<String, Object>) child, scoped.prefix + key + "."));
                }
            }
            nodes = next;
        }
        return nodes;
    }

    static final class Json {
        private final String src;
        private int pos;

        Json(String src) {
            this.src = src;
        }

        Object parse() {
            Object value = readValue();
            skipWhitespace();
            if (pos != src.length()) {
                throw new IllegalArgumentException("trailing input at " + pos);
            }
            return value;
        }

        private Object readValue() {
            skipWhitespace();
            char c = src.charAt(pos);
            return switch (c) {
                case '{' -> readObject();
                case '[' -> readArray();
                case '"' -> readString();
                case 't' -> readLiteral("true", Boolean.TRUE);
                case 'f' -> readLiteral("false", Boolean.FALSE);
                case 'n' -> readLiteral("null", null);
                default -> readNumber();
            };
        }

        private Map<String, Object> readObject() {
            Map<String, Object> out = new LinkedHashMap<>();
            pos++; // {
            skipWhitespace();
            if (src.charAt(pos) == '}') {
                pos++;
                return out;
            }
            while (true) {
                skipWhitespace();
                String key = readString();
                skipWhitespace();
                expect(':');
                out.put(key, readValue());
                skipWhitespace();
                char c = src.charAt(pos++);
                if (c == '}') return out;
                if (c != ',') throw new IllegalArgumentException("expected , or } at " + pos);
            }
        }

        private List<Object> readArray() {
            List<Object> out = new ArrayList<>();
            pos++; // [
            skipWhitespace();
            if (src.charAt(pos) == ']') {
                pos++;
                return out;
            }
            while (true) {
                out.add(readValue());
                skipWhitespace();
                char c = src.charAt(pos++);
                if (c == ']') return out;
                if (c != ',') throw new IllegalArgumentException("expected , or ] at " + pos);
            }
        }

        private String readString() {
            expect('"');
            StringBuilder sb = new StringBuilder();
            while (true) {
                char c = src.charAt(pos++);
                if (c == '"') return sb.toString();
                if (c != '\\') {
                    sb.append(c);
                    continue;
                }
                char esc = src.charAt(pos++);
                switch (esc) {
                    case '"', '\\', '/' -> sb.append(esc);
                    case 'b' -> sb.append('\b');
                    case 'f' -> sb.append('\f');
                    case 'n' -> sb.append('\n');
                    case 'r' -> sb.append('\r');
                    case 't' -> sb.append('\t');
                    case 'u' -> {
                        sb.append((char) Integer.parseInt(src.substring(pos, pos + 4), 16));
                        pos += 4;
                    }
                    default -> throw new IllegalArgumentException("bad escape at " + pos);
                }
            }
        }

        private Object readNumber() {
            int start = pos;
            while (pos < src.length() && "+-.eE0123456789".indexOf(src.charAt(pos)) >= 0) {
                pos++;
            }
            String text = src.substring(start, pos);
            // Integers stay integers so a quantity of 1 is not "1.0".
            return text.contains(".") || text.contains("e") || text.contains("E")
                    ? (Object) Double.valueOf(text)
                    : (Object) Long.valueOf(text);
        }

        private Object readLiteral(String literal, Object value) {
            if (!src.startsWith(literal, pos)) {
                throw new IllegalArgumentException("bad literal at " + pos);
            }
            pos += literal.length();
            return value;
        }

        private void expect(char c) {
            if (src.charAt(pos++) != c) {
                throw new IllegalArgumentException("expected " + c + " at " + (pos - 1));
            }
        }

        private void skipWhitespace() {
            while (pos < src.length() && Character.isWhitespace(src.charAt(pos))) {
                pos++;
            }
        }
    }
}
