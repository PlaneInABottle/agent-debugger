package com.big;

import java.util.concurrent.CompletableFuture;

/** CompletableFuture pipeline; breakpoint inside an async stage. */
public class Async {
    public static void main(String[] args) throws Exception {
        int result = CompletableFuture
                .supplyAsync(() -> fetch("orders"))
                .thenApply(Async::price)
                .thenApply(total -> total * 2)
                .get();
        System.out.println("RESULT=" + result);
    }

    static String fetch(String what) {
        try { Thread.sleep(200); } catch (InterruptedException ignored) {}
        return what + "-42";
    }

    static int price(String s) {
        int total = s.length() * 100; // <-- breakpoint here
        return total;
    }
}
