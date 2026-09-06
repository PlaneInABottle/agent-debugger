import com.sun.jdi.Field;
import com.sun.jdi.Location;
import com.sun.jdi.ThreadReference;
import com.sun.jdi.VirtualMachine;
import java.net.ServerSocket;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

// Session data, config and control-flow exceptions. Moved verbatim from JdiBridge.java; same package, no imports change meaning.
class UsageException extends Exception {
        UsageException(String m) { super(m); }
    }
class BridgeException extends Exception {
        BridgeException(String m) { super(m); }
        // Additive structured context for wait/capture timeouts (the honest
        // trigger-unknown report); pre-rendered JSON object or null. The
        // message keeps the exact frozen timeout prefix either way.
        String waitContextJson = null;
        BridgeException(String m, String waitContextJson) {
            super(m);
            this.waitContextJson = waitContextJson;
        }
    }
class Config {
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
        Map<String, String> breakRaws = new LinkedHashMap<>(); // "cls:line|cond" -> stored raw (remove/clear echo)
        String observedTargetJson = null; // redacted CLI-observed identity (verbatim JSON)
        String observedHint = ""; // one-line redacted diagnostic hint
        String targetIdentityJson = null; // layered {debuggee,endpoint,adapter} (verbatim JSON, redacted+capped)
        String identityHint = ""; // debuggee-first one-liner for timeouts
        List<Logpoint> logpoints = new ArrayList<>();
        List<Watchpoint> watchpoints = new ArrayList<>();
        Map<String, List<String>> exitMethods = new LinkedHashMap<>(); // cls -> methods
        long timeoutMs = 20_000;
        String sessionDir;
        String sessionKind; // "attach" | "launch" for session mode
    }
class Logpoint {
        String cls;
        int line;
        String template;
    }

    /** Field watch: stop when a field is read and/or written. */
class Watchpoint {
        String cls;
        String field;
        boolean onRead;
        boolean onWrite;
    }
class Launched {
        VirtualMachine vm;
        StreamGobbler out;
        StreamGobbler err;
    }
class SessionState {
        Config cfg;
        VirtualMachine vm;
        StreamGobbler out;
        StreamGobbler err;
        ServerSocket server;
        ThreadReference thread;
        Location location;
        // M5: sessionLock serializes session-state access and JDI *mutation*
        // (break plants/removes, suspend/resume, step/delete requests). It is
        // NEVER held across blocking waits (eventQueue.remove, accept,
        // socket IO, sleep). Read-only evaluation (condition checks and
        // logpoint renders, which may invokeMethod with a 10s join) runs
        // OUTSIDE sessionLock — per-set reconciliation under the lock stays
        // bounded and fast, so live reads stay prompt.
        final Object sessionLock = new Object();
        // HIGH: pumpLock serializes EventQueue.remove — exactly one consumer.
        // The dispatch pump (continue/step/wait/capture) holds it for its
        // whole wait; the serveLoop idle pump only tryLocks and skips when a
        // dispatch pump owns delivery. Lock order is pumpLock -> sessionLock,
        // and no path blocks on pumpLock while holding sessionLock (idle uses
        // tryLock only), so this cannot deadlock.
        final java.util.concurrent.locks.ReentrantLock pumpLock =
                new java.util.concurrent.locks.ReentrantLock();
        // All session state below is owned by sessionLock holders.
        boolean suspended;
        boolean exited;
        String outstanding = null; // resume cmd in flight (continue/step)
        int activeHandlers = 0;    // live connection handlers (bounded)
        volatile boolean closing = false; // close accepted: further cmds fail fast
        Map<String, String> lastTop; // top-frame locals at previous stop
        String lastChanged = "[]"; // JSON array of new/changed local names
        String stopInfo; // JSON object describing WHY we stopped (watch/exit/exception)
        java.util.Set<String> planted = new java.util.HashSet<>(); // classes already planted
        Path dir; // session dir (logs.jsonl lives here)
        int logCount; // retained lines on disk (<= MAX_LOG_LINES)
        int logDropped; // lifetime lines evicted by the log ring
        String ownerNonce; // session ownership token (see amOwner)
        String lastStopJson; // pre-rendered {"file","line","method"}, null until first stop
        Map<String, Integer> hitCounts = new java.util.HashMap<>(); // hit-key -> stops fired (served by `breaks`)
        // -- stop diagnostics (UX batch): session-monotonic stop id plus the
        // previous park for same-location/same-thread diagnosis. Owned by
        // sessionLock holders, like every field above.
        long stopDiagSeq = 0; // session-monotonic stop id
        String prevParkFile = null; // previous park file (rel form), null until first park
        int prevParkLine = -1; // previous park line
        long prevParkThreadId = -1; // previous park thread uniqueId
        long prevParkAtMs = 0; // previous park wall clock
        String stopReason = null; // reason of the current park (breakpoint/step/exception/watch/exit)
        long parkedAtMs = 0; // wall clock ms of the current park
        long lastStopId = 0; // stopId of the current park
        boolean lastSameLoc = false; // same file+line as the previous park
        boolean lastSameThread = false; // same thread+target as the previous park
        Long lastElapsedMs = null; // ms since the previous park (null before the second)
        // -- capture ephemeral (UX batch): one line-only break, target-scoped,
        // never in cfg intent (no stops.json, no inheritance). Set while a
        // capture waits, cleared by unplant BEFORE resume (or on timeout).
        // hasStoppingBreak/countBreakHit consult it alongside cfg.
        String captureCls = null;
        int captureLine = -1;
        String captureCond = null;
        // Last full thread dump (served as the roster while a resume owns
        // the pump — the pump owns the event queue, so no fresh JDI dump is
        // taken; running:true stays honest). Null until the first dump.
        String cachedThreads = null;
    }
class CloseSession extends Exception {}
