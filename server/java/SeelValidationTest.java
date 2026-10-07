/*
 * Runs the shared validation cases against this port.
 *
 *   javac *.java && java SeelValidationTest
 *
 * Exits non-zero on failure, so it works as a CI step.
 *
 * The cases live in ../validation-cases.json and are read by the test suite
 * in every language port. They exist to catch drift: the ports must make
 * the same accept/reject decision and report the same field paths, and four
 * hand-written suites would encode divergence rather than catch it.
 *
 * Uses SeelValidation's minimal JSON reader, which exists there so the
 * contract check can reason about structure rather than pattern-match text.
 */

import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

public class SeelValidationTest {

    public static void main(String[] args) throws IOException {
        Path fixture = Path.of("..", "validation-cases.json");
        String raw = Files.readString(fixture, StandardCharsets.UTF_8);

        @SuppressWarnings("unchecked")
        Map<String, Object> doc = (Map<String, Object>) SeelValidation.parseJson(raw);
        @SuppressWarnings("unchecked")
        List<Object> cases = (List<Object>) doc.get("cases");

        @SuppressWarnings("unchecked")
        List<Object> contractCases = (List<Object>) doc.get("contract_cases");
        @SuppressWarnings("unchecked")
        List<Object> uncovered = (List<Object>) doc.get("uncovered_requests");

        int passed = 0;
        List<String> failures = new ArrayList<>();

        // The fixture is the single point of failure for cross-port drift,
        // so an emptied or defanged one has to fail rather than pass green.
        if (cases.isEmpty() || contractCases.isEmpty() || uncovered.isEmpty()) {
            System.out.println("FAIL  fixture is empty");
            System.exit(1);
        }
        for (Object entry : cases) {
            @SuppressWarnings("unchecked")
            Map<String, Object> testCase = (Map<String, Object>) entry;
            if (!Boolean.TRUE.equals(testCase.get("expect_clean"))) {
                @SuppressWarnings("unchecked")
                List<Object> expected = (List<Object>) testCase.get("expect_contains");
                if (expected == null || expected.isEmpty()) {
                    failures.add(testCase.get("name") + ": expect_contains is empty");
                }
            }
        }

        for (Object entry : cases) {
            @SuppressWarnings("unchecked")
            Map<String, Object> testCase = (Map<String, Object>) entry;
            String name = (String) testCase.get("name");
            @SuppressWarnings("unchecked")
            Map<String, Object> payload = (Map<String, Object>) testCase.get("payload");

            List<String> problems = switch ((String) testCase.get("operation")) {
                case "quote" -> SeelValidation.validateQuote(payload);
                case "order" -> SeelValidation.validateOrder(payload);
                case "merchant" -> SeelValidation.validateMerchant(payload);
                default -> throw new IllegalStateException("unknown operation in " + name);
            };
            String joined = String.join("; ", problems);

            if (Boolean.TRUE.equals(testCase.get("expect_clean"))) {
                if (problems.isEmpty()) {
                    passed++;
                } else {
                    failures.add(name + ": expected no problems, got " + joined);
                }
            } else {
                @SuppressWarnings("unchecked")
                List<Object> expected = (List<Object>) testCase.get("expect_contains");
                List<String> missing = new ArrayList<>();
                for (Object fragment : expected) {
                    if (!joined.contains((String) fragment)) {
                        missing.add((String) fragment);
                    }
                }
                if (missing.isEmpty()) {
                    passed++;
                } else {
                    failures.add(name + ": expected " + missing + " in \"" + joined + "\"");
                }
                if (problems.isEmpty()) {
                    failures.add(name + ": expected problems, got none");
                }
            }
        }

        // The post-condition check on Create Order must agree across ports.
        // This port scans raw JSON, so the fixture ships the same values
        // pre-serialized.
        for (Object entry : contractCases) {
            @SuppressWarnings("unchecked")
            Map<String, Object> testCase = (Map<String, Object>) entry;
            String name = (String) testCase.get("name");
            String responseJson = (String) testCase.get("response_json");
            boolean minted = SeelValidation.contractNotMintedReason(responseJson) == null;
            if (minted == Boolean.TRUE.equals(testCase.get("expect_minted"))) {
                passed++;
            } else {
                failures.add("contract: " + name + ": expected minted="
                        + testCase.get("expect_minted") + ", got " + minted);
            }
        }

        for (Object entry : uncovered) {
            @SuppressWarnings("unchecked")
            Map<String, Object> testCase = (Map<String, Object>) entry;
            String name = (String) testCase.get("name");
            boolean checked = SeelValidation.carriesCoverage((String) testCase.get("request_json"));
            if (checked == Boolean.TRUE.equals(testCase.get("expect_checked"))) {
                passed++;
            } else {
                failures.add("gating: " + name + ": expected checked="
                        + testCase.get("expect_checked") + ", got " + checked);
            }
        }

        // Parser checks only this port needs, since only this port parses.
        // Each guards a defect the example server once shipped: a duplicate
        // key rejected here but accepted by every other port, so a signed
        // webhook was ACKed then dropped; a deep body overflowing the stack
        // with no response; a megabyte-long number literal taking seconds
        // to construct; and a stamped body going upstream changed.
        Map<String, Boolean> parserChecks = new LinkedHashMap<>();
        Object dup = SeelValidation.parseJson("{\"a\":1,\"a\":2}");
        parserChecks.put("duplicate keys resolve last-key-wins",
                dup instanceof Map && Long.valueOf(2).equals(((Map<?, ?>) dup).get("a")));
        Object escapedDup = SeelValidation.parseJson("{\"a\":1,\"\\u0061\":2}");
        parserChecks.put("an escaped spelling of a key is the same key, last wins",
                escapedDup instanceof Map && Long.valueOf(2).equals(((Map<?, ?>) escapedDup).get("a")));
        parserChecks.put("20000-deep nesting fails cleanly",
                SeelValidation.parseJson("[".repeat(20000) + "]".repeat(20000)) == null);
        parserChecks.put("a 65-digit number fails cleanly",
                SeelValidation.parseJson("{\"n\":" + "9".repeat(65) + "}") == null);
        parserChecks.put("a 64-digit number still parses",
                SeelValidation.parseJson("{\"n\":" + "9".repeat(64) + "}") != null);
        String sample = "{\"n\":12345678901234567890,\"p\":1.10,\"s\":\"q\\\"\\\\\\n\\u0001\","
                + "\"l\":[true,null,{}]}";
        parserChecks.put("parse then serialize round-trips " + sample,
                sample.equals(SeelValidation.toJson(SeelValidation.parseJson(sample))));
        for (Map.Entry<String, Boolean> check : parserChecks.entrySet()) {
            if (check.getValue()) {
                passed++;
            } else {
                failures.add("parser: " + check.getKey());
            }
        }

        int total = cases.size() + contractCases.size() + uncovered.size() + parserChecks.size();
        for (String failure : failures) {
            System.out.println("FAIL  " + failure);
        }
        System.out.println(passed + "/" + total + " shared cases passed");
        if (!failures.isEmpty()) {
            System.exit(1);
        }
    }
}
