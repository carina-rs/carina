//! Progress UI shared by command paths.
//!
//! All `indicatif` ownership lives here. Callers report semantic progress
//! events through [`ProgressBackend`] and cannot accidentally select a draw
//! target or finish a spinner without the plain-output fallback.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use carina_core::effect::{Effect, EffectInstanceKey};
use carina_core::executor::ProgressInfo;
use carina_core::resource::{ModuleSource, ResourceId};
use colored::Colorize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::cursor::progress_mode;
use crate::error::AppError;

/// Whether ordinary command output may share stdout with progress text.
///
/// Callers must choose this at construction so machine-readable stdout can
/// never accidentally inherit the human-output fallback.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct RecordedProgressLines(Mutex<Vec<String>>);

#[cfg(test)]
impl PartialEq for RecordedProgressLines {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

#[cfg(test)]
impl Eq for RecordedProgressLines {}

#[cfg(test)]
impl RecordedProgressLines {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub(crate) fn snapshot(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProgressSink {
    Human,
    MachineStdout,
    #[cfg(test)]
    Recorded(&'static RecordedProgressLines),
}

impl ProgressSink {
    pub(crate) fn line(self, line: impl std::fmt::Display) {
        match self {
            Self::Human => println!("{line}"),
            Self::MachineStdout => eprintln!("{line}"),
            #[cfg(test)]
            Self::Recorded(lines) => lines.0.lock().unwrap().push(line.to_string()),
        }
    }

    pub(crate) fn blank_line(self) {
        self.line("");
    }
}

#[derive(Debug)]
pub(super) struct EffectProgressKey<S = ModuleSource> {
    resource_id: ResourceId,
    instance: EffectInstanceKey,
    variant: std::mem::Discriminant<Effect<S>>,
}

impl<S> Clone for EffectProgressKey<S> {
    fn clone(&self) -> Self {
        Self {
            resource_id: self.resource_id.clone(),
            instance: self.instance.clone(),
            variant: self.variant,
        }
    }
}

impl<S> PartialEq for EffectProgressKey<S> {
    fn eq(&self, other: &Self) -> bool {
        self.resource_id == other.resource_id
            && self.instance == other.instance
            && self.variant == other.variant
    }
}

impl<S> Eq for EffectProgressKey<S> {}

impl<S> Hash for EffectProgressKey<S> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.resource_id.hash(state);
        self.instance.hash(state);
        self.variant.hash(state);
    }
}

impl<S> EffectProgressKey<S> {
    pub(super) fn new(effect: &Effect<S>) -> Self {
        Self {
            resource_id: effect.resource_id().clone(),
            instance: effect.instance_key(),
            variant: std::mem::discriminant(effect),
        }
    }
}

enum ProgressBackendInner<K> {
    Tty {
        multi: MultiProgress,
        bars: Mutex<TtyBars<K>>,
        separator_writer: Mutex<Box<dyn Write + Send>>,
        bar_region_writer: Mutex<Box<dyn Write + Send>>,
    },
    Plain {
        _heartbeat: PlainHeartbeatOwner,
        state: Arc<PlainState<K>>,
    },
}

struct PlainState<K> {
    active: Mutex<PlainActive<K>>,
    interval: Duration,
    writer: Mutex<Box<dyn Write + Send>>,
    failure_writer: Option<Mutex<Box<dyn Write + Send>>>,
}

struct PlainActive<K> {
    entries: HashMap<K, PlainInFlight>,
    next_sequence: u64,
}

struct PlainInFlight {
    label: String,
    started: Instant,
    last_reported: Instant,
    self_reporting: bool,
    sequence: u64,
}

struct PlainHeartbeatOwner {
    stop_sender: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for PlainHeartbeatOwner {
    fn drop(&mut self) {
        drop(self.stop_sender.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reporting each entry after 30 seconds keeps multi-minute creates visibly
/// alive in CI while effects that finish sooner produce only an outcome line.
const PLAIN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

fn emit_heartbeats<K>(state: &PlainState<K>, now: Instant) {
    let mut active = state.active.lock().unwrap();
    let mut entries: Vec<_> = active
        .entries
        .values_mut()
        .filter(|entry| {
            let silence_limit = if entry.self_reporting {
                state.interval.saturating_mul(2)
            } else {
                state.interval
            };
            now.saturating_duration_since(entry.last_reported) >= silence_limit
        })
        .collect();
    if entries.is_empty() {
        return;
    }

    entries.sort_by(|left, right| {
        left.started
            .cmp(&right.started)
            .then_with(|| left.sequence.cmp(&right.sequence))
    });

    let mut writer = state.writer.lock().unwrap();
    for entry in entries {
        let elapsed = Duration::from_secs(now.saturating_duration_since(entry.started).as_secs());
        let _ = writeln!(
            writer,
            "  … Still running: {} [{} elapsed]",
            entry.label,
            format_heartbeat_duration(elapsed),
        );
        entry.last_reported = now;
    }
    let _ = writer.flush();
}

fn start_plain_heartbeat<K>(state: Arc<PlainState<K>>) -> PlainHeartbeatOwner
where
    K: Send + 'static,
{
    let (stop_sender, stop_receiver) = mpsc::channel();
    let tick = state.interval.min(Duration::from_secs(1));
    let thread = thread::Builder::new()
        .name("carina-plain-progress-heartbeat".to_string())
        .spawn(move || {
            while let Err(mpsc::RecvTimeoutError::Timeout) = stop_receiver.recv_timeout(tick) {
                emit_heartbeats(&state, Instant::now());
            }
        })
        .ok();

    PlainHeartbeatOwner {
        stop_sender: Some(stop_sender),
        thread,
    }
}

fn format_heartbeat_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

#[cfg(test)]
pub(crate) fn wait_for_output_contains(output: &Mutex<Vec<u8>>, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let bytes = output.lock().unwrap().clone();
        let rendered = String::from_utf8(bytes).unwrap();
        if rendered.contains(needle) {
            return;
        }
        // wall-clock-ok: hang guard for output from the real heartbeat thread.
        assert!(
            Instant::now() < deadline,
            "expected output was not emitted before the deadline; needle: {needle:?}; output: {rendered:?}",
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct TtyBars<K> {
    active: HashMap<K, ProgressBar>,
    bar_region_open: bool,
}

impl<K> Default for TtyBars<K> {
    fn default() -> Self {
        Self {
            active: HashMap::new(),
            bar_region_open: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NonFailureKind {
    Success,
    Partial,
    Skipped,
    Retrying,
    Unknown,
}

/// A progress outcome that cannot render as a failure.
///
/// The glyph is selected from the private kind instead of supplied by callers,
/// leaving [`FailedOutcome`] as the only way to construct a `✗` outcome.
pub(crate) struct NonFailureOutcome {
    kind: NonFailureKind,
    summary_line: String,
    detail_lines: Vec<String>,
}

impl NonFailureOutcome {
    pub(crate) fn success(summary_line: impl Into<String>) -> Self {
        Self::new(
            NonFailureKind::Success,
            summary_line,
            std::iter::empty::<String>(),
        )
    }

    pub(crate) fn partial<I, S>(summary_line: impl Into<String>, detail_lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::new(NonFailureKind::Partial, summary_line, detail_lines)
    }

    pub(crate) fn skipped(summary_line: impl Into<String>) -> Self {
        Self::new(
            NonFailureKind::Skipped,
            summary_line,
            std::iter::empty::<String>(),
        )
    }

    pub(crate) fn retrying(summary_line: impl Into<String>) -> Self {
        Self::new(
            NonFailureKind::Retrying,
            summary_line,
            std::iter::empty::<String>(),
        )
    }

    pub(crate) fn unknown(summary_line: impl Into<String>) -> Self {
        Self::new(
            NonFailureKind::Unknown,
            summary_line,
            std::iter::empty::<String>(),
        )
    }

    fn new<I, S>(kind: NonFailureKind, summary_line: impl Into<String>, detail_lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            kind,
            summary_line: summary_line.into(),
            detail_lines: detail_lines.into_iter().map(Into::into).collect(),
        }
    }

    fn rendered(&self) -> String {
        let glyph = match self.kind {
            NonFailureKind::Success => "✓".green(),
            NonFailureKind::Partial => "⚠".yellow(),
            NonFailureKind::Skipped => "⊘".yellow(),
            NonFailureKind::Retrying => "↻".yellow(),
            NonFailureKind::Unknown => "?".yellow(),
        };
        let mut rendered = format!("{glyph} {}", self.summary_line);
        for detail in &self.detail_lines {
            rendered.push_str(&format!("\n      {} {detail}", "→".yellow()));
        }
        rendered
    }
}

/// A failed progress outcome, always routed to stderr exactly once.
pub(crate) struct FailedOutcome {
    summary_line: String,
    detail_lines: Vec<String>,
}

impl FailedOutcome {
    pub(crate) fn new<I, S>(summary_line: impl Into<String>, detail_lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            summary_line: summary_line.into(),
            detail_lines: detail_lines.into_iter().map(Into::into).collect(),
        }
    }

    pub(crate) fn summary(summary_line: impl Into<String>) -> Self {
        Self::new(summary_line, std::iter::empty::<String>())
    }

    fn rendered(&self) -> String {
        let mut rendered = format!("{} {}", "✗".red(), self.summary_line);
        for detail in &self.detail_lines {
            rendered.push_str(&format!("\n      {} {}", "→".red(), detail.red()));
        }
        rendered
    }
}

#[derive(Clone, Copy)]
enum OutputRoute {
    Ordinary,
    Failure,
}

/// Keyed progress output rendered interactively on stderr or as plain lines.
///
/// `indicatif` 0.17's [`MultiProgress`] always renders to stderr by default,
/// so stderr is the stream whose terminal state selects interactive mode. When
/// stderr is redirected, human-facing commands put non-failure progress on
/// stdout while failures remain on stderr; commands whose stdout is
/// machine-readable keep every progress line on stderr.
pub(crate) struct ProgressBackend<K> {
    inner: ProgressBackendInner<K>,
}

impl<K> ProgressBackend<K>
where
    K: Eq + Hash + Send + 'static,
{
    /// Build the command progress backend from the stream indicatif renders to.
    pub(crate) fn new(sink: ProgressSink) -> Self {
        if progress_mode().is_tty() {
            match sink {
                ProgressSink::Human => {
                    Self::with_tty(MultiProgress::new(), io::stdout(), io::stderr())
                }
                ProgressSink::MachineStdout => {
                    Self::with_tty(MultiProgress::new(), io::stderr(), io::stderr())
                }
                #[cfg(test)]
                ProgressSink::Recorded(_) => {
                    Self::with_tty(MultiProgress::new(), io::sink(), io::sink())
                }
            }
        } else {
            match sink {
                ProgressSink::Human => Self::with_plain_writers(
                    io::stdout(),
                    Some(io::stderr()),
                    PLAIN_HEARTBEAT_INTERVAL,
                ),
                ProgressSink::MachineStdout => Self::with_plain_writers(
                    io::stderr(),
                    None::<io::Stderr>,
                    PLAIN_HEARTBEAT_INTERVAL,
                ),
                #[cfg(test)]
                ProgressSink::Recorded(_) => {
                    Self::with_plain_writers(io::sink(), None::<io::Sink>, PLAIN_HEARTBEAT_INTERVAL)
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn plain(writer: impl Write + Send + 'static) -> Self {
        Self::plain_with_interval(writer, PLAIN_HEARTBEAT_INTERVAL)
    }

    #[cfg(test)]
    pub(crate) fn plain_with_interval(
        writer: impl Write + Send + 'static,
        interval: Duration,
    ) -> Self {
        Self::with_plain_writers(writer, None::<io::Sink>, interval)
    }

    fn with_plain_writers(
        writer: impl Write + Send + 'static,
        failure_writer: Option<impl Write + Send + 'static>,
        interval: Duration,
    ) -> Self {
        let state = Arc::new(PlainState {
            active: Mutex::new(PlainActive {
                entries: HashMap::new(),
                next_sequence: 0,
            }),
            interval,
            writer: Mutex::new(Box::new(writer)),
            failure_writer: failure_writer
                .map(|writer| Mutex::new(Box::new(writer) as Box<dyn Write + Send>)),
        });
        let heartbeat = start_plain_heartbeat(Arc::clone(&state));
        Self {
            inner: ProgressBackendInner::Plain {
                _heartbeat: heartbeat,
                state,
            },
        }
    }
}

impl<K> ProgressBackend<K>
where
    K: Eq + Hash,
{
    fn with_tty(
        multi: MultiProgress,
        separator_writer: impl Write + Send + 'static,
        bar_region_writer: impl Write + Send + 'static,
    ) -> Self {
        Self {
            inner: ProgressBackendInner::Tty {
                multi,
                bars: Mutex::new(TtyBars::default()),
                separator_writer: Mutex::new(Box::new(separator_writer)),
                bar_region_writer: Mutex::new(Box::new(bar_region_writer)),
            },
        }
    }

    #[cfg(test)]
    pub(crate) fn tty(
        draw_target: indicatif::ProgressDrawTarget,
        separator_writer: impl Write + Send + 'static,
        bar_region_writer: impl Write + Send + 'static,
    ) -> Self {
        Self::with_tty(
            MultiProgress::with_draw_target(draw_target),
            separator_writer,
            bar_region_writer,
        )
    }

    #[cfg(test)]
    pub(crate) fn active_count(&self) -> usize {
        match &self.inner {
            ProgressBackendInner::Tty { bars, .. } => bars.lock().unwrap().active.len(),
            ProgressBackendInner::Plain { state, .. } => state.active.lock().unwrap().entries.len(),
        }
    }

    #[cfg(test)]
    pub(crate) fn plain_last_reported(&self, key: &K) -> Option<Instant> {
        let ProgressBackendInner::Plain { state, .. } = &self.inner else {
            return None;
        };
        state
            .active
            .lock()
            .unwrap()
            .entries
            .get(key)
            .map(|entry| entry.last_reported)
    }

    #[cfg(test)]
    pub(crate) fn emit_heartbeats_at(&self, now: Instant) {
        let ProgressBackendInner::Plain { state, .. } = &self.inner else {
            panic!("expected plain progress backend");
        };
        emit_heartbeats(state, now);
    }

    /// Start tracking progress for `key`.
    fn start(&self, key: K, label: impl Into<String>) {
        let label = label.into();
        match &self.inner {
            ProgressBackendInner::Tty { multi, bars, .. } => {
                let pb = multi.add(ProgressBar::new_spinner());
                pb.set_style(spinner_style());
                pb.set_message(label);
                pb.enable_steady_tick(Duration::from_millis(80));
                let mut bars = bars.lock().unwrap();
                bars.bar_region_open = true;
                if let Some(previous) = bars.active.insert(key, pb) {
                    previous.finish_and_clear();
                    multi.remove(&previous);
                }
            }
            ProgressBackendInner::Plain { state, .. } => {
                let started = Instant::now();
                let mut active = state.active.lock().unwrap();
                let sequence = active.next_sequence;
                active.next_sequence = active.next_sequence.wrapping_add(1);
                active.entries.insert(
                    key,
                    PlainInFlight {
                        label,
                        started,
                        last_reported: started,
                        self_reporting: false,
                        sequence,
                    },
                );
            }
        }
    }

    fn finish_rendered(&self, key: K, line: String, route: OutputRoute) {
        match &self.inner {
            ProgressBackendInner::Tty { multi, bars, .. } => {
                let mut bars = bars.lock().unwrap();
                if let Some(pb) = bars.active.remove(&key) {
                    pb.finish_and_clear();
                    multi.remove(&pb);
                }
                bars.bar_region_open = true;
                let _ = multi.println(format!("  {line}"));
            }
            ProgressBackendInner::Plain { state, .. } => {
                let mut active = state.active.lock().unwrap();
                active.entries.remove(&key);
                let target = match route {
                    OutputRoute::Ordinary => &state.writer,
                    OutputRoute::Failure => state.failure_writer.as_ref().unwrap_or(&state.writer),
                };
                let mut writer = target.lock().unwrap();
                let _ = writeln!(writer, "  {line}");
            }
        }
    }

    fn abandon(&self, key: &K) {
        match &self.inner {
            ProgressBackendInner::Tty { multi, bars, .. } => {
                let mut bars = bars.lock().unwrap();
                if let Some(pb) = bars.active.remove(key) {
                    pb.finish_and_clear();
                    multi.remove(&pb);
                }
            }
            ProgressBackendInner::Plain { state, .. } => {
                state.active.lock().unwrap().entries.remove(key);
            }
        }
    }

    /// Emit an informational line associated with an in-flight entry.
    ///
    /// In plain mode the line also postpones the entry's next generic
    /// heartbeat because the operation has just reported useful progress.
    pub(crate) fn in_flight_line(&self, key: &K, line: impl AsRef<str>) {
        self.in_flight_line_at(key, line, Instant::now());
    }

    fn in_flight_line_at(&self, key: &K, line: impl AsRef<str>, now: Instant) {
        match &self.inner {
            ProgressBackendInner::Tty { multi, bars, .. } => {
                let mut bars = bars.lock().unwrap();
                bars.bar_region_open = true;
                let _ = multi.println(format!("  {}", line.as_ref()));
            }
            ProgressBackendInner::Plain { state, .. } => {
                let mut active = state.active.lock().unwrap();
                if let Some(entry) = active.entries.get_mut(key) {
                    entry.last_reported = now;
                    entry.self_reporting = true;
                }
                let mut writer = state.writer.lock().unwrap();
                let _ = writeln!(writer, "  {}", line.as_ref());
            }
        }
    }

    /// Emit a non-outcome informational line that is not tied to a spinner.
    ///
    /// Interactive output is printed above the live spinner area.
    pub(crate) fn info_line(&self, line: impl AsRef<str>) {
        self.emit_rendered_line(line.as_ref(), OutputRoute::Ordinary);
    }

    /// Emit a typed non-failure outcome that is not tied to a spinner.
    pub(crate) fn outcome_line(&self, outcome: NonFailureOutcome) {
        self.emit_rendered_line(&outcome.rendered(), OutputRoute::Ordinary);
    }

    /// Emit a failed outcome that is not tied to a spinner.
    pub(crate) fn failed_line(&self, outcome: FailedOutcome) {
        self.emit_rendered_line(&outcome.rendered(), OutputRoute::Failure);
    }

    fn emit_rendered_line(&self, line: &str, route: OutputRoute) {
        match &self.inner {
            ProgressBackendInner::Tty { multi, bars, .. } => {
                let mut bars = bars.lock().unwrap();
                bars.bar_region_open = true;
                let _ = multi.println(format!("  {line}"));
            }
            ProgressBackendInner::Plain { state, .. } => {
                let target = match route {
                    OutputRoute::Ordinary => &state.writer,
                    OutputRoute::Failure => state.failure_writer.as_ref().unwrap_or(&state.writer),
                };
                let _ = writeln!(target.lock().unwrap(), "  {line}");
            }
        }
    }

    /// Emit a blank line and a column-zero section heading.
    pub(crate) fn section_header(&self, header: impl AsRef<str>) {
        match &self.inner {
            ProgressBackendInner::Tty { multi, bars, .. } => {
                let mut bars = bars.lock().unwrap();
                bars.bar_region_open = true;
                let _ = multi.println("");
                let _ = multi.println(header.as_ref());
            }
            ProgressBackendInner::Plain { state, .. } => {
                let mut writer = state.writer.lock().unwrap();
                let _ = writeln!(writer);
                let _ = writeln!(writer, "{}", header.as_ref());
            }
        }
    }

    /// Open a refresh section. Only [`RefreshPhase::start`] calls this so every
    /// open section has an owner that closes it on drop.
    fn open_refresh_phase(&self, header: impl AsRef<str>) {
        match &self.inner {
            ProgressBackendInner::Tty {
                separator_writer, ..
            } => {
                let mut writer = separator_writer.lock().unwrap();
                let _ = writeln!(writer, "{}", header.as_ref());
                let _ = writer.flush();
            }
            ProgressBackendInner::Plain { state, .. } => {
                let mut writer = state.writer.lock().unwrap();
                let _ = writeln!(writer, "{}", header.as_ref());
                let _ = writer.flush();
            }
        }
    }

    /// Record that an external stderr line terminated the interactive bar region.
    ///
    /// A later progress event reopens the region. Plain output has no live bar
    /// region and needs no corresponding state change.
    fn note_external_line(&self) {
        if let ProgressBackendInner::Tty { bars, .. } = &self.inner {
            bars.lock().unwrap().bar_region_open = false;
        }
    }

    /// Close one refresh phase with a blank separator line.
    fn close_refresh_phase(&self) {
        match &self.inner {
            ProgressBackendInner::Tty {
                bars,
                separator_writer,
                bar_region_writer,
                ..
            } => {
                let mut bars = bars.lock().unwrap();
                let bar_region_open = bars.bar_region_open;
                bars.bar_region_open = false;
                drop(bars);

                if bar_region_open {
                    let mut writer = bar_region_writer.lock().unwrap();
                    let _ = writeln!(writer);
                    let _ = writer.flush();
                }

                let mut writer = separator_writer.lock().unwrap();
                let _ = writeln!(writer);
                let _ = writer.flush();
            }
            ProgressBackendInner::Plain { state, .. } => {
                let mut writer = state.writer.lock().unwrap();
                let _ = writeln!(writer);
                let _ = writer.flush();
            }
        }
    }
}

impl<S> ProgressBackend<EffectProgressKey<S>> {
    /// Start a non-refresh effect spinner without exposing the generic spinner
    /// primitive to `ProgressBackend<ResourceId>` callers.
    pub(super) fn start_effect(&self, key: EffectProgressKey<S>, label: impl Into<String>) {
        self.start(key, label);
    }

    /// Finish an observer effect with a non-failure outcome.
    pub(super) fn finish(&self, key: EffectProgressKey<S>, outcome: NonFailureOutcome) {
        self.finish_rendered(key, outcome.rendered(), OutputRoute::Ordinary);
    }

    /// Finish an observer effect with a failure routed to stderr.
    pub(super) fn finish_failed(&self, key: EffectProgressKey<S>, outcome: FailedOutcome) {
        self.finish_rendered(key, outcome.rendered(), OutputRoute::Failure);
    }
}

impl ProgressBackend<usize> {
    /// Start an indexed non-refresh operation spinner.
    pub(crate) fn start_indexed(
        &self,
        key: usize,
        label: impl Into<String>,
    ) -> ProgressGuard<'_, usize> {
        self.start(key, label);
        ProgressGuard {
            backend: self,
            key: Some(key),
        }
    }
}

/// RAII owner for one keyed progress entry.
#[must_use = "keep the progress guard alive until the operation has finished"]
pub(crate) struct ProgressGuard<'backend, K>
where
    K: Eq + Hash,
{
    backend: &'backend ProgressBackend<K>,
    key: Option<K>,
}

impl<K> ProgressGuard<'_, K>
where
    K: Eq + Hash,
{
    pub(crate) fn finish(mut self, outcome: NonFailureOutcome) {
        let key = self.key.take().expect("progress key already taken");
        self.backend
            .finish_rendered(key, outcome.rendered(), OutputRoute::Ordinary);
    }

    pub(crate) fn finish_failed(mut self, outcome: FailedOutcome) {
        let key = self.key.take().expect("progress key already taken");
        self.backend
            .finish_rendered(key, outcome.rendered(), OutputRoute::Failure);
    }
}

impl<K> Drop for ProgressGuard<'_, K>
where
    K: Eq + Hash,
{
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            self.backend.abandon(&key);
        }
    }
}

/// RAII owner for one refresh section.
///
/// [`Self::begin`] is the only exposed constructor for [`RefreshProgress`].
/// Each returned progress handle borrows this phase, so a refresh spinner can
/// neither be started without a phase nor outlive the phase that owns it.
#[must_use = "keep the refresh phase guard alive until the phase has finished"]
pub(crate) struct RefreshPhase<'backend> {
    backend: &'backend ProgressBackend<ResourceId>,
    closed: bool,
}

impl<'backend> RefreshPhase<'backend> {
    /// Print the refresh header and open a structurally owned refresh phase.
    pub(crate) fn start(backend: &'backend ProgressBackend<ResourceId>) -> Self {
        backend.open_refresh_phase(format!("{}", "Refreshing state...".cyan()));
        Self {
            backend,
            closed: false,
        }
    }

    /// Begin tracking a resource read while this phase remains alive.
    pub(crate) fn begin<'phase>(
        &'phase self,
        id: &ResourceId,
    ) -> RefreshProgress<'phase, 'backend> {
        let key = id.clone();
        let label = id.human().to_string();
        self.backend.start(key.clone(), label.clone());
        RefreshProgress {
            _phase: self,
            progress: ProgressGuard {
                backend: self.backend,
                key: Some(key),
            },
            label,
            start: std::time::Instant::now(),
        }
    }

    /// Record that an external stderr line terminated the interactive bar region.
    pub(crate) fn note_external_line(&self) {
        self.backend.note_external_line();
    }

    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.backend.close_refresh_phase();
    }
}

impl Drop for RefreshPhase<'_> {
    fn drop(&mut self) {
        self.close();
    }
}

/// Format a duration as a human-readable string like "3.2s" or "1m 5.3s".
pub(crate) fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{:.1}s", secs)
    } else {
        let mins = secs as u64 / 60;
        let remaining = secs - (mins as f64 * 60.0);
        format!("{}m {:.1}s", mins, remaining)
    }
}

/// Braille spinner frames for animated progress display.
const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Create the spinner style used by both apply and destroy.
fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("  {spinner:.cyan} {msg}...")
        .unwrap()
        .tick_strings(SPINNER_FRAMES)
}

/// Spinner for tracking state refresh progress per resource.
///
/// Shows a spinner while each resource is being read, then displays timing
/// when done. Uses `indicatif` for animated terminal output with a shared
/// `MultiProgress` to support concurrent spinners.
pub(crate) struct RefreshProgress<'phase, 'backend> {
    _phase: &'phase RefreshPhase<'backend>,
    progress: ProgressGuard<'backend, ResourceId>,
    label: String,
    start: std::time::Instant,
}

impl RefreshProgress<'_, '_> {
    /// Finish the spinner with a success checkmark and elapsed time.
    pub fn finish(self) {
        let elapsed = self.start.elapsed();
        let timing = format!("took {}", format_duration(elapsed)).dimmed();
        let outcome = NonFailureOutcome::success(format!("{} {}", self.label, timing));
        self.progress.finish(outcome);
    }
}

/// Format a progress counter as a dimmed string like "1/10".
pub(crate) fn format_progress(progress: &ProgressInfo) -> String {
    format!("{}/{}", progress.completed, progress.total)
}

/// The confirmation prompt's "Enter a value: " has no trailing newline, and on
/// Ctrl+C the prompt interrupt can fire before any other path emits a newline
/// before the lock-release message.
pub(crate) fn emit_newline_on_interrupt<W: std::io::Write>(
    writer: &mut W,
    result: &Result<String, AppError>,
) {
    if matches!(result, Err(AppError::Interrupted)) {
        let _ = writeln!(writer);
        let _ = writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use indicatif::{ProgressDrawTarget, TermLike};

    use super::*;

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Debug, Default)]
    struct RecordingTerm(Arc<Mutex<Vec<u8>>>);

    impl RecordingTerm {
        fn push(&self, bytes: impl AsRef<[u8]>) {
            self.0.lock().unwrap().extend_from_slice(bytes.as_ref());
        }

        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl TermLike for RecordingTerm {
        fn width(&self) -> u16 {
            80
        }

        fn height(&self) -> u16 {
            20
        }

        fn move_cursor_up(&self, n: usize) -> io::Result<()> {
            self.push(format!("\x1b[{n}A"));
            Ok(())
        }

        fn move_cursor_down(&self, n: usize) -> io::Result<()> {
            self.push(format!("\x1b[{n}B"));
            Ok(())
        }

        fn move_cursor_right(&self, n: usize) -> io::Result<()> {
            self.push(format!("\x1b[{n}C"));
            Ok(())
        }

        fn move_cursor_left(&self, n: usize) -> io::Result<()> {
            self.push(format!("\x1b[{n}D"));
            Ok(())
        }

        fn write_line(&self, line: &str) -> io::Result<()> {
            self.push(line);
            self.push("\r\n");
            Ok(())
        }

        fn write_str(&self, text: &str) -> io::Result<()> {
            self.push(text);
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            self.push("\r\x1b[2K");
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn tty_backend_emits_finished_and_untied_lines_once() {
        let terminal = RecordingTerm::default();
        let backend: ProgressBackend<usize> = ProgressBackend::tty(
            ProgressDrawTarget::term_like(Box::new(terminal.clone())),
            io::sink(),
            io::sink(),
        );

        let progress = backend.start_indexed(1, "running");
        progress.finish(NonFailureOutcome::success("finished after start"));
        backend.outcome_line(NonFailureOutcome::success("finished without start"));
        backend.info_line("standalone event");

        let raw = String::from_utf8(terminal.bytes()).unwrap();
        for outcome in [
            "finished after start",
            "finished without start",
            "standalone event",
        ] {
            assert_eq!(
                raw.matches(outcome).count(),
                1,
                "expected one emitted {outcome:?}\nraw output:\n{raw}",
            );
        }
    }

    #[test]
    fn tty_backend_splits_bar_terminator_and_refresh_separator() {
        let terminal = RecordingTerm::default();
        let stdout = SharedWriter::default();
        let stderr = SharedWriter::default();
        let backend: ProgressBackend<usize> = ProgressBackend::tty(
            ProgressDrawTarget::term_like(Box::new(terminal)),
            stdout.clone(),
            stderr.clone(),
        );

        let progress = backend.start_indexed(1, "running");
        progress.finish(NonFailureOutcome::success("finished"));
        backend.close_refresh_phase();

        assert_eq!(stdout.0.lock().unwrap().as_slice(), b"\n");
        assert_eq!(stderr.0.lock().unwrap().as_slice(), b"\n");
    }

    #[test]
    fn tty_backend_skips_bar_terminator_without_rendered_progress() {
        let stdout = SharedWriter::default();
        let stderr = SharedWriter::default();
        let backend: ProgressBackend<usize> =
            ProgressBackend::tty(ProgressDrawTarget::hidden(), stdout.clone(), stderr.clone());

        backend.close_refresh_phase();

        assert_eq!(stdout.0.lock().unwrap().as_slice(), b"\n");
        assert!(stderr.0.lock().unwrap().is_empty());
    }

    #[test]
    fn tty_backend_skips_bar_terminator_after_external_line() {
        let stdout = SharedWriter::default();
        let stderr = SharedWriter::default();
        let backend: ProgressBackend<usize> =
            ProgressBackend::tty(ProgressDrawTarget::hidden(), stdout.clone(), stderr.clone());

        let progress = backend.start_indexed(1, "running");
        progress.finish(NonFailureOutcome::success("finished"));
        backend.note_external_line();
        backend.close_refresh_phase();

        assert_eq!(stdout.0.lock().unwrap().as_slice(), b"\n");
        assert!(stderr.0.lock().unwrap().is_empty());
    }

    #[test]
    fn refresh_phase_close_is_idempotent_within_the_phase() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::<ResourceId>::plain(output.clone());

        let mut phase = RefreshPhase::start(&backend);
        phase.close();
        phase.close();
        drop(phase);

        assert_eq!(
            output.0.lock().unwrap().as_slice(),
            b"Refreshing state...\n\n"
        );
    }

    #[test]
    fn refresh_progress_requires_a_live_phase() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::<ResourceId>::plain(output.clone());
        let id = ResourceId::with_identity("test.Resource", "example");

        {
            let phase = RefreshPhase::start(&backend);
            let _progress = phase.begin(&id);
        }

        assert_eq!(
            output.0.lock().unwrap().as_slice(),
            b"Refreshing state...\n\n"
        );
    }

    #[test]
    fn dropped_refresh_progress_is_abandoned_without_a_heartbeat() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::<ResourceId>::plain(output.clone());
        let id = ResourceId::with_identity("test.Resource", "abandoned");
        let phase = RefreshPhase::start(&backend);
        let progress = phase.begin(&id);

        assert_eq!(backend.active_count(), 1);
        drop(progress);
        assert_eq!(backend.active_count(), 0);

        let before_tick = output.0.lock().unwrap().clone();
        backend.emit_heartbeats_at(std::time::Instant::now() + Duration::from_secs(3_600));
        assert_eq!(*output.0.lock().unwrap(), before_tick);
    }

    #[test]
    fn dropped_indexed_progress_guard_abandons_without_output() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::plain(output.clone());

        let progress = backend.start_indexed(7, "Delete r.abandoned");
        assert_eq!(backend.active_count(), 1);
        drop(progress);

        assert_eq!(backend.active_count(), 0);
        backend.emit_heartbeats_at(Instant::now() + Duration::from_secs(3_600));
        assert!(output.0.lock().unwrap().is_empty());
    }

    #[test]
    fn each_refresh_phase_on_one_backend_closes_once() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::<ResourceId>::plain(output.clone());

        drop(RefreshPhase::start(&backend));
        drop(RefreshPhase::start(&backend));

        assert_eq!(
            output.0.lock().unwrap().as_slice(),
            concat!("Refreshing state...\n", "\n", "Refreshing state...\n", "\n",).as_bytes()
        );
    }

    #[test]
    fn plain_backend_emits_each_finished_or_untied_event_once() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::plain(output.clone());

        let progress = backend.start_indexed(1, "started but not emitted");
        progress.finish(NonFailureOutcome::success("finished after start"));
        backend.outcome_line(NonFailureOutcome::success("finished without start"));
        backend.info_line("standalone event");

        assert_eq!(
            String::from_utf8(output.0.lock().unwrap().clone()).unwrap(),
            concat!(
                "  ✓ finished after start\n",
                "  ✓ finished without start\n",
                "  standalone event\n",
            )
        );
    }

    #[test]
    fn plain_heartbeats_emit_in_start_order_and_stop_after_finish() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::plain(output.clone());

        let first = backend.start_indexed(1, "Create r.a");
        let second = backend.start_indexed(2, "Create r.b");
        let ProgressBackendInner::Plain { state, .. } = &backend.inner else {
            panic!("expected plain progress backend");
        };
        // Both fixture entries start at the same injected instant. Reading a
        // fresh wall clock here would include scheduler delays between starts.
        let origin = {
            let mut active = state.active.lock().unwrap();
            let origin = active.entries.get(&1).unwrap().started;
            for entry in active.entries.values_mut() {
                entry.started = origin;
                entry.last_reported = origin;
            }
            origin
        };
        let now = origin + Duration::from_secs(90);

        emit_heartbeats(state, now);
        let first_tick_len = output.0.lock().unwrap().len();
        assert_eq!(
            output.0.lock().unwrap().as_slice(),
            concat!(
                "  … Still running: Create r.a [1m 30s elapsed]\n",
                "  … Still running: Create r.b [1m 30s elapsed]\n",
            )
            .as_bytes()
        );

        first.finish(NonFailureOutcome::success("Create r.a"));
        emit_heartbeats(state, now + PLAIN_HEARTBEAT_INTERVAL);
        assert_eq!(
            &output.0.lock().unwrap()[first_tick_len..],
            concat!(
                "  ✓ Create r.a\n",
                "  … Still running: Create r.b [2m 0s elapsed]\n",
            )
            .as_bytes()
        );

        second.finish(NonFailureOutcome::success("Create r.b"));
        let before_empty_tick = output.0.lock().unwrap().clone();
        emit_heartbeats(state, now);
        assert_eq!(*output.0.lock().unwrap(), before_empty_tick);
    }

    #[test]
    fn plain_heartbeat_uses_each_entrys_own_interval() {
        let output = SharedWriter::default();
        let interval = Duration::from_secs(30);
        let backend = ProgressBackend::plain_with_interval(output.clone(), interval);
        let _progress = backend.start_indexed(1, "Create r.short");
        let ProgressBackendInner::Plain { state, .. } = &backend.inner else {
            panic!("expected plain progress backend");
        };
        let started = state
            .active
            .lock()
            .unwrap()
            .entries
            .get(&1)
            .unwrap()
            .started;

        emit_heartbeats(state, started + interval - Duration::from_millis(1));
        assert!(output.0.lock().unwrap().is_empty());

        emit_heartbeats(state, started + interval);
        assert_eq!(
            output.0.lock().unwrap().as_slice(),
            "  … Still running: Create r.short [30s elapsed]\n".as_bytes()
        );

        let after_first_heartbeat = output.0.lock().unwrap().clone();
        emit_heartbeats(state, started + interval);
        assert_eq!(*output.0.lock().unwrap(), after_first_heartbeat);
    }

    #[test]
    fn self_reporting_entry_waits_two_intervals_for_generic_fallback() {
        let output = SharedWriter::default();
        let interval = Duration::from_secs(30);
        let backend = ProgressBackend::plain_with_interval(output.clone(), interval);
        let _progress = backend.start_indexed(1, "Wait certificate");
        let previous_poll = backend.plain_last_reported(&1).unwrap();
        backend.in_flight_line_at(&1, "~ certificate: still pending", previous_poll);

        backend.emit_heartbeats_at(previous_poll + interval + Duration::from_millis(1_100));
        backend.in_flight_line_at(
            &1,
            "~ certificate: still pending again",
            previous_poll + interval + Duration::from_millis(1_200),
        );

        assert_eq!(
            output.0.lock().unwrap().as_slice(),
            concat!(
                "  ~ certificate: still pending\n",
                "  ~ certificate: still pending again\n",
            )
            .as_bytes(),
            "a slightly late self-report must arrive before the generic fallback",
        );
    }

    #[test]
    fn poll_line_pushes_back_next_generic_heartbeat() {
        let output = SharedWriter::default();
        let interval = Duration::from_secs(30);
        let backend = ProgressBackend::plain_with_interval(output.clone(), interval);
        let _progress = backend.start_indexed(1, "Wait certificate");
        let started = backend.plain_last_reported(&1).unwrap();

        backend.in_flight_line_at(
            &1,
            "~ certificate: still pending",
            started + Duration::from_secs(50),
        );
        output.0.lock().unwrap().clear();

        backend.emit_heartbeats_at(started + Duration::from_secs(61));
        assert!(output.0.lock().unwrap().is_empty());

        backend.emit_heartbeats_at(started + Duration::from_secs(110));
        assert_eq!(
            output.0.lock().unwrap().as_slice(),
            "  … Still running: Wait certificate [1m 50s elapsed]\n".as_bytes(),
        );
    }

    #[test]
    fn plain_heartbeat_thread_emits_before_outcome_without_ansi() {
        let output = SharedWriter::default();
        let backend =
            ProgressBackend::plain_with_interval(output.clone(), Duration::from_millis(10));

        let progress = backend.start_indexed(1, "Create r.slow");
        wait_for_output_contains(&output.0, "Still running: Create r.slow");

        progress.finish(NonFailureOutcome::success("Create r.slow"));
        drop(backend);
        let rendered = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        let last_heartbeat = rendered.rfind("Still running: Create r.slow").unwrap();
        let outcome = rendered.find("✓ Create r.slow").unwrap();
        assert!(
            last_heartbeat < outcome,
            "heartbeat must precede the outcome: {rendered:?}",
        );
        assert!(
            !rendered.contains('\x1b'),
            "plain heartbeat output must not contain ANSI escapes: {rendered:?}",
        );
    }

    #[test]
    fn plain_backend_routes_failures_only_to_the_failure_writer() {
        let ordinary = SharedWriter::default();
        let failures = SharedWriter::default();
        let backend = ProgressBackend::with_plain_writers(
            ordinary.clone(),
            Some(failures.clone()),
            PLAIN_HEARTBEAT_INTERVAL,
        );

        let successful = backend.start_indexed(1, "successful resource");
        let failed = backend.start_indexed(2, "failed resource");
        successful.finish(NonFailureOutcome::success("finished"));
        failed.finish_failed(FailedOutcome::new("failed resource", ["provider error"]));
        backend.failed_line(FailedOutcome::summary("untied failure"));

        assert_eq!(
            String::from_utf8(ordinary.0.lock().unwrap().clone()).unwrap(),
            "  ✓ finished\n"
        );
        assert_eq!(
            String::from_utf8(failures.0.lock().unwrap().clone()).unwrap(),
            concat!(
                "  ✗ failed resource\n",
                "      → provider error\n",
                "  ✗ untied failure\n",
            )
        );
    }

    #[test]
    fn plain_backend_does_not_rewrite_line_contents() {
        let output = SharedWriter::default();
        let backend = ProgressBackend::plain(output.clone());

        let progress = backend.start_indexed(1, "running");
        progress.finish(NonFailureOutcome::success("\x1b[31mfinished\x1b[0m"));
        backend.info_line("\x1b[32mstandalone\x1b[0m");

        assert_eq!(
            String::from_utf8(output.0.lock().unwrap().clone()).unwrap(),
            "  ✓ \x1b[31mfinished\x1b[0m\n  \x1b[32mstandalone\x1b[0m\n"
        );
    }
}
