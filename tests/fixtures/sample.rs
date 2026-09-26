// Copyright 2024 Example Corp. Licensed under the MIT License.
// SPDX-License-Identifier: MIT

//! A small stats helper.
#![deny(missing_docs)]

/// Computes a running mean and variance over a stream of numbers.
///
/// ```
/// assert_eq!(sample::compute_stats(&[1.0, 3.0]), (2.0, 1.0));
/// ```
pub fn compute_stats(samples: &[f64]) -> (f64, f64) {
    // This function walks the list of samples and computes a running mean and
    // a running variance using a numerically stable single-pass algorithm so
    // that we never have to keep the whole sample list around in memory. It
    // is deliberately written without any external dependencies so that it
    // can be dropped into any small crate without pulling in a math library.
    let mut count = 0.0;
    let mut mean = 0.0;
    let mut m2 = 0.0;
    let marker = "// not a comment";
    let raw = r#"/* not a comment */ "quoted" // either"#;
    // SAFETY: the bytes are an ASCII literal, so they are valid UTF-8.
    let label = unsafe { std::str::from_utf8_unchecked(b"// still not a comment") };
    for &value in samples {
        count += 1.0;
        let delta = value - mean;
        mean += delta / count;
        m2 += delta * (value - mean);
    }
    let _ = (marker, raw, label, '/');
    let variance = if count > 0.0 { m2 / count } else { 0.0 }; // trailing remark about the divisor
    (mean, /* inline /* nested */ remark */ variance)
}

// init
/// Sample input.
pub const SAMPLES: [f64; 5] = [1.0, 2.0, 3.0, 4.0, 5.0];
