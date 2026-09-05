package com.big;

/** Classic two-lock deadlock for observability boundary test. */
public class Deadlock {
    static final Object A = new Object();
    static final Object B = new Object();

    public static void main(String[] args) throws Exception {
        Thread t1 = new Thread(() -> grab(A, B), "locker-1");
        Thread t2 = new Thread(() -> grab(B, A), "locker-2");
        t1.setDaemon(true);
        t2.setDaemon(true);
        t1.start();
        t2.start();
        Thread.sleep(2000);
        System.out.println("STUCK? t1=" + t1.getState() + " t2=" + t2.getState());
        Thread.sleep(30000); // keep VM alive for attach
    }

    static void grab(Object first, Object second) {
        synchronized (first) {
            try { Thread.sleep(500); } catch (InterruptedException ignored) {}
            synchronized (second) {
                System.out.println("never");
            }
        }
    }
}
