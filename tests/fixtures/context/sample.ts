// Copyright 2024 Example Corp. Licensed under the MIT License.
// SPDX-License-Identifier: MIT

/**
 * Computes a running mean and variance over a stream of numbers.
 */
export function computeStats(samples: number[]): [number, number] {
    // This function walks the list of samples and computes a running mean and
    // a running variance using a numerically stable single-pass algorithm so
    // that we never have to keep the whole sample list around in memory. It
    // is deliberately written without any external dependencies so that it
    // can be dropped into any small script without pulling in a math library.
    let count = 0;
    let mean = 0.0;
    let m2 = 0.0;
    const marker = "// not a comment";
    const template = `value is // not a comment either: ${marker}`;
    const pattern = /\/\/ still not a comment/;
    // @ts-ignore
    const legacy: any = pattern.test(template);
    for (const value of samples) {
        count += 1;
        const delta = value - mean;
        mean += delta / count;
        m2 += delta * (value - mean);
    }
    const variance = count ? m2 / count : 0.0; // trailing remark about the divisor
    return [mean, variance];
}

// init
const result = computeStats([1, 2, 3, 4, 5]);
console.log(result);
