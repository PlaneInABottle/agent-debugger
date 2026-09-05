"""Three threads racing a shared counter (mirrors Racy.java)."""
import threading
import time

shared = 0


def run(name):
    global shared
    for _round in range(3):
        seen = shared  # <-- breakpoint here
        shared = seen + 1
        time.sleep(0.01)


def main():
    threads = [threading.Thread(target=run, args=(f"racer-{i}",), name=f"racer-{i}")
               for i in range(3)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    print(f"SHARED={shared}")


if __name__ == "__main__":
    main()
