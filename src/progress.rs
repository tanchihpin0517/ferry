//! Progress: the counts the peers report, shown on a line that updates in
//! place, and a summary at the end.
//!
//! The peers count, and report the counts; see [`crate::report`]. The client
//! shows them, with what else the peers report: the lines a dry run lists,
//! on standard output, and lines about what goes wrong, on standard error.
//! Scanning is usually faster than transferring, so the totals settle early.

use std::io::{self, IsTerminal, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use ratatui::crossterm::terminal;

use crate::report::{Counts, Report};

/// The line shown while the transfer goes.
pub fn line(counts: &Counts, elapsed: Duration) -> String {
    format!(
        "{} entries · {} of {} files · {} · {}/s{}",
        count(counts.entries),
        count(counts.files),
        count(counts.needed),
        size(counts.bytes),
        size(rate(counts.bytes, elapsed)),
        saved(counts),
    )
}

/// The line shown while a dry run goes: how far the comparison has got, and
/// what holds it back, since a dry run may list nothing for a long time.
pub fn dry_run_line(counts: &Counts, elapsed: Duration, busy: &str) -> String {
    let mut line = format!(
        "{} compared · {} to send · {}",
        count(counts.entries),
        count(counts.needed),
        clock(elapsed),
    );
    if !busy.is_empty() {
        line.push_str(" · ");
        line.push_str(busy);
    }
    line
}

/// A time as minutes and seconds, or hours too: 1:05, 1:02:03.
fn clock(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

/// `line` cut to fit a terminal `width` wide, so that it never wraps, which
/// would leave its start behind each time it is redrawn.
pub(crate) fn fitted(line: &str, width: usize) -> String {
    if line.chars().count() < width {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(width.saturating_sub(2)).collect();
    cut.push('…');
    cut
}

/// What [`show`] shows as it goes, besides what the peers say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    /// Nothing.
    None,
    /// The transfer's counts.
    Transfer,
    /// A dry run's comparison, and what holds it back.
    DryRun,
}

/// The line shown when the transfer is over.
pub fn summary(counts: &Counts, elapsed: Duration, dry_run: bool) -> String {
    if dry_run {
        return format!(
            "{} compared, {} to send, in {:.1} s",
            counted(counts.entries, "entry", "entries"),
            counted(counts.needed, "file", "files"),
            elapsed.as_secs_f64()
        );
    }
    format!(
        "{} compared, {} sent, {} in {:.1} s ({}/s){}",
        counted(counts.entries, "entry", "entries"),
        counted(counts.files, "file", "files"),
        size(counts.bytes),
        elapsed.as_secs_f64(),
        size(rate(counts.bytes, elapsed)),
        saved(counts),
    )
}

/// How much compression saved, once there is something to say.
pub fn saved(counts: &Counts) -> String {
    let (bytes, wire) = (counts.bytes, counts.wire);
    if bytes == 0 || wire == 0 || wire >= bytes {
        return String::new();
    }
    format!(
        " · {:.0}% saved by compression",
        100.0 * (1.0 - wire as f64 / bytes as f64)
    )
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

/// Shows the reports as they come, until every reporter is gone, and
/// returns the counts. Listed lines go to standard output, and other lines to
/// standard error. A [`Line`] on standard error shows how it goes: on a
/// terminal it is redrawn in place, and elsewhere, for logs, a line is
/// written every few seconds instead. Listed lines are written out as they
/// come when standard output is a terminal, the line first cleared.
pub fn show(reports: Receiver<Report>, shown: Line, started: Instant) -> io::Result<Counts> {
    let terminal = io::stderr().is_terminal();
    let listing_to_terminal = io::stdout().is_terminal();
    let every = if terminal {
        Duration::from_millis(200)
    } else {
        Duration::from_secs(5)
    };
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let mut counts = Counts::default();
    let mut busy = String::new();
    let mut drawn = false;
    let mut last = Instant::now();
    loop {
        match reports.recv_timeout(Duration::from_millis(50)) {
            Ok(Report::Counts(more)) => counts.add(&more),
            Ok(Report::Busy(now)) => busy = now,
            Ok(Report::Listed(lines)) => {
                if listing_to_terminal && drawn {
                    write!(io::stderr(), "\r\x1b[K")?;
                    drawn = false;
                }
                for line in lines {
                    stdout.write_all(&line)?;
                    stdout.write_all(b"\n")?;
                }
                if listing_to_terminal {
                    stdout.flush()?;
                }
            }
            Ok(Report::Said(said)) => {
                let mut stderr = io::stderr().lock();
                if drawn {
                    write!(stderr, "\r\x1b[K")?;
                    drawn = false;
                }
                writeln!(stderr, "ferry: {said}")?;
            }
            // For the dashboard.
            Ok(Report::Queues(_) | Report::Writing(_)) | Err(RecvTimeoutError::Timeout) => {}
            // The relay keeps these.
            Ok(Report::Profile(_) | Report::Snapshot(_) | Report::Metrics(_)) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if shown != Line::None && last.elapsed() >= every {
            last = Instant::now();
            let line = match shown {
                Line::DryRun => dry_run_line(&counts, started.elapsed(), &busy),
                _ => line(&counts, started.elapsed()),
            };
            let mut stderr = io::stderr().lock();
            if terminal {
                // A terminal that does not say how wide it is says 0.
                let width = match terminal::size() {
                    Ok((columns, _)) if columns > 0 => usize::from(columns),
                    _ => 80,
                };
                write!(stderr, "\r\x1b[K{}", fitted(&line, width))?;
                drawn = true;
            } else {
                writeln!(stderr, "{line}")?;
            }
            stderr.flush()?;
        }
    }
    if drawn {
        write!(io::stderr(), "\r\x1b[K")?;
    }
    stdout.flush()?;
    Ok(counts)
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
    fn a_dry_run_says_how_far_it_got_and_what_holds_it_back() {
        let counts = Counts {
            entries: 312_450,
            needed: 12,
            ..Counts::default()
        };
        let busy = "Source on fatcat: list directories";
        let line = dry_run_line(&counts, Duration::from_secs(102), busy);
        assert_eq!(
            line,
            "312,450 compared · 12 to send · 1:42 · Source on fatcat: list directories"
        );
        // It fits a terminal 80 wide, with a host's name.
        assert!(line.chars().count() < 80, "{line}");
        assert_eq!(
            dry_run_line(&Counts::default(), Duration::from_secs(3723), ""),
            "0 compared · 0 to send · 1:02:03"
        );
        assert_eq!(fitted("abcdef", 10), "abcdef");
        assert_eq!(fitted("abcdefghijkl", 10), "abcdefgh…");
        assert!(fitted("abcdefghijkl", 10).chars().count() < 10);
    }

    #[test]
    fn the_summary_says_what_was_done() {
        let counts = Counts {
            entries: 1_234,
            needed: 10,
            files: 10,
            bytes: 4 * 1024 * 1024,
            wire: 1024 * 1024,
            ..Counts::default()
        };
        let summary = summary(&counts, Duration::from_secs(2), false);
        assert!(summary.contains("1,234 entries compared"), "{summary}");
        assert_eq!(counted(1, "file", "files"), "1 file");
        assert!(summary.contains("10 files sent, 4.0 MiB"), "{summary}");
        assert!(summary.contains("2.0 MiB/s"), "{summary}");
        assert!(summary.contains("75% saved"), "{summary}");
    }
}
