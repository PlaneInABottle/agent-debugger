/** Strict input parity for the Java bridge (plain asserts, nonzero exit).
 *  1. Bad --port reads as UsageException (exit 2 upstream), never a raw
 *     NumberFormatException -> internal.
 *  2. Truncated/malformed \\u escapes read as BridgeException("bad request
 *     json"), never StringIndexOutOfBounds/NumberFormatException.
 *  3. Owner claim is verified right after the write (Node/Browser/Python
 *     parity): matching nonce reads as owner, tampered does not.
 *  4. Removing a line break prunes its hit counter (bounded hitCounts);
 *     the removed[] echo still carries the pre-removal hits.
 *
 *  Compile: javac -cp <bridge classes> -d <out> tests/StrictJavaCheck.java
 *  Run:     java -cp <bridge classes>:<out> StrictJavaCheck
 */
public class StrictJavaCheck {
    static int failures = 0;

    static void check(boolean cond, String name) {
        if (cond) {
            System.out.println("ok: " + name);
        } else {
            System.out.println("FAIL: " + name);
            failures++;
        }
    }

    @SuppressWarnings("unchecked")
    static <T> T fake(Class<T> iface, java.util.Map<String, Object> answers) {
        java.lang.reflect.InvocationHandler h = (proxy, m, args) -> {
            if (m.getName().equals("toString")) return "fake";
            if (m.getName().equals("hashCode")) return 0;
            if (m.getName().equals("equals")) return proxy == args[0];
            return answers.get(m.getName());
        };
        return (T) java.lang.reflect.Proxy.newProxyInstance(
                StrictJavaCheck.class.getClassLoader(),
                new Class<?>[]{iface}, h);
    }

    public static void main(String[] argv) throws Exception {
        // 1. --port validation.
        try {
            BridgeCli.parsePort("notaport");
            check(false, "bad port throws UsageException");
        } catch (UsageException e) {
            check(e.getMessage().contains("--port needs a number"), "bad port is Usage");
        }
        try {
            BridgeCli.parsePort("12x");
            check(false, "trailing-junk port throws");
        } catch (UsageException e) {
            check(true, "trailing-junk port is Usage");
        }
        check(BridgeCli.parsePort("5005") == 5005, "numeric port parses");

        // 2. \\u escape strictness.
        int[] end = new int[1];
        String[] bad = new String[]{"\"ab\\u12\"", "\"ab\\uZZZZ\"", "\"ab\\u12GZ\""};
        for (String c : bad) {
            try {
                BridgeProto.parseJsonString(c, 0, end);
                check(false, "malformed escape rejected: " + c);
            } catch (BridgeException e) {
                check("bad request json".equals(e.getMessage()), "malformed escape is BridgeException: " + c);
            } catch (Throwable t) {
                check(false, "wrong exception for " + c + ": " + t.getClass().getSimpleName());
            }
        }
        check("ab\u1234".equals(BridgeProto.parseJsonString("\"ab\\u1234\"", 0, end)), "valid \\u parses");

        // 3. Owner claim verification.
        java.nio.file.Path tmp = java.nio.file.Files.createTempDirectory("strict-owner-");
        SessionState st = new SessionState();
        st.cfg = new Config();
        st.dir = tmp;
        st.ownerNonce = "strict-nonce";
        java.nio.file.Files.write(tmp.resolve("owner.json"),
                "{\"pid\":1,\"nonce\":\"strict-nonce\"}".getBytes("UTF-8"));
        check(BridgeSession.amOwner(st), "matching owner claim reads as owner");
        java.nio.file.Files.write(tmp.resolve("owner.json"),
                "{\"pid\":1,\"nonce\":\"someone-else\"}".getBytes("UTF-8"));
        check(!BridgeSession.amOwner(st), "foreign owner claim reads as non-owner");

        // 4. hitCounts prune on remove (echo keeps pre-removal hits).
        SessionState st2 = new SessionState();
        st2.cfg = new Config();
        st2.dir = tmp;
        st2.cfg.breakpoints.put("Foo", new java.util.ArrayList<>(java.util.List.of(10)));
        st2.cfg.breakRaws.put("Foo:10|", "Foo:10");
        st2.hitCounts.put("break|Foo|10", 3);
        java.util.Map<String, Object> erm = new java.util.LinkedHashMap<>();
        erm.put("breakpointRequests", new java.util.ArrayList<>());
        java.util.Map<String, Object> vm = new java.util.LinkedHashMap<>();
        vm.put("eventRequestManager", fake(com.sun.jdi.request.EventRequestManager.class, erm));
        vm.put("classesByName", new java.util.ArrayList<>());
        st2.vm = fake(com.sun.jdi.VirtualMachine.class, vm);
        BridgeSession.AddedLine p = new BridgeSession.AddedLine();
        p.raw = "Foo:10";
        p.cls = "Foo";
        p.line = 10;
        p.cond = null;
        String resp = BridgeSession.dropBreakKeys(st2,
                new java.util.ArrayList<>(java.util.List.of(p)),
                new java.util.ArrayList<>());
        check(resp.contains("\"hits\":3"), "removed echo keeps pre-removal hits: " + resp);
        check(!st2.hitCounts.containsKey("break|Foo|10"), "hit counter pruned on remove");

        if (failures > 0) {
            System.out.println("FAILURES: " + failures);
            System.exit(1);
        }
        System.out.println("ok: StrictJavaCheck all green");
    }
}
