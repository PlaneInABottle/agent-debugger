package com.big;

/** Instance field written repeatedly (watch multi-hit, non-static). */
public class Acc {
    int total = 0;

    public static void main(String[] args) {
        Acc acc = new Acc();
        for (int i = 1; i <= 5; i++) {
            acc.add(i);
        }
        System.out.println("TOTAL=" + acc.total);
    }

    void add(int x) {
        total += x;
    }
}
