//! The dashboard: a full screen, on a terminal, that shows the transfer as
//! it goes, for you to follow it and to find what holds it back; see the
//! [design](../docs/design/dashboard.md).
//!
//! It shows the reports the peers send, as the progress line does, and what
//! the client's own transport knows of each SSH connection. Lines for
//! standard error are kept, and returned, to be written once the terminal is
//! back as it was. Ctrl-C cancels the transfer, as it does without it.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, IsTerminal, Stderr, Write};
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

use crate::profile::QueueMetrics;
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
/// How many of the latest entries a dry run listed are shown.
const LATEST: usize = 5;
/// How many seconds the history shows, unless it shows the whole transfer.
const RECENT: usize = 120;
/// How many lines of braille dots the history has above its axis, for bytes,
/// and below it, for files.
const ABOVE: usize = 4;
const BELOW: usize = 3;
/// How wide the history's scale, beside it, may be: `1023.9 MiB/s`,
/// `12,345 files/s`.
const SCALE: usize = 16;

/// What the dashboard is about: the two locations, and the SSH connections
/// to each host.
pub struct View {
    pub from: String,
    pub to: String,
    pub monitors: Vec<Monitor>,
    /// A dry run: what it lists, and how far its comparison has got, rather
    /// than what it sends.
    pub dry_run: bool,
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
    // A dry run's listing goes to standard output as it comes, unless that is
    // a terminal, which the dashboard has: there it is only counted.
    if !io::stdout().is_terminal() {
        state.listing = Some(io::BufWriter::new(io::stdout()));
    }
    let mut drawn = Instant::now() - EVERY;
    loop {
        match reports.recv_timeout(Duration::from_millis(20)) {
            Ok(report) => state.take(report, Instant::now())?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if event::poll(Duration::ZERO)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                // Raw, the terminal sends no signal for Ctrl-C: the process
                // sends itself one, which cancels the transfer as usual.
                restore();
                // SAFETY: `kill` with this process's own pid and a signal.
                unsafe {
                    libc::kill(libc::getpid(), libc::SIGINT);
                }
            } else if key.code == KeyCode::Char('t') {
                // The history over the whole transfer, or its last minutes.
                state.whole = !state.whole;
                drawn = Instant::now() - EVERY;
            }
        }
        if drawn.elapsed() >= EVERY {
            drawn = Instant::now();
            state.sample(view, drawn);
            screen.draw(|frame| draw(frame, &state, view, drawn))?;
        }
    }
    if let Some(listing) = &mut state.listing {
        listing.flush()?;
    } else if state.listed > 0 {
        state.said.push(format!(
            "{} listed, not written to the terminal: redirect standard output to see \
             them, as with `> file` or `| less`",
            crate::progress::counted(state.listed, "entry", "entries")
        ));
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
    /// The entries compared, and the files whose content arrived, over the
    /// last [`RATE_WINDOW`].
    entries: History,
    arrived: History,
    /// For each monitor, each SSH connection's bytes, over the last
    /// [`RATE_WINDOW`], and the age of its last heartbeat.
    connections: Vec<Vec<Seen>>,
    /// Lines for standard error.
    said: Vec<String>,
    /// The files being transferred, those first reported first.
    transfers: Vec<Transferring>,
    /// For a dry run: how many entries it listed, the latest of them, where
    /// the listing goes, if anywhere, and what holds it back.
    listed: u64,
    latest: VecDeque<String>,
    listing: Option<io::BufWriter<io::Stdout>>,
    busy: String,
    /// How full each side's queues were over its latest second, by side.
    levels: Vec<(String, Vec<QueueMetrics>)>,
    /// The bytes and the files whose content arrived in each whole second
    /// since the start, and the counts at the last of them.
    seconds: Vec<(u64, u64)>,
    marked: (u64, u64),
    /// The history shows the whole transfer, rather than its last
    /// [`RECENT`] seconds.
    whole: bool,
}

impl State {
    fn new(monitors: usize, started: Instant) -> Self {
        Self {
            started,
            counts: Counts::default(),
            queues: BTreeMap::new(),
            bytes: VecDeque::new(),
            entries: VecDeque::new(),
            arrived: VecDeque::new(),
            connections: vec![Vec::new(); monitors],
            said: Vec::new(),
            transfers: Vec::new(),
            listed: 0,
            latest: VecDeque::new(),
            listing: None,
            busy: String::new(),
            levels: Vec::new(),
            seconds: Vec::new(),
            marked: (0, 0),
            whole: false,
        }
    }

    fn take(&mut self, report: Report, now: Instant) -> io::Result<()> {
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
            Report::Listed(lines) => {
                self.listed += lines.len() as u64;
                for line in &lines {
                    if let Some(listing) = &mut self.listing {
                        listing.write_all(line)?;
                        listing.write_all(b"\n")?;
                    }
                }
                for line in lines.iter().rev().take(LATEST).rev() {
                    if self.latest.len() == LATEST {
                        self.latest.pop_front();
                    }
                    self.latest
                        .push_back(String::from_utf8_lossy(line).into_owned());
                }
            }
            Report::Busy(now) => self.busy = now,
            Report::Levels { side, queues } => {
                match self.levels.iter_mut().find(|(seen, _)| *seen == side) {
                    Some((_, levels)) => *levels = queues,
                    None => {
                        self.levels.push((side, queues));
                        // The Source, the client between, then the Destination.
                        self.levels.sort_by_key(|(side, _)| side_order(side));
                    }
                }
            }
            // The relay keeps these.
            Report::Profile(_) | Report::Snapshot(_) | Report::Metrics(_) => {}
        }
        Ok(())
    }

    /// Takes the bytes so far, and each SSH connection's.
    fn sample(&mut self, view: &View, now: Instant) {
        push(&mut self.bytes, now, self.counts.received);
        push(&mut self.entries, now, self.counts.entries);
        push(&mut self.arrived, now, self.counts.arrived);
        // Each whole second gone by gets what came in it; any skipped, none.
        let second = now.saturating_duration_since(self.started).as_secs() as usize;
        while self.seconds.len() < second {
            let counts = (self.counts.received, self.counts.arrived);
            self.seconds.push((
                counts.0.saturating_sub(self.marked.0),
                counts.1.saturating_sub(self.marked.1),
            ));
            self.marked = counts;
        }
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

/// Where a side goes among the queues: the Source, the client, then the
/// Destination.
fn side_order(side: &str) -> u8 {
    if side.starts_with("the Source") {
        0
    } else if side == "the client" {
        1
    } else {
        2
    }
}

/// How full a queue was, or several taken together, such as the paths of a
/// connection: what waited, its room, and the most that did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Level {
    now: u64,
    room: u64,
    peak: u64,
    /// How many queues are taken together.
    queues: u64,
}

/// The queues of the side whose name starts with `side` that `wanted`
/// picks, taken together; none if it has none.
fn level(
    levels: &[(String, Vec<QueueMetrics>)],
    side: &str,
    wanted: impl Fn(&str) -> bool,
) -> Option<Level> {
    let (_, queues) = levels.iter().find(|(name, _)| name.starts_with(side))?;
    let mut level = Level::default();
    for queue in queues.iter().filter(|queue| wanted(&queue.name)) {
        level.now += queue.level;
        level.room += queue.capacity;
        level.peak += queue.peak;
        level.queues += 1;
    }
    (level.queues > 0).then_some(level)
}

/// A queue, or several, as `name now/room ↑peak`, the peak only if more,
/// the room only if bounded; sizes in bytes for `bytes`.
fn short_level(name: &str, level: Level, bytes: bool) -> String {
    let amount = |value: u64| if bytes { size(value) } else { count(value) };
    let mut shown = format!("{name} {}", amount(level.now));
    if level.room > 0 {
        shown.push('/');
        shown.push_str(&amount(level.room));
    }
    if level.peak > level.now {
        shown.push_str(&format!(" ↑{}", amount(level.peak)));
    }
    shown
}

/// The lines of the queues, as the sides' latest seconds said: those the
/// data goes through, in the order it goes, in a table; then those of the
/// needs and results going back, the messages, and the manifest, on a line
/// each, those used only.
fn queues(levels: &[(String, Vec<QueueMetrics>)], dim: Style) -> Vec<Line<'static>> {
    if levels.is_empty() {
        return Vec::new();
    }
    let paths = |prefix: &'static str| move |name: &str| name.starts_with(prefix);
    let named = |wanted: &'static str| move |name: &str| name == wanted;
    // The way the data goes: each row's side, name, and queues.
    let rows: [(&str, &str, Option<Level>); 9] = [
        ("Source", "read", level(levels, "the Source", named("read"))),
        (
            "",
            "compress",
            level(levels, "the Source", named("compress")),
        ),
        ("", "data", level(levels, "the Source", named("data"))),
        ("", "send", level(levels, "the Source", paths("send path "))),
        (
            "client",
            "from Source",
            level(levels, "the client", named("Source receive")),
        ),
        (
            "",
            "to Destination",
            level(levels, "the client", paths("Destination send path ")),
        ),
        (
            "Destination",
            "receive",
            level(levels, "the Destination", named("receive")),
        ),
        (
            "",
            "decompress",
            level(levels, "the Destination", named("decompress")),
        ),
        (
            "",
            "write",
            level(levels, "the Destination", named("write")),
        ),
    ];
    let mut lines = vec![
        Line::raw(""),
        Line::styled(
            format!(" {:<35}{:>7}{:>7}{:>7}", "queues", "now", "room", "peak"),
            dim,
        ),
    ];
    let mut side = "";
    for (row_side, name, level) in rows {
        if !row_side.is_empty() {
            side = row_side;
        }
        let Some(level) = level else {
            continue;
        };
        let name = if level.queues > 1 {
            format!("{name} ×{}", level.queues)
        } else {
            name.to_owned()
        };
        let full = if level.room > 0 {
            100.0 * level.now as f64 / level.room as f64
        } else {
            0.0
        };
        lines.push(Line::from(vec![
            Span::styled(format!("   {side:<12} "), dim),
            Span::raw(format!(
                "{name:<20}{:>7}{:>7}{:>7}  ",
                count(level.now),
                count(level.room),
                count(level.peak)
            )),
            Span::raw(cells(full)),
        ]));
        side = "";
    }
    // The other queues, a line for each kind, those used only.
    let mut line = |label: &str, items: Vec<Option<String>>| {
        let items: Vec<String> = items.into_iter().flatten().collect();
        if !items.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(format!("   {label:<12} "), dim),
                Span::raw(items.join(" · ")),
            ]));
        }
    };
    let used = |name: &str, level: Option<Level>, bytes: bool| {
        level
            .filter(|level| level.peak > 0)
            .map(|level| short_level(name, level, bytes))
    };
    line(
        "back",
        vec![
            used(
                "Destination send",
                level(levels, "the Destination", paths("send path ")),
                false,
            ),
            used(
                "client from Destination",
                level(levels, "the client", named("Destination receive")),
                false,
            ),
            used(
                "client to Source",
                level(levels, "the client", paths("Source send path ")),
                false,
            ),
            used(
                "Source receive",
                level(levels, "the Source", named("receive")),
                false,
            ),
        ],
    );
    line(
        "messages",
        vec![
            used(
                "Source",
                level(levels, "the Source", named("messages")),
                false,
            ),
            used(
                "Destination",
                level(levels, "the Destination", named("messages")),
                false,
            ),
        ],
    );
    line(
        "manifest",
        vec![
            used(
                "Source",
                level(levels, "the Source", named("manifest")),
                true,
            ),
            used(
                "Destination",
                level(levels, "the Destination", named("manifest")),
                true,
            ),
        ],
    );
    lines
}

/// Four cells, as full as a queue.
fn cells(level: f64) -> String {
    let full = ((level / 25.0).round() as usize).min(4);
    format!("{}{}", "▮".repeat(full), "▯".repeat(4 - full))
}

/// The last `span` seconds spread over `columns`: each the average of the
/// seconds it covers, or, where there are more columns than seconds, the
/// second it falls in. Seconds before the start count as none.
fn spread(seconds: &[u64], span: usize, columns: usize) -> Vec<f64> {
    let first = seconds.len() as isize - span as isize;
    (0..columns)
        .map(|column| {
            let from = column * span / columns;
            let to = ((column + 1) * span / columns).max(from + 1);
            let sum: u64 = (from..to)
                .filter_map(|offset| usize::try_from(first + offset as isize).ok())
                .filter_map(|index| seconds.get(index))
                .sum();
            sum as f64 / (to - from) as f64
        })
        .collect()
}

/// Braille dots on `rows` lines, a column of dots for each value, filled
/// from the axis as far as the value goes toward `top`: up from an axis
/// below them, or, `down`, down from one above.
fn dots(values: &[f64], top: f64, rows: usize, down: bool) -> Vec<String> {
    // The bits of a braille character's dots, by row, then column.
    const BITS: [[u32; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
    let high = rows * 4;
    let mut grid = vec![vec![0; values.len().div_ceil(2)]; rows];
    for (column, value) in values.iter().enumerate() {
        let mut filled = if top > 0.0 {
            (value / top * high as f64).round() as usize
        } else {
            0
        };
        // Anything at all shows.
        if *value > 0.0 {
            filled = filled.max(1);
        }
        for dot in 0..filled.min(high) {
            let y = if down { dot } else { high - 1 - dot };
            grid[y / 4][column / 2] |= BITS[y % 4][column % 2];
        }
    }
    grid.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|bits| char::from_u32(0x2800 + bits).unwrap_or(' '))
                .collect()
        })
        .collect()
}

/// The lines of the history, on a screen `width` wide: the bytes a second
/// above an axis, and the files a second below it, over the last [`RECENT`]
/// seconds or the whole transfer, each scaled to the most it shows, and the
/// times under them. None on a screen too narrow.
fn history(state: &State, width: usize, dim: Style) -> Vec<Line<'static>> {
    let room = width.saturating_sub(3 + SCALE);
    if room < 20 {
        return Vec::new();
    }
    let span = if state.whole {
        state.seconds.len().max(1)
    } else {
        RECENT
    };
    // A braille character has two columns of dots.
    let spread = |pick: fn(&(u64, u64)) -> u64| {
        let seconds: Vec<u64> = state.seconds.iter().map(pick).collect();
        spread(&seconds, span, room * 2)
    };
    let (bytes, files) = (spread(|second| second.0), spread(|second| second.1));
    let most = |values: &[f64]| values.iter().copied().fold(0.0, f64::max);
    let (most_bytes, most_files) = (most(&bytes), most(&files));

    let heading = if state.whole {
        "the whole transfer   t: the last 2 minutes"
    } else {
        "the last 2 minutes   t: the whole transfer"
    };
    let mut lines = vec![
        Line::raw(""),
        Line::styled(
            format!(" history{heading:>0$}", (room + 2).saturating_sub(7)),
            dim,
        ),
    ];
    let row = |dots: String, scale: String| {
        Line::from(vec![
            Span::raw(format!("   {dots}")),
            Span::styled(format!("  {scale}"), dim),
        ])
    };
    for (index, dots) in dots(&bytes, most_bytes, ABOVE, false)
        .into_iter()
        .enumerate()
    {
        let scale = if index == 0 {
            format!("{}/s", size(most_bytes.round() as u64))
        } else {
            String::new()
        };
        lines.push(row(dots, scale));
    }
    lines.push(Line::styled(format!("   {}", "─".repeat(room)), dim));
    for (index, dots) in dots(&files, most_files, BELOW, true)
        .into_iter()
        .enumerate()
    {
        let scale = if index == BELOW - 1 {
            format!("{} files/s", count(most_files.round() as u64))
        } else {
            String::new()
        };
        lines.push(row(dots, scale));
    }
    // The times at each quarter, from the start or back from now, as fit
    // before now.
    let mut times = vec![' '; room];
    for quarter in 0..4 {
        let label = if state.whole {
            clock(Duration::from_secs((span * quarter / 4) as u64))
        } else {
            format!(
                "-{}",
                clock(Duration::from_secs((span * (4 - quarter) / 4) as u64))
            )
        };
        let at = room * quarter / 4;
        if at + label.len() < room - 4 {
            times[at..at + label.len()].copy_from_slice(&label.chars().collect::<Vec<_>>());
        }
    }
    times[room - 3..].copy_from_slice(&['n', 'o', 'w']);
    lines.push(Line::styled(
        format!("   {}", times.into_iter().collect::<String>()),
        dim,
    ));
    lines
}

fn draw(frame: &mut ratatui::Frame, state: &State, view: &View, now: Instant) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let counts = &state.counts;
    let speed = rate(&state.bytes);

    let mut lines: Vec<Line> = Vec::new();
    // Where the history goes, if anywhere: after the counts.
    let mut graph = None;
    // The counts.
    // Each with how many a second, as files' content arrives, one by one,
    // where the files done go up a batch at a time.
    let per_second = |history: &History| {
        if history.len() < 2 {
            String::new()
        } else {
            format!("{}/s", count(rate(history)))
        }
    };
    lines.push(Line::from(vec![
        Span::styled(" compared ", dim),
        Span::raw(format!("{:>12} entries", count(counts.entries))),
        Span::styled(format!(" {:>10}", per_second(&state.entries)), dim),
        Span::styled("      to send ", dim),
        Span::raw(format!("{:>10} files", count(counts.needed))),
    ]));
    if view.dry_run {
        // What a dry run lists, and the latest of it, rather than what it
        // sends.
        lines.push(Line::from(vec![
            Span::styled(" listed   ", dim),
            Span::raw(format!("{:>12} entries", count(state.listed))),
            Span::styled(format!(" {:>10}", ""), dim),
            Span::styled("      to send ", dim),
            Span::raw(format!("{:>10}", size(counts.to_send))),
        ]));
        lines.push(Line::raw(""));
        lines.push(Line::styled(" latest listed", dim));
        if state.latest.is_empty() {
            lines.push(Line::styled("   none", dim));
        }
        let room = (frame.area().width as usize).saturating_sub(4);
        for listed in &state.latest {
            lines.push(Line::raw(format!("   {}", shortened(listed, room))));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled(" sent     ", dim),
            Span::raw(format!("{:>12} files  ", count(counts.files))),
            Span::styled(format!(" {:>10}", per_second(&state.arrived)), dim),
            Span::styled("      failed  ", dim),
            Span::raw(format!("{:>10}", count(counts.failed + counts.problems))),
        ]));
        graph = Some(lines.len());
        // The files being transferred.
        lines.push(Line::raw(""));
        lines.extend(transfers(
            &state.transfers,
            frame.area().width as usize,
            dim,
        ));
    }
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
    // The pipeline, which carries nothing in a dry run.
    let averages = state.averages();
    if !view.dry_run {
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
    }
    // How full each side's queues were.
    lines.extend(queues(&state.levels, dim));
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
    // The history goes first when the screen is too short for it all.
    if let Some(at) = graph {
        let history = history(state, frame.area().width as usize, dim);
        if lines.len() + history.len() <= body.height as usize {
            lines.splice(at..at, history);
        }
    }
    let elapsed = clock(now.duration_since(state.started));
    let heading = format!(
        " ferry: {} → {}{}",
        view.from,
        view.to,
        if view.dry_run { ", a dry run" } else { "" }
    );
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
    if view.dry_run {
        // Nothing is sent, so no bar: what holds the run back instead.
        let busy = if state.busy.is_empty() {
            "—"
        } else {
            &state.busy
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" busiest  ", dim),
                Span::raw(busy.to_owned()),
            ])),
            bar,
        );
        frame.render_widget(Paragraph::new(lines), body);
        return;
    }
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
        state
            .take(
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
            )
            .unwrap();
        state
            .take(
                Report::Queues(vec![(Stage::Network, 100), (Stage::Read, 100)]),
                now,
            )
            .unwrap();
        state
            .take(Report::Queues(vec![(Stage::Write, 0)]), now)
            .unwrap();
        state
            .take(Report::Said("cannot read sub/locked.txt".into()), now)
            .unwrap();
        let image = |done: u64| Transfer {
            id: 7,
            path: "photos/2024/08/IMG_4211.jpg".into(),
            size: 100 << 20,
            done,
        };
        state
            .take(
                Report::Transferring(vec![image(20 << 20)]),
                now - Duration::from_secs(1),
            )
            .unwrap();
        state
            .take(Report::Transferring(vec![image(45 << 20)]), now)
            .unwrap();
        let view = View {
            from: "./data".into(),
            to: "ntu:/backup".into(),
            monitors: Vec::new(),
            dry_run: false,
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
    fn a_dry_run_shows_what_it_lists_and_what_holds_it_back() {
        let started = Instant::now();
        let mut state = State::new(0, started);
        let now = started + Duration::from_secs(5);
        state
            .take(
                Report::Counts(Counts {
                    entries: 312_450,
                    needed: 12,
                    to_send: 3 << 20,
                    ..Counts::default()
                }),
                now,
            )
            .unwrap();
        let listed: Vec<Vec<u8>> = (1..=7).map(|n| format!("dir/file{n}").into()).collect();
        state.take(Report::Listed(listed), now).unwrap();
        state
            .take(
                Report::Busy("Source on fatcat: list directories".into()),
                now,
            )
            .unwrap();
        let view = View {
            from: "fatcat:/data".into(),
            to: "./data".into(),
            monitors: Vec::new(),
            dry_run: true,
        };
        let text = drawn(&state, &view, now, 90, 24);
        for expected in [
            "ferry: fatcat:/data → ./data, a dry run",
            "busiest  Source on fatcat: list directories",
            "312,450 entries",
            "12 files",
            "listed              7 entries",
            "3.0 MiB",
            "latest listed",
            "   dir/file3",
            "   dir/file7",
        ] {
            assert!(text.contains(expected), "no {expected:?} in:\n{text}");
        }
        // Only the latest are shown, and nothing of sending.
        assert_eq!(text.matches("dir/file").count(), LATEST, "{text}");
        for unexpected in ["pipeline", "transferring", "ETA"] {
            assert!(!text.contains(unexpected), "{unexpected:?} in:\n{text}");
        }
    }

    #[test]
    fn the_queues_are_shown_the_way_the_data_goes() {
        let queue = |name: &str, level, peak, capacity, bytes| QueueMetrics {
            name: name.to_owned(),
            level,
            peak,
            capacity,
            bytes,
        };
        let levels = |side: &str, queues| Report::Levels {
            side: side.to_owned(),
            queues,
        };
        let started = Instant::now();
        let mut state = State::new(0, started);
        state
            .take(
                levels(
                    "the Destination on nano4",
                    vec![
                        queue("receive", 0, 2, 256, false),
                        queue("send path 1", 0, 0, 8, false),
                        queue("send path 2", 0, 1, 8, false),
                        queue("write", 0, 0, 32, false),
                    ],
                ),
                started,
            )
            .unwrap();
        state
            .take(
                levels(
                    "the Source on this machine",
                    vec![
                        queue("send path 1", 8, 9, 8, false),
                        queue("send path 2", 2, 8, 8, false),
                        queue("data", 256, 258, 256, false),
                        queue("manifest", 1 << 20, 1 << 20, 8 << 20, true),
                        queue("messages", 0, 2, 0, false),
                        queue("receive", 0, 0, 256, false),
                    ],
                ),
                started,
            )
            .unwrap();
        state
            .take(
                levels(
                    "the client",
                    vec![
                        queue("Source receive", 256, 256, 256, false),
                        queue("Destination send path 1", 8, 8, 8, false),
                        queue("Destination send path 2", 8, 8, 8, false),
                    ],
                ),
                started,
            )
            .unwrap();
        let lines: Vec<String> = queues(&state.levels, Style::default())
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect();
        assert_eq!(
            lines[1..],
            [
                " queues                                 now   room   peak",
                "   Source       data                    256    256    258  ▮▮▮▮",
                "                send ×2                  10     16     17  ▮▮▮▯",
                "   client       from Source             256    256    256  ▮▮▮▮",
                "                to Destination ×2        16     16     16  ▮▮▮▮",
                "   Destination  receive                   0    256      2  ▯▯▯▯",
                "                write                     0     32      0  ▯▯▯▯",
                "   back         Destination send 0/16 ↑1",
                "   messages     Source 0 ↑2",
                "   manifest     Source 1.0 MiB/8.0 MiB",
            ]
        );
    }

    #[test]
    fn entries_and_files_are_shown_with_how_many_a_second() {
        let started = Instant::now();
        let mut state = State::new(0, started);
        let view = View {
            from: "a".into(),
            to: "b".into(),
            monitors: Vec::new(),
            dry_run: false,
        };
        let more = |entries, arrived| {
            Report::Counts(Counts {
                entries,
                arrived,
                ..Counts::default()
            })
        };
        state.take(more(1_000, 10), started).unwrap();
        state.sample(&view, started);
        // Two seconds later, 2,000 more entries compared, and 20 more files'
        // content arrived.
        let later = started + Duration::from_secs(2);
        state.take(more(2_000, 20), later).unwrap();
        state.sample(&view, later);
        let text = drawn(&state, &view, later, 90, 24);
        for expected in ["3,000 entries    1,000/s", "0 files         10/s"] {
            assert!(text.contains(expected), "no {expected:?} in:\n{text}");
        }
    }

    #[test]
    fn the_history_shows_each_second_over_the_last_minutes_or_the_whole_transfer() {
        let started = Instant::now();
        let mut state = State::new(0, started);
        let view = View {
            from: "a".into(),
            to: "b".into(),
            monitors: Vec::new(),
            dry_run: false,
        };
        // Three minutes: none at first, then 1 MiB and 10 files a second.
        for second in 1..=180 {
            if second > 60 {
                state
                    .take(
                        Report::Counts(Counts {
                            received: 1 << 20,
                            arrived: 10,
                            ..Counts::default()
                        }),
                        started,
                    )
                    .unwrap();
            }
            state.sample(&view, started + Duration::from_secs(second));
        }
        assert_eq!(state.seconds.len(), 180);
        assert_eq!(state.seconds[59], (0, 0));
        assert_eq!(state.seconds[60], (1 << 20, 10));

        let now = started + Duration::from_secs(180);
        let text = drawn(&state, &view, now, 80, 40);
        for expected in [
            "history",
            "the last 2 minutes   t: the whole transfer",
            "1.0 MiB/s",
            "10 files/s",
            "-02:00",
            "now",
        ] {
            assert!(text.contains(expected), "no {expected:?} in:\n{text}");
        }
        // The last 2 minutes all came at the same pace: full throughout.
        assert!(!text.contains('⠀'), "{text}");

        state.whole = true;
        let text = drawn(&state, &view, now, 80, 40);
        for expected in [
            "the whole transfer   t: the last 2 minutes",
            "00:00",
            "01:30",
        ] {
            assert!(text.contains(expected), "no {expected:?} in:\n{text}");
        }
        // The first minute had none.
        assert!(text.contains('⠀'), "{text}");

        // A screen too short has no history.
        let text = drawn(&state, &view, now, 80, 16);
        assert!(!text.contains("history"), "{text}");
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
        state
            .take(Report::Transferring((1..=5).map(file).collect()), started)
            .unwrap();
        // The first is done; four more start, after the others.
        state
            .take(Report::Transferring((2..=9).map(file).collect()), started)
            .unwrap();
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
        state
            .take(Report::Transferring(Vec::new()), started)
            .unwrap();
        let lines = transfers(&state.transfers, 80, Style::default());
        assert_eq!(lines[1].to_string(), "   none");
    }
}
