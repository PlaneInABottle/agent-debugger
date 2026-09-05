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
    }
class SessionState {
        Config cfg;
        VirtualMachine vm;
        StreamGobbler out;
        ServerSocket server;
        ThreadReference thread;
        Location location;
        // All session state is owned by the single serving/event thread.
        boolean suspended;
        boolean exited;
        Map<String, String> lastTop; // top-frame locals at previous stop
        String lastChanged = "[]"; // JSON array of new/changed local names
        String stopInfo; // JSON object describing WHY we stopped (watch/exit/exception)
        java.util.Set<String> planted = new java.util.HashSet<>(); // classes already planted
        Path dir; // session dir (logs.jsonl lives here)
        int logCount;
        String ownerNonce; // session ownership token (see amOwner)
        String lastStopJson; // pre-rendered {"file","line","method"}, null until first stop
        Map<String, Integer> hitCounts = new java.util.HashMap<>(); // hit-key -> stops fired (served by `breaks`)
    }
class CloseSession extends Exception {}
