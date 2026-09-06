/** M-ID/M-TC unit parity for the layered target identity + honest
 *  waitContext (no live VM: every case below resolves before touching JDI
 *  transport). Covers: observed-JSON extractors (pid/source/exe/cwd/argv,
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
        cfg.observedHint = "target identity: java Foo (cwd /t)";
        SessionState st = new SessionState();
        st.cfg = cfg;
        return st;
    }

    public static void main(String[] args) {
        // -- extractors over the CLI-redacted observed JSON.
        String obs = "{\"kind\":\"process\",\"pid\":15298,"
                + "\"executable\":\"/usr/bin/java\","
                + "\"argv\":[\"java\",\"-Dtoken=S3CR3T\",\"Foo\"],"
                + "\"cwd\":\"/srv\",\"source\":\"os-proc\",\"observedAt\":1,"
                + "\"unavailable\":[],\"warnings\":[]}";
        check(BridgeSession.jsonLong(obs, "pid") == 15298L, "pid extracted");
        check("os-proc".equals(BridgeSession.jsonString(obs, "source")), "source extracted");
        check("/usr/bin/java".equals(BridgeSession.jsonString(obs, "executable")), "exe extracted");
        check("/srv".equals(BridgeSession.jsonString(obs, "cwd")), "cwd extracted");
        String argv = BridgeSession.jsonStringArray(obs, "argv");
        check(argv != null && argv.startsWith("[") && argv.contains("Foo"), "argv array verbatim");
        check(BridgeSession.jsonLong(obs, "missing") == null, "missing long is null");
        check(BridgeSession.jsonString(obs, "missing") == null, "missing string is null");
        check(BridgeSession.jsonStringArray(obs, "missing") == null, "missing array is null");
        check(BridgeSession.jsonLong("not json", "pid") == null, "malformed long is null");
        check(BridgeSession.jsonStringArray("{\"argv\":null}", "argv") == null, "null array is null");
        check(BridgeSession.jsonStringArray("{\"argv\":{}}", "argv") == null, "object array is null");
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
        st.cfg.observedTargetJson = obs;
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
        st2.cfg.observedTargetJson = null;
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

        if (failures > 0) {
            System.out.println("M7JavaCheck: " + failures + " FAILURES");
            System.exit(1);
        }
        System.out.println("M7JavaCheck: all green");
    }
}
