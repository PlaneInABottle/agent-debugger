import com.sun.jdi.AbsentInformationException;
import com.sun.jdi.ArrayReference;
import com.sun.jdi.BooleanValue;
import com.sun.jdi.Bootstrap;
import com.sun.jdi.CharValue;
import com.sun.jdi.Field;
import com.sun.jdi.IncompatibleThreadStateException;
import com.sun.jdi.LocalVariable;
import com.sun.jdi.Location;
import com.sun.jdi.ObjectReference;
import com.sun.jdi.PrimitiveValue;
import com.sun.jdi.ReferenceType;
import com.sun.jdi.StackFrame;
import com.sun.jdi.StringReference;
import com.sun.jdi.ThreadReference;
import com.sun.jdi.Value;
import com.sun.jdi.VirtualMachine;
import com.sun.jdi.VoidValue;
import com.sun.jdi.connect.AttachingConnector;
import com.sun.jdi.connect.Connector;
import com.sun.jdi.connect.LaunchingConnector;
import com.sun.jdi.event.BreakpointEvent;
import com.sun.jdi.event.ClassPrepareEvent;
import com.sun.jdi.event.Event;
import com.sun.jdi.event.EventSet;
import com.sun.jdi.event.VMDeathEvent;
import com.sun.jdi.event.VMDisconnectEvent;
import com.sun.jdi.request.BreakpointRequest;
import com.sun.jdi.request.ClassPrepareRequest;
import com.sun.jdi.request.EventRequest;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;


/**
 * JDI bridge entry point for agent-debugger (session mode: a per-session
 * daemon over TCP; see BridgeSession).
 *
 * <p>One public class plus one file per concern, same default package:
 * model (BridgeModel), CLI parsing (BridgeCli), attach/launch/one-shot
 * (BridgeConn), snapshot rendering (BridgeSnapshot), session server
 * (BridgeSession), wire IO (BridgeProto), expression layer (BridgeEval).
 * Zero dependencies, only {@code com.sun.jdi} from the JDK.
 *
 * <p>Usage:
 *
 * <pre>
 *   java JdiBridge attach --host localhost --port 5005 --break com.example.Hello:30 --src ./src
 *   java JdiBridge launch --main com.example.Hello --cp ./classes --src ./src --break com.example.Hello:30
 *   # conditional: stop only when count is 5 (bridge-side, zero extra roundtrips)
 *   java JdiBridge launch --main com.example.Hello --cp ./classes --break 'com.example.Hello:30|count == 5'
 *   # logpoint: collect without stopping, read back with {"cmd":"logs"}
 *   java JdiBridge launch --main com.example.Hello --cp ./classes --break com.example.Hello:30 \
 *        --logpoint 'com.example.Hello:25:{it.name}={it.price}'
 * </pre>
 */
public class JdiBridge {
    static final int MAX_STRING = 200;
    static final int MAX_ITEMS = 3;
    static final int MAX_FIELDS = 20;
    static final int MAX_VARS = 20;
    static final int MAX_FRAMES = 10;
    static final int MAX_OUTPUT = 4000;

    public static void main(String[] argv) {
        try {
            BridgeCli.run(argv);
        } catch (UsageException e) {
            System.out.println("{\"error\":" + quote(e.getMessage()) + "}");
            System.exit(2);
        } catch (BridgeException e) {
            System.out.println("{\"error\":" + quote(e.getMessage()) + "}");
            System.exit(1);
        } catch (Exception e) {
            System.out.println("{\"error\":" + quote("internal: " + e) + "}");
            System.exit(1);
        }
    }

    static String shortMsg(Throwable t) {
        String m = t.getMessage();
        if (m == null || m.isEmpty()) return t.getClass().getSimpleName();
        if (m.length() > 160) m = m.substring(0, 160) + "…";
        return t.getClass().getSimpleName() + ": " + m;
    }

    /** Sanitized unexpected-crash text: class + message + a few stack
     *  frames, capped ~2KB. No env, no heap — only the failure itself.
     *  Known Usage/BridgeException messages bypass this (exact text). */
    static String sanitizeUnexpected(Throwable t) {
        String head = t.getClass().getSimpleName()
                + (t.getMessage() == null ? "" : ": " + t.getMessage());
        StringBuilder sb = new StringBuilder("internal: ").append(head);
        try {
            StackTraceElement[] frames = t.getStackTrace();
            for (int i = 0; i < Math.min(6, frames.length); i++) {
                sb.append("\n    at ").append(frames[i].toString());
                if (sb.length() > 2048) break;
            }
        } catch (Exception ignored) {}
        String s = sb.toString();
        if (s.length() > 2048) s = s.substring(0, 2047) + "…";
        return s;
    }

    static void kv(StringBuilder sb, String key, String val, boolean comma) {
        sb.append(quote(key)).append(':').append(quote(val));
    }

    static String quote(String s) {
        if (s == null) return "null";
        StringBuilder sb = new StringBuilder(s.length() + 2).append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"': sb.append("\\\""); break;
                case '\\': sb.append("\\\\"); break;
                case '\n': sb.append("\\n"); break;
                case '\r': sb.append("\\r"); break;
                case '\t': sb.append("\\t"); break;
                default:
                    if (c < 0x20) sb.append(String.format("\\u%04x", (int) c));
                    else sb.append(c);
            }
        }
        return sb.append('"').toString();
    }
}
