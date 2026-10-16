//! The dashboard: a full screen, on a terminal, that shows the transfer as
//! it goes, for you to follow it and to find what holds it back; see the
//! [design](../docs/design/dashboard.md).
//!
//! It shows the reports the peers send, as the progress line does, and what
//! the client's own transport knows of each SSH connection. Lines for
//! standard error are kept, and returned, to be written once the terminal is
//! back as it was. Ctrl-C cancels the transfer, as it does without it.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Stderr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::progress::{count, saved, size};
use crate::report::{Counts, Report, Stage, Transfer};
use crate::transport::Monitor;

/// How often the screen is drawn.
const EVERY: Duration = Duration::from_millis(200);
/// Over how long rates are measured.
const RATE_WINDOW: Duration = Duration::from_secs(5);
/// Over how long the queues are averaged, to find the bottleneck.
const QUEUE_WINDOW: Duration = Duration::from_secs(3);
/// A queue at least this full, in percent, holds work back.
const FULL: f64 = 75.0;
/// How many cells the progress bar has, on a screen wide enough.
const BAR: usize = 28;
/// How many of the latest warnings are shown.
const WARNINGS: usize = 6;
/// How many of the files being transferred are shown.
const TRANSFERS: usize = 6;

/// What the dashboard is about: the two locations, and the SSH connections
/// to each host.
pub struct View {
    pub from: String,
    pub to: String,
    pub monitors: Vec<Monitor>,
}

/// The terminal is taken.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Puts the terminal back as it was, if the dashboard has it.
fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stderr(), terminal::LeaveAlternateScreen, cursor::Show);
    }
}

/// Shows the dashboard until every reporter is gone, and returns the counts
/// and the lines for standard error, to write once it is closed.
pub fn show(
    reports: Receiver<Report>,
    view: &View,
    started: Instant,
) -> io::Result<(Counts, Vec<String>)> {
    terminal::enable_raw_mode()?;
    ACTIVE.store(true, Ordering::SeqCst);
    // However the run ends, the terminal is put back.
    crate::interrupt::on_exit(Some(restore));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    // Cleared by hand: clearing through the terminal reads the cursor's
    // position, which not every terminal answers.
    let result = execute!(
        io::stderr(),
        terminal::EnterAlternateScreen,
        terminal::Clear(terminal::ClearType::All),
        cursor::Hide
    )
    .and_then(|()| Terminal::new(CrosstermBackend::new(io::stderr())))
    .and_then(|mut screen| run(&mut screen, reports, view, started));
    restore();
    crate::interrupt::on_exit(None);
    result
}

fn run(
    screen: &mut Terminal<CrosstermBackend<Stderr>>,
    reports: Receiver<Report>,
    view: &View,
    started: Instant,
) -> io::Result<(Counts, Vec<String>)> {
    let mut state = State::new(view.monitors.len(), started);
    let mut drawn = Instant::now() - EVERY;
    loop {
        match reports.recv_timeout(Duration::from_millis(20)) {
            Ok(report) => state.take(report, Instant::now()),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if event::poll(Duration::ZERO)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            // Raw, the terminal sends no signal for Ctrl-C: the process
            // sends itself one, which cancels the transfer as usual.
            restore();
            // SAFETY: `kill` with this process's own pid and a signal.
            unsafe {
                libc::kill(libc::getpid(), libc::SIGINT);
            }
        }
        if drawn.elapsed() >= EVERY {
            drawn = Instant::now();
            state.sample(view, drawn);
            screen.draw(|frame| draw(frame, &state, view, drawn))?;
        }
    }
    Ok((state.counts, state.said))
}

/// Samples of a count, over a window.
type History = VecDeque<(Instant, u64)>;

/// An SSH connection, as seen: its bytes, and the age of its last heartbeat.
type Seen = (History, Option<Duration>);

/// A file being transferred, as last reported, and its bytes over the last
/// [`RATE_WINDOW`].
struct Transferring {
    file: Transfer,
    history: History,
}

/// What the dashboard has learnt so far.
struct State {
    started: Instant,
    counts: Counts,
    /// Each stage's queue, as reported, over the last [`QUEUE_WINDOW`].
    queues: BTreeMap<Stage, VecDeque<(Instant, u8)>>,
    /// The bytes sent, over the last [`RATE_WINDOW`].
    bytes: VecDeque<(Instant, u64)>,
    /// For each monitor, each SSH connection's bytes, over the last
    /// [`RATE_WINDOW`], and the age of its last heartbeat.
    connections: Vec<Vec<Seen>>,
    /// Lines for standard error.
    said: Vec<String>,
    /// The files being transferred, those first reported first.
    transfers: Vec<Transferring>,
}

impl State {
    fn new(monitors: usize, started: Instant) -> Self {
        Self {
            started,
            counts: Counts::default(),
            queues: BTreeMap::new(),
            bytes: VecDeque::new(),
            connections: vec![Vec::new(); monitors],
            said: Vec::new(),
            transfers: Vec::new(),
        }
    }

    fn take(&mut self, report: Report, now: Instant) {
        match report {
            Report::Counts(more) => self.counts.add(&more),
            Report::Said(said) => self.said.push(said),
            Report::Transferring(files) => {
                self.transfers
                    .retain(|shown| files.iter().any(|file| file.id == shown.file.id));
                for file in files {
                    match self
                        .transfers
                        .iter_mut()
                        .find(|shown| shown.file.id == file.id)
                    {
                        Some(shown) => {
                            push(&mut shown.history, now, file.done);
                            shown.file = file;
                        }
                        None => {
                            let mut history = VecDeque::new();
                            push(&mut history, now, file.done);
                            self.transfers.push(Transferring { file, history });
                        }
                    }
                }
            }
            Report::Queues(levels) => {
                for (stage, level) in levels {
                    let history = self.queues.entry(stage).or_default();
                    history.push_back((now, level));
                    while history
                        .front()
                        .is_some_and(|(when, _)| now.duration_since(*when) > QUEUE_WINDOW)
                    {
                        history.pop_front();
                    }
                }
            }
            // A dry run has no dashboard.
            Report::Listed(_) | Report::Busy(_) => {}
            // The relay keeps these.
            Report::Profile(_) | Report::Snapshot(_) | Report::Metrics(_) => {}
        }
    }

    /// Takes the bytes so far, and each SSH connection's.
    fn sample(&mut self, view: &View, now: Instant) {
        push(&mut self.bytes, now, self.counts.received);
        for (monitor, connections) in view.monitors.iter().zip(&mut self.connections) {
            let seen = monitor.connections();
            connections.resize_with(seen.len(), || (VecDeque::new(), None));
            for ((history, age), (bytes, heartbeat)) in connections.iter_mut().zip(seen) {
                push(history, now, bytes);
                *age = heartbeat;
            }
        }
    }

    /// Each stage's average level over the last [`QUEUE_WINDOW`].
    fn averages(&self) -> BTreeMap<Stage, f64> {
        self.queues
            .iter()
            .filter(|(_, history)| !history.is_empty())
            .map(|(stage, history)| {
                let sum: f64 = history.iter().map(|(_, level)| f64::from(*level)).sum();
                (*stage, sum / history.len() as f64)
            })
            .collect()
    }
}

/// Adds a sample, keeping the last [`RATE_WINDOW`] of them.
fn push(history: &mut VecDeque<(Instant, u64)>, now: Instant, value: u64) {
    history.push_back((now, value));
    while history.len() > 2
        && history
            .front()
            .is_some_and(|(when, _)| now.duration_since(*when) > RATE_WINDOW)
    {
        history.pop_front();
    }
}

/// Bytes a second over the samples.
fn rate(history: &VecDeque<(Instant, u64)>) -> u64 {
    match (history.front(), history.back()) {
        (Some((first, from)), Some((last, to))) if last > first => {
            (to.saturating_sub(*from) as f64 / last.duration_since(*first).as_secs_f64()) as u64
        }
        _ => 0,
    }
}

/// The stage the others wait for: the first whose queue stays full while
/// the queue of the stage after it does not, or the last, if every queue up
/// to it is full. None when no queue is full: the transfer waits on none of
/// its stages, as when it is scanning.
fn bottleneck(averages: &BTreeMap<Stage, f64>) -> Option<Stage> {
    let reported: Vec<(Stage, f64)> = Stage::ALL
        .iter()
        .filter_map(|stage| averages.get(stage).map(|level| (*stage, *level)))
        .collect();
    reported
        .iter()
        .enumerate()
        .find(|(index, (_, level))| {
            *level >= FULL && reported.get(index + 1).is_none_or(|(_, next)| *next < FULL)
        })
        .map(|(_, (stage, _))| *stage)
}

/// The time left, at the rate of the last few seconds.
fn time_left(counts: &Counts, rate: u64) -> Option<Duration> {
    let left = counts.to_send.checked_sub(counts.received)?;
    (rate > 0).then(|| Duration::from_secs(left / rate))
}

fn clock(time: Duration) -> String {
    let seconds = time.as_secs();
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

/// `path` in at most `room` characters: one too long loses its middle to
/// `…`, keeping where it starts and its file name.
fn shortened(path: &str, room: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= room {
        return path.to_owned();
    }
    let Some(kept) = room.checked_sub(1) else {
        return String::new();
    };
    let head = kept / 2;
    let tail = kept - head;
    let mut shown: String = chars[..head].iter().collect();
    shown.push('…');
    shown.extend(&chars[chars.len() - tail..]);
    shown
}

/// The lines of the files being transferred, on a screen `width` wide: a
/// heading, then each file, the first [`TRANSFERS`] of them, with how much of
/// it has come, its size, its rate, and its time left.
fn transfers(transfers: &[Transferring], width: usize, dim: Style) -> Vec<Line<'static>> {
    let columns = |percent: &str, size: &str, rate: &str, left: &str| {
        format!("{percent:>5}{size:>12}{rate:>14}{left:>8}")
    };
    let headings = columns("", "size", "rate", "ETA");
    // The path takes what the columns leave.
    let room = width.saturating_sub(3 + headings.chars().count() + 1);
    let mut lines = vec![Line::styled(
        format!(" {:<room$}  {headings}", "transferring"),
        dim,
    )];
    if transfers.is_empty() {
        lines.push(Line::styled("   none", dim));
    }
    for shown in transfers.iter().take(TRANSFERS) {
        let file = &shown.file;
        let percent = (file.done * 100).checked_div(file.size).unwrap_or(100);
        // A file all here waits for its writer: it has no rate any more.
        let speed = if file.done < file.size {
            rate(&shown.history)
        } else {
            0
        };
        let left = (speed > 0).then(|| clock(Duration::from_secs((file.size - file.done) / speed)));
        let path = shortened(&file.path, room);
        lines.push(Line::raw(format!(
            "   {path:<room$}{}",
            columns(
                &format!("{percent}%"),
                &size(file.size),
                &if speed > 0 {
                    format!("{}/s", size(speed))
                } else {
                    "—".to_owned()
                },
                left.as_deref().unwrap_or("—"),
            )
        )));
    }
    if transfers.len() > TRANSFERS {
        lines.push(Line::styled(
            format!(
                "   … and {} more",
                count((transfers.len() - TRANSFERS) as u64)
            ),
            dim,
        ));
    }
    lines
}

/// Four cells, as full as a queue.
fn cells(level: f64) -> String {
    let full = ((level / 25.0).round() as usize).min(4);
    format!("{}{}", "▮".repeat(full), "▯".repeat(4 - full))
}

fn draw(frame: &mut ratatui::Frame, state: &State, view: &View, now: Instant) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let counts = &state.counts;
    let speed = rate(&state.bytes);

    let mut lines: Vec<Line> = Vec::new();
    // The counts.
    lines.push(Line::from(vec![
        Span::styled(" compared ", dim),
        Span::raw(format!("{:>12} entries", count(counts.entries))),
        Span::styled("      to send ", dim),
        Span::raw(format!("{:>10} files", count(counts.needed))),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" sent     ", dim),
        Span::raw(format!("{:>12} files  ", count(counts.files))),
        Span::styled("      failed  ", dim),
        Span::raw(format!("{:>10}", count(counts.failed + counts.problems))),
    ]));
    // The files being transferred.
    lines.push(Line::raw(""));
    lines.extend(transfers(
        &state.transfers,
        frame.area().width as usize,
        dim,
    ));
    // Each host's SSH connections.
    for (monitor, connections) in view.monitors.iter().zip(&state.connections) {
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(format!(" SSH connections to {}", monitor.host()), dim),
            Span::styled(format!("{:>14}{:>16}", "rate", "heartbeat"), dim),
        ]));
        let total = connections.len();
        for (index, (history, age)) in connections.iter().enumerate() {
            let heartbeat =
                age.map_or_else(|| "—".to_owned(), |age| format!("{} s ago", age.as_secs()));
            lines.push(Line::raw(format!(
                "   {:<24}{:>13}/s{:>16}",
                format!("{} of {total}", index + 1),
                size(rate(history)),
                heartbeat
            )));
        }
    }
    // The pipeline.
    let averages = state.averages();
    lines.push(Line::raw(""));
    let mut stages = vec![Span::styled(" pipeline  ", dim)];
    for stage in Stage::ALL {
        if let Some(level) = averages.get(&stage) {
            stages.push(Span::raw(format!("{} {}  ", stage.name(), cells(*level))));
        }
    }
    lines.push(Line::from(stages));
    let bottleneck = bottleneck(&averages).map_or("none", Stage::name);
    let compression = saved(counts);
    lines.push(Line::from(vec![
        Span::styled(" bottleneck: ", dim),
        Span::raw(bottleneck).bold(),
        Span::raw(compression.replace(" · ", "      ")),
    ]));
    // The latest warnings.
    lines.push(Line::raw(""));
    lines.push(Line::styled(" warnings and failures", dim));
    let latest = state.said.len().saturating_sub(WARNINGS);
    if state.said.is_empty() {
        lines.push(Line::styled("   none", dim));
    }
    for said in &state.said[latest..] {
        lines.push(Line::raw(format!("   {said}")));
    }

    let [title, bar, _, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(frame.area());
    let elapsed = clock(now.duration_since(state.started));
    let heading = format!(" ferry: {} → {}", view.from, view.to);
    let padding =
        (title.width as usize).saturating_sub(heading.chars().count() + elapsed.len() + 10);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(heading).bold(),
            Span::raw(" ".repeat(padding)),
            Span::styled(format!("{elapsed} elapsed"), dim),
        ])),
        title,
    );
    let ratio = if counts.to_send > 0 {
        (counts.received as f64 / counts.to_send as f64).min(1.0)
    } else {
        0.0
    };
    let left = time_left(counts, speed).map_or_else(|| "—".to_owned(), clock);
    let progress = format!(
        "{:.0}%   {} of {}   {}/s   ETA {left}",
        ratio * 100.0,
        size(counts.received.min(counts.to_send)),
        size(counts.to_send),
        size(speed),
    );
    // The bar beside the text, never under it; on a narrow screen, the bar
    // gives way first, and goes when too short to say anything.
    let cells = BAR.min((bar.width as usize).saturating_sub(progress.chars().count() + 3));
    let cells = if cells < 5 { 0 } else { cells };
    let done = ((ratio * cells as f64).round() as usize).min(cells);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::raw("█".repeat(done)),
            Span::styled("░".repeat(cells - done), dim),
            Span::raw(if cells > 0 { "  " } else { "" }),
            Span::raw(progress),
        ])),
        bar,
    );
    frame.render_widget(Paragraph::new(lines), body);
}

/// The dashboard drawn once, on a screen of `width` by `height`: for tests.
#[cfg(test)]
fn drawn(state: &State, view: &View, now: Instant, width: u16, height: u16) -> String {
    let mut screen = Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    screen.draw(|frame| draw(frame, state, view, now)).unwrap();
    let buffer = screen.backend().buffer();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn averages(levels: &[(Stage, f64)]) -> BTreeMap<Stage, f64> {
        levels.iter().copied().collect()
    }

    #[test]
    fn the_bottleneck_is_the_stage_the_others_wait_for() {
        use Stage::*;
        // A slow network: every queue before it full, the ones after empty.
        let slow_network = averages(&[
            (Read, 100.0),
            (Compress, 90.0),
            (Network, 100.0),
            (Decompress, 0.0),
            (Write, 10.0),
        ]);
        assert_eq!(bottleneck(&slow_network), Some(Network));
        // A slow disk at the far end: every queue full.
        let slow_disk = averages(&[
            (Read, 100.0),
            (Compress, 100.0),
            (Network, 100.0),
            (Decompress, 100.0),
            (Write, 100.0),
        ]);
        assert_eq!(bottleneck(&slow_disk), Some(Write));
        // Slow compression.
        let slow_compression = averages(&[(Read, 80.0), (Compress, 100.0), (Network, 5.0)]);
        assert_eq!(bottleneck(&slow_compression), Some(Compress));
        // Nothing waits: scanning.
        assert_eq!(bottleneck(&averages(&[(Read, 0.0), (Write, 0.0)])), None);
    }

    #[test]
    fn a_long_path_loses_its_middle() {
        assert_eq!(shortened("a/b.txt", 10), "a/b.txt");
        assert_eq!(
            shortened("photos/2024/08/IMG_4211.jpg", 15),
            "photos/…211.jpg"
        );
        assert_eq!(
            shortened("photos/2024/08/IMG_4211.jpg", 15).chars().count(),
            15
        );
        assert_eq!(shortened("abc", 0), "");
        assert_eq!(shortened("abc", 1), "…");
    }

    #[test]
    fn the_time_left_is_at_the_latest_rate() {
        let counts = Counts {
            received: 100,
            to_send: 1_100,
            ..Counts::default()
        };
        assert_eq!(time_left(&counts, 10), Some(Duration::from_secs(100)));
        assert_eq!(time_left(&counts, 0), None);
        assert_eq!(clock(Duration::from_secs(3_725)), "1:02:05");
        assert_eq!(clock(Duration::from_secs(65)), "01:05");
    }

    #[test]
    fn it_shows_the_transfer() {
        let started = Instant::now();
        let mut state = State::new(0, started);
        let now = started + Duration::from_secs(42);
        state.take(
            Report::Counts(Counts {
                entries: 10_000,
                needed: 8_000,
                files: 4_620,
                bytes: 600 << 20,
                received: 600 << 20,
                to_send: 1 << 30,
                wire: 300 << 20,
                ..Counts::default()
            }),
            now,
        );
        state.take(
            Report::Queues(vec![(Stage::Network, 100), (Stage::Read, 100)]),
            now,
        );
        state.take(Report::Queues(vec![(Stage::Write, 0)]), now);
        state.take(Report::Said("cannot read sub/locked.txt".into()), now);
        let image = |done: u64| Transfer {
            id: 7,
            path: "photos/2024/08/IMG_4211.jpg".into(),
            size: 100 << 20,
            done,
        };
        state.take(
            Report::Transferring(vec![image(20 << 20)]),
            now - Duration::from_secs(1),
        );
        state.take(Report::Transferring(vec![image(45 << 20)]), now);
        let view = View {
            from: "./data".into(),
            to: "ntu:/backup".into(),
            monitors: Vec::new(),
        };
        let text = drawn(&state, &view, now, 90, 24);
        for expected in [
            "ferry: ./data → ntu:/backup",
            "00:42 elapsed",
            // The bar beside the text, not under it.
            " ████████████████░░░░░░░░░░░░  59%   600.0 MiB of 1.0 GiB",
            "10,000 entries",
            "4,620 files",
            "transferring",
            "size          rate     ETA",
            "photos/2024/08/IMG_4211.jpg",
            "45%   100.0 MiB    25.0 MiB/s   00:02",
            "network ▮▮▮▮",
            "bottleneck: network",
            "50% saved by compression",
            "cannot read sub/locked.txt",
        ] {
            assert!(text.contains(expected), "no {expected:?} in:\n{text}");
        }
        // On a narrow screen, the bar gives way, and the text stays whole.
        let narrow = drawn(&state, &view, now, 46, 24);
        let line = narrow.lines().nth(1).unwrap();
        assert!(line.starts_with(" 59%   600.0 MiB of 1.0 GiB"), "{line:?}");
    }

    #[test]
    fn the_files_shown_are_the_first_reported_and_a_count_of_the_rest() {
        let started = Instant::now();
        let mut state = State::new(0, started);
        let file = |id: u64| Transfer {
            id,
            path: format!("file{id}"),
            size: 10,
            done: 0,
        };
        state.take(Report::Transferring((1..=5).map(file).collect()), started);
        // The first is done; four more start, after the others.
        state.take(Report::Transferring((2..=9).map(file).collect()), started);
        let ids: Vec<u64> = state.transfers.iter().map(|shown| shown.file.id).collect();
        assert_eq!(ids, [2, 3, 4, 5, 6, 7, 8, 9]);
        let lines: Vec<String> = transfers(&state.transfers, 80, Style::default())
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(lines.len(), 1 + TRANSFERS + 1);
        assert!(lines[1].starts_with("   file2 "), "{lines:?}");
        assert_eq!(lines.last().unwrap(), "   … and 2 more");
        assert!(
            lines.iter().all(|line| line.chars().count() <= 80),
            "{lines:?}"
        );
        state.take(Report::Transferring(Vec::new()), started);
        let lines = transfers(&state.transfers, 80, Style::default());
        assert_eq!(lines[1].to_string(), "   none");
    }
}
