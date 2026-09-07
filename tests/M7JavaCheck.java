/** M-ID/M-TC unit parity for the layered target identity + honest
 *  waitContext (no live VM: every case below resolves before touching JDI
 *  transport). Covers: seed-JSON extractors (ownerPid/source/exe/cwd/argv,
 *  nulls and malformed shapes), field caps, unavailable entries, the
 *  waitContext JSON shape (frozen prefix intact, triggerStatus unknown,
 *  expectedBreak only when planted, note honesty), the StopTimeout context
 *  carrier, and the attach-role truth (no pid claimed, adapter
 *  in-process). Also covers the setup-catch equivalent (mapSetupFailure
 *  over the original throwable), the snapshot-failure park truth
 *  (parkSnapshot degrades, suspended stays true), the bounded-locals
 *  truncation sentinel, and the capture exit/stage context fields
 *  (waitStartedAt/waitedMs).
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

    static SessionState state(String kind) {        Config cfg = new Config();
        cfg.sessionKind = kind;
        cfg.host = "127.0.0.1";
        cfg.port = 5005;
        cfg.seedHint = "target identity: java Foo (cwd /t)";
        SessionState st = new SessionState();
        st.cfg = cfg;
        return st;
    }

    /** JDI mirror stub without a VM: answers named methods, explodes on
     *  anything else (proving no field walk / element fetch / invoke). */
    static Object jdiProxy(Class<?> iface, java.util.Map<String, Object> answers) {
        return java.lang.reflect.Proxy.newProxyInstance(
                M7JavaCheck.class.getClassLoader(),
                new Class<?>[]{iface},
                (proxy, method, args) -> {
                    if (answers.containsKey(method.getName())) {
                        Object ans = answers.get(method.getName());
                        if (ans instanceof Throwable) throw (Throwable) ans;
                        return ans;
                    }
                    if (method.getName().equals("hashCode")) return 1;
                    if (method.getName().equals("equals")) return Boolean.FALSE;
                    if (method.getName().equals("toString")) return "proxy";
                    throw new AssertionError(
                            "unexpected JDI call: " + method.getName());
                });
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
        check(BridgeProto.jsonLong(seed, "ownerPid") == 15298L, "ownerPid extracted");
        check("os-proc".equals(BridgeProto.jsonString(seed, "source")), "source extracted");
        check("/usr/bin/java".equals(BridgeProto.jsonString(seed, "executable")), "exe extracted");
        check("/srv".equals(BridgeProto.jsonString(seed, "cwd")), "cwd extracted");
        String argv = BridgeProto.jsonStringArray(seed, "argv");
        check(argv != null && argv.startsWith("[") && argv.contains("Foo"), "argv array verbatim");
        check(BridgeProto.jsonLong(seed, "missing") == null, "missing long is null");
        check(BridgeProto.jsonString(seed, "missing") == null, "missing string is null");
        check(BridgeProto.jsonStringArray(seed, "missing") == null, "missing array is null");
        check(BridgeProto.jsonLong("not json", "ownerPid") == null, "malformed long is null");
        check(BridgeProto.jsonStringArray("{\"argv\":null}", "argv") == null, "null array is null");
        check(BridgeProto.jsonStringArray("{\"argv\":{}}", "argv") == null, "object array is null");
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
        String capped = BridgeProto.truncField(big.toString());
        check(capped.length() <= 512 + 30 && capped.contains("more chars"), "field cap marks");
        check(BridgeProto.truncField(null) == null, "null cap stays null");
        // -- unavailable entries quote safely.
        String un = BridgeProto.unavailableEntry("pid", "no \"source\" here");
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
        String msg = BridgeSnapshot.timeoutText(st, 2000);
        check(msg.startsWith("timeout: no stop within 2s"), "frozen prefix intact, got: " + msg);
        check(msg.contains("target identity: java Foo"), "hint appended, got: " + msg);

        // -- waitContext JSON: unknown trigger, canonical order, honest note.
        String ctx = BridgeSnapshot.waitContextJson(st, 2000, 1735689600000L, null);
        check(ctx.contains("\"triggerStatus\":\"unknown\""), "trigger unknown");
        check(!ctx.contains("expectedBreak"), "wait plants no expectedBreak");
        check(ctx.contains("\"waitStartedAt\":1735689600"), "wait start stamped");
        check(ctx.contains("\"waitedMs\":"), "waited ms stamped");
        check(ctx.contains("not observed") && ctx.contains("not that the code is unreachable"),
                "honest note, no root-cause claim");
        check(ctx.contains("\"targetIdentity\":"), "identity rides along");
        String ctx2 = BridgeSnapshot.waitContextJson(st, 2000, 1735689600000L, "com.Foo:54");
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
        check(BridgeSession.setupErrorJson("x", "runtime").contains("\"phase\":\"runtime\""),
                "runtime stage reads runtime");
        check(BridgeSession.setupErrorJson("x", "internal").contains("\"phase\":\"transport\""),
                "unknown stage reads transport");
        check(BridgeSession.setupErrorJson("x", null).contains("\"phase\":\"transport\""),
                "null stage reads transport");

        // -- phase derives from the exception type, never message text or
        // a stage timer: UsageException and ConfigBridgeException read as
        // config; RuntimeBridgeException and unexpected failures read as
        // runtime (truthful internal error, never endpoint-diagnosed);
        // transport losses, disconnects, and target exits stay transport.
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
        check(BridgeSession.phaseOfError(
                new RuntimeBridgeException("track boom")).equals("runtime"),
                "explicit runtime marker reads runtime");
        check(BridgeSession.phaseOfError(new RuntimeException("boom")).equals("runtime"),
                "unexpected reads runtime");
        check(BridgeSnapshot.withCaptureStage(
                "{\"triggerStatus\":\"unknown\"}", "armed-wait", true).contains(
                "\"captureStage\":\"armed-wait\""),
                "capture stage rides additively");
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
        String rej = BridgeSession.setupErrorJson(
                new RuntimeBridgeException("track boom"), "internal: track boom");
        check(rej.contains("\"phase\":\"runtime\"") && rej.contains("internal: track boom"),
                "runtime payload shape, got: " + rej);

        // -- mapSetupFailure: the exact setup-catch equivalent (error.json
        // is written from the mapped value, so the phase derives from its
        // type). Typed failures pass through untouched.
        Exception cfgPass = new ConfigBridgeException("no method x() in Y");
        check(BridgeSession.mapSetupFailure(cfgPass) == cfgPass, "config passes through");
        BridgeException traPass = new BridgeException("attach failed: refused");
        check(BridgeSession.mapSetupFailure(traPass) == traPass, "transport passes through");
        UsageException usePass = new UsageException("bad line");
        check(BridgeSession.mapSetupFailure(usePass) == usePass, "usage passes through");
        // A vanished target (VMDisconnectedException anywhere in the
        // chain, any throwable shape) stays transport — never runtime.
        Throwable discChain = new RuntimeException("wrapper",
                new com.sun.jdi.VMDisconnectedException("gone"));
        Exception mappedDisc = BridgeSession.mapSetupFailure(discChain);
        check(mappedDisc instanceof BridgeException
                && !(mappedDisc instanceof ConfigBridgeException)
                && !(mappedDisc instanceof RuntimeBridgeException),
                "disconnect chain stays transport, got: "
                + mappedDisc.getClass().getSimpleName());
        check(BridgeSession.phaseOfError(mappedDisc).equals("transport"),
                "mapped disconnect reads transport");
        check(mappedDisc.getMessage() != null && mappedDisc.getMessage().contains("wrapper"),
                "mapped disconnect keeps outer short text, got: " + mappedDisc.getMessage());
        Exception mappedBare = BridgeSession.mapSetupFailure(
                new com.sun.jdi.VMDisconnectedException("gone"));
        check(mappedBare instanceof BridgeException
                && !(mappedBare instanceof RuntimeBridgeException)
                && mappedBare.getMessage() != null && mappedBare.getMessage().contains("gone"),
                "bare disconnect keeps its text, got: " + mappedBare.getMessage());
        Error errWrapped = new LinkageError("boom");
        errWrapped.initCause(new com.sun.jdi.VMDisconnectedException("gone"));
        Exception mappedErrDisc = BridgeSession.mapSetupFailure(errWrapped);
        check(mappedErrDisc instanceof BridgeException
                && !(mappedErrDisc instanceof RuntimeBridgeException)
                && BridgeSession.phaseOfError(mappedErrDisc).equals("transport"),
                "disconnect under Error stays transport");
        // Unexpected shapes (unchecked, checked, Error) sanitize to the
        // runtime marker with an internal: payload.
        Exception mappedRt = BridgeSession.mapSetupFailure(new RuntimeException("boom"));
        check(mappedRt instanceof RuntimeBridgeException, "unexpected unchecked reads runtime marker");
        check(BridgeSession.phaseOfError(mappedRt).equals("runtime"),
                "mapped unexpected reads runtime");
        check(mappedRt.getMessage() != null && mappedRt.getMessage().startsWith("internal:"),
                "mapped unexpected sanitized, got: " + mappedRt.getMessage());
        Exception mappedChecked =
                BridgeSession.mapSetupFailure(new java.io.IOException("disk gone"));
        check(mappedChecked instanceof RuntimeBridgeException
                && BridgeSession.phaseOfError(mappedChecked).equals("runtime"),
                "unexpected checked reads runtime");
        Exception mappedErr = BridgeSession.mapSetupFailure(new AssertionError("bad invariant"));
        check(mappedErr instanceof RuntimeBridgeException
                && BridgeSession.phaseOfError(mappedErr).equals("runtime"),
                "unexpected Error reads runtime");

        // -- parkSnapshot degrade: a snapshot throw keeps the park
        // truthful (suspended stays true, bounded location-only snapshot
        // with a warning) instead of crashing the daemon while the VM
        // sits parked. No live VM needed: a null handle throws inside
        // render and exercises the degrade path.
        SessionState pst = state("attach");
        pst.cfg.mode = "attach";
        pst.thread = null;
        pst.location = null;
        String degraded = BridgeSession.parkSnapshot(pst, null);
        check(pst.suspended, "snapshot failure keeps suspended=true");
        check(degraded != null && degraded.contains("\"snapshotWarning\""),
                "degraded snapshot warns, got: " + degraded);
        check(degraded.contains("\"frames\":[]") && degraded.contains("\"location\":"),
                "degraded snapshot bounded with location");

        // -- snapshotVarsTruncated: only the cap sentinel ("+N more")
        // reads truncated — the no-debug-info and render-error sentinels
        // share the "…" name with a different note, so they read false
        // (nothing was capped). Same contract as the Python bridge's
        // frame_locals sentinel.
        check(BridgeSession.snapshotVarsTruncated(
                "{\"locals\":[{\"name\":\"a\"},{\"name\":\"…\",\"note\":\"+9 more\"}]}"),
                "cap sentinel reads truncated");
        check(!BridgeSession.snapshotVarsTruncated(
                "{\"locals\":[{\"name\":\"…\",\"note\":\"no debug info (-g)\"}]}"),
                "no-debug-info sentinel reads full");
        check(!BridgeSession.snapshotVarsTruncated(
                "{\"locals\":[{\"name\":\"…\",\"note\":\"NullPointerException: boom\"}]}"),
                "render-error sentinel reads full");
        check(!BridgeSession.snapshotVarsTruncated("{\"locals\":[{\"name\":\"a\"}]}"),
                "no sentinel reads full");
        check(!BridgeSession.snapshotVarsTruncated(null), "null reads full");
        // The capture response flag derives from the same helper over a
        // full bounded snapshot: capped frame-0 locals flag vars:true.
        String cappedSnap = "{\"frames\":[{\"index\":0,\"locals\":["
                + "{\"name\":\"a\"},{\"name\":\"…\",\"note\":\"+9 more\"}]}]}";
        String cappedFlag = "\"truncated\":{\"frames\":false,\"vars\":"
                + BridgeSession.snapshotVarsTruncated(cappedSnap) + "}";
        check(cappedFlag.contains("\"vars\":true"),
                "capture flag vars:true on cap, got: " + cappedFlag);
        String infoSnap = "{\"frames\":[{\"index\":0,\"locals\":["
                + "{\"name\":\"…\",\"note\":\"no debug info (-g)\"}]}]}";
        check(!BridgeSession.snapshotVarsTruncated(infoSnap),
                "capture flag input reads full on no-debug-info");

        // -- captureExitContextJson: armed-wait/session-gone stages carry
        // waitStartedAt/waitedMs (timeout-context units), the truthful
        // stage, the planted flag, and the honest trigger-unknown note.
        long entryMs = 1735689600000L;
        String exitCtx = BridgeSnapshot.captureExitContextJson(
                st, "armed-wait", true, "com.Foo:54", entryMs);
        check(exitCtx.contains("\"captureStage\":\"armed-wait\""), "exit stage armed-wait");
        check(exitCtx.contains("\"ephemeralPlanted\":true"), "exit planted rides");
        check(exitCtx.contains("\"expectedBreak\":\"com.Foo:54\""), "exit spec rides");
        check(exitCtx.contains("\"waitStartedAt\":1735689600"), "exit wait start stamped");
        check(exitCtx.contains("\"waitedMs\":"), "exit waited ms stamped");
        check(exitCtx.contains("\"triggerStatus\":\"unknown\""), "exit trigger unknown");
        check(exitCtx.contains("not observed"), "exit honest note");
        String goneCtx = BridgeSnapshot.captureExitContextJson(
                st, "session-gone", false, null, entryMs);
        check(goneCtx.contains("\"captureStage\":\"session-gone\"")
                && goneCtx.contains("\"ephemeralPlanted\":false")
                && !goneCtx.contains("expectedBreak"),
                "session-gone shape, got: " + goneCtx);

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

        // -- display-independent change tracking (compareTrack/storeTrack
        // unit parity, no VM): >20 locals complete, removal, >256
        // truncated with intersection-only changes, first-baseline and
        // function-change resets, and a null-thread scan degrading to
        // tracking-error (never a park crash).
        java.util.Map<String, String> base25 = new java.util.LinkedHashMap<>();
        for (int i = 0; i < 25; i++) base25.put("v" + i, String.valueOf(i));
        SessionState tc = state("launch");
        BridgeSnapshot.compareTrack(tc, new java.util.LinkedHashMap<>(base25),
                "com.Foo#run", 25, false, null, true);
        check("[]".equals(tc.lastChanged)
                && !tc.lastChangedComplete
                && tc.lastChangeTracking.contains("\"reason\":\"first-snapshot\""),
                "first baseline empty+unknown, got: " + tc.lastChanged
                + " / " + tc.lastChangeTracking);
        check(tc.lastTrackWarn != null, "first baseline warns");
        java.util.Map<String, String> next = new java.util.LinkedHashMap<>(base25);
        next.put("v24", "changed");
        next.put("fresh", "1");
        next.remove("v0");
        BridgeSnapshot.compareTrack(tc, next, "com.Foo#run", 25, false, null, true);
        check(tc.lastChanged.contains("\"v24\"") && tc.lastChanged.contains("\"fresh\"")
                && tc.lastRemoved.contains("\"v0\"") && tc.lastChangedComplete
                && !tc.lastChangeTracking.contains("reason"),
                ">20 complete with change+removal, got: " + tc.lastChanged
                + " / " + tc.lastRemoved);
        check(tc.lastTrackWarn == null, "complete scan quiet");
        // No-op stop: complete with empty changed (real no-change).
        BridgeSnapshot.compareTrack(tc, new java.util.LinkedHashMap<>(next),
                "com.Foo#run", 25, false, null, true);
        check("[]".equals(tc.lastChanged) && "[]".equals(tc.lastRemoved)
                && tc.lastChangedComplete, "no-op complete empty");
        // Function change resets to unknown.
        BridgeSnapshot.compareTrack(tc, new java.util.LinkedHashMap<>(next),
                "com.Foo#other", 25, false, null, true);
        check("[]".equals(tc.lastChanged) && !tc.lastChangedComplete
                && tc.lastChangeTracking.contains("\"reason\":\"function-changed\""),
                "function change resets, got: " + tc.lastChangeTracking);
        // >256: truncated, intersection-only value changes, removed
        // suppressed, never complete.
        java.util.Map<String, String> trackBig = new java.util.LinkedHashMap<>();
        for (int i = 0; i < 300; i++) trackBig.put("w" + i, "0");
        SessionState tt = state("launch");
        BridgeSnapshot.compareTrack(tt, new java.util.LinkedHashMap<>(trackBig),
                "com.Foo#run", 301, true, "truncated", false);
        check(!tt.lastChangedComplete
                && tt.lastChangeTracking.contains("\"reason\":\"truncated\"")
                && tt.lastChangeTracking.contains("\"total\":301")
                && tt.lastChangeTracking.contains("\"scanned\":256"),
                "truncated baseline unknown, got: " + tt.lastChangeTracking);
        java.util.Map<String, String> big2 = new java.util.LinkedHashMap<>(trackBig);
        big2.put("w1", "1");
        big2.put("added", "x");
        big2.remove("w2");
        BridgeSnapshot.compareTrack(tt, big2, "com.Foo#run", 301, true,
                "truncated", false);
        check(tt.lastChanged.contains("\"w1\"")
                && !tt.lastChanged.contains("\"added\"")
                && "[]".equals(tt.lastRemoved) && !tt.lastChangedComplete,
                "truncated intersection-only, got: " + tt.lastChanged
                + " / " + tt.lastRemoved);
        // Null-thread scan (no frames): tracking-error with unknown
        // total (null, never 0), never throws.
        SessionState te = state("launch");
        te.thread = null;
        try {
            BridgeSnapshot.trackChanges(te);
            check("[]".equals(te.lastChanged) && !te.lastChangedComplete
                    && te.lastChangeTracking.contains("\"reason\":\"tracking-error\"")
                    && te.lastChangeTracking.contains("\"total\":null")
                    && te.lastTrackWarn != null,
                    "null-thread scan degrades, got: " + te.lastChangeTracking);
        } catch (Throwable trackErr) {
            check(false, "trackChanges threw: " + trackErr);
        }
        // Response fragment shape: changed/removed/changedComplete/
        // changeTracking (+warning only when incomplete).
        String frag = BridgeSnapshot.changeFieldsJson(tc);
        check(frag.contains("\"changed\":") && frag.contains("\"removed\":")
                && frag.contains("\"changedComplete\":")
                && frag.contains("\"changeTracking\":"),
                "change fragment shape, got: " + frag);
        // Byte-exact fragment after the function-change reset above
        // (proves the moved renderer emits identical bytes).
        check(("\"changed\":[],\"removed\":[],\"changedComplete\":false,"
                + "\"changeTracking\":{\"complete\":false,\"scanned\":25,"
                + "\"total\":25,\"truncated\":false,"
                + "\"reason\":\"function-changed\"},"
                + "\"trackingWarning\":\"change tracking incomplete "
                + "(function-changed); changed lists only certain value "
                + "changes\"").equals(frag),
                "change fragment bytes, got: " + frag);
        // -- formatTrackingValue: shallow top-level preview, 200-cap, no
        // field walk / element fetch / invocation (proxies explode on any
        // such call, proving the contract without a VM).
        check("null".equals(BridgeSnapshot.formatTrackingValue(null)),
                "track preview null");
        java.util.Map<String, Object> primAns = new java.util.HashMap<>();
        primAns.put("toString", "42");
        check("42".equals(BridgeSnapshot.formatTrackingValue(
                (com.sun.jdi.Value) jdiProxy(com.sun.jdi.PrimitiveValue.class, primAns))),
                "track preview primitive");
        StringBuilder longStr = new StringBuilder();
        for (int i = 0; i < 300; i++) longStr.append('x');
        java.util.Map<String, Object> strAns = new java.util.HashMap<>();
        strAns.put("value", longStr.toString());
        String cappedPrev = BridgeSnapshot.formatTrackingValue(
                (com.sun.jdi.Value) jdiProxy(com.sun.jdi.StringReference.class, strAns));
        check(cappedPrev.startsWith("\"") && cappedPrev.contains("more chars")
                && cappedPrev.length() < 302, "track preview long string capped");
        java.util.Map<String, Object> arrTypeAns = new java.util.HashMap<>();
        arrTypeAns.put("name", "int[]");
        Object arrTypeProxy = jdiProxy(com.sun.jdi.ArrayType.class, arrTypeAns);
        java.util.Map<String, Object> arrAns = new java.util.HashMap<>();
        arrAns.put("type", arrTypeProxy);
        arrAns.put("length", 5);
        arrAns.put("getValues", new AssertionError("element fetch"));
        check("int[][5]".equals(BridgeSnapshot.formatTrackingValue(
                (com.sun.jdi.Value) jdiProxy(com.sun.jdi.ArrayReference.class, arrAns))),
                "track preview array shallow, no fetch");
        java.util.Map<String, Object> refTypeAns = new java.util.HashMap<>();
        refTypeAns.put("name", "com.Foo");
        Object refTypeProxy = jdiProxy(com.sun.jdi.ReferenceType.class, refTypeAns);
        java.util.Map<String, Object> objAns = new java.util.HashMap<>();
        objAns.put("referenceType", refTypeProxy);
        objAns.put("uniqueID", 123L);
        objAns.put("getValue", new AssertionError("field walk"));
        objAns.put("invokeMethod", new AssertionError("invoke"));
        check("com.Foo@123".equals(BridgeSnapshot.formatTrackingValue(
                (com.sun.jdi.Value) jdiProxy(com.sun.jdi.ObjectReference.class, objAns))),
                "track preview object shallow, no walk");
        // -- frameIdentity: type+method+signature (overloads differ), nulls
        // degrade to "?", never null.
        check("com.Foo#run()V".equals(
                BridgeEval.frameIdentity("com.Foo", "run", "()V")),
                "identity with signature");
        check("com.Foo#run".equals(
                BridgeEval.frameIdentity("com.Foo", "run", null)),
                "identity without signature");
        check(!BridgeEval.frameIdentity("com.Foo", "run", "()V").equals(
                BridgeEval.frameIdentity("com.Foo", "run", "(I)V")),
                "identity distinguishes overloads");
        check("?#?".equals(BridgeEval.frameIdentity(null, null, null)),
                "identity nulls degrade");
        // -- degradeTrack: unknown total (null), exactly one class-only
        // stderr warning, and the warning matches the response fragment.
        SessionState dg = state("launch");
        dg.thread = null;
        java.io.PrintStream keepErr = System.err;
        java.io.ByteArrayOutputStream errBuf = new java.io.ByteArrayOutputStream();
        System.setErr(new java.io.PrintStream(errBuf));
        try {
            BridgeSnapshot.degradeTrack(dg, "IllegalStateException", null);
        } finally {
            System.setErr(keepErr);
        }
        String errText = errBuf.toString(java.nio.charset.StandardCharsets.UTF_8);
        int warnLines = 0;
        for (String ln : errText.split("\n")) {
            if (ln.contains("warn: change tracking degraded")) warnLines++;
        }
        check(warnLines == 1, "degrade warns once, got: " + errText.trim());
        check(dg.lastChangeTracking.contains("\"total\":null")
                && dg.lastChangeTracking.contains("\"scanned\":0"),
                "degrade total null, got: " + dg.lastChangeTracking);
        check(dg.lastTrackWarn != null
                && dg.lastTrackWarn.contains("IllegalStateException")
                && BridgeSnapshot.changeFieldsJson(dg).contains(
                        JdiBridge.quote(dg.lastTrackWarn)),
                "degrade warn matches response");
        // -- M5.2 lock contract: the moved stateful helpers (compareTrack/
        // changeFieldsJson) execute under the caller-held sessionLock,
        // exactly like the awaitStopInner Phase-B and dispatchInner
        // production sections. Two threads hammer one shared state with
        // every call wrapped in synchronized (sessionLock); holdsLock
        // asserts the test honors the contract it proves, and the final
        // fragment is well-formed from exactly one writer (never torn).
        final SessionState lk = state("launch");
        final java.util.Map<String, String> snapA = new java.util.LinkedHashMap<>();
        snapA.put("k", "1");
        final java.util.Map<String, String> snapB = new java.util.LinkedHashMap<>();
        snapB.put("k", "2");
        final java.util.concurrent.atomic.AtomicReference<Throwable> lockErr =
                new java.util.concurrent.atomic.AtomicReference<>();
        Runnable hammer = () -> {
            try {
                for (int i = 0; i < 50; i++) {
                    java.util.Map<String, String> cur =
                            (i % 2 == 0) ? snapA : snapB;
                    synchronized (lk.sessionLock) {
                        if (!Thread.holdsLock(lk.sessionLock)) {
                            throw new AssertionError("lock not held");
                        }
                        BridgeSnapshot.compareTrack(lk,
                                new java.util.LinkedHashMap<>(cur),
                                "com.Foo#run", 1, false, null, true);
                        String f = BridgeSnapshot.changeFieldsJson(lk);
                        if (!f.contains("\"changeTracking\":{")) {
                            throw new AssertionError("torn fragment: " + f);
                        }
                    }
                }
            } catch (Throwable th) {
                lockErr.compareAndSet(null, th);
            }
        };
        Thread t1 = new Thread(hammer);
        Thread t2 = new Thread(hammer);
        t1.start();
        t2.start();
        try {
            t1.join(30000);
            t2.join(30000);
        } catch (InterruptedException ie) {
            Thread.currentThread().interrupt();
            check(false, "lock hammer interrupted");
        }
        check(lockErr.get() == null,
                "lock hammer clean: " + lockErr.get());
        check(!t1.isAlive() && !t2.isAlive(), "lock hammer joined");
        final String[] finalFrag = new String[1];
        synchronized (lk.sessionLock) {
            finalFrag[0] = BridgeSnapshot.changeFieldsJson(lk);
        }
        check(finalFrag[0].contains("\"changeTracking\":{\"complete\":true,")
                && (finalFrag[0].contains("\"k\"") || finalFrag[0].contains("\"changed\":[]")),
                "lock hammer final fragment coherent, got: " + finalFrag[0]);

        if (failures > 0) {
            System.out.println("M7JavaCheck: " + failures + " FAILURES");
            System.exit(1);
        }
        System.out.println("M7JavaCheck: all green");
    }
}
