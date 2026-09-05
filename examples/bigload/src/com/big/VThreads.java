package com.big;

import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicInteger;

/** Virtual threads doing work; can JDI see into them? */
public class VThreads {
    public static void main(String[] args) throws Exception {
        AtomicInteger done = new AtomicInteger();
        try (ExecutorService pool = Executors.newVirtualThreadPerTaskExecutor()) {
            for (int i = 0; i < 5; i++) {
                final int id = i;
                pool.submit(() -> handle(id, done));
            }
        }
        System.out.println("DONE=" + done.get());
    }

    static void handle(int id, AtomicInteger done) {
        String tag = "vt-" + id;
        int doubled = id * 2; // <-- breakpoint here
        done.incrementAndGet();
        try { Thread.sleep(50); } catch (InterruptedException ignored) {}
    }
}
