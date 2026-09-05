package com.big;

/** NullPointerException at a precise throw site (for exc: breakpoints). */
public class Npe {
    public static void main(String[] args) {
        new Npe().run();
    }

    void run() {
        Holder holder = new Holder();
        holder.name = null;
        int len = holder.name.length(); // NPE here
        System.out.println(len);
    }

    static class Holder {
        String name;
    }
}
