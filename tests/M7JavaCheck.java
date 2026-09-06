/** M-ID/M-TC unit parity for the layered target identity + honest
 *  waitContext (no live VM: every case below resolves before touching JDI
 *  transport). Covers: seed-JSON extractors (ownerPid/source/exe/cwd/argv,
 *  nulls and malformed shapes), field caps, unavailable entries, the
 *  waitContext JSON shape (frozen prefix intact, triggerStatus unknown,
 *  expectedBreak only when planted, note honesty), the StopTimeout context
 *  carrier, and the attach-role truth (no pid claimed, adapter
 *  in-process).
 *
 *  Compile: javac -d <out> bridge/java/src/*.java tests/M7JavaCheck.java
 *  Run:     java -cp <out> M7JavaCheck
 */
public class M7JavaCheck {
    static int failures = 0;

    static void check(boolean cond, String name) {
        if (cond) {
            System.out.println("ok: " + name);
        } else {
            System.out.println("FAIL: " + name);
            failures++;
        }
    }

    static SessionState state(String kind) {
        Config cfg = new Config();
        cfg.sessionKind = kind;
        cfg.host = "127.0.0.1";
        cfg.port = 5005;
        cfg.seedHint = "target identity: java Foo (cwd /t)";
        SessionState st = new SessionState();
        st.cfg = cfg;
        return st;
    }

    public static void main(String[] args) {
        // -- extractors over the CLI-redacted layered seed JSON.
        String seed = "{\"debuggee\":{\"kind\":\"process\",\"pid\":null,"
                + "\"confidence\":\"unavailable\"},"
                + "\"endpoint\":{\"host\":\"127.0.0.1\",\"port\":5005,\"ownerPid\":15298,"
                + "\"executable\":\"/usr/bin/java\","
                + "\"argv\":[\"java\",\"-Dtoken=[redacted]\",\"Foo\"],"
                + "\"cwd\":\"/srv\",\"source\":\"os-proc\"},"
                + "\"adapter\":{\"confidence\":\"unavailable\"}}";
        check(BridgeSession.jsonLong(seed, "ownerPid") == 15298L, "ownerPid extracted");
        check("os-proc".equals(BridgeSession.jsonString(seed, "source")), "source extracted");
        check("/usr/bin/java".equals(BridgeSession.jsonString(seed, "executable")), "exe extracted");
        check("/srv".equals(BridgeSession.jsonString(seed, "cwd")), "cwd extracted");
        String argv = BridgeSession.jsonStringArray(seed, "argv");
        check(argv != null && argv.startsWith("[") && argv.contains("Foo"), "argv array verbatim");
        check(BridgeSession.jsonLong(seed, "missing") == null, "missing long is null");
        check(BridgeSession.jsonString(seed, "missing") == null, "missing string is null");
        check(BridgeSession.jsonStringArray(seed, "missing") == null, "missing array is null");
        check(BridgeSession.jsonLong("not json", "ownerPid") == null, "malformed long is null");
        check(BridgeSession.jsonStringArray("{\"argv\":null}", "argv") == null, "null array is null");
        check(BridgeSession.jsonStringArray("{\"argv\":{}}", "argv") == null, "object array is null");
        // -- seed-derived hint: debuggee-first, never a spawn failure.
        check(BridgeSession.seedHint(null).equals(""), "null seed hints empty");
        String sh = BridgeSession.seedHint(seed);
        check(sh.contains("/usr/bin/java") && sh.contains("Foo") && sh.contains("[redacted]"),
                "seed hint derives layered, got: " + sh);
        check(BridgeSession.seedHint("not json")
                .contains("unavailable"), "malformed seed hints unavailable");
        // -- caps.
        StringBuilder big = new StringBuilder();
        for (int i = 0; i < 900; i++) big.append('x');
        String capped = BridgeSession.truncField(big.toString());
        check(capped.length() <= 512 + 30 && capped.contains("more chars"), "field cap marks");
        check(BridgeSession.truncField(null) == null, "null cap stays null");
        // -- unavailable entries quote safely.
        String un = BridgeSession.unavailableEntry("pid", "no \"source\" here");
        check(un.contains("\"field\":\"pid\"") && !un.contains("\"source\" here"),
                "unavailable entry escapes");

        // -- attach identity with no VM: every role honest, nothing throws.
        SessionState st = state("attach");
        st.cfg.targetIdentitySeedJson = seed;
        BridgeSession.buildTargetIdentity(st);
        String ident = st.cfg.targetIdentityJson;
        check(ident != null && ident.contains("\"debuggee\"")
                && ident.contains("\"endpoint\"") && ident.contains("\"adapter\""),
                "three roles present");
        check(!ident.contains("\"confidence\":\"protocol-confirmed\""),
                "no vm means no protocol-confirmed claim anywhere");
        check(ident.contains("JDI SocketAttach exposes no pid"), "attach pid unavailable with reason");
        check(ident.contains("\"ownerPid\":15298"), "endpoint corroborates the OS owner pid");
        check(ident.contains("\"confidence\":\"os-corroborated\""), "endpoint corroborated, never confirmed");
        check(ident.contains("no separate adapter"), "adapter in-process by design");
        check(ident.length() <= 4096, "aggregate bounded");
        // Debuggee-first hint falls back to the CLI hint without a VM.
        check(st.cfg.identityHint.equals("target identity: java Foo (cwd /t)"),
                "hint falls back honestly without vm, got: " + st.cfg.identityHint);

        // -- remote/unobserved attach: endpoint unavailable, session usable.
        SessionState st2 = state("attach");
        st2.cfg.targetIdentitySeedJson = null;
        BridgeSession.buildTargetIdentity(st2);
        check(st2.cfg.targetIdentityJson.contains("no independent pid source"),
                "missing observation degrades, never fabricates");

        // -- timeout text keeps the frozen prefix with the hint appended.
        String msg = BridgeSession.timeoutText(st, 2000);
        check(msg.startsWith("timeout: no stop within 2s"), "frozen prefix intact, got: " + msg);
        check(msg.contains("target identity: java Foo"), "hint appended, got: " + msg);

        // -- waitContext JSON: unknown trigger, canonical order, honest note.
        String ctx = BridgeSession.waitContextJson(st, 2000, 1735689600000L, null);
        check(ctx.contains("\"triggerStatus\":\"unknown\""), "trigger unknown");
        check(!ctx.contains("expectedBreak"), "wait plants no expectedBreak");
        check(ctx.contains("\"waitStartedAt\":1735689600"), "wait start stamped");
        check(ctx.contains("\"waitedMs\":"), "waited ms stamped");
        check(ctx.contains("not observed") && ctx.contains("not that the code is unreachable"),
                "honest note, no root-cause claim");
        check(ctx.contains("\"targetIdentity\":"), "identity rides along");
        String ctx2 = BridgeSession.waitContextJson(st, 2000, 1735689600000L, "com.Foo:54");
        check(ctx2.contains("\"expectedBreak\":\"com.Foo:54\""), "capture expectedBreak rides");
        int trigPos = ctx2.indexOf("triggerStatus");
        int expPos = ctx2.indexOf("expectedBreak");
        int identPos = ctx2.indexOf("targetIdentity");
        check(trigPos < expPos && expPos < identPos, "canonical field order");

        // -- StopTimeout carries the context; the message stays exact.
        BridgeSession.StopTimeout t =
                new BridgeSession.StopTimeout("timeout: no stop within 2s", ctx);
        check("timeout: no stop within 2s".equals(t.getMessage()), "message exact");
        check(ctx.equals(t.waitContextJson), "context carried");
        BridgeSession.StopTimeout plain = new BridgeSession.StopTimeout("busy: x outstanding");
        check(plain.waitContextJson == null, "continue/step timeouts carry no context");

        // -- setup-failure phase (error.json `phase`): schemaVersion 2,
        // message verbatim, validated stage, conservative transport default.
        String ej = BridgeSession.setupErrorJson("no method noSuchMethod() in IdleAttach", "config");
        check(ej.contains("\"schemaVersion\":2")
                && ej.contains("\"phase\":\"config\"") && ej.contains("no method noSuchMethod()"),
                "config phase shape, got: " + ej);
        check(BridgeSession.setupErrorJson("attach failed: refused", "transport")
                .contains("\"phase\":\"transport\""), "transport phase shape");
        check(BridgeSession.setupErrorJson("x", "runtime").contains("\"phase\":\"transport\""),
                "unknown stage reads transport");
        check(BridgeSession.setupErrorJson("x", null).contains("\"phase\":\"transport\""),
                "null stage reads transport");

        // -- phase derives from the exception type, never message text or
        // a stage timer: UsageException and ConfigBridgeException read as
        // config; transport losses, disconnects, and unexpected crashes
        // stay transport.
        check(BridgeSession.phaseOfError(new UsageException("bad line")).equals("config"),
                "usage reads config");
        check(BridgeSession.phaseOfError(
                new ConfigBridgeException("no method noSuchMethod() in IdleAttach")).equals("config"),
                "semantic arm failure reads config");
        check(new ConfigBridgeException("x") instanceof BridgeException,
                "config still catches as BridgeException");
        check(BridgeSession.phaseOfError(
                new BridgeException("attach failed (h:1): refused")).equals("transport"),
                "connect loss stays transport");
        check(BridgeSession.phaseOfError(
                new BridgeException("lost connection to target VM: x")).equals("transport"),
                "mid-handshake death stays transport");
        check(BridgeSession.phaseOfError(
                new BridgeException("target VM exited before any breakpoint hit")).equals("transport"),
                "pump target-exit stays transport");
        check(BridgeSession.phaseOfError(new RuntimeException("boom")).equals("transport"),
                "unexpected reads transport");
        check(BridgeSession.phaseOfError(null).equals("transport"),
                "null reads transport");
        String cej = BridgeSession.setupErrorJson(
                new ConfigBridgeException("no method x() in Y"), "no method x() in Y");
        check(cej.contains("\"phase\":\"config\"") && cej.contains("no method x()"),
                "typed payload shape, got: " + cej);
        String tej = BridgeSession.setupErrorJson(
                new BridgeException("target exited"), "target exited");
        check(tej.contains("\"phase\":\"transport\"") && tej.contains("\"schemaVersion\":2"),
                "transport payload shape");

        // -- parse-error scan finds --dir without parsing; the file is config.
        check("/s".equals(BridgeCli.dirFromArgv(
                new String[]{"session", "--dir", "/s", "--break", "a:1"})), "dir scan");
        check("/s".equals(BridgeCli.dirFromArgv(
                new String[]{"session", "--dir=/s"})), "dir= scan");
        check(BridgeCli.dirFromArgv(new String[]{"session", "--break", "a:1"}) == null,
                "no dir reads null");
        try {
            java.nio.file.Path tmp =
                    java.nio.file.Files.createTempDirectory("phase-parse");
            BridgeCli.writeParseError(
                    new String[]{"session", "--dir", tmp.toString(), "--break", "x"},
                    "bad line in --break: x");
            String body = new String(java.nio.file.Files.readAllBytes(
                    tmp.resolve("error.json")), java.nio.charset.StandardCharsets.UTF_8);
            check(body.contains("\"schemaVersion\":2")
                    && body.contains("\"phase\":\"config\"") && body.contains("bad line"),
                    "parse-error file is config, got: " + body);
            // No --dir: never throws, nothing written.
            BridgeCli.writeParseError(new String[]{"session"}, "x");
            // Empty --dir (--dir=) scans as empty and writes nothing (never
            // the process cwd): never throws.
            check("".equals(BridgeCli.dirFromArgv(new String[]{"session", "--dir="})),
                    "empty dir scans empty");
            BridgeCli.writeParseError(new String[]{"session", "--dir="}, "x");
        } catch (Exception e) {
            check(false, "parse-error helpers threw: " + e);
        }

        if (failures > 0) {
            System.out.println("M7JavaCheck: " + failures + " FAILURES");
            System.exit(1);
        }
        System.out.println("M7JavaCheck: all green");
    }
}
