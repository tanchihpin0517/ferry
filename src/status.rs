//! What the client is doing before a transfer shows its own progress, such
//! as logging in to a host, or installing Ferry there: a line on standard
//! error, redrawn in place with the time it has taken, so that a step that
//! takes long does not look stuck.
//!
//! The innermost step is shown, and only once it has taken a moment, so that
//! quick steps do not flicker. Nothing is shown where standard error is not a
//! terminal. The line is cleared when its step ends, and before anything else
//! is written to standard error with [`say`], or asked on the terminal.
//!
//! During a transfer, the display holds the terminal: what is asked then,
//! as a login to an SSH connection to make again, has the display give it up
//! first, and take it back after; see [`lend`].

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, Once};
use std::time::{Duration, Instant};

use ratatui::crossterm::terminal;

use crate::interrupt;

/// How long a step goes before it is shown.
const AFTER: Duration = Duration::from_millis(300);
/// How often the line is redrawn.
const EVERY: Duration = Duration::from_millis(100);

/// What a step says, each time the line is drawn.
type Text = Box<dyn Fn() -> String + Send>;

struct Step {
    text: Text,
    started: Instant,
}

struct Line {
    /// The steps being taken, the innermost last.
    steps: Vec<Step>,
    /// What is drawn, if anything.
    drawn: Option<String>,
}

static LINE: Mutex<Line> = Mutex::new(Line {
    steps: Vec::new(),
    drawn: None,
});

/// A step being shown; dropping it clears the line, for the step around it,
/// if any, to be shown again.
#[must_use]
pub struct Shown(bool);

impl Drop for Shown {
    fn drop(&mut self) {
        if !self.0 {
            return;
        }
        let mut line = LINE.lock().expect("not poisoned");
        line.steps.pop();
        clear(&mut line);
        if line.steps.is_empty() {
            interrupt::on_exit(None);
        }
    }
}

/// Shows `text` as the step being taken, until the result is dropped.
pub fn show(text: impl Into<String>) -> Shown {
    let text = text.into();
    show_with(move || text.clone())
}

/// Shows what `text` says each time the line is drawn, such as how much of
/// something has been done, until the result is dropped.
pub fn show_with(text: impl Fn() -> String + Send + 'static) -> Shown {
    if !io::stderr().is_terminal() {
        return Shown(false);
    }
    static TICKER: Once = Once::new();
    TICKER.call_once(|| {
        std::thread::spawn(tick);
    });
    let mut line = LINE.lock().expect("not poisoned");
    clear(&mut line);
    line.steps.push(Step {
        text: Box::new(text),
        started: Instant::now(),
    });
    // Interrupted, the process leaves the terminal at a clean line.
    interrupt::on_exit(Some(clear_on_exit));
    Shown(true)
}

/// Writes `ferry: <said>` on a line of its own, the step's line cleared
/// first and drawn again after.
pub fn say(said: impl std::fmt::Display) {
    let mut line = LINE.lock().expect("not poisoned");
    clear(&mut line);
    eprintln!("ferry: {said}");
}

/// Clears the line for something else to be written or asked, such as a
/// password, until the result is dropped; the steps are shown again after,
/// their time going on from where it was.
pub fn aside() -> Aside {
    let mut line = LINE.lock().expect("not poisoned");
    clear(&mut line);
    let steps = std::mem::take(&mut line.steps)
        .into_iter()
        .map(|step| {
            let elapsed = step.started.elapsed();
            (step, elapsed)
        })
        .collect();
    Aside(steps)
}

/// Whether the terminal is lent, for something to be asked on it.
static LENT: AtomicBool = AtomicBool::new(false);

/// Whether a transfer's display holds the terminal, which it then gives up
/// while it is lent.
static DISPLAYING: AtomicBool = AtomicBool::new(false);

/// Whether the display has given the terminal up, and a way to wait for it.
static GIVEN_UP: Mutex<bool> = Mutex::new(false);
static GIVEN_UP_CHANGED: Condvar = Condvar::new();

/// How long the display is waited for to give the terminal up: it looks
/// every few tens of milliseconds.
const GIVING_UP: Duration = Duration::from_secs(1);

/// The terminal lent, until dropped; see [`lend`].
#[must_use]
pub struct Lent;

impl Drop for Lent {
    fn drop(&mut self) {
        LENT.store(false, Ordering::SeqCst);
    }
}

/// Lends the terminal, for something to be asked on it during a transfer:
/// the display, if one holds it, gives it up, its lines cleared, nor reads
/// what is typed, until the result is dropped.
pub fn lend() -> Lent {
    LENT.store(true, Ordering::SeqCst);
    if DISPLAYING.load(Ordering::SeqCst) {
        let deadline = Instant::now() + GIVING_UP;
        let mut given_up = GIVEN_UP.lock().expect("not poisoned");
        while !*given_up {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            given_up = GIVEN_UP_CHANGED
                .wait_timeout(given_up, left)
                .expect("not poisoned")
                .0;
        }
    }
    Lent
}

/// For a transfer's display: whether the terminal is lent, to give it up
/// while it is.
pub fn lent() -> bool {
    LENT.load(Ordering::SeqCst)
}

/// For a transfer's display: it holds the terminal, or no longer does.
pub fn displaying(holding: bool) {
    DISPLAYING.store(holding, Ordering::SeqCst);
}

/// For a transfer's display: it has given the terminal up, or taken it
/// back.
pub fn given_up(given: bool) {
    *GIVEN_UP.lock().expect("not poisoned") = given;
    GIVEN_UP_CHANGED.notify_all();
}

/// The line cleared, the steps kept to show again; see [`aside`].
pub struct Aside(Vec<(Step, Duration)>);

impl Drop for Aside {
    fn drop(&mut self) {
        let mut line = LINE.lock().expect("not poisoned");
        for (mut step, elapsed) in self.0.drain(..) {
            step.started = Instant::now() - elapsed;
            line.steps.push(step);
        }
    }
}

fn clear(line: &mut Line) {
    if line.drawn.take().is_some() {
        let mut stderr = io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[K");
        let _ = stderr.flush();
    }
}

fn clear_on_exit() {
    if let Ok(mut line) = LINE.try_lock() {
        clear(&mut line);
    }
}

fn tick() {
    loop {
        std::thread::sleep(EVERY);
        let mut line = LINE.lock().expect("not poisoned");
        let Some(step) = line.steps.last() else {
            continue;
        };
        let elapsed = step.started.elapsed();
        if elapsed < AFTER {
            continue;
        }
        let text = drawn(&(step.text)(), elapsed);
        if line.drawn.as_deref() == Some(&text) {
            continue;
        }
        let width = match terminal::size() {
            Ok((columns, _)) if columns > 0 => usize::from(columns),
            _ => 80,
        };
        let mut stderr = io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[K{}", crate::progress::fitted(&text, width));
        let _ = stderr.flush();
        line.drawn = Some(text);
    }
}

/// The line for a step that says `text`, and has taken `elapsed`.
fn drawn(text: &str, elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds == 0 {
        format!("ferry: {text}…")
    } else {
        format!("ferry: {text}… {seconds} s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lent, the terminal is given up by the display that holds it before
    /// anything is asked, and taken back once the lending ends; with no
    /// display, it is lent at once.
    #[test]
    fn a_display_gives_the_terminal_up_while_it_is_lent() {
        drop(lend());
        displaying(true);
        let display = std::thread::spawn(|| {
            let mut given = false;
            let mut times = 0;
            while times < 2 {
                if lent() != given {
                    given = !given;
                    given_up(given);
                    times += 1;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let lent = lend();
        assert!(
            *GIVEN_UP.lock().unwrap(),
            "lent before the display gave it up"
        );
        drop(lent);
        display.join().unwrap();
        assert!(!*GIVEN_UP.lock().unwrap());
        displaying(false);
    }

    #[test]
    fn a_step_says_what_it_does_and_how_long_it_has_taken() {
        assert_eq!(
            drawn("connecting to fatcat", Duration::from_millis(400)),
            "ferry: connecting to fatcat…"
        );
        assert_eq!(
            drawn("checking Ferry on fatcat", Duration::from_secs(12)),
            "ferry: checking Ferry on fatcat… 12 s"
        );
    }
}
