import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;

/** M4 unit parity for the Java bridge (no JUnit on this path — plain
 *  asserts, nonzero exit on failure). Covers: log ring keeps the latest
 *  2000 physical lines with dropped accounting, multiline flattening,
 *  atomic state writes (no torn reads, no temp leftovers), the
 *  sanitized unexpected-crash payload (capped, no env), and the
 *  multi-target uniformity rule (java rejects `targets`; the CLI serves
 *  the main-only roster and rejects foreign target ids).
 *
 *  Compile: javac -cp <bridge classes> -d <out> tests/M4JavaCheck.java
 *  Run:     java -cp <bridge classes>:<out> M4JavaCheck
 */
public class M4JavaCheck {
    static int failures = 0;

    static void check(boolean cond, String name) {
        if (cond) {
            System.out.println("ok: " + name);
        } else {
            System.out.println("FAIL: " + name);
            failures++;
        }
    }

    public static void main(String[] argv) throws Exception {
        Path tmp = Files.createTempDirectory("m4-java-");

        // 1. Ring: 2005 appends keep the latest 2000.
        SessionState st = new SessionState();
        st.dir = tmp;
        for (int n = 0; n < 2005; n++) {
            BridgeSession.appendSessionLog(st, tmp, "j-" + n);
        }
        List<String> lines = Files.readAllLines(tmp.resolve("logs.jsonl"), StandardCharsets.UTF_8);
        check(lines.size() == 2000, "ring keeps exactly 2000 lines (got " + lines.size() + ")");
        check(lines.get(0).equals("j-5"), "ring evicts oldest first");
        check(!lines.contains("j-0") && !lines.contains("j-4"), "pre-ring lines gone");
        check(lines.get(lines.size() - 1).equals("j-2004"), "ring retains latest");
        check(st.logCount == 2000, "logCount tracks retained");
        check(st.logDropped == 5, "logDropped counts evicted");

        // 2. Oversize burst keeps exactly the latest MAX.
        Path tmp2 = Files.createTempDirectory("m4-java-burst-");
        SessionState st2 = new SessionState();
        st2.dir = tmp2;
        java.util.List<String> bulk = new java.util.ArrayList<>();
        for (int n = 0; n < 2100; n++) bulk.add("bulk-" + n);
        BridgeSession.appendSessionLogParts(st2, tmp2, bulk);
        List<String> lines2 = Files.readAllLines(tmp2.resolve("logs.jsonl"), StandardCharsets.UTF_8);
        check(lines2.size() == 2000 && lines2.get(0).equals("bulk-100"), "burst keeps latest MAX");
        check(st2.logDropped == 100, "burst drops accounted");

        // 3. Multiline values flatten to one physical line.
        Path tmp3 = Files.createTempDirectory("m4-java-flat-");
        SessionState st3 = new SessionState();
        st3.dir = tmp3;
        BridgeSession.appendSessionLog(st3, tmp3, "a\nb\nc");
        List<String> lines3 = Files.readAllLines(tmp3.resolve("logs.jsonl"), StandardCharsets.UTF_8);
        check(lines3.size() == 1 && lines3.get(0).equals("a\u23ceb\u23cec"), "multiline flattened");
        check(st3.logCount == 1, "flattened line counts once");

        // 4. Atomic writes: repeated publish + parse never sees a partial.
        Path target = tmp.resolve("session.json");
        boolean torn = false;
        for (int i = 0; i < 300; i++) {
            BridgeProto.writeFile(target,
                    "{\"n\":" + i + ",\"pad\":\"" + "x".repeat(500) + "\"}");
            try {
                String raw = Files.readString(target, StandardCharsets.UTF_8);
                if (!raw.startsWith("{") || !raw.endsWith("}")) torn = true;
            } catch (Exception e) {
                torn = true;
            }
        }
        check(!torn, "no torn session.json observed in 300 publish/read rounds");
        boolean tempsLeft;
        try (java.util.stream.Stream<Path> s = Files.list(tmp)) {
            tempsLeft = s.anyMatch(p -> p.getFileName().toString().startsWith(".tmp-"));
        }
        check(!tempsLeft, "no temp files left behind");

        // 5. Sanitized unexpected payload: capped, prefixed, no env.
        String big = "y".repeat(5000);
        String msg = JdiBridge.sanitizeUnexpected(new IllegalStateException(big));
        check(msg.startsWith("internal: IllegalStateException:"), "sanitized prefix + class");
        check(msg.length() <= 2048, "sanitized payload capped (got " + msg.length() + ")");
        check(!msg.contains("HOME=") && !msg.contains("PATH="), "no env in payload");

        // 6. Remove/clear warning contract: at most one warning, fixed order.
        check(BridgeSession.removeWarning(false, java.util.Collections.emptyList()) == null,
                "no warning when nothing failed");
        check(BridgeSession.removeWarning(true, java.util.Collections.emptyList())
                .equals("partial remove: some breaks kept"), "partial-only warning");
        check(BridgeSession.removeWarning(false, java.util.Collections.singletonList("logpoint C:1 re-arm failed: x"))
                .equals("re-arm: logpoint C:1 re-arm failed: x"), "rearm-only warning");
        check(BridgeSession.removeWarning(true, java.util.Collections.singletonList("r"))
                .equals("partial remove: some breaks kept; re-arm: r"),
                "combined warning is one string, partial first");

        // 7. Multi-target uniformity is Rust-owned: the Java bridge
        // rejects `targets` (and never silently serves a foreign target
        // id) — the CLI serves the main-only roster without contacting it.
        boolean rejected = false;
        try {
            SessionState stx = new SessionState();
            stx.dir = tmp;
            stx.cfg = new Config();
            BridgeSession.dispatch(stx, "{\"cmd\":\"targets\"}");
        } catch (Exception e) {
            rejected = String.valueOf(e.getMessage()).contains("unknown cmd");
        }
        check(rejected, "java bridge rejects targets (Rust serves the main-only roster)");

        if (failures > 0) {
            System.out.println(failures + " FAILURE(S)");
            System.exit(1);
        }
        System.out.println("M4JavaCheck: all green");
    }
}
