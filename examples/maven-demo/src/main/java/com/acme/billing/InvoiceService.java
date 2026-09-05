package com.acme.billing;

import java.util.ArrayList;
import java.util.List;

public class InvoiceService {
    private final TaxPolicy taxPolicy;

    public InvoiceService(TaxPolicy taxPolicy) {
        this.taxPolicy = taxPolicy;
    }

    public Invoice buildInvoice(String customer, List<Line> lines) {
        List<Line> priced = new ArrayList<>();
        double net = 0.0;
        for (Line line : lines) {
            double amount = line.qty * line.unit;
            priced.add(new Line(line.name, line.qty, line.unit, amount));
            net += amount;
        }
        double tax = taxPolicy.apply(net);
        double grand = net + tax;
        return new Invoice(customer, priced, net, tax, grand); // <-- breakpoint here
    }
}
