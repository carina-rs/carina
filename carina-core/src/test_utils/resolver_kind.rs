//! Scoped observations of the production directory resolver. No raw enriched values escape.
//! The entries below use `session = None`; replay cannot double-record session checks.
use crate::{config_loader, parser::ProviderContext, paths::ResolvedPath, schema::SchemaRegistry};
use std::cell::RefCell;

const EVENTS: [&str; 8] = [
    "lookup",
    "integer_arithmetic",
    "interpolation",
    "user_function",
    "builtin",
    "builtin_success",
    "access",
    "take_present",
];
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counts(pub [usize; 8]);
impl Counts {
    pub fn get(&self, name: &str) -> usize {
        self.0[EVENTS
            .iter()
            .position(|event| *event == name)
            .expect("event name")]
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InvocationKind {
    PerFile,
    Merged,
}
#[derive(Clone, Debug)]
pub struct PathObservation {
    pub path: String,
    pub segmented: bool,
    pub conversion_depth: usize,
}
#[derive(Clone, Debug)]
pub struct InputObservation {
    pub shape: &'static str,
    pub direct_path: Option<String>,
    pub coalesce_left_deferred: bool,
}
#[derive(Clone, Debug)]
pub struct RootObservation {
    pub invocation: usize,
    pub invocation_kind: InvocationKind,
    pub entry: &'static str,
    pub full: bool,
    pub matches_plain: bool,
    pub paths: Vec<PathObservation>,
    pub input: InputObservation,
    pub counts: Counts,
    pub oracle_counts: Counts,
}
#[derive(Clone, Debug)]
pub struct InvocationObservation {
    pub ordinal: usize,
    pub kind: InvocationKind,
    pub entry: &'static str,
    pub counts: Counts,
    pub oracle_counts: Counts,
}
#[derive(Default, Debug)]
pub struct ProjectionReport {
    pub roots: Vec<RootObservation>,
    pub invocations: Vec<InvocationObservation>,
    pub strict_succeeded: bool,
    pub diagnostics_succeeded: bool,
}
#[derive(Default)]
struct State {
    active: bool,
    oracle: bool,
    per_file: bool,
    entry: &'static str,
    ordinal: usize,
    invocation: Option<Frame>,
    roots: Vec<RootObservation>,
    invocations: Vec<InvocationObservation>,
}
struct Frame {
    ordinal: usize,
    kind: InvocationKind,
    entry: &'static str,
    counts: Counts,
    oracle_counts: Counts,
    root_counts: Counts,
    root_oracle_counts: Counts,
}
thread_local! {static OBSERVER:RefCell<State>=RefCell::new(State::default());}
struct ObserverGuard(State);
impl ObserverGuard {
    fn install() -> Self {
        Self(OBSERVER.with(|s| {
            s.replace(State {
                active: true,
                ..State::default()
            })
        }))
    }
}
impl Drop for ObserverGuard {
    fn drop(&mut self) {
        OBSERVER.with(|s| {
            s.replace(std::mem::take(&mut self.0));
        });
    }
}
pub(crate) fn is_observing() -> bool {
    OBSERVER.with(|s| {
        let s = s.borrow();
        s.active && !s.oracle
    })
}
pub(crate) fn count(event: usize) {
    OBSERVER.with(|s| {
        let mut s = s.borrow_mut();
        if !s.active {
            return;
        }
        let oracle = s.oracle;
        if let Some(frame) = &mut s.invocation {
            if oracle {
                frame.oracle_counts.0[event] += 1;
                frame.root_oracle_counts.0[event] += 1;
            } else {
                frame.counts.0[event] += 1;
                frame.root_counts.0[event] += 1;
            }
        }
    });
}
pub(crate) struct PerFileGuard(bool);
impl PerFileGuard {
    pub(crate) fn enter() -> Self {
        Self(OBSERVER.with(|s| {
            let mut s = s.borrow_mut();
            let old = s.per_file;
            s.per_file = true;
            old
        }))
    }
}
impl Drop for PerFileGuard {
    fn drop(&mut self) {
        OBSERVER.with(|s| s.borrow_mut().per_file = self.0);
    }
}
pub(crate) struct InvocationGuard(Option<Frame>);
impl InvocationGuard {
    pub(crate) fn enter(merged: bool) -> Self {
        OBSERVER.with(|s| {
            let mut s = s.borrow_mut();
            s.ordinal += 1;
            let ordinal = s.ordinal;
            let entry = s.entry;
            let merged = merged && !s.per_file;
            let previous = s.invocation.replace(Frame {
                ordinal,
                entry,
                kind: if merged {
                    InvocationKind::Merged
                } else {
                    InvocationKind::PerFile
                },
                counts: Counts::default(),
                oracle_counts: Counts::default(),
                root_counts: Counts::default(),
                root_oracle_counts: Counts::default(),
            });
            Self(previous)
        })
    }
}
impl Drop for InvocationGuard {
    fn drop(&mut self) {
        OBSERVER.with(|s| {
            let mut s = s.borrow_mut();
            let frame = s.invocation.take().expect("root invocation");
            s.invocations.push(InvocationObservation {
                ordinal: frame.ordinal,
                kind: frame.kind,
                entry: frame.entry,
                counts: frame.counts,
                oracle_counts: frame.oracle_counts,
            });
            s.invocation = self.0.take();
        });
    }
}
pub(crate) struct OracleGuard(bool);
impl OracleGuard {
    pub(crate) fn enter() -> Self {
        Self(OBSERVER.with(|s| {
            let mut s = s.borrow_mut();
            let old = s.oracle;
            s.oracle = true;
            old
        }))
    }
}
impl Drop for OracleGuard {
    fn drop(&mut self) {
        OBSERVER.with(|s| s.borrow_mut().oracle = self.0);
    }
}
pub(crate) fn publish(
    matches_plain: bool,
    full: bool,
    paths: Vec<PathObservation>,
    input: InputObservation,
) {
    OBSERVER.with(|s| {
        let mut s = s.borrow_mut();
        let frame = s.invocation.as_mut().expect("root invocation");
        let invocation = frame.ordinal;
        let invocation_kind = frame.kind;
        let entry = frame.entry;
        let counts = std::mem::take(&mut frame.root_counts);
        let oracle_counts = std::mem::take(&mut frame.root_oracle_counts);
        s.roots.push(RootObservation {
            invocation,
            invocation_kind,
            entry,
            full,
            matches_plain,
            paths,
            input,
            counts,
            oracle_counts,
        });
    });
}
/// Compare successful roots against the plain instantiation on their original inputs.
/// The production strict and diagnostics loaders each resolve per-file inputs before merging.
pub fn compare_directory(
    path: &ResolvedPath,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> ProjectionReport {
    let _guard = ObserverGuard::install();
    OBSERVER.with(|s| s.borrow_mut().entry = "strict");
    let strict_succeeded = config_loader::load_configuration_report(path, config, schemas).is_ok();
    OBSERVER.with(|s| s.borrow_mut().entry = "diagnostics");
    let diagnostics_succeeded =
        config_loader::parse_directory_with_overrides_and_diagnostics_with_schemas(
            path,
            config,
            &config_loader::SourceOverrides::new(),
            schemas,
        )
        .is_ok();
    let (roots, invocations) = OBSERVER.with(|s| {
        let mut s = s.borrow_mut();
        (
            std::mem::take(&mut s.roots),
            std::mem::take(&mut s.invocations),
        )
    });
    ProjectionReport {
        roots,
        invocations,
        strict_succeeded,
        diagnostics_succeeded,
    }
}
