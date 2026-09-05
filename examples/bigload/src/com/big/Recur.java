package com.big;

/** Deep recursion (50 frames) for stack-cap and --frame tests. */
public class Recur {
    public static void main(String[] args) {
        System.out.println("F=" + fact(50));
    }

    static long fact(int n) {
        if (n <= 1) return 1; // <-- breakpoint here
        return n * fact(n - 1);
    }
}
