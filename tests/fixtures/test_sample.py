"""This test-support module gathers small pytest helpers shared across the
test files in this package, added during a Q3 test-infra cleanup effort to
consolidate a handful of near-duplicate helper functions that had drifted
apart across individual test modules over time as different authors made
slightly different tweaks. It also documents, for anyone new to this
package, the general naming and layout conventions the test suite here
follows, including where shared fixtures should live. Please keep this
docstring current as the module grows and consult the wiki for more.
"""

import math


def add(a, b):
    """Add two numbers together and return the result.

    >>> add(1, 2)
    3
    """
    return a + b


def helper_thing(items):
    """Normalizes a list of items before they are compared in the
    tests below: each string is stripped of surrounding whitespace,
    empty results are dropped entirely, and everything remaining is
    lowercased so comparisons do not depend on case, matching what
    the fixtures assume about their inputs.
    """
    out = []
    for item in items:
        cleaned = item.strip().lower()
        if cleaned:
            out.append(cleaned)
    return out


def test_short():
    """Guards the empty-list case."""
    assert helper_thing([]) == []


def test_dedup_and_lowercase():
    """Ensures mixed-case, whitespace-padded items are normalized and
    that an item which becomes empty after stripping the whitespace
    is dropped entirely, rather than kept as a blank string in the
    returned list that downstream assertions do not expect.
    """
    assert helper_thing([" Foo ", "BAR", "   "]) == ["foo", "bar"]


class TestThing:
    """Placeholder test class reserved for future thing-related tests."""


class Widget:
    def probe(self): """inline"""


MESSAGE = """not a docstring"""

# comment
value = 1
"""also not a docstring, the bare string after an assignment"""


def make_greeting(name):
    f"""this looks like a docstring but is an f-string, not one"""
    return f"hello {name}, sqrt(2) is about {math.sqrt(2):.3f}"
