import com.sun.jdi.AbsentInformationException;
import com.sun.jdi.ArrayReference;
import com.sun.jdi.BooleanValue;
import com.sun.jdi.CharValue;
import com.sun.jdi.Field;
import com.sun.jdi.LocalVariable;
import com.sun.jdi.Location;
import com.sun.jdi.ObjectReference;
import com.sun.jdi.PrimitiveValue;
import com.sun.jdi.StackFrame;
import com.sun.jdi.StringReference;
import com.sun.jdi.ThreadReference;
import com.sun.jdi.Value;
import com.sun.jdi.VirtualMachine;
import com.sun.jdi.VoidValue;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.Map;

// Snapshot rendering: threads, frames, locals, values, snippets. Moved verbatim from JdiBridge.java.
class BridgeSnapshot {
    static String snapshot(VirtualMachine vm, Config cfg, ThreadReference thread,
            Location loc, StreamGobbler out, StreamGobbler err) throws Exception {
        return snapshotBounded(vm, cfg, thread, loc, out, err,
                JdiBridge.MAX_FRAMES, JdiBridge.MAX_VARS);
    }

    /** Bounded snapshot for capture (frames 1..10, frame-0 vars 1..20). */
    static String snapshotBounded(VirtualMachine vm, Config cfg, ThreadReference thread,
            Location loc, StreamGobbler out, StreamGobbler err,
            int maxFrames, int maxVars) throws Exception {
        StringBuilder sb = new StringBuilder(4096);
        sb.append('{');
        JdiBridge.kv(sb, "mode", cfg.mode, true);
        sb.append(",\"location\":").append(locationJson(loc, cfg));
        sb.append(",\"threads\":").append(threadsJson(vm, thread));
        sb.append(",\"frames\":").append(framesJsonBounded(thread, true, maxFrames, maxVars));
        if (out != null || err != null) sb.append(",\"output\":").append(JdiBridge.quote(combinedOutput(out, err)));
        sb.append('}');
        return sb.toString();
    }

    /** Bounded combined target output (stdout + stderr); stderr never duplicates stdout. */
    static String combinedOutput(StreamGobbler out, StreamGobbler err) {
        String o = "";
        String e = "";
        try { if (out != null) o = out.tail(); } catch (Exception ignored) {}
        try { if (err != null) e = err.tail(); } catch (Exception ignored) {}
        if (o == null) o = "";
        if (e == null) e = "";
        if (e.isEmpty()) {
            if (o.length() > JdiBridge.MAX_OUTPUT) o = o.substring(o.length() - JdiBridge.MAX_OUTPUT);
            return o;
        }
        String combined = o.isEmpty() ? "[stderr]\n" + e : o + "\n[stderr]\n" + e;
        if (combined.length() > JdiBridge.MAX_OUTPUT) combined = combined.substring(combined.length() - JdiBridge.MAX_OUTPUT);
        return combined;
    }

    /** Bounded stderr-inclusive suffix for setup-failure errors (bad main/cp, early exit). */
    static String targetOutputSuffix(StreamGobbler out, StreamGobbler err) {
        String combined;
        try {
            combined = combinedOutput(out, err);
        } catch (Exception ignored) {
            return "";
        }
        if (combined == null || combined.trim().isEmpty()) return "";
        String tail = combined.trim();
        if (tail.length() > 500) tail = tail.substring(tail.length() - 500);
        return " | target output: " + tail;
    }

    static String locationJson(Location loc, Config cfg) {
        String cls = "?";
        String method = "?";
        int line = -1;
        try { cls = loc.declaringType().name(); } catch (Exception ignored) {}
        try { method = loc.method().name(); } catch (Exception ignored) {}
        try { line = loc.lineNumber(); } catch (Exception ignored) {}
        String file = sourcePath(cls);
        return "{\"class\":" + JdiBridge.quote(cls) + ",\"method\":" + JdiBridge.quote(method)
                + ",\"line\":" + line + ",\"file\":" + JdiBridge.quote(file)
                + ",\"snippet\":" + snippet(cfg, file, line) + "}";
    }

    static String threadsJson(VirtualMachine vm, ThreadReference current) throws Exception {
        // Token economics: a Spring/Tomcat VM has 100+ threads. The agent needs
        // the current one; the rest is a capped roster with a +N more note.
        StringBuilder sb = new StringBuilder("[");
        List<ThreadReference> all = vm.allThreads();
        List<ThreadReference> ordered = new ArrayList<>(all.size());
        if (current != null) ordered.add(current);
        for (ThreadReference t : all) {
            if (current == null || !t.equals(current)) ordered.add(t);
        }
        int cap = 8;
        for (int i = 0; i < Math.min(ordered.size(), cap); i++) {
            ThreadReference t = ordered.get(i);
            if (i > 0) sb.append(',');
            sb.append("{\"id\":").append(t.uniqueID()).append(',');
            JdiBridge.kv(sb, "name", t.name(), true);
            sb.append(",\"status\":").append(JdiBridge.quote(threadStatus(t.status()))).append(',');
            sb.append("\"current\":").append(current != null && t.equals(current)).append(',');
            try {
                sb.append("\"suspended\":").append(t.isSuspended());
            } catch (Exception e) {
                sb.append("\"suspended\":false");
            }
            sb.append('}');
        }
        if (ordered.size() > cap) {
            sb.append(",{\"id\":-1,\"name\":\"…\",\"note\":"
                    + JdiBridge.quote("+" + (ordered.size() - cap) + " more threads") + "}");
        }
        return sb.append(']').toString();
    }

    static List<StackFrame> safeFrames(ThreadReference thread) {
        try {
            return thread.frames();
        } catch (Exception e) {
            return new ArrayList<>();
        }
    }

    /**
     * Frames with locals on the top frame only. Lower frames re-dump the same
     * heap objects on every stop (measured ~45% of a snapshot); details stay
     * one `vars --frame N` away. {@code withLocals=false} is headers-only.
     */

    static String framesJson(ThreadReference thread, boolean withLocals) {
        return framesJsonBounded(thread, withLocals, JdiBridge.MAX_FRAMES, JdiBridge.MAX_VARS);
    }

    /** Bounded frame listing for capture (frames 1..10, frame-0 vars 1..20).
     *  Depth and string caps reuse the existing formatValue rules. */
    static String framesJsonBounded(ThreadReference thread, boolean withLocals,
            int maxFrames, int maxVars) {
        List<StackFrame> frames = safeFrames(thread);
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < Math.min(frames.size(), maxFrames); i++) {
            if (i > 0) sb.append(',');
            StackFrame f = frames.get(i);
            sb.append("{\"index\":").append(i).append(',');
            String ftype = "?";
            String mname = "?";
            int fline = -1;
            try { ftype = f.location().declaringType().name(); } catch (Exception ignored) {}
            try { mname = f.location().method().name(); } catch (Exception ignored) {}
            try { fline = f.location().lineNumber(); } catch (Exception ignored) {}
            JdiBridge.kv(sb, "type", ftype, true);
            sb.append(',');
            JdiBridge.kv(sb, "method", mname, true);
            sb.append(",\"line\":").append(fline);
            if (withLocals && i == 0) sb.append(",\"locals\":").append(localsJsonBounded(f, maxVars));
            sb.append('}');
        }
        return sb.append(']').toString();
    }

    static String localsJson(StackFrame f) {
        return localsJsonBounded(f, JdiBridge.MAX_VARS);
    }

    /** Bounded locals listing for capture (vars 1..20). */
    static String localsJsonBounded(StackFrame f, int maxVars) {
        StringBuilder sb = new StringBuilder("[");
        try {
            List<LocalVariable> vars = f.visibleVariables();
            Map<LocalVariable, Value> vals = f.getValues(vars);
            // Deterministic order: agents diff snapshots, HashMap order would lie.
            List<Map.Entry<LocalVariable, Value>> entries = new ArrayList<>(vals.entrySet());
            entries.sort(Comparator.comparing(e -> e.getKey().name()));
            int n = 0;
            for (Map.Entry<LocalVariable, Value> ve : entries) {
                if (n >= maxVars) {
                    if (n == maxVars) sb.append(",{\"name\":\"…\",\"note\":"
                            + JdiBridge.quote("+" + (vals.size() - maxVars) + " more") + "}");
                    n++;
                    continue;
                }
                if (n > 0) sb.append(',');
                sb.append("{\"name\":").append(JdiBridge.quote(ve.getKey().name())).append(',');
                sb.append("\"type\":").append(JdiBridge.quote(ve.getKey().typeName())).append(',');
                sb.append("\"value\":").append(JdiBridge.quote(formatValue(ve.getValue(), 1)));
                sb.append('}');
                n++;
            }
        } catch (AbsentInformationException aie) {
            sb.append("{\"name\":\"…\",\"note\":\"no debug info (-g)\"}");
        } catch (Exception e) {
            sb.append("{\"name\":\"…\",\"note\":").append(JdiBridge.quote(JdiBridge.shortMsg(e))).append('}');
        }
        return sb.append(']').toString();
    }

    static String threadStatus(int status) {
        switch (status) {
            case ThreadReference.THREAD_STATUS_MONITOR: return "MONITOR";
            case ThreadReference.THREAD_STATUS_NOT_STARTED: return "NOT_STARTED";
            case ThreadReference.THREAD_STATUS_RUNNING: return "RUNNING";
            case ThreadReference.THREAD_STATUS_SLEEPING: return "SLEEPING";
            case ThreadReference.THREAD_STATUS_WAIT: return "WAIT";
            case ThreadReference.THREAD_STATUS_ZOMBIE: return "ZOMBIE";
            default: return "UNKNOWN";
        }
    }

    static String sourcePath(String className) {
        String outer = className.split("\\$")[0];
        return outer.replace('.', '/') + ".java";
    }

    static String snippet(Config cfg, String rel, int line) {
        if (line < 0) return "[]";
        for (String dir : cfg.srcDirs) {
            Path p = Paths.get(dir, rel);
            if (!Files.isRegularFile(p)) continue;
            try {
                List<String> lines = Files.readAllLines(p);
                StringBuilder sb = new StringBuilder("[");
                for (int n = Math.max(1, line - 5); n <= Math.min(lines.size(), line + 5); n++) {
                    if (n > Math.max(1, line - 5)) sb.append(',');
                    sb.append("{\"line\":").append(n).append(",\"current\":").append(n == line).append(',');
                    sb.append("\"text\":").append(JdiBridge.quote(lines.get(n - 1))).append('}');
                }
                return sb.append(']').toString();
            } catch (Exception ignored) {
                return "[]";
            }
        }
        return "[]";
    }

    // ---- value formatting (depth-capped, browser-compact style) ----

    static String formatValue(Value v, int depth) {
        if (v == null) return "null";
        if (v instanceof VoidValue) return "void";
        if (v instanceof PrimitiveValue) {
            if (v instanceof BooleanValue) return ((BooleanValue) v).value() ? "true" : "false";
            if (v instanceof CharValue) return "'" + ((CharValue) v).value() + "'";
            return ((PrimitiveValue) v).toString();
        }
        if (v instanceof StringReference) {
            String s = ((StringReference) v).value();
            if (s.length() > JdiBridge.MAX_STRING) s = s.substring(0, JdiBridge.MAX_STRING) + "… (+" + (s.length() - JdiBridge.MAX_STRING) + " more chars)";
            return "\"" + s.replace("\"", "\\\"") + "\"";
        }
        if (v instanceof ArrayReference) {
            ArrayReference arr = (ArrayReference) v;
            int len = arr.length();
            StringBuilder sb = new StringBuilder(arr.type().name())
                    .append('[').append(len).append("]{");
            int show = Math.min(len, JdiBridge.MAX_ITEMS);
            try {
                List<Value> vals = arr.getValues(0, show == 0 ? 0 : show);
                for (int i = 0; i < vals.size(); i++) {
                    if (i > 0) sb.append(", ");
                    sb.append(depth >= 2 ? shallow(vals.get(i)) : formatValue(vals.get(i), depth + 1));
                }
            } catch (Exception e) {
                sb.append("?");
            }
            if (len > show) sb.append(", … (+").append(len - show).append(" more)");
            return sb.append('}').toString();
        }
        if (v instanceof ObjectReference) {
            ObjectReference obj = (ObjectReference) v;
            StringBuilder sb = new StringBuilder(obj.referenceType().name())
                    .append('@').append(obj.uniqueID());
            if (depth >= 2) return sb.toString();
            sb.append('{');
            int n = 0;
            int total = 0;
            List<Field> instanceFields = new ArrayList<>();
            try {
                for (Field f : obj.referenceType().allFields()) {
                    if (f.isStatic() || f.isSynthetic()) continue;
                    instanceFields.add(f);
                }
                total = instanceFields.size();
                for (Field f : instanceFields) {
                    if (n >= JdiBridge.MAX_FIELDS) break;
                    if (n > 0) sb.append(", ");
                    Value fv;
                    try {
                        fv = obj.getValue(f);
                    } catch (Exception e) {
                        fv = null;
                    }
                    sb.append(f.name()).append('=');
                    if (fv instanceof ObjectReference && !(fv instanceof StringReference)) {
                        sb.append(shallow(fv));
                    } else {
                        sb.append(formatValue(fv, depth + 1));
                    }
                    n++;
                }
                if (total > n) {
                    if (n > 0) sb.append(", ");
                    sb.append("\"… (+").append(total - n).append(" more fields)\"");
                }
            } catch (Exception e) {
                sb.append('?');
            }
            return sb.append('}').toString();
        }
        return v.toString();
    }

    static String shallow(Value v) {
        if (v == null) return "null";
        if (v instanceof ObjectReference) {
            ObjectReference o = (ObjectReference) v;
            return o.referenceType().name() + "@" + o.uniqueID();
        }
        return formatValue(v, 2);
    }

    /** Tracking-only top-level preview (uniform shallow contract): the
     *  value's own summary capped at MAX_STRING chars — primitives and
     *  strings by content, arrays by type+length, objects by
     *  type+identity. No field walk (getValue), no element fetch
     *  (getValues), no method invocation: object internals changing
     *  without a top-level change are intentionally NOT detected, on
     *  every adapter. The display formatValue above is untouched. */
    static String formatTrackingValue(Value v) {
        if (v == null) return "null";
        if (v instanceof VoidValue) return "void";
        if (v instanceof PrimitiveValue) {
            String s;
            try {
                s = v.toString();
            } catch (Exception e) {
                return "?";
            }
            return capTrackStr(s);
        }
        if (v instanceof StringReference) {
            String s;
            try {
                s = ((StringReference) v).value();
            } catch (Exception e) {
                return "?";
            }
            if (s == null) return "null";
            return capTrackStr("\"" + s + "\"");
        }
        if (v instanceof ArrayReference) {
            ArrayReference arr = (ArrayReference) v;
            String t;
            try {
                t = arr.type().name();
            } catch (Exception e) {
                t = "Array";
            }
            int len;
            try {
                len = arr.length();
            } catch (Exception e) {
                len = -1;
            }
            return capTrackStr(t + "[" + len + "]");
        }
        if (v instanceof ObjectReference) {
            ObjectReference obj = (ObjectReference) v;
            String t;
            try {
                t = obj.referenceType().name();
            } catch (Exception e) {
                t = "?";
            }
            long id;
            try {
                id = obj.uniqueID();
            } catch (Exception e) {
                id = -1;
            }
            return capTrackStr(t + "@" + id);
        }
        try {
            return capTrackStr(v.toString());
        } catch (Exception e) {
            return "?";
        }
    }

    static String capTrackStr(String s) {
        if (s == null) return "null";
        if (s.length() <= JdiBridge.MAX_STRING) return s;
        return s.substring(0, JdiBridge.MAX_STRING) + "… (+"
                + (s.length() - JdiBridge.MAX_STRING) + " more chars)";
    }

    // ---- session server (persistent VM connection over TCP) ----
    //
    // Protocol: DAP-style framing (Content-Length + JSON) on 127.0.0.1.
    // One request per connection: {"cmd":"step","mode":"over","timeout":20}
    // Response: {"ok":true,...} or {"ok":false,"error":"..."}.
    // Commands: step{mode}, continue, context, stack, vars{frame},
    //           eval{expr,frame}, close.
}
