package com.big;

/** Caught NPE must NOT stop; uncaught NPE must stop (exc: filter check). */
public class CatchMe {
    public static void main(String[] args) {
        try {
            String s = null;
            s.length(); // caught: no stop expected here
        } catch (NullPointerException e) {
            System.out.println("caught one");
        }
        String t = null;
        System.out.println(t.length()); // uncaught: stop expected here
    }
}
