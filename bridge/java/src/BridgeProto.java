import com.sun.jdi.request.ClassPrepareRequest;
import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

// Wire + text IO: TCP framing, minimal JSON parser, file appends, target-output drain. Moved verbatim from JdiBridge.java.
class BridgeProto {
    static void writeFile(Path p, String content) {
        try {
            Files.write(p, content.getBytes(StandardCharsets.UTF_8));
        } catch (Exception ignored) {}
    }

    /** Arm all configured breakpoints; deferred ones via ClassPrepareRequest. */

    static void appendFile(Path p, String line) {
        try {
            Files.write(p, (line + "\n").getBytes(StandardCharsets.UTF_8),
                    java.nio.file.StandardOpenOption.CREATE, java.nio.file.StandardOpenOption.APPEND);
        } catch (Exception ignored) {}
    }

    // ---- DAP-style framing over TCP ----

    static void writeFrame(OutputStream out, String body) throws Exception {
        byte[] json = body.getBytes(StandardCharsets.UTF_8);
        String header = "Content-Length: " + json.length + "\r\n\r\n";
        out.write(header.getBytes(StandardCharsets.US_ASCII));
        out.write(json);
        out.flush();
    }

    static String readFrame(InputStream in) throws Exception {
        long deadline = System.nanoTime() + 5_000_000_000L;
        ByteArrayOutputStream header = new ByteArrayOutputStream();
        int[] last = new int[]{-1, -1, -1, -1};
        int b;
        while ((b = in.read()) >= 0) {
            if (System.nanoTime() >= deadline) throw new BridgeException("frame read timed out");
            header.write(b);
            if (header.size() > 8192) throw new BridgeException("frame header too large");
            last[0] = last[1]; last[1] = last[2]; last[2] = last[3]; last[3] = b;
            if (last[0] == '\r' && last[1] == '\n' && last[2] == '\r' && last[3] == '\n') break;
        }
        if (b < 0) throw new BridgeException("truncated frame");
        int length = -1;
        for (String line : header.toString("US-ASCII").split("\r\n")) {
            int colon = line.indexOf(':');
            if (colon > 0 && line.substring(0, colon).trim().equalsIgnoreCase("Content-Length")) {
                try {
                    length = Integer.parseInt(line.substring(colon + 1).trim());
                } catch (NumberFormatException e) {
                    throw new BridgeException("bad Content-Length");
                }
            }
        }
        if (length < 0) throw new BridgeException("bad frame: no Content-Length");
        if (length > 1024 * 1024) throw new BridgeException("frame body too large");
        byte[] body = new byte[length];
        int off = 0;
        while (off < length) {
            if (System.nanoTime() >= deadline) throw new BridgeException("frame read timed out");
            int n = in.read(body, off, length - off);
            if (n < 0) throw new BridgeException("truncated frame");
            off += n;
        }
        return new String(body, StandardCharsets.UTF_8);
    }

    // ---- minimal JSON parser (flat string/number/bool values) ----

    static Map<String, String> parseJsonObject(String json) throws BridgeException {
        Map<String, String> map = new LinkedHashMap<>();
        int i = skipWs(json, 0);
        if (i >= json.length() || json.charAt(i) != '{') throw new BridgeException("bad request json");
        i++;
        while (true) {
            i = skipWs(json, i);
            if (i < json.length() && json.charAt(i) == '}') break;
            if (i >= json.length() || json.charAt(i) != '"') throw new BridgeException("bad request json");
            int[] end = new int[1];
            String key = parseJsonString(json, i, end);
            i = skipWs(json, end[0]);
            if (i >= json.length() || json.charAt(i) != ':') throw new BridgeException("bad request json");
            i = skipWs(json, i + 1);
            String val;
            if (i < json.length() && json.charAt(i) == '"') {
                val = parseJsonString(json, i, end);
                i = end[0];
            } else {
                int j = i;
                while (j < json.length() && ",}".indexOf(json.charAt(j)) < 0) j++;
                val = json.substring(i, j).trim();
                i = j;
            }
            map.put(key, val);
            i = skipWs(json, i);
            if (i < json.length() && json.charAt(i) == ',') { i++; continue; }
            if (i < json.length() && json.charAt(i) == '}') break;
            if (i >= json.length()) break;
            throw new BridgeException("bad request json");
        }
        return map;
    }

    static int skipWs(String s, int i) {
        while (i < s.length() && Character.isWhitespace(s.charAt(i))) i++;
        return i;
    }

    /**
     * Top-level "cmd" of a request object, without assuming flat values:
     * values are skipped depth-aware (arrays/objects survive), so a later
     * string value containing "\"cmd\"" can never shadow the real key.
     */
    static String parseCmd(String json) throws BridgeException {
        int i = skipWs(json, 0);
        if (i >= json.length() || json.charAt(i) != '{') throw new BridgeException("bad request json");
        i++;
        while (true) {
            i = skipWs(json, i);
            if (i < json.length() && json.charAt(i) == '}') break;
            if (i >= json.length() || json.charAt(i) != '"') throw new BridgeException("bad request json");
            int[] end = new int[1];
            String key = parseJsonString(json, i, end);
            i = skipWs(json, end[0]);
            if (i >= json.length() || json.charAt(i) != ':') throw new BridgeException("bad request json");
            i = skipWs(json, i + 1);
            if (key.equals("cmd")) {
                if (i >= json.length() || json.charAt(i) != '"') throw new BridgeException("request needs a cmd");
                return parseJsonString(json, i, end);
            }
            i = skipValue(json, i);
            i = skipWs(json, i);
            if (i < json.length() && json.charAt(i) == ',') { i++; continue; }
            if (i < json.length() && json.charAt(i) == '}') break;
            if (i >= json.length()) break;
            throw new BridgeException("bad request json");
        }
        throw new BridgeException("request needs a cmd");
    }

    /** Skip one JSON value starting at i (string/array/object/scalar). */
    static int skipValue(String json, int i) throws BridgeException {
        if (i >= json.length()) throw new BridgeException("bad request json");
        char c = json.charAt(i);
        if (c == '"') {
            int[] end = new int[1];
            parseJsonString(json, i, end);
            return end[0];
        }
        if (c == '[' || c == '{') {
            char open = c;
            char close = open == '[' ? ']' : '}';
            int depth = 0;
            while (i < json.length()) {
                char d = json.charAt(i);
                if (d == '"') {
                    int[] end = new int[1];
                    parseJsonString(json, i, end);
                    i = end[0];
                    continue;
                }
                if (d == open) depth++;
                else if (d == close) {
                    depth--;
                    if (depth == 0) return i + 1;
                }
                i++;
            }
            throw new BridgeException("bad request json");
        }
        int j = i;
        while (j < json.length() && ",}".indexOf(json.charAt(j)) < 0) j++;
        return j;
    }

    /** String array under a top-level key (the flat parser cannot hold arrays). */
    static List<String> parseStringArray(String json, String key) throws BridgeException {
        String quoted = "\"" + key + "\"";
        int k = json.indexOf(quoted);
        if (k < 0) throw new BridgeException("request needs \"" + key + "\"");
        int c = json.indexOf(':', k + quoted.length());
        if (c < 0) throw new BridgeException("request needs \"" + key + "\"");
        int i = skipWs(json, c + 1);
        if (i >= json.length() || json.charAt(i) != '[') throw new BridgeException("\"" + key + "\" must be an array");
        i = skipWs(json, i + 1);
        List<String> out = new ArrayList<>();
        if (i < json.length() && json.charAt(i) == ']') return out;
        while (true) {
            i = skipWs(json, i);
            if (i >= json.length() || json.charAt(i) != '"') throw new BridgeException("bad request json");
            int[] end = new int[1];
            out.add(parseJsonString(json, i, end));
            i = skipWs(json, end[0]);
            if (i < json.length() && json.charAt(i) == ',') { i++; continue; }
            if (i < json.length() && json.charAt(i) == ']') break;
            throw new BridgeException("bad request json");
        }
        return out;
    }

    static String parseJsonString(String s, int start, int[] end) throws BridgeException {
        StringBuilder sb = new StringBuilder();
        int i = start + 1;
        while (i < s.length()) {
            char c = s.charAt(i);
            if (c == '"') { end[0] = i + 1; return sb.toString(); }
            if (c == '\\') {
                i++;
                if (i >= s.length()) break;
                char e = s.charAt(i);
                switch (e) {
                    case '"': sb.append('"'); break;
                    case '\\': sb.append('\\'); break;
                    case 'n': sb.append('\n'); break;
                    case 'r': sb.append('\r'); break;
                    case 't': sb.append('\t'); break;
                    case 'u':
                        sb.append((char) Integer.parseInt(s.substring(i + 1, i + 5), 16));
                        i += 4;
                        break;
                    default: sb.append(e); break;
                }
            } else {
                sb.append(c);
            }
            i++;
        }
        throw new BridgeException("bad request json");
    }

    // ---- expression evaluation (paths + zero-arg calls, no compiler) ----

    static String parseJsonLite(String quoted) throws BridgeException {
        int[] end = new int[1];
        return parseJsonString(quoted, 0, end);
    }

    // ---- helpers ----
}
class StreamGobbler extends Thread {
        final InputStream in;
        final ByteArrayOutputStream buf = new ByteArrayOutputStream();

        StreamGobbler(InputStream in) {
            this.in = in;
            setDaemon(true);
        }

        public void run() {
            byte[] tmp = new byte[4096];
            try {
                int n;
                while ((n = in.read(tmp)) >= 0) {
                    synchronized (buf) {
                        buf.write(tmp, 0, n);
                        if (buf.size() > JdiBridge.MAX_OUTPUT * 2) {
                            byte[] all = buf.toByteArray();
                            buf.reset();
                            buf.write(all, all.length - JdiBridge.MAX_OUTPUT * 2, JdiBridge.MAX_OUTPUT * 2);
                        }
                    }
                }
            } catch (Exception ignored) {}
        }

        String tail() {
            synchronized (buf) {
                String s = buf.toString();
                if (s.length() > JdiBridge.MAX_OUTPUT) s = s.substring(s.length() - JdiBridge.MAX_OUTPUT);
                return s;
            }
        }
    }
