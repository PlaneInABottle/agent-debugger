package com.acme.billing;

public class Line {
    public final String name;
    public final int qty;
    public final double unit;
    public final double amount;

    public Line(String name, int qty, double unit) {
        this(name, qty, unit, 0.0);
    }

    public Line(String name, int qty, double unit, double amount) {
        this.name = name;
        this.qty = qty;
        this.unit = unit;
        this.amount = amount;
    }
}
