import com.sun.jdi.Location;
import com.sun.jdi.Method;
import com.sun.jdi.ReferenceType;
import com.sun.jdi.ThreadReference;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Proxy;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.Map;

/** M6 unit parity for the wait/capture/diagnostics UX batch (no live VM:
 *  every case below resolves before touching JDI transport — Location and
 *  ThreadReference are dynamic proxies). Covers: capture bounds parsing
 *  (defaults + every rejection), line-only capture spec parsing, wait and
 *  capture occupying the outstanding slot (rival resume/mutation/eval
 *  busy, close still accepted), the capture-ephemeral dup/conflict rules
 *  without VM traffic, and stop diagnostics (monotonic stopId,
 *  same-location/same-thread/elapsed, requested/bound attribution, native
 *  hit ids never fabricated).
 *
 *  Compile: javac -cp <bridge classes> -d <out> tests/M6JavaCheck.java
 *  Run:     java -cp <bridge classes>:<out> M6JavaCheck
 */
public class M6JavaCheck {
    static int failures = 0;

    static void check(boolean cond, String name) {
        if (cond) {
            System.out.println("ok: " + name);
        } else {
            System.out.println("FAIL: " + name);
            failures++;
        }
    }

    interface Throwing {
        void run() throws Exception;
    }

    static String expectBridgeError(String name, Throwing run, String wantSub) {
        try {
            run.run();
        } catch (BridgeException be) {
            boolean ok = be.getMessage() != null && be.getMessage().contains(wantSub);
            check(ok, name + " (got: " + be.getMessage() + ")");
            return be.getMessage();
        } catch (Exception e) {
            check(false, name + " (wrong exception: " + e + ")");
            return null;
        }
        check(false, name + " (no error raised)");
        return null;
    }

    static SessionState freshState(Path tmp) throws Exception {
        SessionState st = new SessionState();
        st.cfg = new Config();
        st.cfg.sessionKind = "attach";
        st.cfg.timeoutMs = 20000;
        st.dir = tmp;
        st.ownerNonce = "m6-test-nonce";
        Files.write(tmp.resolve("owner.json"),
                "{\"pid\":1,\"nonce\":\"m6-test-nonce\"}".getBytes("UTF-8"));
        return st;
    }

    /** Proxy a JDI interface by method name (unlisted methods return null). */
    @SuppressWarnings("unchecked")
    static <T> T fake(Class<T> iface, Map<String, Object> answers) {
        InvocationHandler h = (proxy, m, args) -> {
            if (m.getName().equals("toString")) return "fake";
            if (m.getName().equals("hashCode")) return 0;
            if (m.getName().equals("equals")) return proxy == args[0];
            return answers.get(m.getName());
        };
        return (T) Proxy.newProxyInstance(M6JavaCheck.class.getClassLoader(),
                new Class<?>[]{iface}, h);
    }

    static Location loc(String cls, int line, String method) {
        ReferenceType rt = fake(ReferenceType.class, Map.of("name", cls));
        com.sun.jdi.Method m = fake(com.sun.jdi.Method.class, Map.of("name", method));
        Map<String, Object> a = new LinkedHashMap<>();
        a.put("declaringType", rt);
        a.put("lineNumber", line);
        a.put("method", m);
        return fake(Location.class, a);
    }

    static ThreadReference thread(long id, String name) {
        Map<String, Object> a = new LinkedHashMap<>();
        a.put("uniqueID", id);
        a.put("name", name);
        return fake(ThreadReference.class, a);
    }

    public static void main(String[] argv) throws Exception {
        Path tmp = Files.createTempDirectory("m6-java-");

        // 1. Capture bounds: defaults + every rejection, no VM traffic.
        {
            long[] budget = new long[1];
            String[] spec = new String[1];
            Map<String, String> empty = new LinkedHashMap<>();
            int[] b = BridgeSession.parseCaptureBounds(empty, budget, spec);
            check(b[0] == 1 && b[1] == 20 && budget[0] == 2000 && spec[0] == null,
                    "capture bounds default frames=1 vars=20 budget=2000ms");
            Map<String, String> full = new LinkedHashMap<>();
            full.put("frames", "3");
            full.put("vars", "5");
            full.put("pauseBudgetMs", "500");
            full.put("break", "com.Foo:54");
            b = BridgeSession.parseCaptureBounds(full, budget, spec);
            check(b[0] == 3 && b[1] == 5 && budget[0] == 500
                    && "com.Foo:54".equals(spec[0]), "capture bounds accept explicit values");
            String[][] bad = {
                {"frames", "0"}, {"frames", "11"}, {"vars", "0"}, {"vars", "21"},
                {"pauseBudgetMs", "0"}, {"pauseBudgetMs", "10001"},
                {"frames", "x"}, {"pauseBudgetMs", "-5"},
            };
            for (String[] kv : bad) {
                Map<String, String> m = new LinkedHashMap<>();
                m.put(kv[0], kv[1]);
                expectBridgeError("capture rejects " + kv[0] + "=" + kv[1],
                        () -> BridgeSession.parseCaptureBounds(m, new long[1], new String[1]),
                        "capture");
            }
        }

        // 2. Capture specs are line-only (method:/exc: rejected capture-worded).
        {
            SessionState st = freshState(tmp);
            BridgeSession.AddedLine p =
                    BridgeSession.parseAddedLine("com.Foo:54|x == null");
            check(p.cls.equals("com.Foo") && p.line == 54 && "x == null".equals(p.cond),
                    "capture parses Class:line|cond");
            try {
                BridgeSession.parseAddedLine("method:com.Foo.bar");
                check(false, "capture rejects method: specs");
            } catch (UsageException ue) {
                check(ue.getMessage().contains("line breaks only"),
                        "capture rejects method: specs");
            }
            try {
                BridgeSession.parseAddedLine("exc:Boom");
                check(false, "capture rejects exc: specs");
            } catch (UsageException ue) {
                check(ue.getMessage().contains("line breaks only"),
                        "capture rejects exc: specs");
            }
        }

        // 3. wait/capture occupy the slot: rivals busy, close accepted.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.outstanding = "wait";
            }
            expectBridgeError("continue busy behind wait",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"continue\"}"),
                    "busy: wait outstanding for main");
            expectBridgeError("eval busy behind wait",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"eval\",\"expr\":\"1\"}"),
                    "busy: wait outstanding for main");
            expectBridgeError("breaksAdd busy behind wait",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"breaksAdd\",\"breaks\":[\"A:1\"]}"),
                    "busy: wait outstanding for main");
            expectBridgeError("capture busy behind wait",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"capture\"}"),
                    "busy: wait outstanding for main");
            expectBridgeError("wait busy behind capture",
                    () -> {
                        synchronized (st.sessionLock) {
                            st.outstanding = "capture";
                        }
                        BridgeSession.dispatch(st, "{\"cmd\":\"wait\"}");
                    },
                    "busy: capture outstanding for main");
            // wait/capture themselves register (dispatch path): a parked
            // frame read still fails fast, never busy.
            synchronized (st.sessionLock) {
                st.outstanding = "wait";
                st.suspended = false;
            }
            expectBridgeError("context fails fast (never busy) behind wait",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"context\"}"),
                    "no stopped thread");
        }

        // 4. Capture-ephemeral dup/conflict rules without VM traffic.
        {
            SessionState st = freshState(tmp);
            st.cfg.breakpoints.put("com.Foo", new ArrayList<>(java.util.List.of(54)));
            BridgeSession.AddedLine dup = BridgeSession.parseAddedLine("com.Foo:54");
            check(!BridgeSession.plantCaptureBreak(st, dup),
                    "capture dup of armed line plants nothing");
            check(st.captureCls == null, "dup leaves no ephemeral fields");
            BridgeSession.AddedLine conflict =
                    BridgeSession.parseAddedLine("com.Foo:54|x > 1");
            expectBridgeError("capture same-line cond conflict",
                    () -> BridgeSession.plantCaptureBreak(st, conflict),
                    "conflicting condition");
            st.cfg.logpoints.add(new Logpoint());
            st.cfg.logpoints.get(0).cls = "com.Bar";
            st.cfg.logpoints.get(0).line = 7;
            BridgeSession.AddedLine shadowed =
                    BridgeSession.parseAddedLine("com.Bar:7");
            expectBridgeError("capture same-line logpoint conflict",
                    () -> BridgeSession.plantCaptureBreak(st, shadowed),
                    "already armed as logpoint");
        }

        // 5. Diagnostics: monotonic stopId, same-line/thread, attribution.
        {
            SessionState st = freshState(tmp);
            st.cfg.breakpoints.put("com.Foo", new ArrayList<>(java.util.List.of(54)));
            st.thread = thread(11, "main");
            st.location = loc("com.Foo", 54, "bill");
            synchronized (st.sessionLock) {
                BridgeSession.notePark(st, "breakpoint");
            }
            check(st.lastStopId == 1, "first park is stopId 1");
            String d1 = BridgeSession.stopDiagJson(st);
            check(d1.contains("\"stopId\":1"), "diag carries stopId");
            check(d1.contains("\"reason\":\"breakpoint\""), "diag carries reason");
            check(d1.contains("\"hitBreakpoints\":null"), "hit ids never fabricated");
            check(d1.contains("\"requestedBreak\":\"com.Foo:54\""),
                    "diag attributes requested break: " + d1);
            check(d1.contains("\"boundLine\":54"), "diag carries bound line");
            check(d1.contains("\"sameLocation\":false"), "first park is not same-location");
            check(d1.contains("\"elapsedSincePreviousStopMs\":null"),
                    "first park has no elapsed");
            check(d1.contains("\"id\":11") && d1.contains("\"name\":\"main\""),
                    "diag names the stopping thread");
            // Same line, same thread: diagnose, don't suppress.
            synchronized (st.sessionLock) {
                BridgeSession.notePark(st, "breakpoint");
            }
            check(st.lastStopId == 2, "second park is stopId 2");
            String d2 = BridgeSession.stopDiagJson(st);
            check(d2.contains("\"sameLocation\":true"), "same-line re-hit diagnosed");
            check(d2.contains("\"sameThread\":true"), "same-thread diagnosed");
            check(!d2.contains("\"elapsedSincePreviousStopMs\":null"),
                    "elapsed present from the second park");
            // Other line: not same-location.
            st.location = loc("com.Foo", 55, "bill");
            synchronized (st.sessionLock) {
                BridgeSession.notePark(st, "step");
            }
            String d3 = BridgeSession.stopDiagJson(st);
            check(d3.contains("\"sameLocation\":false"), "slide/advance is not same-location");
            check(d3.contains("\"reason\":\"step\""), "step reason recorded");
            // Unattributable location: nulls, never fabrications.
            st.location = loc("com.Other", 1, "run");
            String d4 = BridgeSession.stopDiagJson(st);
            check(d4.contains("\"requestedBreak\":null")
                    && d4.contains("\"boundLine\":null")
                    && d4.contains("\"hitCount\":null"),
                    "unattributable stop stays null");
        }

        // 6. waitJson shape: waited flag + diag + parked warning.
        {
            SessionState st = freshState(tmp);
            st.thread = thread(11, "main");
            st.location = loc("com.Foo", 54, "bill");
            st.lastChanged = "[]";
            synchronized (st.sessionLock) {
                BridgeSession.notePark(st, "breakpoint");
            }
            String w = BridgeSession.waitJson(st, "{\"mode\":\"session\"}", false);
            check(w.contains("\"waited\":false"), "immediate wait stamps waited:false");
            check(w.contains("\"diag\":{"), "wait carries diag");
            check(w.contains("HTTP handler remains open"), "wait carries parked warning");
            check(w.contains("\"target\":\"main\""), "wait stamps target main");
        }

        if (failures > 0) {
            System.out.println("M6JavaCheck: " + failures + " FAILURES");
            System.exit(1);
        }
        System.out.println("M6JavaCheck: all green");
    }
}
