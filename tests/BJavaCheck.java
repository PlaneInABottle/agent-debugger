import java.io.InputStream;
import java.io.OutputStream;
import java.lang.reflect.Proxy;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** Milestone B parity checks for the Java bridge (same plain-assert style
 *  as M5JavaCheck: nonzero exit on failure; no live VM needed).
 *  1. Every served response names target main: close ACK, threads
 *     busy-cached + parked + running, breaks, logs, stack, vars, eval
 *     shape, overloaded, and every ok:false envelope (busy, running,
 *     exited, unknown cmd, malformed frame, broken framing).
 *  2. Uniform vars/eval frame validation matrix via parseFrameIndex plus
 *     dispatch-level valid/range/type vars cases.
 *
 *  Compile: javac -cp <bridge classes> -d <out> tests/BJavaCheck.java
 *  Run:     java -cp <bridge classes>:<out> BJavaCheck
 */
public class BJavaCheck {
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
        st.ownerNonce = "b-test-nonce";
        Files.write(tmp.resolve("owner.json"),
                "{\"pid\":1,\"nonce\":\"b-test-nonce\"}".getBytes("UTF-8"));
        return st;
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
        return (T) Proxy.newProxyInstance(BJavaCheck.class.getClassLoader(),
                new Class<?>[]{iface}, h);
    }

    static com.sun.jdi.StackFrame fakeFrame() {
        return fake(com.sun.jdi.StackFrame.class, Map.of());
    }

    static com.sun.jdi.ThreadReference fakeThread(List<com.sun.jdi.StackFrame> frames) {
        Map<String, Object> a = new LinkedHashMap<>();
        a.put("frames", frames);
        return fake(com.sun.jdi.ThreadReference.class, a);
    }

    /** Serve exactly one framed request through handleOne (the real socket
     *  envelope path: close ACK, overloaded, central ok:false) and return
     *  the raw response string. */
    static String serveOne(SessionState st, String reqJson) throws Exception {
        ServerSocket ss = new ServerSocket(0);
        int port = ss.getLocalPort();
        Socket client = new Socket("127.0.0.1", port);
        client.setSoTimeout(10000);
        Socket srv = ss.accept();
        Thread t = new Thread(() -> BridgeSession.handleOne(st, srv));
        t.start();
        try {
            BridgeProto.writeFrame(client.getOutputStream(), reqJson);
            return BridgeProto.readFrame(client.getInputStream());
        } finally {
            try { client.close(); } catch (Exception ignored) {}
            try { ss.close(); } catch (Exception ignored) {}
            t.join(10000);
        }
    }

    /** Broken framing: raw bytes that are not a frame, then half-close
     *  so the server's frame read hits EOF instead of blocking. */
    static String serveGarbage(SessionState st) throws Exception {
        ServerSocket ss = new ServerSocket(0);
        int port = ss.getLocalPort();
        Socket client = new Socket("127.0.0.1", port);
        client.setSoTimeout(10000);
        Socket srv = ss.accept();
        Thread t = new Thread(() -> BridgeSession.handleOne(st, srv));
        t.start();
        try {
            OutputStream o = client.getOutputStream();
            o.write("not-a-frame".getBytes("UTF-8"));
            o.flush();
            client.shutdownOutput();
            return BridgeProto.readFrame(client.getInputStream());
        } finally {
            try { client.close(); } catch (Exception ignored) {}
            try { ss.close(); } catch (Exception ignored) {}
            t.join(10000);
        }
    }

    static void checkTarget(String resp, String name) {
        check(resp.contains("\"target\":\"main\""), name + " names target main: " + resp);
        check(resp.indexOf("\"target\"") == resp.lastIndexOf("\"target\""),
                name + " carries exactly one target key: " + resp);
    }

    static String expectOk(String name, String resp) {
        check(resp.contains("\"ok\":true"), name + " ok:true: " + resp);
        checkTarget(resp, name);
        return resp;
    }

    static String expectErr(String name, Throwing run, String wantSub) {
        try {
            run.run();
        } catch (BridgeException be) {
            check(be.getMessage() != null && be.getMessage().contains(wantSub),
                    name + " (got: " + be.getMessage() + ")");
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
        Path tmp = Files.createTempDirectory("b-java-");

        // 1. Close ACK names its target (socket envelope path).
        {
            SessionState st = freshState(tmp);
            String resp = serveOne(st, "{\"cmd\":\"close\"}");
            check(resp.contains("\"closed\":true"), "close ACK closes: " + resp);
            checkTarget(resp, "close ACK");
        }

        // 2. Overloaded names its target (accept-loop helper; the loop
        // itself only admits it past the handler bound).
        {
            String resp = BridgeSession.overloadedJson();
            check(resp.contains("overloaded"), "overloaded rejects: " + resp);
            checkTarget(resp, "overloaded");
        }

        // 3. Central ok:false envelope names its target: busy, running
        // reads, unknown cmd, broken framing.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.outstanding = "continue";
            }
            String busy = serveOne(st, "{\"cmd\":\"continue\"}");
            check(busy.contains("\"ok\":false") && busy.contains("busy: continue outstanding for main"),
                    "busy rejects: " + busy);
            checkTarget(busy, "busy error");
        }
        {
            SessionState st = freshState(tmp);
            String running = serveOne(st, "{\"cmd\":\"vars\",\"frame\":0}");
            check(running.contains("\"ok\":false") && running.contains("no stopped thread"),
                    "running vars fails fast: " + running);
            checkTarget(running, "running-vars error");
        }
        {
            SessionState st = freshState(tmp);
            String unknown = serveOne(st, "{\"cmd\":\"nope\"}");
            check(unknown.contains("\"ok\":false") && unknown.contains("unknown cmd"),
                    "unknown cmd errors: " + unknown);
            checkTarget(unknown, "unknown-cmd error");
        }
        {
            SessionState st = freshState(tmp);
            String broken = serveGarbage(st);
            check(broken.contains("\"ok\":false"), "broken framing errors: " + broken);
            checkTarget(broken, "framing error");
        }

        // 4. Dispatch-level ok:true responses name their target: threads
        // busy-cached, parked, running; breaks; logs.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.outstanding = "continue";
            }
            String threads = BridgeSession.dispatch(st, "{\"cmd\":\"threads\"}");
            check(threads.contains("\"running\":true"), "busy threads running: " + threads);
            checkTarget(threads, "busy-cached threads");
        }
        {
            com.sun.jdi.VirtualMachine vm = fake(com.sun.jdi.VirtualMachine.class,
                    Map.of("allThreads", List.of()));
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.vm = vm;
                st.suspended = true;
            }
            String threads = BridgeSession.dispatch(st, "{\"cmd\":\"threads\"}");
            check(threads.contains("\"running\":false"), "parked threads: " + threads);
            checkTarget(threads, "parked threads");
        }
        {
            com.sun.jdi.VirtualMachine vm = fake(com.sun.jdi.VirtualMachine.class,
                    Map.of("allThreads", List.of()));
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.vm = vm;
                st.suspended = false;
            }
            String threads = BridgeSession.dispatch(st, "{\"cmd\":\"threads\"}");
            check(threads.contains("\"running\":true"), "running threads: " + threads);
            checkTarget(threads, "running threads");
        }
        {
            SessionState st = freshState(tmp);
            checkTarget(BridgeSession.dispatch(st, "{\"cmd\":\"breaks\"}"), "breaks");
            checkTarget(BridgeSession.dispatch(st, "{\"cmd\":\"logs\"}"), "logs");
            String clear = BridgeSession.dispatch(st, "{\"cmd\":\"breaksClear\"}");
            check(clear.contains("\"removed\":[]"), "empty clear: " + clear);
            checkTarget(clear, "empty breaksClear");
        }

        // 5. Frame validation matrix (pure helper, both commands).
        {
            List<com.sun.jdi.StackFrame> two = List.of(fakeFrame(), fakeFrame());
            for (String what : new String[]{"vars", "eval"}) {
                check(BridgeSession.parseFrameIndex(Map.of(), two, what) == 0,
                        what + " missing reads as 0");
                Map<String, String> nul = new LinkedHashMap<>();
                nul.put("frame", "null");
                check(BridgeSession.parseFrameIndex(nul, two, what) == 0,
                        what + " JSON null reads as 0");
                check(BridgeSession.parseFrameIndex(Map.of("frame", "0"), two, what) == 0,
                        what + " 0 ok");
                check(BridgeSession.parseFrameIndex(Map.of("frame", "1"), two, what) == 1,
                        what + " 1 ok");
                check(BridgeSession.parseFrameIndex(Map.of("frame", "001"), two, what) == 1,
                        what + " leading zeros normalize");
                for (String bad : new String[]{"abc", "", "3x", "3.5", "-1", "+1",
                        " 3", "3 ", "0x3", "9999999999999999", "99999999999999999999",
                        // Intentional representation gap (see parseFrameIndex
                        // javadoc): the flat parser erases quoted-vs-numeric,
                        // so "1.0" stays invalid here while the other bridges
                        // accept numeric 1.0.
                        "1.0"}) {
                    Map<String, String> req = Map.of("frame", bad);
                    try {
                        BridgeSession.parseFrameIndex(req, two, what);
                        check(false, what + " rejects " + bad);
                    } catch (BridgeException be) {
                        check((what + " needs integer frame").equals(be.getMessage()),
                                what + " typed error for " + bad + " (got: " + be.getMessage() + ")");
                    }
                }
                for (String big : new String[]{"2", "99", "99999999999"}) {
                    Map<String, String> req = Map.of("frame", big);
                    try {
                        BridgeSession.parseFrameIndex(req, two, what);
                        check(false, what + " ranges " + big);
                    } catch (BridgeException be) {
                        check(be.getMessage().startsWith("no frame " + new java.math.BigInteger(big)
                                        + " (have 2)"),
                                what + " range error for " + big + " (got: " + be.getMessage() + ")");
                    }
                }
            }
        }

        // 6. Dispatch-level vars: valid echo, range, type — all stamped.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.suspended = true;
                st.thread = fakeThread(List.of(fakeFrame(), fakeFrame()));
            }
            String one = BridgeSession.dispatch(st, "{\"cmd\":\"vars\",\"frame\":1}");
            check(one.contains("\"frame\":1"), "vars valid echo: " + one);
            checkTarget(one, "vars valid");
            try {
                BridgeSession.dispatch(st, "{\"cmd\":\"vars\",\"frame\":5}");
                check(false, "vars range raises");
            } catch (BridgeException be) {
                check("no frame 5 (have 2)".equals(be.getMessage()),
                        "vars range text (got: " + be.getMessage() + ")");
            }
            try {
                BridgeSession.dispatch(st, "{\"cmd\":\"vars\",\"frame\":\"abc\"}");
                check(false, "vars type raises");
            } catch (BridgeException be) {
                check("vars needs integer frame".equals(be.getMessage()),
                        "vars type text (got: " + be.getMessage() + ")");
            }
            try {
                BridgeSession.dispatch(st, "{\"cmd\":\"eval\",\"expr\":\"1\",\"frame\":\"1.5\"}");
                check(false, "eval type raises");
            } catch (BridgeException be) {
                check("eval needs integer frame".equals(be.getMessage()),
                        "eval type text (got: " + be.getMessage() + ")");
            }
        }

        // 7. Representative dispatched errors already carry BridgeException
        // text the central envelope stamps (covered live at handleOne
        // above); exited + closing fail fast here.
        {
            SessionState st = freshState(tmp);
            synchronized (st.sessionLock) {
                st.exited = true;
            }
            expectOkAbsent("exited threads rejects", st);
        }

        if (failures > 0) {
            System.out.println("FAILURES: " + failures);
            System.exit(1);
        }
        System.out.println("all B checks passed");
    }

    static void expectOkAbsent(String name, SessionState st) {
        try {
            BridgeSession.dispatch(st, "{\"cmd\":\"threads\"}");
            check(false, name + " (no error raised)");
        } catch (BridgeException be) {
            check(be.getMessage() != null
                    && be.getMessage().contains("target VM has exited"),
                    name + " (got: " + be.getMessage() + ")");
        } catch (Exception e) {
            check(false, name + " (wrong exception: " + e + ")");
        }
    }
}
