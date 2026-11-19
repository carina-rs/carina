//! Scoped observations of the production directory resolver. No raw enriched values escape.
//! Observed entries require `session = None` (asserted in the facade);
//! replay is never permitted to record checks through a live session.
use crate::{config_loader, parser::ProviderContext, paths::ResolvedPath, schema::SchemaRegistry};
use std::cell::RefCell;

pub const EVENTS: &[&str] = crate::parser::RESOLUTION_EVENTS;
pub const STORAGE_EVENTS: &[&str] = crate::parser::STORAGE_EVENTS;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageCounts(pub [usize; STORAGE_EVENTS.len()]);
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counts(pub [usize; EVENTS.len()]);
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
#[derive(Clone, Debug, PartialEq, Eq)]
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
    pub storage_counts: StorageCounts,
    pub oracle_storage_counts: StorageCounts,
    pub same_inputs: bool,
    pub same_roots: bool,
    pub same_error: bool,
}
/// Publication identity contains no values or parse IDs. The ordinal preserves
/// multiplicity and source order within each invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootPublication {
    pub invocation: usize,
    pub kind: InvocationKind,
    pub entry: &'static str,
    pub ordinal: usize,
    pub full: bool,
    pub input: InputObservation,
}
fn publications(roots: &[RootObservation]) -> Vec<RootPublication> {
    let mut ordinals = std::collections::BTreeMap::<usize, usize>::new();
    roots
        .iter()
        .map(|root| {
            let ordinal = ordinals.entry(root.invocation).or_default();
            *ordinal += 1;
            RootPublication {
                invocation: root.invocation,
                kind: root.invocation_kind,
                entry: root.entry,
                ordinal: *ordinal,
                full: root.full,
                input: root.input.clone(),
            }
        })
        .collect()
}
#[derive(Default, Debug)]
pub struct ProjectionReport {
    pub roots: Vec<RootObservation>,
    pub invocations: Vec<InvocationObservation>,
    pub plain_strict_succeeded: bool,
    pub plain_diagnostics_succeeded: bool,
    pub plain_publications: Vec<RootPublication>,
    pub strict_succeeded: bool,
    pub diagnostics_succeeded: bool,
    pub strict_error_matches: bool,
    pub diagnostics_error_matches: bool,
}
impl ProjectionReport {
    pub fn publications(&self) -> Vec<RootPublication> {
        publications(&self.roots)
    }
}
#[derive(Default)]
struct State {
    active: bool,
    plain_capture: bool,
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
    storage_counts: StorageCounts,
    oracle_storage_counts: StorageCounts,
    same_inputs: bool,
    same_roots: bool,
    same_error: bool,
}
thread_local! {static OBSERVER:RefCell<State>=RefCell::new(State::default());}
struct ObserverGuard(State);
impl ObserverGuard {
    fn install(plain_capture: bool) -> Self {
        Self(OBSERVER.with(|s| {
            s.replace(State {
                active: true,
                plain_capture,
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
pub(crate) fn is_plain_capture() -> bool {
    OBSERVER.with(|state| state.borrow().plain_capture)
}
pub(crate) fn count(event: usize) {
    OBSERVER.with(|s| {
        let mut s = s.borrow_mut();
        if !s.active {
            return;
        }
        let oracle = s.oracle;
        if let Some(frame) = &mut s.invocation {
            if event >= EVENTS.len() {
                let counts = if oracle {
                    &mut frame.oracle_storage_counts
                } else {
                    &mut frame.storage_counts
                };
                counts.0[event - EVENTS.len()] += 1;
            } else if oracle {
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
                storage_counts: StorageCounts::default(),
                oracle_storage_counts: StorageCounts::default(),
                same_inputs: false,
                same_roots: false,
                same_error: false,
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
                storage_counts: frame.storage_counts,
                oracle_storage_counts: frame.oracle_storage_counts,
                same_inputs: frame.same_inputs,
                same_roots: frame.same_roots,
                same_error: frame.same_error,
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
/// Compare safe, typed diagnostic identity and presentation without Debug-formatting values.
pub(crate) fn same_parse_error(
    a: &crate::parser::ParseError,
    b: &crate::parser::ParseError,
) -> bool {
    use crate::parser::ParseError;
    use std::error::Error;
    fn chain(error: &dyn Error) -> Vec<String> {
        let mut items = Vec::new();
        let mut next = Some(error);
        while let Some(error) = next {
            items.push(error.to_string());
            next = error.source();
        }
        items
    }
    if std::mem::discriminant(a) != std::mem::discriminant(b)
        || a.diagnostic_range() != b.diagnostic_range()
        || a.source_file() != b.source_file()
        || a.diagnostic_message() != b.diagnostic_message()
        || chain(a) != chain(b)
    {
        return false;
    }
    macro_rules! same_fields {
        ($variant:ident { $($field:ident),* $(,)? }) => {{
            let fields = ($($field,)*);
            matches!(b, ParseError::$variant { $($field,)* } if fields == ($($field,)*))
        }};
    }
    match a {
        ParseError::ForKey(diagnostic) => {
            matches!(b, ParseError::ForKey(other) if diagnostic == other)
        }
        ParseError::Located { path, error } => {
            if let ParseError::Located {
                path: other_path,
                error: other_error,
            } = b
            {
                path == other_path && same_parse_error(error, other_error)
            } else {
                false
            }
        }
        ParseError::AtElement(error) => {
            if let ParseError::AtElement(other) = b {
                error.location() == other.location()
                    && same_parse_error(error.detail(), other.detail())
            } else {
                false
            }
        }
        ParseError::DeferredForBodyTemplate {
            line,
            header,
            span,
            error,
        } => {
            if let ParseError::DeferredForBodyTemplate {
                line: other_line,
                header: other_header,
                span: other_span,
                error: other_error,
            } = b
            {
                (line, header, span) == (other_line, other_header, other_span)
                    && same_parse_error(error, other_error)
            } else {
                false
            }
        }
        ParseError::Evaluation { line, error } => same_fields!(Evaluation { line, error }),
        ParseError::UnavailableSecret(error) => {
            matches!(b, ParseError::UnavailableSecret(other) if error == other)
        }
        ParseError::Syntax(error) => {
            matches!(b, ParseError::Syntax(other) if std::ops::Deref::deref(error) == std::ops::Deref::deref(other))
        }
        ParseError::RequireOutsideArguments { span } => {
            same_fields!(RequireOutsideArguments { span })
        }
        ParseError::DeferredForBodyUnsupported(error) => {
            matches!(b, ParseError::DeferredForBodyUnsupported(other) if error == other)
        }
        ParseError::InterpolationInConstraintString {
            kind,
            line,
            column,
            span,
        } => same_fields!(InterpolationInConstraintString {
            kind,
            line,
            column,
            span
        }),
        ParseError::UnboundLoopRead { kind, line, span } => {
            same_fields!(UnboundLoopRead { kind, line, span })
        }
        ParseError::UnresolvedDeclarationType {
            token,
            line,
            message,
        } => same_fields!(UnresolvedDeclarationType {
            token,
            line,
            message
        }),
        ParseError::SecretDerivedBackendAttribute {
            backend_type,
            attribute,
            line,
            start_column,
            end_column,
        } => same_fields!(SecretDerivedBackendAttribute {
            backend_type,
            attribute,
            line,
            start_column,
            end_column
        }),
        ParseError::UndefinedIdentifier {
            name,
            line,
            span,
            suggestion,
            in_scope,
        } => same_fields!(UndefinedIdentifier {
            name,
            line,
            span,
            suggestion,
            in_scope
        }),
        ParseError::UndefinedDeferredIterable {
            name,
            line,
            span,
            suggestion,
            in_scope,
        } => same_fields!(UndefinedDeferredIterable {
            name,
            line,
            span,
            suggestion,
            in_scope
        }),
        ParseError::CyclicLetBinding { chain } => same_fields!(CyclicLetBinding { chain }),
        ParseError::RetainedEvidence(_)
        | ParseError::InsideSecret { line: _ }
        | ParseError::SecretInConstraint { line: _ }
        | ParseError::ValueWalk(_)
        | ParseError::ProtectedFile(_)
        | ParseError::InvalidExpression {
            line: _,
            message: _,
        }
        | ParseError::InvalidAuthoredExpression {
            line: _,
            diagnostic: _,
        }
        | ParseError::InvalidFloatLiteral { line: _, source: _ }
        | ParseError::IntegerArithmetic {
            line: _,
            column: _,
            end_column: _,
            message: _,
        }
        | ParseError::UnknownFunction {
            name: _,
            line: _,
            column: _,
        }
        | ParseError::ReservedResourceAttribute {
            attribute: _,
            line: _,
            column: _,
            end_line: _,
            end_column: _,
        }
        | ParseError::InterpolationNotAllowedInLiteralString { line: _ }
        | ParseError::InvalidResourceIdentity { line: _, error: _ }
        | ParseError::InvalidResourceType(_)
        | ParseError::DuplicateBinding { name: _, line: _ }
        | ParseError::InternalError {
            expected: _,
            context: _,
        }
        | ParseError::RecursiveFunction(_)
        | ParseError::UserFunctionError(_) => true,
    }
}

pub(crate) fn take_oracle_root_counts() -> Counts {
    OBSERVER.with(|s| {
        std::mem::take(
            &mut s
                .borrow_mut()
                .invocation
                .as_mut()
                .expect("invocation")
                .root_oracle_counts,
        )
    })
}
pub(crate) fn set_oracle_root_counts(counts: Counts) {
    OBSERVER.with(|s| {
        s.borrow_mut()
            .invocation
            .as_mut()
            .expect("invocation")
            .root_oracle_counts = counts
    });
}
pub(crate) fn finish_invocation(same_inputs: bool, same_roots: bool, same_error: bool) {
    OBSERVER.with(|s| {
        let mut s = s.borrow_mut();
        let frame = s.invocation.as_mut().expect("invocation");
        frame.same_inputs = same_inputs;
        frame.same_roots = same_roots;
        frame.same_error = same_error;
    });
}
fn same_configuration_failure(
    a: Option<&config_loader::ConfigurationLoadFailure>,
    b: Option<&config_loader::ConfigurationLoadFailure>,
) -> bool {
    use config_loader::ConfigurationLoadDiagnostic;
    match a {
        None => b.is_none(),
        Some(a) => {
            let Some(b) = b else {
                return false;
            };
            a.kind() == b.kind()
                && a.to_string() == b.to_string()
                && a.diagnostics().len() == b.diagnostics().len()
                && a.diagnostics()
                    .iter()
                    .zip(b.diagnostics())
                    .all(|(a, b)| match a {
                        ConfigurationLoadDiagnostic::Parse { file, error } => {
                            if let ConfigurationLoadDiagnostic::Parse {
                                file: other_file,
                                error: other_error,
                            } = b
                            {
                                file == other_file && same_parse_error(error, other_error)
                            } else {
                                false
                            }
                        }
                        ConfigurationLoadDiagnostic::Message { file, message } => {
                            if let ConfigurationLoadDiagnostic::Message {
                                file: other_file,
                                message: other_message,
                            } = b
                            {
                                file == other_file && message == other_message
                            } else {
                                false
                            }
                        }
                    })
        }
    }
}
fn same_directory_failure(
    a: Option<&config_loader::DirectoryParseFailure>,
    b: Option<&config_loader::DirectoryParseFailure>,
) -> bool {
    match a {
        None => b.is_none(),
        Some(a) => {
            let Some(b) = b else {
                return false;
            };
            a.to_string() == b.to_string()
                && a.source_file() == b.source_file()
                && match a.parse_error() {
                    None => b.parse_error().is_none(),
                    Some(error) => b
                        .parse_error()
                        .is_some_and(|other| same_parse_error(error, other)),
                }
                && a.element_diagnostic().map(|error| error.location())
                    == b.element_diagnostic().map(|error| error.location())
                && a.io_error()
                    .map(|error| (error.kind(), error.raw_os_error()))
                    == b.io_error()
                        .map(|error| (error.kind(), error.raw_os_error()))
        }
    }
}
/// Every invocation replays the original saved input, including failures and
/// shared plain backend/directive work. These entries use None sessions only.
/// Nested module fixture directories are loaded as roots, not expanded as modules.
pub fn compare_directory(
    path: &ResolvedPath,
    config: &ProviderContext,
    schemas: &SchemaRegistry,
) -> ProjectionReport {
    // A separate plain production run uses only an identity publication
    // collector: no enriched lowering, saved-input replay or value comparisons.
    let (plain_strict, plain_diagnostics, plain_publications) = {
        let _plain = ObserverGuard::install(true);
        OBSERVER.with(|s| s.borrow_mut().entry = "strict");
        let strict = config_loader::load_configuration_report(path, config, schemas).err();
        OBSERVER.with(|s| s.borrow_mut().entry = "diagnostics");
        let diagnostics =
            config_loader::parse_directory_with_overrides_and_diagnostics_with_schemas(
                path,
                config,
                &config_loader::SourceOverrides::new(),
                schemas,
            )
            .err();
        let roots = OBSERVER.with(|s| publications(&s.borrow().roots));
        (strict, diagnostics, roots)
    };
    let _guard = ObserverGuard::install(false);
    OBSERVER.with(|s| s.borrow_mut().entry = "strict");
    let strict_error = config_loader::load_configuration_report(path, config, schemas).err();
    OBSERVER.with(|s| s.borrow_mut().entry = "diagnostics");
    let diagnostics_error =
        config_loader::parse_directory_with_overrides_and_diagnostics_with_schemas(
            path,
            config,
            &config_loader::SourceOverrides::new(),
            schemas,
        )
        .err();
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
        plain_strict_succeeded: plain_strict.is_none(),
        plain_diagnostics_succeeded: plain_diagnostics.is_none(),
        plain_publications,
        strict_succeeded: strict_error.is_none(),
        diagnostics_succeeded: diagnostics_error.is_none(),
        strict_error_matches: same_configuration_failure(
            plain_strict.as_ref(),
            strict_error.as_ref(),
        ),
        diagnostics_error_matches: same_directory_failure(
            plain_diagnostics.as_ref(),
            diagnostics_error.as_ref(),
        ),
    }
}
