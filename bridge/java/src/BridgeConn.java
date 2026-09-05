import com.sun.jdi.AbsentInformationException;
import com.sun.jdi.Bootstrap;
import com.sun.jdi.Location;
import com.sun.jdi.ReferenceType;
import com.sun.jdi.VirtualMachine;
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
import com.sun.jdi.request.EventRequest;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

// One-shot attach/launch and drive-to-first-snapshot. Moved verbatim from JdiBridge.java.
class BridgeConn {
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
            throw new BridgeException("attach failed (" + cfg.host + ":" + cfg.port + "): " + JdiBridge.shortMsg(e)
                    + " — is the target started with -agentlib:jdwp=transport=dt_socket,server=y,address=*:" + cfg.port + " ?");
        }
        return vm;
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

    /** Reject tokens the connector cannot represent instead of corrupting argv. */
    static String quoteArg(String s) throws BridgeException {
        if (s.isEmpty() || s.indexOf('"') >= 0 || s.indexOf('\n') >= 0 || s.indexOf('\r') >= 0) {
            throw new BridgeException("JDI launcher cannot preserve empty arguments, double quotes or newlines; launch externally and attach");
        }
        return "\"" + s + "\"";
    }

    static Launched launchVm(Config cfg) throws Exception {
        LaunchingConnector connector = Bootstrap.virtualMachineManager().defaultConnector();
        Map<String, Connector.Argument> args = connector.defaultArguments();
        StringBuilder main = new StringBuilder(cfg.mainClass);
        for (String pa : cfg.programArgs) main.append(' ').append(quoteArg(pa));
        args.get("main").setValue(main.toString());
        args.get("options").setValue("-cp " + quoteArg(cfg.classpath));
        args.get("suspend").setValue("true");
        VirtualMachine vm;
        try {
            vm = connector.launch(args);
        } catch (Exception e) {
            throw new BridgeException("launch failed: " + JdiBridge.shortMsg(e));
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
        BridgeSession.armBreakpoints(vm, cfg);
        List<String> logs = new ArrayList<>();
        java.util.Set<String> planted = new java.util.HashSet<>();

        vm.resume();
        long deadline = System.currentTimeMillis() + cfg.timeoutMs;
        while (true) {
            long remaining = deadline - System.currentTimeMillis();
            if (remaining <= 0) {
                throw new BridgeException("timeout: no breakpoint hit within "
                        + (cfg.timeoutMs / 1000) + "s (breakpoints: " + BridgeEval.describeBreaks(cfg) + ")");
            }
            EventSet set;
            try {
                set = vm.eventQueue().remove(Math.min(remaining, 1000));
            } catch (InterruptedException ie) {
                continue;
            } catch (Exception e) {
                throw new BridgeException("lost connection to target VM: " + JdiBridge.shortMsg(e));
            }
            if (set == null) continue;
            boolean done = false;
            String stopInfo = null;
            for (Event event : set) {
                if (event instanceof BreakpointEvent) {
                    BreakpointEvent bp = (BreakpointEvent) event;
                    BridgeSession.fireLogpoints(null, logs, null, cfg, bp.thread(), bp.location());
                    if (!BridgeSession.hasStoppingBreak(cfg, bp.location())) continue;
                    String cond = BridgeEval.lookupCond(cfg, bp.location());
                    if (cond != null && !BridgeEval.checkCond(bp.thread(), bp.location(), cond)) continue;
                    System.out.println("{\"snapshot\":" + BridgeSnapshot.snapshot(vm, cfg, bp.thread(), bp.location(), out)
                            + ",\"logs\":" + BridgeSession.toJsonArray(logs) + ",\"stopInfo\":null}");
                    done = true;
                } else if (event instanceof ClassPrepareEvent) {
                    ClassPrepareEvent cp = (ClassPrepareEvent) event;
                    try { cp.request().disable(); } catch (Exception ignored) {}
                    try {
                        BridgeSession.plantPending(vm, cfg, cp.referenceType(), planted);
                    } catch (Exception e) {
                        try { set.resume(); } catch (Exception ignored) {}
                        throw e;
                    }
                } else if (event instanceof com.sun.jdi.event.ExceptionEvent) {
                    com.sun.jdi.event.ExceptionEvent ee = (com.sun.jdi.event.ExceptionEvent) event;
                    if (!BridgeSession.matchesExcFilter(cfg, ee)) continue;
                    BridgeSession.fireLogpoints(null, logs, null, cfg, ee.thread(), ee.location());
                    String cond = BridgeEval.lookupCond(cfg, ee.location());
                    if (cond != null && !BridgeEval.checkCond(ee.thread(), ee.location(), cond)) continue;
                    stopInfo = BridgeEval.exceptionInfo(ee);
                    System.out.println("{\"snapshot\":" + BridgeSnapshot.snapshot(vm, cfg, ee.thread(), ee.location(), out)
                            + ",\"logs\":" + BridgeSession.toJsonArray(logs) + ",\"stopInfo\":" + stopInfo + "}");
                    done = true;
                } else if (event instanceof com.sun.jdi.event.ModificationWatchpointEvent) {
                    com.sun.jdi.event.ModificationWatchpointEvent we =
                            (com.sun.jdi.event.ModificationWatchpointEvent) event;
                    stopInfo = BridgeEval.watchInfo(we.field(), "write", we.valueToBe());
                    System.out.println("{\"snapshot\":" + BridgeSnapshot.snapshot(vm, cfg, we.thread(), we.location(), out)
                            + ",\"logs\":" + BridgeSession.toJsonArray(logs) + ",\"stopInfo\":" + stopInfo + "}");
                    done = true;
                } else if (event instanceof com.sun.jdi.event.AccessWatchpointEvent) {
                    com.sun.jdi.event.AccessWatchpointEvent we =
                            (com.sun.jdi.event.AccessWatchpointEvent) event;
                    stopInfo = BridgeEval.watchInfo(we.field(), "read", we.valueCurrent());
                    System.out.println("{\"snapshot\":" + BridgeSnapshot.snapshot(vm, cfg, we.thread(), we.location(), out)
                            + ",\"logs\":" + BridgeSession.toJsonArray(logs) + ",\"stopInfo\":" + stopInfo + "}");
                    done = true;
                } else if (event instanceof com.sun.jdi.event.MethodExitEvent) {
                    com.sun.jdi.event.MethodExitEvent me = (com.sun.jdi.event.MethodExitEvent) event;
                    if (!BridgeEval.wantedExit(cfg, me)) continue;
                    stopInfo = BridgeEval.exitInfo(me);
                    System.out.println("{\"snapshot\":" + BridgeSnapshot.snapshot(vm, cfg, me.thread(), me.location(), out)
                            + ",\"logs\":" + BridgeSession.toJsonArray(logs) + ",\"stopInfo\":" + stopInfo + "}");
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
}
