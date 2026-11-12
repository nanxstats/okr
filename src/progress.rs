//! Progress reporting for `okr sync`.
//!
//! On an interactive terminal a sync draws a small live region on stderr:
//!
//! ```text
//! Resolved 12 sources in 1.4s
//! ⠹ Vendoring sources 4/12
//!     admiral  ━━━━━━━╸────────   1.9/4.1 MiB  412.5 KiB/s  from github.com
//! ```
//!
//! The header names the phase and counts finished sources. Beneath it is a
//! pool of rows, one for each source being worked on, which says what is
//! happening to that source: a download with its bytes, rate, and host, or a
//! step such as `resolving v1.5.0 via git ls-remote (3s)`. Three rules keep
//! the region calm:
//!
//! - A source gets a row only after it has been running for [`REVEAL_DELAY`],
//!   so work served from the cache never flashes on screen. The header obeys
//!   the same rule, so a fast sync draws nothing but its results.
//! - A row is a slot rather than a transient line. A finished source leaves
//!   its outcome in the slot, as in `✓ admiral  vendored 1.5.0
//!   (forge-tarball)`, until another source needs it. The region therefore
//!   grows to the number of sources in flight and then stops moving.
//! - Each phase collapses into one durable summary line when it ends.
//!
//! `--verbose` keeps one durable line per source instead of reusing its row,
//! and writes those lines even when stderr is not a terminal. Without
//! `--verbose`, redirected output carries no progress at all.
//!
//! Nothing else may write to the terminal while the region is live, because
//! indicatif redraws it by moving the cursor over the lines it last drew.
//! Host tools are therefore run with their output captured, and `sync`
//! finishes the display before it runs R.
//!
//! The reporter and its tasks are `Send + Sync`, and every source reports
//! through its own [`Task`], so the pool renders any number of sources at
//! once. `sync` currently works on one source at a time.

use std::fmt;
use std::io::{self, Read, Write};
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};

/// How often the live region is redrawn.
const TICK_INTERVAL: Duration = Duration::from_millis(100);
/// Work that finishes sooner than this is never drawn.
const REVEAL_DELAY: Duration = Duration::from_millis(200);
/// Spinner frames. indicatif reserves the last string for a finished bar.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", " "];
const BAR_CHARS: &str = "━╸─";
/// Bounds for the source-name column shared by every row.
const NAME_WIDTH: RangeInclusive<usize> = 8..=28;

/// A counted stage of a sync.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Phase {
    Resolve,
    Vendor,
}

impl Phase {
    const fn title(self) -> &'static str {
        match self {
            Self::Resolve => "Resolving sources",
            Self::Vendor => "Vendoring sources",
        }
    }

    const fn finished(self) -> &'static str {
        match self {
            Self::Resolve => "Resolved",
            Self::Vendor => "Vendored",
        }
    }
}

/// The progress reporter for one `okr sync` run.
pub(crate) struct SyncProgress {
    display: Option<Owner>,
}

impl SyncProgress {
    /// Choose a display for this run. `names` are the declared source names
    /// and size the name column.
    pub(crate) fn new<'a>(
        names: impl IntoIterator<Item = &'a str>,
        quiet: bool,
        verbose: bool,
    ) -> Self {
        if quiet {
            return Self::disabled();
        }
        let name_width = names
            .into_iter()
            .map(|name| name.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(*NAME_WIDTH.start(), *NAME_WIDTH.end());
        let target = ProgressDrawTarget::stderr();
        if !target.is_hidden() {
            Self::live(target, name_width, verbose, true)
        } else if verbose {
            Self::log(Box::new(io::stderr()), name_width)
        } else {
            Self::disabled()
        }
    }

    /// A reporter that draws and prints nothing.
    pub(crate) const fn disabled() -> Self {
        Self { display: None }
    }

    /// An animated display. `animate` starts the thread that redraws it;
    /// tests leave it off and advance time themselves.
    fn live(target: ProgressDrawTarget, name_width: usize, verbose: bool, animate: bool) -> Self {
        let display = Display::Live(MultiProgress::with_draw_target(target));
        Self::with_display(display, name_width, verbose, animate)
    }

    /// Durable lines only, for verbose runs without a terminal.
    pub(crate) fn log(sink: Box<dyn Write + Send>, name_width: usize) -> Self {
        Self::with_display(Display::Log(Mutex::new(sink)), name_width, true, false)
    }

    fn with_display(display: Display, name_width: usize, verbose: bool, animate: bool) -> Self {
        let core = Arc::new(Core {
            display,
            verbose,
            name_width,
            stopped: AtomicBool::new(false),
            state: Mutex::new(State {
                closed: false,
                born: Instant::now(),
                header: None,
                title: String::new(),
                phase: None,
                slots: Vec::new(),
                tasks: Vec::new(),
                next_id: 0,
            }),
        });
        let ticker = animate.then(|| spawn_ticker(Arc::clone(&core))).flatten();
        Self {
            display: Some(Owner {
                core,
                ticker: Mutex::new(ticker),
            }),
        }
    }

    fn core(&self) -> Option<&Arc<Core>> {
        self.display.as_ref().map(|owner| &owner.core)
    }

    /// Describe uncounted work, such as verification, in the header.
    pub(crate) fn status(&self, title: &str) {
        if let Some(core) = self.core() {
            core.set_title(title, None);
        }
    }

    /// Start a counted phase over `total` sources.
    pub(crate) fn begin(&self, phase: Phase, total: usize) {
        if let Some(core) = self.core() {
            core.set_title(phase.title(), Some(PhaseState::new(phase, total)));
        }
    }

    /// End the current phase and replace its rows with one summary line.
    pub(crate) fn complete(&self) {
        if let Some(core) = self.core() {
            core.complete_phase();
        }
    }

    /// Start reporting one source. It counts toward the current phase once
    /// [`Task::done`] is called; dropping it earlier reports nothing.
    pub(crate) fn task(&self, name: &str) -> Task {
        Task {
            handle: self.core().map(|core| {
                let id = core.start_task(name);
                (Arc::clone(core), id)
            }),
        }
    }

    /// Erase the live region, leaving only durable lines. Later calls on this
    /// reporter and its tasks do nothing.
    pub(crate) fn finish(&self) {
        if let Some(owner) = &self.display {
            owner.close();
        }
    }
}

/// The owner of the display and its redraw thread. Dropping the reporter
/// closes the display on the dropping thread and waits for the redraw
/// thread, so an error printed afterwards never races a redraw.
struct Owner {
    core: Arc<Core>,
    ticker: Mutex<Option<JoinHandle<()>>>,
}

impl Owner {
    fn close(&self) {
        self.core.stopped.store(true, Ordering::Release);
        if let Some(ticker) = lock(&self.ticker).take() {
            ticker.thread().unpark();
            let _ = ticker.join();
        }
        self.core.close();
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.close();
    }
}

fn spawn_ticker(core: Arc<Core>) -> Option<JoinHandle<()>> {
    // Without the thread the display is static but still correct.
    thread::Builder::new()
        .name("okr-progress".into())
        .spawn(move || {
            loop {
                thread::park_timeout(TICK_INTERVAL);
                if core.stopped.load(Ordering::Acquire) {
                    break;
                }
                core.tick(Instant::now());
            }
        })
        .ok()
}

enum Display {
    /// The animated region plus durable lines, drawn through indicatif.
    Live(MultiProgress),
    /// Durable lines written as plain text.
    Log(Mutex<Box<dyn Write + Send>>),
}

struct Core {
    display: Display,
    verbose: bool,
    name_width: usize,
    stopped: AtomicBool,
    state: Mutex<State>,
}

/// Everything the display knows. Every bar is created, changed, and removed
/// while this is locked, so the redraw thread and the working threads never
/// interleave their updates.
struct State {
    closed: bool,
    born: Instant,
    header: Option<ProgressBar>,
    title: String,
    phase: Option<PhaseState>,
    slots: Vec<Slot>,
    /// Sources being worked on, oldest first.
    tasks: Vec<TaskRecord>,
    next_id: u64,
}

struct PhaseState {
    phase: Phase,
    total: usize,
    done: usize,
    downloaded: u64,
    started: Instant,
}

impl PhaseState {
    fn new(phase: Phase, total: usize) -> Self {
        Self {
            phase,
            total,
            done: 0,
            downloaded: 0,
            started: Instant::now(),
        }
    }
}

/// One row of the pool. An empty slot keeps showing the outcome of the last
/// source that used it.
struct Slot {
    bar: ProgressBar,
    task: Option<u64>,
    kind: RowKind,
    text: String,
}

struct TaskRecord {
    id: u64,
    name: String,
    started: Instant,
    step: String,
    step_started: Instant,
    transfer: Option<TransferRecord>,
    /// Bytes received by transfers that have ended.
    downloaded: u64,
}

struct TransferRecord {
    host: String,
    total: Option<u64>,
    received: Arc<AtomicU64>,
    /// When the response began to arrive.
    streaming_since: Option<Instant>,
}

impl TransferRecord {
    /// The average rate in bytes per second, once it has had a second to
    /// settle. indicatif's own estimate would start from the jump a row makes
    /// when it first appears partway through a download.
    fn rate(&self, received: u64, now: Instant) -> Option<u64> {
        let elapsed = now.saturating_duration_since(self.streaming_since?);
        (elapsed >= Duration::from_secs(1))
            .then(|| (received as f64 / elapsed.as_secs_f64()).round() as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    /// A step in words, or a download whose size is unknown.
    Text,
    /// A download of known size.
    Bar,
    /// The outcome of a finished source.
    Done,
}

struct Row {
    kind: RowKind,
    text: String,
    /// Bytes received and expected. Only a [`RowKind::Bar`] draws them.
    position: u64,
    length: u64,
}

impl Row {
    const fn text(text: String) -> Self {
        Self {
            kind: RowKind::Text,
            text,
            position: 0,
            length: 0,
        }
    }
}

impl TaskRecord {
    /// The row for this source as of `now`.
    ///
    /// A download puts its numbers before its host, and the host before
    /// anything else, so a narrow terminal truncates the least useful part.
    fn row(&self, now: Instant) -> Row {
        let waited = now.saturating_duration_since(self.step_started);
        let Some(transfer) = &self.transfer else {
            return Row::text(with_elapsed(&self.step, waited));
        };
        let host = &transfer.host;
        let received = transfer.received.load(Ordering::Relaxed);
        let rate = transfer
            .rate(received, now)
            .map(|rate| format!("  {:<11}", format!("{}/s", format_bytes(rate))))
            .unwrap_or_default();
        match transfer.total {
            Some(total) => Row {
                kind: RowKind::Bar,
                text: format!(
                    "{:>13}{rate}  from {host}",
                    format_progress(received, total)
                ),
                position: received,
                length: total,
            },
            None if received > 0 => {
                Row::text(format!("{:>9}{rate}  from {host}", format_bytes(received)))
            }
            // Still connecting: nothing has arrived to count.
            None => {
                let step = if self.step.is_empty() {
                    "downloading"
                } else {
                    &self.step
                };
                Row::text(with_elapsed(&format!("{step} from {host}"), waited))
            }
        }
    }
}

/// A line that stays on screen or in the log after the work it describes.
enum Line<'a> {
    Source { name: &'a str, text: &'a str },
    Summary { headline: &'a str, detail: &'a str },
}

impl Core {
    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn set_title(&self, title: &str, phase: Option<PhaseState>) {
        let mut guard = self.state();
        let state = &mut *guard;
        if state.closed {
            return;
        }
        title.clone_into(&mut state.title);
        state.phase = phase;
        self.replace_header(state);
    }

    fn complete_phase(&self) {
        let mut guard = self.state();
        let state = &mut *guard;
        let Some(phase) = state.phase.take() else {
            return;
        };
        if state.closed {
            return;
        }
        state.title.clear();
        if let Display::Live(multi) = &self.display {
            for slot in state.slots.drain(..) {
                multi.remove(&slot.bar);
            }
        }
        let headline = format!(
            "{} {} {}",
            phase.phase.finished(),
            phase.done,
            if phase.done == 1 { "source" } else { "sources" }
        );
        let mut detail = format!("in {}", format_duration(phase.started.elapsed()));
        if phase.downloaded > 0 {
            detail.push_str(&format!(" · {} downloaded", format_bytes(phase.downloaded)));
        }
        // The header loses its title first, so the summary is never drawn
        // above a header that still names the phase it just closed.
        self.replace_header(state);
        self.print(Line::Summary {
            headline: &headline,
            detail: &detail,
        });
    }

    fn start_task(&self, name: &str) -> u64 {
        let mut state = self.state();
        let id = state.next_id;
        state.next_id += 1;
        if !state.closed {
            let now = Instant::now();
            state.tasks.push(TaskRecord {
                id,
                name: name.to_owned(),
                started: now,
                step: String::new(),
                step_started: now,
                transfer: None,
                downloaded: 0,
            });
        }
        id
    }

    fn set_step(&self, id: u64, step: &str) {
        let mut state = self.state();
        if let Some(task) = state.task_mut(id) {
            step.clone_into(&mut task.step);
            task.step_started = Instant::now();
        }
    }

    fn start_transfer(&self, id: u64, host: &str, received: &Arc<AtomicU64>) {
        let mut state = self.state();
        if let Some(task) = state.task_mut(id) {
            task.step_started = Instant::now();
            task.transfer = Some(TransferRecord {
                host: host.to_owned(),
                total: None,
                received: Arc::clone(received),
                streaming_since: None,
            });
        }
    }

    fn set_transfer_length(&self, id: u64, total: Option<u64>) {
        let mut state = self.state();
        if let Some(transfer) = state.task_mut(id).and_then(|task| task.transfer.as_mut()) {
            transfer.total = total;
            transfer.streaming_since = Some(Instant::now());
        }
    }

    fn end_transfer(&self, id: u64) {
        let mut state = self.state();
        if let Some(task) = state.task_mut(id)
            && let Some(transfer) = task.transfer.take()
        {
            task.downloaded += transfer.received.load(Ordering::Relaxed);
        }
    }

    fn finish_task(&self, id: u64, outcome: &str) {
        let mut guard = self.state();
        let state = &mut *guard;
        let Some(index) = state.tasks.iter().position(|task| task.id == id) else {
            return;
        };
        let task = state.tasks.remove(index);
        let text = done_text(outcome, task.downloaded, task.started.elapsed());
        if let Some(phase) = &mut state.phase {
            phase.done += 1;
            phase.downloaded += task.downloaded;
        }
        let slot = state.slots.iter().position(|slot| slot.task == Some(id));
        if self.verbose {
            // The durable line replaces the row, so the outcome is shown once.
            if let (Some(index), Display::Live(multi)) = (slot, &self.display) {
                multi.remove(&state.slots.remove(index).bar);
            }
            self.print(Line::Source {
                name: &task.name,
                text: &text,
            });
        } else if let Some(index) = slot {
            let slot = &mut state.slots[index];
            slot.task = None;
            self.draw_row(
                slot,
                Row {
                    kind: RowKind::Done,
                    text,
                    position: 0,
                    length: 0,
                },
            );
        }
        if let Some(header) = &state.header {
            header.set_message(state.counter());
        }
    }

    /// Forget a source that ended without an outcome, as on an error.
    fn abandon_task(&self, id: u64) {
        let mut guard = self.state();
        let state = &mut *guard;
        let Some(index) = state.tasks.iter().position(|task| task.id == id) else {
            return;
        };
        state.tasks.remove(index);
        if let Display::Live(multi) = &self.display
            && let Some(index) = state.slots.iter().position(|slot| slot.task == Some(id))
        {
            multi.remove(&state.slots.remove(index).bar);
        }
    }

    /// Redraw the live region as of `now`.
    fn tick(&self, now: Instant) {
        let Display::Live(multi) = &self.display else {
            return;
        };
        let mut guard = self.state();
        let state = &mut *guard;
        if state.closed {
            return;
        }
        if state.header.is_none() {
            if now.saturating_duration_since(state.born) < REVEAL_DELAY {
                return;
            }
            let header = multi.add(prepared_bar(&state.title, &state.counter()));
            header.set_style(header_style());
            state.header = Some(header);
        }
        for task in &state.tasks {
            if now.saturating_duration_since(task.started) < REVEAL_DELAY
                || state.slots.iter().any(|slot| slot.task == Some(task.id))
            {
                continue;
            }
            let row = task.row(now);
            let bar = prepared_bar(&task.name, &row.text);
            if row.kind == RowKind::Bar {
                bar.set_length(row.length);
                bar.set_position(row.position);
            }
            // Take over the row of a finished source before adding a row.
            let free = state.slots.iter().position(|slot| slot.task.is_none());
            let bar = match free {
                Some(index) => {
                    let finished = &state.slots[index].bar;
                    let bar = multi.insert_before(finished, bar);
                    multi.remove(finished);
                    bar
                }
                None => multi.add(bar),
            };
            bar.set_style(row_style(row.kind, self.name_width));
            let slot = Slot {
                bar,
                task: Some(task.id),
                kind: row.kind,
                text: row.text,
            };
            match free {
                Some(index) => state.slots[index] = slot,
                None => state.slots.push(slot),
            }
        }
        for slot in &mut state.slots {
            let task = slot
                .task
                .and_then(|id| state.tasks.iter().find(|task| task.id == id));
            if let Some(task) = task {
                self.draw_row(slot, task.row(now));
            }
        }
        if let Some(header) = &state.header {
            header.tick();
        }
    }

    /// Bring a row up to date with the source that occupies it.
    ///
    /// indicatif may draw after each change. Bytes are therefore set before
    /// the style and the text, which makes every frame drawn on the way
    /// either the previous one or the final one.
    fn draw_row(&self, slot: &mut Slot, row: Row) {
        if row.kind == RowKind::Bar {
            if slot.bar.length() != Some(row.length) {
                slot.bar.set_length(row.length);
            }
            slot.bar.set_position(row.position);
        }
        if slot.kind != row.kind {
            slot.bar.set_style(row_style(row.kind, self.name_width));
            slot.kind = row.kind;
        }
        if slot.text != row.text {
            slot.bar.set_message(row.text.clone());
            slot.text = row.text;
        }
        slot.bar.tick();
    }

    /// Show the current title in the header.
    ///
    /// The header is rebuilt off screen and swapped in whole, so no frame
    /// pairs the new title with the old counter.
    fn replace_header(&self, state: &mut State) {
        let Display::Live(multi) = &self.display else {
            return;
        };
        let Some(previous) = state.header.take() else {
            return;
        };
        let header = multi.insert_before(&previous, prepared_bar(&state.title, &state.counter()));
        multi.remove(&previous);
        header.set_style(header_style());
        // Not subject to the redraw rate limit: a skipped frame would leave
        // the previous title on screen until the next tick.
        header.force_draw();
        state.header = Some(header);
    }

    fn print(&self, line: Line<'_>) {
        match &self.display {
            Display::Live(multi) => {
                // A finished bar at the top of the region is drawn once more
                // and then left behind, above everything that is still live.
                // Unlike `MultiProgress::println`, its template is styled by
                // the terminal's own color rules.
                let (style, prefix, message) = match line {
                    Line::Source { name, text } => (source_line_style(self.name_width), name, text),
                    Line::Summary { headline, detail } => (summary_line_style(), headline, detail),
                };
                let bar = multi.insert(0, prepared_bar(prefix, message));
                bar.set_style(style);
                bar.finish();
            }
            Display::Log(sink) => {
                let mut sink = lock(sink);
                let _ = match line {
                    Line::Source { name, text } => {
                        writeln!(sink, "  ✓ {name:<width$} {text}", width = self.name_width)
                    }
                    Line::Summary { headline, detail } => writeln!(sink, "{headline} {detail}"),
                };
            }
        }
    }

    fn close(&self) {
        let mut state = self.state();
        if state.closed {
            return;
        }
        state.closed = true;
        state.tasks.clear();
        state.phase = None;
        if let Display::Live(multi) = &self.display {
            for slot in state.slots.drain(..) {
                multi.remove(&slot.bar);
            }
            // Clearing the header redraws the region without its rows.
            // `MultiProgress::clear` would also erase the durable lines.
            if let Some(header) = state.header.take() {
                header.finish_and_clear();
            }
        }
    }
}

impl State {
    fn task_mut(&mut self, id: u64) -> Option<&mut TaskRecord> {
        self.tasks.iter_mut().find(|task| task.id == id)
    }

    /// The header's count of finished sources, empty outside a phase.
    fn counter(&self) -> String {
        self.phase
            .as_ref()
            .map(|phase| format!("{}/{}", phase.done, phase.total))
            .unwrap_or_default()
    }
}

/// The progress of one source. It reports nothing when the reporter is
/// disabled, so callers never branch on whether progress is shown.
pub(crate) struct Task {
    handle: Option<(Arc<Core>, u64)>,
}

impl Task {
    pub(crate) const fn disabled() -> Self {
        Self { handle: None }
    }

    /// Say what is being done for this source right now.
    pub(crate) fn step(&self, step: &str) {
        if let Some((core, id)) = &self.handle {
            core.set_step(*id, step);
        }
    }

    /// Start a download from `url` for this source. Only the host is shown,
    /// so credentials embedded in a URL never reach the terminal.
    pub(crate) fn transfer(&self, url: &str) -> Transfer {
        let received = Arc::new(AtomicU64::new(0));
        if let Some((core, id)) = &self.handle {
            core.start_transfer(*id, display_host(url), &received);
        }
        Transfer {
            handle: self.handle.clone(),
            received,
        }
    }

    /// Record that this source succeeded. `outcome` reads as a short
    /// past-tense clause, for example `vendored 1.5.0 (forge-tarball)`.
    pub(crate) fn done(&self, outcome: &str) {
        if let Some((core, id)) = &self.handle {
            core.finish_task(*id, outcome);
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if let Some((core, id)) = &self.handle {
            core.abandon_task(*id);
        }
    }
}

/// One download in flight. Dropping it ends the download.
pub(crate) struct Transfer {
    handle: Option<(Arc<Core>, u64)>,
    received: Arc<AtomicU64>,
}

impl Transfer {
    /// Declare the expected size once the response headers are known.
    pub(crate) fn set_length(&self, total: Option<u64>) {
        if let Some((core, id)) = &self.handle {
            core.set_transfer_length(*id, total);
        }
    }

    /// Count the bytes read through `reader` toward this download.
    pub(crate) fn wrap_read<R: Read>(&self, reader: R) -> impl Read + use<R> {
        CountingReader {
            reader,
            received: Arc::clone(&self.received),
        }
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if let Some((core, id)) = &self.handle {
            core.end_transfer(*id);
        }
    }
}

/// Counts bytes without locking, so reading stays cheap however often the
/// display is redrawn.
struct CountingReader<R> {
    reader: R,
    received: Arc<AtomicU64>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.reader.read(buffer)?;
        self.received.fetch_add(count as u64, Ordering::Relaxed);
        Ok(count)
    }
}

/// A bar with its text in place that is not on screen yet.
///
/// A bar draws whenever its text changes, so a row or header whose subject
/// changes is built here and swapped in whole rather than edited in view.
/// Styles are applied after the bar joins the live region, which is when
/// indicatif can tell that their colors follow stderr.
fn prepared_bar(prefix: &str, message: &str) -> ProgressBar {
    let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden());
    bar.set_prefix(prefix.to_owned());
    bar.set_message(message.to_owned());
    bar
}

fn header_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} {prefix:.bold} {wide_msg:.dim}")
        .expect("static header template is valid")
        .tick_strings(SPINNER_FRAMES)
}

fn row_style(kind: RowKind, name_width: usize) -> ProgressStyle {
    let template = match kind {
        RowKind::Text => format!("    {{prefix:{name_width}.bold}} {{wide_msg}}"),
        RowKind::Bar => {
            format!("    {{prefix:{name_width}.bold}} {{bar:16.cyan/dim}} {{wide_msg}}")
        }
        RowKind::Done => format!("  {{mark:.green}} {{prefix:{name_width}}} {{wide_msg:.dim}}"),
    };
    ProgressStyle::with_template(&template)
        .expect("static row template is valid")
        .progress_chars(BAR_CHARS)
        .with_key("mark", check_mark)
}

fn source_line_style(name_width: usize) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "  {{mark:.green}} {{prefix:{name_width}}} {{msg:.dim}}"
    ))
    .expect("static source line template is valid")
    .with_key("mark", check_mark)
}

fn summary_line_style() -> ProgressStyle {
    ProgressStyle::with_template("{prefix:.bold} {msg:.dim}")
        .expect("static summary line template is valid")
}

fn check_mark(_: &ProgressState, writer: &mut dyn fmt::Write) {
    let _ = writer.write_str("✓");
}

/// Append the time spent on a step once it is long enough to be worth
/// showing. Steps without byte progress have no other sign of life.
fn with_elapsed(step: &str, elapsed: Duration) -> String {
    match elapsed.as_secs() {
        0 => step.to_owned(),
        seconds if step.is_empty() => format!("({seconds}s)"),
        seconds => format!("{step} ({seconds}s)"),
    }
}

fn done_text(outcome: &str, downloaded: u64, elapsed: Duration) -> String {
    if downloaded > 0 {
        format!(
            "{outcome} · downloaded {} in {}",
            format_bytes(downloaded),
            format_duration(elapsed)
        )
    } else if elapsed >= Duration::from_secs(1) {
        format!("{outcome} in {}", format_duration(elapsed))
    } else {
        outcome.to_owned()
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 60 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else if seconds >= 1 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

/// The largest binary unit that keeps `bytes` at or above one, as a divisor
/// and a name. One decimal is shown, so a value that would round up to 1024
/// moves to the next unit.
fn byte_unit(bytes: u64) -> (f64, &'static str) {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut divisor = 1.0;
    let mut unit = 0;
    while bytes as f64 / divisor >= 1023.95 && unit + 1 < UNITS.len() {
        divisor *= 1024.0;
        unit += 1;
    }
    (divisor, UNITS[unit])
}

/// A byte count such as `512 B` or `1.9 MiB`.
fn format_bytes(bytes: u64) -> String {
    match byte_unit(bytes) {
        (_, "B") => format!("{bytes} B"),
        (divisor, unit) => format!("{:.1} {unit}", bytes as f64 / divisor),
    }
}

/// Bytes received out of a known total, in the total's unit: `1.9/4.1 MiB`.
fn format_progress(received: u64, total: u64) -> String {
    match byte_unit(total) {
        (_, "B") => format!("{received}/{total} B"),
        (divisor, unit) => format!(
            "{:.1}/{:.1} {unit}",
            received as f64 / divisor,
            total as f64 / divisor
        ),
    }
}

/// The host of a remote URL, without any credentials it embeds.
pub(crate) fn display_host(url: &str) -> &str {
    const LOCAL: &str = "the local filesystem";
    let authority = match url.split_once("://") {
        Some(("file", _)) => return LOCAL,
        Some((_, rest)) => rest.split('/').next().unwrap_or(rest),
        // An scp-like git remote, `user@host:path`. Anything else is a path.
        None => match url.split_once(':') {
            Some((authority, _)) if !authority.contains('/') => authority,
            _ => return LOCAL,
        },
    };
    match authority.rsplit('@').next() {
        Some(host) if !host.is_empty() => host,
        _ => LOCAL,
    }
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
impl SyncProgress {
    /// Redraw as if `elapsed` had passed since now, then flush the frame.
    fn advance(&self, elapsed: Duration) {
        let core = self.core().expect("reporter is enabled");
        core.tick(Instant::now() + elapsed);
        self.settle();
    }

    /// Draw the current state regardless of indicatif's rate limit.
    fn settle(&self) {
        let core = self.core().expect("reporter is enabled");
        let state = core.state();
        if let Some(header) = &state.header {
            header.force_draw();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Cursor, Read, Write};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use indicatif::{ProgressDrawTarget, TermLike};

    use super::{
        Phase, SPINNER_FRAMES, SyncProgress, display_host, done_text, format_bytes,
        format_duration, format_progress, lock, with_elapsed,
    };

    const SECOND: Duration = Duration::from_secs(1);

    /// A terminal that keeps the screen it has been asked to draw.
    #[derive(Debug, Clone, Default)]
    struct Screen(Arc<Mutex<ScreenState>>);

    #[derive(Debug, Default)]
    struct ScreenState {
        lines: Vec<Vec<char>>,
        row: usize,
        column: usize,
    }

    impl Screen {
        const WIDTH: usize = 110;

        fn reporter(&self, verbose: bool) -> SyncProgress {
            let target = ProgressDrawTarget::term_like(Box::new(self.clone()));
            SyncProgress::live(target, 8, verbose, false)
        }

        /// The visible text, without styling or trailing blanks. Every
        /// spinner frame reads as the first one, so expectations do not
        /// depend on how often a scenario redraws.
        fn text(&self) -> String {
            let spinning = SPINNER_FRAMES[..SPINNER_FRAMES.len() - 1].concat();
            let state = lock(&self.0);
            let mut lines = state
                .lines
                .iter()
                .map(|line| {
                    let line = line
                        .iter()
                        .map(|character| {
                            if spinning.contains(*character) {
                                '⠋'
                            } else {
                                *character
                            }
                        })
                        .collect::<String>();
                    line.trim_end().to_owned()
                })
                .collect::<Vec<_>>();
            while lines.last().is_some_and(String::is_empty) {
                lines.pop();
            }
            lines.join("\n")
        }
    }

    impl ScreenState {
        fn line(&mut self) -> &mut Vec<char> {
            while self.lines.len() <= self.row {
                self.lines.push(Vec::new());
            }
            &mut self.lines[self.row]
        }

        fn write(&mut self, text: &str) {
            let mut characters = text.chars();
            while let Some(character) = characters.next() {
                match character {
                    '\r' => self.column = 0,
                    '\n' => {
                        self.row += 1;
                        self.column = 0;
                    }
                    // Skip a styling sequence: ESC [ parameters letter.
                    '\u{1b}' => {
                        for parameter in characters.by_ref() {
                            if parameter.is_ascii_alphabetic() {
                                break;
                            }
                        }
                    }
                    _ => {
                        // A full line wraps when the next character arrives.
                        if self.column >= Screen::WIDTH {
                            self.row += 1;
                            self.column = 0;
                        }
                        let column = self.column;
                        let line = self.line();
                        if line.len() <= column {
                            line.resize(column + 1, ' ');
                        }
                        line[column] = character;
                        self.column += 1;
                    }
                }
            }
        }
    }

    impl TermLike for Screen {
        fn width(&self) -> u16 {
            Self::WIDTH as u16
        }

        fn height(&self) -> u16 {
            40
        }

        fn move_cursor_up(&self, count: usize) -> io::Result<()> {
            let mut state = lock(&self.0);
            state.row = state.row.saturating_sub(count);
            Ok(())
        }

        fn move_cursor_down(&self, count: usize) -> io::Result<()> {
            lock(&self.0).row += count;
            Ok(())
        }

        fn move_cursor_right(&self, count: usize) -> io::Result<()> {
            lock(&self.0).column += count;
            Ok(())
        }

        fn move_cursor_left(&self, count: usize) -> io::Result<()> {
            let mut state = lock(&self.0);
            state.column = state.column.saturating_sub(count);
            Ok(())
        }

        fn write_line(&self, text: &str) -> io::Result<()> {
            let mut state = lock(&self.0);
            state.write(text);
            state.write("\n");
            Ok(())
        }

        fn write_str(&self, text: &str) -> io::Result<()> {
            lock(&self.0).write(text);
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            let mut state = lock(&self.0);
            state.line().clear();
            state.column = 0;
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A log sink the test can read back.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<u8>>>);

    impl Log {
        fn text(&self) -> String {
            String::from_utf8(lock(&self.0).clone()).unwrap()
        }
    }

    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            lock(&self.0).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn read_through(transfer: &super::Transfer, bytes: usize) {
        let mut reader = transfer.wrap_read(Cursor::new(vec![0_u8; bytes]));
        io::copy(&mut reader, &mut io::sink()).unwrap();
    }

    #[test]
    fn fast_work_draws_only_the_phase_summary() {
        let screen = Screen::default();
        let progress = screen.reporter(false);
        progress.begin(Phase::Resolve, 2);
        for name in ["alpha", "bravo"] {
            let task = progress.task(name);
            task.step("resolving main via git ls-remote");
            progress.advance(Duration::ZERO);
            assert_eq!(screen.text(), "", "{name} was drawn before the delay");
            task.done("resolved 1.0.0");
        }
        progress.complete();
        progress.finish();

        let text = screen.text();
        assert!(text.starts_with("Resolved 2 sources in "), "{text}");
        assert_eq!(text.lines().count(), 1, "{text}");
    }

    #[test]
    fn slow_steps_show_the_source_the_step_and_the_time_spent() {
        let screen = Screen::default();
        let progress = screen.reporter(false);
        progress.begin(Phase::Resolve, 3);
        let task = progress.task("alpha");
        task.step("resolving v1.5.0 via git ls-remote");
        progress.advance(3 * SECOND);

        assert_eq!(
            screen.text(),
            "⠋ Resolving sources 0/3\n    alpha    resolving v1.5.0 via git ls-remote (3s)"
        );
    }

    #[test]
    fn downloads_show_bytes_and_the_host_without_credentials() {
        let screen = Screen::default();
        let progress = screen.reporter(false);
        progress.begin(Phase::Vendor, 2);
        let task = progress.task("alpha");
        task.step("downloading source archive");
        let transfer = task.transfer("https://user:secret@example.org/alpha_1.0.0.tar.gz");
        progress.advance(2 * SECOND);
        assert_eq!(
            screen.text(),
            "⠋ Vendoring sources 0/2\n    alpha    downloading source archive from example.org (2s)"
        );

        transfer.set_length(Some(4096));
        read_through(&transfer, 1024);
        progress.advance(2 * SECOND);
        let text = screen.text();
        let row = text.lines().nth(1).unwrap();
        assert!(
            row.starts_with("    alpha    ━━━━╸───────────   1.0/4.0 KiB  "),
            "{text}"
        );
        assert!(row.ends_with(" B/s      from example.org"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn downloads_of_unknown_size_show_bytes_received() {
        let screen = Screen::default();
        let progress = screen.reporter(false);
        progress.begin(Phase::Vendor, 1);
        let task = progress.task("alpha");
        let transfer = task.transfer("https://github.com/owner/alpha/archive/main.tar.gz");
        transfer.set_length(None);
        read_through(&transfer, 2048);
        progress.advance(SECOND);

        assert_eq!(
            screen.text(),
            "⠋ Vendoring sources 0/1\n    alpha      2.0 KiB  2.0 KiB/s    from github.com"
        );
    }

    #[test]
    fn a_finished_source_keeps_its_row_until_another_source_needs_it() {
        let screen = Screen::default();
        let progress = screen.reporter(false);
        progress.begin(Phase::Vendor, 3);

        let alpha = progress.task("alpha");
        alpha.step("downloading source archive");
        let transfer = alpha.transfer("https://example.org/alpha.tar.gz");
        transfer.set_length(Some(2048));
        read_through(&transfer, 2048);
        progress.advance(SECOND);
        drop(transfer);
        alpha.done("vendored 1.2.0 (tarball)");
        progress.settle();
        let text = screen.text();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("⠋ Vendoring sources 1/3"));
        let row = lines.next().unwrap();
        assert!(
            row.starts_with("  ✓ alpha    vendored 1.2.0 (tarball) · downloaded 2.0 KiB in "),
            "{text}"
        );
        assert_eq!(lines.next(), None);

        // A source served from the cache finishes without taking the row.
        let bravo = progress.task("bravo");
        progress.advance(Duration::ZERO);
        bravo.done("vendored 0.9.1 (tarball, cached)");
        progress.settle();
        let text = screen.text();
        assert!(
            text.starts_with("⠋ Vendoring sources 2/3\n  ✓ alpha "),
            "{text}"
        );
        assert_eq!(text.lines().count(), 2, "{text}");

        // A slow source reuses the row instead of adding one.
        let charlie = progress.task("charlie");
        charlie.step("cloning from example.org");
        progress.advance(SECOND);
        assert_eq!(
            screen.text(),
            "⠋ Vendoring sources 2/3\n    charlie  cloning from example.org (1s)"
        );
    }

    #[test]
    fn sources_in_flight_each_get_a_row() {
        let screen = Screen::default();
        let progress = screen.reporter(false);
        progress.begin(Phase::Vendor, 2);
        let alpha = progress.task("alpha");
        let bravo = progress.task("bravo");
        alpha.step("extracting");
        bravo.step("cloning from example.org");
        progress.advance(SECOND);

        assert_eq!(
            screen.text(),
            "⠋ Vendoring sources 0/2\n    alpha    extracting (1s)\n    bravo    cloning from example.org (1s)"
        );
    }

    #[test]
    fn a_completed_phase_leaves_a_summary_and_keeps_the_header_live() {
        let screen = Screen::default();
        let progress = screen.reporter(false);
        progress.begin(Phase::Vendor, 1);
        let task = progress.task("alpha");
        let transfer = task.transfer("https://example.org/alpha.tar.gz");
        read_through(&transfer, 3 * 1024 * 1024);
        progress.advance(SECOND);
        drop(transfer);
        task.done("vendored 1.2.0 (tarball)");
        progress.complete();
        progress.status("Checking installed R library");
        progress.settle();

        let text = screen.text();
        let mut lines = text.lines();
        let summary = lines.next().unwrap();
        assert!(summary.starts_with("Vendored 1 source in "), "{text}");
        assert!(summary.ends_with(" · 3.0 MiB downloaded"), "{text}");
        assert_eq!(lines.next(), Some("⠋ Checking installed R library"));
        assert_eq!(lines.next(), None);

        progress.finish();
        assert_eq!(screen.text(), summary);
    }

    #[test]
    fn a_failed_source_leaves_nothing_behind() {
        let screen = Screen::default();
        let progress = screen.reporter(false);
        progress.begin(Phase::Vendor, 2);
        let task = progress.task("alpha");
        task.step("cloning from example.org");
        progress.advance(SECOND);
        assert_eq!(screen.text().lines().count(), 2);

        drop(task);
        drop(progress);
        assert_eq!(screen.text(), "");
    }

    #[test]
    fn verbose_keeps_a_line_per_source_above_the_live_region() {
        let screen = Screen::default();
        let progress = screen.reporter(true);
        progress.begin(Phase::Resolve, 2);
        let alpha = progress.task("alpha");
        alpha.step("resolving main via git ls-remote");
        progress.advance(SECOND);
        alpha.done("resolved main to 9f2c9f2");
        let bravo = progress.task("bravo");
        bravo.done("resolved 0.9.1");
        progress.settle();

        let text = screen.text();
        let mut lines = text.lines();
        let alpha_line = lines.next().unwrap();
        assert!(
            alpha_line.starts_with("  ✓ alpha    resolved main to 9f2c9f2"),
            "{text}"
        );
        assert_eq!(lines.next(), Some("  ✓ bravo    resolved 0.9.1"));
        assert_eq!(lines.next(), Some("⠋ Resolving sources 2/2"));
        assert_eq!(lines.next(), None);

        progress.complete();
        progress.finish();
        assert_eq!(screen.text().lines().count(), 3);
        assert!(screen.text().contains("\nResolved 2 sources in "));
    }

    #[test]
    fn verbose_without_a_terminal_logs_plain_lines() {
        let log = Log::default();
        let progress = SyncProgress::log(Box::new(log.clone()), 8);
        progress.status("Verifying vendor tree");
        progress.begin(Phase::Vendor, 1);
        let task = progress.task("alpha");
        task.step("extracting");
        task.done("vendored 1.2.0 (tarball, cached)");
        progress.complete();
        progress.finish();

        let text = log.text();
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("  ✓ alpha    vendored 1.2.0 (tarball, cached)")
        );
        assert!(lines.next().unwrap().starts_with("Vendored 1 source in "));
        assert_eq!(lines.next(), None);
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn a_disabled_reporter_still_counts_bytes_through_its_reader() {
        let progress = SyncProgress::new(["alpha"], true, false);
        progress.begin(Phase::Vendor, 1);
        let task = progress.task("alpha");
        task.step("downloading source archive");
        let transfer = task.transfer("https://example.org/alpha.tar.gz");
        let mut bytes = Vec::new();
        transfer
            .wrap_read(Cursor::new(b"fixture"))
            .read_to_end(&mut bytes)
            .unwrap();
        task.done("vendored 1.0.0");
        progress.complete();
        progress.finish();

        assert_eq!(bytes, b"fixture");
    }

    #[test]
    fn durations_and_outcomes_are_compact() {
        assert_eq!(format_duration(Duration::from_millis(12)), "12ms");
        assert_eq!(format_duration(Duration::from_millis(1_440)), "1.4s");
        assert_eq!(format_duration(Duration::from_secs(92)), "1m 32s");

        assert_eq!(
            with_elapsed("extracting", Duration::from_millis(900)),
            "extracting"
        );
        assert_eq!(with_elapsed("extracting", 4 * SECOND), "extracting (4s)");

        let quick = Duration::from_millis(20);
        assert_eq!(done_text("vendored 1.0.0", 0, quick), "vendored 1.0.0");
        assert_eq!(
            done_text("vendored 1.0.0", 0, 2 * SECOND),
            "vendored 1.0.0 in 2.0s"
        );
        assert_eq!(
            done_text("vendored 1.0.0", 2048, 2 * SECOND),
            "vendored 1.0.0 · downloaded 2.0 KiB in 2.0s"
        );
    }

    #[test]
    fn reporting_handles_can_cross_threads() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SyncProgress>();
        assert_send_sync::<super::Task>();
        assert_send_sync::<super::Transfer>();
        assert_send_sync::<crate::fetch::Fetcher>();
    }

    #[test]
    fn byte_counts_use_one_decimal_in_binary_units() {
        for (bytes, expected) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1024 * 1024 - 1, "1.0 MiB"),
            (1_992_294, "1.9 MiB"),
            (5 * 1024 * 1024 * 1024, "5.0 GiB"),
        ] {
            assert_eq!(format_bytes(bytes), expected, "{bytes}");
        }

        assert_eq!(format_progress(300, 900), "300/900 B");
        assert_eq!(format_progress(1_992_294, 4_299_161), "1.9/4.1 MiB");
        assert_eq!(format_progress(512 * 1024, 4_299_161), "0.5/4.1 MiB");
    }

    #[test]
    fn hosts_are_shown_without_credentials_or_paths() {
        for (url, host) in [
            (
                "https://github.com/owner/repo/archive/abc.tar.gz",
                "github.com",
            ),
            (
                "https://user:token@ghe.example:8443/owner/repo.git",
                "ghe.example:8443",
            ),
            ("git@ghe.example:stats/simlib.git", "ghe.example"),
            ("ssh://git@ghe.example/stats/simlib.git", "ghe.example"),
            ("file:///srv/git/repo", "the local filesystem"),
            ("/srv/git/repo", "the local filesystem"),
        ] {
            assert_eq!(display_host(url), host, "{url}");
        }
    }
}
