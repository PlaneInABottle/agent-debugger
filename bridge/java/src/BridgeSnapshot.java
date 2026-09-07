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
import java.util.LinkedHashMap;
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

    // ---- M5.2: st-parameterized tracking + text helpers (moved verbatim
    // from BridgeSession; no new top-level class). Lock contract (see the
    // per-method notes): the tracking group below MUTATES SessionState
    // (lastTop/lastFunc/lastChanged/lastRemoved/lastChangedComplete plus
    // the lastTrack* fields), so every production caller holds the
    // caller-held st.sessionLock — the six trackChanges sites run inside
    // the awaitStopInner Phase-B synchronized block, the three
    // changeFieldsJson sites inside dispatchInner synchronized sections
    // (incl. via waitJson), and the internal storeTrack/compareTrack/
    // degradeTrack/trackWarn/jsonTotal/jsonStrings calls never escape the
    // group. The text group (timeoutText/withCaptureStage/
    // captureExitContextJson/waitContextJson) READS st/st.cfg fields and
    // formats text only — it acquires no lock, spawns no thread, performs
    // no socket IO, so moving it changes no synchronization (call sites
    // and threads are identical). No moved body contains synchronized,
    // lock acquisition, thread spawn, or socket IO (grep-enforced via
    // scripts/check_java_owners.sh).

    /** Caller must hold st.sessionLock: mutates the tracking fields. */
    static void trackChanges(SessionState st) {
        // Display-independent change tracking (MAX_VARS display cap and
        // its "…" sentinel never feed this path — visibleVariables are
        // scanned up to CHANGE_TRACK_MAX=256 with shallow top-level
        // previews (formatTrackingValue: no field walk, no element
        // fetch, never invoke target code).
        //
        // Contract (uniform on all adapters): changed holds sorted
        // value-changes + new names on complete scans; removed holds
        // sorted missing names on complete scans only (never asserted
        // under incomplete tracking); changedComplete is true only when
        // both previous and current scans are exhaustive with the same
        // frame identity and no tracking error; changeTracking carries
        // {complete,scanned,total,truncated,reason?} with reason in
        // first-snapshot|function-changed|truncated|tracking-error. Empty
        // changed with complete=false is UNKNOWN, not no-change. First
        // baseline / function change stores changed=[] + complete=false
        // (no previous snapshot means no change comparison — never report
        // all locals as changed). Incomplete/error reports only value
        // changes over the name intersection; added/removed suppressed. A
        // baseline stored while incomplete can never make the NEXT
        // comparison complete; after a complete current baseline lands,
        // the following stop can become complete. Never throws: failures
        // degrade to an empty baseline with a class-only stderr warning
        // (no variable data/secrets), so the park always completes.
        Map<String, String> cur = new LinkedHashMap<>();
        Integer total = null;
        boolean truncated = false;
        String scanReason = null;
        String func = "?";
        boolean fetchOk = true;
        try {
            List<StackFrame> frames = BridgeSnapshot.safeFrames(st.thread);
            if (!frames.isEmpty()) {
                StackFrame f = frames.get(0);
                func = BridgeEval.frameIdentityOf(f);
                try {
                    List<LocalVariable> vars = f.visibleVariables();
                    Map<LocalVariable, Value> vals = f.getValues(vars);
                    java.util.TreeMap<String, String> sorted = new java.util.TreeMap<>();
                    for (Map.Entry<LocalVariable, Value> ve : vals.entrySet()) {
                        String name;
                        try {
                            name = ve.getKey().name();
                        } catch (Exception ignored) {
                            continue;
                        }
                        if (name == null) continue;
                        String v;
                        try {
                            v = BridgeSnapshot.formatTrackingValue(ve.getValue());
                        } catch (Throwable t) {
                            v = "?";
                        }
                        sorted.put(name, v);
                    }
                    total = sorted.size();
                    truncated = total > JdiBridge.CHANGE_TRACK_MAX;
                    scanReason = truncated ? "truncated" : null;
                    int n = 0;
                    for (Map.Entry<String, String> e : sorted.entrySet()) {
                        if (n >= JdiBridge.CHANGE_TRACK_MAX) break;
                        cur.put(e.getKey(), e.getValue());
                        n++;
                    }
                } catch (AbsentInformationException aie) {
                    fetchOk = false;
                    scanReason = "tracking-error";
                }
            } else {
                fetchOk = false;
                scanReason = "tracking-error";
            }
        } catch (Throwable t) {
            // No explicit warning here: compareTrack below stores with
            // warn=true, which writes exactly one class-only line.
            fetchOk = false;
            cur = new LinkedHashMap<>();
            scanReason = "tracking-error";
        }
        boolean curComplete = fetchOk && scanReason == null;
        try {
            compareTrack(st, cur, func, total, truncated, scanReason, curComplete);
        } catch (Throwable t) {
            try {
                System.err.println("warn: change tracking degraded ("
                        + t.getClass().getSimpleName() + ")");
            } catch (Exception ignored) {}
            st.lastTop = new LinkedHashMap<>();
            st.lastChanged = "[]";
            st.lastRemoved = "[]";
            st.lastChangedComplete = false;
            st.lastTrackComplete = false;
            st.lastTrackReason = "tracking-error";
            st.lastTrackWarn = "change tracking incomplete (tracking-error); "
                    + "changed lists only certain value changes";
            st.lastChangeTracking = "{\"complete\":false,\"scanned\":0,"
                    + "\"total\":" + jsonTotal(total) + ",\"truncated\":" + truncated
                    + ",\"reason\":\"tracking-error\"}";
        }
    }

    /** Class-only tracking warning (never variable data/secrets).
     *  Production callers hold st.sessionLock (via storeTrack/degradeTrack). */
    static String trackWarn(String reason) {
        try {
            System.err.println("warn: change tracking degraded (" + reason + ")");
        } catch (Exception ignored) {}
        return "change tracking incomplete (" + reason
                + "); changed lists only certain value changes";
    }

    /** Render a tracking total: a failed scan is unknown (null, never 0). Pure. */
    static String jsonTotal(Integer total) {
        return total == null ? "null" : String.valueOf(total);
    }

    /** Caller must hold st.sessionLock: mutates the tracking fields. */
    static void storeTrack(SessionState st, Map<String, String> cur,
            String func, List<String> changed, List<String> removed,
            boolean complete, int scanned, Integer total, boolean truncated,
            String reason, boolean curComplete, String scanReason,
            boolean warn) {
        st.lastTop = cur != null ? cur : new LinkedHashMap<>();
        st.lastFunc = func;
        st.lastChanged = jsonStrings(changed);
        st.lastRemoved = jsonStrings(removed);
        st.lastChangedComplete = complete;
        st.lastTrackComplete = curComplete;
        st.lastTrackReason = scanReason != null ? scanReason
                : (curComplete ? null : reason);
        // Incomplete branches always pass warn=true, so exactly one
        // class-only warning is written here; complete scans stay quiet.
        st.lastTrackWarn = warn ? trackWarn(reason) : null;
        StringBuilder sb = new StringBuilder("{\"complete\":").append(complete)
                .append(",\"scanned\":").append(scanned)
                .append(",\"total\":").append(jsonTotal(total))
                .append(",\"truncated\":").append(truncated);
        if (reason != null && !complete) {
            sb.append(",\"reason\":").append(JdiBridge.quote(reason));
        }
        st.lastChangeTracking = sb.append('}').toString();
    }

    /** Pure JSON rendering of a name list. */
    static String jsonStrings(List<String> names) {
        StringBuilder sb = new StringBuilder("[");
        boolean first = true;
        if (names != null) {
            for (String name : names) {
                if (!first) sb.append(',');
                first = false;
                sb.append(JdiBridge.quote(name));
            }
        }
        return sb.append(']').toString();
    }

    /** Caller must hold st.sessionLock: mutates the tracking fields. */
    static void compareTrack(SessionState st, Map<String, String> cur,
            String func, Integer total, boolean truncated, String scanReason,
            boolean curComplete) {
        int scanned = total == null ? 0
                : Math.min(total, JdiBridge.CHANGE_TRACK_MAX);
        Map<String, String> last = st.lastTop;
        boolean prevComplete = st.lastTrackComplete;
        String prevReason = st.lastTrackReason != null ? st.lastTrackReason
                : "truncated";
        if (last == null) {
            // No previous snapshot means no change comparison. A problem
            // with the CURRENT scan (truncated/error) dominates the
            // reason — it describes the stored baseline the next stop
            // compares against; first-snapshot only when the current scan
            // is itself exhaustive.
            String reason = scanReason != null ? scanReason
                    : (curComplete ? "first-snapshot" : "tracking-error");
            storeTrack(st, cur, func, new ArrayList<>(), new ArrayList<>(),
                    false, scanned, total, truncated, reason,
                    curComplete, scanReason, true);
            return;
        }
        if (st.lastFunc == null || !st.lastFunc.equals(func)) {
            String reason = scanReason != null ? scanReason
                    : (curComplete ? "function-changed" : "tracking-error");
            storeTrack(st, cur, func, new ArrayList<>(), new ArrayList<>(),
                    false, scanned, total, truncated, reason,
                    curComplete, scanReason, true);
            return;
        }
        if (!prevComplete || !curComplete) {
            List<String> changed = new ArrayList<>();
            try {
                for (Map.Entry<String, String> e : cur.entrySet()) {
                    String old = last.get(e.getKey());
                    if (old != null && !e.getValue().equals(old)) {
                        changed.add(e.getKey());
                    }
                }
                java.util.Collections.sort(changed);
            } catch (Exception ignored) {
                changed = new ArrayList<>();
            }
            String reason = !curComplete
                    ? (scanReason != null ? scanReason : "tracking-error")
                    : (prevReason != null ? prevReason : "truncated");
            storeTrack(st, cur, func, changed, new ArrayList<>(),
                    false, scanned, total, truncated, reason,
                    curComplete, scanReason, true);
            return;
        }
        List<String> changed = new ArrayList<>();
        List<String> removed = new ArrayList<>();
        try {
            for (Map.Entry<String, String> e : cur.entrySet()) {
                String old = last.get(e.getKey());
                if (old == null || !e.getValue().equals(old)) {
                    changed.add(e.getKey());
                }
            }
            for (String name : last.keySet()) {
                if (!cur.containsKey(name)) removed.add(name);
            }
            java.util.Collections.sort(changed);
            java.util.Collections.sort(removed);
        } catch (Exception e) {
            storeTrack(st, cur, func, new ArrayList<>(), new ArrayList<>(),
                    false, scanned, total, truncated, "tracking-error",
                    false, "tracking-error", true);
            return;
        }
        storeTrack(st, cur, func, changed, removed,
                true, scanned, total, truncated, null, true, null, false);
    }

    /** Caller must hold st.sessionLock: mutates the tracking fields. */
    static void degradeTrack(SessionState st, String clsName, Integer total) {
        List<StackFrame> frames = BridgeSnapshot.safeFrames(st.thread);
        String func = "?";
        if (!frames.isEmpty()) {
            func = BridgeEval.frameIdentityOf(frames.get(0));
        }
        storeTrack(st, new LinkedHashMap<>(), func,
                new ArrayList<>(), new ArrayList<>(),
                false, 0, total, false, "tracking-error",
                false, "tracking-error", false);
        // Single class-only warning (storeTrack stayed silent by design),
        // matching the response's trackingWarning.
        st.lastTrackWarn = trackWarn(clsName != null ? clsName : "tracking-error");
    }

    /** Additive change-tracking response fragment (uniform contract):
     *  `"changed":…,"removed":…,"changedComplete":…,"changeTracking":…`
     *  plus `"trackingWarning":…` only when incomplete. The stored strings
     *  are bridge-rendered JSON, so this never throws.
     *  Caller must hold st.sessionLock (all production sites do). */
    static String changeFieldsJson(SessionState st) {
        StringBuilder sb = new StringBuilder("\"changed\":");
        sb.append(st.lastChanged != null ? st.lastChanged : "[]");
        sb.append(",\"removed\":");
        sb.append(st.lastRemoved != null ? st.lastRemoved : "[]");
        sb.append(",\"changedComplete\":").append(st.lastChangedComplete);
        sb.append(",\"changeTracking\":");
        sb.append(st.lastChangeTracking != null ? st.lastChangeTracking
                : "{\"complete\":false,\"scanned\":0,\"total\":null,"
                + "\"truncated\":false}");
        if (st.lastTrackWarn != null && !st.lastChangedComplete) {
            sb.append(",\"trackingWarning\":")
                    .append(JdiBridge.quote(st.lastTrackWarn));
        }
        return sb.toString();
    }

    /** Timeout message with the compact identity hint (debuggee-first,
     *  names the target, never claims root cause).
     *  Reads st/cfg fields only; acquires no lock. */
    static String timeoutText(SessionState st, long timeoutMs) {
        String msg = "timeout: no stop within " + (timeoutMs / 1000) + "s";
        String hint = (st != null && st.cfg != null && st.cfg.identityHint != null
                && !st.cfg.identityHint.isEmpty()) ? st.cfg.identityHint
                : (st != null && st.cfg != null ? st.cfg.seedHint : null);
        if (hint != null && !hint.isEmpty()) {
            msg += "; " + hint;
        }
        return msg;
    }

    /** Additive capture stage on a pre-rendered wait-context JSON: inserts
     *  `"captureStage":"<stage>","ephemeralPlanted":<bool>` before the
     *  final `}`. Best-effort (returns the input when it is not an
     *  object); never endpoint diagnosis, never "unreachable code".
     *  Pure (string in, string out); acquires no lock. */
    static String withCaptureStage(String ctxJson, String stage, boolean planted) {
        if (ctxJson == null || !ctxJson.endsWith("}")) return ctxJson;
        return ctxJson.substring(0, ctxJson.length() - 1)
                + ",\"captureStage\":" + JdiBridge.quote(stage)
                + ",\"ephemeralPlanted\":" + planted + "}";
    }

    /** Pre-rendered capture exit/stale-session context: wait timers
     *  (seconds since epoch / ms waited, same units as the timeout
     *  context), trigger unknown, the truthful stage, and the layered
     *  identity. waitStartMs is the pump/entry start; session-gone and
     *  before-armed pass the entry time (waitedMs ~0 — no wait occurred).
     *  Reads st/cfg fields only; acquires no lock. */
    static String captureExitContextJson(SessionState st, String stage,
            boolean planted, String expectedBreak, long waitStartMs) {
        long nowMs = System.currentTimeMillis();
        StringBuilder sb = new StringBuilder("{\"waitStartedAt\":");
        sb.append(waitStartMs / 1000)
                .append(",\"waitedMs\":").append(Math.max(0, nowMs - waitStartMs));
        sb.append(",\"triggerStatus\":\"unknown\"");
        sb.append(",\"captureStage\":").append(JdiBridge.quote(stage));
        sb.append(",\"ephemeralPlanted\":").append(planted);
        if (expectedBreak != null) {
            sb.append(",\"expectedBreak\":").append(JdiBridge.quote(expectedBreak));
        }
        String ident = (st != null && st.cfg != null && st.cfg.targetIdentityJson != null)
                ? st.cfg.targetIdentityJson : "null";
        sb.append(",\"targetIdentity\":").append(ident);
        sb.append(",\"note\":").append(JdiBridge.quote(
                "external trigger execution is not observed by the debugger; "
                + "this timeout means no stop was observed, "
                + "not that the code is unreachable"));
        return sb.append('}').toString();
    }

    /** Honest timeout context (pre-rendered JSON): the debugger never
     *  observes the external trigger, so triggerStatus is always unknown;
     *  success paths never fabricate sent/failed. expectedBreak rides only
     *  when the capture planted one. The layered targetIdentity is the
     *  redacted + capped handshake copy (never rebuilt per command).
     *  Reads st/cfg fields only; acquires no lock. */
    static String waitContextJson(SessionState st, long timeoutMs, long waitStartMs,
            String expectedBreak) {
        long waitedMs = Math.max(0, System.currentTimeMillis() - waitStartMs);
        StringBuilder sb = new StringBuilder("{\"waitStartedAt\":");
        sb.append(waitStartMs / 1000).append(",\"waitedMs\":").append(waitedMs)
                .append(",\"triggerStatus\":\"unknown\"");
        if (expectedBreak != null) {
            sb.append(",\"expectedBreak\":").append(JdiBridge.quote(expectedBreak));
        }
        String ident = (st != null && st.cfg != null && st.cfg.targetIdentityJson != null)
                ? st.cfg.targetIdentityJson : "null";
        sb.append(",\"targetIdentity\":").append(ident);
        sb.append(",\"note\":").append(JdiBridge.quote(
                "external trigger execution is not observed by the debugger; "
                + "this timeout means no stop was observed, "
                + "not that the code is unreachable"));
        return sb.append('}').toString();
    }
    // ---- end M5.2 ----

    // ---- session server (persistent VM connection over TCP) ----
    //
    // Protocol: DAP-style framing (Content-Length + JSON) on 127.0.0.1.
    // One request per connection: {"cmd":"step","mode":"over","timeout":20}
    // Response: {"ok":true,...} or {"ok":false,"error":"..."}.
    // Commands: step{mode}, continue, context, stack, vars{frame},
    //           eval{expr,frame}, close.
}
