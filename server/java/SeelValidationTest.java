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

        int total = cases.size() + contractCases.size() + uncovered.size();
        for (String failure : failures) {
            System.out.println("FAIL  " + failure);
        }
        System.out.println(passed + "/" + total + " shared cases passed");
        if (!failures.isEmpty()) {
            System.exit(1);
        }
    }
}
