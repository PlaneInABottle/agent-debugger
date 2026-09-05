package com.acme.billing;

import java.util.List;

/** Maven-layout target: multi-package, nested calls, like a real service. */
public class BillingApp {
    public static void main(String[] args) {
        InvoiceService service = new InvoiceService(new TaxPolicy(0.20));
        Invoice invoice = service.buildInvoice("ACME-1", List.of(
                new Line("setup", 2, 499.0),
                new Line("seats", 12, 29.0)));
        System.out.println("GRAND=" + invoice.grandTotal);
    }
}
