"""Big state for token measurement (mirrors BigApp.java)."""
import threading
import time


class Fat:
    def __init__(self):
        for i in range(30):
            setattr(self, f"f{i}", float(i))
        self.f0 = 7.5
        self.label = "fat"


def level1(items, bean, payload):
    return level2(items, bean, payload, 1)


def level2(items, bean, payload, d):
    return level3(items, bean, payload, d + 1)


def level3(items, bean, payload, depth):
    total = sum(it["price"] for it in items[:5])
    note = f"run-{depth}"
    return inspect(items, bean, payload, total, note, depth)  # <-- break here


def inspect(items, bean, payload, total, note, depth):
    print(f"TOTAL={total + bean.f0 + len(payload) + depth + len(note)}")


def worker():
    time.sleep(60)


def main():
    for i in range(25):
        t = threading.Thread(target=worker, name=f"worker-{i}", daemon=True)
        t.start()
    items = [{"id": i, "name": f"item-{i}", "price": i * 1.5} for i in range(1000)]
    bean = Fat()
    payload = "x" * 5000
    level1(items, bean, payload)


if __name__ == "__main__":
    main()
