package com.big;

/** Three threads racing through the same method; who stops at the breakpoint? */
public class Racy {
    static int shared = 0;

    public static void main(String[] args) throws Exception {
        Thread[] ts = new Thread[3];
        for (int i = 0; i < 3; i++) {
            final String name = "racer-" + i;
            ts[i] = new Thread(() -> run(name), name);
            ts[i].start();
        }
        for (Thread t : ts) t.join();
        System.out.println("SHARED=" + shared);
    }

    static void run(String name) {
        for (int round = 0; round < 3; round++) {
            int seen = shared; // <-- breakpoint here
            shared = seen + 1;
            try { Thread.sleep(10); } catch (InterruptedException ignored) {}
        }
    }
}
