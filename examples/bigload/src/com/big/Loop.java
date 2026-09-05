package com.big;

/** 1000-iteration loop for conditional-breakpoint stress (match at the end). */
public class Loop {
    public static void main(String[] args) {
        long sum = 0;
        for (int i = 0; i < 1000; i++) {
            sum += work(i); // <-- conditional breakpoint here (i == 999)
        }
        System.out.println("SUM=" + sum);
    }

    static long work(int i) {
        return i * 2L;
    }
}
