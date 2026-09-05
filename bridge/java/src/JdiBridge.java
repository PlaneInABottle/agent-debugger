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
 * Minimal JDI bridge for agent-debugger (Faz 1).
 *
 * <p>One-shot snapshot-on-breakpoint: attach to (or launch) a JVM, set
 * breakpoints (deferred via ClassPrepareRequest when the class is not loaded
 * yet), resume, wait for the first breakpoint hit, print a compact JSON
 * snapshot to stdout and disconnect. Zero dependencies, only
 * {@code com.sun.jdi} from the JDK.
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
            run(argv);
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

    // ---- CLI ----

    static class UsageException extends Exception {
        UsageException(String m) { super(m); }
    }

    static class BridgeException extends Exception {
        BridgeException(String m) { super(m); }
    }

    static class Config {
        String mode; // "attach" | "launch"
        String host = "localhost";
        int port = 5005;
        String mainClass;
        String classpath = ".";
        List<String> programArgs = new ArrayList<>();
        List<String> srcDirs = new ArrayList<>();
        Map<String, List<Integer>> breakpoints = new LinkedHashMap<>();
        Map<String, List<String>> methodBreaks = new LinkedHashMap<>();
        List<String> excFilters = new ArrayList<>();
        Map<String, String> condByLoc = new LinkedHashMap<>(); // "cls:line" -> condition
        List<Logpoint> logpoints = new ArrayList<>();
        List<Watchpoint> watchpoints = new ArrayList<>();
        Map<String, List<String>> exitMethods = new LinkedHashMap<>(); // cls -> methods
        long timeoutMs = 20_000;
        String sessionDir;
        String sessionKind; // "attach" | "launch" for session mode
    }

    static class Logpoint {
        String cls;
        int line;
        String template;
    }

    /** Field watch: stop when a field is read and/or written. */
    static class Watchpoint {
        String cls;
        String field;
        boolean onRead;
        boolean onWrite;
    }

    static void run(String[] argv) throws Exception {
        if (argv.length == 0) throw new UsageException("usage: JdiBridge attach|launch|session [options]");
        Config cfg = new Config();
        cfg.mode = argv[0];
        if (!cfg.mode.equals("attach") && !cfg.mode.equals("launch") && !cfg.mode.equals("session")) {
            throw new UsageException("first arg must be attach, launch or session");
        }
        boolean dashdash = false;
        for (int i = 1; i < argv.length; i++) {
            String a = argv[i];
            if (dashdash) { cfg.programArgs.add(a); continue; }
            switch (a) {
                case "--": dashdash = true; break;
                case "--host": cfg.host = next(argv, ++i, "--host"); break;
                case "--port": cfg.port = Integer.parseInt(next(argv, ++i, "--port")); break;
                case "--main": cfg.mainClass = next(argv, ++i, "--main"); break;
                case "--cp": case "--classpath": cfg.classpath = next(argv, ++i, a); break;
                case "--src": cfg.srcDirs.add(next(argv, ++i, "--src")); break;
                case "--break": parseBreakpoint(cfg, next(argv, ++i, "--break")); break;
                case "--logpoint": parseLogpoint(cfg, next(argv, ++i, "--logpoint")); break;
                case "--watch": parseWatchpoint(cfg, next(argv, ++i, "--watch")); break;
                case "--exit": parseExit(cfg, next(argv, ++i, "--exit")); break;
                case "--timeout": cfg.timeoutMs = Long.parseLong(next(argv, ++i, "--timeout")) * 1000; break;
                case "--dir": cfg.sessionDir = next(argv, ++i, "--dir"); break;
                case "--kind": cfg.sessionKind = next(argv, ++i, "--kind"); break;
                default: throw new UsageException("unknown arg: " + a);
            }
        }
        if (cfg.mode.equals("session")) {
            if (cfg.sessionDir == null) throw new UsageException("session needs --dir");
            if (cfg.sessionKind == null) throw new UsageException("session needs --kind attach|launch");
            // Empty sessions allowed: thread dumps and log collection need no stop.
            session(cfg);
            return;
        }
        requireAnyBreak(cfg);
        if (cfg.mode.equals("attach")) {
            attach(cfg);
        } else {
            if (cfg.mainClass == null) throw new UsageException("launch needs --main");
            launch(cfg);
        }
    }

    static void requireAnyBreak(Config cfg) throws UsageException {
        if (cfg.breakpoints.isEmpty() && cfg.methodBreaks.isEmpty() && cfg.excFilters.isEmpty()
                && cfg.logpoints.isEmpty() && cfg.watchpoints.isEmpty() && cfg.exitMethods.isEmpty()) {
            throw new UsageException("at least one stop is required "
                    + "(--break, --watch, --exit or --logpoint)");
        }
    }

    static boolean hasStoppingBreaks(Config cfg) {
        return !cfg.breakpoints.isEmpty() || !cfg.methodBreaks.isEmpty() || !cfg.excFilters.isEmpty()
                || !cfg.watchpoints.isEmpty() || !cfg.exitMethods.isEmpty();
    }

    /**
     * --watch com.Foo.count (write) | com.Foo.count:read | com.Foo.count:read,write.
     * Default is write: "who changed this?" beats "who looked at this?".
     */
    static void parseWatchpoint(Config cfg, String spec) throws UsageException {
        String mode = "write";
        int colon = spec.lastIndexOf(':');
        if (colon > 0 && (spec.endsWith(":read") || spec.endsWith(":write") || spec.endsWith(":read,write"))) {
            mode = spec.substring(colon + 1);
            spec = spec.substring(0, colon);
        }
        int dot = spec.lastIndexOf('.');
        if (dot <= 0) throw new UsageException("--watch must look like com.Foo.field[:read|:write], got: " + spec);
        Watchpoint w = new Watchpoint();
        w.cls = spec.substring(0, dot).replace('/', '.');
        w.field = spec.substring(dot + 1);
        if (w.field.isEmpty()) throw new UsageException("--watch needs a field name: " + spec);
        w.onRead = mode.contains("read");
        w.onWrite = mode.contains("write");
        cfg.watchpoints.add(w);
    }

    /** --exit com.Foo.bar: stop at method exit, capturing the return value. */
    static void parseExit(Config cfg, String spec) throws UsageException {
        String rest = spec.replace('/', '.');
        int dot = rest.lastIndexOf('.');
        if (dot <= 0) throw new UsageException("--exit must look like com.Foo.bar, got: " + spec);
        String cls = rest.substring(0, dot);
        String method = rest.substring(dot + 1);
        if (method.isEmpty()) throw new UsageException("--exit must look like com.Foo.bar, got: " + spec);
        cfg.exitMethods.computeIfAbsent(cls, k -> new ArrayList<>()).add(method);
    }

    static String next(String[] argv, int i, String flag) throws UsageException {
        if (i >= argv.length) throw new UsageException(flag + " needs a value");
        return argv[i];
    }

    static void parseBreakpoint(Config cfg, String spec) throws UsageException {
        // Forms: com.Foo:30 | com.Foo:30|x == null | method:com.Foo.bar
        //        method:com.Foo.bar|x != null | exc:java.lang.NullPointerException
        String cond = null;
        int bar = spec.indexOf('|');
        if (bar >= 0) {
            cond = spec.substring(bar + 1).trim();
            spec = spec.substring(0, bar);
            validateCond(cond);
        }
        if (spec.startsWith("exc:")) {
            String f = spec.substring(4).replace('/', '.');
            if (f.isEmpty()) throw new UsageException("--break exc: needs an exception class");
            cfg.excFilters.add(f);
            return;
        }
        if (spec.startsWith("method:")) {
            String rest = spec.substring(7).replace('/', '.');
            int dot = rest.lastIndexOf('.');
            if (dot <= 0) throw new UsageException("--break must look like method:com.Foo.bar, got: " + spec);
            String cls = rest.substring(0, dot);
            String method = rest.substring(dot + 1);
            if (method.isEmpty()) throw new UsageException("--break must look like method:com.Foo.bar, got: " + spec);
            cfg.methodBreaks.computeIfAbsent(cls, k -> new ArrayList<>()).add(method);
            if (cond != null) cfg.condByLoc.put("method:" + cls + "." + method, cond);
            return;
        }
        int colon = spec.lastIndexOf(':');
        if (colon <= 0) throw new UsageException("--break must look like com.example.Hello:30, got: " + spec);
        String cls = spec.substring(0, colon).replace('/', '.');
        if (cls.endsWith(".java")) cls = cls.substring(0, cls.length() - 5).replace('/', '.');
        int line;
        try {
            line = Integer.parseInt(spec.substring(colon + 1));
        } catch (NumberFormatException e) {
            throw new UsageException("bad line in --break: " + spec);
        }
        cfg.breakpoints.computeIfAbsent(cls, k -> new ArrayList<>()).add(line);
        if (cond != null) cfg.condByLoc.put(cls + ":" + line, cond);
    }

    /**
     * Restricted condition language: {@code <path> <op> <literal|null>},
     * op in ==, !=, >, <, >=, <=. Paths are eval paths WITHOUT method calls
     * (a mutating call evaluated 1000x per loop would corrupt state).
     */
    static void validateCond(String cond) throws UsageException {
        if (cond == null || cond.isEmpty()) throw new UsageException("empty condition after '|'");
        String[] ops = {"==", "!=", ">=", "<=", ">", "<"};
        boolean found = false;
        for (String op : ops) {
            if (cond.contains(op)) { found = true; break; }
        }
        if (!found) throw new UsageException("condition needs ==, !=, >, <, >= or <= : " + cond);
        // Calls: only the read-only allowlist with literal args. Anything else
        // could mutate target state on every loop iteration.
        int i = 0;
        while ((i = cond.indexOf('(', i)) >= 0) {
            int j = i - 1;
            while (j >= 0 && Character.isJavaIdentifierPart(cond.charAt(j))) j--;
            String name = cond.substring(j + 1, i).trim();
            if (!COND_CALLS.contains(name)) {
                throw new UsageException("conditions cannot call " + name + "() (side-effect risk): " + cond);
            }
            int depth = 1;
            int k = i + 1;
            boolean inStr = false;
            while (k < cond.length() && depth > 0) {
                char c = cond.charAt(k);
                if (c == '"' && cond.charAt(k - 1) != '\\') inStr = !inStr;
                if (!inStr) {
                    if (c == '(') depth++;
                    else if (c == ')') depth--;
                }
                k++;
            }
            if (depth != 0) throw new UsageException("unbalanced ( in condition: " + cond);
            String argText = cond.substring(i + 1, k - 1).trim();
            validateCondArgs(name, argText, cond);
            i = k;
        }
    }

    static void validateCondArgs(String name, String argText, String cond) throws UsageException {
        if (name.equals("get")) {
            if (!argText.matches("-?\\d+")) {
                throw new UsageException("get() in conditions needs one int literal: " + cond);
            }
            return;
        }
        if (!argText.isEmpty()) {
            throw new UsageException(name + "() in conditions takes no arguments: " + cond);
        }
    }

    static void parseLogpoint(Config cfg, String spec) throws UsageException {
        // Form: com.Foo:54:order total={total}
        int c1 = spec.indexOf(':');
        if (c1 <= 0) throw new UsageException("--logpoint must look like Class:line:template, got: " + spec);
        int c2 = spec.indexOf(':', c1 + 1);
        if (c2 <= 0) throw new UsageException("--logpoint must look like Class:line:template, got: " + spec);
        String cls = spec.substring(0, c1).replace('/', '.');
        int line;
        try {
            line = Integer.parseInt(spec.substring(c1 + 1, c2));
        } catch (NumberFormatException e) {
            throw new UsageException("bad line in --logpoint: " + spec);
        }
        String template = spec.substring(c2 + 1);
        if (template.isEmpty()) throw new UsageException("--logpoint template is empty: " + spec);
        // Holes must be call-free paths (evaluated on every hit, possibly 1000s).
        int i = 0;
        while ((i = template.indexOf('{', i)) >= 0) {
            int j = template.indexOf('}', i);
            if (j < 0) throw new UsageException("unbalanced { in --logpoint template");
            if (template.substring(i + 1, j).contains("(")) {
                throw new UsageException("logpoint holes cannot call methods: " + template);
            }
            i = j + 1;
        }
        Logpoint lp = new Logpoint();
        lp.cls = cls;
        lp.line = line;
        lp.template = template;
        cfg.logpoints.add(lp);
    }

    // ---- connect ----

    static void attach(Config cfg) throws Exception {
        VirtualMachine vm = attachVm(cfg);
        try {
            driveToSnapshot(vm, cfg, null);
        } finally {
            try { vm.dispose(); } catch (Exception ignored) {}
        }
    }

    static VirtualMachine attachVm(Config cfg) throws Exception {
        AttachingConnector connector = null;
        for (AttachingConnector c : Bootstrap.virtualMachineManager().attachingConnectors()) {
            if (c.name().equals("com.sun.jdi.SocketAttach")) { connector = c; break; }
        }
        if (connector == null) throw new BridgeException("no SocketAttach connector");
        Map<String, Connector.Argument> args = connector.defaultArguments();
        args.get("hostname").setValue(cfg.host);
        args.get("port").setValue(String.valueOf(cfg.port));
        VirtualMachine vm;
        try {
            vm = connector.attach(args);
        } catch (Exception e) {
            throw new BridgeException("attach failed (" + cfg.host + ":" + cfg.port + "): " + shortMsg(e)
                    + " — is the target started with -agentlib:jdwp=transport=dt_socket,server=y,address=*:" + cfg.port + " ?");
        }
        return vm;
    }

    static class Launched {
        VirtualMachine vm;
        StreamGobbler out;
    }

    static void launch(Config cfg) throws Exception {
        if (cfg.mainClass == null) throw new UsageException("launch needs --main");
        Launched l = launchVm(cfg);
        try {
            driveToSnapshot(l.vm, cfg, l.out);
        } finally {
            try { l.vm.exit(0); } catch (Exception ignored) {}
        }
    }

    static Launched launchVm(Config cfg) throws Exception {
        LaunchingConnector connector = Bootstrap.virtualMachineManager().defaultConnector();
        Map<String, Connector.Argument> args = connector.defaultArguments();
        StringBuilder main = new StringBuilder(cfg.mainClass);
        for (String pa : cfg.programArgs) main.append(' ').append(pa);
        args.get("main").setValue(main.toString());
        args.get("options").setValue("-cp " + cfg.classpath);
        args.get("suspend").setValue("true");
        VirtualMachine vm;
        try {
            vm = connector.launch(args);
        } catch (Exception e) {
            throw new BridgeException("launch failed: " + shortMsg(e));
        }
        StreamGobbler out = new StreamGobbler(vm.process().getInputStream());
        StreamGobbler err = new StreamGobbler(vm.process().getErrorStream());
        out.start();
        err.start();
        Launched l = new Launched();
        l.vm = vm;
        l.out = out;
        return l;
    }

    static String shortMsg(Exception e) {
        String m = e.getMessage();
        if (m == null || m.isEmpty()) return e.getClass().getSimpleName();
        if (m.length() > 160) m = m.substring(0, 160) + "…";
        return e.getClass().getSimpleName() + ": " + m;
    }

    // ---- event loop ----

    static void driveToSnapshot(VirtualMachine vm, Config cfg, StreamGobbler out) throws Exception {
        armBreakpoints(vm, cfg);
        List<String> logs = new ArrayList<>();
        java.util.Set<String> planted = new java.util.HashSet<>();

        vm.resume();
        long deadline = System.currentTimeMillis() + cfg.timeoutMs;
        while (true) {
            long remaining = deadline - System.currentTimeMillis();
            if (remaining <= 0) {
                throw new BridgeException("timeout: no breakpoint hit within "
                        + (cfg.timeoutMs / 1000) + "s (breakpoints: " + describeBreaks(cfg) + ")");
            }
            EventSet set;
            try {
                set = vm.eventQueue().remove(Math.min(remaining, 1000));
            } catch (InterruptedException ie) {
                continue;
            } catch (Exception e) {
                throw new BridgeException("lost connection to target VM: " + shortMsg(e));
            }
            if (set == null) continue;
            boolean done = false;
            String stopInfo = null;
            for (Event event : set) {
                if (event instanceof BreakpointEvent) {
                    BreakpointEvent bp = (BreakpointEvent) event;
                    fireLogpoints(null, logs, null, cfg, bp.thread(), bp.location());
                    if (!hasStoppingBreak(cfg, bp.location())) continue;
                    String cond = lookupCond(cfg, bp.location());
                    if (cond != null && !checkCond(bp.thread(), bp.location(), cond)) continue;
                    System.out.println("{\"snapshot\":" + snapshot(vm, cfg, bp.thread(), bp.location(), out)
                            + ",\"logs\":" + toJsonArray(logs) + ",\"stopInfo\":null}");
                    done = true;
                } else if (event instanceof ClassPrepareEvent) {
                    ClassPrepareEvent cp = (ClassPrepareEvent) event;
                    try { cp.request().disable(); } catch (Exception ignored) {}
                    try {
                        plantPending(vm, cfg, cp.referenceType(), planted);
                    } catch (Exception e) {
                        try { set.resume(); } catch (Exception ignored) {}
                        throw e;
                    }
                } else if (event instanceof com.sun.jdi.event.ExceptionEvent) {
                    com.sun.jdi.event.ExceptionEvent ee = (com.sun.jdi.event.ExceptionEvent) event;
                    if (!matchesExcFilter(cfg, ee)) continue;
                    fireLogpoints(null, logs, null, cfg, ee.thread(), ee.location());
                    String cond = lookupCond(cfg, ee.location());
                    if (cond != null && !checkCond(ee.thread(), ee.location(), cond)) continue;
                    stopInfo = exceptionInfo(ee);
                    System.out.println("{\"snapshot\":" + snapshot(vm, cfg, ee.thread(), ee.location(), out)
                            + ",\"logs\":" + toJsonArray(logs) + ",\"stopInfo\":" + stopInfo + "}");
                    done = true;
                } else if (event instanceof com.sun.jdi.event.ModificationWatchpointEvent) {
                    com.sun.jdi.event.ModificationWatchpointEvent we =
                            (com.sun.jdi.event.ModificationWatchpointEvent) event;
                    stopInfo = watchInfo(we.field(), "write", we.valueToBe());
                    System.out.println("{\"snapshot\":" + snapshot(vm, cfg, we.thread(), we.location(), out)
                            + ",\"logs\":" + toJsonArray(logs) + ",\"stopInfo\":" + stopInfo + "}");
                    done = true;
                } else if (event instanceof com.sun.jdi.event.AccessWatchpointEvent) {
                    com.sun.jdi.event.AccessWatchpointEvent we =
                            (com.sun.jdi.event.AccessWatchpointEvent) event;
                    stopInfo = watchInfo(we.field(), "read", we.valueCurrent());
                    System.out.println("{\"snapshot\":" + snapshot(vm, cfg, we.thread(), we.location(), out)
                            + ",\"logs\":" + toJsonArray(logs) + ",\"stopInfo\":" + stopInfo + "}");
                    done = true;
                } else if (event instanceof com.sun.jdi.event.MethodExitEvent) {
                    com.sun.jdi.event.MethodExitEvent me = (com.sun.jdi.event.MethodExitEvent) event;
                    if (!wantedExit(cfg, me)) continue;
                    stopInfo = exitInfo(me);
                    System.out.println("{\"snapshot\":" + snapshot(vm, cfg, me.thread(), me.location(), out)
                            + ",\"logs\":" + toJsonArray(logs) + ",\"stopInfo\":" + stopInfo + "}");
                    done = true;
                } else if (event instanceof VMDeathEvent || event instanceof VMDisconnectEvent) {
                    throw new BridgeException("target VM exited before any breakpoint hit");
                }
            }
            // Resume unless we just snapshotted (threads stay suspended for a coherent read above).
            if (!done) {
                set.resume();
            } else {
                return;
            }
        }
    }

    static void setLines(VirtualMachine vm, ReferenceType rt, List<Integer> lines)
            throws AbsentInformationException, BridgeException {
        setLines(vm, rt, lines, EventRequest.SUSPEND_ALL, false);
    }

    static void setLines(VirtualMachine vm, ReferenceType rt, List<Integer> lines, int policy)
            throws AbsentInformationException, BridgeException {
        setLines(vm, rt, lines, policy, false);
    }

    /**
     * @param singleLoc for logpoints: one location per line. A line can map to
     * several bytecode locations (branches); planting on all of them logs the
     * same pass twice. Stopping breakpoints keep all locations (first hit wins).
     */
    static void setLines(VirtualMachine vm, ReferenceType rt, List<Integer> lines, int policy,
            boolean singleLoc) throws AbsentInformationException, BridgeException {
        for (int line : lines) {
            List<Location> locs;
            try {
                locs = rt.locationsOfLine(line);
            } catch (AbsentInformationException aie) {
                throw new BridgeException("class " + rt.name()
                        + " has no line info — recompile with -g");
            }
            if (locs.isEmpty()) {
                throw new BridgeException("no executable code at " + rt.name() + ":" + line);
            }
            for (Location loc : locs) {
                BreakpointRequest bp = vm.eventRequestManager().createBreakpointRequest(loc);
                bp.setSuspendPolicy(policy);
                bp.enable();
                if (singleLoc) break;
            }
        }
    }

    // ---- snapshot ----

    static String snapshot(VirtualMachine vm, Config cfg, ThreadReference thread,
            Location loc, StreamGobbler out) throws Exception {
        StringBuilder sb = new StringBuilder(4096);
        sb.append('{');
        kv(sb, "mode", cfg.mode, true);
        sb.append(",\"location\":").append(locationJson(loc, cfg));
        sb.append(",\"threads\":").append(threadsJson(vm, thread));
        sb.append(",\"frames\":").append(framesJson(thread, true));
        if (out != null) sb.append(",\"output\":").append(quote(out.tail()));
        sb.append('}');
        return sb.toString();
    }

    static String locationJson(Location loc, Config cfg) {
        String cls = "?";
        String method = "?";
        int line = -1;
        try { cls = loc.declaringType().name(); } catch (Exception ignored) {}
        try { method = loc.method().name(); } catch (Exception ignored) {}
        try { line = loc.lineNumber(); } catch (Exception ignored) {}
        String file = sourcePath(cls);
        return "{\"class\":" + quote(cls) + ",\"method\":" + quote(method)
                + ",\"line\":" + line + ",\"file\":" + quote(file)
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
            kv(sb, "name", t.name(), true);
            sb.append(",\"status\":").append(quote(threadStatus(t.status()))).append(',');
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
                    + quote("+" + (ordered.size() - cap) + " more threads") + "}");
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
        List<StackFrame> frames = safeFrames(thread);
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < Math.min(frames.size(), MAX_FRAMES); i++) {
            if (i > 0) sb.append(',');
            StackFrame f = frames.get(i);
            sb.append("{\"index\":").append(i).append(',');
            String ftype = "?";
            String mname = "?";
            int fline = -1;
            try { ftype = f.location().declaringType().name(); } catch (Exception ignored) {}
            try { mname = f.location().method().name(); } catch (Exception ignored) {}
            try { fline = f.location().lineNumber(); } catch (Exception ignored) {}
            kv(sb, "type", ftype, true);
            sb.append(',');
            kv(sb, "method", mname, true);
            sb.append(",\"line\":").append(fline);
            if (withLocals && i == 0) sb.append(",\"locals\":").append(localsJson(f));
            sb.append('}');
        }
        return sb.append(']').toString();
    }

    static String localsJson(StackFrame f) {
        StringBuilder sb = new StringBuilder("[");
        try {
            List<LocalVariable> vars = f.visibleVariables();
            Map<LocalVariable, Value> vals = f.getValues(vars);
            // Deterministic order: agents diff snapshots, HashMap order would lie.
            List<Map.Entry<LocalVariable, Value>> entries = new ArrayList<>(vals.entrySet());
            entries.sort(Comparator.comparing(e -> e.getKey().name()));
            int n = 0;
            for (Map.Entry<LocalVariable, Value> ve : entries) {
                if (n >= MAX_VARS) {
                    if (n == MAX_VARS) sb.append(",{\"name\":\"…\",\"note\":"
                            + quote("+" + (vals.size() - MAX_VARS) + " more") + "}");
                    n++;
                    continue;
                }
                if (n > 0) sb.append(',');
                sb.append("{\"name\":").append(quote(ve.getKey().name())).append(',');
                sb.append("\"type\":").append(quote(ve.getKey().typeName())).append(',');
                sb.append("\"value\":").append(quote(formatValue(ve.getValue(), 1)));
                sb.append('}');
                n++;
            }
        } catch (AbsentInformationException aie) {
            sb.append("{\"name\":\"…\",\"note\":\"no debug info (-g)\"}");
        } catch (Exception e) {
            sb.append("{\"name\":\"…\",\"note\":").append(quote(shortMsg(e))).append('}');
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
                    sb.append("\"text\":").append(quote(lines.get(n - 1))).append('}');
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
            if (s.length() > MAX_STRING) s = s.substring(0, MAX_STRING) + "… (+" + (s.length() - MAX_STRING) + " more chars)";
            return "\"" + s.replace("\"", "\\\"") + "\"";
        }
        if (v instanceof ArrayReference) {
            ArrayReference arr = (ArrayReference) v;
            int len = arr.length();
            StringBuilder sb = new StringBuilder(arr.type().name())
                    .append('[').append(len).append("]{");
            int show = Math.min(len, MAX_ITEMS);
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
                    if (n >= MAX_FIELDS) break;
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

    // ---- session server (persistent VM connection over TCP) ----
    //
    // Protocol: DAP-style framing (Content-Length + JSON) on 127.0.0.1.
    // One request per connection: {"cmd":"step","mode":"over","timeout":20}
    // Response: {"ok":true,...} or {"ok":false,"error":"..."}.
    // Commands: step{mode}, continue, context, stack, vars{frame},
    //           eval{expr,frame}, close.

    static class SessionState {
        Config cfg;
        VirtualMachine vm;
        StreamGobbler out;
        ServerSocket server;
        ThreadReference thread;
        Location location;
        boolean suspended; // true between stops, false while target runs
        boolean exited;
        Map<String, String> lastTop; // top-frame locals at previous stop
        String lastChanged = "[]"; // JSON array of new/changed local names
        String stopInfo; // JSON object describing WHY we stopped (watch/exit/exception)
        java.util.Set<String> planted = new java.util.HashSet<>(); // classes already planted
        Path dir; // session dir (logs.jsonl lives here)
        int logCount;
        String ownerNonce; // session ownership token (see amOwner)
        String lastStopJson; // pre-rendered {"file","line","method"}, null until first stop
        final Object queueLock = new Object(); // guards waiterActive
        boolean waiterActive; // a continue/step owns the event queue right now
    }

    static void session(Config cfg) throws Exception {
        Path dir = Paths.get(cfg.sessionDir);
        Files.createDirectories(dir);
        ServerSocket server = new ServerSocket(0, 5, InetAddress.getByName("127.0.0.1"));
        // Idle accept gets a 1s timeout so rm -rf abandonment is noticed even
        // with zero traffic (blocking accept would orphan forever).
        server.setSoTimeout(1000);
        SessionState st = new SessionState();
        st.cfg = cfg;
        st.server = server;
        st.dir = dir;
        // Claim the session dir first thing: if the owner deletes it (rm -rf
        // instead of close) or respawns under our name, our nonce mismatches
        // and we quit quietly instead of orphaning. Same contract as the
        // node/python/browser bridges' owner.json.
        st.ownerNonce = ProcessHandle.current().pid() + "-"
                + System.currentTimeMillis() + "-" + new java.util.Random().nextInt(1000000000);
        writeFile(dir.resolve("owner.json"),
                "{\"pid\":" + ProcessHandle.current().pid()
                + ",\"nonce\":" + quote(st.ownerNonce) + "}");
        try {
            if (cfg.sessionKind.equals("attach")) {
                st.vm = attachVm(cfg);
            } else if (cfg.sessionKind.equals("launch")) {
                if (cfg.mainClass == null) throw new UsageException("launch needs --main");
                Launched l = launchVm(cfg);
                st.vm = l.vm;
                st.out = l.out;
            } else {
                throw new UsageException("--kind must be attach or launch");
            }
            armBreakpoints(st.vm, cfg);
            st.vm.resume();
            st.suspended = false;
            if (hasStoppingBreaks(cfg)) {
                // First stop, synchronously: CLI polls session.json for readiness.
                awaitStop(st, cfg.timeoutMs);
            } else if (!cfg.logpoints.isEmpty()) {
                // Nobody will ever stop: drain logpoints in the background so
                // `logs` keeps working while the session idles in serveLoop.
                Thread drainer = new Thread(() -> drainLoop(st));
                drainer.setDaemon(true);
                drainer.start();
            }
            // else: unstopped session (pure log collection / thread dumps).
            // Commands needing a stop fail gracefully until one arrives.
            publishState(st, hasStoppingBreaks(cfg));
            serveLoop(st, dir);
        } catch (UsageException | BridgeException e) {
            writeFile(dir.resolve("error.json"), "{\"error\":" + quote(e.getMessage()) + "}");
            throw e;
        } finally {
            try { server.close(); } catch (Exception ignored) {}
        }
    }

    static void writeFile(Path p, String content) {
        try {
            Files.write(p, content.getBytes(StandardCharsets.UTF_8));
        } catch (Exception ignored) {}
    }

    /** Arm all configured breakpoints; deferred ones via ClassPrepareRequest. */
    static void armBreakpoints(VirtualMachine vm, Config cfg) throws Exception {
        for (Map.Entry<String, List<Integer>> e : cfg.breakpoints.entrySet()) {
            List<ReferenceType> loaded = vm.classesByName(e.getKey());
            if (!loaded.isEmpty()) {
                for (ReferenceType rt : loaded) setLines(vm, rt, e.getValue());
            } else {
                watchClass(vm, e.getKey());
            }
        }
        for (Map.Entry<String, List<String>> e : cfg.methodBreaks.entrySet()) {
            List<ReferenceType> loaded = vm.classesByName(e.getKey());
            if (!loaded.isEmpty()) {
                for (ReferenceType rt : loaded) setMethods(vm, rt, e.getValue());
            } else {
                watchClass(vm, e.getKey());
            }
        }
        if (!cfg.excFilters.isEmpty()) {
            com.sun.jdi.request.ExceptionRequest req = vm.eventRequestManager()
                    .createExceptionRequest(null, false, true);
            req.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            req.enable();
        }
        for (Watchpoint w : cfg.watchpoints) {
            List<ReferenceType> loaded = vm.classesByName(w.cls);
            if (!loaded.isEmpty()) {
                for (ReferenceType rt : loaded) armWatch(vm, rt, w);
            } else {
                watchClass(vm, w.cls);
            }
        }
        if (!cfg.exitMethods.isEmpty()) {
            if (!vm.canGetMethodReturnValues()) {
                throw new BridgeException("target VM cannot provide method return values");
            }
            for (String cls : cfg.exitMethods.keySet()) {
                com.sun.jdi.request.MethodExitRequest req =
                        vm.eventRequestManager().createMethodExitRequest();
                req.addClassFilter(cls);
                req.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                req.enable();
            }
        }
        java.util.Set<String> watched = new java.util.HashSet<>();
        for (Logpoint lp : cfg.logpoints) {
            List<ReferenceType> loaded = vm.classesByName(lp.cls);
            if (!loaded.isEmpty()) {
                for (ReferenceType rt : loaded) setLines(vm, rt, java.util.Collections.singletonList(lp.line),
                        EventRequest.SUSPEND_EVENT_THREAD, true);
            } else if (watched.add(lp.cls)) {
                watchClass(vm, lp.cls);
            }
        }
    }

    static void watchClass(VirtualMachine vm, String cls) {
        ClassPrepareRequest req = vm.eventRequestManager().createClassPrepareRequest();
        req.addClassFilter(cls);
        req.setSuspendPolicy(EventRequest.SUSPEND_ALL);
        req.enable();
    }

    /** Plant deferred line/method breakpoints when a class finishes loading. */
    static void plantPending(VirtualMachine vm, Config cfg, ReferenceType rt,
            java.util.Set<String> planted) throws Exception {
        // Each class may carry several ClassPrepareRequests (line breaks,
        // method breaks, logpoints arm separately) -> several events per load.
        // Plant exactly once.
        if (!planted.add(rt.name())) return;
        List<Integer> lines = cfg.breakpoints.get(rt.name());
        if (lines != null) {
            try {
                setLines(vm, rt, lines);
            } catch (AbsentInformationException aie) {
                throw new BridgeException("class " + rt.name() + " has no debug info — recompile with -g");
            }
        }
        List<String> methods = cfg.methodBreaks.get(rt.name());
        if (methods != null) setMethods(vm, rt, methods);
        for (Watchpoint w : cfg.watchpoints) {
            if (w.cls.equals(rt.name())) armWatch(vm, rt, w);
        }
        for (Logpoint lp : cfg.logpoints) {
            if (lp.cls.equals(rt.name())) {
                setLines(vm, rt, java.util.Collections.singletonList(lp.line),
                        EventRequest.SUSPEND_EVENT_THREAD, true);
            }
        }
    }

    static void armWatch(VirtualMachine vm, ReferenceType rt, Watchpoint w) throws BridgeException {
        List<Field> fields = new ArrayList<>();
        for (Field f : rt.allFields()) {
            if (f.name().equals(w.field)) fields.add(f);
        }
        if (fields.isEmpty()) throw new BridgeException("no field " + w.field + " in " + rt.name());
        for (Field f : fields) {
            try {
                if (w.onWrite) {
                    com.sun.jdi.request.ModificationWatchpointRequest req = vm.eventRequestManager()
                            .createModificationWatchpointRequest(f);
                    req.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                    req.enable();
                }
                if (w.onRead) {
                    com.sun.jdi.request.AccessWatchpointRequest req = vm.eventRequestManager()
                            .createAccessWatchpointRequest(f);
                    req.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                    req.enable();
                }
            } catch (UnsupportedOperationException e) {
                throw new BridgeException("target VM cannot watch field " + w.field);
            }
        }
    }

    static void setMethods(VirtualMachine vm, ReferenceType rt, List<String> methods) throws BridgeException {
        for (String name : methods) {
            List<com.sun.jdi.Method> found = rt.methodsByName(name);
            if (found.isEmpty()) {
                throw new BridgeException("no method " + name + "() in " + rt.name());
            }
            for (com.sun.jdi.Method m : found) {
                if (m.isNative() || m.isAbstract()) continue;
                Location loc = m.location();
                if (loc == null || loc.codeIndex() < 0) {
                    throw new BridgeException("method " + name + "() in " + rt.name()
                            + " has no code — recompile with -g");
                }
                BreakpointRequest bp = vm.eventRequestManager().createBreakpointRequest(loc);
                bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                bp.enable();
            }
        }
    }

    static boolean matchesExcFilter(Config cfg, com.sun.jdi.event.ExceptionEvent ee) {
        if (cfg.excFilters.isEmpty()) return false;
        String actual;
        try {
            actual = ee.exception().referenceType().name();
        } catch (Exception e) {
            return false;
        }
        for (String f : cfg.excFilters) {
            if (actual.equals(f) || actual.endsWith("." + f)) return true;
        }
        return false;
    }

    // ---- conditions: <path> <op> <literal|null>, evaluated bridge-side ----
    // No compiler needed: conditions reuse the read-only path resolver, and a
    // non-matching hit auto-resumes inside the bridge (zero LLM roundtrips).
    // Read-only allowlist for calls inside conditions (a mutating call run on
    // every loop iteration would corrupt state silently).

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
            List<StackFrame> frames = safeFrames(thread);
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
            sb.append(formatValue(resolvePath(thread, frame, hole, COND_CALLS), 0));
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
        if (!(v instanceof ObjectReference)) return path + " is not an object (" + formatValue(v, 1) + ")";
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
        StringBuilder sb = new StringBuilder("target: ").append(shallow(target));
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
            StringBuilder chain = new StringBuilder(shallow(byId.get(id)));
            long walk = id;
            while (parent.containsKey(walk) && parent.get(walk) != targetId) {
                walk = parent.get(walk);
                chain.append(" <- ").append(shallow(byId.get(walk)));
            }
            chain.append(" <- ").append(shallow(target));
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
            kv(sb, "name", t.name(), true);
            sb.append(",\"status\":").append(quote(threadStatus(t.status())));
            sb.append(",\"frames\":").append(framesJson(t, false));
            sb.append('}');
        }
        if (all.size() > cap) {
            sb.append(",{\"id\":-1,\"name\":\"…\",\"note\":"
                    + quote("+" + (all.size() - cap) + " more threads") + "}");
        }
        return sb.append(']').toString();
    }

    static String watchInfo(Field f, String access, Value v) {
        String owner = "?";
        try { owner = f.declaringType().name(); } catch (Exception ignored) {}
        String val;
        try {
            val = formatValue(v, 1);
        } catch (Exception e) {
            val = "?";
        }
        return "{\"watch\":{\"field\":" + quote(owner + "." + f.name())
                + ",\"access\":" + quote(access) + ",\"value\":" + quote(val) + "}}";
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
            ret = formatValue(me.returnValue(), 1);
        } catch (UnsupportedOperationException e) {
            ret = "unavailable";
        } catch (Exception e) {
            ret = "?";
        }
        return "{\"exit\":{\"method\":" + quote(m) + ",\"returns\":" + quote(ret) + "}}";
    }

    static String exceptionInfo(com.sun.jdi.event.ExceptionEvent ee) {
        String name = "?";
        try { name = ee.exception().referenceType().name(); } catch (Exception ignored) {}
        return "{\"exception\":{\"class\":" + quote(name) + "}}";
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
    static void trackChanges(SessionState st) {
        try {
            List<StackFrame> frames = safeFrames(st.thread);
            Map<String, String> cur = new LinkedHashMap<>();
            if (!frames.isEmpty()) {
                StackFrame f = frames.get(0);
                try {
                    List<LocalVariable> vars = f.visibleVariables();
                    for (Map.Entry<LocalVariable, Value> ve : f.getValues(vars).entrySet()) {
                        cur.put(ve.getKey().name(), formatValue(ve.getValue(), 1));
                    }
                } catch (AbsentInformationException ignored) {}
            }
            StringBuilder sb = new StringBuilder("[");
            boolean first = true;
            if (st.lastTop == null) {
                for (String name : cur.keySet()) {
                    if (!first) sb.append(',');
                    first = false;
                    sb.append(quote(name));
                }
            } else {
                for (Map.Entry<String, String> e : cur.entrySet()) {
                    String old = st.lastTop.get(e.getKey());
                    if (!e.getValue().equals(old)) {
                        if (!first) sb.append(',');
                        first = false;
                        sb.append(quote(e.getKey()));
                    }
                }
            }
            st.lastChanged = sb.append(']').toString();
            st.lastTop = cur;
        } catch (Exception e) {
            st.lastChanged = "[]";
        }
    }

    /**
     * Wait for the next stop (breakpoint or step end). Updates st.thread /
     * st.location and returns the stop snapshot. Throws on timeout or VM exit.
     * Takes queue ownership from the background drainer while waiting.
     */
    static String awaitStop(SessionState st, long timeoutMs) throws Exception {
        synchronized (st.queueLock) {
            st.waiterActive = true;
        }
        try {
            return awaitStopInner(st, timeoutMs);
        } finally {
            synchronized (st.queueLock) {
                st.waiterActive = false;
                st.queueLock.notifyAll();
            }
        }
    }

    static String awaitStopInner(SessionState st, long timeoutMs) throws Exception {
        VirtualMachine vm = st.vm;
        long deadline = System.currentTimeMillis() + timeoutMs;
        while (true) {
            // Same abandonment guard as serveLoop (1s event windows bound it).
            if (!amOwner(st)) {
                cleanup(st);
                System.exit(0);
            }
            long remaining = deadline - System.currentTimeMillis();
            if (remaining <= 0) throw new BridgeException("timeout: no stop within " + (timeoutMs / 1000) + "s");
            EventSet set;
            try {
                set = vm.eventQueue().remove(Math.min(remaining, 1000));
            } catch (InterruptedException ie) {
                continue;
            } catch (Exception e) {
                st.exited = true;
                publishState(st, false);
                throw new BridgeException("lost connection to target VM: " + shortMsg(e));
            }
            if (set == null) continue;
            String stop = null;
            for (Event event : set) {
                if (event instanceof BreakpointEvent) {
                    BreakpointEvent bp = (BreakpointEvent) event;
                    fireLogpoints(st, null, st.dir, st.cfg, bp.thread(), bp.location());
                    if (!hasStoppingBreak(st.cfg, bp.location())) continue;
                    String cond = lookupCond(st.cfg, bp.location());
                    if (cond != null && !checkCond(bp.thread(), bp.location(), cond)) continue;
                    st.thread = bp.thread();
                    st.location = bp.location();
                    st.suspended = true;
                    st.stopInfo = null; // plain stop supersedes any previous reason
                    trackChanges(st);
                    stop = snapshot(vm, st.cfg, st.thread, st.location, st.out);
                } else if (event instanceof com.sun.jdi.event.StepEvent) {
                    com.sun.jdi.event.StepEvent se = (com.sun.jdi.event.StepEvent) event;
                    st.thread = se.thread();
                    st.location = se.location();
                    st.suspended = true;
                    st.stopInfo = null;
                    trackChanges(st);
                    stop = snapshot(vm, st.cfg, st.thread, st.location, st.out);
                } else if (event instanceof com.sun.jdi.event.ExceptionEvent) {
                    com.sun.jdi.event.ExceptionEvent ee = (com.sun.jdi.event.ExceptionEvent) event;
                    if (!matchesExcFilter(st.cfg, ee)) continue;
                    fireLogpoints(st, null, st.dir, st.cfg, ee.thread(), ee.location());
                    String cond = lookupCond(st.cfg, ee.location());
                    if (cond != null && !checkCond(ee.thread(), ee.location(), cond)) continue;
                    st.thread = ee.thread();
                    st.location = ee.location();
                    st.suspended = true;
                    st.stopInfo = exceptionInfo(ee);
                    trackChanges(st);
                    stop = snapshot(vm, st.cfg, st.thread, st.location, st.out);
                } else if (event instanceof com.sun.jdi.event.ModificationWatchpointEvent) {
                    com.sun.jdi.event.ModificationWatchpointEvent we =
                            (com.sun.jdi.event.ModificationWatchpointEvent) event;
                    st.thread = we.thread();
                    st.location = we.location();
                    st.suspended = true;
                    st.stopInfo = watchInfo(we.field(), "write", we.valueToBe());
                    trackChanges(st);
                    stop = snapshot(vm, st.cfg, st.thread, st.location, st.out);
                } else if (event instanceof com.sun.jdi.event.AccessWatchpointEvent) {
                    com.sun.jdi.event.AccessWatchpointEvent we =
                            (com.sun.jdi.event.AccessWatchpointEvent) event;
                    st.thread = we.thread();
                    st.location = we.location();
                    st.suspended = true;
                    st.stopInfo = watchInfo(we.field(), "read", we.valueCurrent());
                    trackChanges(st);
                    stop = snapshot(vm, st.cfg, st.thread, st.location, st.out);
                } else if (event instanceof com.sun.jdi.event.MethodExitEvent) {
                    com.sun.jdi.event.MethodExitEvent me = (com.sun.jdi.event.MethodExitEvent) event;
                    if (!wantedExit(st.cfg, me)) continue;
                    st.thread = me.thread();
                    st.location = me.location();
                    st.suspended = true;
                    st.stopInfo = exitInfo(me);
                    trackChanges(st);
                    stop = snapshot(vm, st.cfg, st.thread, st.location, st.out);
                } else if (event instanceof ClassPrepareEvent) {
                    ClassPrepareEvent cp = (ClassPrepareEvent) event;
                    try { cp.request().disable(); } catch (Exception ignored) {}
                    try {
                        plantPending(vm, st.cfg, cp.referenceType(), st.planted);
                    } catch (Exception e) {
                        // Don't poison the session: resume before surfacing
                        // (e.g. unknown method name in method:Class.m).
                        try { set.resume(); } catch (Exception ignored) {}
                        throw e;
                    }
                } else if (event instanceof VMDeathEvent || event instanceof VMDisconnectEvent) {
                    st.exited = true;
                    publishState(st, false);
                    throw new BridgeException("target VM exited");
                }
            }
            if (stop != null) {
                publishState(st, true);
                return stop;
            }
            set.resume();
        }
    }

    /** Background event drainer for unstopped sessions (logpoints only). */
    static void drainLoop(SessionState st) {
        while (!st.exited) {
            synchronized (st.queueLock) {
                while (st.waiterActive && !st.exited) {
                    try {
                        st.queueLock.wait(500);
                    } catch (InterruptedException ie) {
                        return;
                    }
                }
                if (st.exited) return;
            }
            EventSet set;
            try {
                set = st.vm.eventQueue().remove(500);
            } catch (InterruptedException ie) {
                return;
            } catch (Exception e) {
                st.exited = true;
                return;
            }
            if (set == null) continue;
            try {
                for (Event event : set) {
                    if (event instanceof BreakpointEvent) {
                        BreakpointEvent bp = (BreakpointEvent) event;
                        fireLogpoints(st, null, st.dir, st.cfg, bp.thread(), bp.location());
                    } else if (event instanceof ClassPrepareEvent) {
                        ClassPrepareEvent cp = (ClassPrepareEvent) event;
                        try { cp.request().disable(); } catch (Exception ignored) {}
                        try {
                            plantPending(st.vm, st.cfg, cp.referenceType(), st.planted);
                        } catch (Exception ignored) {
                            // Setup errors surface on commands; keep draining.
                        }
                    } else if (event instanceof VMDeathEvent || event instanceof VMDisconnectEvent) {
                        st.exited = true;
                        return;
                    }
                }
            } finally {
                try { set.resume(); } catch (Exception ignored) {}
            }
        }
    }

    static boolean amOwner(SessionState st) {
        try {
            String raw = new String(Files.readAllBytes(st.dir.resolve("owner.json")),
                    StandardCharsets.UTF_8);
            Map<String, String> m = parseJsonObject(raw);
            return st.ownerNonce.equals(m.get("nonce"));
        } catch (Exception e) {
            return false;
        }
    }

    static void serveLoop(SessionState st, Path dir) throws Exception {
        while (true) {
            // Abandoned (dir rm'd or respawned under our name)? Clean up and
            // vanish; legit flows always close (which returns from here)
            // before removing the dir.
            if (!amOwner(st)) {
                cleanup(st);
                return;
            }
            Socket sock;
            try {
                sock = st.server.accept();
            } catch (java.net.SocketTimeoutException te) {
                continue; // idle window: re-check abandonment above
            } catch (Exception e) {
                return; // server closed
            }
            try {
                String req = readFrame(sock.getInputStream());
                String resp = dispatch(st, req);
                writeFrame(sock.getOutputStream(), resp);
            } catch (CloseSession c) {
                try {
                    writeFrame(sock.getOutputStream(), "{\"ok\":true,\"closed\":true}");
                } catch (Exception ignored) {}
                try { sock.close(); } catch (Exception ignored) {}
                cleanup(st);
                return;
            } catch (Exception e) {
                try {
                    writeFrame(sock.getOutputStream(),
                            "{\"ok\":false,\"error\":" + quote(shortMsg(e)) + "}");
                } catch (Exception ignored) {}
            } finally {
                try { sock.close(); } catch (Exception ignored) {}
            }
        }
    }

    static class CloseSession extends Exception {}

    static void cleanup(SessionState st) {
        if (st.cfg.sessionKind.equals("launch")) {
            try { st.vm.exit(0); } catch (Exception ignored) {}
        } else {
            try { st.vm.dispose(); } catch (Exception ignored) {}
        }
        try { st.server.close(); } catch (Exception ignored) {}
    }

    static String dispatch(SessionState st, String reqJson) throws Exception {
        Map<String, String> req = parseJsonObject(reqJson);
        String cmd = req.get("cmd");
        if (cmd == null) throw new BridgeException("request needs a cmd");
        long timeout = req.containsKey("timeout")
                ? Long.parseLong(req.get("timeout")) * 1000 : st.cfg.timeoutMs;
        switch (cmd) {
            case "close": throw new CloseSession();
            case "threads": {
                // Momentary freeze for an instant thread dump. Balanced
                // suspend/resume pair: a stopped session stays stopped.
                if (st.exited) throw new BridgeException("target VM has exited — close this session");
                boolean wasSuspended = st.suspended;
                if (!wasSuspended) {
                    try {
                        st.vm.suspend();
                    } catch (Exception e) {
                        throw new BridgeException("cannot suspend target: " + shortMsg(e));
                    }
                }
                String dump;
                try {
                    dump = threadsDumpJson(st.vm);
                } catch (Exception e) {
                    throw new BridgeException("cannot read threads: " + shortMsg(e));
                } finally {
                    if (!wasSuspended) {
                        try { st.vm.resume(); } catch (Exception ignored) {}
                    }
                }
                return "{\"ok\":true,\"running\":" + (!wasSuspended) + ",\"threads\":" + dump + "}";
            }
            case "breaks": {
                // Arm-time intent with live plant state, no stop required.
                if (st.exited) throw new BridgeException("target VM has exited — close this session");
                return breaksJson(st);
            }
            case "logs": {
                int tail = 50;
                if (req.containsKey("tail")) {
                    try { tail = Integer.parseInt(req.get("tail")); } catch (NumberFormatException ignored) {}
                    if (tail < 1) tail = 1;
                    if (tail > 500) tail = 500;
                }
                List<String> lines = new ArrayList<>();
                int total = 0;
                try {
                    List<String> all = Files.readAllLines(st.dir.resolve("logs.jsonl"), StandardCharsets.UTF_8);
                    total = all.size();
                    for (int i = Math.max(0, total - tail); i < total; i++) lines.add(all.get(i));
                } catch (Exception ignored) {}
                return "{\"ok\":true,\"total\":" + total + ",\"truncated\":" + (total > lines.size())
                        + ",\"lines\":" + toJsonArray(lines) + "}";
            }
            case "context": {
                requireStopped(st);
                return "{\"ok\":true,\"stopInfo\":" + stopInfoJson(st)
                        + ",\"location\":" + locationJson(st.location, st.cfg)
                        + ",\"threads\":" + threadsJson(st.vm, st.thread)
                        + ",\"frames\":" + framesJson(st.thread, true) + "}";
            }
            case "stack": {
                requireStopped(st);
                return "{\"ok\":true,\"frames\":" + framesJson(st.thread, false) + "}";
            }
            case "vars": {
                requireStopped(st);
                int frame = req.containsKey("frame") ? Integer.parseInt(req.get("frame")) : 0;
                List<StackFrame> frames = safeFrames(st.thread);
                if (frame < 0 || frame >= frames.size()) {
                    throw new BridgeException("no frame " + frame + " (have " + frames.size() + ")");
                }
                return "{\"ok\":true,\"frame\":" + frame + ",\"locals\":" + localsJson(frames.get(frame)) + "}";
            }
            case "eval": {
                requireStopped(st);
                String expr = req.get("expr");
                if (expr == null) throw new BridgeException("eval needs an expr");
                int frame = req.containsKey("frame") ? Integer.parseInt(req.get("frame")) : 0;
                List<StackFrame> frames = safeFrames(st.thread);
                if (frame < 0 || frame >= frames.size()) {
                    throw new BridgeException("no frame " + frame + " (have " + frames.size() + ")");
                }
                String value = evalExpr(st.thread, frames.get(frame), expr);
                return "{\"ok\":true,\"expr\":" + quote(expr) + ",\"value\":" + quote(value) + "}";
            }
            case "continue": {
                requireLive(st);
                if (st.suspended) st.vm.resume();
                st.suspended = false;
                publishState(st, false);
                String snap = awaitStop(st, timeout);
                return "{\"ok\":true,\"stopped\":true,\"changed\":" + st.lastChanged + ",\"stopInfo\":" + stopInfoJson(st) + ",\"snapshot\":" + snap + "}";
            }
            case "step": {
                requireLive(st);
                String mode = req.getOrDefault("mode", "over");
                com.sun.jdi.request.StepRequest sr;
                try {
                    sr = st.vm.eventRequestManager().createStepRequest(
                            st.thread,
                            com.sun.jdi.request.StepRequest.STEP_LINE,
                            mode.equals("into") ? com.sun.jdi.request.StepRequest.STEP_INTO
                                    : mode.equals("out") ? com.sun.jdi.request.StepRequest.STEP_OUT
                                    : com.sun.jdi.request.StepRequest.STEP_OVER);
                } catch (Exception e) {
                    throw new BridgeException("cannot step: " + shortMsg(e));
                }
                for (String ex : new String[]{"java.*", "javax.*", "jdk.*", "com.sun.*"}) {
                    sr.addClassExclusionFilter(ex);
                }
                sr.addCountFilter(1);
                sr.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                sr.enable();
                try {
                    if (st.suspended) st.vm.resume();
                    st.suspended = false;
                    publishState(st, false);
                    String snap = awaitStop(st, timeout);
                    return "{\"ok\":true,\"stopped\":true,\"changed\":" + st.lastChanged + ",\"stopInfo\":" + stopInfoJson(st) + ",\"snapshot\":" + snap + "}";
                } finally {
                    try { st.vm.eventRequestManager().deleteEventRequest(sr); } catch (Exception ignored) {}
                }
            }
            default: throw new BridgeException("unknown cmd: " + cmd);
        }
    }

    static String stopInfoJson(SessionState st) {
        return st.stopInfo == null ? "null" : st.stopInfo;
    }

    /**
     * Rewrite session.json so `status` shows live truth (parked stop +
     * time) with zero prior memory. lastStop survives resume/exit — it
     * answers 'where was I last', not 'where am I now'. updatedAt marks the
     * last stop/resume/exit transition (not every read command).
     */
    static void publishState(SessionState st, boolean stopped) {
        if (stopped) {
            String ls = lastStopJson(st);
            if (ls != null) st.lastStopJson = ls;
        }
        long now = System.currentTimeMillis() / 1000;
        int port = 0;
        try { port = st.server.getLocalPort(); } catch (Exception ignored) {}
        String name = "?";
        try { name = st.dir.getFileName().toString(); } catch (Exception ignored) {}
        writeFile(st.dir.resolve("session.json"),
                "{\"name\":" + quote(name)
                + ",\"kind\":" + quote(st.cfg.sessionKind)
                + ",\"port\":" + port
                + ",\"stopped\":" + stopped
                + ",\"lastStop\":" + (st.lastStopJson == null ? "null" : st.lastStopJson)
                + ",\"updatedAt\":" + now + "}");
    }

    /** Trimmed stop locator (no snippet — file reads stay in snapshots). */
    static String lastStopJson(SessionState st) {
        if (st.location == null) return null;
        String cls = "?";
        String method = "?";
        int line = -1;
        try { cls = st.location.declaringType().name(); } catch (Exception ignored) {}
        try { method = st.location.method().name(); } catch (Exception ignored) {}
        try { line = st.location.lineNumber(); } catch (Exception ignored) {}
        return "{\"file\":" + quote(sourcePath(cls))
                + ",\"line\":" + line
                + ",\"method\":" + quote(method) + "}";
    }

    static void requireStopped(SessionState st) throws BridgeException {
        if (st.exited) throw new BridgeException("target VM has exited — close this session");
        if (!st.suspended) throw new BridgeException("no stopped thread (target is running — continue first)");
        if (st.thread == null) throw new BridgeException("no stopped thread yet in this session");
    }

    static void requireLive(SessionState st) throws BridgeException {
        if (st.exited) throw new BridgeException("target VM has exited — close this session");
    }

    /** A stopping (line/method) breakpoint planted at this location? */
    static boolean hasStoppingBreak(Config cfg, Location loc) {
        String cls = "?";
        int line = -1;
        String method = "?";
        try { cls = loc.declaringType().name(); } catch (Exception ignored) {}
        try { line = loc.lineNumber(); } catch (Exception ignored) {}
        try { method = loc.method().name(); } catch (Exception ignored) {}
        List<Integer> lines = cfg.breakpoints.get(cls);
        if (lines != null && lines.contains(line)) return true;
        List<String> methods = cfg.methodBreaks.get(cls);
        return methods != null && methods.contains(method);
    }

    /**
     * Arm-time intent with live plant state (served by `breaks`, no stop
     * required). Line/method breaks report verified iff their class is
     * loaded right now — deferred ClassPrepare planting flips pending to
     * verified automatically, so no stored state can go stale. Everything
     * else reports armed: JDI enables those synchronously with no per-item
     * receipt to report.
     */
    static String breaksJson(SessionState st) throws Exception {
        Config cfg = st.cfg;
        StringBuilder sb = new StringBuilder("{\"ok\":true,\"stops\":[");
        boolean first = true;
        for (Map.Entry<String, List<Integer>> e : cfg.breakpoints.entrySet()) {
            boolean loaded = !st.vm.classesByName(e.getKey()).isEmpty();
            for (int line : e.getValue()) {
                String spec = e.getKey() + ":" + line;
                String cond = cfg.condByLoc.get(e.getKey() + ":" + line);
                if (cond != null) spec += "|" + cond;
                first = breakRec(sb, first, spec, "break",
                        loaded ? "verified" : "pending",
                        loaded ? null : "class not loaded yet (deferred)");
            }
        }
        for (Map.Entry<String, List<String>> e : cfg.methodBreaks.entrySet()) {
            boolean loaded = !st.vm.classesByName(e.getKey()).isEmpty();
            for (String m : e.getValue()) {
                String spec = "method:" + e.getKey() + "." + m;
                String cond = cfg.condByLoc.get("method:" + e.getKey() + "." + m);
                if (cond != null) spec += "|" + cond;
                first = breakRec(sb, first, spec, "method",
                        loaded ? "verified" : "pending",
                        loaded ? null : "class not loaded yet (deferred)");
            }
        }
        for (String f : cfg.excFilters) {
            first = breakRec(sb, first, "exc:" + f, "exc", "armed", null);
        }
        for (Logpoint lp : cfg.logpoints) {
            first = breakRec(sb, first, lp.cls + ":" + lp.line, "logpoint", "armed", lp.template);
        }
        for (Watchpoint w : cfg.watchpoints) {
            String mode = w.onRead && w.onWrite ? "read,write" : (w.onRead ? "read" : "write");
            first = breakRec(sb, first, w.cls + "." + w.field, "watch", "armed", mode);
        }
        for (Map.Entry<String, List<String>> e : cfg.exitMethods.entrySet()) {
            for (String m : e.getValue()) {
                first = breakRec(sb, first, e.getKey() + "." + m, "exit", "armed", null);
            }
        }
        return sb.append("]}").toString();
    }

    static boolean breakRec(StringBuilder sb, boolean first,
            String spec, String kind, String state, String detail) {
        if (!first) sb.append(',');
        sb.append("{\"spec\":").append(quote(spec));
        sb.append(",\"kind\":").append(quote(kind));
        sb.append(",\"state\":").append(quote(state));
        if (detail != null) sb.append(",\"detail\":").append(quote(detail));
        sb.append('}');
        return false;
    }

    static String toJsonArray(List<String> items) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < items.size(); i++) {
            if (i > 0) sb.append(',');
            sb.append(quote(items.get(i)));
        }
        return sb.append(']').toString();
    }

    static void fireLogpoints(SessionState st, List<String> oneShotSink, Path dir,
            Config cfg, ThreadReference thread, Location loc) {
        List<String> templates = matchingTemplates(cfg, loc);
        if (templates.isEmpty()) return;
        List<StackFrame> frames = safeFrames(thread);
        if (frames.isEmpty()) return;
        for (String t : templates) {
            String line;
            try {
                line = renderTemplate(thread, frames.get(0), t);
            } catch (Exception e) {
                line = "[logpoint error: " + shortMsg(e) + "]";
            }
            if (st != null) {
                appendSessionLog(st, dir, line);
            } else if (oneShotSink != null) {
                oneShotSink.add(line);
            }
        }
    }

    static void appendSessionLog(SessionState st, Path dir, String line) {
        if (st.logCount >= MAX_LOG_LINES) {
            if (st.logCount == MAX_LOG_LINES) {
                appendFile(dir.resolve("logs.jsonl"), "[log cap reached: " + MAX_LOG_LINES + " lines]");
                st.logCount++;
            }
            return;
        }
        appendFile(dir.resolve("logs.jsonl"), line);
        st.logCount++;
    }

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
        ByteArrayOutputStream header = new ByteArrayOutputStream();
        int[] last = new int[]{-1, -1, -1, -1};
        int b;
        while ((b = in.read()) >= 0) {
            header.write(b);
            last[0] = last[1]; last[1] = last[2]; last[2] = last[3]; last[3] = b;
            if (last[0] == '\r' && last[1] == '\n' && last[2] == '\r' && last[3] == '\n') break;
        }
        int length = -1;
        for (String line : header.toString("US-ASCII").split("\r\n")) {
            int colon = line.indexOf(':');
            if (colon > 0 && line.substring(0, colon).trim().equalsIgnoreCase("Content-Length")) {
                length = Integer.parseInt(line.substring(colon + 1).trim());
            }
        }
        if (length < 0) throw new BridgeException("bad frame: no Content-Length");
        byte[] body = new byte[length];
        int off = 0;
        while (off < length) {
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
        return formatValue(resolvePath(thread, frame, e, null), 1);
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
                    jdiArgs.add(obj.virtualMachine().mirrorOf(parseJsonLite(a)));
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
                throw new BridgeException("cannot invoke " + name + "(): " + shortMsg(e));
            }
            throw new BridgeException("cannot invoke " + name + "(): " + shortMsg(e));
        }
        return out[0];
    }

    /** Decode a JSON string literal (quotes included) to raw text. */
    static String parseJsonLite(String quoted) throws BridgeException {
        int[] end = new int[1];
        return parseJsonString(quoted, 0, end);
    }

    // ---- helpers ----

    static class StreamGobbler extends Thread {
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
                        if (buf.size() > MAX_OUTPUT * 2) {
                            byte[] all = buf.toByteArray();
                            buf.reset();
                            buf.write(all, all.length - MAX_OUTPUT * 2, MAX_OUTPUT * 2);
                        }
                    }
                }
            } catch (Exception ignored) {}
        }

        String tail() {
            synchronized (buf) {
                String s = buf.toString();
                if (s.length() > MAX_OUTPUT) s = s.substring(s.length() - MAX_OUTPUT);
                return s;
            }
        }
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
