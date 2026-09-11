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
 * The minimal JSON reader below exists only so this port can join that
 * fixture. SeelClient stays dependency-free by working in raw JSON strings,
 * which means there is no parser to borrow. A real backend should use its
 * own JSON library rather than copy this.
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
        Map<String, Object> doc = (Map<String, Object>) new Json(raw).parse();
        @SuppressWarnings("unchecked")
        List<Object> cases = (List<Object>) doc.get("cases");

        int passed = 0;
        List<String> failures = new ArrayList<>();

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
            }
        }

        for (String failure : failures) {
            System.out.println("FAIL  " + failure);
        }
        System.out.println(passed + "/" + cases.size() + " shared validation cases passed");
        if (!failures.isEmpty()) {
            System.exit(1);
        }
    }

    /** Minimal JSON reader. Test-only; see the file comment. */
    private static final class Json {
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
