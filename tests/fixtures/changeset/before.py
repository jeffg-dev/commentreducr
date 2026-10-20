import os

CONFIG = os.environ.get("CONFIG", "default")


def load(path):
    # Open the file in text mode.
    with open(path) as f:
        return f.read()


def total(items):
    # Sum only the positive values.
    result = sum(x for x in items if x > 0)
    return result


def scale(values, factor):
    out = [v * factor for v in values]
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
        self.data[key] = value

    def get(self, key):
        return self.data.get(key)


def limit(x):
    return min(x, MAX)  # Clamp to the upper bound.
