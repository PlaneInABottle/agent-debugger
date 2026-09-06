import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

/** M5 unit parity for the Java bridge (no JUnit on this path — plain
 *  asserts, nonzero exit on failure; no live VM needed: every case below
 *  resolves before touching JDI). Covers: live reads stay prompt with no
 *  JDI while a resume is outstanding; second resume / mutation / eval on
 *  the busy target reject immediately with the frozen busy shape; frame
 *  reads fail fast once the resume publishes running (never stale); close
 *  is accepted despite an outstanding resume; the handler bound is small
 *  and fixed; concurrent live dispatches from threads all succeed.
 *
 *  Compile: javac -cp <bridge classes> -d <out> tests/M5JavaCheck.java
 *  Run:     java -cp <bridge classes>:<out> M5JavaCheck
 */
public class M5JavaCheck {
    static int failures = 0;

    static void check(boolean cond, String name) {
        if (cond) {
            System.out.println("ok: " + name);
        } else {
            System.out.println("FAIL: " + name);
            failures++;
        }
    }

    static SessionState freshState(Path tmp) throws Exception {
        SessionState st = new SessionState();
        st.cfg = new Config();
        st.cfg.sessionKind = "attach";
        st.cfg.timeoutMs = 20000;
        st.dir = tmp;
        // Claim ownership so awaitStop never takes the abandonment exit.
        st.ownerNonce = "m5-test-nonce";
        Files.write(tmp.resolve("owner.json"),
                "{\"pid\":1,\"nonce\":\"m5-test-nonce\"}".getBytes("UTF-8"));
        return st;
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

    interface Throwing {
        void run() throws Exception;
    }

    public static void main(String[] argv) throws Exception {
        Path tmp = Files.createTempDirectory("m5-java-");

        // 1. Handler bound is small and fixed.
        check(BridgeSession.MAX_ACTIVE_HANDLERS == 8, "MAX_ACTIVE_HANDLERS is 8");

        // 2. Live reads answer promptly with no VM while a resume is held.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.outstanding = "continue";
            }
            long t0 = System.nanoTime();
            String threads = BridgeSession.dispatch(st, "{\"cmd\":\"threads\"}");
            String breaks = BridgeSession.dispatch(st, "{\"cmd\":\"breaks\"}");
            String logs = BridgeSession.dispatch(st, "{\"cmd\":\"logs\"}");
            long ms = (System.nanoTime() - t0) / 1_000_000;
            check(threads.contains("\"ok\":true") && threads.contains("\"running\":true"),
                    "threads served running while resume held: " + threads);
            check(breaks.contains("\"ok\":true") && breaks.contains("\"stops\":[]"),
                    "breaks served while resume held: " + breaks);
            check(logs.contains("\"ok\":true"), "logs served while resume held");
            check(ms < 2000, "live reads prompt (" + ms + "ms)");
        }

        // 3. Second resume / mutation / eval busy-reject on the busy target.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.outstanding = "continue";
            }
            expectBridgeError("second continue is busy",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"continue\"}"),
                    "busy: continue outstanding for main");
            expectBridgeError("step is busy",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"step\"}"),
                    "busy: continue outstanding for main");
            expectBridgeError("breaksAdd is busy",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"breaksAdd\",\"breaks\":[\"A:1\"]}"),
                    "busy: continue outstanding for main");
            expectBridgeError("breaksRemove is busy",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"breaksRemove\",\"breaks\":[\"A:1\"]}"),
                    "busy: continue outstanding for main");
            expectBridgeError("breaksClear is busy",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"breaksClear\"}"),
                    "busy: continue outstanding for main");
            expectBridgeError("eval is busy despite no frames check",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"eval\",\"expr\":\"1\"}"),
                    "busy: continue outstanding for main");
        }

        // 4. Frame reads fail fast while running (never stale, never busy).
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.outstanding = "continue";
                st.suspended = false;
            }
            expectBridgeError("context fails fast while running",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"context\"}"),
                    "no stopped thread");
            expectBridgeError("stack fails fast while running",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"stack\"}"),
                    "no stopped thread");
            expectBridgeError("vars fails fast while running",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"vars\"}"),
                    "no stopped thread");
        }

        // 5. Close is accepted despite an outstanding resume.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.outstanding = "continue";
            }
            try {
                BridgeSession.dispatch(st, "{\"cmd\":\"close\"}");
                check(false, "close accepted (no error)");
            } catch (CloseSession c) {
                check(true, "close accepted despite outstanding resume");
            } catch (Exception e) {
                check(false, "close accepted (wrong exception: " + e + ")");
            }
        }

        // 6. Concurrent live dispatches from threads all succeed promptly.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.outstanding = "continue";
            }
            String[] cmds = {"{\"cmd\":\"threads\"}", "{\"cmd\":\"breaks\"}",
                    "{\"cmd\":\"logs\"}", "{\"cmd\":\"threads\"}",
                    "{\"cmd\":\"breaks\"}", "{\"cmd\":\"logs\"}",
                    "{\"cmd\":\"threads\"}", "{\"cmd\":\"breaks\"}"};
            List<String> out = new ArrayList<>();
            for (int i = 0; i < cmds.length; i++) out.add(null);
            List<Thread> ths = new ArrayList<>();
            for (int i = 0; i < cmds.length; i++) {
                final int idx = i;
                Thread t = new Thread(() -> {
                    try {
                        out.set(idx, BridgeSession.dispatch(st, cmds[idx]));
                    } catch (Exception e) {
                        out.set(idx, "ERROR: " + e);
                    }
                });
                t.setDaemon(true);
                ths.add(t);
            }
            long t0 = System.nanoTime();
            for (Thread t : ths) t.start();
            for (Thread t : ths) t.join(5000);
            boolean allOk = true;
            for (String r : out) {
                if (r == null || !r.contains("\"ok\":true")) {
                    allOk = false;
                    System.out.println("  concurrent result: " + r);
                }
            }
            long ms = (System.nanoTime() - t0) / 1_000_000;
            check(allOk, "8 concurrent live dispatches all ok");
            check(ms < 5000, "concurrent live dispatches prompt (" + ms + "ms)");
        }

        // 7. Registration lifecycle: a resume that fails fast still clears.
        {
            SessionState st = freshState(tmp);
            // No VM: continue fails at requireLive? No — requireLive only
            // checks exited (false) — it proceeds to st.vm.resume() which
            // NPEs (unchecked). Outstanding must still clear via finally.
            try {
                BridgeSession.dispatch(st, "{\"cmd\":\"continue\",\"timeout\":\"1\"}");
            } catch (Exception ignored) {
            }
            synchronized (st.sessionLock) {
                check(st.outstanding == null, "outstanding cleared after failed resume");
            }
        }

        if (failures > 0) {
            System.out.println("M5JavaCheck: " + failures + " FAILURES");
            System.exit(1);
        }
        System.out.println("M5JavaCheck: all ok");
    }
}
