import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.List;

/** M2 framing parity: the shared tests/contract/framing.json negatives
 *  must all reject in BridgeProto.readFrame, valid shapes must accept.
 *  Category parity only (reject vs accept), never message text.
 *  Plus the programmatic exact-cap accept (1048576, same as py/js).
 *
 *  Compile: javac -cp <bridge classes> -d <out> tests/FramingJavaCheck.java
 *  Run from the repo root: java -cp <bridge classes>:<out> FramingJavaCheck
 */
public class FramingJavaCheck {
    static int failures = 0;

    static void check(boolean cond, String name) {
        if (cond) {
            System.out.println("ok: " + name);
        } else {
            System.out.println("FAIL: " + name);
            failures++;
        }
    }

    static class Case {
        String name;
        String expect;
        byte[] raw;
        String body;
    }

    /** Minimal reader for the controlled fixture shape only: an object with
     *  a "cases" array of flat objects whose string values hold no nested
     *  unescaped quotes. rawRepeat expands to prefix + char*count. */
    static String extract(String obj, String key) {
        String needle = "\"" + key + "\"";
        int k = obj.indexOf(needle);
        if (k < 0) return null;
        int c = obj.indexOf(':', k + needle.length());
        int q = obj.indexOf('"', c + 1);
        StringBuilder sb = new StringBuilder();
        for (int i = q + 1; i < obj.length(); i++) {
            char ch = obj.charAt(i);
            if (ch == '\\') {
                char e = obj.charAt(++i);
                switch (e) {
                    case 'r': sb.append('\r'); break;
                    case 'n': sb.append('\n'); break;
                    case 't': sb.append('\t'); break;
                    case '"': sb.append('"'); break;
                    case '\\': sb.append('\\'); break;
                    case 'u':
                        sb.append((char) Integer.parseInt(obj.substring(i + 1, i + 5), 16));
                        i += 4;
                        break;
                    default: sb.append(e); break;
                }
            } else if (ch == '"') {
                return sb.toString();
            } else {
                sb.append(ch);
            }
        }
        return null;
    }

    static List<Case> loadCases(Path fixture) throws Exception {
        String text = new String(Files.readAllBytes(fixture), StandardCharsets.UTF_8);
        int arr = text.indexOf('[', text.indexOf("\"cases\""));
        List<Case> out = new ArrayList<>();
        int i = arr + 1;
        while (true) {
            int start = text.indexOf('{', i);
            if (start < 0) break;
            int endArr = text.indexOf(']', i);
            if (endArr >= 0 && endArr < start) break;
            // Scan to the matching close brace, string-aware (nested
            // rawRepeat objects stay inside the outer case object).
            boolean inStr = false;
            boolean esc = false;
            int depth = 0;
            int j = start;
            for (; j < text.length(); j++) {
                char ch = text.charAt(j);
                if (inStr) {
                    if (esc) esc = false;
                    else if (ch == '\\') esc = true;
                    else if (ch == '"') inStr = false;
                } else {
                    if (ch == '"') inStr = true;
                    else if (ch == '{') depth++;
                    else if (ch == '}') {
                        depth--;
                        if (depth == 0) break;
                    }
                }
            }
            String obj = text.substring(start, j + 1);
            Case c = new Case();
            c.name = extract(obj, "name");
            c.expect = extract(obj, "expect");
            String raw = extract(obj, "raw");
            if (raw != null) {
                c.raw = raw.getBytes(StandardCharsets.UTF_8);
            } else {
                String prefix = extract(obj, "prefix");
                String chs = extract(obj, "char");
                String count = obj.replaceAll("(?s).*\"count\"\\s*:\\s*(\\d+).*", "$1");
                StringBuilder sb = new StringBuilder(prefix);
                int n = Integer.parseInt(count);
                for (int k = 0; k < n; k++) sb.append(chs);
                c.raw = sb.toString().getBytes(StandardCharsets.UTF_8);
            }
            c.body = extract(obj, "body");
            out.add(c);
            i = j + 1;
        }
        return out;
    }

    public static void main(String[] args) throws Exception {
        Path fixture = Paths.get("tests/contract/framing.json");
        List<Case> cases = loadCases(fixture);
        check(!cases.isEmpty(), "fixture loads (" + cases.size() + " cases)");
        for (Case c : cases) {
            boolean rejected;
            String got = null;
            try {
                got = BridgeProto.readFrame(new ByteArrayInputStream(c.raw));
                rejected = false;
            } catch (Exception e) {
                rejected = true;
            }
            if ("reject".equals(c.expect)) {
                check(rejected, "rejects " + c.name);
            } else {
                check(!rejected && c.body.equals(got), "accepts " + c.name + " (got: " + got + ")");
            }
        }
        // Exact-cap programmatic accept (same parameters as py/js).
        StringBuilder inner = new StringBuilder();
        for (int i = 0; i < 1048576 - 8; i++) inner.append('x');
        String body = "{\"k\":\"" + inner + "\"}";
        String raw = "Content-Length: 1048576\r\n\r\n" + body;
        try {
            String got = BridgeProto.readFrame(new ByteArrayInputStream(
                    raw.getBytes(StandardCharsets.UTF_8)));
            check(body.equals(got), "body at cap accepts (1048576)");
        } catch (Exception e) {
            check(false, "body at cap accepts (threw: " + e.getMessage() + ")");
        }
        if (failures > 0) {
            System.out.println("FAILURES: " + failures);
            System.exit(1);
        }
        System.out.println("all framing checks passed");
    }
}
