package com.big;

/** Tricky values: unicode, quotes, newlines (JSON robustness). */
public class Uni {
    public static void main(String[] args) {
        String s = "héllo \"wörld\"\nline2\ttab😀"; // <-- breakpoint here
        String empty = "";
        System.out.println(s.length() + "," + empty.length());
    }
}
