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
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

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
        if (!amOwner(st)) {
            // A silent owner-write failure would surface later as a
            // baffling instant self-reap (Node/Browser/Python verify the
            // same claim explicitly) — fail fast instead.
            throw new BridgeException("session owner claim not visible; refusing to start");
        }
        // Setup-failure phase for error.json derives from the exception
        // type (UsageException/ConfigBridgeException read as config; a
        // vanished target stays transport; unexpected crashes report
        // runtime) — never from message text or a stage timer (see
        // mapSetupFailure). A successful connection never globally flips
        // later failures to config.
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
            // Layered identity before the first user-visible completion:
            // JDI VM properties (protocol-confirmed debuggee) + the CLI OS
            // listener observation (os-corroborated endpoint). Never throws.
            buildTargetIdentity(st);
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
            BridgeProto.writeFile(dir.resolve("error.json"), setupErrorJson(e, setupErrorText(e, st)));
            throw e;
        } catch (Throwable t) {
            // Any other setup failure: the same cleanup, then a sanitized
            // error.json the CLI surfaces instead of a bare stdout
            // "internal:" with no file. The typed mapping below decides
            // the phase (a vanished target stays transport; unexpected
            // crashes report runtime) — never message text. The name stays
            // reusable — the CLI removes failed-setup dirs wholesale.
            Exception mapped = mapSetupFailure(t);
            cleanupVm(st);
            BridgeProto.writeFile(dir.resolve("error.json"), setupErrorJson(mapped, setupErrorText(mapped, st)));
            throw mapped;
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
                throw new ConfigBridgeException("target VM cannot provide method return values");
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
        if (fields.isEmpty()) throw new ConfigBridgeException("no field " + w.field + " in " + rt.name());
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
                throw new ConfigBridgeException("target VM cannot watch field " + w.field);
            }
        }
    }

    static void setMethods(VirtualMachine vm, ReferenceType rt, List<String> methods) throws BridgeException {
        for (String name : methods) {
            List<com.sun.jdi.Method> found = rt.methodsByName(name);
            if (found.isEmpty()) {
                throw new ConfigBridgeException("no method " + name + "() in " + rt.name());
            }
            for (com.sun.jdi.Method m : found) {
                if (m.isNative() || m.isAbstract()) continue;
                Location loc = m.location();
                if (loc == null || loc.codeIndex() < 0) {
                    throw new ConfigBridgeException("method " + name + "() in " + rt.name()
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
    // Frame identity builders live in BridgeEval (M5.2).

    // Tracking + text helpers live in BridgeSnapshot/BridgeProto (M5.2).

    /**
     * Wait for the next stop (breakpoint or step end). Updates st.thread /
     * st.location and returns the stop snapshot. Throws on timeout or VM exit.
     * The session thread is the only event-queue consumer.
     */

    static String awaitStop(SessionState st, long timeoutMs) throws Exception {
        return awaitStop(st, timeoutMs, null, false);
    }

    /** Wait/capture entry: attaches the honest trigger-unknown context to
     *  the timeout (expectedBreak only when the capture planted one).
     *  Continue/step/handshake/idle waits pass withContext=false — their
     *  timeouts carry no waitContext. */
    static String awaitStop(SessionState st, long timeoutMs, String expectedBreak,
            boolean withContext) throws Exception {
        return awaitStopInner(st, timeoutMs, false, expectedBreak, withContext);
    }

    /**
     * Idle pump: harvest a pending stop while nobody waits (keeps a parked
     * VM's session.json honest). Never steals from a dispatch pump: the
     * outstanding/suspended/exited precheck and the pumpLock tryLock both
     * fail fast, so at most one EventQueue.remove consumer exists. Returns
     * null when there is nothing to do (a genuine stop still returns its
     * snapshot; a quiet window raises StopTimeout like any other wait).
     */
    static String awaitStopIdle(SessionState st, long timeoutMs) throws Exception {
        synchronized (st.sessionLock) {
            if (st.exited || st.suspended || st.outstanding != null) return null;
        }
        // A dispatch pump owns delivery for its whole wait: skip this idle
        // window instead of queueing a rival remove behind it.
        if (!st.pumpLock.tryLock()) return null;
        try {
            return awaitStopInner(st, timeoutMs, true);
        } finally {
            st.pumpLock.unlock();
        }
    }

    static String awaitStopInner(SessionState st, long timeoutMs) throws Exception {
        return awaitStopInner(st, timeoutMs, false);
    }

    static String awaitStopInner(SessionState st, long timeoutMs, boolean idle) throws Exception {
        return awaitStopInner(st, timeoutMs, idle, null, false);
    }

    static String awaitStopInner(SessionState st, long timeoutMs, boolean idle,
            String expectedBreak, boolean withContext) throws Exception {
        final long waitStartMs = System.currentTimeMillis();
        // LOW closure: snapshot vm/closing under sessionLock (visibility) —
        // cleanupVm nulls vm under the same lock, so the pump observes either
        // the live VM or null, never a half-closed transport. The blocking
        // remove and evaluation below stay outside the lock (live reads
        // prompt); only this snapshot and the per-set reconciliation take it.
        final VirtualMachine vm;
        synchronized (st.sessionLock) {
            vm = st.vm;
        }
        // HIGH: strict pump serialization — exactly one EventQueue.remove
        // consumer. The dispatch pump holds pumpLock for the WHOLE wait
        // (remove windows plus per-set evaluation); the idle pump only
        // tryLocks (see awaitStopIdle) and skips. The blocking remove — and
        // the unbounded condition/logpoint evaluation in preEvaluate — never
        // hold sessionLock, so live reads stay prompt.
        if (!idle) st.pumpLock.lock();
        try {
        long deadline = System.nanoTime() + timeoutMs * 1_000_000;
        while (true) {
            // Same abandonment guard as serveLoop (1s event windows bound it).
            if (!amOwner(st)) {
                cleanup(st);
                System.exit(0);
            }
            long remaining = (deadline - System.nanoTime()) / 1_000_000;
            if (remaining <= 0) {
                if (!idle) {
                    // Defense in depth (never the sole fix — pumpLock above
                    // is the serialization): a stop parked between our last
                    // remove and this timeout still exposes the park instead
                    // of a spurious timeout.
                    String parked = parkedRecheck(st);
                    if (parked != null) return parked;
                }
                String ctx = withContext
                        ? BridgeSnapshot.waitContextJson(st, timeoutMs, waitStartMs, expectedBreak) : null;
                throw new StopTimeout(BridgeSnapshot.timeoutText(st, timeoutMs), ctx);
            }
            EventSet set;
            try {
                // M5: the single pump consumer owns eventQueue.remove; the
                // blocking wait itself never holds sessionLock (live reads
                // stay prompt). Per-set processing below takes the lock only
                // for the bounded reconciliation (Phase B).
                set = vm.eventQueue().remove(Math.min(remaining, 1000));
            } catch (InterruptedException ie) {
                // Pool shutdown (close): never spin — a closing session
                // aborts the wait instead of looping forever. Read under
                // sessionLock like every other st.closing read (same LOW
                // visibility closure as the vm snapshot above; closing is
                // also volatile, so either read is safe).
                final boolean closingCopy;
                synchronized (st.sessionLock) {
                    closingCopy = st.closing;
                }
                if (closingCopy) throw new BridgeException("session closing");
                continue;
            } catch (Exception e) {
                synchronized (st.sessionLock) {
                    st.exited = true;
                    publishState(st, false);
                }
                throw new BridgeException("lost connection to target VM: " + JdiBridge.shortMsg(e));
            }
            if (set == null) continue;
            // Phase A: unbounded evaluation OUTSIDE sessionLock (condition
            // checks and logpoint renders may invokeMethod with a 10s join
            // — live reads must never wait on it). Pure per-event decisions;
            // no st.* mutation happens here.
            List<PreEval> pre = preEvaluate(st, set);
            synchronized (st.sessionLock) {
            String stop = null;
            String parkReason = null;
            for (PreEval pe : pre) {
                Event event = pe.event;
                if (event instanceof BreakpointEvent) {
                    BreakpointEvent bp = (BreakpointEvent) event;
                    applyLogFire(st, pe.logLines, bp.location());
                    if (!hasStoppingBreak(st.cfg, bp.location())
                            && !isCaptureBreak(st, bp.location())) continue;
                    // Condition verdicts come from Phase A (evaluated outside
                    // sessionLock); the lock only reconciles them here.
                    if (pe.cond != null && !pe.condPass) continue;
                    countBreakHit(st, bp.location());
                    // First stopping event in the set wins the exposed
                    // stop (deterministic); every matching event still
                    // counts its hits and fires its logpoints above.
                    if (stop != null) continue;
                    parkReason = "breakpoint";
                    st.thread = bp.thread();
                    st.location = bp.location();
                    st.stopInfo = null; // plain stop supersedes any previous reason
                    BridgeSnapshot.trackChanges(st);
                    stop = parkSnapshot(st, vm);
                } else if (event instanceof com.sun.jdi.event.StepEvent) {
                    com.sun.jdi.event.StepEvent se = (com.sun.jdi.event.StepEvent) event;
                    if (stop != null) continue;
                    parkReason = "step";
                    st.thread = se.thread();
                    st.location = se.location();
                    st.stopInfo = null;
                    BridgeSnapshot.trackChanges(st);
                    stop = parkSnapshot(st, vm);
                } else if (event instanceof com.sun.jdi.event.ExceptionEvent) {
                    com.sun.jdi.event.ExceptionEvent ee = (com.sun.jdi.event.ExceptionEvent) event;
                    if (!pe.wanted) continue;
                    applyLogFire(st, pe.logLines, ee.location());
                    if (pe.cond != null && !pe.condPass) continue;
                    countExcHits(st, ee);
                    if (stop != null) continue;
                    parkReason = "exception";
                    st.thread = ee.thread();
                    st.location = ee.location();
                    st.stopInfo = BridgeEval.exceptionInfo(ee);
                    BridgeSnapshot.trackChanges(st);
                    stop = parkSnapshot(st, vm);
                } else if (event instanceof com.sun.jdi.event.ModificationWatchpointEvent) {
                    com.sun.jdi.event.ModificationWatchpointEvent we =
                            (com.sun.jdi.event.ModificationWatchpointEvent) event;
                    try { bump(st, "watch|" + we.field().declaringType().name() + "." + we.field().name()); } catch (Exception ignored) {}
                    if (stop != null) continue;
                    st.thread = we.thread();
                    st.location = we.location();
                    parkReason = "watch";
                    st.stopInfo = BridgeEval.watchInfo(we.field(), "write", we.valueToBe());
                    BridgeSnapshot.trackChanges(st);
                    stop = parkSnapshot(st, vm);
                } else if (event instanceof com.sun.jdi.event.AccessWatchpointEvent) {
                    com.sun.jdi.event.AccessWatchpointEvent we =
                            (com.sun.jdi.event.AccessWatchpointEvent) event;
                    try { bump(st, "watch|" + we.field().declaringType().name() + "." + we.field().name()); } catch (Exception ignored) {}
                    if (stop != null) continue;
                    st.thread = we.thread();
                    st.location = we.location();
                    parkReason = "watch";
                    st.stopInfo = BridgeEval.watchInfo(we.field(), "read", we.valueCurrent());
                    BridgeSnapshot.trackChanges(st);
                    stop = parkSnapshot(st, vm);
                } else if (event instanceof com.sun.jdi.event.MethodExitEvent) {
                    com.sun.jdi.event.MethodExitEvent me = (com.sun.jdi.event.MethodExitEvent) event;
                    if (!BridgeEval.wantedExit(st.cfg, me)) continue;
                    try { bump(st, "exit|" + me.method().declaringType().name() + "." + me.method().name()); } catch (Exception ignored) {}
                    if (stop != null) continue;
                    parkReason = "exit";
                    st.thread = me.thread();
                    st.location = me.location();
                    st.stopInfo = BridgeEval.exitInfo(me);
                    BridgeSnapshot.trackChanges(st);
                    stop = parkSnapshot(st, vm);
                } else if (event instanceof ClassPrepareEvent) {
                    ClassPrepareEvent cp = (ClassPrepareEvent) event;
                    try { cp.request().disable(); } catch (Exception ignored) {}
                    try {
                        plantPending(vm, st.cfg, cp.referenceType(), st.planted);
                        plantCaptureForClass(st, cp.referenceType());
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
                notePark(st, parkReason);
                publishState(st, true);
                return stop;
            }
            set.resume();
            } // synchronized (st.sessionLock): one pump's set is fully
              // reconciled (flags, bounded JDI reads, hit counts) before any
              // rival dispatch observes it; the blocking remove and the
              // unbounded evaluation above stay out.
        }
        } finally {
            if (!idle) st.pumpLock.unlock();
        }
    }

    /** Per-event evaluation verdict, computed OUTSIDE sessionLock. */
    static class PreEval {
        Event event;
        String cond; // condition found for this event (null = none)
        boolean condPass = true; // false only when a found cond failed
        List<String> logLines; // null = no logpoint fire for this event
        boolean wanted = true; // exception-filter match (other events: true)
    }

    /**
     * Phase A of event-set processing: unbounded evaluation without holding
     * sessionLock. Condition checks and logpoint template renders may call
     * invokeMethod (worker join up to 10s) — that must never block live
     * reads. No st.* field is read or written here except via the narrow,
     * exception-safe consults below (cfg maps are append-only while a pump
     * runs: rivals busy-reject, so the worst case is a benign stale read).
     */
    static List<PreEval> preEvaluate(SessionState st, EventSet set) {
        List<PreEval> out = new ArrayList<>(set.size());
        for (Event event : set) {
            PreEval pe = new PreEval();
            pe.event = event;
            try {
                if (event instanceof BreakpointEvent) {
                    BreakpointEvent bp = (BreakpointEvent) event;
                    String cond = BridgeEval.lookupCond(st.cfg, bp.location());
                    if (cond == null) cond = captureCond(st, bp.location());
                    pe.cond = cond;
                    if (cond != null) {
                        try {
                            pe.condPass = BridgeEval.checkCond(bp.thread(), bp.location(), cond);
                        } catch (Exception e) {
                            pe.condPass = false;
                        }
                    }
                    pe.logLines = renderLogLines(st.cfg, bp.thread(), bp.location());
                } else if (event instanceof com.sun.jdi.event.ExceptionEvent) {
                    com.sun.jdi.event.ExceptionEvent ee =
                            (com.sun.jdi.event.ExceptionEvent) event;
                    pe.wanted = matchesExcFilter(st.cfg, ee);
                    if (pe.wanted) {
                        String cond = BridgeEval.lookupCond(st.cfg, ee.location());
                        pe.cond = cond;
                        if (cond != null) {
                            try {
                                pe.condPass = BridgeEval.checkCond(ee.thread(), ee.location(), cond);
                            } catch (Exception e) {
                                pe.condPass = false;
                            }
                        }
                        pe.logLines = renderLogLines(st.cfg, ee.thread(), ee.location());
                    }
                }
            } catch (Exception e) {
                // Evaluation-time race or JDI hiccup: reconcile
                // conservatively (a found-but-unevaluated cond never stops;
                // a half-rendered fire is dropped, never half-appended).
                if (pe.cond != null) pe.condPass = false;
                pe.logLines = null;
            }
            out.add(pe);
        }
        return out;
    }

    /**
     * Render logpoint templates for one event OUTSIDE sessionLock (holes use
     * the read-only call allowlist but may still invoke, e.g. get()). Null
     * when nothing fires (no templates or no frames). The caller bumps hit
     * counts and appends under the lock via applyLogFire.
     */
    static List<String> renderLogLines(Config cfg, ThreadReference thread, Location loc) {
        try {
            List<String> templates = BridgeEval.matchingTemplates(cfg, loc);
            if (templates.isEmpty()) return null;
            List<StackFrame> frames = BridgeSnapshot.safeFrames(thread);
            if (frames.isEmpty()) return null;
            List<String> out = new ArrayList<>(templates.size());
            for (String t : templates) {
                try {
                    out.add(BridgeEval.renderTemplate(thread, frames.get(0), t));
                } catch (Exception e) {
                    out.add("[logpoint error: " + JdiBridge.shortMsg(e) + "]");
                }
            }
            return out;
        } catch (Exception e) {
            return null;
        }
    }

    /** Phase B logpoint commit (caller holds sessionLock): hit counts plus
     *  the pre-rendered lines. Mirrors fireLogpoints minus the evaluation. */
    static void applyLogFire(SessionState st, List<String> lines, Location loc) {
        if (lines == null) return;
        String cls = "?";
        int line = -1;
        try { cls = loc.declaringType().name(); } catch (Exception ignored) {}
        try { line = loc.lineNumber(); } catch (Exception ignored) {}
        bump(st, "logpoint|" + cls + "|" + line);
        for (String l : lines) appendSessionLog(st, st.dir, l);
    }

    /**
     * Post-timeout parked recheck: if a stop parked between our last remove
     * and this timeout (e.g. a racing consumer that no longer exists), expose
     * the park instead of a spurious timeout. Null when nothing parked.
     */
    static String parkedRecheck(SessionState st) {
        synchronized (st.sessionLock) {
            try {
                if (st.suspended && st.thread != null && st.location != null && st.vm != null) {
                    return BridgeSnapshot.snapshot(st.vm, st.cfg, st.thread,
                            st.location, st.out, st.err);
                }
            } catch (Exception ignored) {}
            return null;
        }
    }

    static class StopTimeout extends BridgeException {
        StopTimeout(String message) { super(message); }
        StopTimeout(String message, String waitContextJson) {
            super(message, waitContextJson);
        }
    }

    // Timeout/capture text helpers live in BridgeSnapshot (M5.2).

    /** Stage a short-lived-target exit during a capture wait as the
     *  truthful armed-wait error (null when e is not an exit).
     *  Reaching the pump means the ephemeral WAS armed (plant confirmed,
     *  or the idempotent already-armed case): the stop never arrived in
     *  time. Exits BEFORE arming wrap at the plant site (before-armed).
     *  Never endpoint-rejected or unreachable. */
    static BridgeException stageCaptureExit(Exception e, boolean planted,
            String spec, SessionState st, long waitStartMs) {
        if (!(e instanceof BridgeException) || e.getMessage() == null) return null;
        String lower = e.getMessage().toLowerCase();
        if (!lower.contains("exit") && !lower.contains("closed")) return null;
        boolean wasPlanted = planted;
        String msg = "target exited before capture hit"
                + (spec != null ? " (" + spec + ")" : "")
                + ": " + e.getMessage();
        return new BridgeException(msg, BridgeSnapshot.captureExitContextJson(
                st, "armed-wait", wasPlanted, spec, waitStartMs));
    }

    // -- layered target identity (M-ID): {debuggee, endpoint,
    // adapter} roles with strict confidence (no flat identity view remains).
    // strict: protocol-confirmed only from JDI VM properties (attach) plus
    // the launch pid when this runtime exposes it; os-corroborated only for
    // the OS-observed listener owner; attach pids stay honestly unavailable
    // (JDI SocketAttach exposes none). No target-code eval anywhere.

    static final int IDENT_TOTAL_CAP = 4096;

    // Request/identity text builders live in BridgeProto (M5.2).

    /** Build the layered identity once the VM handle exists (handshake, both
     *  kinds). Never throws: every unknown reads as structured unavailable,
     *  never a fabricated role. Redacted + capped before return. */
    static void buildTargetIdentity(SessionState st) {
        try {
            buildTargetIdentityInner(st);
        } catch (Exception e) {
            st.cfg.targetIdentityJson = null;
            st.cfg.identityHint = st.cfg.seedHint == null ? "" : st.cfg.seedHint;
        }
    }

    /** One-line redacted hint derived from the layered CLI seed (debuggee
     *  launcher args, then endpoint listener details). Empty for a null
     *  seed; an unavailable note when the seed carries nothing nameable.
     *  Never throws; a malformed seed never fails the bridge. */
    static String seedHint(String seed) {
        if (seed == null) return "";
        try {
            String exe = BridgeProto.jsonString(seed, "executable");
            String cwd = BridgeProto.jsonString(seed, "cwd");
            java.util.List<String> parts = new java.util.ArrayList<>();
            java.util.regex.Matcher m = java.util.regex.Pattern.compile(
                    "\"((?:[^\"\\\\]|\\\\.)*)\"").matcher(
                    BridgeProto.jsonStringArray(seed, "argv") == null ? "" : BridgeProto.jsonStringArray(seed, "argv"));
            while (m.find() && parts.size() < 3) {
                parts.add(m.group(1));
            }
            if (exe == null && parts.isEmpty()) {
                return "target identity unavailable (no independent source)";
            }
            String hint = "target identity: " + (exe == null ? "?" : exe)
                    + " " + String.join(" ", parts)
                    + " (cwd " + (cwd == null ? "?" : cwd) + ")";
            return hint.length() <= 200 ? hint : hint.substring(0, 200);
        } catch (Exception e) {
            return "";
        }
    }

    static void buildTargetIdentityInner(SessionState st) {
        long now = System.currentTimeMillis() / 1000;
        String seed = st.cfg.targetIdentitySeedJson;
        Long ownerPid = BridgeProto.jsonLong(seed, "ownerPid");
        String ownerSource = BridgeProto.jsonString(seed, "source");
        // -- debuggee: JDI VM properties confirm it (safe metadata calls,
        // never target eval). Attach exposes no pid; launch adds the child
        // pid only when this runtime offers it (host JDK 9+; guarded).
        String vmName = null;
        String vmVersion = null;
        try {
            if (st.vm != null) {
                vmName = st.vm.name();
                vmVersion = st.vm.version();
            }
        } catch (Exception ignored) {
            vmName = null;
            vmVersion = null;
        }
        StringBuilder dg = new StringBuilder("{\"kind\":\"process\"");
        Long launchPid = null;
        if ("launch".equals(st.cfg.sessionKind) && st.vm != null) {
            try {
                Process proc = st.vm.process();
                if (proc != null) launchPid = proc.pid();
            } catch (Exception ignored) {
                launchPid = null;
            }
        }
        if (vmName != null) {
            dg.append(",\"name\":").append(JdiBridge.quote(BridgeProto.truncField(vmName)));
        } else {
            dg.append(",\"name\":null");
        }
        if (vmVersion != null) {
            dg.append(",\"version\":").append(JdiBridge.quote(BridgeProto.truncField(vmVersion)));
        }
        if (launchPid != null) {
            dg.append(",\"pid\":").append(launchPid);
        }
        // Strict confidence: protocol-confirmed only when the VM actually
        // exposed its properties; otherwise the whole role is unavailable
        // (never a confirmed-looking shell).
        if (vmName != null) {
            dg.append(",\"source\":\"jdi-vm-props\",\"confidence\":\"protocol-confirmed\"");
        } else {
            dg.append(",\"source\":null,\"confidence\":\"unavailable\"");
        }
        dg.append(",\"observedAt\":").append(now).append(",\"unavailable\":[");
        boolean needComma = false;
        if (vmName == null) {
            dg.append(BridgeProto.unavailableEntry("name", "JDI VM properties not readable"));
            needComma = true;
        }
        if (launchPid == null && !"attach".equals(st.cfg.sessionKind)) {
            if (needComma) dg.append(',');
            dg.append(BridgeProto.unavailableEntry("pid", "launch pid not available from this runtime"));
            needComma = true;
        }
        if ("attach".equals(st.cfg.sessionKind)) {
            if (needComma) dg.append(',');
            dg.append(BridgeProto.unavailableEntry("pid", "JDI SocketAttach exposes no pid"));
        }
        dg.append("]}");
        // -- endpoint: the JDWP listener the CLI attached to (attach) or the
        // launcher args (launch, from the CLI observation). The JDWP port is
        // held by the target JVM itself (in-process), so the OS owner
        // corroborates at most, never confirms.
        StringBuilder ep = new StringBuilder("{\"host\":")
                .append(JdiBridge.quote(st.cfg.host))
                .append(",\"port\":").append(st.cfg.port);
        String ownerArgv = BridgeProto.jsonStringArray(seed, "argv");
        String ownerExe = BridgeProto.jsonString(seed, "executable");
        String ownerCwd = BridgeProto.jsonString(seed, "cwd");
        if (ownerPid != null) ep.append(",\"ownerPid\":").append(ownerPid);
        if (ownerExe != null) {
            ep.append(",\"executable\":").append(JdiBridge.quote(BridgeProto.truncField(ownerExe)));
        }
        if (ownerArgv != null) ep.append(",\"argv\":").append(ownerArgv);
        if (ownerCwd != null) {
            ep.append(",\"cwd\":").append(JdiBridge.quote(BridgeProto.truncField(ownerCwd)));
        }
        ep.append(",\"role\":").append(JdiBridge.quote(
                "attach".equals(st.cfg.sessionKind)
                        ? "listener-owner (the target JVM holds its own JDWP port)"
                        : "launcher-observed (the target JVM holds its own JDWP port)"));
        if (ownerSource != null) {
            ep.append(",\"source\":").append(JdiBridge.quote(BridgeProto.truncField(ownerSource)));
        } else {
            ep.append(",\"source\":null");
        }
        ep.append(",\"confidence\":")
                .append(JdiBridge.quote(ownerPid != null ? "os-corroborated" : "unavailable"))
                .append(",\"observedAt\":").append(now).append(",\"unavailable\":[");
        if (ownerPid == null) {
            ep.append(BridgeProto.unavailableEntry("ownerPid", "no independent pid source"));
        }
        ep.append("]}");
        // -- adapter: JDWP runs in-process — no separate adapter by design.
        String ad = "{\"inProcess\":true,\"confidence\":\"unavailable\""
                + ",\"reason\":" + JdiBridge.quote(
                        "JDWP agent runs in-process; no separate adapter process")
                + ",\"observedAt\":" + now + ",\"unavailable\":[]}";
        String ident = "{\"debuggee\":" + dg + ",\"endpoint\":" + ep + ",\"adapter\":" + ad + "}";
        if (ident.length() > IDENT_TOTAL_CAP) {
            // Over budget only via the embedded CLI argv: drop it (marked)
            // and rebuild; the CLI seed copy is untouched.
            ep = new StringBuilder("{\"host\":")
                    .append(JdiBridge.quote(st.cfg.host))
                    .append(",\"port\":").append(st.cfg.port);
            if (ownerPid != null) ep.append(",\"ownerPid\":").append(ownerPid);
            if (ownerExe != null) {
                ep.append(",\"executable\":").append(JdiBridge.quote(BridgeProto.truncField(ownerExe)));
            }
            if (ownerCwd != null) {
                ep.append(",\"cwd\":").append(JdiBridge.quote(BridgeProto.truncField(ownerCwd)));
            }
            ep.append(",\"role\":").append(JdiBridge.quote("listener-owner"))
                    .append(",\"source\":")
                    .append(ownerSource == null ? "null"
                            : JdiBridge.quote(BridgeProto.truncField(ownerSource)))
                    .append(",\"confidence\":")
                    .append(JdiBridge.quote(ownerPid != null ? "os-corroborated" : "unavailable"))
                    .append(",\"observedAt\":").append(now).append(",\"unavailable\":[");
            if (ownerPid == null) {
                ep.append(BridgeProto.unavailableEntry("ownerPid", "no independent pid source")).append(',');
            }
            ep.append(BridgeProto.unavailableEntry("argv", "dropped: over budget")).append("]}");
            ident = "{\"debuggee\":" + dg + ",\"endpoint\":" + ep + ",\"adapter\":" + ad + "}";
        }
        st.cfg.targetIdentityJson = ident;
        // Debuggee-first one-liner (concise, no root-cause claim).
        String hint;
        if (vmName != null && launchPid != null) {
            hint = "debuggee: " + vmName + " (pid " + launchPid + ", protocol-confirmed)";
        } else if (vmName != null) {
            hint = "debuggee: " + vmName + " (protocol-confirmed)";
        } else {
            hint = st.cfg.seedHint == null ? "" : st.cfg.seedHint;
        }
        st.cfg.identityHint = hint.length() <= 200 ? hint : hint.substring(0, 200);
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

    /** M5: at most this many concurrent connection handlers; overflow is
     *  an immediate rejection, never an unbounded thread/task spawn. One
     *  response per connection, as before. */
    static final int MAX_ACTIVE_HANDLERS = 8;

    static void serveLoop(SessionState st, Path dir) throws Exception {
        st.server.setSoTimeout(100);
        ExecutorService pool = Executors.newFixedThreadPool(MAX_ACTIVE_HANDLERS, r -> {
            Thread t = new Thread(r);
            t.setDaemon(true);
            return t;
        });
        try {
        while (true) {
            // Abandoned (dir rm'd or respawned under our name)? Clean up and
            // vanish; legit flows always close (which returns from here)
            // before removing the dir.
            if (!amOwner(st)) {
                cleanup(st);
                return;
            }
            boolean closing;
            synchronized (st.sessionLock) {
                closing = st.closing;
            }
            if (closing) {
                // Bounded grace for in-flight handlers to flush their
                // aborts, then unconditional exit — no joining a handler
                // that itself awaits a stop. The sleep stays OUTSIDE
                // sessionLock so live reads never stall on it.
                try { Thread.sleep(500); } catch (InterruptedException ignored) {}
                return;
            }
            // Use the same event handler between commands, without a second
            // consumer or shared mutable stop state. A parked VM is not resumed.
            // M5: only while NO resume is outstanding — the outstanding
            // resume's pump owns eventQueue consumption (a second consumer
            // would steal its stop). awaitStopIdle rechecks this atomically
            // and tryLocks the pump, so the check-then-pump here is only a
            // fast path, never the serialization.
            boolean idlePump;
            synchronized (st.sessionLock) {
                idlePump = !st.exited && !st.suspended && st.outstanding == null;
            }
            if (idlePump) {
                try { awaitStopIdle(st, 10); }
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
            boolean overloaded;
            synchronized (st.sessionLock) {
                if (st.closing) {
                    try { sock.close(); } catch (Exception ignored) {}
                    return;
                }
                // Pool-full bypass decision only: the overload read/teardown
                // below runs OUTSIDE the lock (a stalled peer's 5s framing
                // read must never head-of-line-block every other handler).
                overloaded = st.activeHandlers >= MAX_ACTIVE_HANDLERS;
                if (!overloaded) st.activeHandlers++;
            }
            if (overloaded) {
                serveOverload(st, sock);
                continue;
            }
            pool.execute(() -> handleOne(st, sock));
        }
        } finally {
            pool.shutdownNow();
        }
    }

    /** Serve one CLI connection: exactly one request and one response.
     *  Socket IO never holds sessionLock; dispatch/cases take it for
     *  bounded sections only. A client disconnect drops only its own
     *  response — target-side work still publishes. */
    // overloadedJson lives in BridgeProto (M5.2).

    /** Pool-full bypass: one bounded frame read under the existing framing
     *  limits/deadlines (never an unbounded wait). An exact `close` gets
     *  terminal close handling outside the pool — close can never be
     *  starved by admitted handlers. Anything else gets the existing
     *  overloaded rejection; malformed/timeout reads just close the
     *  socket. The pool counter is untouched (this path never counted). */
    static void serveOverload(SessionState st, Socket sock) {
        String req;
        try {
            sock.setSoTimeout(5000);
            req = BridgeProto.readFrame(sock.getInputStream());
        } catch (Exception ignored) {
            try { sock.close(); } catch (Exception ignored2) {}
            return;
        }
        String cmd;
        try {
            cmd = BridgeProto.parseCmd(req);
        } catch (Exception ignored) {
            try { sock.close(); } catch (Exception ignored2) {}
            return;
        }
        if ("close".equals(cmd)) {
            closeFromConn(st, sock);
            return;
        }
        try {
            BridgeProto.writeFrame(sock.getOutputStream(), BridgeProto.overloadedJson());
        } catch (Exception ignored) {}
        try { sock.close(); } catch (Exception ignored) {}
    }

    /** Terminal close handling for one connection, outside the handler
     *  pool: every close gets the closed ACK; exactly one winner runs the
     *  teardown (check-and-set under sessionLock). The pool counter is
     *  untouched. */
    static void closeFromConn(SessionState st, Socket sock) {
        boolean mine;
        synchronized (st.sessionLock) {
            mine = !st.closing;
            st.closing = true;
        }
        try {
            BridgeProto.writeFrame(sock.getOutputStream(), "{\"ok\":true,\"closed\":true,\"target\":\"main\"}");
        } catch (Exception ignored) {}
        try { sock.close(); } catch (Exception ignored) {}
        if (mine) cleanup(st);
    }
    static void handleOne(SessionState st, Socket sock) {
        try {
            sock.setSoTimeout(5000);
            String req;
            try {
                req = BridgeProto.readFrame(sock.getInputStream());
            } catch (Exception e) {
                try {
                    BridgeProto.writeFrame(sock.getOutputStream(),
                            "{\"ok\":false,\"error\":" + JdiBridge.quote(JdiBridge.shortMsg(e)) + ",\"target\":\"main\"}");
                } catch (Exception ignored) {}
                return;
            }
            try {
                String resp = dispatch(st, req);
                try {
                    BridgeProto.writeFrame(sock.getOutputStream(), resp);
                } catch (Exception ignored) {
                    // Client went away mid-command: work already ran.
                }
            } catch (CloseSession c) {
                // Terminal and accepted despite any outstanding resume: never
                // wait for a handler that itself awaits a stop — tear down
                // now (launch kills its VM, attach detaches). In-flight
                // resume handlers abort on the torn-down transport.
                closeFromConn(st, sock);
            } catch (Exception e) {
                // Central ok:false envelope: every dispatch failure (busy,
                // closing, stopped/exited, frame validation, unknown cmd,
                // capture stages) names target main here, so no per-case
                // append can be missed or doubled (this string is built
                // fresh and never carries a target yet).
                String errBody = "{\"ok\":false,\"error\":"
                        + JdiBridge.quote(JdiBridge.shortMsg(e));
                if (e instanceof BridgeException
                        && ((BridgeException) e).waitContextJson != null) {
                    errBody += ",\"waitContext\":" + ((BridgeException) e).waitContextJson;
                }
                errBody += ",\"target\":\"main\"}";
                try {
                    BridgeProto.writeFrame(sock.getOutputStream(), errBody);
                } catch (Exception ignored) {}
            }
        } catch (Exception ignored) {
        } finally {
            try { sock.close(); } catch (Exception ignored) {}
            synchronized (st.sessionLock) {
                if (st.activeHandlers > 0) st.activeHandlers--;
            }
        }
    }

    static void cleanup(SessionState st) {
        cleanupCalls++;
        cleanupVm(st);
        try { st.server.close(); } catch (Exception ignored) {}
    }

    /** Teardown invocation count (test seam for close idempotence: every
     *  close ACKs, exactly one winner runs the teardown above). Production
     *  semantics untouched — incremented unconditionally, never read. */
    static int cleanupCalls = 0;

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
            // Null under the lock: every other st.vm read takes it, so a
            // torn-down transport is observed atomically (a racing pump sees
            // either the live VM or null, never a half-closed one).
            synchronized (st.sessionLock) {
                st.vm = null;
            }
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

    /** Validated setup-failure phase for error.json: only exact "config"
     *  or "runtime" stages pass — unknown, null, or missing stages read
     *  as transport (conservative: the CLI keeps endpoint diagnosis
     *  instead of guessing).
     */
    static String setupPhaseOf(String stage) {
        if ("config".equals(stage)) return "config";
        if ("runtime".equals(stage)) return "runtime";
        return "transport";
    }

    /** Park the session on a genuine stop and render its snapshot. The
     *  caller sets thread/location/stopInfo first; this helper marks
     *  suspended and renders. A snapshot throw degrades to a bounded
     *  location-only snapshot with an additive warning instead of
     *  escaping: the park stands (suspended stays true, the EventSet
     *  stays un-resumed, notePark + the stopped publish below still run),
     *  so session.json never claims running for a suspended VM and the
     *  caller can context/continue while cleanup still detaches. The
     *  degradation matches Python/Node (bounded snapshot + warning, the
     *  normal stop continues). Class + short message only in the
     *  warning — never target data. Catches Throwable (not just
     *  Exception): an Error during render must degrade the same way,
     *  never crash the daemon while the VM sits parked. */
    static String parkSnapshot(SessionState st, VirtualMachine vm) {
        st.suspended = true;
        try {
            return BridgeSnapshot.snapshot(vm, st.cfg, st.thread, st.location, st.out, st.err);
        } catch (Throwable e) {
            String loc;
            try {
                loc = BridgeSnapshot.locationJson(st.location, st.cfg);
            } catch (Throwable ignored) {
                loc = "{\"class\":\"?\",\"method\":\"?\",\"line\":-1,\"file\":\"?\",\"snippet\":[]}";
            }
            return "{\"mode\":" + JdiBridge.quote(st.cfg.mode)
                    + ",\"location\":" + loc
                    + ",\"threads\":[],\"frames\":[]"
                    + ",\"snapshotWarning\":" + JdiBridge.quote(
                            "snapshot degraded (" + JdiBridge.shortMsg(e) + "); park stands — context/continue still available")
                    + "}";
        }
    }

    /** True only when a bounded snapshot capped frame-0 vars: locals
     *  rendering marks the cap with a trailing {"name":"…","note":"+N
     *  more"} sentinel (never a real variable name) — same contract as
     *  the Python bridge's frame_locals sentinel. The no-debug-info and
     *  render-error sentinels share the "…" name with a different note,
     *  so they read false (nothing was capped). Matched by regex over
     *  the adjacent name+note pair, never a broad substring. */
    static final java.util.regex.Pattern VARS_CAP_SENTINEL =
            java.util.regex.Pattern.compile("\"name\":\"…\",\"note\":\"\\+\\d+ more\"");

    static boolean snapshotVarsTruncated(String boundedJson) {
        return boundedJson != null && VARS_CAP_SENTINEL.matcher(boundedJson).find();
    }

    /** True when a setup failure is really a vanished target: a JDI
     *  VMDisconnectedException anywhere in the cause chain. Type-based,
     *  never message-matched — those stay transport, every other
     *  failure (checked or unchecked, Exception or Error) reads as
     *  runtime. */
    static boolean isVmDisconnect(Throwable t) {
        for (Throwable c = t; c != null; c = c.getCause()) {
            if (c instanceof com.sun.jdi.VMDisconnectedException) return true;
        }
        return false;
    }

    /** Map a setup catch to the typed failure to persist and throw (the
     *  catch-equivalent path: the setup catch above delegates here, so
     *  tests drive this helper instead of asserting phaseOfError alone).
     *  Usage/Bridge failures pass through untouched; a vanished target
     *  (VMDisconnectedException anywhere in the chain, any throwable
     *  shape) stays a transport BridgeException; anything else becomes a
     *  sanitized RuntimeBridgeException (runtime phase: truthful internal
     *  error, never endpoint-diagnosed). error.json is written from the
     *  mapped value, so the phase derives from its type, never message
     *  text. */
    static Exception mapSetupFailure(Throwable t) {
        if (t instanceof UsageException || t instanceof BridgeException) return (Exception) t;
        if (isVmDisconnect(t)) {
            return new BridgeException(JdiBridge.shortMsg(t));
        }
        return new RuntimeBridgeException(JdiBridge.sanitizeUnexpected(t));
    }

    /** Phase from the failure type: UsageException (CLI-arg/spec validation)
     *  and ConfigBridgeException (arm-time semantic validation) read as
     *  config; RuntimeBridgeException and any other unexpected failure
     *  (checked or unchecked, Exception or Error) read as runtime
     *  (truthful internal error, never endpoint diagnosis); JDI transport
     *  losses, VM disconnects, and target exits stay transport. Never
     *  message text, never a stage timer.
     */
    static String phaseOfError(Throwable t) {
        if (t == null) return "transport";
        if (t instanceof UsageException || t instanceof ConfigBridgeException) return "config";
        if (t instanceof RuntimeBridgeException) return "runtime";
        if (t instanceof BridgeException) return "transport";
        return "runtime";
    }

    /** error.json body: schemaVersion 2, the message verbatim, plus the
     *  additive phase. */
    static String setupErrorJson(String message, String stage) {
        return "{\"schemaVersion\":" + 2
                + ",\"error\":" + JdiBridge.quote(message)
                + ",\"phase\":" + JdiBridge.quote(setupPhaseOf(stage)) + "}";
    }

    /** error.json body with the phase derived from the failure type. */
    static String setupErrorJson(Throwable t, String message) {
        return "{\"schemaVersion\":" + 2
                + ",\"error\":" + JdiBridge.quote(message)
                + ",\"phase\":" + JdiBridge.quote(phaseOfError(t)) + "}";
    }

    // busyError lives in BridgeProto (M5.2; caller holds sessionLock).

    static String dispatch(SessionState st, String reqJson) throws Exception {
        // cmd first (depth-aware): breaksAdd carries a JSON array the flat
        // parser cannot hold, so it branches before flat parsing.
        String cmd = BridgeProto.parseCmd(reqJson);
        // M5 acceptance (atomic under sessionLock): close is always served;
        // a closing session fails the rest fast; rivals busy-reject; the
        // resume registers BEFORE any JDI so a simultaneous rival observes
        // it. Locking below is per bounded section — never across waits.
        synchronized (st.sessionLock) {
            if (st.closing && !cmd.equals("close")) {
                throw new BridgeException("session is closing");
            }
            String busy = BridgeProto.busyError(st, cmd);
            if (busy != null) throw new BridgeException(busy);
            if (cmd.equals("continue") || cmd.equals("step")
                    || cmd.equals("wait") || cmd.equals("capture")) {
                st.outstanding = cmd;
            }
        }
        boolean resume = cmd.equals("continue") || cmd.equals("step")
                || cmd.equals("wait") || cmd.equals("capture");
        try {
            return dispatchInner(st, reqJson, cmd);
        } finally {
            if (resume) {
                synchronized (st.sessionLock) {
                    if (cmd.equals(st.outstanding)) st.outstanding = null;
                }
            }
        }
    }

    static String dispatchInner(SessionState st, String reqJson, String cmd) throws Exception {
        if (cmd.equals("breaksAdd")) {
            synchronized (st.sessionLock) {
                return breaksAddJson(st, BridgeProto.parseStringArray(reqJson, "breaks"));
            }
        }
        if (cmd.equals("breaksRemove")) {
            synchronized (st.sessionLock) {
                return breaksRemoveJson(st, BridgeProto.parseStringArray(reqJson, "breaks"));
            }
        }
        if (cmd.equals("breaksClear")) {
            synchronized (st.sessionLock) {
                return breaksClearJson(st);
            }
        }
        Map<String, String> req = BridgeProto.parseJsonObject(reqJson);
        long timeout = req.containsKey("timeout")
                ? BridgeCli.timeoutMillis(req.get("timeout")) : st.cfg.timeoutMs;
        switch (cmd) {
            case "close": throw new CloseSession();
            case "threads": {
                // Momentary freeze for an instant thread dump. Balanced
                // suspend/resume pair: a stopped session stays stopped.
                // M5: while a resume is outstanding the dump serves the
                // published running truth with NO JDI (the pump owns the
                // event queue) — prompt and never stale.
                synchronized (st.sessionLock) {
                if (st.exited) throw new BridgeException("target VM has exited — close this session");
                if (st.outstanding != null) {
                    // The pump owns the event queue: no fresh JDI dump.
                    // Serve the last-known roster (null until the first full
                    // dump) with an honest running:true — prompt, never
                    // stale-shaped, never blocking delivery.
                    String cached = st.cachedThreads;
                    return "{\"ok\":true,\"running\":true,\"threads\":"
                            + (cached == null ? "[]" : cached) + ",\"target\":\"main\"}";
                }
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
                st.cachedThreads = dump;
                return "{\"ok\":true,\"running\":" + (!wasSuspended) + ",\"threads\":" + dump + ",\"target\":\"main\"}";
                }
            }
            case "breaks": {
                // Arm-time intent with live plant state, no stop required.
                synchronized (st.sessionLock) {
                if (st.exited) throw new BridgeException("target VM has exited — close this session");
                return breaksJson(st);
                }
            }
            case "logs": {
                synchronized (st.sessionLock) {
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
                        + ",\"dropped\":" + st.logDropped + ",\"lines\":" + toJsonArray(lines) + ",\"target\":\"main\"}";
                }
            }
            case "context": {
                synchronized (st.sessionLock) {
                requireStopped(st);
                return "{\"ok\":true,\"stopInfo\":" + stopInfoJson(st)
                        + ",\"location\":" + BridgeSnapshot.locationJson(st.location, st.cfg)
                        + ",\"threads\":" + BridgeSnapshot.threadsJson(st.vm, st.thread)
                        + ",\"frames\":" + BridgeSnapshot.framesJson(st.thread, true)
                        + ",\"diag\":" + stopDiagJson(st)
                        + ",\"warning\":" + JdiBridge.quote(PARK_WARNING)
                        + ",\"target\":\"main\"}";
                }
            }
            case "stack": {
                synchronized (st.sessionLock) {
                requireStopped(st);
                return "{\"ok\":true,\"frames\":" + BridgeSnapshot.framesJson(st.thread, false) + ",\"target\":\"main\"}";
                }
            }
            case "vars": {
                synchronized (st.sessionLock) {
                requireStopped(st);
                List<StackFrame> frames = BridgeSnapshot.safeFrames(st.thread);
                int frame = parseFrameIndex(req, frames, "vars");
                return "{\"ok\":true,\"frame\":" + frame + ",\"locals\":" + BridgeSnapshot.localsJson(frames.get(frame)) + ",\"target\":\"main\"}";
                }
            }
            case "eval": {
                synchronized (st.sessionLock) {
                requireStopped(st);
                String expr = req.get("expr");
                if (expr == null) throw new BridgeException("eval needs an expr");
                List<StackFrame> frames = BridgeSnapshot.safeFrames(st.thread);
                int frame = parseFrameIndex(req, frames, "eval");
                String value = BridgeEval.evalExpr(st.thread, frames.get(frame), expr);
                return "{\"ok\":true,\"expr\":" + JdiBridge.quote(expr) + ",\"value\":" + JdiBridge.quote(value) + ",\"target\":\"main\"}";
                }
            }
            case "continue": {
                synchronized (st.sessionLock) {
                    requireLive(st);
                    if (st.suspended) st.vm.resume();
                    st.suspended = false;
                    publishState(st, false);
                }
                // M5: the stop wait never holds sessionLock — live reads
                // stay prompt and the single event-queue consumer is this
                // pump (the serve loop skips its idle pump while outstanding).
                String snap = awaitStop(st, timeout);
                synchronized (st.sessionLock) {
                    return "{\"ok\":true,\"stopped\":true," + BridgeSnapshot.changeFieldsJson(st) + ",\"stopInfo\":" + stopInfoJson(st) + ",\"snapshot\":" + snap
                            + ",\"diag\":" + stopDiagJson(st)
                            + ",\"warning\":" + JdiBridge.quote(PARK_WARNING)
                            + ",\"target\":\"main\"}";
                }
            }
            case "step": {
                com.sun.jdi.request.StepRequest sr;
                synchronized (st.sessionLock) {
                requireLive(st);
                // Stepping needs a stopped thread to step from (uniform
                // contract on all bridges); continuing works from running
                // (it just waits for the next stop).
                requireStopped(st);
                String mode = req.getOrDefault("mode", "over");
                if (!mode.equals("over") && !mode.equals("into") && !mode.equals("out")) {
                    throw new BridgeException("unknown step mode: " + mode + " (want over|into|out)");
                }
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
                if (st.suspended) st.vm.resume();
                st.suspended = false;
                publishState(st, false);
                }
                try {
                    String snap = awaitStop(st, timeout);
                    synchronized (st.sessionLock) {
                        return "{\"ok\":true,\"stopped\":true," + BridgeSnapshot.changeFieldsJson(st) + ",\"stopInfo\":" + stopInfoJson(st) + ",\"snapshot\":" + snap
                                + ",\"diag\":" + stopDiagJson(st)
                                + ",\"warning\":" + JdiBridge.quote(PARK_WARNING)
                                + ",\"target\":\"main\"}";
                    }
                } finally {
                    synchronized (st.sessionLock) {
                        try { st.vm.eventRequestManager().deleteEventRequest(sr); } catch (Exception ignored) {}
                    }
                }
            }
            case "wait": {
                // Pure long-poll: NEVER resumes. Immediate success when
                // already parked; otherwise awaitStop without any resume.
                // Timeout preserves session/intents (typed message).
                synchronized (st.sessionLock) {
                    requireLive(st);
                    if (st.suspended) {
                        requireStopped(st);
                        String snap = BridgeSnapshot.snapshot(
                                st.vm, st.cfg, st.thread, st.location, st.out, st.err);
                        return waitJson(st, snap, false);
                    }
                }
                String snap = awaitStop(st, timeout, null, true);
                synchronized (st.sessionLock) {
                    return waitJson(st, snap, true);
                }
            }
            case "capture": {
                // One-shot bounded stop. Pre-parked: collect WITHOUT
                // resuming. Fresh: collect, REMOVE EPHEMERAL BEFORE RESUME,
                // auto-resume within the pause budget (overrun still
                // resumes, then reports). Collection/removal failure still
                // resumes; timeout never resumes (nothing parked). No eval,
                // no persisted vars.
                long[] budgetOut = new long[1];
                String[] specOut = new String[1];
                int[] bounds = parseCaptureBounds(req, budgetOut, specOut);
                int framesN = bounds[0];
                int varsN = bounds[1];
                long budgetMs = budgetOut[0];
                AddedLine cap = null;
                if (specOut[0] != null) {
                    String raw = specOut[0];
                    String head = raw.contains("|")
                            ? raw.substring(0, raw.indexOf('|')) : raw;
                    if (head.equals("exc") || head.startsWith("exc:")
                            || head.startsWith("method:")) {
                        throw new BridgeException(
                                "capture takes line breaks only (got '" + raw + "')");
                    }
                    try {
                        cap = parseAddedLine(raw);
                    } catch (UsageException ue) {
                        throw new BridgeException(ue.getMessage());
                    }
                }
                boolean prepark;
                try {
                    synchronized (st.sessionLock) {
                        requireLive(st);
                        prepark = st.suspended;
                    }
                } catch (BridgeException e) {
                    // Session already gone before the capture arrived
                    // (short-lived target): keep the truthful exited
                    // message verbatim, attach the additive stage.
                    if (e.getMessage() != null
                            && e.getMessage().toLowerCase().contains("exit")
                            && e.waitContextJson == null) {
                        synchronized (st.sessionLock) {
                            e.waitContextJson = BridgeSnapshot.captureExitContextJson(
                                    st, "session-gone", false, specOut[0],
                                    System.currentTimeMillis());
                        }
                    }
                    throw e;
                }
                if (prepark) {
                    synchronized (st.sessionLock) {
                        requireStopped(st);
                        String snap = BridgeSnapshot.snapshotBounded(st.vm, st.cfg,
                                st.thread, st.location, st.out, st.err, framesN, varsN);
                        int total = BridgeSnapshot.safeFrames(st.thread).size();
                        return "{\"ok\":true,\"stopped\":true,"
                                + "\"targetWasPaused\":true,\"resumed\":false,"
                                + "\"pauseDurationMs\":0,\"pauseBudgetMs\":" + budgetMs + ","
                                + "\"ephemeralPlanted\":false,"
                                + "\"truncated\":{\"frames\":" + (total > framesN)
                                + ",\"vars\":" + snapshotVarsTruncated(snap) + "},"
                                + "\"snapshot\":" + snap
                                + ",\"diag\":" + stopDiagJson(st)
                                + ",\"warning\":" + JdiBridge.quote(PARK_WARNING)
                                + ",\"target\":\"main\"}";
                    }
                }
                // Fresh path: plant first (failure parks nothing, so no
                // resume is owed). A stop that lands between the check and
                // the plant is impossible (the outstanding slot owns the
                // event queue), but re-check defensively: a live park
                // becomes a prepark collect.
                boolean planted;
                synchronized (st.sessionLock) {
                    requireLive(st);
                    if (st.suspended) {
                        requireStopped(st);
                        String snap = BridgeSnapshot.snapshotBounded(st.vm, st.cfg,
                                st.thread, st.location, st.out, st.err, framesN, varsN);
                        int total = BridgeSnapshot.safeFrames(st.thread).size();
                        return "{\"ok\":true,\"stopped\":true,"
                                + "\"targetWasPaused\":true,\"resumed\":false,"
                                + "\"pauseDurationMs\":0,\"pauseBudgetMs\":" + budgetMs + ","
                                + "\"ephemeralPlanted\":false,"
                                + "\"truncated\":{\"frames\":" + (total > framesN)
                                + ",\"vars\":" + snapshotVarsTruncated(snap) + "},"
                                + "\"snapshot\":" + snap
                                + ",\"diag\":" + stopDiagJson(st)
                                + ",\"warning\":" + JdiBridge.quote(PARK_WARNING)
                                + ",\"target\":\"main\"}";
                    }
                    try {
                        planted = plantCaptureBreak(st, cap);
                    } catch (BridgeException e) {
                        // Plant failure from a dying VM: the target exited
                        // BEFORE the ephemeral was armed — wrapped
                        // truthfully (spec/condition errors stay verbatim:
                        // the target did not exit).
                        if (e.getMessage() != null
                                && e.getMessage().toLowerCase().contains("exit")
                                && e.waitContextJson == null) {
                            throw new BridgeException(
                                    "capture target exited before ephemeral "
                                    + "breakpoint was armed: " + e.getMessage(),
                                    BridgeSnapshot.captureExitContextJson(st, "before-armed",
                                            false, specOut[0],
                                            System.currentTimeMillis()));
                        }
                        throw e;
                    }
                }
                String snap;
                long waitStartMs = System.currentTimeMillis();
                try {
                    snap = awaitStop(st, timeout, specOut[0], true);
                } catch (Exception e) {
                    // Timeout/exit: nothing parked by us — no resume — but
                    // the ephemeral must not leak. A removal failure on a
                    // dead VM must not mask the exit stage either: the
                    // staged error carries the removal note with its
                    // waitContext.
                    synchronized (st.sessionLock) {
                        try {
                            unplantCaptureBreak(st);
                        } catch (Exception ue) {
                            BridgeException staged = stageCaptureExit(
                                    e, planted, specOut[0], st, waitStartMs);
                            if (staged != null) {
                                throw new BridgeException(staged.getMessage()
                                        + "; capture ephemeral may still be planted"
                                        + " (breaks remove to clear)",
                                        staged.waitContextJson);
                            }
                            String ctx = (e instanceof BridgeException)
                                    ? ((BridgeException) e).waitContextJson : null;
                            throw new BridgeException(e.getMessage()
                                    + "; capture ephemeral may still be planted"
                                    + " (breaks remove to clear)", ctx);
                        }
                    }
                    boolean wasPlanted = planted && cap != null;
                    if (e instanceof StopTimeout
                            && ((BridgeException) e).waitContextJson != null) {
                        // Reaching the pump means the ephemeral WAS armed
                        // (plant confirmed, or the idempotent already-armed
                        // case) and the stop simply never arrived — never
                        // an endpoint verdict, never "unreachable".
                        ((BridgeException) e).waitContextJson = BridgeSnapshot.withCaptureStage(
                                ((BridgeException) e).waitContextJson,
                                "armed-wait-timeout", wasPlanted);
                        throw e;
                    }
                    BridgeException staged = stageCaptureExit(
                            e, planted, specOut[0], st, waitStartMs);
                    if (staged != null) throw staged;
                    throw e;
                }
                synchronized (st.sessionLock) {
                    long parkMs = st.parkedAtMs;
                    String snapErr = null;
                    String removeErr = null;
                    String resumeErr = null;
                    String bounded;
                    try {
                        bounded = BridgeSnapshot.snapshotBounded(st.vm, st.cfg,
                                st.thread, st.location, st.out, st.err,
                                framesN, varsN);
                    } catch (Exception e) {
                        snapErr = JdiBridge.shortMsg(e);
                        bounded = "{\"mode\":\"session\",\"location\":"
                                + BridgeSnapshot.locationJson(st.location, st.cfg)
                                + ",\"threads\":[],\"frames\":[]}";
                    }
                    int total = BridgeSnapshot.safeFrames(st.thread).size();
                    // REMOVE EPHEMERAL BEFORE RESUME — even on failure.
                    try {
                        unplantCaptureBreak(st);
                    } catch (Exception e) {
                        removeErr = JdiBridge.shortMsg(e);
                    }
                    // Resume while still marked suspended (honest on
                    // failure: the park stands and resumed:false reports).
                    boolean resumed = false;
                    try {
                        if (st.suspended && st.vm != null) st.vm.resume();
                        st.suspended = false;
                        publishState(st, false);
                        resumed = true;
                    } catch (Exception e) {
                        resumeErr = JdiBridge.shortMsg(e);
                    }
                    long pauseMs = Math.max(0,
                            System.currentTimeMillis() - (parkMs > 0 ? parkMs
                                    : System.currentTimeMillis()));
                    StringBuilder resp = new StringBuilder(
                            "{\"ok\":true,\"stopped\":true,");
                    resp.append("\"targetWasPaused\":false,\"resumed\":").append(resumed);
                    resp.append(",\"pauseDurationMs\":").append(pauseMs);
                    resp.append(",\"pauseBudgetMs\":").append(budgetMs);
                    resp.append(",\"budgetExceeded\":").append(pauseMs > budgetMs);
                    resp.append(",\"ephemeralPlanted\":").append(planted);
                    resp.append(",\"truncated\":{\"frames\":").append(total > framesN);
                    resp.append(",\"vars\":").append(snapshotVarsTruncated(bounded)).append("},");
                    resp.append("\"snapshot\":").append(bounded);
                    resp.append(",\"diag\":").append(stopDiagJson(st));
                    resp.append(",\"warning\":").append(JdiBridge.quote(PARK_WARNING));
                    if (snapErr != null) {
                        resp.append(",\"snapshotError\":").append(JdiBridge.quote(snapErr));
                    }
                    if (removeErr != null) {
                        resp.append(",\"removeError\":").append(JdiBridge.quote(removeErr));
                    }
                    if (resumeErr != null) {
                        resp.append(",\"resumeError\":").append(JdiBridge.quote(resumeErr));
                    }
                    resp.append(",\"target\":\"main\"}");
                    return resp.toString();
                }
            }
            default: throw new BridgeException("unknown cmd: " + cmd);
        }
    }

    /** Parked-stop UX warning (suspend semantics, HTTP handler impact).
     *  No root-cause claim, ever. */
    static final String PARK_WARNING =
            "parked breakpoint suspends target; HTTP handler remains open"
            + " until continue/capture-resume/close(detach)";

    /** Tag on JDI requests planted by a capture ephemeral (line-break tags
     *  stay untouched, so unplant deletes exactly the ephemeral). */
    static final String CAPTURE_TAG = "agent-debugger-capture";

    /** Record one genuine park for stop diagnostics. Caller holds
     *  sessionLock; st.thread/st.location are the winning stop. */
    static void notePark(SessionState st, String reason) {
        String file = "?";
        int line = -1;
        long tid = -1;
        try {
            file = BridgeSnapshot.sourcePath(st.location.declaringType().name());
        } catch (Exception ignored) {}
        try { line = st.location.lineNumber(); } catch (Exception ignored) {}
        try { tid = st.thread.uniqueID(); } catch (Exception ignored) {}
        long now = System.currentTimeMillis();
        Long elapsed = st.prevParkFile == null ? null : now - st.prevParkAtMs;
        boolean sameLoc = file.equals(st.prevParkFile == null ? "" : st.prevParkFile)
                && line == st.prevParkLine;
        boolean sameThread = tid != -1 && tid == st.prevParkThreadId;
        st.stopDiagSeq++;
        st.prevParkFile = file;
        st.prevParkLine = line;
        st.prevParkThreadId = tid;
        st.prevParkAtMs = now;
        st.stopReason = reason;
        st.parkedAtMs = now;
        st.lastStopId = st.stopDiagSeq;
        st.lastSameLoc = sameLoc;
        st.lastSameThread = sameThread;
        st.lastElapsedMs = elapsed;
    }

    /** Additive stop diagnostics for the parked target (caller holds
     *  sessionLock). Native hit ids stay null on JDI (never fabricated);
     *  requested/bound resolve from the armed intent (or the live capture
     *  ephemeral) when attributable, else null. */
    static String stopDiagJson(SessionState st) {
        String cls = "?";
        String file = "?";
        int line = -1;
        String method = "?";
        long tid = -1;
        String tname = null;
        try { cls = st.location.declaringType().name(); } catch (Exception ignored) {}
        try { file = BridgeSnapshot.sourcePath(cls); } catch (Exception ignored) {}
        try { line = st.location.lineNumber(); } catch (Exception ignored) {}
        try { method = st.location.method().name(); } catch (Exception ignored) {}
        try { tid = st.thread.uniqueID(); } catch (Exception ignored) {}
        try { tname = st.thread.name(); } catch (Exception ignored) {}
        String requested = null;
        Integer bound = null;
        Integer hits = null;
        List<Integer> lines = st.cfg.breakpoints.get(cls);
        if (lines != null && lines.contains(line)) {
            String cond = st.cfg.condByLoc.get(cls + ":" + line);
            requested = cls + ":" + line + (cond == null ? "" : "|" + cond);
            bound = line;
            hits = hitsOf(st, "break|" + cls + "|" + line);
        } else {
            List<String> methods = st.cfg.methodBreaks.get(cls);
            if (methods != null && methods.contains(method)) {
                requested = "method:" + cls + "." + method;
                bound = line;
                hits = hitsOf(st, "method|" + cls + "." + method);
            } else if (st.captureCls != null && st.captureCls.equals(cls)
                    && st.captureLine == line) {
                requested = st.captureCls + ":" + st.captureLine
                        + (st.captureCond == null ? "" : "|" + st.captureCond);
                bound = line;
                hits = hitsOf(st, "break|" + cls + "|" + line);
            }
        }
        StringBuilder sb = new StringBuilder("{");
        sb.append("\"stopId\":").append(st.lastStopId);
        sb.append(",\"parkedAtMs\":").append(st.parkedAtMs);
        sb.append(",\"target\":\"main\"");
        sb.append(",\"reason\":").append(st.stopReason == null ? "null"
                : JdiBridge.quote(st.stopReason));
        sb.append(",\"stoppingThread\":{\"id\":").append(tid);
        sb.append(",\"name\":").append(tname == null ? "null" : JdiBridge.quote(tname));
        sb.append("}");
        sb.append(",\"hitBreakpoints\":null");
        sb.append(",\"requestedBreak\":").append(requested == null ? "null"
                : JdiBridge.quote(requested));
        sb.append(",\"boundLine\":").append(bound == null ? "null" : bound);
        sb.append(",\"hitCount\":").append(hits == null ? "null" : hits);
        sb.append(",\"sameLocation\":").append(st.lastSameLoc);
        sb.append(",\"sameThread\":").append(st.lastSameThread);
        sb.append(",\"elapsedSincePreviousStopMs\":").append(st.lastElapsedMs == null
                ? "null" : st.lastElapsedMs);
        // Name the stop without claiming root cause.
        sb.append(",\"file\":").append(JdiBridge.quote(file));
        sb.append(",\"line\":").append(line);
        sb.append(",\"method\":").append(JdiBridge.quote(method));
        return sb.append('}').toString();
    }

    static String waitJson(SessionState st, String snap, boolean waited) {
        return "{\"ok\":true,\"stopped\":true,\"waited\":" + waited
                + "," + BridgeSnapshot.changeFieldsJson(st)
                + ",\"stopInfo\":" + stopInfoJson(st)
                + ",\"snapshot\":" + snap
                + ",\"diag\":" + stopDiagJson(st)
                + ",\"warning\":" + JdiBridge.quote(PARK_WARNING)
                + ",\"target\":\"main\"}";
    }

    /** Capture bounds from the flat request map (bridge-side enforcement;
     *  the CLI mirrors). Returns {frames, vars}; budget and spec ride out
     *  via the single-element holders. */
    static int[] parseCaptureBounds(Map<String, String> req, long[] budgetOut,
            String[] specOut) throws BridgeException {
        int frames;
        int vars;
        long budget;
        try {
            frames = req.containsKey("frames") ? Integer.parseInt(req.get("frames")) : 1;
            vars = req.containsKey("vars") ? Integer.parseInt(req.get("vars")) : 20;
            budget = req.containsKey("pauseBudgetMs") ? Long.parseLong(req.get("pauseBudgetMs"))
                    : 2000;
        } catch (NumberFormatException nfe) {
            throw new BridgeException("capture needs integer frames/vars/pauseBudgetMs");
        }
        if (frames < 1 || frames > 10) {
            throw new BridgeException("capture frames must be between 1 and 10");
        }
        if (vars < 1 || vars > 20) {
            throw new BridgeException("capture vars must be between 1 and 20");
        }
        if (budget < 1 || budget > 10000) {
            throw new BridgeException("capture pause budget must be between 1 and 10000 ms");
        }
        budgetOut[0] = budget;
        String spec = req.get("break");
        specOut[0] = spec;
        return new int[]{frames, vars};
    }

    /** A capture-ephemeral stop at this location? (cfg intent untouched.) */
    static boolean isCaptureBreak(SessionState st, Location loc) {
        if (st.captureCls == null) return false;
        try {
            return st.captureCls.equals(loc.declaringType().name())
                    && st.captureLine == loc.lineNumber();
        } catch (Exception e) {
            return false;
        }
    }

    /** The capture-ephemeral condition for this location, if any. */
    static String captureCond(SessionState st, Location loc) {
        return isCaptureBreak(st, loc) ? st.captureCond : null;
    }

    /** Plant one ephemeral line-only break for a capture. Returns true when
     *  planted (false when the exact line is already armed — idempotent,
     *  nothing to remove). Throws BEFORE anything parks on invalid or
     *  conflicting specs. Never touches cfg intent, stops.json, watches,
     *  or inheritance. Caller holds sessionLock. */
    static boolean plantCaptureBreak(SessionState st, AddedLine p) throws Exception {
        if (p == null) return false;
        if (st.captureCls != null) {
            // A capture ephemeral is already pending (dispatch serializes
            // captures, so this is purely defensive): same spec is
            // idempotent, anything else conflicts.
            if (st.captureCls.equals(p.cls) && st.captureLine == p.line
                    && condEqual(st.captureCond, p.cond)) return false;
            throw new BridgeException("capture already pending for "
                    + st.captureCls + ":" + st.captureLine);
        }
        String loc = p.cls + ":" + p.line;
        List<Integer> have = st.cfg.breakpoints.get(p.cls);
        String haveCond = st.cfg.condByLoc.get(loc);
        if (have != null && have.contains(p.line)) {
            if (condEqual(haveCond, p.cond)) return false; // already armed
            throw new BridgeException("conflicting condition for " + loc + " (already armed"
                    + (haveCond == null ? " plain" : " as '" + haveCond + "'") + "): " + p.raw);
        }
        for (Logpoint lp : st.cfg.logpoints) {
            if (lp.cls.equals(p.cls) && lp.line == p.line) {
                throw new BridgeException("conflicting logpoint for " + loc
                        + " (already armed as logpoint): " + p.raw);
            }
        }
        // Read-only line check for loaded classes (no JDI mutation yet).
        List<ReferenceType> loaded = st.vm.classesByName(p.cls);
        for (ReferenceType rt : loaded) {
            List<Location> locs;
            try {
                locs = rt.locationsOfLine(p.line);
            } catch (AbsentInformationException aie) {
                throw new BridgeException("class " + p.cls
                        + " has no debug info — recompile with -g");
            }
            if (locs.isEmpty()) {
                throw new BridgeException("no executable code at " + p.cls + ":" + p.line);
            }
        }
        st.captureCls = p.cls;
        st.captureLine = p.line;
        st.captureCond = p.cond;
        if (!loaded.isEmpty()) {
            for (ReferenceType rt : loaded) {
                plantCaptureForClass(st, rt);
            }
        } else {
            // Deferred: our OWN tagged ClassPrepareRequest (never the shared
            // watchClass one), so the timeout path deletes exactly the
            // ephemeral's request and nothing armed leaks.
            ClassPrepareRequest req = st.vm.eventRequestManager().createClassPrepareRequest();
            req.addClassFilter(p.cls);
            req.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            req.putProperty(CAPTURE_TAG, p.cls + ":" + p.line);
            req.enable();
        }
        return true;
    }

    /** Plant the live capture ephemeral on one prepared class. */
    static void plantCaptureForClass(SessionState st, ReferenceType rt) throws Exception {
        if (st.captureCls == null || !st.captureCls.equals(rt.name())) return;
        List<Location> locs;
        try {
            locs = rt.locationsOfLine(st.captureLine);
        } catch (AbsentInformationException aie) {
            throw new BridgeException("class " + rt.name()
                    + " has no line info — recompile with -g");
        }
        if (locs.isEmpty()) {
            throw new BridgeException("no executable code at " + rt.name() + ":" + st.captureLine);
        }
        for (Location l : locs) {
            BreakpointRequest bp =
                    st.vm.eventRequestManager().createBreakpointRequest(l);
            bp.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            bp.putProperty(CAPTURE_TAG, st.captureCls + ":" + st.captureLine);
            bp.enable();
        }
    }

    /** Remove a capture ephemeral BEFORE resume. Always clears the fields
     *  (orphaned JDI requests are benign: later pumps skip and resume them,
     *  close drops them). Deletes the tagged breakpoint AND ClassPrepare
     *  requests, so a timed-out deferred capture leaks nothing. Throws on
     *  backend failure — the caller still resumes, then reports removeError.
     *  Caller holds sessionLock. */
    static void unplantCaptureBreak(SessionState st) throws Exception {
        try {
            List<BreakpointRequest> doomed = new ArrayList<>();
            for (BreakpointRequest req : st.vm.eventRequestManager().breakpointRequests()) {
                Object tag = null;
                try { tag = req.getProperty(CAPTURE_TAG); } catch (Exception ignored) {}
                if (tag != null) doomed.add(req);
            }
            for (BreakpointRequest req : doomed) {
                st.vm.eventRequestManager().deleteEventRequest(req);
            }
            List<ClassPrepareRequest> doomedCp = new ArrayList<>();
            for (ClassPrepareRequest req : st.vm.eventRequestManager().classPrepareRequests()) {
                Object tag = null;
                try { tag = req.getProperty(CAPTURE_TAG); } catch (Exception ignored) {}
                if (tag != null) doomedCp.add(req);
            }
            for (ClassPrepareRequest req : doomedCp) {
                st.vm.eventRequestManager().deleteEventRequest(req);
            }
        } finally {
            st.captureCls = null;
            st.captureLine = -1;
            st.captureCond = null;
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
        String identity = st.cfg.targetIdentityJson == null ? "null" : st.cfg.targetIdentityJson;
        BridgeProto.writeFile(st.dir.resolve("session.json"),
                "{\"name\":" + JdiBridge.quote(name)
                + ",\"kind\":" + JdiBridge.quote(st.cfg.sessionKind)
                + ",\"port\":" + port
                + ",\"stopped\":" + stopped
                + ",\"lastStop\":" + (st.lastStopJson == null ? "null" : st.lastStopJson)
                + ",\"updatedAt\":" + now
                + ",\"schemaVersion\":" + 2
                + ",\"targetIdentity\":" + identity + "}");
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

    /** Uniform frame validation shared by vars/eval (same contract on all
     *  four bridges, with one intentional representation gap: the flat
     *  request parser stores every value as a String, so a JSON numeric
     *  {@code 1.0} arrives as {@code "1.0"} — indistinguishable from the
     *  quoted string {@code "1.0"}, which must stay invalid. Java therefore
     *  rejects {@code "1.0"} while the other bridges accept numeric
     *  {@code 1.0}; canonical integer JSON ({@code 1}/{@code "1"}) agrees
     *  everywhere. Absent or JSON null reads as 0; otherwise 1–15 ASCII
     *  digits (longer would lose precision on double-based bridges, so it
     *  is a typed error everywhere). Malformed, fractional, negative, or
     *  over-long input is {@code <what> needs integer frame} (never
     *  coerced, never an internal/parse message); a well-formed index past
     *  the end is {@code no frame N (have M)}. requireStopped still runs
     *  first at the call sites. */
    static int parseFrameIndex(Map<String, String> req, List<StackFrame> frames,
            String what) throws BridgeException {
        int total = frames == null ? 0 : frames.size();
        String raw = req.get("frame");
        if (raw == null || raw.equals("null")) return 0;
        if (!raw.matches("[0-9]{1,15}")) throw new BridgeException(what + " needs integer frame");
        long v = Long.parseLong(raw);
        if (v >= total) throw new BridgeException("no frame " + v + " (have " + total + ")");
        return (int) v;
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
        return "{\"ok\":true,\"stops\":" + stopsArrayJson(st) + ",\"target\":\"main\"}";
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
            st.cfg.breakRaws.putIfAbsent(
                    p.cls + ":" + p.line + "|" + (p.cond == null ? "" : p.cond), p.raw);
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
        return "{\"ok\":true,\"added\":" + added + ",\"stops\":" + stopsArrayJson(st) + ",\"target\":\"main\"}";
    }

    /** Line-break-only parse (mirrors BridgeCli.parseBreakpoint normalization). */
    /**
     * Remove live line breaks by stored identity (running or parked — never
     * suspended/resumed here). Phase 1 matches the whole batch with zero JDI
     * mutation (unparseable or unmatched specs land in `missing`, never
     * ok:false); phase 2 deletes each confirmed break's tagged JDI requests.
     * `removed[]` echoes the persisted stored raws so the CLI drops exactly
     * the confirmed entries. A removed break re-arms a same-line shadowed
     * logpoint via the normal logpoint path (armed now when the class is
     * loaded, deferred watch otherwise, pending + warning on plant failure).
     */
    static String breaksRemoveJson(SessionState st, List<String> raws) throws Exception {
        if (st.exited) throw new BridgeException("target VM has exited — close this session");
        if (raws.isEmpty()) throw new BridgeException("breaks remove needs at least one --break");
        List<AddedLine> matched = new ArrayList<>();
        List<String> missing = new ArrayList<>();
        java.util.Set<String> seen = new java.util.HashSet<>();
        for (String raw : raws) {
            if (raw == null || raw.isEmpty()) throw new BridgeException("bad break spec: " + raw);
            if (!seen.add(raw)) continue; // intra-batch duplicate: idempotent
            AddedLine p;
            try {
                p = parseAddedLine(raw);
            } catch (UsageException ue) {
                missing.add(raw); // malformed can never match stored identity
                continue;
            }
            String loc = p.cls + ":" + p.line;
            List<Integer> have = st.cfg.breakpoints.get(p.cls);
            if (have == null || !have.contains(p.line)
                    || !condEqual(st.cfg.condByLoc.get(loc), p.cond)) {
                // Stored-raw fallback (covers spellings the lexer cannot
                // reproduce); plain specs never match conditional records.
                AddedLine alt = matchStoredRaw(st, raw);
                if (alt == null) {
                    missing.add(raw);
                    continue;
                }
                p = alt;
                loc = p.cls + ":" + p.line;
            }
            boolean dup = false;
            for (AddedLine m : matched) {
                if (m.cls.equals(p.cls) && m.line == p.line && condEqual(m.cond, p.cond)) {
                    dup = true;
                    break;
                }
            }
            if (!dup) matched.add(p);
        }
        if (matched.isEmpty()) {
            return "{\"ok\":true,\"removed\":[],\"missing\":" + toJsonArray(missing)
                    + ",\"stops\":" + stopsArrayJson(st) + ",\"target\":\"main\"}";
        }
        return dropBreakKeys(st, matched, missing);
    }

    /** Stored-raw fallback: the exact persisted string identifies its key
     *  even when the request spelling lexes differently. */
    static AddedLine matchStoredRaw(SessionState st, String raw) {
        for (Map.Entry<String, String> e : st.cfg.breakRaws.entrySet()) {
            if (!e.getValue().equals(raw)) continue;
            String key = e.getKey();
            int bar = key.lastIndexOf('|');
            String loc = bar < 0 ? key : key.substring(0, bar);
            String cond = bar < 0 ? null : key.substring(bar + 1);
            if (cond != null && cond.isEmpty()) cond = null;
            int colon = loc.lastIndexOf(':');
            if (colon <= 0) continue;
            AddedLine p = new AddedLine();
            p.raw = raw;
            p.cls = loc.substring(0, colon);
            try {
                p.line = Integer.parseInt(loc.substring(colon + 1));
            } catch (NumberFormatException nfe) {
                continue;
            }
            p.cond = cond;
            List<Integer> have = st.cfg.breakpoints.get(p.cls);
            if (have != null && have.contains(p.line)
                    && condEqual(st.cfg.condByLoc.get(loc), p.cond)) {
                return p;
            }
        }
        return null;
    }

    static String breaksClearJson(SessionState st) throws Exception {
        if (st.exited) throw new BridgeException("target VM has exited — close this session");
        List<AddedLine> ordered = new ArrayList<>();
        for (Map.Entry<String, List<Integer>> e : st.cfg.breakpoints.entrySet()) {
            for (int line : e.getValue()) {
                AddedLine p = new AddedLine();
                p.cls = e.getKey();
                p.line = line;
                p.cond = st.cfg.condByLoc.get(e.getKey() + ":" + line);
                String key = p.cls + ":" + p.line + "|" + (p.cond == null ? "" : p.cond);
                p.raw = st.cfg.breakRaws.getOrDefault(key, p.cls + ":" + p.line
                        + (p.cond == null ? "" : "|" + p.cond));
                ordered.add(p);
            }
        }
        if (ordered.isEmpty()) {
            return "{\"ok\":true,\"removed\":[],\"stops\":" + stopsArrayJson(st) + ",\"target\":\"main\"}";
        }
        return dropBreakKeys(st, ordered, new ArrayList<>());
    }

    static String dropBreakKeys(SessionState st, List<AddedLine> keys, List<String> missing)
            throws Exception {
        StringBuilder removed = new StringBuilder("[");
        StringBuilder failed = new StringBuilder("[");
        List<String> rearmWarnings = new ArrayList<>();
        boolean rFirst = true;
        boolean fFirst = true;
        int okCount = 0;
        for (AddedLine p : keys) {
            String loc = p.cls + ":" + p.line;
            String storedRaw = st.cfg.breakRaws.getOrDefault(
                    loc + "|" + (p.cond == null ? "" : p.cond), p.raw);
            List<BreakpointRequest> doomed = new ArrayList<>();
            try {
                for (BreakpointRequest req
                        : st.vm.eventRequestManager().breakpointRequests()) {
                    Object tag = null;
                    try { tag = req.getProperty("agent-debugger-break"); } catch (Exception ignored) {}
                    if (tag != null && tag.equals(loc)) doomed.add(req);
                }
                for (BreakpointRequest req : doomed) {
                    st.vm.eventRequestManager().deleteEventRequest(req);
                }
            } catch (Exception e) {
                if (!fFirst) failed.append(',');
                fFirst = false;
                failed.append("{\"raw\":").append(JdiBridge.quote(storedRaw));
                failed.append(",\"spec\":").append(JdiBridge.quote(keySpec(p)));
                failed.append(",\"error\":").append(JdiBridge.quote(JdiBridge.shortMsg(e)));
                failed.append('}');
                continue;
            }
            List<Integer> lines = st.cfg.breakpoints.get(p.cls);
            if (lines != null) {
                lines.remove(Integer.valueOf(p.line));
                if (lines.isEmpty()) st.cfg.breakpoints.remove(p.cls);
            }
            if (p.cond != null) st.cfg.condByLoc.remove(loc);
            st.cfg.breakRaws.remove(loc + "|" + (p.cond == null ? "" : p.cond));
            String warn = rearmShadowedLogpoint(st, p);
            if (warn != null) rearmWarnings.add(warn);
            boolean loaded;
            try {
                loaded = !st.vm.classesByName(p.cls).isEmpty();
            } catch (Exception e) {
                loaded = false;
            }
            if (!rFirst) removed.append(',');
            rFirst = false;
            removed.append("{\"raw\":").append(JdiBridge.quote(storedRaw));
            removed.append(",\"spec\":").append(JdiBridge.quote(keySpec(p)));
            removed.append(",\"kind\":\"break\"");
            removed.append(",\"state\":").append(JdiBridge.quote(loaded ? "verified" : "pending"));
            removed.append(",\"hits\":").append(hitsOf(st, "break|" + p.cls + "|" + p.line));
            removed.append('}');
            // The echo above is recorded: drop the counter so repeated
            // add/remove cycles of distinct lines cannot grow hitCounts
            // for the life of the daemon (a re-added line restarts at 0).
            st.hitCounts.remove("break|" + p.cls + "|" + p.line);
            okCount++;
        }
        if (okCount == 0) {
            if (!failed.toString().equals("[")) {
                throw new BridgeException("breaks remove failed: " + failed + "]");
            }
            return "{\"ok\":true,\"removed\":[],\"missing\":" + toJsonArray(missing)
                    + ",\"stops\":" + stopsArrayJson(st) + ",\"target\":\"main\"}";
        }
        StringBuilder resp = new StringBuilder("{\"ok\":true,\"removed\":");
        resp.append(removed).append(']');
        resp.append(",\"stops\":").append(stopsArrayJson(st));
        if (!missing.isEmpty()) resp.append(",\"missing\":").append(toJsonArray(missing));
        if (!fFirst) {
            resp.append(",\"failed\":").append(failed).append(']');
        }
        // One warning key, deterministically ordered: partial-delete first,
        // then re-arm notes (duplicate keys would let one silently win).
        String warning = removeWarning(!fFirst, rearmWarnings);
        if (warning != null) {
            resp.append(",\"warning\":").append(JdiBridge.quote(warning));
        }
        resp.append(",\"target\":\"main\"}");
        return resp.toString();
    }

    /** Single combined remove/clear warning (null when nothing to report).
     *  Extracted so the one-key contract is checkable without a live VM. */
    static String removeWarning(boolean partialFailed, List<String> rearmWarnings) {
        StringBuilder sb = new StringBuilder();
        if (partialFailed) sb.append("partial remove: some breaks kept");
        if (rearmWarnings != null && !rearmWarnings.isEmpty()) {
            if (sb.length() > 0) sb.append("; ");
            sb.append("re-arm: ").append(String.join("; ", rearmWarnings));
        }
        return sb.length() == 0 ? null : sb.toString();
    }

    static String keySpec(AddedLine p) {
        return p.cls + ":" + p.line + (p.cond == null ? "" : "|" + p.cond);
    }

    /**
     * Re-arm a same-line shadowed logpoint freed by a break removal. The
     * break won, so no plant exists: the current logpoint path runs once —
     * armed now when the class is loaded, deferred watch otherwise, pending
     * + warning when the plant fails. Returns a warning or null.
     */
    static String rearmShadowedLogpoint(SessionState st, AddedLine p) {
        Logpoint shadow = null;
        for (Logpoint lp : st.cfg.logpoints) {
            if (lp.cls.equals(p.cls) && lp.line == p.line) {
                shadow = lp;
                break;
            }
        }
        // Only shadowed logpoints re-arm here: an armed (planted) logpoint
        // on the same line cannot exist (add-time conflict), and planting
        // over one would double-fire.
        if (shadow == null) return null;
        List<ReferenceType> loaded;
        try {
            loaded = st.vm.classesByName(shadow.cls);
        } catch (Exception e) {
            loaded = new ArrayList<>();
        }
        if (loaded.isEmpty()) {
            try {
                watchClass(st.vm, shadow.cls);
            } catch (Exception ignored) {}
            return null; // deferred: plantPending arms it on class load
        }
        try {
            for (ReferenceType rt : loaded) {
                BridgeConn.setLines(st.vm, rt,
                        java.util.Collections.singletonList(shadow.line),
                        EventRequest.SUSPEND_EVENT_THREAD, true);
            }
        } catch (Exception e) {
            return "logpoint " + shadow.cls + ":" + shadow.line
                    + " re-arm failed: " + JdiBridge.shortMsg(e);
        }
        return null;
    }

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
