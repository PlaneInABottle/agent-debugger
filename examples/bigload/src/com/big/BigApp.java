package com.big;

import java.util.ArrayList;
import java.util.List;

/** Realistic load: fat beans, big list, long string, deep stack, many threads. */
public class BigApp {
    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 25; i++) {
            Thread t = new Thread(() -> {
                try { Thread.sleep(60_000); } catch (InterruptedException ignored) {}
            }, "worker-" + i);
            t.setDaemon(true);
            t.start();
        }
        List<Item> items = new ArrayList<>();
        for (int i = 0; i < 1000; i++) items.add(new Item(i, "item-" + i, i * 1.5));
        FatBean bean = FatBean.sample();
        String payload = "x".repeat(5000);
        new BigApp().level1(items, bean, payload);
    }

    void level1(List<Item> items, FatBean bean, String payload) { level2(items, bean, payload, 1); }
    void level2(List<Item> items, FatBean bean, String payload, int d) { level3(items, bean, payload, d + 1); }

    void level3(List<Item> items, FatBean bean, String payload, int depth) {
        double sum = 0;
        int count = 0;
        String tag = "run";
        for (Item it : items) {
            sum += it.price;
            count++;
            if (count >= 5) break;
        }
        double avg = count == 0 ? 0 : sum / count;
        String note = tag + "-" + depth;
        inspect(items, bean, payload, sum, count, avg, note, depth); // <-- breakpoint here
    }

    void inspect(List<Item> items, FatBean bean, String payload, double sum,
            int count, double avg, String note, int depth) {
        double total = sum + bean.f0 + payload.length() + depth + avg + note.length();
        System.out.println("TOTAL=" + total);
    }

    public static class Item {
        public final int id;
        public final String name;
        public final double price;

        public Item(int id, String name, double price) {
            this.id = id;
            this.name = name;
            this.price = price;
        }
    }

    public static class FatBean {
        public double f0, f1, f2, f3, f4, f5, f6, f7, f8, f9;
        public double f10, f11, f12, f13, f14, f15, f16, f17, f18, f19;
        public double f20, f21, f22, f23, f24, f25, f26, f27, f28, f29;
        public String label;

        static FatBean sample() {
            FatBean b = new FatBean();
            b.f0 = 7.5;
            b.label = "fat";
            return b;
        }
    }
}
