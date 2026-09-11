/*
 * Pre-flight payload validation for Seel's ecommerce APIs.
 *
 * Required-field sets measured against sandbox on 2026-09-10 by removing one
 * field per request from a known-good payload and recording the response.
 *
 * Requiredness is PER-ACCOUNT. Seel validates a strict default profile and
 * relaxes individual fields for some accounts, so an account may legitimately
 * accept less than this. These sets are the strict profile: sending them is
 * never wrong, but rejecting a payload locally could be. Treat a reported
 * problem as a warning worth checking, not as proof the API would refuse it.
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
import java.util.regex.Matcher;
import java.util.regex.Pattern;

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

    /** Return the problems with a Create Quote payload. Empty means clean. */
    public static List<String> validateQuote(Map<String, Object> payload) {
        return asProblems(collectMissing(payload, QUOTE_REQUIRED));
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

        // Create Order has no top-level quote_id. Sending one is the classic
        // attach mistake: the API returns 200 with seel_services: null and no
        // error, so the integration looks healthy while covering nothing.
        if (payload.containsKey("quote_id")) {
            problems.add("quote_id must go inside a seel_services entry, not at the top "
                    + "level - a top-level quote_id is ignored and the order attaches no "
                    + "coverage");
        }
        if (services != null && !(services instanceof List)) {
            problems.add("seel_services must be a list, got "
                    + services.getClass().getSimpleName()
                    + " - an object is rejected by the parser with a 500");
        }
        return problems;
    }

    private static final Pattern SEEL_SERVICES_NULL =
            Pattern.compile("\"seel_services\"\\s*:\\s*null");
    private static final Pattern CONTRACT_ID_NULL =
            Pattern.compile("\"contract_id\"\\s*:\\s*(null|\"\")");

    /**
     * Return why a Create Order response carries no contract, or null if it
     * does.
     *
     * <p>Seel reports a failed attach as contract_id: null on an otherwise
     * successful 200 - there is no error status code - so a caller that
     * trusts the status code believes it has coverage when it has none.
     *
     * <p>This scans the raw response text rather than parsing it, because
     * SeelClient is deliberately dependency-free. That makes it a
     * best-effort check, not a parser: it will not understand a
     * contract_id nested somewhere unexpected. Call it only when the
     * request actually carried a seel_services array. With a JSON library
     * available, read seel_services[].contract_id directly instead.
     */
    public static String contractNotMintedReason(String responseJson) {
        if (responseJson == null || responseJson.isEmpty()) {
            return "empty response body";
        }
        if (SEEL_SERVICES_NULL.matcher(responseJson).find()) {
            return "response seel_services is null - check seel_services is an array and "
                    + "quote_id is inside it, not at the top level";
        }
        Matcher m = CONTRACT_ID_NULL.matcher(responseJson);
        if (m.find()) {
            return "a seel_services entry returned contract_id " + m.group(1);
        }
        if (!responseJson.contains("contract_id")) {
            return "response carries no contract_id";
        }
        return null;
    }

    /** Return the problems with a Create Merchant payload. Empty means clean. */
    public static List<String> validateMerchant(Map<String, Object> payload) {
        return asProblems(collectMissing(payload, MERCHANT_REQUIRED));
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
}
