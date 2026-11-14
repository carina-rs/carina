//! Module resolution error type.

use crate::parser::{ModuleCallId, ParseError};
use crate::validation::typecheck::TypeExprValueError;
use std::path::{Path, PathBuf};

use super::validation::ModuleConstraintDiagnostic;

/// Two or more resolver failures retained by a non-collecting API.
///
/// The field is private and construction is centralized in
/// [`ModuleError::from_errors`], so `ModuleError::Multiple` cannot represent
/// an empty or needlessly singleton failure.
#[derive(Debug)]
pub struct MultipleModuleErrors(Vec<ModuleError>);

impl MultipleModuleErrors {
    pub(crate) fn iter(&self) -> impl Iterator<Item = &ModuleError> {
        self.0.iter()
    }

    pub(crate) fn into_vec(self) -> Vec<ModuleError> {
        self.0
    }
}

impl std::fmt::Display for MultipleModuleErrors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n")?;
            }
            write!(formatter, "{error}")?;
        }
        Ok(())
    }
}

/// Source ownership attached to a resolver error.
#[derive(Debug, Clone, Copy)]
pub enum ModuleErrorCallSource<'a> {
    /// One concrete expansion failed.
    Failing(&'a ModuleCallId),
    /// An imported graph failed before one of its calls could be singled out.
    Owners(&'a [ModuleCallId]),
    /// The error has no meaningful source-local module-call anchor.
    Unavailable,
}

/// One semantic leaf with the outermost call ownership retained during
/// decomposition. Removing a multiple-error container cannot erase its anchor.
pub struct ModuleErrorDiagnostic<'a> {
    pub error: &'a ModuleError,
    pub call_source: ModuleErrorCallSource<'a>,
}

/// Presentation preserves evidence as a typed continuation until the caller
/// chooses a human note or editor related information.
#[derive(Debug, Clone)]
pub enum DiagnosticLine {
    Message(String),
    RootEvidence(crate::config_loader::RootEvidence),
    ModuleEvidence(crate::config_loader::ModuleEvidence),
}

fn require_outside_module_message(
    finding: &crate::parser::Located<super::validation::RequireOutsideModuleDiagnostic>,
    detail: impl std::fmt::Display,
) -> String {
    format!(
        "{}:{}:{}: {detail}",
        crate::paths::DisplayPath::for_diagnostics(finding.location().file().path()),
        finding.location().span().start_line(),
        finding.location().span().start_column()
    )
}

/// Module resolution error
#[derive(Debug, thiserror::Error)]
pub enum ModuleError {
    #[error(transparent)]
    AnalysisInvariant(#[from] crate::parser::AnalysisTreeError),
    /// Terminal providerless failure together with every unresolved type site.
    #[error("{error}\n{}", render_provider_type_warnings(.warnings))]
    WithProviderTypeWarnings {
        error: Box<ModuleError>,
        warnings: Vec<crate::parser::ParseWarning>,
    },
    #[error("{0}")]
    Multiple(MultipleModuleErrors),

    /// Resolver failure owned by concrete call sites in the containing
    /// configuration. Nested wrappers preserve the full import chain; editor
    /// diagnostics anchor at the outermost source identities.
    #[error("{error}")]
    OwnedByCalls {
        calls: Vec<ModuleCallId>,
        error: Box<ModuleError>,
    },

    /// Failure raised while expanding one concrete call site.
    #[error("{error}")]
    AtCall {
        call: ModuleCallId,
        error: Box<ModuleError>,
    },

    /// The prohibition and its directory evidence are one semantic diagnostic.
    #[error("{error}\nNote: {}", root.note())]
    WithRootEvidence {
        error: Box<ModuleError>,
        root: crate::config_loader::RootEvidence,
    },

    #[error("No .crn files found in {path}")]
    NoConfigurationFiles { path: crate::paths::DisplayPath },

    #[error("Module not found: {0}")]
    NotFound(String),

    #[error("Circular import detected: {0}")]
    CircularImport(String),

    #[error("Missing required argument '{argument}' for module '{module}'")]
    MissingArgument { module: String, argument: String },

    #[error("Invalid argument type for '{argument}' in module '{module}': {mismatch}")]
    InvalidArgumentType {
        module: String,
        argument: String,
        mismatch: Box<TypeExprValueError>,
    },

    #[error(
        "module call '{module}' argument '{argument}' in {source_path} at line {line}: {message}"
    )]
    BareStructuralHandleArgument {
        module: String,
        argument: String,
        source_path: String,
        line: usize,
        message: String,
    },

    /// Expansion diagnostics require a parser-owned location immediately.
    ///
    /// ```compile_fail,E0308
    /// use carina_core::module_resolver::ModuleError;
    /// let _ = ModuleError::ArgumentValueAccess {
    ///     module: "child".into(), source_path: "sibling.crn".into(), line: 1,
    ///     diagnostic: Box::new(None),
    /// };
    /// ```
    #[error("module '{module}' ({source_path}) at line {line}: {}", .diagnostic.detail())]
    ArgumentValueAccess {
        module: String,
        source_path: String,
        line: usize,
        diagnostic: Box<crate::parser::Located<crate::resolver::ResolutionError>>,
    },

    #[error(
        "binding '{binding}' is defined only in a dropped branch of condition `{condition}` in {source_path} at line {line}"
    )]
    DroppedConditionalBinding {
        binding: String,
        condition: String,
        source_path: PathBuf,
        line: usize,
    },

    #[error(
        "cannot publish a reusable module template with an unresolved argument-dependent `if` at {context}; instantiate the module with concrete arguments before plan or apply"
    )]
    UnexpandedModuleTemplate { context: String },

    #[error("Reference type validation failed: {0}")]
    ReferenceType(crate::validation::ExpansionBoundaryReferenceErrors),

    // An I/O error is a terminal leaf here. Keep its diagnostic in Display
    // and do not expose it a second time through Error::source.
    #[error("IO error: {0}")]
    Io(std::io::Error),

    #[error("Parse error: {0}")]
    Parse(ParseError),

    #[error("Parse error in {path}: {error}")]
    ParseInFile {
        path: PathBuf,
        error: Box<ParseError>,
    },

    /// Declaration type resolution remains typed through recursive module
    /// loading so the CLI/LSP finding can cover this resolver backstop without
    /// comparing rendered parse-error text.
    #[error(transparent)]
    TypeResolution(Box<crate::parser::Located<crate::validation::TypeResolutionDiagnostic>>),

    #[error("Unknown argument '{argument}' for module '{module}'")]
    UnknownArgument { module: String, argument: String },

    #[error("Unknown module: {0}")]
    UnknownModule(String),

    #[error(
        "provider blocks are not allowed inside modules. Define providers at the root configuration level."
    )]
    ProviderInModule,

    #[error(
        "state blocks (moved, removed, and import) are not allowed inside modules. Define state blocks at the root configuration level."
    )]
    StateBlockInModule,

    #[error(
        "backend blocks are not allowed inside modules. Define the backend at the root configuration level."
    )]
    BackendInModule,

    #[error(
        "upstream_state declarations are not allowed inside modules. Define upstream_state declarations at the root configuration level."
    )]
    UpstreamStateInModule,

    #[error(
        "exports blocks are not allowed inside modules. Define exports at the root configuration level."
    )]
    ExportsInModule,

    #[error(transparent)]
    Constraint(Box<ModuleConstraintDiagnostic>),

    #[error("{}", require_outside_module_message(.0, .0.detail()))]
    RequireOutsideModule(
        Box<crate::parser::Located<super::validation::RequireOutsideModuleDiagnostic>>,
    ),

    #[error(
        "Module path '{path}' must be a directory. Single-file modules are not supported; put the module's .crn files in a directory and import the directory."
    )]
    NotADirectory { path: String },
}

impl From<std::io::Error> for ModuleError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ParseError> for ModuleError {
    fn from(error: ParseError) -> Self {
        Self::Parse(error)
    }
}

impl ModuleError {
    pub(crate) fn with_root_evidence(self, root: crate::config_loader::RootEvidence) -> Self {
        Self::WithRootEvidence {
            error: Box::new(self),
            root,
        }
    }

    /// Decompose only evidence-bearing import rejections. Other resolver
    /// failures keep their established grouping and source rendering.
    pub(crate) fn into_root_evidence_errors(self) -> Vec<Self> {
        if self.root_evidence().is_none() {
            return vec![self];
        }
        match self {
            Self::Multiple(errors) => errors
                .into_vec()
                .into_iter()
                .flat_map(Self::into_root_evidence_errors)
                .collect(),
            Self::OwnedByCalls { calls, error } => error
                .into_root_evidence_errors()
                .into_iter()
                .map(|error| error.with_owning_calls(calls.clone()))
                .collect(),
            Self::AtCall { call, error } => error
                .into_root_evidence_errors()
                .into_iter()
                .map(|error| error.with_failing_call(call.clone()))
                .collect(),
            // Every other variant is retained, including future diagnostic kinds.
            error => vec![error],
        }
    }

    pub fn root_evidence(&self) -> Option<&crate::config_loader::RootEvidence> {
        match self {
            Self::WithRootEvidence { error: _, root } => Some(root),
            Self::OwnedByCalls { calls: _, error }
            | Self::AtCall { call: _, error }
            | Self::WithProviderTypeWarnings { error, warnings: _ } => error.root_evidence(),
            Self::Multiple(errors) => errors.iter().find_map(Self::root_evidence),
            Self::AnalysisInvariant(_)
            | Self::NoConfigurationFiles { path: _ }
            | Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument {
                module: _,
                argument: _,
            }
            | Self::InvalidArgumentType {
                module: _,
                argument: _,
                mismatch: _,
            }
            | Self::BareStructuralHandleArgument {
                module: _,
                argument: _,
                source_path: _,
                line: _,
                message: _,
            }
            | Self::ArgumentValueAccess {
                module: _,
                source_path: _,
                line: _,
                diagnostic: _,
            }
            | Self::DroppedConditionalBinding {
                binding: _,
                condition: _,
                source_path: _,
                line: _,
            }
            | Self::UnexpandedModuleTemplate { context: _ }
            | Self::ReferenceType(_)
            | Self::Io(_)
            | Self::Parse(_)
            | Self::ParseInFile { path: _, error: _ }
            | Self::TypeResolution(_)
            | Self::UnknownArgument {
                module: _,
                argument: _,
            }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::RequireOutsideModule(_)
            | Self::NotADirectory { path: _ } => None,
        }
    }

    pub fn lsp_message_relative_to(&self, root: &Path) -> String {
        // Preserve established renderers for diagnostics without evidence,
        // including deferred-body call-site ownership.
        if self.root_evidence().is_none() {
            return self.display_relative_to(root);
        }
        match self {
            Self::WithRootEvidence { error, root: _ }
            | Self::OwnedByCalls { calls: _, error }
            | Self::AtCall { call: _, error } => error.lsp_message_relative_to(root),
            Self::Multiple(errors) => errors
                .iter()
                .map(|error| error.lsp_message_relative_to(root))
                .collect::<Vec<_>>()
                .join("\n"),
            Self::WithProviderTypeWarnings { error, warnings } => format!(
                "{}\n{}",
                error.lsp_message_relative_to(root),
                render_provider_type_warnings(warnings),
            ),
            Self::AnalysisInvariant(_)
            | Self::NoConfigurationFiles { path: _ }
            | Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument {
                module: _,
                argument: _,
            }
            | Self::InvalidArgumentType {
                module: _,
                argument: _,
                mismatch: _,
            }
            | Self::BareStructuralHandleArgument {
                module: _,
                argument: _,
                source_path: _,
                line: _,
                message: _,
            }
            | Self::ArgumentValueAccess {
                module: _,
                source_path: _,
                line: _,
                diagnostic: _,
            }
            | Self::DroppedConditionalBinding {
                binding: _,
                condition: _,
                source_path: _,
                line: _,
            }
            | Self::UnexpandedModuleTemplate { context: _ }
            | Self::ReferenceType(_)
            | Self::Io(_)
            | Self::Parse(_)
            | Self::ParseInFile { path: _, error: _ }
            | Self::TypeResolution(_)
            | Self::UnknownArgument {
                module: _,
                argument: _,
            }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::RequireOutsideModule(_)
            | Self::NotADirectory { path: _ } => self.display_relative_to(root),
        }
    }

    pub fn diagnostic_lines_relative_to(&self, root: Option<&Path>) -> Vec<DiagnosticLine> {
        let plain = || {
            DiagnosticLine::Message(
                root.map_or_else(|| self.to_string(), |root| self.display_relative_to(root)),
            )
        };
        if !self.individual_errors().iter().any(|finding| {
            finding.error.root_evidence().is_some()
                || finding.error.require_outside_module().is_some()
        }) {
            return vec![plain()];
        }
        match self {
            Self::WithRootEvidence {
                error,
                root: evidence,
            } => {
                let mut lines = error.diagnostic_lines_relative_to(root);
                lines.push(DiagnosticLine::RootEvidence(evidence.clone()));
                lines
            }
            Self::RequireOutsideModule(finding) => {
                let message =
                    require_outside_module_message(finding, finding.detail().lsp_message());
                vec![
                    DiagnosticLine::Message(message),
                    DiagnosticLine::RootEvidence(finding.detail().root_evidence().clone()),
                ]
            }
            Self::Multiple(errors) => errors
                .iter()
                .flat_map(|error| error.diagnostic_lines_relative_to(root))
                .collect(),
            Self::OwnedByCalls { calls: _, error } | Self::AtCall { call: _, error } => {
                error.diagnostic_lines_relative_to(root)
            }
            Self::WithProviderTypeWarnings { error, warnings } => {
                let mut lines = error.diagnostic_lines_relative_to(root);
                lines.push(DiagnosticLine::Message(render_provider_type_warnings(
                    warnings,
                )));
                lines
            }
            Self::AnalysisInvariant(_)
            | Self::NoConfigurationFiles { path: _ }
            | Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument {
                module: _,
                argument: _,
            }
            | Self::InvalidArgumentType {
                module: _,
                argument: _,
                mismatch: _,
            }
            | Self::BareStructuralHandleArgument {
                module: _,
                argument: _,
                source_path: _,
                line: _,
                message: _,
            }
            | Self::ArgumentValueAccess {
                module: _,
                source_path: _,
                line: _,
                diagnostic: _,
            }
            | Self::DroppedConditionalBinding {
                binding: _,
                condition: _,
                source_path: _,
                line: _,
            }
            | Self::UnexpandedModuleTemplate { context: _ }
            | Self::ReferenceType(_)
            | Self::Io(_)
            | Self::Parse(_)
            | Self::ParseInFile { path: _, error: _ }
            | Self::TypeResolution(_)
            | Self::UnknownArgument {
                module: _,
                argument: _,
            }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::NotADirectory { path: _ } => vec![plain()],
        }
    }

    /// Authored require rejected outside a module and its location, without exposing the diagnostic
    /// already included in this error's Display as another source-chain link.
    pub fn require_outside_module(
        &self,
    ) -> Option<&crate::parser::Located<super::validation::RequireOutsideModuleDiagnostic>> {
        match self.root_cause() {
            Self::RequireOutsideModule(finding) => Some(finding),
            _ => None,
        }
    }

    pub(crate) fn with_provider_type_warnings(
        self,
        warnings: Vec<crate::parser::ParseWarning>,
    ) -> Self {
        let existing = self.provider_type_warnings();
        let warnings = warnings
            .into_iter()
            .filter(|warning| !existing.contains(&warning))
            .collect::<Vec<_>>();
        if warnings.is_empty() {
            return self;
        }
        let (error, mut all) = match self {
            Self::WithProviderTypeWarnings { error, warnings } => (error, warnings),
            error => (Box::new(error), Vec::new()),
        };
        for warning in warnings {
            if !all.contains(&warning) {
                all.push(warning);
            }
        }
        Self::WithProviderTypeWarnings {
            error,
            warnings: all,
        }
    }

    pub fn provider_type_warnings(&self) -> Vec<&crate::parser::ParseWarning> {
        match self {
            Self::WithProviderTypeWarnings { warnings, error } => warnings
                .iter()
                .chain(error.provider_type_warnings())
                .collect(),
            Self::OwnedByCalls { error, .. }
            | Self::AtCall { error, .. }
            | Self::WithRootEvidence { error, .. } => error.provider_type_warnings(),
            Self::Multiple(errors) => errors
                .iter()
                .flat_map(Self::provider_type_warnings)
                .collect(),
            _ => Vec::new(),
        }
    }
    pub(crate) fn from_errors(errors: Vec<Self>) -> Option<Self> {
        let mut errors = errors.into_iter();
        let first = errors.next()?;
        let rest = errors.collect::<Vec<_>>();
        if rest.is_empty() {
            Some(first)
        } else {
            Some(Self::Multiple(MultipleModuleErrors(
                std::iter::once(first).chain(rest).collect(),
            )))
        }
    }

    pub(crate) fn with_owning_calls(self, calls: Vec<ModuleCallId>) -> Self {
        if calls.is_empty() {
            self
        } else {
            Self::OwnedByCalls {
                calls,
                error: Box::new(self),
            }
        }
    }

    pub(crate) fn with_failing_call(self, call: ModuleCallId) -> Self {
        Self::AtCall {
            call,
            error: Box::new(self),
        }
    }

    /// Outermost typed source ownership for this failure.
    pub fn call_source(&self) -> ModuleErrorCallSource<'_> {
        match self {
            Self::WithProviderTypeWarnings { error, .. } | Self::WithRootEvidence { error, .. } => {
                error.call_source()
            }
            Self::Multiple(_errors) => ModuleErrorCallSource::Unavailable,
            Self::OwnedByCalls { calls, .. } => ModuleErrorCallSource::Owners(calls),
            Self::AtCall { call, .. } => ModuleErrorCallSource::Failing(call),
            Self::NoConfigurationFiles { .. }
            | Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument { .. }
            | Self::InvalidArgumentType { .. }
            | Self::BareStructuralHandleArgument { .. }
            | Self::ArgumentValueAccess { .. }
            | Self::DroppedConditionalBinding { .. }
            | Self::UnexpandedModuleTemplate { .. }
            | Self::ReferenceType(_)
            | Self::AnalysisInvariant(_)
            | Self::TypeResolution(_)
            | Self::Io(_)
            | Self::Parse(_)
            | Self::ParseInFile { .. }
            | Self::UnknownArgument { .. }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::RequireOutsideModule(_)
            | Self::NotADirectory { .. } => ModuleErrorCallSource::Unavailable,
        }
    }

    pub(crate) fn failing_call(&self) -> Option<&ModuleCallId> {
        match self {
            Self::WithProviderTypeWarnings { error, .. } | Self::WithRootEvidence { error, .. } => {
                error.failing_call()
            }
            Self::Multiple(_errors) => None,
            Self::OwnedByCalls { calls: _, error } => error.failing_call(),
            Self::AtCall { call, error } => error.failing_call().or(Some(call)),
            Self::NoConfigurationFiles { .. }
            | Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument {
                module: _,
                argument: _,
            }
            | Self::InvalidArgumentType {
                module: _,
                argument: _,
                mismatch: _,
            }
            | Self::BareStructuralHandleArgument {
                module: _,
                argument: _,
                source_path: _,
                line: _,
                message: _,
            }
            | Self::ArgumentValueAccess {
                module: _,
                source_path: _,
                line: _,
                diagnostic: _,
            }
            | Self::DroppedConditionalBinding {
                binding: _,
                condition: _,
                source_path: _,
                line: _,
            }
            | Self::UnexpandedModuleTemplate { context: _ }
            | Self::ReferenceType(_)
            | Self::AnalysisInvariant(_)
            | Self::TypeResolution(_)
            | Self::Io(_)
            | Self::Parse(_)
            | Self::ParseInFile { path: _, error: _ }
            | Self::UnknownArgument {
                module: _,
                argument: _,
            }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::RequireOutsideModule(_)
            | Self::NotADirectory { path: _ } => None,
        }
    }

    /// Individual semantic failures before any terminal newline joining.
    pub fn individual_errors(&self) -> Vec<ModuleErrorDiagnostic<'_>> {
        fn collect<'a>(
            error: &'a ModuleError,
            owners: ModuleErrorCallSource<'a>,
            findings: &mut Vec<ModuleErrorDiagnostic<'a>>,
        ) {
            let owners = if matches!(owners, ModuleErrorCallSource::Unavailable) {
                error.call_source()
            } else {
                owners
            };
            match error {
                ModuleError::Multiple(errors) => {
                    for error in errors.iter() {
                        collect(error, owners, findings);
                    }
                }
                ModuleError::OwnedByCalls { error, .. }
                | ModuleError::AtCall { error, .. }
                | ModuleError::WithProviderTypeWarnings { error, .. } => {
                    collect(error, owners, findings)
                }
                error => findings.push(ModuleErrorDiagnostic {
                    error,
                    call_source: owners,
                }),
            }
        }
        let mut findings = Vec::new();
        collect(self, ModuleErrorCallSource::Unavailable, &mut findings);
        findings
    }

    /// Semantic leaf after removing typed import ownership wrappers.
    pub fn root_cause(&self) -> &Self {
        match self {
            Self::OwnedByCalls { error, .. }
            | Self::AtCall { error, .. }
            | Self::WithProviderTypeWarnings { error, .. }
            | Self::WithRootEvidence { error, .. } => error.root_cause(),
            error @ (Self::Multiple(_)
            | Self::NoConfigurationFiles { .. }
            | Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument { .. }
            | Self::InvalidArgumentType { .. }
            | Self::BareStructuralHandleArgument { .. }
            | Self::ArgumentValueAccess { .. }
            | Self::DroppedConditionalBinding { .. }
            | Self::UnexpandedModuleTemplate { .. }
            | Self::ReferenceType(_)
            | Self::AnalysisInvariant(_)
            | Self::TypeResolution(_)
            | Self::Io(_)
            | Self::Parse(_)
            | Self::ParseInFile { .. }
            | Self::UnknownArgument { .. }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::RequireOutsideModule(_)
            | Self::NotADirectory { .. }) => error,
        }
    }

    /// Render paths owned by an imported module relative to the root
    /// configuration directory. The resolver may receive an absolute root
    /// (notably from the LSP) or `.` (from the CLI); diagnostics should expose
    /// the same stable, project-relative path in either case.
    pub fn display_relative_to(&self, root: &Path) -> String {
        match self {
            Self::WithRootEvidence {
                error,
                root: evidence,
            } => format!(
                "{}\nNote: {}",
                error.display_relative_to(root),
                evidence.note()
            ),
            Self::WithProviderTypeWarnings { error, warnings } => format!(
                "{}\n{}",
                error.display_relative_to(root),
                render_provider_type_warnings(warnings)
            ),
            Self::Multiple(errors) => errors
                .iter()
                .map(|error| error.display_relative_to(root))
                .collect::<Vec<_>>()
                .join("\n"),
            Self::AtCall { call, error } if error.is_deferred_for_body_rejection() => format!(
                "module call in {} at line {}: {}",
                display_source_path_relative_to(call.source_file(), root),
                call.call_span().start_line(),
                error.display_relative_to(root),
            ),
            Self::OwnedByCalls { calls, error }
                if !calls.is_empty() && error.is_deferred_for_body_rejection() =>
            {
                let owners = calls
                    .iter()
                    .map(|call| {
                        format!(
                            "{} at line {}",
                            display_source_path_relative_to(call.source_file(), root),
                            call.call_span().start_line(),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    "module call in {owners}: {}",
                    error.display_relative_to(root)
                )
            }
            Self::OwnedByCalls { calls: _, error } | Self::AtCall { call: _, error } => {
                error.display_relative_to(root)
            }
            Self::BareStructuralHandleArgument {
                module,
                argument,
                source_path,
                line,
                message,
            } => format!(
                "module call '{module}' argument '{argument}' in {} at line {line}: {message}",
                display_source_path_relative_to(Path::new(source_path), root),
            ),
            Self::ArgumentValueAccess {
                module,
                source_path,
                line,
                diagnostic,
            } => format!(
                "module '{module}' ({}) at line {line}: {}",
                display_source_path_relative_to(Path::new(source_path), root),
                diagnostic.detail(),
            ),
            Self::DroppedConditionalBinding {
                binding,
                condition,
                source_path,
                line,
            } => format!(
                "binding '{binding}' is defined only in a dropped branch of condition `{condition}` in {} at line {line}",
                display_source_path_relative_to(source_path, root),
            ),
            Self::UnexpandedModuleTemplate { context } => format!(
                "cannot publish a reusable module template with an unresolved argument-dependent `if` at {context}; instantiate the module with concrete arguments before plan or apply"
            ),
            Self::Parse(ParseError::Located { path, error }) => format!(
                "Parse error in {}: {}",
                display_source_path_relative_to(path, root),
                display_parse_error_relative_to(error, root),
            ),
            Self::Parse(error) => format!(
                "Parse error: {}",
                display_parse_error_relative_to(error, root)
            ),
            Self::ParseInFile { path, error } => format!(
                "Parse error in {}: {}",
                display_source_path_relative_to(path, root),
                display_parse_error_relative_to(error, root),
            ),
            Self::AnalysisInvariant(error) => error.to_string(),
            Self::TypeResolution(diagnostic) => format!(
                "{}:{}:{}: {}",
                display_source_path_relative_to(diagnostic.location().file().path(), root),
                diagnostic.location().span().start_line(),
                diagnostic.location().span().start_column(),
                diagnostic.detail()
            ),
            Self::NoConfigurationFiles { .. }
            | Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument { .. }
            | Self::InvalidArgumentType { .. }
            | Self::ReferenceType(_)
            | Self::Io(_)
            | Self::UnknownArgument { .. }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::RequireOutsideModule(_)
            | Self::NotADirectory { .. } => self.to_string(),
        }
    }

    fn is_deferred_for_body_rejection(&self) -> bool {
        match self.root_cause() {
            Self::WithProviderTypeWarnings { error, .. } => error.is_deferred_for_body_rejection(),
            Self::Multiple(errors) => errors
                .iter()
                .all(ModuleError::is_deferred_for_body_rejection),
            Self::Parse(error) => parse_error_is_deferred_for_body_rejection(error),
            Self::ParseInFile { path: _, error } => {
                parse_error_is_deferred_for_body_rejection(error)
            }
            Self::WithRootEvidence { .. }
            | Self::OwnedByCalls { .. }
            | Self::AtCall { .. }
            | Self::NoConfigurationFiles { .. }
            | Self::NotFound(_)
            | Self::CircularImport(_)
            | Self::MissingArgument { .. }
            | Self::InvalidArgumentType { .. }
            | Self::BareStructuralHandleArgument { .. }
            | Self::ArgumentValueAccess { .. }
            | Self::DroppedConditionalBinding { .. }
            | Self::UnexpandedModuleTemplate { .. }
            | Self::ReferenceType(_)
            | Self::AnalysisInvariant(_)
            | Self::TypeResolution(_)
            | Self::Io(_)
            | Self::UnknownArgument { .. }
            | Self::UnknownModule(_)
            | Self::ProviderInModule
            | Self::StateBlockInModule
            | Self::BackendInModule
            | Self::UpstreamStateInModule
            | Self::ExportsInModule
            | Self::Constraint(_)
            | Self::RequireOutsideModule(_)
            | Self::NotADirectory { .. } => false,
        }
    }
}

fn render_provider_type_warnings(warnings: &[crate::parser::ParseWarning]) -> String {
    warnings
        .iter()
        .map(|warning| {
            format!(
                "warning: {}:{}: {}",
                warning.file.as_deref().unwrap_or("<module>"),
                warning.line,
                warning.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_error_is_deferred_for_body_rejection(error: &ParseError) -> bool {
    match error {
        ParseError::DeferredForBodyUnsupported(_)
        | ParseError::UndefinedDeferredIterable { .. } => true,
        ParseError::Located { path: _, error } => parse_error_is_deferred_for_body_rejection(error),
        ParseError::AtElement(finding) => {
            parse_error_is_deferred_for_body_rejection(finding.detail())
        }
        ParseError::DeferredForBodyTemplate {
            line: _,
            header: _,
            span: _,
            error: _,
        }
        | ParseError::InsideSecret { .. }
        | ParseError::SecretInConstraint { .. }
        | ParseError::InterpolationInConstraintString { .. }
        | ParseError::Syntax(_)
        | ParseError::ProtectedFile(_)
        | ParseError::UnavailableSecret(_)
        | ParseError::InvalidFloatLiteral { .. }
        | ParseError::InvalidExpression { .. }
        | ParseError::InvalidAuthoredExpression { .. }
        | ParseError::Evaluation { .. }
        | ParseError::IntegerArithmetic { .. }
        | ParseError::UnboundLoopRead { .. }
        | ParseError::UnresolvedDeclarationType { .. }
        | ParseError::UnknownFunction { .. }
        | ParseError::ReservedResourceAttribute { .. }
        | ParseError::SecretDerivedBackendAttribute { .. }
        | ParseError::InterpolationNotAllowedInLiteralString { .. }
        | ParseError::InvalidResourceIdentity { .. }
        | ParseError::UndefinedVariable(_)
        | ParseError::InvalidResourceType(_)
        | ParseError::DuplicateModule(_)
        | ParseError::DuplicateBinding { .. }
        | ParseError::UndefinedIdentifier { .. }
        | ParseError::ModuleNotFound(_)
        | ParseError::InternalError { .. }
        | ParseError::RecursiveFunction(_)
        | ParseError::CyclicLetBinding { .. }
        | ParseError::ValueWalk(_)
        | ParseError::UserFunctionError(_)
        | ParseError::RetainedEvidence(_) => false,
    }
}

fn display_parse_error_relative_to(error: &ParseError, root: &Path) -> String {
    match error {
        ParseError::AtElement(finding) => {
            let location = finding.location();
            let span = location.span();
            format!(
                "{}:{}:{}: {}",
                display_source_path_relative_to(location.file().path(), root),
                span.start_line(),
                span.start_column(),
                display_parse_error_relative_to(finding.detail(), root)
            )
        }
        ParseError::Located { path, error } => format!(
            "{}: {}",
            display_source_path_relative_to(path, root),
            display_parse_error_relative_to(error, root),
        ),
        _ => crate::executor::render_error_chain(error),
    }
}

fn display_source_path_relative_to(path: &Path, root: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::ProviderContext;

    #[test]
    fn module_errors_stay_below_clippy_large_error_threshold() {
        let size = size_of::<ModuleError>();
        println!("ModuleError: {size} bytes");
        assert!(size < 128, "ModuleError is {size} bytes");
    }

    #[test]
    fn lsp_renderer_keeps_non_root_deferred_body_call_context() {
        let parsed = crate::parser::parse_file(
            "let child = use { source = './child' }\nchild {}\n",
            Path::new("main.crn"),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        let call = &parsed.module_calls[0];
        let error = ModuleError::Parse(ParseError::UndefinedDeferredIterable {
            name: "missing".to_string(),
            line: 2,
            span: Box::new(call.source_span()),
            suggestion: None,
            in_scope: Vec::new(),
        })
        .with_failing_call(call.id().clone());
        let cli = error.display_relative_to(Path::new("."));
        assert!(
            cli.starts_with("module call in main.crn at line 2: "),
            "{cli}"
        );
        assert_eq!(error.lsp_message_relative_to(Path::new(".")), cli);
    }

    #[test]
    fn spanned_generic_undefined_identifier_is_not_a_deferred_for_rejection() {
        let parsed = crate::parser::parse_file(
            "let child = use { source = './child' }\nchild {}\n",
            Path::new("main.crn"),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("module-call fixture parses");
        let call = parsed.module_calls[0].clone();
        let error = ParseError::undefined_identifier_at(
            "missing".to_string(),
            2,
            Vec::new(),
            Some(call.source_span()),
        );
        let rendered = ModuleError::Parse(error)
            .with_failing_call(call.id().clone())
            .display_relative_to(Path::new("."));

        assert!(
            !rendered.starts_with("module call in "),
            "a source span does not make a generic undefined identifier a deferred-for body rejection: {rendered}",
        );
    }
}
