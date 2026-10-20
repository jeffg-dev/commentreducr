"""Module docstring for the scopes fixture."""

import threading


class Cache:
    """A tiny cache guarded by a lock."""

    def __init__(self):
        self._lock = threading.Lock()
        self._data = {}

    def set(self, key, value):
        # take the lock before touching the dict
        with self._lock:
            self._data[key] = value  # last write wins
            self._data[key + "!"] = value
            # nothing else to do here


def _helper(x):
    """Double the input."""
    return x * 2


def test_helper():
    # the helper doubles
    assert _helper(2) == 4
