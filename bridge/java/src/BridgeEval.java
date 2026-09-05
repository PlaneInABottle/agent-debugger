import com.sun.jdi.AbsentInformationException;
import com.sun.jdi.ArrayReference;
import com.sun.jdi.Field;
import com.sun.jdi.LocalVariable;
import com.sun.jdi.Location;
import com.sun.jdi.ObjectReference;
import com.sun.jdi.StackFrame;
import com.sun.jdi.StringReference;
import com.sun.jdi.ThreadReference;
import com.sun.jdi.Value;
import com.sun.jdi.VirtualMachine;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

// Read-only expression layer: conditions, logpoint templates, referrers, eval. Moved verbatim from JdiBridge.java.
class BridgeEval {
    static final java.util.Set<String> COND_CALLS = new java.util.HashSet<>(
            java.util.Arrays.asList("size", "length", "isEmpty", "get"));

    static String locKey(String cls, int line) {
        return cls + ":" + line;
    }

    static String lookupCond(Config cfg, Location loc) {
        String cls = "?";
        String method = "?";
        int line = -1;
        try { cls = loc.declaringType().name(); } catch (Exception ignored) {}
        try { method = loc.method().name(); } catch (Exception ignored) {}
        try { line = loc.lineNumber(); } catch (Exception ignored) {}
        String c = cfg.condByLoc.get(locKey(cls, line));
        if (c == null) c = cfg.condByLoc.get("method:" + cls + "." + method);
        return c;
    }

    /** True when the stop should be reported; unknown names count as false. */

    static boolean checkCond(ThreadReference thread, Location loc, String cond) {
        try {
            List<StackFrame> frames = BridgeSnapshot.safeFrames(thread);
            if (frames.isEmpty()) return false;
            String[] ops = {"==", "!=", ">=", "<=", ">", "<"};
            String op = null;
            int at = -1;
            for (String o : ops) {
                at = cond.indexOf(o);
                if (at >= 0) { op = o; break; }
            }
            if (op == null) return false;
            String left = cond.substring(0, at).trim();
            String right = cond.substring(at + op.length()).trim();
            Value lv = resolvePath(thread, frames.get(0), left, COND_CALLS);
            return compareValues(lv, op, right);
        } catch (Exception e) {
            return false;
        }
    }

    static boolean compareValues(Value left, String op, String right) {
        boolean eq = op.equals("==");
        boolean ne = op.equals("!=");
        if (right.equals("null")) {
            if (eq) return left == null;
            if (ne) return left != null;
            return false;
        }
        if (left == null) return ne;
        if (left instanceof com.sun.jdi.BooleanValue) {
            if (!right.equals("true") && !right.equals("false")) return false;
            boolean rv = Boolean.parseBoolean(right);
            boolean lv = ((com.sun.jdi.BooleanValue) left).value();
            if (eq) return lv == rv;
            if (ne) return lv != rv;
            return false;
        }
        Double ln = numOf(left);
        if (ln != null) {
            Double rn = parseNum(right);
            if (rn == null) return false;
            int c = ln.compareTo(rn);
            switch (op) {
                case "==": return c == 0;
                case "!=": return c != 0;
                case ">": return c > 0;
                case "<": return c < 0;
                case ">=": return c >= 0;
                case "<=": return c <= 0;
                default: return false;
            }
        }
        if (left instanceof StringReference) {
            String lv = ((StringReference) left).value();
            String rv = right;
            if (rv.startsWith("\"") && rv.endsWith("\"") && rv.length() >= 2) {
                rv = rv.substring(1, rv.length() - 1);
            }
            if (eq) return lv.equals(rv);
            if (ne) return !lv.equals(rv);
            return false;
        }
        return false;
    }

    static Double numOf(Value v) {
        if (v instanceof com.sun.jdi.IntegerValue) return (double) ((com.sun.jdi.IntegerValue) v).value();
        if (v instanceof com.sun.jdi.LongValue) return (double) ((com.sun.jdi.LongValue) v).value();
        if (v instanceof com.sun.jdi.DoubleValue) return ((com.sun.jdi.DoubleValue) v).value();
        if (v instanceof com.sun.jdi.FloatValue) return (double) ((com.sun.jdi.FloatValue) v).value();
        if (v instanceof com.sun.jdi.ShortValue) return (double) ((com.sun.jdi.ShortValue) v).value();
        if (v instanceof com.sun.jdi.ByteValue) return (double) ((com.sun.jdi.ByteValue) v).value();
        return null;
    }

    static Double parseNum(String s) {
        try {
            return Double.parseDouble(s);
        } catch (NumberFormatException e) {
            return null;
        }
    }

    // ---- logpoints: evaluate template holes, never stop ----

    static final int MAX_LOG_LINES = 2000;

    static List<String> matchingTemplates(Config cfg, Location loc) {
        String cls = "?";
        int line = -1;
        try { cls = loc.declaringType().name(); } catch (Exception ignored) {}
        try { line = loc.lineNumber(); } catch (Exception ignored) {}
        List<String> out = new ArrayList<>();
        for (Logpoint lp : cfg.logpoints) {
            if (lp.cls.equals(cls) && lp.line == line) out.add(lp.template);
        }
        return out;
    }

    static String renderTemplate(ThreadReference thread, StackFrame frame, String template) throws Exception {
        StringBuilder sb = new StringBuilder();
        int i = 0;
        while (i < template.length()) {
            int open = template.indexOf('{', i);
            if (open < 0) { sb.append(template.substring(i)); break; }
            int close = template.indexOf('}', open);
            if (close < 0) { sb.append(template.substring(i)); break; }
            sb.append(template, i, open);
            String hole = template.substring(open + 1, close).trim();
            sb.append(BridgeSnapshot.formatValue(resolvePath(thread, frame, hole, COND_CALLS), 0));
            i = close + 1;
        }
        return sb.toString();
    }

    // ---- referrers: who holds this object (bounded heap walk) ----

    static final int MAX_REFS_PER_NODE = 50;
    static final int MAX_REF_NODES = 500;
    static final int MAX_REF_DEPTH = 4;
    static final int MAX_REF_CHAINS = 20;

    static String referrers(ThreadReference thread, StackFrame frame, String inner) throws Exception {
        List<String> parts = splitTopLevel(inner, ',');
        if (parts.isEmpty()) throw new BridgeException("refs() needs a path");
        String path = parts.get(0).trim();
        int depth = 2;
        if (parts.size() > 1) {
            try {
                depth = Integer.parseInt(parts.get(1).trim());
            } catch (NumberFormatException e) {
                throw new BridgeException("refs() depth must be a number");
            }
            if (depth < 1 || depth > MAX_REF_DEPTH) {
                throw new BridgeException("refs() depth 1.." + MAX_REF_DEPTH);
            }
        }
        Value v = resolvePath(thread, frame, path, null);
        if (!(v instanceof ObjectReference)) return path + " is not an object (" + BridgeSnapshot.formatValue(v, 1) + ")";
        ObjectReference target = (ObjectReference) v;
        // BFS outward. Heap references only: locals are GC roots but not objects,
        // so a purely stack-held object reports zero referrers. Documented, not a bug.
        java.util.Map<Long, Long> parent = new java.util.HashMap<>();
        java.util.Map<Long, Integer> nodeDepth = new java.util.HashMap<>();
        java.util.Map<Long, ObjectReference> byId = new java.util.HashMap<>();
        java.util.ArrayDeque<ObjectReference> queue = new java.util.ArrayDeque<>();
        long targetId = target.uniqueID();
        byId.put(targetId, target);
        nodeDepth.put(targetId, 0);
        queue.add(target);
        int direct = 0;
        boolean capped = false;
        while (!queue.isEmpty() && byId.size() < MAX_REF_NODES) {
            ObjectReference cur = queue.poll();
            int d = nodeDepth.get(cur.uniqueID());
            if (d >= depth) continue;
            List<ObjectReference> refs;
            try {
                refs = cur.referringObjects(MAX_REFS_PER_NODE);
            } catch (Exception e) {
                continue;
            }
            if (cur.uniqueID() == targetId) direct = refs.size();
            if (refs.size() >= MAX_REFS_PER_NODE) capped = true;
            for (ObjectReference r : refs) {
                long id = r.uniqueID();
                if (byId.containsKey(id)) continue;
                byId.put(id, r);
                parent.put(id, cur.uniqueID());
                nodeDepth.put(id, d + 1);
                queue.add(r);
                if (byId.size() >= MAX_REF_NODES) { capped = true; break; }
            }
        }
        StringBuilder sb = new StringBuilder("target: ").append(BridgeSnapshot.shallow(target));
        sb.append("\ndirect referrers: ").append(direct).append(capped ? "+" : "");
        sb.append(" (heap only; stack locals are invisible to this walk)");
        // Emit chains target <- holder <- holder-holder..., up to the cap.
        List<Long> leaves = new ArrayList<>();
        for (Long id : byId.keySet()) {
            if (id != targetId) leaves.add(id);
        }
        leaves.sort((a, b) -> Integer.compare(nodeDepth.get(a), nodeDepth.get(b)));
        int shown = 0;
        for (Long id : leaves) {
            if (shown >= MAX_REF_CHAINS) break;
            if (nodeDepth.get(id) > depth) continue;
            StringBuilder chain = new StringBuilder(BridgeSnapshot.shallow(byId.get(id)));
            long walk = id;
            while (parent.containsKey(walk) && parent.get(walk) != targetId) {
                walk = parent.get(walk);
                chain.append(" <- ").append(BridgeSnapshot.shallow(byId.get(walk)));
            }
            chain.append(" <- ").append(BridgeSnapshot.shallow(target));
            sb.append('\n').append(chain);
            shown++;
        }
        int remaining = leaves.size() - shown;
        if (remaining > 0) sb.append("\n(+").append(remaining).append(" more chains)");
        return sb.toString();
    }

    static String threadsDumpJson(VirtualMachine vm) throws Exception {
        List<ThreadReference> all = vm.allThreads();
        StringBuilder sb = new StringBuilder("[");
        int cap = 15;
        for (int i = 0; i < Math.min(all.size(), cap); i++) {
            ThreadReference t = all.get(i);
            if (i > 0) sb.append(',');
            sb.append("{\"id\":").append(t.uniqueID()).append(',');
            JdiBridge.kv(sb, "name", t.name(), true);
            sb.append(",\"status\":").append(JdiBridge.quote(BridgeSnapshot.threadStatus(t.status())));
            sb.append(",\"frames\":").append(BridgeSnapshot.framesJson(t, false));
            sb.append('}');
        }
        if (all.size() > cap) {
            sb.append(",{\"id\":-1,\"name\":\"…\",\"note\":"
                    + JdiBridge.quote("+" + (all.size() - cap) + " more threads") + "}");
        }
        return sb.append(']').toString();
    }

    static String watchInfo(Field f, String access, Value v) {
        String owner = "?";
        try { owner = f.declaringType().name(); } catch (Exception ignored) {}
        String val;
        try {
            val = BridgeSnapshot.formatValue(v, 1);
        } catch (Exception e) {
            val = "?";
        }
        return "{\"watch\":{\"field\":" + JdiBridge.quote(owner + "." + f.name())
                + ",\"access\":" + JdiBridge.quote(access) + ",\"value\":" + JdiBridge.quote(val) + "}}";
    }

    static boolean wantedExit(Config cfg, com.sun.jdi.event.MethodExitEvent me) {
        String cls = "?";
        String m = "?";
        try { cls = me.method().declaringType().name(); } catch (Exception ignored) {}
        try { m = me.method().name(); } catch (Exception ignored) {}
        List<String> methods = cfg.exitMethods.get(cls);
        return methods != null && methods.contains(m);
    }

    static String exitInfo(com.sun.jdi.event.MethodExitEvent me) {
        String m = "?";
        try { m = me.method().name(); } catch (Exception ignored) {}
        String ret;
        try {
            ret = BridgeSnapshot.formatValue(me.returnValue(), 1);
        } catch (UnsupportedOperationException e) {
            ret = "unavailable";
        } catch (Exception e) {
            ret = "?";
        }
        return "{\"exit\":{\"method\":" + JdiBridge.quote(m) + ",\"returns\":" + JdiBridge.quote(ret) + "}}";
    }

    static String exceptionInfo(com.sun.jdi.event.ExceptionEvent ee) {
        String name = "?";
        try { name = ee.exception().referenceType().name(); } catch (Exception ignored) {}
        return "{\"exception\":{\"class\":" + JdiBridge.quote(name) + "}}";
    }

    static String describeBreaks(Config cfg) {        StringBuilder sb = new StringBuilder();
        for (Map.Entry<String, List<Integer>> e : cfg.breakpoints.entrySet()) {
            for (int line : e.getValue()) {
                if (sb.length() > 0) sb.append(", ");
                sb.append(e.getKey()).append(':').append(line);
            }
        }
        for (Map.Entry<String, List<String>> e : cfg.methodBreaks.entrySet()) {
            for (String m : e.getValue()) {
                if (sb.length() > 0) sb.append(", ");
                sb.append("method:").append(e.getKey()).append('.').append(m);
            }
        }
        for (String f : cfg.excFilters) {
            if (sb.length() > 0) sb.append(", ");
            sb.append("exc:").append(f);
        }
        for (Watchpoint w : cfg.watchpoints) {
            if (sb.length() > 0) sb.append(", ");
            sb.append("watch:").append(w.cls).append('.').append(w.field);
        }
        for (Map.Entry<String, List<String>> e : cfg.exitMethods.entrySet()) {
            for (String m : e.getValue()) {
                if (sb.length() > 0) sb.append(", ");
                sb.append("exit:").append(e.getKey()).append('.').append(m);
            }
        }
        return sb.toString();
    }

    /** Diff top-frame locals against the previous stop; store JSON name array. */

    static String evalExpr(ThreadReference thread, StackFrame frame, String expr) throws Exception {
        String e = expr.trim();
        if (e.isEmpty()) throw new BridgeException("empty expression");
        // Referrers query: refs(path) or refs(path, depth).
        if (e.startsWith("refs(") && e.endsWith(")")) {
            return referrers(thread, frame, e.substring(5, e.length() - 1));
        }
        // Literals pass through.
        if ((e.startsWith("\"") && e.endsWith("\"") && e.length() >= 2)
                || e.equals("true") || e.equals("false") || e.equals("null")
                || e.matches("-?\\d+") || e.matches("-?\\d+\\.\\d+")) {
            return e;
        }
        return BridgeSnapshot.formatValue(resolvePath(thread, frame, e, null), 1);
    }

    /**
     * Resolve a dotted path to a live JDI value. {@code allowedCalls==null}
     * permits any literal-arg call (interactive eval); otherwise only the
     * named read-only methods (conditions, logpoints).
     */

    static Value resolvePath(ThreadReference thread, StackFrame frame, String expr,
            java.util.Set<String> allowedCalls) throws Exception {
        List<String> parts = splitTopLevel(expr, '.');
        Value cur = null;
        for (int i = 0; i < parts.size(); i++) {
            String p = parts.get(i).trim();
            if (p.isEmpty()) throw new BridgeException("bad expression: " + expr);
            if (p.endsWith(")")) {
                int lp = p.indexOf('(');
                if (lp < 0) throw new BridgeException("bad expression: " + expr);
                String name = p.substring(0, lp).trim();
                String argText = p.substring(lp + 1, p.length() - 1).trim();
                if (allowedCalls != null && !allowedCalls.contains(name)) {
                    throw new BridgeException("method " + name + "() not allowed here "
                            + "(allowed: " + allowedCalls + ")");
                }
                if (cur == null) {
                    ObjectReference thiz = frame.thisObject();
                    if (thiz == null) throw new BridgeException("no this at " + p);
                    cur = thiz;
                }
                cur = invokeCall(thread, cur, name, argText);
            } else if (p.contains("[")) {
                int lb = p.indexOf('[');
                int rb = p.lastIndexOf(']');
                String base = p.substring(0, lb).trim();
                int idx;
                try {
                    idx = Integer.parseInt(p.substring(lb + 1, rb).trim());
                } catch (Exception ex) {
                    throw new BridgeException("only numeric indexes supported: " + p);
                }
                Value baseVal = base.isEmpty() ? cur : (cur == null ? resolveName(frame, base) : resolveField(cur, base));
                if (baseVal instanceof ArrayReference) {
                    ArrayReference arr = (ArrayReference) baseVal;
                    if (idx < 0 || idx >= arr.length()) {
                        throw new BridgeException("index " + idx + " out of bounds (len " + arr.length() + ")");
                    }
                    cur = arr.getValue(idx);
                } else if (baseVal instanceof ObjectReference) {
                    // List sugar: items[0] -> items.get(0). Agents always try this.
                    if (allowedCalls != null && !allowedCalls.contains("get")) {
                        throw new BridgeException("indexing a List is not allowed here: " + p);
                    }
                    cur = invokeCall(thread, baseVal, "get", String.valueOf(idx));
                } else {
                    throw new BridgeException("not indexable: " + (base.isEmpty() ? p : base));
                }
            } else {
                cur = (cur == null) ? resolveName(frame, p) : resolveField(cur, p);
            }
        }
        return cur;
    }

    static List<String> splitTopLevel(String s, char sep) {
        List<String> parts = new ArrayList<>();
        int depth = 0;
        boolean inStr = false;
        int start = 0;
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '"' && (i == 0 || s.charAt(i - 1) != '\\')) inStr = !inStr;
            if (inStr) continue;
            if (c == '(' || c == '[') depth++;
            else if (c == ')' || c == ']') depth--;
            else if (c == sep && depth == 0) {
                parts.add(s.substring(start, i));
                start = i + 1;
            }
        }
        parts.add(s.substring(start));
        return parts;
    }

    static Value resolveName(StackFrame frame, String name) throws Exception {
        if (name.equals("this")) {
            try {
                ObjectReference thiz = frame.thisObject();
                if (thiz != null) return thiz;
            } catch (Exception ignored) {}
            throw new BridgeException("no this in static context");
        }
        try {
            LocalVariable var = frame.visibleVariableByName(name);
            if (var != null) return frame.getValue(var);
        } catch (AbsentInformationException ignored) {}
        ObjectReference thiz = null;
        try { thiz = frame.thisObject(); } catch (Exception ignored) {}
        if (thiz != null) {
            Value v = fieldValue(thiz, name);
            if (v != null || hasField(thiz, name)) return v;
        }
        StringBuilder known = new StringBuilder();
        try {
            for (LocalVariable v : frame.visibleVariables()) {
                if (known.length() > 0) known.append(", ");
                known.append(v.name());
            }
        } catch (Exception ignored) {}
        throw new BridgeException("unknown name: " + name + " (locals: " + known + ")");
    }

    static boolean hasField(ObjectReference obj, String name) {
        for (Field f : obj.referenceType().allFields()) {
            if (f.name().equals(name)) return true;
        }
        return false;
    }

    static Value fieldValue(ObjectReference obj, String name) {
        for (Field f : obj.referenceType().allFields()) {
            if (f.name().equals(name)) {
                try {
                    return obj.getValue(f);
                } catch (Exception e) {
                    return null;
                }
            }
        }
        return null;
    }

    static Value resolveField(Value cur, String name) throws Exception {
        if (cur instanceof ArrayReference && name.equals("length")) {
            return cur.virtualMachine().mirrorOf(((ArrayReference) cur).length());
        }
        if (!(cur instanceof ObjectReference)) {
            throw new BridgeException("cannot get field " + name + " of primitive");
        }
        ObjectReference obj = (ObjectReference) cur;
        Value v = fieldValue(obj, name);
        if (v != null || hasField(obj, name)) return v;
        throw new BridgeException("no field " + name + " on " + obj.referenceType().name());
    }

    /** Invoke name(...) with literal int/string/boolean args (e.g. get(0)). */

    static Value invokeCall(ThreadReference thread, Value target, String name, String argText) throws Exception {
        if (!(target instanceof ObjectReference)) {
            throw new BridgeException("cannot call " + name + "() on primitive");
        }
        ObjectReference obj = (ObjectReference) target;
        List<Value> jdiArgs = new ArrayList<>();
        int arity = 0;
        if (!argText.isEmpty()) {
            for (String a : splitTopLevel(argText, ',')) {
                a = a.trim();
                if (a.startsWith("\"") && a.endsWith("\"") && a.length() >= 2) {
                    jdiArgs.add(obj.virtualMachine().mirrorOf(BridgeProto.parseJsonLite(a)));
                } else if (a.equals("true") || a.equals("false")) {
                    jdiArgs.add(obj.virtualMachine().mirrorOf(Boolean.parseBoolean(a)));
                } else if (a.matches("-?\\d+")) {
                    jdiArgs.add(obj.virtualMachine().mirrorOf(Integer.parseInt(a)));
                } else {
                    throw new BridgeException("only literal int/string/boolean args supported: " + a);
                }
                arity++;
            }
        }
        com.sun.jdi.Method method = null;
        for (com.sun.jdi.Method m : obj.referenceType().methodsByName(name)) {
            try {
                if (!m.isStatic() && !m.isNative() && m.argumentTypeNames().size() == arity) {
                    method = m;
                    break;
                }
            } catch (Exception ignored) {}
        }
        if (method == null) {
            throw new BridgeException("no " + arity + "-arg method " + name + "() on " + obj.referenceType().name());
        }
        // Invoke off-thread with a timeout: a method blocked on a lock held by
        // another suspended thread would otherwise hang the bridge forever
        // (and poison the session). The worker is daemon; a timed-out invoke
        // leaks one thread, not the session.
        final com.sun.jdi.Method m = method;
        final Value[] out = new Value[1];
        final Exception[] err = new Exception[1];
        Thread worker = new Thread(() -> {
            try {
                out[0] = obj.invokeMethod(thread, m, jdiArgs, ObjectReference.INVOKE_SINGLE_THREADED);
            } catch (Exception e) {
                err[0] = e;
            }
        });
        worker.setDaemon(true);
        worker.start();
        try {
            worker.join(10000);
        } catch (InterruptedException ie) {
            Thread.currentThread().interrupt();
            throw new BridgeException("interrupted while invoking " + name + "()");
        }
        if (worker.isAlive()) {
            throw new BridgeException("method " + name + "() timed out after 10s "
                    + "(likely waiting on a lock held by a suspended thread; avoid eval on synchronized methods)");
        }
        if (err[0] instanceof com.sun.jdi.InvocationException) {
            throw new BridgeException("method " + name + "() threw "
                    + ((com.sun.jdi.InvocationException) err[0]).exception().referenceType().name());
        }
        if (err[0] != null) {
            Exception e = err[0];
            if (e instanceof com.sun.jdi.InvalidTypeException || e instanceof com.sun.jdi.ClassNotLoadedException
                    || e instanceof com.sun.jdi.IncompatibleThreadStateException) {
                throw new BridgeException("cannot invoke " + name + "(): " + JdiBridge.shortMsg(e));
            }
            throw new BridgeException("cannot invoke " + name + "(): " + JdiBridge.shortMsg(e));
        }
        return out[0];
    }

    /** Decode a JSON string literal (quotes included) to raw text. */
}
