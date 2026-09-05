"""Uncaught exception (for exc: breakpoints)."""


def parse(s):
    return int(s)  # ValueError here on bad input


def main():
    try:
        print(parse("oops"))
    except ValueError:
        print("caught one")
    print(parse("nope"))


if __name__ == "__main__":
    main()
