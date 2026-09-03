//! Progress line on stderr for long reduce runs. On a terminal the line is redrawn in place;
//! otherwise a line is printed at every 10% milestone. All stderr/stdout output during the run
//! goes through `warn` / `print` so nothing lands in the middle of the progress line.
//! `Progress::silent()` (total 0) prints no progress and just forwards messages.
use crate::llm::TokenTotals;
use std::fmt::Display;
use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

pub struct Progress<'a> {
    total: usize,
    files: usize,
    tokens: Option<&'a TokenTotals>,
    done: AtomicUsize,
    files_done: AtomicUsize,
    tty: bool,
    started: Instant,
    /// Serializes output; holds the last 10% milestone printed (non-tty).
    last_milestone: Mutex<usize>,
}

impl<'a> Progress<'a> {
    pub fn new(total: usize, files: usize, tokens: Option<&'a TokenTotals>) -> Self {
        Progress {
            total,
            files,
            tokens,
            done: AtomicUsize::new(0),
            files_done: AtomicUsize::new(0),
            tty: std::io::stderr().is_terminal(),
            started: Instant::now(),
            last_milestone: Mutex::new(0),
        }
    }

    pub fn silent() -> Self {
        Self::new(0, 0, None)
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// One LLM call finished.
    pub fn tick(&self) {
        self.done.fetch_add(1, Relaxed);
        self.redraw(&mut self.last_milestone.lock().unwrap());
    }

    pub fn file_done(&self) {
        self.files_done.fetch_add(1, Relaxed);
        self.redraw(&mut self.last_milestone.lock().unwrap());
    }

    /// `warning: {msg}` on stderr, keeping the progress line intact.
    pub fn warn(&self, msg: impl Display) {
        let mut last = self.last_milestone.lock().unwrap();
        self.clear_line();
        eprintln!("warning: {msg}");
        self.redraw(&mut last);
    }

    /// A line on stdout (verbose / dry-run detail), keeping the progress line intact.
    pub fn print(&self, msg: impl Display) {
        let mut last = self.last_milestone.lock().unwrap();
        self.clear_line();
        println!("{msg}");
        self.redraw(&mut last);
    }

    pub fn finish(&self) {
        let _guard = self.last_milestone.lock().unwrap();
        self.clear_line();
    }

    fn clear_line(&self) {
        if self.tty && self.total > 0 {
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }

    fn redraw(&self, last: &mut usize) {
        if self.total == 0 {
            return;
        }
        let done = self.done.load(Relaxed).min(self.total);
        let pct = done * 100 / self.total;
        if self.tty {
            eprint!("\r\x1b[2K{}", self.line(done, pct));
            let _ = std::io::stderr().flush();
        } else if pct / 10 > *last {
            *last = pct / 10;
            eprintln!("{}", self.line(done, pct));
        }
    }

    fn line(&self, done: usize, pct: usize) -> String {
        let mut s = format!(
            "{pct:3}%  {done}/{} blocks, {}/{} files",
            self.total,
            self.files_done.load(Relaxed),
            self.files
        );
        let elapsed = self.started.elapsed();
        if done > 0 && done < self.total {
            let eta = elapsed / done as u32 * (self.total - done) as u32;
            s.push_str(&format!(", ~{} left", fmt_duration(eta)));
        }
        if let Some(t) = self.tokens {
            s.push_str(&format!(" | {}", token_rates(t.snapshot(), elapsed)));
        }
        s
    }
}

/// `12.3k in @1150/s, 840 out @42/s`: prompt and completion tokens with their throughput.
pub fn token_rates(u: crate::llm::TokenUsage, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64().max(0.001);
    format!(
        "{} in @{}/s, {} out @{}/s",
        fmt_k(u.prompt),
        fmt_k((u.prompt as f64 / secs) as u64),
        fmt_k(u.completion),
        fmt_k((u.completion as f64 / secs) as u64)
    )
}

fn fmt_k(n: u64) -> String {
    match n {
        0..10_000 => n.to_string(),
        10_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.2}M", n as f64 / 1e6),
    }
}

fn fmt_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_duration_format() {
        assert_eq!(fmt_duration(Duration::from_secs(5)), "5s");
        assert_eq!(fmt_duration(Duration::from_secs(125)), "2m05s");
        assert_eq!(fmt_duration(Duration::from_secs(3660)), "1h01m");
        assert_eq!(fmt_k(999), "999");
        assert_eq!(fmt_k(12_345), "12.3k");
        assert_eq!(fmt_k(2_500_000), "2.50M");
        let totals = TokenTotals::default();
        totals.prompt.fetch_add(12_345, Relaxed);
        totals.completion.fetch_add(840, Relaxed);
        let r = token_rates(totals.snapshot(), Duration::from_secs(10));
        assert_eq!(r, "12.3k in @1234/s, 840 out @84/s");
        let p = Progress::new(200, 10, None);
        p.files_done.fetch_add(3, Relaxed);
        assert_eq!(p.line(0, 0), "  0%  0/200 blocks, 3/10 files");
        assert!(
            p.line(50, 25)
                .starts_with(" 25%  50/200 blocks, 3/10 files, ~")
        );
        assert_eq!(p.line(200, 100), "100%  200/200 blocks, 3/10 files");
    }
}
