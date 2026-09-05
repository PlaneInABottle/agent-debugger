import com.sun.jdi.AbsentInformationException;
import com.sun.jdi.Field;
import com.sun.jdi.LocalVariable;
import com.sun.jdi.Location;
import com.sun.jdi.ReferenceType;
import com.sun.jdi.StackFrame;
import com.sun.jdi.ThreadReference;
import com.sun.jdi.Value;
import com.sun.jdi.VirtualMachine;
import com.sun.jdi.event.BreakpointEvent;
import com.sun.jdi.event.ClassPrepareEvent;
import com.sun.jdi.event.Event;
import com.sun.jdi.event.EventSet;
import com.sun.jdi.event.VMDeathEvent;
import com.sun.jdi.event.VMDisconnectEvent;
import com.sun.jdi.request.BreakpointRequest;
import com.sun.jdi.request.ClassPrepareRequest;
import com.sun.jdi.request.EventRequest;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

// Persistent session server: arming, event loop, dispatch, breaks, logs. Moved verbatim from JdiBridge.java.
class BridgeSession {
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
        BridgeProto.writeFile(dir.resolve("owner.json"),
                "{\"pid\":" + ProcessHandle.current().pid()
                + ",\"nonce\":" + JdiBridge.quote(st.ownerNonce) + "}");
        try {
            if (cfg.sessionKind.equals("attach")) {
                st.vm = BridgeConn.attachVm(cfg);
            } else if (cfg.sessionKind.equals("launch")) {
                if (cfg.mainClass == null) throw new UsageException("launch needs --main");
                Launched l = BridgeConn.launchVm(cfg);
                st.vm = l.vm;
                st.out = l.out;
                st.err = l.err;
            } else {
                throw new UsageException("--kind must be attach or launch");
            }
            armBreakpoints(st.vm, cfg);
            st.vm.resume();
            st.suspended = false;
            if (BridgeCli.hasStoppingBreaks(cfg)) {
                // First stop, synchronously: CLI polls session.json for readiness.
                try {
                    awaitStop(st, cfg.timeoutMs);
                } catch (StopTimeout t) {
                    // Attach-only fallback: a live target that never hits
                    // stays a running session with breaks armed; the agent
                    // triggers the stop later via continue. Launch timeouts
                    // still fail (outer catch maps them to error.json), and
                    // target exit (BridgeException) never falls back.
                    if (!cfg.sessionKind.equals("attach")) throw t;
                    publishState(st, false);
                    serveLoop(st, dir);
                    return;
                }
            }
            publishState(st, st.suspended);
            serveLoop(st, dir);
        } catch (UsageException | BridgeException e) {
            cleanupVm(st);
            BridgeProto.writeFile(dir.resolve("error.json"), "{\"error\":" + JdiBridge.quote(setupErrorText(e, st)) + "}");
            throw e;
        } catch (RuntimeException e) {
            // JDI failures surface as unchecked VMDisconnectedException etc.
            // Map them to the same error file (never a bare "internal:"
            // crash), e.g. a target that vanishes mid-handshake.
            BridgeException be = new BridgeException(JdiBridge.shortMsg(e));
            cleanupVm(st);
            BridgeProto.writeFile(dir.resolve("error.json"), "{\"error\":" + JdiBridge.quote(setupErrorText(be, st)) + "}");
            throw be;
        } catch (Throwable t) {
            // Any other setup crash (checked IO, linkage errors): same
            // cleanup, then a sanitized error.json the CLI surfaces instead
            // of a bare stdout "internal:" with no file. The name stays
            // reusable — the CLI removes failed-setup dirs wholesale.
            // (M2 setup cleanup above is reused, never duplicated.)
            BridgeException be = new BridgeException(JdiBridge.sanitizeUnexpected(t));
            cleanupVm(st);
            BridgeProto.writeFile(dir.resolve("error.json"), "{\"error\":" + JdiBridge.quote(setupErrorText(be, st)) + "}");
            throw be;
        } finally {
            try { server.close(); } catch (Exception ignored) {}
        }
    }

    static void armBreakpoints(VirtualMachine vm, Config cfg) throws Exception {
        for (Map.Entry<String, List<Integer>> e : cfg.breakpoints.entrySet()) {
            List<ReferenceType> loaded = vm.classesByName(e.getKey());
            if (!loaded.isEmpty()) {
                for (ReferenceType rt : loaded) BridgeConn.setLines(vm, rt, e.getValue());
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
            if (isShadowed(cfg, lp)) continue; // break wins: no JDI request, no double-event
            List<ReferenceType> loaded = vm.classesByName(lp.cls);
            if (!loaded.isEmpty()) {
                for (ReferenceType rt : loaded) BridgeConn.setLines(vm, rt, java.util.Collections.singletonList(lp.line),
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
                BridgeConn.setLines(vm, rt, lines);
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
                if (isShadowed(cfg, lp)) continue; // break wins on the same line
                BridgeConn.setLines(vm, rt, java.util.Collections.singletonList(lp.line),
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

    static void trackChanges(SessionState st) {
        try {
            List<StackFrame> frames = BridgeSnapshot.safeFrames(st.thread);
            Map<String, String> cur = new LinkedHashMap<>();
            if (!frames.isEmpty()) {
                StackFrame f = frames.get(0);
                try {
                    List<LocalVariable> vars = f.visibleVariables();
                    for (Map.Entry<LocalVariable, Value> ve : f.getValues(vars).entrySet()) {
                        cur.put(ve.getKey().name(), BridgeSnapshot.formatValue(ve.getValue(), 1));
                    }
                } catch (AbsentInformationException ignored) {}
            }
            StringBuilder sb = new StringBuilder("[");
            boolean first = true;
            if (st.lastTop == null) {
                for (String name : cur.keySet()) {
                    if (!first) sb.append(',');
                    first = false;
                    sb.append(JdiBridge.quote(name));
                }
            } else {
                for (Map.Entry<String, String> e : cur.entrySet()) {
                    String old = st.lastTop.get(e.getKey());
                    if (!e.getValue().equals(old)) {
                        if (!first) sb.append(',');
                        first = false;
                        sb.append(JdiBridge.quote(e.getKey()));
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
     * The session thread is the only event-queue consumer.
     */

    static String awaitStop(SessionState st, long timeoutMs) throws Exception {
        return awaitStopInner(st, timeoutMs);
    }

    static String awaitStopInner(SessionState st, long timeoutMs) throws Exception {
        VirtualMachine vm = st.vm;
        long deadline = System.nanoTime() + timeoutMs * 1_000_000;
        while (true) {
            // Same abandonment guard as serveLoop (1s event windows bound it).
            if (!amOwner(st)) {
                cleanup(st);
                System.exit(0);
            }
            long remaining = (deadline - System.nanoTime()) / 1_000_000;
            if (remaining <= 0) {
                throw new StopTimeout("timeout: no stop within " + (timeoutMs / 1000) + "s");
            }
            EventSet set;
            try {
                set = vm.eventQueue().remove(Math.min(remaining, 1000));
            } catch (InterruptedException ie) {
                continue;
            } catch (Exception e) {
                st.exited = true;
                publishState(st, false);
                throw new BridgeException("lost connection to target VM: " + JdiBridge.shortMsg(e));
            }
            if (set == null) continue;
            String stop = null;
            for (Event event : set) {
                if (event instanceof BreakpointEvent) {
                    BreakpointEvent bp = (BreakpointEvent) event;
                    fireLogpoints(st, null, st.dir, st.cfg, bp.thread(), bp.location());
                    if (!hasStoppingBreak(st.cfg, bp.location())) continue;
                    String cond = BridgeEval.lookupCond(st.cfg, bp.location());
                    if (cond != null && !BridgeEval.checkCond(bp.thread(), bp.location(), cond)) continue;
                    countBreakHit(st, bp.location());
                    // First stopping event in the set wins the exposed
                    // stop (deterministic); every matching event still
                    // counts its hits and fires its logpoints above.
                    if (stop != null) continue;
                    st.thread = bp.thread();
                    st.location = bp.location();
                    st.stopInfo = null; // plain stop supersedes any previous reason
                    trackChanges(st);
                    st.suspended = true;
                    stop = BridgeSnapshot.snapshot(vm, st.cfg, st.thread, st.location, st.out, st.err);
                } else if (event instanceof com.sun.jdi.event.StepEvent) {
                    com.sun.jdi.event.StepEvent se = (com.sun.jdi.event.StepEvent) event;
                    if (stop != null) continue;
                    st.thread = se.thread();
                    st.location = se.location();
                    st.stopInfo = null;
                    trackChanges(st);
                    st.suspended = true;
                    stop = BridgeSnapshot.snapshot(vm, st.cfg, st.thread, st.location, st.out, st.err);
                } else if (event instanceof com.sun.jdi.event.ExceptionEvent) {
                    com.sun.jdi.event.ExceptionEvent ee = (com.sun.jdi.event.ExceptionEvent) event;
                    if (!matchesExcFilter(st.cfg, ee)) continue;
                    fireLogpoints(st, null, st.dir, st.cfg, ee.thread(), ee.location());
                    String cond = BridgeEval.lookupCond(st.cfg, ee.location());
                    if (cond != null && !BridgeEval.checkCond(ee.thread(), ee.location(), cond)) continue;
                    countExcHits(st, ee);
                    if (stop != null) continue;
                    st.thread = ee.thread();
                    st.location = ee.location();
                    st.stopInfo = BridgeEval.exceptionInfo(ee);
                    trackChanges(st);
                    st.suspended = true;
                    stop = BridgeSnapshot.snapshot(vm, st.cfg, st.thread, st.location, st.out, st.err);
                } else if (event instanceof com.sun.jdi.event.ModificationWatchpointEvent) {
                    com.sun.jdi.event.ModificationWatchpointEvent we =
                            (com.sun.jdi.event.ModificationWatchpointEvent) event;
                    try { bump(st, "watch|" + we.field().declaringType().name() + "." + we.field().name()); } catch (Exception ignored) {}
                    if (stop != null) continue;
                    st.thread = we.thread();
                    st.location = we.location();
                    st.stopInfo = BridgeEval.watchInfo(we.field(), "write", we.valueToBe());
                    trackChanges(st);
                    st.suspended = true;
                    stop = BridgeSnapshot.snapshot(vm, st.cfg, st.thread, st.location, st.out, st.err);
                } else if (event instanceof com.sun.jdi.event.AccessWatchpointEvent) {
                    com.sun.jdi.event.AccessWatchpointEvent we =
                            (com.sun.jdi.event.AccessWatchpointEvent) event;
                    try { bump(st, "watch|" + we.field().declaringType().name() + "." + we.field().name()); } catch (Exception ignored) {}
                    if (stop != null) continue;
                    st.thread = we.thread();
                    st.location = we.location();
                    st.stopInfo = BridgeEval.watchInfo(we.field(), "read", we.valueCurrent());
                    trackChanges(st);
                    st.suspended = true;
                    stop = BridgeSnapshot.snapshot(vm, st.cfg, st.thread, st.location, st.out, st.err);
                } else if (event instanceof com.sun.jdi.event.MethodExitEvent) {
                    com.sun.jdi.event.MethodExitEvent me = (com.sun.jdi.event.MethodExitEvent) event;
                    if (!BridgeEval.wantedExit(st.cfg, me)) continue;
                    try { bump(st, "exit|" + me.method().declaringType().name() + "." + me.method().name()); } catch (Exception ignored) {}
                    if (stop != null) continue;
                    st.thread = me.thread();
                    st.location = me.location();
                    st.stopInfo = BridgeEval.exitInfo(me);
                    trackChanges(st);
                    st.suspended = true;
                    stop = BridgeSnapshot.snapshot(vm, st.cfg, st.thread, st.location, st.out, st.err);
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
                    throw new BridgeException("target VM exited"
                            + BridgeSnapshot.targetOutputSuffix(st.out, st.err));
                }
            }
            if (stop != null) {
                publishState(st, true);
                return stop;
            }
            set.resume();
        }
    }

    static class StopTimeout extends BridgeException {
        StopTimeout(String message) { super(message); }
    }

    static boolean amOwner(SessionState st) {
        try {
            String raw = new String(Files.readAllBytes(st.dir.resolve("owner.json")),
                    StandardCharsets.UTF_8);
            Map<String, String> m = BridgeProto.parseJsonObject(raw);
            return st.ownerNonce.equals(m.get("nonce"));
        } catch (Exception e) {
            return false;
        }
    }

    static void serveLoop(SessionState st, Path dir) throws Exception {
        st.server.setSoTimeout(100);
        while (true) {
            // Abandoned (dir rm'd or respawned under our name)? Clean up and
            // vanish; legit flows always close (which returns from here)
            // before removing the dir.
            if (!amOwner(st)) {
                cleanup(st);
                return;
            }
            // Use the same event handler between commands, without a second
            // consumer or shared mutable stop state. A parked VM is not resumed.
            if (!st.exited && !st.suspended) {
                try { awaitStopInner(st, 10); }
                catch (StopTimeout idle) { /* no pending stop */ }
                catch (Exception e) { System.err.println("event: " + JdiBridge.shortMsg(e)); }
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
                sock.setSoTimeout(5000);
                String req = BridgeProto.readFrame(sock.getInputStream());
                String resp = dispatch(st, req);
                BridgeProto.writeFrame(sock.getOutputStream(), resp);
            } catch (CloseSession c) {
                try {
                    BridgeProto.writeFrame(sock.getOutputStream(), "{\"ok\":true,\"closed\":true}");
                } catch (Exception ignored) {}
                try { sock.close(); } catch (Exception ignored) {}
                cleanup(st);
                return;
            } catch (Exception e) {
                try {
                    BridgeProto.writeFrame(sock.getOutputStream(),
                            "{\"ok\":false,\"error\":" + JdiBridge.quote(JdiBridge.shortMsg(e)) + "}");
                } catch (Exception ignored) {}
            } finally {
                try { sock.close(); } catch (Exception ignored) {}
            }
        }
    }

    static void cleanup(SessionState st) {
        cleanupVm(st);
        try { st.server.close(); } catch (Exception ignored) {}
    }

    /** Setup-failure path: kill exactly the VM we started (launch exits the
     *  target, attach detaches); close semantics stay untouched. */
    static void cleanupVm(SessionState st) {
        if (st == null || st.vm == null) return;
        try {
            if (st.cfg != null && "launch".equals(st.cfg.sessionKind)) {
                try { st.vm.exit(0); } catch (Exception ignored) {}
            } else {
                try { st.vm.dispose(); } catch (Exception ignored) {}
            }
        } finally {
            st.vm = null;
        }
    }

    /** Setup errors carry bounded target output so bad main/cp failures name
     *  the real cause (e.g. "Could not find or load main class"). */
    static String setupErrorText(Exception e, SessionState st) {
        String base = e.getMessage() == null ? e.toString() : e.getMessage();
        StreamGobbler out = st == null ? null : st.out;
        StreamGobbler err = st == null ? null : st.err;
        String suffix = "";
        try { suffix = BridgeSnapshot.targetOutputSuffix(out, err); } catch (Exception ignored) {}
        if (suffix == null || suffix.isEmpty()) return base;
        if (base != null && base.contains("target output:")) return base;
        return base + suffix;
    }

    static String dispatch(SessionState st, String reqJson) throws Exception {
        // cmd first (depth-aware): breaksAdd carries a JSON array the flat
        // parser cannot hold, so it branches before flat parsing.
        String cmd = BridgeProto.parseCmd(reqJson);
        if (cmd.equals("breaksAdd")) {
            return breaksAddJson(st, BridgeProto.parseStringArray(reqJson, "breaks"));
        }
        Map<String, String> req = BridgeProto.parseJsonObject(reqJson);
        long timeout = req.containsKey("timeout")
                ? BridgeCli.timeoutMillis(req.get("timeout")) : st.cfg.timeoutMs;
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
                        throw new BridgeException("cannot suspend target: " + JdiBridge.shortMsg(e));
                    }
                }
                String dump;
                try {
                    dump = BridgeEval.threadsDumpJson(st.vm);
                } catch (Exception e) {
                    throw new BridgeException("cannot read threads: " + JdiBridge.shortMsg(e));
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
                // total = retained lines on disk (<= MAX); dropped = lifetime
                // lines evicted by the ring; truncated = the tail was cut OR
                // any line was ever evicted (historical drops, not just cut).
                return "{\"ok\":true,\"total\":" + total + ",\"truncated\":" + (total > lines.size() || st.logDropped > 0)
                        + ",\"dropped\":" + st.logDropped + ",\"lines\":" + toJsonArray(lines) + "}";
            }
            case "context": {
                requireStopped(st);
                return "{\"ok\":true,\"stopInfo\":" + stopInfoJson(st)
                        + ",\"location\":" + BridgeSnapshot.locationJson(st.location, st.cfg)
                        + ",\"threads\":" + BridgeSnapshot.threadsJson(st.vm, st.thread)
                        + ",\"frames\":" + BridgeSnapshot.framesJson(st.thread, true) + "}";
            }
            case "stack": {
                requireStopped(st);
                return "{\"ok\":true,\"frames\":" + BridgeSnapshot.framesJson(st.thread, false) + "}";
            }
            case "vars": {
                requireStopped(st);
                int frame = req.containsKey("frame") ? Integer.parseInt(req.get("frame")) : 0;
                List<StackFrame> frames = BridgeSnapshot.safeFrames(st.thread);
                if (frame < 0 || frame >= frames.size()) {
                    throw new BridgeException("no frame " + frame + " (have " + frames.size() + ")");
                }
                return "{\"ok\":true,\"frame\":" + frame + ",\"locals\":" + BridgeSnapshot.localsJson(frames.get(frame)) + "}";
            }
            case "eval": {
                requireStopped(st);
                String expr = req.get("expr");
                if (expr == null) throw new BridgeException("eval needs an expr");
                int frame = req.containsKey("frame") ? Integer.parseInt(req.get("frame")) : 0;
                List<StackFrame> frames = BridgeSnapshot.safeFrames(st.thread);
                if (frame < 0 || frame >= frames.size()) {
                    throw new BridgeException("no frame " + frame + " (have " + frames.size() + ")");
                }
                String value = BridgeEval.evalExpr(st.thread, frames.get(frame), expr);
                return "{\"ok\":true,\"expr\":" + JdiBridge.quote(expr) + ",\"value\":" + JdiBridge.quote(value) + "}";
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
                // Stepping needs a stopped thread to step from (uniform
                // contract on all bridges); continuing works from running
                // (it just waits for the next stop).
                requireStopped(st);
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
                    throw new BridgeException("cannot step: " + JdiBridge.shortMsg(e));
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
        BridgeProto.writeFile(st.dir.resolve("session.json"),
                "{\"name\":" + JdiBridge.quote(name)
                + ",\"kind\":" + JdiBridge.quote(st.cfg.sessionKind)
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
        return "{\"file\":" + JdiBridge.quote(BridgeSnapshot.sourcePath(cls))
                + ",\"line\":" + line
                + ",\"method\":" + JdiBridge.quote(method) + "}";
    }

    static void requireStopped(SessionState st) throws BridgeException {
        if (st.exited) throw new BridgeException("target VM has exited — close this session");
        if (!st.suspended) throw new BridgeException("no stopped thread (target is running — continue first)");
        if (st.thread == null) throw new BridgeException("no stopped thread yet in this session");
    }

    static void requireLive(SessionState st) throws BridgeException {
        if (st.exited) throw new BridgeException("target VM has exited — close this session");
    }

    /** Attribute a reported breakpoint stop to its line/method records.
     *  Step landings never reach here (StepEvent branch doesn't count), so
     *  dead breakpoints honestly read 0. */

    static void countBreakHit(SessionState st, Location loc) {
        String cls = "?";
        int line = -1;
        String method = "?";
        try { cls = loc.declaringType().name(); } catch (Exception ignored) {}
        try { line = loc.lineNumber(); } catch (Exception ignored) {}
        try { method = loc.method().name(); } catch (Exception ignored) {}
        List<Integer> lines = st.cfg.breakpoints.get(cls);
        if (lines != null && lines.contains(line)) bump(st, "break|" + cls + "|" + line);
        List<String> methods = st.cfg.methodBreaks.get(cls);
        if (methods != null && methods.contains(method)) bump(st, "method|" + cls + "." + method);
    }

    /** Attribute a reported exception stop to each matching exc filter. */

    static void countExcHits(SessionState st, com.sun.jdi.event.ExceptionEvent ee) {
        String actual = "?";
        try { actual = ee.exception().referenceType().name(); } catch (Exception ignored) {}
        for (String f : st.cfg.excFilters) {
            if (actual.equals(f) || actual.endsWith("." + f)) bump(st, "exc|" + f);
        }
    }

    /** Same-line logpoint shadowed by a real break (break wins, mirroring node/browser). */
    static boolean isShadowed(Config cfg, Logpoint lp) {
        List<Integer> lines = cfg.breakpoints.get(lp.cls);
        return lines != null && lines.contains(lp.line);
    }

    static String shadowDetail(Logpoint lp) {
        return "logpoint shadowed by breakpoint: " + lp.cls + ":" + lp.line;
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
        return "{\"ok\":true,\"stops\":" + stopsArrayJson(st) + "}";
    }

    /**
     * Additive line breaks on a live session (running or parked — never
     * suspended/resumed here). Two phases: the whole batch validates first
     * (parse, canonical dedup/conflict, read-only line checks) with zero JDI
     * mutation, then every fresh break arms. Exact canonical duplicates are
     * idempotent (added empty); the same line with a different condition
     * — or any same-line logpoint — rejects the batch before anything mutates.
     */
    static String breaksAddJson(SessionState st, List<String> raws) throws Exception {
        if (st.exited) throw new BridgeException("target VM has exited — close this session");
        if (raws.isEmpty()) throw new BridgeException("breaks add needs at least one --break");
        List<AddedLine> fresh = new ArrayList<>();
        Map<String, String> batchCond = new LinkedHashMap<>(); // loc -> cond (null = plain)
        java.util.Set<String> seen = new java.util.HashSet<>();
        for (String raw : raws) {
            AddedLine p = parseAddedLine(raw);
            String loc = p.cls + ":" + p.line;
            String key = loc + "|" + (p.cond == null ? "" : p.cond);
            if (!seen.add(key)) continue; // intra-batch duplicate: idempotent
            List<Integer> have = st.cfg.breakpoints.get(p.cls);
            String haveCond = st.cfg.condByLoc.get(loc);
            if (have != null && have.contains(p.line)) {
                if (condEqual(haveCond, p.cond)) continue; // already armed: idempotent
                throw new BridgeException("conflicting condition for " + loc + " (already armed"
                        + (haveCond == null ? " plain" : " as '" + haveCond + "'") + "): " + raw);
            }
            if (batchCond.containsKey(loc) && !condEqual(batchCond.get(loc), p.cond)) {
                throw new BridgeException("conflicting condition for " + loc + " (same batch): " + raw);
            }
            for (Logpoint lp : st.cfg.logpoints) {
                if (lp.cls.equals(p.cls) && lp.line == p.line) {
                    throw new BridgeException("conflicting logpoint for " + loc + " (already armed as logpoint): " + raw);
                }
            }
            batchCond.put(loc, p.cond);
            // Read-only line check for loaded classes (no JDI mutation yet).
            for (ReferenceType rt : st.vm.classesByName(p.cls)) {
                List<Location> locs;
                try {
                    locs = rt.locationsOfLine(p.line);
                } catch (AbsentInformationException aie) {
                    throw new BridgeException("class " + p.cls + " has no debug info — recompile with -g");
                }
                if (locs.isEmpty()) throw new BridgeException("no executable code at " + p.cls + ":" + p.line);
            }
            fresh.add(p);
        }
        StringBuilder added = new StringBuilder("[");
        boolean first = true;
        for (AddedLine p : fresh) {
            st.cfg.breakpoints.computeIfAbsent(p.cls, k -> new ArrayList<>()).add(p.line);
            if (p.cond != null) st.cfg.condByLoc.put(p.cls + ":" + p.line, p.cond);
            List<ReferenceType> loaded = st.vm.classesByName(p.cls);
            boolean verified;
            String detail = null;
            if (!loaded.isEmpty()) {
                // Direct arm: class is loaded right now.
                for (ReferenceType rt : loaded) {
                    BridgeConn.setLines(st.vm, rt, java.util.Collections.singletonList(p.line));
                }
                verified = true;
            } else {
                watchClass(st.vm, p.cls);
                // Class may have finished loading between validation and
                // arming: recheck before settling for deferred.
                loaded = st.vm.classesByName(p.cls);
                if (!loaded.isEmpty()) {
                    for (ReferenceType rt : loaded) {
                        BridgeConn.setLines(st.vm, rt, java.util.Collections.singletonList(p.line));
                    }
                    verified = true;
                } else {
                    verified = false;
                    detail = "class not loaded yet (deferred)";
                }
            }
            String spec = p.cls + ":" + p.line + (p.cond == null ? "" : "|" + p.cond);
            if (!first) added.append(',');
            first = false;
            added.append("{\"raw\":").append(JdiBridge.quote(p.raw));
            added.append(",\"spec\":").append(JdiBridge.quote(spec));
            added.append(",\"kind\":\"break\"");
            added.append(",\"state\":").append(JdiBridge.quote(verified ? "verified" : "pending"));
            if (detail != null) added.append(",\"detail\":").append(JdiBridge.quote(detail));
            added.append(",\"hits\":").append(hitsOf(st, "break|" + p.cls + "|" + p.line));
            added.append('}');
        }
        added.append(']');
        return "{\"ok\":true,\"added\":" + added + ",\"stops\":" + stopsArrayJson(st) + "}";
    }

    /** Line-break-only parse (mirrors BridgeCli.parseBreakpoint normalization). */
    static AddedLine parseAddedLine(String raw) throws UsageException {
        String cond = null;
        String head = raw;
        int bar = raw.indexOf('|');
        if (bar >= 0) {
            cond = raw.substring(bar + 1).trim();
            head = raw.substring(0, bar);
            BridgeCli.validateCond(cond);
        }
        if (head.startsWith("method:") || head.startsWith("exc:")) {
            throw new UsageException("breaks add takes line breaks only (got '" + raw + "')");
        }
        int colon = head.lastIndexOf(':');
        if (colon <= 0) throw new UsageException("--break must look like com.example.Hello:30, got: " + raw);
        String cls = head.substring(0, colon).replace('/', '.');
        if (cls.endsWith(".java")) cls = cls.substring(0, cls.length() - 5).replace('/', '.');
        int line;
        try {
            line = Integer.parseInt(head.substring(colon + 1));
        } catch (NumberFormatException e) {
            throw new UsageException("bad line in --break: " + raw);
        }
        if (line < 1) throw new UsageException("bad line in --break (must be >= 1): " + raw);
        AddedLine p = new AddedLine();
        p.raw = raw;
        p.cls = cls;
        p.line = line;
        p.cond = cond;
        return p;
    }

    static class AddedLine {
        String raw;
        String cls;
        int line;
        String cond;
    }

    static boolean condEqual(String a, String b) {
        return a == null ? b == null : a.equals(b);
    }

    static String stopsArrayJson(SessionState st) throws Exception {
        Config cfg = st.cfg;
        StringBuilder sb = new StringBuilder("[");
        boolean first = true;
        for (Map.Entry<String, List<Integer>> e : cfg.breakpoints.entrySet()) {
            boolean loaded = !st.vm.classesByName(e.getKey()).isEmpty();
            for (int line : e.getValue()) {
                String spec = e.getKey() + ":" + line;
                String cond = cfg.condByLoc.get(e.getKey() + ":" + line);
                if (cond != null) spec += "|" + cond;
                first = breakRec(sb, first, spec, "break",
                        loaded ? "verified" : "pending",
                        loaded ? null : "class not loaded yet (deferred)",
                        hitsOf(st, "break|" + e.getKey() + "|" + line));
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
                        loaded ? null : "class not loaded yet (deferred)",
                        hitsOf(st, "method|" + e.getKey() + "." + m));
            }
        }
        for (String f : cfg.excFilters) {
            first = breakRec(sb, first, "exc:" + f, "exc", "armed", null,
                    hitsOf(st, "exc|" + f));
        }
        for (Logpoint lp : cfg.logpoints) {
            if (isShadowed(cfg, lp)) {
                first = breakRec(sb, first, lp.cls + ":" + lp.line, "logpoint", "shadowed", shadowDetail(lp), 0);
            } else {
                first = breakRec(sb, first, lp.cls + ":" + lp.line, "logpoint", "armed", lp.template,
                        hitsOf(st, "logpoint|" + lp.cls + "|" + lp.line));
            }
        }
        for (Watchpoint w : cfg.watchpoints) {
            String mode = w.onRead && w.onWrite ? "read,write" : (w.onRead ? "read" : "write");
            first = breakRec(sb, first, w.cls + "." + w.field, "watch", "armed", mode,
                    hitsOf(st, "watch|" + w.cls + "." + w.field));
        }
        for (Map.Entry<String, List<String>> e : cfg.exitMethods.entrySet()) {
            for (String m : e.getValue()) {
                first = breakRec(sb, first, e.getKey() + "." + m, "exit", "armed", null,
                        hitsOf(st, "exit|" + e.getKey() + "." + m));
            }
        }
        return sb.append(']').toString();
    }

    static boolean breakRec(StringBuilder sb, boolean first,
            String spec, String kind, String state, String detail, int hits) {
        if (!first) sb.append(',');
        sb.append("{\"spec\":").append(JdiBridge.quote(spec));
        sb.append(",\"kind\":").append(JdiBridge.quote(kind));
        sb.append(",\"state\":").append(JdiBridge.quote(state));
        if (detail != null) sb.append(",\"detail\":").append(JdiBridge.quote(detail));
        sb.append(",\"hits\":").append(hits);
        sb.append('}');
        return false;
    }

    /** Hit-key counters behind `breaks`' hits. Keys mirror the specs:
     *  break|cls|line, method|cls|m, exc|filter, logpoint|cls|line,
     *  watch|cls|field, exit|cls|m. Step landings never bump — only reported
     *  stops do, so dead breakpoints honestly read 0. */

    static void bump(SessionState st, String key) {
        try {
            Integer n = st.hitCounts.get(key);
            st.hitCounts.put(key, n == null ? 1 : n + 1);
        } catch (Exception ignored) {}
    }

    static int hitsOf(SessionState st, String key) {
        try {
            Integer n = st.hitCounts.get(key);
            return n == null ? 0 : n;
        } catch (Exception ignored) {
            return 0;
        }
    }

    static String toJsonArray(List<String> items) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < items.size(); i++) {
            if (i > 0) sb.append(',');
            sb.append(JdiBridge.quote(items.get(i)));
        }
        return sb.append(']').toString();
    }

    static void fireLogpoints(SessionState st, List<String> oneShotSink, Path dir,
            Config cfg, ThreadReference thread, Location loc) {
        List<String> templates = BridgeEval.matchingTemplates(cfg, loc);
        if (templates.isEmpty()) return;
        List<StackFrame> frames = BridgeSnapshot.safeFrames(thread);
        if (frames.isEmpty()) return;
        if (st != null) {
            // Logpoint fires are observable here (unlike py's DAP-native
            // ones), including on cond-false non-stops — count the fire.
            String cls = "?";
            int line = -1;
            try { cls = loc.declaringType().name(); } catch (Exception ignored) {}
            try { line = loc.lineNumber(); } catch (Exception ignored) {}
            bump(st, "logpoint|" + cls + "|" + line);
        }
        for (String t : templates) {
            String line;
            try {
                line = BridgeEval.renderTemplate(thread, frames.get(0), t);
            } catch (Exception e) {
                line = "[logpoint error: " + JdiBridge.shortMsg(e) + "]";
            }
            if (st != null) {
                appendSessionLog(st, dir, line);
            } else if (oneShotSink != null) {
                oneShotSink.add(line);
            }
        }
    }

    static void appendSessionLog(SessionState st, Path dir, String line) {
        if (line == null) return;
        // One physical line per entry (like the other bridges): multi-line
        // values would otherwise shatter logs.jsonl structure and break the
        // <= MAX physical-lines bound below.
        String flat = line.replace("\r\n", "\u23ce").replace('\n', '\u23ce').replace('\r', '\u23ce');
        appendSessionLogParts(st, dir, java.util.Collections.singletonList(flat));
    }

    /** Ring-kept logs: logs.jsonl holds the latest MAX_LOG_LINES physical
     *  lines; older lines are evicted (counted in st.logDropped, surfaced by
     *  `logs`) instead of freezing on stale output. Bounded rewrite of a
     *  <=2000-line file (published atomically); plain appends stay
     *  append-only. */
    static void appendSessionLogParts(SessionState st, Path dir, java.util.List<String> parts) {
        if (parts.isEmpty()) return;
        Path file = dir.resolve("logs.jsonl");
        if (st.logCount + parts.size() <= BridgeEval.MAX_LOG_LINES) {
            for (String l : parts) BridgeProto.appendFile(file, l);
            st.logCount += parts.size();
            return;
        }
        java.util.List<String> kept = new java.util.ArrayList<>();
        try {
            kept.addAll(java.nio.file.Files.readAllLines(file, StandardCharsets.UTF_8));
        } catch (Exception ignored) {}
        kept.addAll(parts);
        int evicted = kept.size() - BridgeEval.MAX_LOG_LINES;
        java.util.List<String> tail = kept.subList(Math.max(0, evicted), kept.size());
        if (evicted > 0) st.logDropped += evicted;
        StringBuilder sb = new StringBuilder();
        for (String l : tail) sb.append(l).append('\n');
        BridgeProto.writeFile(file, sb.toString());
        st.logCount = tail.size();
    }
}
