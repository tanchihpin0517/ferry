//! Progress: counts kept as a transfer goes, shown on a line that updates in
//! place, and a summary at the end.
//!
//! The client counts, whichever peer it runs: entries as the walk finds or
//! the manifest brings them, files as they are needed and as they are sent or
//! written, and bytes before and after compression. Walking is usually faster
//! than transferring, so the totals settle early.
//!
//! The counts are the process's, in [`COUNTS`]. Only the peer that reports,
//! the client's, counts, so a local copy, which runs both peers in one
//! process, counts once.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// This process's counts.
pub static COUNTS: Progress = Progress::new();

#[derive(Default)]
pub struct Progress {
    /// Entries in the manifest so far.
    pub entries: AtomicU64,
    /// Files whose content is needed.
    pub needed: AtomicU64,
    /// Files sent, or written.
    pub files: AtomicU64,
    /// Their bytes.
    pub bytes: AtomicU64,
    /// The bytes that crossed the connection for them, compressed.
    pub wire: AtomicU64,
}

/// Adds to a count of this process's, when this side counts.
pub fn add(counts: bool, counter: &AtomicU64, amount: u64) {
    if counts {
        counter.fetch_add(amount, Ordering::Relaxed);
    }
}

impl Progress {
    pub const fn new() -> Self {
        Self {
            entries: AtomicU64::new(0),
            needed: AtomicU64::new(0),
            files: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            wire: AtomicU64::new(0),
        }
    }

    fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    /// The line shown while the transfer goes.
    pub fn line(&self, elapsed: Duration) -> String {
        let bytes = Self::get(&self.bytes);
        format!(
            "{} entries · {} of {} files · {} · {}/s{}",
            count(Self::get(&self.entries)),
            count(Self::get(&self.files)),
            count(Self::get(&self.needed)),
            size(bytes),
            size(rate(bytes, elapsed)),
            self.saved(),
        )
    }

    /// The line shown when the transfer is over.
    pub fn summary(&self, elapsed: Duration, dry_run: bool) -> String {
        if dry_run {
            return format!(
                "{} compared, {} to send, in {:.1} s",
                counted(Self::get(&self.entries), "entry", "entries"),
                counted(Self::get(&self.needed), "file", "files"),
                elapsed.as_secs_f64()
            );
        }
        let bytes = Self::get(&self.bytes);
        format!(
            "{} compared, {} sent, {} in {:.1} s ({}/s){}",
            counted(Self::get(&self.entries), "entry", "entries"),
            counted(Self::get(&self.files), "file", "files"),
            size(bytes),
            elapsed.as_secs_f64(),
            size(rate(bytes, elapsed)),
            self.saved(),
        )
    }

    /// How much compression saved, once there is something to say.
    fn saved(&self) -> String {
        let bytes = Self::get(&self.bytes);
        let wire = Self::get(&self.wire);
        if bytes == 0 || wire == 0 || wire >= bytes {
            return String::new();
        }
        format!(
            " · {:.0}% saved by compression",
            100.0 * (1.0 - wire as f64 / bytes as f64)
        )
    }
}

fn rate(bytes: u64, elapsed: Duration) -> u64 {
    let seconds = elapsed.as_secs_f64();
    if seconds > 0.0 {
        (bytes as f64 / seconds) as u64
    } else {
        0
    }
}

/// A count with thousands separated: 1,234,567.
pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut shown = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            shown.push(',');
        }
        shown.push(digit);
    }
    shown
}

/// A count and what it counts, singular or plural: 1 file, 2 files.
pub fn counted(value: u64, one: &str, many: &str) -> String {
    format!("{} {}", count(value), if value == 1 { one } else { many })
}

/// A size in binary units: 1.5 MiB.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Whether to show progress: when asked to, or when standard error is a
/// terminal, where a line updating in place belongs.
pub fn wanted(asked: bool) -> bool {
    asked || std::io::stderr().is_terminal()
}

/// Shows the progress line on standard error, updating it in place, until
/// `done`. On a terminal the line is redrawn; elsewhere, for logs, a line is
/// written every few seconds instead.
pub fn show(progress: &Progress, started: Instant, done: &AtomicBool) {
    let terminal = std::io::stderr().is_terminal();
    let every = if terminal {
        Duration::from_millis(200)
    } else {
        Duration::from_secs(5)
    };
    let mut last = Instant::now();
    while !done.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
        if last.elapsed() < every {
            continue;
        }
        last = Instant::now();
        let line = progress.line(started.elapsed());
        let mut stderr = std::io::stderr().lock();
        let _ = if terminal {
            write!(stderr, "\r\x1b[K{line}")
        } else {
            writeln!(stderr, "{line}")
        };
        let _ = stderr.flush();
    }
    if terminal {
        let _ = write!(std::io::stderr(), "\r\x1b[K");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_sizes_read_well() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(1_234_567), "1,234,567");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.5 KiB");
        assert_eq!(size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn the_summary_says_what_was_done() {
        let progress = Progress::default();
        add(true, &progress.entries, 1_234);
        add(true, &progress.needed, 10);
        add(true, &progress.files, 10);
        add(true, &progress.bytes, 4 * 1024 * 1024);
        add(true, &progress.wire, 1024 * 1024);
        add(false, &progress.wire, 1024 * 1024);
        let summary = progress.summary(Duration::from_secs(2), false);
        assert!(summary.contains("1,234 entries compared"), "{summary}");
        assert_eq!(counted(1, "file", "files"), "1 file");
        assert!(summary.contains("10 files sent, 4.0 MiB"), "{summary}");
        assert!(summary.contains("2.0 MiB/s"), "{summary}");
        assert!(summary.contains("75% saved"), "{summary}");
    }
}
