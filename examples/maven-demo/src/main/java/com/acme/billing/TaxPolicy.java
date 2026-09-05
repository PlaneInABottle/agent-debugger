package com.acme.billing;

public class TaxPolicy {
    private final double rate;

    public TaxPolicy(double rate) {
        this.rate = rate;
    }

    public double apply(double net) {
        return net * rate;
    }
}
