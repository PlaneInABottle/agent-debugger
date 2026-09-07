import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/** Milestone C unit parity for the Java bridge (no JUnit on this path —
 *  plain asserts, nonzero exit on failure; no live VM needed: every case
 *  below resolves before touching JDI).
 *
 *  Covers the pool-full bypass: with all eight handler slots occupied, an
 *  exact `close` still terminates outside the pool (closed ACK, teardown
 *  once, pool counter untouched) while ordinary commands get the existing
 *  overloaded rejection and malformed reads just drop. Close handling is
 *  idempotent: a second close still ACKs without re-running teardown.
 *
 *  Compile: javac -cp <bridge classes> -d <out> tests/CJavaCheck.java
 *  Run:     java -cp <bridge classes>:<out> CJavaCheck
 */
public class CJavaCheck {
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
        st.server = new ServerSocket(); // unbound dummy: close() is safe
        st.ownerNonce = "c-test-nonce";
        Files.write(tmp.resolve("owner.json"),
                "{\"pid\":1,\"nonce\":\"c-test-nonce\"}".getBytes("UTF-8"));
        return st;
    }

    static void writeFrame(OutputStream out, String body) throws Exception {
        byte[] json = body.getBytes(StandardCharsets.UTF_8);
        String header = "Content-Length: " + json.length + "\r\n\r\n";
        out.write(header.getBytes(StandardCharsets.US_ASCII));
        out.write(json);
        out.flush();
    }

    /** Read one framed reply (empty string on clean EOF / timeout). */
    static String readFrame(InputStream in) throws Exception {
        StringBuilder header = new StringBuilder();
        int[] last = new int[]{-1, -1, -1, -1};
        int b;
        while ((b = in.read()) >= 0) {
            header.append((char) b);
            if (header.length() > 8192) return "";
            last[0] = last[1]; last[1] = last[2]; last[2] = last[3]; last[3] = b;
            if (last[0] == '\r' && last[1] == '\n' && last[2] == '\r' && last[3] == '\n') break;
        }
        if (b < 0) return "";
        int length = -1;
        for (String line : header.toString().split("\r\n")) {
            int colon = line.indexOf(':');
            if (colon > 0 && line.substring(0, colon).trim().equalsIgnoreCase("Content-Length")) {
                try {
                    length = Integer.parseInt(line.substring(colon + 1).trim());
                } catch (NumberFormatException e) {
                    return "";
                }
            }
        }
        if (length < 0 || length > 1024 * 1024) return "";
        byte[] body = new byte[length];
        int off = 0;
        while (off < length) {
            int n = in.read(body, off, length - off);
            if (n < 0) return "";
            off += n;
        }
        return new String(body, StandardCharsets.UTF_8);
    }

    /** Connected pair: client socket returned, server side accepted. */
    static Socket[] pair(ServerSocket server) throws Exception {
        Socket client = new Socket();
        client.connect(new InetSocketAddress("127.0.0.1", server.getLocalPort()), 5000);
        client.setSoTimeout(5000);
        Socket accepted = server.accept();
        accepted.setSoTimeout(5000);
        return new Socket[]{client, accepted};
    }

    public static void main(String[] args) throws Exception {
        Path tmp = Files.createTempDirectory("c-java");
        try {
            ServerSocket listener = new ServerSocket(
                    0, 50, java.net.InetAddress.getByName("127.0.0.1"));
            try {
                // Pool-full ordinary command: existing overloaded rejection,
                // pool counter untouched, no close accepted.
                {
                    SessionState st = freshState(tmp);
                    st.activeHandlers = 8;
                    Socket[] p = pair(listener);
                    Thread t = new Thread(() -> BridgeSession.serveOverload(st, p[1]));
                    t.setDaemon(true);
                    t.start();
                    writeFrame(p[0].getOutputStream(), "{\"cmd\":\"threads\"}");
                    String resp = readFrame(p[0].getInputStream());
                    t.join(8000);
                    check(!t.isAlive(), "overload returns (no hang)");
                    check(resp.contains("\"ok\":false") && resp.contains("overloaded"),
                            "pool-full ordinary is overloaded (got: " + resp + ")");
                    check(resp.contains("\"target\":\"main\""), "overloaded names main");
                    synchronized (st.sessionLock) {
                        check(st.activeHandlers == 8, "pool counter untouched");
                        check(!st.closing, "ordinary never closes");
                    }
                    p[0].close();
                }
                // Pool-full exact close: terminal ACK outside the pool,
                // teardown exactly once, counter untouched.
                {
                    SessionState st = freshState(tmp);
                    st.activeHandlers = 8;
                    BridgeSession.cleanupCalls = 0;
                    Socket[] p = pair(listener);
                    Thread t = new Thread(() -> BridgeSession.serveOverload(st, p[1]));
                    t.setDaemon(true);
                    t.start();
                    writeFrame(p[0].getOutputStream(), "{\"cmd\":\"close\"}");
                    String resp = readFrame(p[0].getInputStream());
                    t.join(8000);
                    check(!t.isAlive(), "close returns (no hang)");
                    check(resp.contains("\"closed\":true") && resp.contains("\"ok\":true"),
                            "pool-full close ACKs (got: " + resp + ")");
                    synchronized (st.sessionLock) {
                        check(st.closing, "close marks closing");
                        check(st.activeHandlers == 8, "close outside pool never counts");
                    }
                    p[0].close();
                    // Second close: still ACKs, teardown not re-run.
                    Socket[] p2 = pair(listener);
                    BridgeSession.closeFromConn(st, p2[1]);
                    String resp2 = readFrame(p2[0].getInputStream());
                    check(resp2.contains("\"closed\":true"),
                            "second close still ACKs (got: " + resp2 + ")");
                    synchronized (st.sessionLock) {
                        check(st.closing, "closing stays set");
                    }
                    check(BridgeSession.cleanupCalls == 1,
                            "teardown runs exactly once (got: " + BridgeSession.cleanupCalls + ")");
                    p2[0].close();
                }
                // Pool-full malformed read: socket dropped, nothing served.
                {
                    SessionState st = freshState(tmp);
                    st.activeHandlers = 8;
                    Socket[] p = pair(listener);
                    Thread t = new Thread(() -> BridgeSession.serveOverload(st, p[1]));
                    t.setDaemon(true);
                    t.start();
                    OutputStream out = p[0].getOutputStream();
                    out.write("junk-bytes".getBytes(StandardCharsets.US_ASCII));
                    out.flush();
                    p[0].shutdownOutput();
                    t.join(8000);
                    check(!t.isAlive(), "malformed overload returns (no hang)");
                    String resp = readFrame(p[0].getInputStream());
                    check(resp.isEmpty(), "malformed gets no reply");
                    synchronized (st.sessionLock) {
                        check(!st.closing, "malformed never closes");
                        check(st.activeHandlers == 8, "malformed never counts");
                    }
                    p[0].close();
                }
            } finally {
                listener.close();
            }
        } finally {
            // best-effort cleanup of the temp dir
            try {
                Files.walk(tmp)
                        .sorted(java.util.Comparator.reverseOrder())
                        .forEach(p -> {
                            try { Files.deleteIfExists(p); } catch (Exception ignored) {}
                        });
            } catch (Exception ignored) {}
        }
        if (failures > 0) {
            System.out.println("FAILURES: " + failures);
            System.exit(1);
        }
        System.out.println("all C checks passed");
    }
}
