import java.lang.reflect.Proxy;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** M5 unit parity for the Java bridge (no JUnit on this path — plain
 *  asserts, nonzero exit on failure; no live VM needed: every case below
 *  resolves before touching JDI). Covers: live reads stay prompt with no
 *  JDI while a resume is outstanding; second resume / mutation / eval on
 *  the busy target reject immediately with the frozen busy shape; frame
 *  reads fail fast once the resume publishes running (never stale); close
 *  is accepted despite an outstanding resume; the handler bound is small
 *  and fixed; concurrent live dispatches from threads all succeed; the
 *  pump-serialization model (idle tryLock skips a held queue/outstanding);
 *  the cached thread roster while outstanding; unknown step modes failing
 *  fast; condition/logpoint evaluation completing without sessionLock
 *  (delayed-JDI worker under a held lock); the post-timeout parked recheck.
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

    /** Proxy a JDI interface by method name (unlisted methods return null). */
    @SuppressWarnings("unchecked")
    static <T> T fake(Class<T> iface, Map<String, Object> answers) {
        java.lang.reflect.InvocationHandler h = (proxy, m, args) -> {
            if (m.getName().equals("toString")) return "fake";
            if (m.getName().equals("hashCode")) return 0;
            if (m.getName().equals("equals")) return proxy == args[0];
            return answers.get(m.getName());
        };
        return (T) Proxy.newProxyInstance(M5JavaCheck.class.getClassLoader(),
                new Class<?>[]{iface}, h);
    }

    static com.sun.jdi.Location loc(String cls, int line, String method) {
        com.sun.jdi.ReferenceType rt = fake(com.sun.jdi.ReferenceType.class, Map.of("name", cls));
        com.sun.jdi.Method m = fake(com.sun.jdi.Method.class, Map.of("name", method));
        Map<String, Object> a = new LinkedHashMap<>();
        a.put("declaringType", rt);
        a.put("lineNumber", line);
        a.put("method", m);
        return fake(com.sun.jdi.Location.class, a);
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

        // 8. Pump serialization model: at most one EventQueue.remove
        // consumer. The idle pump skips while a dispatch pump holds the
        // queue (vm is null here — any JDI touch would NPE, so a null return
        // proves no touch) and while a resume is outstanding.
        {
            SessionState st = freshState(tmp);
            // A rival pump thread owns the queue: the idle pump skips without
            // touching JDI (vm is null here — any JDI touch would NPE, so a
            // null return proves no touch). A separate holder thread matters:
            // ReentrantLock is reentrant on the same thread.
            java.util.concurrent.CountDownLatch held =
                    new java.util.concurrent.CountDownLatch(1);
            java.util.concurrent.CountDownLatch release =
                    new java.util.concurrent.CountDownLatch(1);
            Thread holder = new Thread(() -> {
                st.pumpLock.lock();
                held.countDown();
                try {
                    release.await();
                } catch (InterruptedException ignored) {
                } finally {
                    st.pumpLock.unlock();
                }
            });
            holder.setDaemon(true);
            holder.start();
            held.await();
            try {
                long t0 = System.nanoTime();
                String r = BridgeSession.awaitStopIdle(st, 5);
                long ms = (System.nanoTime() - t0) / 1_000_000;
                check(r == null, "idle pump skips while a dispatch pump holds the queue");
                check(ms < 2000, "idle skip is prompt (" + ms + "ms)");
            } finally {
                release.countDown();
                holder.join(5000);
            }
            synchronized (st.sessionLock) {
                st.outstanding = "continue";
            }
            check(BridgeSession.awaitStopIdle(st, 5) == null,
                    "idle pump skips while a resume is outstanding");
            synchronized (st.sessionLock) {
                st.outstanding = null;
            }
        }

        // 9. Threads serves the cached roster while a resume owns the pump
        // (no JDI while outstanding); empty only when nothing cached yet.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.outstanding = "continue";
                st.cachedThreads = "[{\"id\":7}]";
            }
            String r = BridgeSession.dispatch(st, "{\"cmd\":\"threads\"}");
            check(r.contains("\"running\":true") && r.contains("\"id\":7"),
                    "threads serves the cached roster while a resume owns the pump: " + r);
            SessionState st2 = freshState(tmp);
            synchronized (st2.sessionLock) {
                st2.outstanding = "wait";
            }
            String r2 = BridgeSession.dispatch(st2, "{\"cmd\":\"threads\"}");
            check(r2.contains("\"threads\":[]"),
                    "no cache yet serves empty (never stale-shaped): " + r2);
        }

        // 10. Unknown step modes fail fast (before any JDI), and the
        // outstanding slot still clears.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.suspended = true;
                st.thread = fake(com.sun.jdi.ThreadReference.class, new LinkedHashMap<>());
            }
            expectBridgeError("unknown step mode fails fast",
                    () -> BridgeSession.dispatch(st, "{\"cmd\":\"step\",\"mode\":\"sideways\"}"),
                    "unknown step mode");
            synchronized (st.sessionLock) {
                check(st.outstanding == null, "failed step clears the outstanding slot");
            }
        }

        // 11. Condition/logpoint evaluation never takes sessionLock: a worker
        // running both against slow JDI completes while the main thread holds
        // the lock (a 10s invokeMethod join under the lock would stall this
        // past the join budget and fail).
        {
            SessionState st = freshState(tmp);
            Config cfg = new Config();
            Logpoint lp = new Logpoint();
            lp.cls = "com.Foo";
            lp.line = 54;
            lp.template = "v={x}";
            cfg.logpoints.add(lp);
            com.sun.jdi.Location location = loc("com.Foo", 54, "m");
            Map<String, Object> frameAnswers = new LinkedHashMap<>();
            frameAnswers.put("visibleVariableByName", null);
            frameAnswers.put("thisObject", null);
            frameAnswers.put("location", location);
            com.sun.jdi.StackFrame frame = fake(com.sun.jdi.StackFrame.class, frameAnswers);
            List<com.sun.jdi.StackFrame> frames = new ArrayList<>();
            frames.add(frame);
            com.sun.jdi.ThreadReference slowThread = (com.sun.jdi.ThreadReference) Proxy.newProxyInstance(
                    M5JavaCheck.class.getClassLoader(),
                    new Class<?>[]{com.sun.jdi.ThreadReference.class},
                    (proxy, m, args) -> {
                        if (m.getName().equals("toString")) return "slow-fake";
                        if (m.getName().equals("hashCode")) return 0;
                        if (m.getName().equals("equals")) return proxy == args[0];
                        if (m.getName().equals("frames")) {
                            try { Thread.sleep(300); } catch (InterruptedException ignored) {}
                            return frames;
                        }
                        return null;
                    });
            final List<String> linesOut = new ArrayList<>();
            final boolean[] condOut = new boolean[]{true};
            final Throwable[] err = new Throwable[1];
            Thread worker = new Thread(() -> {
                try {
                    List<String> lines =
                            BridgeSession.renderLogLines(cfg, slowThread, location);
                    if (lines != null) linesOut.addAll(lines);
                    condOut[0] = BridgeEval.checkCond(slowThread, location, "x == 1");
                } catch (Throwable t) {
                    err[0] = t;
                }
            });
            worker.setDaemon(true);
            synchronized (st.sessionLock) {
                long t0 = System.nanoTime();
                worker.start();
                try {
                    worker.join(5000);
                } catch (InterruptedException ie) {
                    Thread.currentThread().interrupt();
                }
                long ms = (System.nanoTime() - t0) / 1_000_000;
                check(!worker.isAlive(),
                        "evaluation completes while sessionLock is held (" + ms + "ms)");
                check(ms < 5000, "delayed evaluator stays bounded (" + ms + "ms)");
            }
            check(err[0] == null, "evaluation worker raised nothing (" + err[0] + ")");
            check(linesOut.size() == 1 && linesOut.get(0).startsWith("[logpoint error:"),
                    "slow logpoint render degrades to an error line: " + linesOut);
            check(!condOut[0], "unknown name in condition counts as false");
        }

        // 12. Post-timeout parked recheck: a parked stop exposes its snapshot
        // instead of a spurious timeout; a running session rechecks to null.
        {
            SessionState st = freshState(tmp);
            st.cfg.mode = "attach";
            com.sun.jdi.ReferenceType rt =
                    fake(com.sun.jdi.ReferenceType.class, Map.of("name", "com.Foo"));
            Map<String, Object> vmAnswers = new LinkedHashMap<>();
            vmAnswers.put("allThreads", new ArrayList<>());
            com.sun.jdi.VirtualMachine vm =
                    fake(com.sun.jdi.VirtualMachine.class, vmAnswers);
            Map<String, Object> threadAnswers = new LinkedHashMap<>();
            threadAnswers.put("uniqueID", 11L);
            threadAnswers.put("name", "main");
            threadAnswers.put("status", 1);
            threadAnswers.put("isSuspended", Boolean.FALSE);
            threadAnswers.put("frames", new ArrayList<>());
            com.sun.jdi.ThreadReference thread =
                    fake(com.sun.jdi.ThreadReference.class, threadAnswers);
            st.vm = vm;
            st.thread = thread;
            st.location = loc("com.Foo", 54, "bill");
            synchronized (st.sessionLock) {
                st.suspended = true;
            }
            String parked = BridgeSession.parkedRecheck(st);
            check(parked != null && parked.contains("com.Foo") && parked.contains("bill"),
                    "parked recheck exposes the stop snapshot: " + parked);
            synchronized (st.sessionLock) {
                st.suspended = false;
            }
            check(BridgeSession.parkedRecheck(st) == null,
                    "running session rechecks to null (timeout stands)");
        }

        if (failures > 0) {
            System.out.println("M5JavaCheck: " + failures + " FAILURES");
            System.exit(1);
        }
        System.out.println("M5JavaCheck: all ok");
    }
}
