package com.acme.billing;

import java.util.List;

public class Invoice {
    public final String customer;
    public final List<Line> lines;
    public final double net;
    public final double tax;
    public final double grandTotal;

    public Invoice(String customer, List<Line> lines, double net, double tax, double grandTotal) {
        this.customer = customer;
        this.lines = lines;
        this.net = net;
        this.tax = tax;
        this.grandTotal = grandTotal;
    }
}
