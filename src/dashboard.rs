//! The dashboard: a full screen, on a terminal, that shows the transfer as
//! it goes, for you to follow it and to find what holds it back; see the
//! [design](../docs/design/dashboard.md).
//!
//! It shows the reports the peers send, as the progress line does, and what
//! the client's own transport knows of each SSH connection. Lines for
//! standard error are kept, and returned, to be written once the terminal is
//! back as it was. Ctrl-C cancels the transfer, as it does without it.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, IsTerminal, Stderr};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::profile::QueueMetrics;
use crate::progress::{count, saved, size};
use crate::report::{Counts, Report, Stage, Transfer};
use crate::transport::Monitor;
use crate::transport::session::{KEEPALIVE, SILENCE};

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

/// What the dashboard is about: the two locations, and the SSH connection
/// to each host.
pub struct View {
    pub from: String,
    pub to: String,
    pub monitors: Vec<Monitor>,
    /// A dry run: what it lists, and how far its comparison has got, rather
    /// than what it sends.
    pub dry_run: bool,
    /// A dry run's listing, where it goes to standard output, as `--null`
    /// writes it.
    pub null: bool,
}

/// Whether to color, as `NO_COLOR` says: not when it is set to anything.
static COLORED: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()));

/// Text in `color`, in the terminal's own shade of it; or as it is, without
/// color.
fn colored(color: Color) -> Style {
    if *COLORED {
        Style::default().fg(color)
    } else {
        Style::default()
    }
}

/// The terminal is taken.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Held while a frame is drawn, and while the terminal is put back.
static DRAWING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What a key asks of the dashboard.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Ctrl-C: cancel the transfer.
    Cancel,
    /// `Ctrl-\`: say where every thread is.
    Snapshot,
    /// `t`: the history over the whole transfer, or its last minutes.
    History,
}

/// What `key` asks, if anything. `Ctrl-\` comes as `Ctrl-4` from most
/// terminals, and as itself from those with the kitty keyboard protocol.
fn action(key: &KeyEvent) -> Option<Action> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('c') if control => Some(Action::Cancel),
        KeyCode::Char('4' | '\\') if control => Some(Action::Snapshot),
        KeyCode::Char('t') if !control => Some(Action::History),
        _ => None,
    }
}

/// Puts the terminal back as it was, if the dashboard has it.
fn restore() {
    // After a frame being drawn, which it would otherwise interleave with;
    // tried for a moment, and gone without, should this run in a panic of
    // the thread that draws.
    let _drawing = (0..100).find_map(|_| {
        DRAWING.try_lock().ok().or_else(|| {
            std::thread::sleep(Duration::from_millis(1));
            None
        })
    });
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
) -> Result<(Counts, Vec<String>), crate::progress::Failed> {
    let mut state = State::new(view.monitors.len(), started);
    match show_in(&mut state, &reports, view) {
        Ok(()) => Ok((state.counts, state.said)),
        Err(error) => Err(crate::progress::Failed::counting(
            state.counts,
            error,
            &reports,
        )),
    }
}

fn show_in(state: &mut State, reports: &Receiver<Report>, view: &View) -> io::Result<()> {
    state.null = view.null;
    terminal::enable_raw_mode()?;
    ACTIVE.store(true, Ordering::SeqCst);
    crate::status::displaying(true);
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
    .and_then(|mut screen| run(&mut screen, state, reports, view));
    crate::status::displaying(false);
    restore();
    crate::interrupt::on_exit(None);
    result
}

fn run(
    screen: &mut Terminal<CrosstermBackend<Stderr>>,
    state: &mut State,
    reports: &Receiver<Report>,
    view: &View,
) -> io::Result<()> {
    // A dry run's listing goes to standard output as it comes, unless that is
    // a terminal, which the dashboard has: there it is only counted.
    if !io::stdout().is_terminal() {
        state.listing = Some(io::BufWriter::new(io::stdout()));
    }
    let mut drawn = Instant::now() - EVERY;
    // The terminal given up, lent for something to be asked on it.
    let mut given_up = false;
    loop {
        match reports.recv_timeout(Duration::from_millis(20)) {
            Ok(report) => state.take(report, Instant::now())?,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if crate::status::lent() != given_up {
            if given_up {
                terminal::enable_raw_mode()?;
                execute!(
                    io::stderr(),
                    terminal::EnterAlternateScreen,
                    terminal::Clear(terminal::ClearType::All),
                    cursor::Hide
                )?;
                // Drawn whole again.
                screen.clear()?;
                drawn = Instant::now() - EVERY;
            } else {
                let _drawing = DRAWING.lock().expect("not poisoned");
                terminal::disable_raw_mode()?;
                execute!(io::stderr(), terminal::LeaveAlternateScreen, cursor::Show)?;
            }
            given_up = !given_up;
            crate::status::given_up(given_up);
        }
        // Lent, it is neither read, as what is asked is typed there, nor
        // drawn on; the reports are still taken.
        if given_up {
            continue;
        }
        if event::poll(Duration::ZERO)?
            && let Event::Key(key) = event::read()?
        {
            match action(&key) {
                Some(Action::Cancel) => {
                    // Raw, the terminal sends no signal for Ctrl-C: the
                    // process sends itself one, which cancels the transfer as
                    // usual.
                    restore();
                    // SAFETY: `kill` with this process's own pid and a signal.
                    unsafe {
                        libc::kill(libc::getpid(), libc::SIGINT);
                    }
                }
                // Nor for `Ctrl-\`: asked for as its SIGQUIT asks, and shown
                // among the warnings, then on standard error at the end.
                Some(Action::Snapshot) => crate::interrupt::ask_for_snapshot(),
                Some(Action::History) => {
                    // The history over the whole transfer, or its last
                    // minutes.
                    state.whole = !state.whole;
                    drawn = Instant::now() - EVERY;
                }
                None => {}
            }
        }
        if drawn.elapsed() >= EVERY {
            drawn = Instant::now();
            state.sample(view, drawn);
            // Not once the terminal is put back, as on a signal: a frame
            // drawn then would be left on the screen.
            let _drawing = DRAWING.lock().expect("not poisoned");
            if ACTIVE.load(Ordering::SeqCst) {
                screen.draw(|frame| draw(frame, state, view, drawn))?;
            }
        }
    }
    if let Some(listing) = &mut state.listing {
        crate::progress::flush_listing(listing)?;
    } else if state.listed > 0 {
        state.said.push(format!(
            "{} listed, not written to the terminal: redirect standard output to see \
             them, as with `> file` or `| less`",
            crate::progress::counted(state.listed, "entry", "entries")
        ));
    }
    Ok(())
}

/// Samples of a count, over a window.
type History = VecDeque<(Instant, u64)>;

/// An SSH connection, as seen: its bytes, and how long ago anything came on
/// it.
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
    /// For each monitor, its SSH connection's bytes, over the last
    /// [`RATE_WINDOW`], and how long ago anything came on it.
    connections: Vec<Seen>,
    /// Lines for standard error.
    said: Vec<String>,
    /// The files being transferred, those first reported first.
    transfers: Vec<Transferring>,
    /// For a dry run: how many entries it listed, the latest of them, where
    /// the listing goes, if anywhere, and what holds it back.
    listed: u64,
    latest: VecDeque<String>,
    listing: Option<io::BufWriter<io::Stdout>>,
    /// Whether the listing is written as `--null` writes it.
    null: bool,
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
            connections: vec![(VecDeque::new(), None); monitors],
            said: Vec::new(),
            transfers: Vec::new(),
            listed: 0,
            latest: VecDeque::new(),
            listing: None,
            null: false,
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
            Report::Said(said) => self.said.push(crate::printable::line(&said)),
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
                if let Some(listing) = &mut self.listing {
                    crate::progress::list(listing, &lines, self.null)?;
                }
                for line in lines.iter().rev().take(LATEST).rev() {
                    if self.latest.len() == LATEST {
                        self.latest.pop_front();
                    }
                    self.latest.push_back(crate::progress::shown_listed(line));
                }
            }
            Report::Busy(now) => self.busy = now,
            Report::Levels { side, queues } => {
                match self.levels.iter_mut().find(|(seen, _)| *seen == side) {
                    Some((_, levels)) => *levels = queues,
                    None => {
                        self.levels.push((side, queues));
                        // The Source, then the Destination.
                        self.levels.sort_by_key(|(side, _)| side_order(side));
                    }
                }
            }
            // The client keeps these as it follows the servers.
            Report::Profile(_)
            | Report::Snapshot(_)
            | Report::Metrics(_)
            | Report::Failed(_)
            | Report::Streams(_)
            | Report::Broke(_)
            | Report::Done => {}
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
        for (monitor, (history, age)) in view.monitors.iter().zip(&mut self.connections) {
            let (bytes, heard) = monitor.connection();
            push(history, now, bytes);
            *age = heard;
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
/// heading, then each file, the first `rows` of them, with how much of it
/// has come, its size, its rate, and its time left, and a count of the rest.
fn transfers(
    transfers: &[Transferring],
    rows: usize,
    width: usize,
    dim: Style,
) -> Vec<Line<'static>> {
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
    for shown in transfers.iter().take(rows) {
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
    if transfers.len() > rows {
        lines.push(Line::styled(
            format!("   … and {} more", count((transfers.len() - rows) as u64)),
            dim,
        ));
    }
    lines
}

/// Where a side goes among the queues: the Source, then the Destination.
fn side_order(side: &str) -> u8 {
    u8::from(!side.starts_with("the Source"))
}

/// How full a queue was, or several taken together, such as a side's
/// streams: what waited, its room, and the most that did, which only says
/// whether it was used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Level {
    now: u64,
    room: u64,
    peak: u64,
    /// How many queues are taken together.
    queues: u64,
    /// Whether they count bytes, rather than things.
    bytes: bool,
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
        level.bytes = queue.bytes;
    }
    (level.queues > 0).then_some(level)
}

/// A queue, or several, as `name now/room`, the room only if bounded; sizes
/// in bytes for `bytes`.
fn short_level(name: &str, level: Level, bytes: bool) -> String {
    let amount = |value: u64| if bytes { size(value) } else { count(value) };
    let mut shown = format!("{name} {}", amount(level.now));
    if level.room > 0 {
        shown.push('/');
        shown.push_str(&amount(level.room));
    }
    shown
}

/// The lines of the queues, as the sides' latest seconds said: those the
/// data goes through, in the order it goes, in a table; then those of the
/// needs and results going back, the messages, and the manifest, on a line
/// each, those used only. Every line is there from the start, `—` for
/// what has not come, so that it is always as tall.
fn queues(levels: &[(String, Vec<QueueMetrics>)], dim: Style) -> Vec<Line<'static>> {
    let paths = |prefix: &'static str| move |name: &str| name.starts_with(prefix);
    let named = |wanted: &'static str| move |name: &str| name == wanted;
    // The way the data goes: each row's side, name, and queues.
    let rows: [(&str, &str, Option<Level>); 6] = [
        ("Source", "read", level(levels, "the Source", named("read"))),
        (
            "",
            "compress",
            level(levels, "the Source", named("compress")),
        ),
        ("", "data", level(levels, "the Source", named("data"))),
        (
            "",
            "send",
            level(levels, "the Source", paths("send stream ")),
        ),
        (
            "Destination",
            "decompress",
            level(levels, "the Destination", named("decompress")),
        ),
        (
            "",
            "write",
            level(levels, "the Destination", named("write")),
        ),
    ];
    let mut lines = vec![Line::styled(
        format!(" {:<35}{:>10}{:>10}", "queues", "now", "room"),
        dim,
    )];
    let mut side = "";
    for (row_side, name, level) in rows {
        if !row_side.is_empty() {
            side = row_side;
        }
        let Some(level) = level else {
            lines.push(Line::from(vec![
                Span::styled(format!("   {side:<12} "), dim),
                Span::raw(format!("{name:<20}{:>10}{:>10}", "—", "—")),
            ]));
            side = "";
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
        let amount = |value: u64| {
            if level.bytes {
                size(value)
            } else {
                count(value)
            }
        };
        let mut row = vec![
            Span::styled(format!("   {side:<12} "), dim),
            Span::raw(format!(
                "{name:<20}{:>10}{:>10}  ",
                amount(level.now),
                amount(level.room)
            )),
        ];
        row.extend(cells(full, dim));
        lines.push(Line::from(row));
        side = "";
    }
    // The other queues, a line for each kind, those used only.
    let mut line = |label: &str, items: Vec<Option<String>>| {
        let items: Vec<String> = items.into_iter().flatten().collect();
        let shown = if items.is_empty() {
            "—".to_owned()
        } else {
            items.join(" · ")
        };
        lines.push(Line::from(vec![
            Span::styled(format!("   {label:<12} "), dim),
            Span::raw(shown),
        ]));
    };
    let used = |name: &str, level: Option<Level>, bytes: bool| {
        level
            .filter(|level| level.peak > 0)
            .map(|level| short_level(name, level, bytes))
    };
    line(
        "back",
        vec![used(
            "Destination send",
            level(levels, "the Destination", paths("send stream ")),
            true,
        )],
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

/// Four cells, as full as a queue, as a traffic light: green under half
/// full, yellow to [`FULL`], red beyond.
fn cells(level: f64, dim: Style) -> [Span<'static>; 2] {
    let full = ((level / 25.0).round() as usize).min(4);
    let color = if level < 50.0 {
        Color::Green
    } else if level < FULL {
        Color::Yellow
    } else {
        Color::Red
    };
    [
        Span::styled("▮".repeat(full), colored(color)),
        Span::styled("▯".repeat(4 - full), dim),
    ]
}

/// How long ago anything came on an SSH connection, as a traffic light: as it
/// is while something comes, as a keepalive does when nothing else does;
/// yellow once one is missed; red as the silence nears the one that takes
/// its streams for broken.
fn late(age: Duration) -> Style {
    if age > SILENCE * 2 / 3 {
        colored(Color::Red)
    } else if age > KEEPALIVE * 2 {
        colored(Color::Yellow)
    } else {
        Style::default()
    }
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
    let mut lines = vec![Line::styled(
        format!(" history{heading:>0$}", (room + 2).saturating_sub(7)),
        dim,
    )];
    // Each half, and its scale, in a color of its own.
    let row = |dots: String, scale: String, color: Color| {
        Line::from(vec![
            Span::styled(format!("   {dots}"), colored(color)),
            Span::styled(format!("  {scale}"), colored(color)),
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
        lines.push(row(dots, scale, Color::Cyan));
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
        lines.push(row(dots, scale, Color::Magenta));
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

/// Rows of the counts, and a blank row.
const COUNTS_ROWS: usize = 3;
/// Rows of the history: its heading, the dots above and below its axis, the
/// axis, the times, and a blank row.
const HISTORY_ROWS: usize = ABOVE + BELOW + 4;
/// Rows of the pipeline, the bottleneck, and a blank row.
const PIPELINE_ROWS: usize = 3;
/// Rows of the queues: a heading, the 6 the data goes through, the 3 lines
/// after, and a blank row.
const QUEUE_ROWS: usize = 11;
/// The fewest files being transferred shown, on a short screen.
const FEW_TRANSFERS: usize = 3;

/// How tall the sections are, those that a short screen may leave out.
struct Sizes {
    /// Those always shown: the counts, a dry run's latest, each host's
    /// connections, and the warnings.
    always: usize,
    /// The history, or 0 for none, as on a screen too narrow.
    history: usize,
    /// Whether the files being transferred are shown, as they are but in a
    /// dry run.
    transfers: bool,
    /// The pipeline, or 0 for none.
    pipeline: usize,
}

/// What a screen has room for, as its height alone says, so that a screen of
/// one size keeps one layout from start to end.
#[derive(Debug, PartialEq, Eq)]
struct Fit {
    history: bool,
    /// How many files being transferred.
    transfers: usize,
    pipeline: bool,
    queues: bool,
}

impl Fit {
    /// For a body `height` rows tall: everything, if it fits; else without
    /// the queues, then the pipeline, then with fewer files being
    /// transferred, then without the history. What is left loses its bottom
    /// rows: the warnings', then the connections'.
    fn for_height(height: usize, sizes: &Sizes) -> Self {
        let transfers = |rows: usize| if sizes.transfers { rows + 3 } else { 0 };
        let fewer = if sizes.transfers { FEW_TRANSFERS } else { 0 };
        let all = if sizes.transfers { TRANSFERS } else { 0 };
        let without_queues = sizes.always + sizes.history + transfers(TRANSFERS) + sizes.pipeline;
        let fit = |history, transfers, pipeline, queues| Fit {
            history,
            transfers,
            pipeline,
            queues,
        };
        let history = sizes.history > 0;
        let pipeline = sizes.pipeline > 0;
        if height >= without_queues + QUEUE_ROWS {
            fit(history, all, pipeline, true)
        } else if height >= without_queues {
            fit(history, all, pipeline, false)
        } else if height >= without_queues - sizes.pipeline {
            fit(history, all, false, false)
        } else if height >= sizes.always + sizes.history + transfers(FEW_TRANSFERS) {
            fit(history, fewer, false, false)
        } else {
            fit(false, fewer, false, false)
        }
    }
}

fn draw(frame: &mut ratatui::Frame, state: &State, view: &View, now: Instant) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let counts = &state.counts;
    let speed = rate(&state.bytes);

    let [title, bar, _, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(frame.area());
    let width = frame.area().width as usize;
    // Which sections there is room for, as the screen's size alone says.
    let graph = if view.dry_run {
        Vec::new()
    } else {
        history(state, width, dim)
    };
    // A row for each host's SSH connection, and a blank row.
    let connections = 2 * state.connections.len();
    let fit = Fit::for_height(
        body.height as usize,
        &Sizes {
            always: COUNTS_ROWS
                + if view.dry_run { LATEST + 2 } else { 0 }
                + connections
                + WARNINGS
                + 1,
            history: if graph.is_empty() { 0 } else { HISTORY_ROWS },
            transfers: !view.dry_run,
            pipeline: if view.dry_run { 0 } else { PIPELINE_ROWS },
        },
    );

    // Each section as tall as the screen makes it, never as what it holds:
    // what has less leaves its rows blank.
    let mut lines: Vec<Line> = Vec::new();
    let section = |lines: &mut Vec<Line>, content: Vec<Line<'static>>, rows: usize| {
        let start = lines.len();
        lines.extend(content);
        lines.truncate(start + rows);
        lines.resize(start + rows, Line::raw(""));
    };
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
    let mut shown = vec![Line::from(vec![
        Span::styled(" compared ", dim),
        Span::raw(format!("{:>12} entries", count(counts.entries))),
        Span::styled(format!(" {:>10}", per_second(&state.entries)), dim),
        Span::styled("      to send ", dim),
        Span::raw(format!("{:>10} files", count(counts.needed))),
    ])];
    if view.dry_run {
        // What a dry run lists, and the latest of it, rather than what it
        // sends.
        shown.push(Line::from(vec![
            Span::styled(" listed   ", dim),
            Span::raw(format!("{:>12} entries", count(state.listed))),
            Span::styled(format!(" {:>10}", ""), dim),
            Span::styled("      to send ", dim),
            Span::raw(format!("{:>10}", size(counts.to_send))),
        ]));
        section(&mut lines, shown, COUNTS_ROWS);
        let mut latest = vec![Line::styled(" latest listed", dim)];
        if state.latest.is_empty() {
            latest.push(Line::styled("   none", dim));
        }
        let room = width.saturating_sub(4);
        for listed in &state.latest {
            latest.push(Line::raw(format!("   {}", shortened(listed, room))));
        }
        section(&mut lines, latest, LATEST + 2);
    } else {
        shown.push(Line::from(vec![
            Span::styled(" sent     ", dim),
            Span::raw(format!("{:>12} files  ", count(counts.files))),
            Span::styled(format!(" {:>10}", per_second(&state.arrived)), dim),
            Span::styled("      failed  ", dim),
            Span::styled(
                format!("{:>10}", count(counts.failed + counts.problems)),
                if counts.failed + counts.problems > 0 {
                    colored(Color::Red)
                } else {
                    Style::default()
                },
            ),
        ]));
        section(&mut lines, shown, COUNTS_ROWS);
        if fit.history {
            section(&mut lines, graph, HISTORY_ROWS);
        }
        // The files being transferred: a heading, the files, the rest
        // counted, and a blank row.
        section(
            &mut lines,
            transfers(&state.transfers, fit.transfers, width, dim),
            fit.transfers + 3,
        );
    }
    // Each host's SSH connection, known before the dashboard opens: its
    // rate, and how long since anything came on it.
    for (monitor, (history, age)) in view.monitors.iter().zip(&state.connections) {
        let heard = age.map_or_else(|| "—".to_owned(), |age| format!("{} s ago", age.as_secs()));
        let shown = vec![Line::from(vec![
            Span::styled(
                format!(" {:<26}", format!("SSH to {}", monitor.host())),
                dim,
            ),
            Span::raw(format!("{:>13}/s", size(rate(history)))),
            Span::styled(format!("{:>9}", "heard"), dim),
            Span::styled(
                format!("{heard:>10}"),
                age.map_or_else(Style::default, late),
            ),
        ])];
        section(&mut lines, shown, 2);
    }
    // The pipeline, which carries nothing in a dry run.
    if fit.pipeline {
        let averages = state.averages();
        let mut stages = vec![Span::styled(" pipeline  ", dim)];
        for stage in Stage::ALL {
            if let Some(level) = averages.get(&stage) {
                stages.push(Span::raw(format!("{} ", stage.name())));
                stages.extend(cells(*level, dim));
                stages.push(Span::raw("  "));
            }
        }
        let bottleneck = match bottleneck(&averages) {
            Some(stage) => Span::styled(stage.name(), colored(Color::Yellow)),
            None => Span::raw("none"),
        };
        let compression = saved(counts);
        let shown = vec![
            Line::from(stages),
            Line::from(vec![
                Span::styled(" bottleneck: ", dim),
                bottleneck.bold(),
                Span::raw(compression.replace(" · ", "      ")),
            ]),
        ];
        section(&mut lines, shown, PIPELINE_ROWS);
    }
    // How full each side's queues were.
    if fit.queues {
        section(&mut lines, queues(&state.levels, dim), QUEUE_ROWS);
    }
    // The latest warnings, at the bottom: the first to lose rows to a short
    // screen.
    let mut shown = vec![Line::styled(" warnings and failures", dim)];
    let latest = state.said.len().saturating_sub(WARNINGS);
    if state.said.is_empty() {
        shown.push(Line::styled("   none", dim));
    }
    for said in &state.said[latest..] {
        shown.push(Line::styled(format!("   {said}"), colored(Color::Yellow)));
    }
    section(&mut lines, shown, WARNINGS + 1);

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
            Span::styled("█".repeat(done), colored(Color::Green)),
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

    /// Ctrl-C cancels, `Ctrl-\\` asks where every thread is, as a terminal
    /// sends it either way, and `t` turns the history.
    #[test]
    fn each_key_asks_what_it_says() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let control = KeyModifiers::CONTROL;
        assert_eq!(
            action(&key(KeyCode::Char('c'), control)),
            Some(Action::Cancel)
        );
        assert_eq!(
            action(&key(KeyCode::Char('4'), control)),
            Some(Action::Snapshot)
        );
        assert_eq!(
            action(&key(KeyCode::Char('\\'), control)),
            Some(Action::Snapshot)
        );
        assert_eq!(
            action(&key(KeyCode::Char('t'), KeyModifiers::NONE)),
            Some(Action::History)
        );
        assert_eq!(action(&key(KeyCode::Char('c'), KeyModifiers::NONE)), None);
        assert_eq!(action(&key(KeyCode::Char('4'), KeyModifiers::NONE)), None);
    }

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

    /// At every small size, down to none, a transfer and a dry run are
    /// drawn, with every part there is to draw, and nothing panics.
    #[test]
    fn it_draws_at_every_small_size() {
        let started = Instant::now();
        let mut state = State::new(0, started);
        let now = started + Duration::from_secs(42);
        let counts = Counts {
            entries: 10_000,
            needed: 8_000,
            files: 4_620,
            bytes: 600 << 20,
            received: 600 << 20,
            to_send: 1 << 30,
            wire: 300 << 20,
            ..Counts::default()
        };
        state.take(Report::Counts(counts), now).unwrap();
        state
            .take(
                Report::Queues(vec![(Stage::Network, 100), (Stage::Read, 100)]),
                now,
            )
            .unwrap();
        state
            .take(Report::Said("cannot read sub/locked.txt".into()), now)
            .unwrap();
        state
            .take(Report::Busy("the Destination: write files".into()), now)
            .unwrap();
        state
            .take(
                Report::Transferring(vec![Transfer {
                    id: 7,
                    path: "photos/2024/08/IMG_4211.jpg".into(),
                    size: 100 << 20,
                    done: 45 << 20,
                }]),
                now,
            )
            .unwrap();
        for dry_run in [false, true] {
            let view = View {
                from: "./data".into(),
                to: "ntu:/backup".into(),
                monitors: Vec::new(),
                dry_run,
                null: false,
            };
            for width in 0..=40 {
                for height in 0..=12 {
                    let _ = drawn(&state, &view, now, width, height);
                }
            }
        }
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
            null: false,
        };
        // As tall as everything needs.
        let text = drawn(&state, &view, now, 90, 50);
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
        let listed: Vec<crate::report::Listed> = (1..=7)
            .map(|n| crate::report::Listed::to_copy(format!("dir/file{n}").into()))
            .collect();
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
            null: false,
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
                        queue("send stream 0", 0, 0, 256 << 10, true),
                        queue("send stream 1", 0, 16 << 10, 256 << 10, true),
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
                        queue("send stream 0", 256 << 10, 256 << 10, 256 << 10, true),
                        queue("send stream 1", 128 << 10, 256 << 10, 256 << 10, true),
                        queue("data", 256, 258, 256, false),
                        queue("manifest", 1 << 20, 1 << 20, 8 << 20, true),
                        queue("messages", 0, 2, 0, false),
                    ],
                ),
                started,
            )
            .unwrap();
        let lines: Vec<String> = queues(&state.levels, Style::default())
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect();
        // Every row, those that have not come as `—`, so that it is always
        // as tall.
        assert_eq!(
            lines,
            [
                " queues                                    now      room",
                "   Source       read                         —         —",
                "                compress                     —         —",
                "                data                       256       256  ▮▮▮▮",
                "                send ×2              384.0 KiB 512.0 KiB  ▮▮▮▯",
                "   Destination  decompress                   —         —",
                "                write                        0        32  ▯▯▯▯",
                "   back         Destination send 0 B/512.0 KiB",
                "   messages     Source 0",
                "   manifest     Source 1.0 MiB/8.0 MiB",
            ]
        );
        assert_eq!(lines.len(), QUEUE_ROWS - 1);
        // Before any came, as tall, all `—`.
        let none = queues(&[], Style::default());
        assert_eq!(none.len(), QUEUE_ROWS - 1);
        assert_eq!(none[7].to_string().trim_end(), "   back         —");
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
            null: false,
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
            null: false,
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
    fn what_needs_a_look_is_in_color() {
        if !*COLORED {
            // `NO_COLOR` is set where the tests run.
            return;
        }
        let started = Instant::now();
        let mut state = State::new(0, started);
        let now = started + Duration::from_secs(3);
        state
            .take(
                Report::Counts(Counts {
                    received: 1 << 20,
                    to_send: 2 << 20,
                    failed: 1,
                    ..Counts::default()
                }),
                now,
            )
            .unwrap();
        state
            .take(
                Report::Queues(vec![
                    (Stage::Read, 30),
                    (Stage::Compress, 60),
                    (Stage::Network, 100),
                ]),
                now,
            )
            .unwrap();
        state
            .take(Report::Said("cannot read a: denied".into()), now)
            .unwrap();
        let view = View {
            from: "a".into(),
            to: "b".into(),
            monitors: Vec::new(),
            dry_run: false,
            null: false,
        };
        let mut screen = Terminal::new(ratatui::backend::TestBackend::new(90, 50)).unwrap();
        screen
            .draw(|frame| draw(frame, &state, &view, now))
            .unwrap();
        let buffer = screen.backend().buffer();
        // The color of the cell `skip` characters into `text`, where it is
        // on the screen.
        let color = |text: &str, skip: usize| {
            for y in 0..buffer.area.height {
                let line: Vec<&str> = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                let joined = line.concat();
                if let Some(at) = joined.find(text) {
                    let x = (joined[..at].chars().count() + skip) as u16;
                    return buffer[(x, y)].fg;
                }
            }
            panic!("no {text:?} on the screen");
        };
        assert_eq!(color("█", 0), Color::Green);
        assert_eq!(color("░", 0), Color::Reset);
        assert_eq!(color("read ▮▯▯▯", 5), Color::Green);
        assert_eq!(color("read ▮▯▯▯", 6), Color::Reset);
        assert_eq!(color("compress ▮▮▯▯", 9), Color::Yellow);
        assert_eq!(color("network ▮▮▮▮", 8), Color::Red);
        assert_eq!(color("bottleneck: network", 12), Color::Yellow);
        assert_eq!(color("cannot read a", 0), Color::Yellow);
        assert_eq!(color("failed           1", 17), Color::Red);
        assert_eq!(late(Duration::from_secs(5)), Style::default());
        assert_eq!(late(Duration::from_secs(12)), colored(Color::Yellow));
        assert_eq!(late(Duration::from_secs(25)), colored(Color::Red));
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
        let lines: Vec<String> = transfers(&state.transfers, TRANSFERS, 80, Style::default())
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
        let lines = transfers(&state.transfers, TRANSFERS, 80, Style::default());
        assert_eq!(lines[1].to_string(), "   none");
    }

    /// What a screen has room for goes by its height alone, as the design
    /// gives it for a host: everything from 49 rows, no queues from 38, no
    /// pipeline from 35, 3 files being transferred from 32, and no history
    /// below.
    #[test]
    fn a_screen_has_room_for_what_its_height_says() {
        let sizes = Sizes {
            always: COUNTS_ROWS + 2 + WARNINGS + 1,
            history: HISTORY_ROWS,
            transfers: true,
            pipeline: PIPELINE_ROWS,
        };
        // The screen's rows, less the title, the bar, and the blank row.
        let fit = |rows: usize| Fit::for_height(rows - 3, &sizes);
        let all = Fit {
            history: true,
            transfers: TRANSFERS,
            pipeline: true,
            queues: true,
        };
        assert_eq!(fit(49), all);
        assert_eq!(
            fit(48),
            Fit {
                queues: false,
                ..all
            }
        );
        assert_eq!(
            fit(38),
            Fit {
                queues: false,
                ..all
            }
        );
        let no_pipeline = Fit {
            pipeline: false,
            queues: false,
            ..all
        };
        assert_eq!(fit(37), no_pipeline);
        assert_eq!(fit(35), no_pipeline);
        let fewer = Fit {
            transfers: FEW_TRANSFERS,
            ..no_pipeline
        };
        assert_eq!(fit(34), fewer);
        assert_eq!(fit(32), fewer);
        assert_eq!(
            fit(31),
            Fit {
                history: false,
                ..fewer
            }
        );
        assert_eq!(
            fit(16),
            Fit {
                history: false,
                ..fewer
            }
        );
    }

    /// The sections stay where they are, whatever they hold: no files or
    /// many, no warnings or many, the queues before they come and after.
    #[test]
    fn the_sections_stay_in_place_whatever_they_hold() {
        let started = Instant::now();
        let now = started + Duration::from_secs(5);
        let view = View {
            from: "a".into(),
            to: "b".into(),
            monitors: Vec::new(),
            dry_run: false,
            null: false,
        };
        let empty = State::new(0, started);
        let mut full = State::new(0, started);
        let file = |id: u64| Transfer {
            id,
            path: format!("file-{id}"),
            size: 100,
            done: 50,
        };
        full.take(Report::Transferring((0..9).map(file).collect()), now)
            .unwrap();
        for index in 0..8 {
            full.take(Report::Said(format!("warning {index}")), now)
                .unwrap();
        }
        full.take(
            Report::Levels {
                side: "the Source on this machine".into(),
                queues: vec![QueueMetrics {
                    name: "read".into(),
                    level: 3,
                    peak: 4,
                    capacity: 64,
                    bytes: false,
                }],
            },
            now,
        )
        .unwrap();
        full.take(Report::Queues(vec![(Stage::Read, 30)]), now)
            .unwrap();
        let headings = |state: &State, rows: u16| -> Vec<(usize, &str)> {
            let text = drawn(state, &view, now, 90, rows);
            let mut found = Vec::new();
            for (row, line) in text.lines().enumerate() {
                for heading in [
                    " history",
                    " transferring",
                    " pipeline",
                    " queues",
                    " warnings and failures",
                ] {
                    if line.starts_with(heading) {
                        found.push((row, heading));
                    }
                }
            }
            found
        };
        for rows in [50, 40, 30, 20] {
            assert_eq!(headings(&empty, rows), headings(&full, rows), "{rows} rows");
        }
        // As tall as everything needs, everything is there.
        assert_eq!(headings(&empty, 50).len(), 5);
    }
}
