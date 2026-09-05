"""asyncio tasks + ThreadPoolExecutor (async + pool scenarios)."""
import asyncio
import concurrent.futures
import time


async def fetch(item):
    await asyncio.sleep(0.05)
    return f"{item}-42"


async def price(tag):
    total = len(tag) * 100  # <-- breakpoint here (async)
    return total


async def amain():
    tags = await asyncio.gather(*(fetch(f"o{i}") for i in range(3)))
    totals = await asyncio.gather(*(price(t) for t in tags))
    return sum(totals)


def blocking(n):
    time.sleep(0.05)
    doubled = n * 2  # <-- breakpoint here (pool thread)
    return doubled


def main():
    print(f"ASYNC={asyncio.run(amain())}")
    with concurrent.futures.ThreadPoolExecutor(max_workers=2,
                                               thread_name_prefix="pool") as pool:
        print(f"POOL={sum(pool.map(blocking, [1, 2, 3]))}")


if __name__ == "__main__":
    main()
