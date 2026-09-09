import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;

/** M2 serveLoop saturation: the pool-full overload read/teardown must run
 *  OUTSIDE sessionLock. A stalled peer (connects, never sends) occupies the
 *  overload path while the test asserts sessionLock stays acquirable and an
 *  exact `close` still terminates through the real serveLoop entry point
 *  (CJavaCheck only drives the serveOverload helper directly).
 *
 *  Pre-fix RED: serveLoop holds sessionLock across the 5s framing read, so
 *  the lock sample blocks ~4.5s (asserted < 2000ms). Post-fix GREEN: the
 *  lock is free immediately and close terminates under load.
 *
 *  Compile: javac -cp <bridge classes> -d <out> tests/SaturationJavaCheck.java
 *  Run:     java -cp <bridge classes>:<out> SaturationJavaCheck
 */
public class SaturationJavaCheck {
    static int failures = 0;

    static void check(boolean cond, String name) {
        if (cond) {
            System.out.println("ok: " + name);
        } else {
            System.out.println("FAIL: " + name);
            failures++;
        }
    }

    static void writeFrame(OutputStream out, String body) throws Exception {
        byte[] json = body.getBytes(StandardCharsets.UTF_8);
        String header = "Content-Length: " + json.length + "\r\n\r\n";
        out.write(header.getBytes(StandardCharsets.US_ASCII));
        out.write(json);
        out.flush();
    }

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

    public static void main(String[] args) throws Exception {
        Path tmp = Files.createTempDirectory("sat-java");
        try {
            SessionState st = new SessionState();
            st.cfg = new Config();
            st.cfg.sessionKind = "attach";
            st.cfg.timeoutMs = 20000;
            st.dir = tmp;
            st.server = new ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"));
            st.ownerNonce = "sat-test-nonce";
            Files.write(tmp.resolve("owner.json"),
                    "{\"pid\":1,\"nonce\":\"sat-test-nonce\"}".getBytes("UTF-8"));
            // Pool-full: every accept takes the overload path. Suspended
            // skips the JDI idle pump (no VM in this check).
            st.activeHandlers = 8;
            st.suspended = true;
            BridgeSession.cleanupCalls = 0;
            Thread loop = new Thread(() -> {
                try {
                    BridgeSession.serveLoop(st, tmp);
                } catch (Exception ignored) {}
            });
            loop.setDaemon(true);
            loop.start();
            int port = st.server.getLocalPort();
            // Stalled peer: connects, never sends. serveLoop must accept it
            // without holding sessionLock across the 5s framing read.
            Socket stalled = new Socket();
            stalled.connect(new InetSocketAddress("127.0.0.1", port), 5000);
            // Max of three samples: the lock is held continuously from
            // accept (100ms accept ticks) until the stall ends, so at least
            // one sample catches a held lock pre-fix; post-fix all are free.
            long worstMs = 0;
            for (int i = 0; i < 3; i++) {
                if (i > 0) Thread.sleep(300);
                long t0 = System.nanoTime();
                synchronized (st.sessionLock) {
                }
                long elapsedMs = (System.nanoTime() - t0) / 1_000_000;
                if (elapsedMs > worstMs) worstMs = elapsedMs;
            }
            check(worstMs < 2000,
                    "sessionLock free during stalled overload read (worst " + worstMs + "ms)");
            // Exact close still terminates under load through serveLoop. The
            // close frame waits in the kernel backlog while the loop is in
            // the stalled read; closing the stall lets the loop reach it.
            Socket closer = new Socket();
            closer.connect(new InetSocketAddress("127.0.0.1", port), 5000);
            closer.setSoTimeout(15000);
            writeFrame(closer.getOutputStream(), "{\"cmd\":\"close\"}");
            try {
                stalled.close();
            } catch (Exception ignored) {}
            String resp = readFrame(closer.getInputStream());
            check(resp.contains("\"closed\":true"),
                    "close under load ACKs (got: " + resp + ")");
            closer.close();
            loop.join(15000);
            check(!loop.isAlive(), "serveLoop returns after close");
            synchronized (st.sessionLock) {
                check(st.closing, "close marks closing");
                check(st.activeHandlers == 8, "overload path never counts");
            }
            check(BridgeSession.cleanupCalls == 1,
                    "teardown runs exactly once (got: " + BridgeSession.cleanupCalls + ")");
        } finally {
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
        System.out.println("all saturation checks passed");
    }
}
