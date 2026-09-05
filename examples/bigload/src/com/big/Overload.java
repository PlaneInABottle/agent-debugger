package com.big;

/** Overloaded methods + inner class + lambda + static init + constructor. */
public class Overload {
    static String boot = init();

    static String init() {
        return "up"; // <-- static-init line
    }

    final String tag;

    Overload(String tag) {
        this.tag = tag; // <-- constructor line
    }

    int add(int a, int b) {
        return a + b;
    }

    double add(double a, double b) {
        return a + b; // <-- overload line
    }

    public static void main(String[] args) {
        Overload o = new Overload("o1");
        int i = o.add(1, 2);
        double d = o.add(1.5, 2.5);
        Inner inner = o.new Inner();
        int v = inner.nine();
        java.util.List<Integer> xs = java.util.List.of(1, 2, 3);
        int sum = xs.stream().mapToInt(x -> {
            int y = x * 10 + v; // <-- lambda line
            return y;
        }).sum();
        System.out.println("R=" + (i + d + sum));
    }

    class Inner {
        int nine() {
            return 9; // <-- inner-class line
        }
    }
}
