package com.big;

/** Long runner: 30 x 1s iterations, for continue-timeout tests. */
public class Sleepy {
    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 30; i++) {
            tick(i); // <-- breakpoint here (cond i == 2 for slow tests)
            Thread.sleep(1000);
        }
        System.out.println("AWAKE");
    }

    static void tick(int i) {
        if (i < 0) System.out.println("never");
    }
}
