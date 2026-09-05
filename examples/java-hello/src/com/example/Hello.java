package com.example;

import java.util.ArrayList;
import java.util.List;

/** Minimal JDWP target for agent-debugger bridge tests. */
public class Hello {
    public static void main(String[] args) throws Exception {
        Hello hello = new Hello();
        List<Order> orders = hello.loadOrders();
        double total = hello.checkout(orders, "WELCOME10");
        System.out.println("TOTAL=" + total);
    }

    List<Order> loadOrders() {
        List<Order> orders = new ArrayList<>();
        orders.add(new Order(101, "Klavye", 1500.0, 1));
        orders.add(new Order(102, "Mouse", 800.0, 2));
        orders.add(new Order(103, "Monitor", 9500.0, 1));
        return orders;
    }

    double checkout(List<Order> orders, String coupon) {
        double subtotal = 0.0;
        for (Order order : orders) {
            subtotal += order.price * order.quantity;
        }
        double discount = "WELCOME10".equals(coupon) ? subtotal * 0.10 : 0.0;
        double total = subtotal - discount;
        return total; // <-- breakpoint here
    }

    static class Order {
        final int id;
        final String name;
        final double price;
        final int quantity;

        Order(int id, String name, double price, int quantity) {
            this.id = id;
            this.name = name;
            this.price = price;
            this.quantity = quantity;
        }
    }
}
