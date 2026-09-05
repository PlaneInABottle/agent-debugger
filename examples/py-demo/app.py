"""Billing demo mirroring the Java example (for adapter parity tests)."""


class Order:
    def __init__(self, oid, name, price, qty):
        self.id = oid
        self.name = name
        self.price = price
        self.qty = qty


def load_orders():
    return [
        Order(101, "Klavye", 1500.0, 1),
        Order(102, "Mouse", 800.0, 2),
        Order(103, "Monitor", 9500.0, 1),
    ]


def checkout(orders, coupon):
    subtotal = 0.0
    for order in orders:
        subtotal += order.price * order.qty
    discount = subtotal * 0.10 if coupon == "WELCOME10" else 0.0
    total = subtotal - discount
    return total  # <-- breakpoint here


def main():
    orders = load_orders()
    total = checkout(orders, "WELCOME10")
    print(f"TOTAL={total}")


if __name__ == "__main__":
    main()
