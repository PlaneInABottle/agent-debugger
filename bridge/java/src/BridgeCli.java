import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.List;

// argv parsing and stop-spec validation for attach/launch/session modes. Moved verbatim from JdiBridge.java.
class BridgeCli {
    static long timeoutMillis(String raw) throws UsageException {
        try {
            long seconds = Long.parseLong(raw);
            if (seconds >= 1 && seconds <= 3600) return seconds * 1000;
        } catch (NumberFormatException ignored) {}
        throw new UsageException("timeout must be between 1 and 3600 seconds");
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
                case "--timeout": cfg.timeoutMs = timeoutMillis(next(argv, ++i, "--timeout")); break;
                case "--dir": cfg.sessionDir = next(argv, ++i, "--dir"); break;
                case "--kind": cfg.sessionKind = next(argv, ++i, "--kind"); break;
                default: throw new UsageException("unknown arg: " + a);
            }
        }
        if (cfg.mode.equals("session")) {
            if (cfg.sessionDir == null) throw new UsageException("session needs --dir");
            if (cfg.sessionKind == null) throw new UsageException("session needs --kind attach|launch");
            // Empty sessions allowed: thread dumps and log collection need no stop.
            BridgeSession.session(cfg);
            return;
        }
        requireAnyBreak(cfg);
        if (cfg.mode.equals("attach")) {
            BridgeConn.attach(cfg);
        } else {
            if (cfg.mainClass == null) throw new UsageException("launch needs --main");
            BridgeConn.launch(cfg);
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
        if (line < 1) throw new UsageException("bad line in --break (must be >= 1): " + spec);
        cfg.breakpoints.computeIfAbsent(cls, k -> new ArrayList<>()).add(line);
        if (cond != null) cfg.condByLoc.put(cls + ":" + line, cond);
    }

    /**
     * Restricted condition language: {@code <path> <op> <literal|null>},
     * op in ==, !=, >, <, >=, <=. Paths are eval paths WITHOUT method calls
     * (a mutating call evaluated 1000x per loop would corrupt state).
     */

    static void validateCond(String cond) throws UsageException {
        if (cond == null || cond.trim().isEmpty()) throw new UsageException("empty condition after '|'");
        // Single operator outside string literals (longest match first).
        String[] ops = {"==", "!=", ">=", "<=", ">", "<"};
        List<int[]> found = new ArrayList<>(); // {pos, len}
        boolean inStr = false;
        for (int i = 0; i < cond.length(); i++) {
            char c = cond.charAt(i);
            if (c == '"' && (i == 0 || cond.charAt(i - 1) != '\\')) { inStr = !inStr; continue; }
            if (inStr) continue;
            boolean matched = false;
            for (String op : ops) {
                if (cond.startsWith(op, i)) {
                    // A two-char op's first char also prefixes a one-char op;
                    // longest-first order + single advance keeps it atomic.
                    found.add(new int[]{i, op.length()});
                    i += op.length() - 1;
                    matched = true;
                    break;
                }
            }
            if (matched) continue;
            // Stray single `!` or `=` outside an operator is never valid.
            if (c == '!' || c == '=') {
                throw new UsageException("invalid operator in condition: " + cond);
            }
        }
        if (inStr) throw new UsageException("unbalanced \" in condition: " + cond);
        if (found.isEmpty()) throw new UsageException("condition needs ==, !=, >, <, >= or <= : " + cond);
        if (found.size() > 1) throw new UsageException("condition takes one comparison only: " + cond);
        int at = found.get(0)[0];
        int len = found.get(0)[1];
        String left = cond.substring(0, at).trim();
        String right = cond.substring(at + len).trim();
        if (left.isEmpty() || right.isEmpty()) {
            throw new UsageException("condition needs <path> <op> <literal|null> (empty side): " + cond);
        }
        validateCondSide(left, cond, true);
        validateCondSide(right, cond, false);
        // Calls: only the read-only allowlist with literal args. Anything else
        // could mutate target state on every loop iteration.
        int i = 0;
        while ((i = cond.indexOf('(', i)) >= 0) {
            int j = i - 1;
            while (j >= 0 && Character.isJavaIdentifierPart(cond.charAt(j))) j--;
            String name = cond.substring(j + 1, i).trim();
            if (!BridgeEval.COND_CALLS.contains(name)) {
                throw new UsageException("conditions cannot call " + name + "() (side-effect risk): " + cond);
            }
            int depth = 1;
            int k = i + 1;
            boolean inS = false;
            while (k < cond.length() && depth > 0) {
                char c = cond.charAt(k);
                if (c == '"' && cond.charAt(k - 1) != '\\') inS = !inS;
                if (!inS) {
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

    /** Light path/literal shape check (not a full parser). String literals
     *  are stripped before punctuation checks so `==` inside quotes is fine. */
    static void validateCondSide(String side, String cond, boolean isLeft) throws UsageException {
        if (side.startsWith("\"")) {
            if (side.length() < 2 || !side.endsWith("\"")) {
                throw new UsageException("unbalanced \" in condition: " + cond);
            }
            return;
        }
        if (!isLeft && (side.equals("null") || side.equals("true") || side.equals("false")
                || side.matches("-?\\d+") || side.matches("-?\\d+\\.\\d+"))) {
            return;
        }
        // Path-ish: must start with an identifier char, never with ./[/)/,/;/=,
        // never end with a dangling dot/paren, no empty segments or stray punctuation.
        char f = side.charAt(0);
        if (!Character.isJavaIdentifierStart(f)) {
            throw new UsageException("bad path in condition: " + cond);
        }
        char l = side.charAt(side.length() - 1);
        if (l == '.' || l == '(' || l == ',' || l == '[') {
            throw new UsageException("bad path in condition: " + cond);
        }
        String bare = stripCondStrings(side);
        if (bare.contains("..") || bare.contains(";;") || bare.contains("==")
                || bare.contains("!=") || bare.contains("&&") || bare.contains("||")
                || bare.contains(">=") || bare.contains("<=")) {
            throw new UsageException("bad path in condition: " + cond);
        }
        for (int k = 0; k < bare.length(); k++) {
            char c = bare.charAt(k);
            // The single comparison operator was split out already; any
            // leftover comparison/stmt char outside strings is malformed.
            if (c == ';' || c == '{' || c == '}' || c == '='
                    || c == '!' || c == '>' || c == '<') {
                throw new UsageException("bad path in condition: " + cond);
            }
        }
    }

    /** Remove double-quoted runs (with backslash escapes) for punctuation checks. */
    static String stripCondStrings(String s) {
        StringBuilder sb = new StringBuilder(s.length());
        boolean inStr = false;
        for (int k = 0; k < s.length(); k++) {
            char c = s.charAt(k);
            if (c == '"' && (k == 0 || s.charAt(k - 1) != '\\')) {
                inStr = !inStr;
                continue;
            }
            if (!inStr) sb.append(c);
        }
        return sb.toString();
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
        if (line < 1) throw new UsageException("bad line in --logpoint (must be >= 1): " + spec);
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
}
