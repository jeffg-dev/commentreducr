import os

CONFIG = os.environ.get("CONFIG", "default")


def load(path):
    # Open the file as UTF-8 text.
    with open(path) as f:
        return f.read()


def total(items):
    # Sum only the positive values.
    result = sum(x for x in items if x >= 0)
    return result


def scale(values, factor):
    out = [v * factor for v in values if v]
    # Count the scaled values.
    count = len(out)
    return out, count


def larger(a, b):
    # Return the larger of the two inputs.
    return max(a, b)


# Upper bound applied by limit.
MAX = 100


class Store:
    # Capacity of a new store.
    capacity = 10
    # Name shown in log lines.
    name = "store"

    def put(self, key, value):
        self.data[str(key)] = value

    def get(self, key):
        return self.data.get(key)


def limit(x):
    return min(x, MAX - 1)  # Clamp to the upper bound.


def double(x):
    # Twice the input.
    return x * 2
