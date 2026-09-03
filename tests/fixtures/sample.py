# Copyright 2024 Example Corp. Licensed under the MIT License.
# SPDX-License-Identifier: MIT

"""Module docstring: a small stats helper, not a comment at all."""

import math


def compute_stats(samples):
    # This function walks the list of samples and computes a running mean and
    # a running variance using a numerically stable single-pass algorithm so
    # that we never have to keep the whole sample list around in memory. It
    # is deliberately written without any external dependencies so that it
    # can be dropped into any small script without pulling in numpy.
    count = 0
    mean = 0.0
    m2 = 0.0
    marker = "# not a comment"  # noqa: E501
    for value in samples:
        count += 1
        delta = value - mean
        mean += delta / count
        m2 += delta * (value - mean)
    variance = m2 / count if count else 0.0
    return mean, variance  # trailing remark about the return shape


# init
result = compute_stats([1, 2, 3, 4, 5])
print(result, math.sqrt(result[1]))
