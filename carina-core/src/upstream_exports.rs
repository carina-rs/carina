//! Static validation of `upstream_state` field references.
//!
//! An `upstream_state { source = "..." }` binding exposes exactly the keys
//! declared by the upstream project's `exports { }` block. Those keys are
//! fixed at declaration time and visible by parsing the upstream's `.crn`
//! files — no state I/O.
//!
//! Two public entry points:
//! - [`resolve_upstream_exports`] parses each upstream's source directory
//!   and returns the declared key set per binding.
//! - [`check_upstream_state_field_references`] walks a parsed project and
//!   returns an error for every reference whose field isn't in the set.
//!
//! Both are pure functions so `validate`, LSP diagnostics, and any other
//! surface can share the same logic without duplicating traversal code.

use crate::paths::ResolvedPath;
mod admission;
#[cfg(any(test, feature = "test-utils"))]
pub use admission::remote_bindings_for_test;
pub use admission::{
    AdmittedRemoteBindings, AdmittedUpstreamExports, UpstreamExportAdmissionError,
};
use std::collections::HashMap;
#[cfg(test)]
use std::path::Path;

#[cfg(test)]
use crate::config_loader::parse_directory;
use crate::parser::{AnalysisReference, AnalysisTree, Located, TypeExpr, UpstreamState};
#[cfg(test)]
use crate::parser::{LoadedProviderContext, ProviderContext};
use crate::resource::{
    AbsoluteBindingAddress, BindingAddress, DiagnosticScope, InsideSecret, ParserPlaceholder,
    RelativeBindingName, SecretDiagnosticError, Subscript, Value,
};
use crate::schema::{SECRET_VALUE_MASK, suggest_similar_name};
use crate::static_type::ConsumerExportStaticType;
#[cfg(test)]
use crate::static_type::StaticType;

#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamExportType {
    Static(ConsumerExportStaticType),
    InferenceNotRun,
}

impl UpstreamExportType {
    pub fn declared_known(&self) -> Option<&TypeExpr> {
        match self {
            Self::Static(static_type) => static_type.declared_known(),
            Self::InferenceNotRun => None,
        }
    }
}

/// One export's static-type provenance and literal value, as carried through
/// [`UpstreamExports`]. `value` is currently always `Some` (the parser requires
/// `name = expr`) but is kept `Option` for forward compatibility with
/// future grammar widenings.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamExportEntry {
    pub static_type: UpstreamExportType,
    pub value: Option<Value<ParserPlaceholder, AbsoluteBindingAddress>>,
}

/// Exports for a single `upstream_state` binding: export name → entry.
pub type UpstreamExportEntries = HashMap<String, UpstreamExportEntry>;

/// Backend loading and cycle discovery have bootstrap authority only. Copy the
/// command's runtime hooks, never its installed provider catalog; admission
/// resolves source types separately against the downstream's loaded metadata.
pub struct UpstreamBootstrapContext(crate::parser::ProviderContext);

impl UpstreamBootstrapContext {
    pub fn from_runtime(context: &crate::parser::ProviderContext) -> Self {
        let mut bootstrap = crate::parser::ProviderContext::new(context.environment.clone());
        bootstrap.decryptor = context.decryptor.clone();
        Self(bootstrap)
    }

    fn load_source(
        &self,
        source: &ResolvedPath,
    ) -> Result<crate::config_loader::LoadedConfig, String> {
        crate::config_loader::load_configuration_for_bootstrap(
            source,
            &self.0,
            &crate::schema::SchemaRegistry::new(),
        )
    }

    /// Discover transitive upstream declarations without consuming their
    /// backend or values. A loaded provider catalog cannot enter this parse.
    pub fn load_metadata(
        &self,
        source: &ResolvedPath,
    ) -> Result<crate::config_loader::LoadedConfig, String> {
        crate::config_loader::load_configuration_for_metadata(
            source,
            &self.0,
            &crate::schema::SchemaRegistry::new(),
        )
    }
}

/// Static export discovery preserves the typecheck/validate/LSP caller's
/// catalog and schemas, including providerless fallback authority. It cannot
/// accept the bootstrap context used by backend loading and cycle discovery.
struct UpstreamDiscoveryContext<'a> {
    context: &'a dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: Option<&'a crate::schema::SchemaRegistry>,
}

impl UpstreamDiscoveryContext<'_> {
    fn parse_source(
        &self,
        source: &ResolvedPath,
        overrides: &crate::config_loader::SourceOverrides,
    ) -> Result<
        crate::config_loader::ParsedRootDirectory,
        crate::config_loader::DirectoryParseFailure,
    > {
        crate::config_loader::parse_directory_for_metadata_with_source_overrides_report(
            source,
            self.context.catalog(),
            overrides,
            self.schemas
                .unwrap_or(&crate::schema::SchemaRegistry::new()),
        )
    }
}

/// A binding and its pinned source directory, resolved together from one
/// upstream declaration. Loading cannot substitute an unrelated directory.
#[derive(Debug, Clone)]
pub struct UpstreamSourceIdentity {
    binding: String,
    source: ResolvedPath,
    declared_source: std::path::PathBuf,
}

impl UpstreamSourceIdentity {
    pub fn resolve<B: BindingAddress>(
        upstream: &UpstreamState<B>,
        base: &ResolvedPath,
    ) -> std::io::Result<Self> {
        Ok(Self {
            binding: upstream.binding.to_string(),
            source: base.join_display(&base.persisted_path().display().join(&upstream.source))?,
            declared_source: upstream.source.clone(),
        })
    }

    pub fn binding(&self) -> &str {
        &self.binding
    }
    pub fn source(&self) -> &ResolvedPath {
        &self.source
    }

    /// Backend parsing requires bootstrap authority; source admission receives
    /// the downstream type authority separately.
    pub fn load(
        &self,
        bootstrap: &UpstreamBootstrapContext,
        context: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
        schemas: &crate::schema::SchemaRegistry,
    ) -> Result<ResolvedUpstreamSource, UpstreamSourceLoadError> {
        let loaded = bootstrap.load_source(&self.source).map_err(|message| {
            UpstreamSourceLoadError::Configuration {
                binding: self.binding.clone(),
                message,
            }
        })?;
        let surface = resolve_upstream_source_surface(&loaded, self, context, schemas)?;
        Ok(ResolvedUpstreamSource {
            identity: self.clone(),
            backend: loaded
                .parsed
                .backend
                .clone()
                .map(crate::parser::BackendConfig::into_expanded)
                .transpose()?,
            upstream_states: loaded.parsed.upstream_states.clone(),
            surface,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamSourceLoadError {
    #[error("upstream_state '{binding}': {message}")]
    Configuration { binding: String, message: String },
    #[error("{0}")]
    Rejected(Box<UpstreamResolveFailure>),
    #[error(transparent)]
    Invariant(#[from] crate::parser::AnalysisTreeError),
    #[error(transparent)]
    Expansion(#[from] crate::resource::LeftoverPlaceholder),
}

/// Identity, backend metadata, and admission surface from the same pinned source.
/// Admission is a method on this object; neither the binding nor surface can be
/// supplied independently at the value boundary.
#[derive(Debug, Clone)]
pub struct ResolvedUpstreamSource {
    identity: UpstreamSourceIdentity,
    backend: Option<crate::parser::BackendConfig>,
    upstream_states: Vec<UpstreamState<RelativeBindingName>>,
    surface: SourceSurface,
}

impl ResolvedUpstreamSource {
    pub fn binding(&self) -> &str {
        self.identity.binding()
    }
    pub fn source(&self) -> &ResolvedPath {
        self.identity.source()
    }
    pub fn backend(&self) -> Option<&crate::parser::BackendConfig> {
        self.backend.as_ref()
    }
    pub fn upstream_states(&self) -> &[UpstreamState<RelativeBindingName>] {
        &self.upstream_states
    }
    pub fn unavailable_reason(&self) -> Option<&UpstreamSourceUnavailable> {
        self.surface
            .entries
            .as_ref()
            .err()
            .map(std::sync::Arc::as_ref)
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(binding: impl Into<String>, entries: Option<&UpstreamExportEntries>) -> Self {
        Self {
            identity: UpstreamSourceIdentity {
                binding: binding.into(),
                source: ResolvedPath::new(".").expect("fixture directory exists"),
                declared_source: ".".into(),
            },
            backend: None,
            upstream_states: vec![],
            surface: SourceSurface {
                entries: entries.cloned().ok_or_else(|| {
                    std::sync::Arc::new(UpstreamSourceUnavailable::Configuration(
                        "test fixture has no source surface".into(),
                    ))
                }),
            },
        }
    }
}

#[derive(Debug, Clone)]
struct SourceSurface {
    entries: Result<UpstreamExportEntries, std::sync::Arc<UpstreamSourceUnavailable>>,
}

/// Non-rejecting source failures preserve unchecked admission and their typed
/// causes. A module-template rejection cannot inhabit this unavailable surface.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamSourceUnavailable {
    #[error("{0}")]
    Configuration(String),
    #[error("{error}")]
    Module {
        base: std::path::PathBuf,
        error: Box<crate::module_resolver::ModuleError>,
    },
    #[error("{}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"))]
    Boundaries(Vec<crate::validation::ExportBoundaryError>),
}

/// Discover admission authority from an actual loader-owned source snapshot.
/// Unavailable declarations preserve the unchecked policy without warnings.
fn resolve_upstream_source_surface(
    loaded: &crate::config_loader::LoadedConfig,
    identity: &UpstreamSourceIdentity,
    context: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: &crate::schema::SchemaRegistry,
) -> Result<SourceSurface, UpstreamSourceLoadError> {
    let entries = match loaded.upstream_export_source(context.catalog(), schemas) {
        Ok(parsed) => match resolve_upstream_exports_from_parsed(
            parsed,
            identity.source(),
            context,
            Some(schemas),
            &Default::default(),
        ) {
            Ok(entries) => Ok(entries),
            Err(UpstreamExportSourceError::Invariant(error)) => return Err(error.into()),
            Err(UpstreamExportSourceError::ModuleTemplate(template)) => {
                return Err(UpstreamSourceLoadError::Rejected(Box::new(
                    UpstreamResolveFailure::module_template(
                        identity.binding.clone(),
                        identity.declared_source.clone(),
                        template,
                    ),
                )));
            }
            Err(UpstreamExportSourceError::Unavailable(reason)) => {
                Err(UpstreamSourceUnavailable::Configuration(reason))
            }
            Err(UpstreamExportSourceError::Module { base, error }) => {
                Err(UpstreamSourceUnavailable::Module { base, error })
            }
            Err(UpstreamExportSourceError::Boundaries(errors)) => {
                Err(UpstreamSourceUnavailable::Boundaries(errors))
            }
        },
        Err(reason) => Err(UpstreamSourceUnavailable::Configuration(reason)),
    };
    Ok(SourceSurface {
        entries: entries.map_err(std::sync::Arc::new),
    })
}

/// Identity of the immutable type evidence used to discover a source surface.
/// Catalog and schema witnesses are retained so identity cannot be recycled.
#[derive(Clone)]
pub struct UpstreamTypeAuthority {
    catalog: crate::parser::ProviderCustomTypes,
    resource_types: std::collections::HashSet<(String, String)>,
    schemas: HashMap<
        (String, String, bool),
        (
            std::sync::Arc<crate::schema::CompiledResourceSchema>,
            String,
        ),
    >,
}

impl UpstreamTypeAuthority {
    pub fn capture(
        context: &crate::parser::ProviderContext,
        schemas: &crate::schema::SchemaRegistry,
    ) -> Self {
        Self {
            catalog: context.custom_types.clone(),
            resource_types: context.resource_types.clone(),
            schemas: schemas
                .iter()
                .map(|(provider, name, kind, schema)| {
                    (
                        (
                            provider.into(),
                            name.into(),
                            matches!(kind, crate::schema::SchemaKind::DataSource),
                        ),
                        (
                            schema.shared_schema(),
                            schemas
                                .get(provider, name, kind)
                                .expect("iterated schema exists")
                                .semantic_fingerprint(),
                        ),
                    )
                })
                .collect(),
        }
    }
}

impl PartialEq for UpstreamTypeAuthority {
    fn eq(&self, other: &Self) -> bool {
        self.catalog.same_snapshot(&other.catalog)
            && self.resource_types == other.resource_types
            && self.schemas.len() == other.schemas.len()
            && self.schemas.iter().all(|(key, (schema, fingerprint))| {
                other
                    .schemas
                    .get(key)
                    .is_some_and(|(other_schema, other_fingerprint)| {
                        std::sync::Arc::ptr_eq(schema, other_schema)
                            && fingerprint == other_fingerprint
                    })
            })
    }
}

impl Eq for UpstreamTypeAuthority {}

/// Exports declared by each `upstream_state` binding: binding name →
/// per-export entry map. Carrying the literal `Value` alongside the
/// static-type provenance lets downstream consumers (LSP map-literal-key
/// completion, future doc tools) read the body without re-parsing
/// the upstream directory.
pub type UpstreamExports<B = RelativeBindingName> = HashMap<B, UpstreamExportEntries>;
type UpstreamAnalysis<'a, D, B, E> =
    AnalysisTree<'a, ParserPlaceholder, ParserPlaceholder, D, B, E>;

/// A diagnostic about a `binding.field` reference whose downstream usage
/// doesn't fit the upstream's exports.
///
/// Four error types share this shape — name-not-exported
/// ([`UpstreamFieldError`]), `for`-iterable shape mismatch
/// ([`UpstreamForIterableShapeError`]), attribute-access shape mismatch
/// ([`UpstreamAttributeAccessShapeError`]), and subscript shape mismatch
/// ([`UpstreamSubscriptShapeError`]). They share their
/// CLI and LSP wirings through this trait so adding another check is
/// one `impl`, not three identical extends/loops.
///
/// Excludes [`UpstreamResolveError`] on purpose: a resolve failure
/// doesn't have a `(binding, field)` pair (the upstream source itself
/// failed to parse) so the LSP anchoring path can't render it the same
/// way.
///
/// `Display` is required so `to_string()` produces the canonical
/// `"location: message"` form for the CLI's combined-error path —
/// keeping the format in one place (the per-type `Display` impl)
/// instead of letting the CLI reimplement it inline.
pub trait UpstreamRefDiagnostic: std::fmt::Display {
    /// Where in the downstream project the bad reference appears
    /// (e.g. `"aws.s3.Bucket.main attribute `name`"`).
    fn location(&self) -> &str;
    /// Root binding name (e.g. `"orgs"`).
    fn binding(&self) -> &str;
    /// Top-level field on the upstream's exports (e.g. `"accounts"`).
    fn field(&self) -> &str;
    /// User-facing diagnostic body, location-free. The LSP diagnostic
    /// range carries the *where*; the message says *what*.
    fn diagnostic_message(&self) -> String;
}

impl<D: UpstreamRefDiagnostic> UpstreamRefDiagnostic for Located<D> {
    fn location(&self) -> &str {
        self.detail().location()
    }
    fn binding(&self) -> &str {
        self.detail().binding()
    }
    fn field(&self) -> &str {
        self.detail().field()
    }
    fn diagnostic_message(&self) -> String {
        self.detail().diagnostic_message()
    }
}

/// An upstream finding whose entire payload is discarded at a secret boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamDiagnostic<D> {
    detail: SecretDiagnosticError<D>,
}

impl<D> UpstreamDiagnostic<D> {
    /// Structured details are available only for public findings.
    pub fn public_detail(&self) -> Option<&D> {
        match &self.detail {
            SecretDiagnosticError::Public(detail) => Some(detail),
            SecretDiagnosticError::InsideSecret => None,
        }
    }
}

impl<D> From<D> for UpstreamDiagnostic<D> {
    fn from(detail: D) -> Self {
        Self {
            detail: DiagnosticScope::Public.protect(detail),
        }
    }
}

impl<D> InsideSecret for UpstreamDiagnostic<D> {
    fn inside_secret(self) -> Self {
        Self {
            detail: self.detail.inside_secret(),
        }
    }
}

impl<D: UpstreamRefDiagnostic> UpstreamRefDiagnostic for UpstreamDiagnostic<D> {
    fn location(&self) -> &str {
        self.public_detail().map_or(SECRET_VALUE_MASK, D::location)
    }

    fn binding(&self) -> &str {
        self.public_detail().map_or(SECRET_VALUE_MASK, D::binding)
    }

    fn field(&self) -> &str {
        self.public_detail().map_or(SECRET_VALUE_MASK, D::field)
    }

    fn diagnostic_message(&self) -> String {
        self.public_detail().map_or_else(
            || crate::resource::INSIDE_SECRET_MESSAGE.into(),
            D::diagnostic_message,
        )
    }
}

impl<D: UpstreamRefDiagnostic> std::fmt::Display for UpstreamDiagnostic<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location(), self.diagnostic_message())
    }
}

impl<D: UpstreamRefDiagnostic + std::fmt::Debug> std::error::Error for UpstreamDiagnostic<D> {}

/// An `upstream_state` binding whose source directory exists but couldn't
/// be parsed. Downstream field-reference checks against this binding are
/// skipped (we don't know what it exports), but the failure itself must be
/// surfaced — otherwise a broken upstream silently masks downstream typos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamResolveError {
    pub binding: String,
    pub source: std::path::PathBuf,
    pub reason: String,
    kind: UpstreamResolveErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpstreamResolveErrorKind {
    ParseFailure,
    ModuleTemplate,
}

impl UpstreamResolveError {
    fn fmt_reason(
        &self,
        f: &mut std::fmt::Formatter<'_>,
        reason: &dyn std::fmt::Display,
    ) -> std::fmt::Result {
        write!(
            f,
            "upstream_state `{}`: failed to parse source `{}`: {}",
            self.binding,
            self.source.display(),
            reason
        )
    }
}

impl std::fmt::Display for UpstreamResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            UpstreamResolveErrorKind::ParseFailure => self.fmt_reason(f, &self.reason),
            UpstreamResolveErrorKind::ModuleTemplate => write!(
                f,
                "upstream_state `{}`: source `{}` {}",
                self.binding,
                self.source.display(),
                self.reason
            ),
        }
    }
}

impl std::error::Error for UpstreamResolveError {}

/// Typed source failures retained separately from their legacy combined display.
#[derive(Debug, Clone)]
pub struct UpstreamResolveFailure {
    legacy: UpstreamResolveError,
    cause: UpstreamResolveCause,
}
#[derive(Debug, Clone)]
enum UpstreamResolveCause {
    Message,
    ModuleTemplate(crate::config_loader::ModuleTemplateDeclarations),
    Boundaries(Vec<crate::validation::ExportBoundaryError>),
    Parse(std::sync::Arc<crate::config_loader::DirectoryParseFailure>),
    Module {
        base: std::path::PathBuf,
        error: std::sync::Arc<crate::module_resolver::ModuleError>,
    },
}
impl UpstreamResolveFailure {
    fn message(binding: String, source: std::path::PathBuf, reason: String) -> Self {
        Self {
            legacy: UpstreamResolveError {
                binding,
                source,
                reason,
                kind: UpstreamResolveErrorKind::ParseFailure,
            },
            cause: UpstreamResolveCause::Message,
        }
    }
    fn boundaries(
        binding: String,
        source: std::path::PathBuf,
        boundaries: Vec<crate::validation::ExportBoundaryError>,
    ) -> Self {
        let reason = boundaries
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            legacy: UpstreamResolveError {
                binding,
                source,
                reason,
                kind: UpstreamResolveErrorKind::ParseFailure,
            },
            cause: UpstreamResolveCause::Boundaries(boundaries),
        }
    }
    fn parse(
        binding: String,
        source: std::path::PathBuf,
        error: crate::config_loader::DirectoryParseFailure,
    ) -> Self {
        Self {
            legacy: UpstreamResolveError {
                binding,
                source,
                reason: error.to_string(),
                kind: UpstreamResolveErrorKind::ParseFailure,
            },
            cause: UpstreamResolveCause::Parse(std::sync::Arc::new(error)),
        }
    }
    fn module(
        binding: String,
        source: std::path::PathBuf,
        base: std::path::PathBuf,
        error: crate::module_resolver::ModuleError,
    ) -> Self {
        Self {
            legacy: UpstreamResolveError {
                binding,
                source,
                reason: String::new(),
                kind: UpstreamResolveErrorKind::ParseFailure,
            },
            cause: UpstreamResolveCause::Module {
                base,
                error: std::sync::Arc::new(error),
            },
        }
    }
    fn module_template(
        binding: String,
        source: std::path::PathBuf,
        template: crate::config_loader::ModuleTemplateDeclarations,
    ) -> Self {
        Self {
            legacy: UpstreamResolveError {
                binding,
                source,
                reason: String::new(),
                kind: UpstreamResolveErrorKind::ModuleTemplate,
            },
            cause: UpstreamResolveCause::ModuleTemplate(template),
        }
    }
    pub fn module_evidence(&self) -> Option<&crate::config_loader::ModuleEvidence> {
        match &self.cause {
            UpstreamResolveCause::ModuleTemplate(template) => Some(template.evidence()),
            UpstreamResolveCause::Message
            | UpstreamResolveCause::Boundaries(_)
            | UpstreamResolveCause::Parse(_)
            | UpstreamResolveCause::Module { base: _, error: _ } => None,
        }
    }
    fn module_template_message(&self) -> String {
        format!(
            "upstream_state `{}`: source `{}` is a module definition and has no state to read",
            self.legacy.binding,
            self.legacy.source.display()
        )
    }
    pub fn boundary_errors(&self) -> &[crate::validation::ExportBoundaryError] {
        match &self.cause {
            UpstreamResolveCause::Boundaries(errors) => errors,
            UpstreamResolveCause::Message
            | UpstreamResolveCause::ModuleTemplate(_)
            | UpstreamResolveCause::Parse(_)
            | UpstreamResolveCause::Module { base: _, error: _ } => &[],
        }
    }
    pub fn parse_failure(&self) -> Option<&crate::config_loader::DirectoryParseFailure> {
        match &self.cause {
            UpstreamResolveCause::Parse(error) => Some(error),
            UpstreamResolveCause::Message
            | UpstreamResolveCause::ModuleTemplate(_)
            | UpstreamResolveCause::Boundaries(_)
            | UpstreamResolveCause::Module { base: _, error: _ } => None,
        }
    }
    pub fn module_failure(
        &self,
    ) -> Option<(&std::path::Path, &crate::module_resolver::ModuleError)> {
        match &self.cause {
            UpstreamResolveCause::Module { base, error } => Some((base, error)),
            UpstreamResolveCause::Message
            | UpstreamResolveCause::ModuleTemplate(_)
            | UpstreamResolveCause::Boundaries(_)
            | UpstreamResolveCause::Parse(_) => None,
        }
    }
    pub fn diagnostic_lines(&self) -> Vec<crate::module_resolver::DiagnosticLine> {
        use crate::module_resolver::DiagnosticLine;
        match &self.cause {
            UpstreamResolveCause::ModuleTemplate(template) => vec![
                DiagnosticLine::Message(self.module_template_message()),
                DiagnosticLine::ModuleEvidence(template.evidence().clone()),
            ],
            UpstreamResolveCause::Module { base: _, error } => {
                let mut lines = error.diagnostic_lines_relative_to(None);
                if let Some(DiagnosticLine::Message(message)) = lines.first_mut() {
                    *message =
                        std::fmt::from_fn(|formatter| self.legacy.fmt_reason(formatter, message))
                            .to_string();
                }
                lines
            }
            UpstreamResolveCause::Message
            | UpstreamResolveCause::Parse(_)
            | UpstreamResolveCause::Boundaries(_) => {
                vec![DiagnosticLine::Message(self.to_string())]
            }
        }
    }

    /// Editor projection keeps declaration evidence in related information.
    pub fn lsp_message(&self) -> String {
        if self.module_evidence().is_some() {
            return self.module_template_message();
        }
        if let Some((_, error)) = self.module_failure()
            && let Some(rejection) = error.root_cause().require_outside_module()
        {
            let message = crate::parser::Located::new(
                rejection.location().clone(),
                rejection.detail().lsp_message(),
            );
            return std::fmt::from_fn(|formatter| self.legacy.fmt_reason(formatter, &message))
                .to_string();
        }
        self.to_string()
    }
    fn into_legacy(mut self) -> UpstreamResolveError {
        // Only the explicit compatibility API requests a location-free String
        // container. Diagnostic callers retain typed causes via the accessors.
        match &self.cause {
            UpstreamResolveCause::Module { error, base: _ } => {
                self.legacy.reason = error.to_string()
            }
            UpstreamResolveCause::ModuleTemplate(template) => {
                self.legacy.reason = format!(
                    "is a module definition and has no state to read\nNote: {}",
                    template.evidence().note()
                )
            }
            UpstreamResolveCause::Message
            | UpstreamResolveCause::Boundaries(_)
            | UpstreamResolveCause::Parse(_) => {}
        }
        self.legacy
    }
}
impl std::fmt::Display for UpstreamResolveFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.cause {
            UpstreamResolveCause::Module { error, .. } => {
                self.legacy.fmt_reason(formatter, error.as_ref())
            }
            UpstreamResolveCause::ModuleTemplate(template) => write!(
                formatter,
                "{}\nNote: {}",
                self.module_template_message(),
                template.evidence().note()
            ),
            UpstreamResolveCause::Message
            | UpstreamResolveCause::Boundaries(_)
            | UpstreamResolveCause::Parse(_) => self.legacy.fmt(formatter),
        }
    }
}
// Display includes the complete cause with upstream context. Keep typed
// causes in parse_failure/module_failure, not as duplicate rendered links.
impl std::error::Error for UpstreamResolveFailure {}

/// A reference in the downstream project whose field isn't declared by the
/// upstream's `exports { }` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamFieldError {
    /// Where in the downstream project the bad reference appears
    /// (e.g. `"aws.s3.Bucket.main attribute `name`"`).
    pub location: String,
    pub binding: String,
    pub field: String,
    pub suggestion: Option<String>,
}

impl UpstreamFieldError {
    /// Location-free phrasing shared by CLI and LSP — the LSP diagnostic
    /// range already tells the user *where*, so it only needs *what*.
    pub fn diagnostic_message(&self) -> String {
        let suggestion = self
            .suggestion
            .as_ref()
            .map(|s| format!(" Did you mean `{}`?", s))
            .unwrap_or_default();
        format!(
            "upstream_state `{}` does not export `{}`.{}",
            self.binding, self.field, suggestion
        )
    }
}

impl std::fmt::Display for UpstreamFieldError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.diagnostic_message())
    }
}

impl std::error::Error for UpstreamFieldError {}

impl UpstreamRefDiagnostic for UpstreamFieldError {
    fn location(&self) -> &str {
        &self.location
    }
    fn binding(&self) -> &str {
        &self.binding
    }
    fn field(&self) -> &str {
        &self.field
    }
    fn diagnostic_message(&self) -> String {
        UpstreamFieldError::diagnostic_message(self)
    }
}

/// Statically resolve each `upstream_state` binding's declared exports.
///
/// Returns `(exports, resolve_errors)` — a **partial** result so one broken
/// upstream doesn't suppress checks against the others.
///
/// - Present in `exports` (possibly empty set): source exists and parses.
///   An empty set means "exports nothing", so every downstream reference
///   against it is invalid.
/// - Omitted from `exports` and listed in `resolve_errors`: source exists
///   but fails to parse — callers should surface this error separately.
/// - Absent from both: source directory is missing. The dedicated
///   `check_upstream_state_sources` diagnostic already handles that case.
///
/// An empty directory (no `.crn` files) is treated as "exports nothing"
/// rather than a parse failure; the user may be mid-setup.
pub fn resolve_upstream_exports<D, B: BindingAddress, E>(
    base_dir: &ResolvedPath,
    tree: &UpstreamAnalysis<'_, D, B, E>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
) -> Result<
    (UpstreamExports<B>, Vec<Located<UpstreamResolveError>>),
    crate::parser::AnalysisTreeError,
>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    resolve_upstream_exports_with_schemas(base_dir, tree, config, None)
}

pub fn resolve_upstream_exports_with_schemas<D, B: BindingAddress, E>(
    base_dir: &ResolvedPath,
    tree: &UpstreamAnalysis<'_, D, B, E>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
) -> Result<
    (UpstreamExports<B>, Vec<Located<UpstreamResolveError>>),
    crate::parser::AnalysisTreeError,
>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    resolve_upstream_exports_with_schemas_and_overrides(
        base_dir,
        tree,
        config,
        schemas,
        &Default::default(),
    )
}

pub fn resolve_upstream_exports_with_schemas_and_overrides<D, B: BindingAddress, E>(
    base_dir: &ResolvedPath,
    tree: &UpstreamAnalysis<'_, D, B, E>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
    overrides: &crate::config_loader::SourceOverrides,
) -> Result<
    (UpstreamExports<B>, Vec<Located<UpstreamResolveError>>),
    crate::parser::AnalysisTreeError,
>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let (exports, findings) =
        resolve_upstream_exports_report_with_overrides(base_dir, tree, config, schemas, overrides)?;
    Ok((
        exports,
        findings
            .into_iter()
            .map(|finding| {
                let (location, error) = finding.into_parts();
                Located::new(location, error.into_legacy())
            })
            .collect(),
    ))
}

pub fn resolve_upstream_exports_report<D, B: BindingAddress, E>(
    base_dir: &ResolvedPath,
    tree: &UpstreamAnalysis<'_, D, B, E>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
) -> Result<
    (UpstreamExports<B>, Vec<Located<UpstreamResolveFailure>>),
    crate::parser::AnalysisTreeError,
>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    resolve_upstream_exports_report_with_overrides(
        base_dir,
        tree,
        config,
        schemas,
        &Default::default(),
    )
}

pub fn resolve_upstream_exports_report_with_overrides<D, B: BindingAddress, E>(
    base_dir: &ResolvedPath,
    tree: &UpstreamAnalysis<'_, D, B, E>,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
    overrides: &crate::config_loader::SourceOverrides,
) -> Result<
    (UpstreamExports<B>, Vec<Located<UpstreamResolveFailure>>),
    crate::parser::AnalysisTreeError,
>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let mut exports = HashMap::new();
    let mut findings = Vec::new();
    for state in tree.upstream_states() {
        let (resolved, errors) = resolve_upstream_exports_context_report(
            base_dir,
            std::slice::from_ref(*state.detail()),
            config,
            schemas,
            overrides,
        )?;
        exports.extend(resolved);
        findings.extend(
            errors
                .into_iter()
                .map(|error| Located::new(state.location().clone(), error)),
        );
    }
    Ok((exports, findings))
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("upstream_state '{binding}': source '{source_path}' does not exist")]
pub struct MissingUpstreamDirectory {
    binding: String,
    source_path: String,
}

impl MissingUpstreamDirectory {
    pub fn check<B: BindingAddress>(base: &ResolvedPath, state: &UpstreamState<B>) -> Option<Self> {
        (!base.join(&state.source).is_ok_and(|source| source.is_dir())).then(|| Self {
            binding: state.binding.to_string(),
            source_path: state.source.to_string_lossy().into_owned(),
        })
    }
}

pub fn check_upstream_state_sources<D, B: BindingAddress, E>(
    base: &ResolvedPath,
    tree: &UpstreamAnalysis<'_, D, B, E>,
) -> Vec<Located<MissingUpstreamDirectory>>
where
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    tree.upstream_states()
        .iter()
        .filter_map(|state| {
            MissingUpstreamDirectory::check(base, state.detail())
                .map(|error| Located::new(state.location().clone(), error))
        })
        .collect()
}

/// Best-effort discovery for completion. It supplies no analysis findings;
/// diagnostic consumers must use the sealed-tree resolver above.
pub fn discover_upstream_exports<B: BindingAddress>(
    base_dir: &ResolvedPath,
    upstream_states: &[UpstreamState<B>],
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
) -> UpstreamExports<B> {
    resolve_upstream_exports_with_context(
        base_dir,
        upstream_states,
        config,
        schemas,
        &Default::default(),
    )
    .map(|(exports, _)| exports)
    .unwrap_or_default()
}

fn resolve_upstream_exports_with_context<B: BindingAddress>(
    base_dir: &ResolvedPath,
    upstream_states: &[UpstreamState<B>],
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
    overrides: &crate::config_loader::SourceOverrides,
) -> Result<(UpstreamExports<B>, Vec<UpstreamResolveError>), crate::parser::AnalysisTreeError> {
    resolve_upstream_exports_context_report(base_dir, upstream_states, config, schemas, overrides)
        .map(|(exports, errors)| {
            (
                exports,
                errors
                    .into_iter()
                    .map(UpstreamResolveFailure::into_legacy)
                    .collect(),
            )
        })
}

fn resolve_upstream_exports_context_report<B: BindingAddress>(
    base_dir: &ResolvedPath,
    upstream_states: &[UpstreamState<B>],
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
    overrides: &crate::config_loader::SourceOverrides,
) -> Result<(UpstreamExports<B>, Vec<UpstreamResolveFailure>), crate::parser::AnalysisTreeError> {
    let mut out: UpstreamExports<B> = HashMap::new();
    let mut errors: Vec<UpstreamResolveFailure> = Vec::new();
    let discovery = UpstreamDiscoveryContext {
        context: config,
        schemas,
    };
    for us in upstream_states {
        let source_abs = match base_dir.join(&us.source) {
            Ok(source) => source,
            Err(error) => {
                errors.push(UpstreamResolveFailure::message(
                    us.binding.to_string(),
                    us.source.clone(),
                    error.to_string(),
                ));
                continue;
            }
        };
        if !source_abs.is_dir() {
            continue;
        }
        if matches!(crate::config_loader::DirectoryFiles::discover(&source_abs, overrides), Ok(files) if files.is_empty())
        {
            out.insert(us.binding.clone(), HashMap::new());
            continue;
        }
        match discovery.parse_source(&source_abs, overrides) {
            Ok(parsed) => match resolve_upstream_exports_from_parsed(
                parsed,
                &source_abs,
                config,
                schemas,
                overrides,
            ) {
                Ok(entries) => {
                    out.insert(us.binding.clone(), entries);
                }
                Err(UpstreamExportSourceError::Invariant(error)) => return Err(error),
                Err(reason) => errors
                    .push(reason.into_resolve_failure(us.binding.to_string(), us.source.clone())),
            },
            Err(reason) => {
                errors.push(UpstreamResolveFailure::parse(
                    us.binding.to_string(),
                    us.source.clone(),
                    reason,
                ));
            }
        }
    }
    Ok((out, errors))
}

/// Failure to compute a source surface is distinct from an analysis invariant.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamExportSourceError {
    #[error("source is a module definition and has no state to read\nNote: {}", .0.evidence().note())]
    ModuleTemplate(crate::config_loader::ModuleTemplateDeclarations),
    #[error(transparent)]
    Invariant(#[from] crate::parser::AnalysisTreeError),
    #[error("{0}")]
    Unavailable(String),
    #[error("{error}")]
    Module {
        base: std::path::PathBuf,
        error: Box<crate::module_resolver::ModuleError>,
    },
    #[error("{}", .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n"))]
    Boundaries(Vec<crate::validation::ExportBoundaryError>),
}

impl UpstreamExportSourceError {
    /// Source directory and typed resolver failure whose complete diagnostic
    /// is already rendered by this error's Display.
    pub fn module_failure(
        &self,
    ) -> Option<(&std::path::Path, &crate::module_resolver::ModuleError)> {
        match self {
            Self::Module { base, error } => Some((base, error)),
            Self::ModuleTemplate(_)
            | Self::Invariant(_)
            | Self::Unavailable(_)
            | Self::Boundaries(_) => None,
        }
    }

    fn into_resolve_failure(
        self,
        binding: String,
        source: std::path::PathBuf,
    ) -> UpstreamResolveFailure {
        match self {
            Self::ModuleTemplate(template) => {
                UpstreamResolveFailure::module_template(binding, source, template)
            }
            Self::Unavailable(reason) => UpstreamResolveFailure::message(binding, source, reason),
            Self::Module { base, error } => {
                UpstreamResolveFailure::module(binding, source, base, *error)
            }
            Self::Boundaries(errors) => UpstreamResolveFailure::boundaries(binding, source, errors),
            Self::Invariant(_) => unreachable!("analysis invariants propagate separately"),
        }
    }
}

/// Reuse a loader-owned source parse for export discovery. This performs the
/// same normalization, module expansion and inference as directory discovery.
pub fn resolve_upstream_exports_from_parsed(
    parsed: crate::config_loader::ParsedRootDirectory,
    source_abs: &ResolvedPath,
    config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: Option<&crate::schema::SchemaRegistry>,
    overrides: &crate::config_loader::SourceOverrides,
) -> Result<UpstreamExportEntries, UpstreamExportSourceError> {
    match parsed.complete_directory_verdict() {
        crate::config_loader::DirectoryVerdict::Root(_)
        | crate::config_loader::DirectoryVerdict::NoEvidence(_) => {}
        crate::config_loader::DirectoryVerdict::ModuleTemplate(template) => {
            return Err(UpstreamExportSourceError::ModuleTemplate(template));
        } // Unknown is uninhabited after the complete source-loading parse;
          // unavailable sources retain their existing parse/load diagnostics.
    }
    let provider_context =
        crate::validation::typecheck::TypecheckProviderContext::for_session(config);
    let parsed = parsed.try_update(|parsed| {
        crate::validation::resolve_file_type_exprs_best_effort(parsed, config)
    })?;
    // Normalize every consumer-visible declaration before module
    // expansion and export inference. Inference compares the
    // structural TypeExpr values it receives, so leaving one
    // declaration as DottedUnresolved while an equivalent schema
    // projection is already SchemaType creates a false mismatch.

    // Expand module calls before inference so that
    // `attributes { ... }`-typed `Virtual` resources land in
    // `parsed.resources` (#2493). Without this the inferer
    // sees module-call bindings as known-but-non-inferable
    // and any `exports` value referencing
    // `<module_call_binding>.<attr>` (e.g.
    // `oidc_provider_arn = bs.oidc_provider_arn` at
    // `infra/registry/dev/bootstrap/exports.crn`) remains
    // unchecked even when the type is fully derivable.
    // `resolve_modules_with_config` is the same expansion
    // pass `load_configuration` runs, so the inference reach
    // here matches what the CLI sees post-expansion.
    let empty_schemas = crate::schema::SchemaRegistry::new();
    let expansion_schemas = schemas.unwrap_or(&empty_schemas);
    let module_resolution = match provider_context.loaded_context() {
        Some(_loaded) => crate::module_resolver::resolve_upstream_modules_with_config(
            parsed.admit(expansion_schemas, config),
            source_abs,
            overrides,
        ),
        None => crate::module_resolver::resolve_upstream_modules_with_providerless_config(
            parsed.admit(expansion_schemas, config),
            source_abs,
            overrides,
        ),
    };
    let parsed = match module_resolution {
        Ok(parsed) => parsed,
        Err(reason) => {
            if let crate::module_resolver::ModuleError::AnalysisInvariant(error) =
                reason.root_cause()
            {
                return Err(UpstreamExportSourceError::Invariant(error.clone()));
            }
            return Err(UpstreamExportSourceError::Module {
                base: source_abs.source_label(),
                error: Box::new(reason),
            });
        }
    };
    // Declarations were resolved before expansion so inference
    // compares canonical identities. Resolve once more afterward
    // to classify newly inferred names that the consumer cannot
    // name, preserving that distinction from inference failure.
    if schemas.is_none() {
        // Even the providerless discovery API is a state-boundary
        // loader. Handle RHSs and declared Ref types are
        // inferable without schemas and must never be advertised
        // as value-shaped upstream exports.
        let inferred = crate::validation::inference::infer_export_params(
            crate::config_loader::load_buffer_configuration(
                &parsed,
                &empty_schemas,
                config.catalog(),
            )
            .judged(),
        );
        if let Err(boundary_errors) =
            crate::validation::validate_export_handle_boundaries(inferred.parameters())
        {
            return Err(UpstreamExportSourceError::Boundaries(boundary_errors));
        }
    }
    let entries: UpstreamExportEntries = match schemas {
        Some(s) => {
            let inferred =
                crate::validation::inference::apply_inference(parsed, s, config.catalog());
            if let Err(boundary_errors) =
                crate::validation::validate_export_handle_boundaries(&inferred.export_params)
            {
                return Err(UpstreamExportSourceError::Boundaries(boundary_errors));
            }
            inferred
                .export_params
                .into_iter()
                .map(|e| {
                    (
                        e.name,
                        UpstreamExportEntry {
                            static_type: UpstreamExportType::Static(
                                crate::validation::resolve_export_static_type_for_consumer(
                                    e.static_type,
                                    config.catalog(),
                                ),
                            ),
                            value: e.value,
                        },
                    )
                })
                .collect()
        }
        // `schemas` is None (legacy entry point) — forward
        // the declared type unchanged. Note that without
        // schemas there is no way to run inference, so
        // unannotated exports are marked `InferenceNotRun`.
        None => parsed
            .export_params
            .into_iter()
            .map(|e| {
                let static_type = match e.static_type {
                    Some(static_type) => {
                        UpstreamExportType::Static(ConsumerExportStaticType::Declared(
                            crate::validation::resolve_consumer_static_type(
                                &static_type,
                                config.catalog(),
                            ),
                        ))
                    }
                    None => UpstreamExportType::InferenceNotRun,
                };
                (
                    e.name,
                    UpstreamExportEntry {
                        static_type,
                        value: e.value.and_then(|expression| match expression {
                            crate::parser::BoundaryExpression::Value(value) => Some(value),
                            crate::parser::BoundaryExpression::Handle(_handle) => None,
                        }),
                    },
                )
            })
            .collect(),
    };
    Ok(entries)
}

/// Walk a parsed project and return an error for every reference whose root
/// binding is in `exports` but whose field isn't in its declared key set.
/// Also covers deferred for-iterables (e.g. `for _ in orgs.accounts`), which
/// parse into `deferred_for_expressions` rather than `Value::Deferred(DeferredValue::ResourceRef)`.
///
/// Bindings absent from `exports` are skipped — the caller decides what to
/// do about unresolved upstreams.
///
/// ```compile_fail,E0308
/// use carina_core::{parser::ParsedFile, upstream_exports::{check_upstream_state_field_references, UpstreamExports}};
/// fn analyze(file: &ParsedFile) {
///     check_upstream_state_field_references(file, &UpstreamExports::new());
/// }
/// ```
pub fn check_upstream_state_field_references<
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    B: BindingAddress,
    E,
>(
    parsed: &UpstreamAnalysis<'_, D, B, E>,
    exports: &UpstreamExports<B>,
) -> Vec<Located<UpstreamDiagnostic<UpstreamFieldError>>> {
    check_upstream_state_field_references_inner(parsed, exports, true)
}

/// Check only scopes that do not have a typed consumer and therefore cannot
/// route existence through [`crate::binding_index::BindingIndex::ref_type`].
/// Production validation uses this alongside the unified typed-reference
/// passes; the broader legacy entry point above remains useful to callers that
/// run this check in isolation.
pub fn check_upstream_state_untyped_field_references<
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    B: BindingAddress,
    E,
>(
    parsed: &UpstreamAnalysis<'_, D, B, E>,
    exports: &UpstreamExports<B>,
) -> Vec<Located<UpstreamDiagnostic<UpstreamFieldError>>> {
    check_upstream_state_field_references_inner(parsed, exports, false)
}

fn check_upstream_state_field_references_inner<
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    B: BindingAddress,
    E,
>(
    parsed: &UpstreamAnalysis<'_, D, B, E>,
    exports: &UpstreamExports<B>,
    include_typed_consumers: bool,
) -> Vec<Located<UpstreamDiagnostic<UpstreamFieldError>>> {
    let mut errors: Vec<Located<UpstreamDiagnostic<UpstreamFieldError>>> = Vec::new();

    // One &str slice per binding so a project with many bad refs doesn't
    // re-materialize the same Vec for every error.
    let known_by_binding: HashMap<&str, Vec<&str>> = exports
        .iter()
        .map(|(b, keys)| (b.as_str(), keys.keys().map(String::as_str).collect()))
        .collect();

    let mut check = |reference: &AnalysisReference<B>, usage, context: &str| {
        if usage == crate::resource::ResourceRefUsage::CoalesceValue {
            return;
        }
        errors.extend(reference.collect_diagnostics(|path, errors| {
            let binding = path.binding_address();
            let field = path.attribute();
            let Some(keys) = exports.get(binding) else {
                return;
            };
            if keys.contains_key(field) {
                return;
            }
            let known = known_by_binding
                .get(binding.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            errors.push(Located::new(
                reference.prefix_location().clone(),
                UpstreamDiagnostic::from(UpstreamFieldError {
                    location: context.to_string(),
                    binding: binding.to_string(),
                    field: field.to_string(),
                    suggestion: suggest_similar_name(field, known, &[]),
                }),
            ));
        }));
    };
    for occurrence in parsed.upstream_field_references() {
        if include_typed_consumers || !occurrence.typed_consumer() {
            check(
                occurrence.reference(),
                occurrence.usage(),
                occurrence.context(),
            );
        }
    }
    for deferred in parsed.for_iterables() {
        let context = format!("for-expression `{}`", deferred.header());
        for (reference, usage) in deferred.reference_occurrences() {
            check(reference, usage, &context);
        }
    }

    // Preserve the established grouping by source location, binding, and field.
    errors.sort_by(|a, b| {
        (a.detail().location(), a.binding(), a.field()).cmp(&(
            b.detail().location(),
            b.binding(),
            b.field(),
        ))
    });
    errors
}

use crate::validation::walk_type_expr_fields;

/// A `for` expression iterates an `upstream_state` field whose declared
/// export type doesn't match the binding pattern's expected shape:
/// `for x in ...` requires a list, `for k, v in ...` requires a map.
///
/// This is the shape side of #1894 — surfacing pending upstream
/// `list ↔ map` migrations in the downstream's plan/validate output
/// instead of letting them blow up at apply time.
#[derive(Debug, Clone)]
pub struct UpstreamForIterableShapeError {
    pub location: String,
    pub binding: String,
    pub field: String,
    /// Declared export type — what the upstream's `exports.crn` says.
    pub export_type: TypeExpr,
    /// What kind of binding the downstream `for` introduces.
    pub binding_kind: ForIterableBindingKind,
}

/// Coarse classification of a `ForBinding` for cross-directory shape
/// compatibility. `Simple`/`Indexed` both require a list; `Map` requires
/// a map. The full `ForBinding` is intentionally not stored here — the
/// diagnostic only cares about the shape, not the variable names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForIterableBindingKind {
    /// `for x in ...` or `for (i, x) in ...` — both consume a list.
    List,
    /// `for k, v in ...` — consumes a map.
    Map,
}

impl ForIterableBindingKind {
    fn from_for_binding(binding: &crate::parser::ForBinding) -> Self {
        use crate::parser::ForBinding;
        match binding {
            ForBinding::Simple(_) | ForBinding::Indexed(_, _) => ForIterableBindingKind::List,
            ForBinding::Map(_, _) => ForIterableBindingKind::Map,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ForIterableBindingKind::List => "list",
            ForIterableBindingKind::Map => "map",
        }
    }
}

impl UpstreamForIterableShapeError {
    pub fn diagnostic_message(&self) -> String {
        // Suggestion text depends on what the upstream actually exports,
        // not just on what the binding expected: a `for x in scalar` has
        // no valid binding form to suggest at all.
        let suggested = match &self.export_type {
            TypeExpr::List(_) => match self.binding_kind {
                ForIterableBindingKind::List => "",
                ForIterableBindingKind::Map => "; use `for x in ...` to iterate the list",
            },
            TypeExpr::Map(_) => match self.binding_kind {
                ForIterableBindingKind::List => "; use `for k, v in ...` to iterate the map",
                ForIterableBindingKind::Map => "",
            },
            // Scalar exports can't be iterated at all; saying nothing is
            // honest. The upstream contract has to change first.
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::UnloadedSchemaType(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Struct { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_) => "",
        };
        format!(
            "upstream_state `{}.{}` is declared as `{}` but `{}` requires a {} iterable{}",
            self.binding,
            self.field,
            self.export_type,
            self.location,
            self.binding_kind.as_str(),
            suggested,
        )
    }
}

impl std::fmt::Display for UpstreamForIterableShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.diagnostic_message())
    }
}

impl std::error::Error for UpstreamForIterableShapeError {}

impl UpstreamRefDiagnostic for UpstreamForIterableShapeError {
    fn location(&self) -> &str {
        &self.location
    }
    fn binding(&self) -> &str {
        &self.binding
    }
    fn field(&self) -> &str {
        &self.field
    }
    fn diagnostic_message(&self) -> String {
        UpstreamForIterableShapeError::diagnostic_message(self)
    }
}

/// Walk every `for` expression that iterates an `upstream_state` field
/// reference and emit an error when the export's declared type
/// (`list(...)` vs `map(...)`) doesn't match the binding pattern's
/// expected shape.
///
/// Skipped silently when:
/// - the binding isn't in `exports` (already surfaced by
///   `check_upstream_state_field_references` if it should be);
/// - the field isn't exported (same — duplicate diagnostics hurt);
/// - the export has no declared type (`accounts` without `: T`) — there's
///   no upstream type to compare against, and the field-name path
///   handles existence.
///
/// This is the type-level sibling of the runtime check in
/// `parser::ParsedFile::expand_deferred_for_expressions` (`parser/ast.rs`).
/// Both flag `(ForBinding × shape)` mismatches; this one fires at
/// validate time off the upstream's *declared* type, the parser-side
/// one fires at expansion time off the *resolved* `Value`.
pub fn check_upstream_state_for_iterable_shapes<D, B, E>(
    parsed: &UpstreamAnalysis<'_, D, B, E>,
    exports: &UpstreamExports<B>,
) -> Vec<Located<UpstreamDiagnostic<UpstreamForIterableShapeError>>>
where
    B: BindingAddress,
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    let mut errors: Vec<Located<UpstreamDiagnostic<UpstreamForIterableShapeError>>> = Vec::new();
    for deferred in parsed.for_iterables() {
        let binding_kind = ForIterableBindingKind::from_for_binding(deferred.binding());
        for reference in deferred.result_references() {
            errors.extend(reference.collect_diagnostics(|path, errors| {
                let Some(fields) = exports.get(path.binding_address()) else {
                    return;
                };
                let Some(entry) = fields.get(path.attribute()) else {
                    return;
                };
                let Some(export_type) = entry.static_type.declared_known() else {
                    return;
                };
                let export_kind = match export_type {
                    TypeExpr::List(_) => Some(ForIterableBindingKind::List),
                    TypeExpr::Map(_) => Some(ForIterableBindingKind::Map),
                    TypeExpr::String
                    | TypeExpr::Bool
                    | TypeExpr::Int
                    | TypeExpr::Float
                    | TypeExpr::Duration
                    | TypeExpr::Simple(_)
                    | TypeExpr::UnloadedSchemaType(_)
                    | TypeExpr::Ref(_)
                    | TypeExpr::DottedUnresolved(_)
                    | TypeExpr::SchemaType { .. }
                    | TypeExpr::Struct { .. }
                    | TypeExpr::StringLiteral(_)
                    | TypeExpr::Union(_) => None,
                };
                // Scalars feeding a `for` are a different kind of bug; the
                // shape-check fires on them too because no kind matches.
                if export_kind == Some(binding_kind) {
                    return;
                }
                errors.push(Located::new(
                    reference.prefix_location().clone(),
                    UpstreamDiagnostic::from(UpstreamForIterableShapeError {
                        location: format!("for-expression `{}`", deferred.header()),
                        binding: path.binding().to_string(),
                        field: path.attribute().to_string(),
                        export_type: export_type.clone(),
                        binding_kind,
                    }),
                ));
            }));
        }
    }
    errors.sort_by(|a, b| {
        (a.detail().location(), a.binding(), a.field()).cmp(&(
            b.detail().location(),
            b.binding(),
            b.field(),
        ))
    });
    errors
}

/// A reference walks past the top-level upstream export with a `.field`
/// chain that doesn't match the export's declared `TypeExpr` — the
/// downstream wrote `orgs.account.foo` but `account`'s declared
/// `TypeExpr` is a `list(...)` / `map(...)` / scalar (no fields), or
/// a `Struct{...}` whose fields don't include `foo`.
///
/// Sibling of [`UpstreamForIterableShapeError`] from #2317 — same family
/// of "downstream's usage doesn't fit the upstream's declared shape",
/// different consumer (attribute access vs `for` iterable). The two
/// share the same detection-layer style: walk the parsed project,
/// look up the declared `TypeExpr` per `(binding, field)`, and emit a
/// structured diagnostic when the usage doesn't fit.
#[derive(Debug, Clone)]
pub struct UpstreamAttributeAccessShapeError {
    pub location: String,
    pub binding: String,
    pub field: String,
    /// Field-path segments after `binding.field`, exactly as the
    /// downstream wrote them. Kept in full for code actions that want
    /// to render the original access path.
    pub field_path: Vec<String>,
    /// What the upstream declared at the deepest segment that *did*
    /// resolve. For `orgs.account.network.bad_field` that's the
    /// `Struct{...}` for `network`; for `orgs.accounts.foo` against a
    /// `list(...)` it's the `list(...)` itself.
    pub mismatched_at: TypeExpr,
    /// The first segment in `field_path` that didn't fit
    /// `mismatched_at`. Stored directly rather than as an index because
    /// no consumer currently needs the position back into `field_path`.
    pub bad_segment: String,
}

impl UpstreamAttributeAccessShapeError {
    pub fn diagnostic_message(&self) -> String {
        match &self.mismatched_at {
            TypeExpr::Struct { fields } => {
                let known: Vec<&str> = fields.iter().map(|(name, _)| name.as_str()).collect();
                format!(
                    "upstream_state `{}.{}` has no field `{}`; declared fields are: {}",
                    self.binding,
                    self.field,
                    self.bad_segment,
                    known.join(", "),
                )
            }
            TypeExpr::List(_) => format!(
                "upstream_state `{}.{}` is declared as `{}` but `.{}` reads it as a struct; iterate the list with `for x in {}.{}` to access elements",
                self.binding,
                self.field,
                self.mismatched_at,
                self.bad_segment,
                self.binding,
                self.field,
            ),
            TypeExpr::Map(_) => format!(
                "upstream_state `{}.{}` is declared as `{}` but `.{}` reads it as a struct; iterate the map with `for k, v in {}.{}` to access entries",
                self.binding,
                self.field,
                self.mismatched_at,
                self.bad_segment,
                self.binding,
                self.field,
            ),
            other @ (TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::UnloadedSchemaType(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_)) => format!(
                "upstream_state `{}.{}` is declared as `{}` (a scalar), but `.{}` reads it as a struct; scalars have no fields",
                self.binding, self.field, other, self.bad_segment,
            ),
        }
    }
}

impl std::fmt::Display for UpstreamAttributeAccessShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.diagnostic_message())
    }
}

impl std::error::Error for UpstreamAttributeAccessShapeError {}

impl UpstreamRefDiagnostic for UpstreamAttributeAccessShapeError {
    fn location(&self) -> &str {
        &self.location
    }
    fn binding(&self) -> &str {
        &self.binding
    }
    fn field(&self) -> &str {
        &self.field
    }
    fn diagnostic_message(&self) -> String {
        UpstreamAttributeAccessShapeError::diagnostic_message(self)
    }
}

/// Walk every `Value::Deferred(DeferredValue::ResourceRef)` whose root is an `upstream_state`
/// binding and whose `field_path` is non-empty, and emit an error
/// whenever a path segment doesn't fit the declared upstream
/// `TypeExpr`.
///
/// Skipped silently when:
/// - the binding isn't in `exports` (already surfaced by
///   `check_upstream_state_field_references` if it should be);
/// - the field isn't exported (same — duplicate diagnostics hurt);
/// - the export has no declared type (`account` without `: T`) — there's
///   no upstream type to compare the path against;
/// - the `field_path` is empty — that's a top-level field access,
///   handled by the typed consumer through `BindingIndex::ref_type`.
///
/// Sibling of [`check_upstream_state_for_iterable_shapes`] from #2317;
/// both surface "downstream usage doesn't fit upstream's declared
/// shape" before apply.
pub fn check_upstream_state_attribute_access_shapes<
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    B: BindingAddress,
    E,
>(
    parsed: &UpstreamAnalysis<'_, D, B, E>,
    exports: &UpstreamExports<B>,
) -> Vec<Located<UpstreamDiagnostic<UpstreamAttributeAccessShapeError>>> {
    check_upstream_state_attribute_access_shapes_inner(parsed, exports, true)
}

/// Check only attribute-access shapes that
/// [`crate::binding_index::BindingIndex::ref_type`] cannot represent.
///
/// The unified resolver reports missing fields on declared structs itself.
/// List, map, and scalar values reached through field syntax resolve to
/// `Unchecked`, so this fallback retains their more specific upstream shape
/// diagnostics without reporting a struct-field typo twice.
pub fn check_upstream_state_attribute_access_shape_fallbacks<
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    B: BindingAddress,
    E,
>(
    parsed: &UpstreamAnalysis<'_, D, B, E>,
    exports: &UpstreamExports<B>,
) -> Vec<Located<UpstreamDiagnostic<UpstreamAttributeAccessShapeError>>> {
    check_upstream_state_attribute_access_shapes_inner(parsed, exports, false)
}

fn check_upstream_state_attribute_access_shapes_inner<
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    B: BindingAddress,
    E,
>(
    parsed: &UpstreamAnalysis<'_, D, B, E>,
    exports: &UpstreamExports<B>,
    include_resolver_struct_errors: bool,
) -> Vec<Located<UpstreamDiagnostic<UpstreamAttributeAccessShapeError>>> {
    let mut errors: Vec<Located<UpstreamDiagnostic<UpstreamAttributeAccessShapeError>>> =
        Vec::new();
    for occurrence in parsed.project_value_references() {
        visit_attribute_access(
            occurrence.reference(),
            exports,
            occurrence.context(),
            include_resolver_struct_errors,
            &mut errors,
        );
    }
    for deferred in parsed.for_iterables() {
        for reference in deferred.references() {
            visit_attribute_access(
                reference,
                exports,
                &format!("for-expression `{}`", deferred.header()),
                include_resolver_struct_errors,
                &mut errors,
            );
        }
    }
    errors.sort_by(|a, b| {
        (a.detail().location(), a.binding(), a.field()).cmp(&(
            b.detail().location(),
            b.binding(),
            b.field(),
        ))
    });
    errors
}

/// Walk a Value tree, find every `ResourceRef` with a non-empty
/// `field_path`, and check the path against the upstream's declared
/// `TypeExpr`.
fn visit_attribute_access<B: BindingAddress>(
    reference: &AnalysisReference<B>,
    exports: &UpstreamExports<B>,
    location: &str,
    include_resolver_struct_errors: bool,
    errors: &mut Vec<Located<UpstreamDiagnostic<UpstreamAttributeAccessShapeError>>>,
) {
    errors.extend(reference.collect_diagnostics(|path, errors| {
        let leading: Vec<String> = path.leading_field_path();
        if leading.is_empty() {
            return;
        }
        let binding = path.binding_address();
        let attribute = path.attribute();
        let Some(fields) = exports.get(binding) else {
            return;
        };
        let Some(entry) = fields.get(attribute) else {
            return;
        };
        let Some(export_type) = entry.static_type.declared_known() else {
            return;
        };
        let walk_result = walk_type_expr_fields(export_type, &leading);
        if let Err((mismatched_at, bad_segment)) = walk_result {
            if !include_resolver_struct_errors && matches!(mismatched_at, TypeExpr::Struct { .. }) {
                return;
            }
            let mismatched_at = mismatched_at.clone();
            let bad_segment = bad_segment.to_string();
            errors.push(Located::new(
                reference.prefix_location().clone(),
                UpstreamDiagnostic::from(UpstreamAttributeAccessShapeError {
                    location: location.to_string(),
                    binding: binding.to_string(),
                    field: attribute.to_string(),
                    field_path: leading,
                    mismatched_at,
                    bad_segment,
                }),
            ));
        }
    }));
}

/// A `[index]` subscript reads an `upstream_state` export at a depth
/// where the declared `TypeExpr` doesn't permit that subscript shape:
/// integer subscript against `map(_)` / `Struct{...}` / scalar, or
/// string subscript against `list(_)` / `Struct{...}` / scalar.
#[derive(Debug, Clone)]
pub struct UpstreamSubscriptShapeError {
    pub location: String,
    pub binding: String,
    pub field: String,
    /// Field-chain segments between `binding.field` and the offending
    /// subscript. Empty when the subscript sits directly on the
    /// top-level export (`orgs.accounts[0]`).
    pub field_path: Vec<String>,
    /// What the upstream declared at the position the bad subscript
    /// reads. For `orgs.accounts[0]` against `accounts: map(_)` this
    /// is the `map(_)`; for `orgs.region[0]` it's the scalar.
    pub mismatched_at: TypeExpr,
    /// The literal subscript the user wrote — preserved so the
    /// diagnostic echoes their own syntax (`[0]` / `["alpha"]`)
    /// rather than a placeholder.
    pub bad_subscript: Subscript,
}

impl UpstreamSubscriptShapeError {
    pub fn diagnostic_message(&self) -> String {
        let mut access = format!("{}.{}", self.binding, self.field);
        for seg in &self.field_path {
            access.push('.');
            access.push_str(seg);
        }
        let mut bad = String::new();
        self.bad_subscript.append_to_dot_string(&mut bad);
        let suggestion = match &self.mismatched_at {
            TypeExpr::List(_) => match &self.bad_subscript {
                Subscript::Int { .. } => "",
                Subscript::Str { .. } => "; integer subscript `[i]` reads list elements",
            },
            TypeExpr::Map(_) => match &self.bad_subscript {
                Subscript::Int { .. } => "; string subscript `[\"k\"]` reads map entries",
                Subscript::Str { .. } => "",
            },
            // Struct/scalar don't host any subscript form, so the
            // "use the other syntax" hint would be misleading. Stay
            // silent and let the caller show the declared type.
            TypeExpr::String
            | TypeExpr::Bool
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Duration
            | TypeExpr::Simple(_)
            | TypeExpr::UnloadedSchemaType(_)
            | TypeExpr::Ref(_)
            | TypeExpr::DottedUnresolved(_)
            | TypeExpr::SchemaType { .. }
            | TypeExpr::Struct { .. }
            | TypeExpr::StringLiteral(_)
            | TypeExpr::Union(_) => "",
        };
        format!(
            "upstream_state `{}` is declared as `{}`; `{}{}` does not fit{}",
            access, self.mismatched_at, access, bad, suggestion,
        )
    }
}

impl std::fmt::Display for UpstreamSubscriptShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.diagnostic_message())
    }
}

impl std::error::Error for UpstreamSubscriptShapeError {}

impl UpstreamRefDiagnostic for UpstreamSubscriptShapeError {
    fn location(&self) -> &str {
        &self.location
    }
    fn binding(&self) -> &str {
        &self.binding
    }
    fn field(&self) -> &str {
        &self.field
    }
    fn diagnostic_message(&self) -> String {
        UpstreamSubscriptShapeError::diagnostic_message(self)
    }
}

/// Walk every `Value::Deferred(DeferredValue::ResourceRef)` whose root is an `upstream_state`
/// binding and whose `subscripts` chain is non-empty, and emit an error
/// whenever a subscript's kind (integer vs string) doesn't fit the
/// declared upstream `TypeExpr` at that depth.
///
/// Skipped silently when:
/// - the binding isn't in `exports` (already surfaced by
///   `check_upstream_state_field_references` if it should be);
/// - the field isn't exported (same — duplicate diagnostics hurt);
/// - the export has no declared type (`accounts` without `: T`) — there's
///   no upstream type to compare the subscript against;
/// - the leading `field_path` already mismatches the declared shape
///   (already handled by `check_upstream_state_attribute_access_shapes`)
///   — in that case the field-walk diagnostic is enough.
pub fn check_upstream_state_subscript_shapes<
    D: crate::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    B: BindingAddress,
    E,
>(
    parsed: &UpstreamAnalysis<'_, D, B, E>,
    exports: &UpstreamExports<B>,
) -> Vec<Located<UpstreamDiagnostic<UpstreamSubscriptShapeError>>> {
    let mut errors: Vec<Located<UpstreamDiagnostic<UpstreamSubscriptShapeError>>> = Vec::new();
    for occurrence in parsed.project_value_references() {
        visit_subscript_access(
            occurrence.reference(),
            exports,
            occurrence.context(),
            &mut errors,
        );
    }
    for deferred in parsed.for_iterables() {
        for reference in deferred.references() {
            visit_subscript_access(
                reference,
                exports,
                &format!("for-expression `{}`", deferred.header()),
                &mut errors,
            );
        }
    }
    errors.sort_by(|a, b| {
        (a.detail().location(), a.binding(), a.field()).cmp(&(
            b.detail().location(),
            b.binding(),
            b.field(),
        ))
    });
    errors
}

/// Walk a Value tree, find every `ResourceRef` with a non-empty
/// `subscripts` chain, and check each subscript's kind against the
/// upstream's declared `TypeExpr` at that depth.
fn visit_subscript_access<B: BindingAddress>(
    reference: &AnalysisReference<B>,
    exports: &UpstreamExports<B>,
    location: &str,
    errors: &mut Vec<Located<UpstreamDiagnostic<UpstreamSubscriptShapeError>>>,
) {
    errors.extend(reference.collect_diagnostics(|path, errors| {
        // Chained access (`cert.foo[0].bar`) is not yet exercised by
        // this checker — see #3025 for the planned segments-driven
        // walker. Skip the path here; the resolver still surfaces
        // typos at apply time and the simple-shape paths remain
        // covered.
        if !path.is_simple_shape() {
            return;
        }
        let subscripts = path.trailing_subscripts();
        if subscripts.is_empty() {
            return;
        }
        let binding = path.binding_address();
        let attribute = path.attribute();
        let Some(fields) = exports.get(binding) else {
            return;
        };
        let Some(entry) = fields.get(attribute) else {
            return;
        };
        let Some(export_type) = entry.static_type.declared_known() else {
            return;
        };
        // If the field path itself doesn't resolve, the
        // attribute-access check will already report it. Skip here so
        // we don't double-fire.
        let field_path: Vec<String> = path.leading_field_path();
        let Ok(at_field_end) = walk_type_expr_fields(export_type, &field_path) else {
            return;
        };
        // Step through each subscript, descending into the element
        // type when it fits and emitting at the first kind mismatch.
        let mut current = at_field_end;
        for sub in &subscripts {
            let next = match current {
                TypeExpr::List(inner) => match sub {
                    Subscript::Int { .. } => Some(inner.as_ref()),
                    Subscript::Str { .. } => None,
                },
                TypeExpr::Map(inner) => match sub {
                    Subscript::Int { .. } => None,
                    Subscript::Str { .. } => Some(inner.as_ref()),
                },
                TypeExpr::String
                | TypeExpr::Bool
                | TypeExpr::Int
                | TypeExpr::Float
                | TypeExpr::Duration
                | TypeExpr::Simple(_)
                | TypeExpr::UnloadedSchemaType(_)
                | TypeExpr::Ref(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { .. }
                | TypeExpr::Struct { .. }
                | TypeExpr::StringLiteral(_)
                | TypeExpr::Union(_) => None,
            };
            let Some(next) = next else {
                errors.push(Located::new(
                    reference.prefix_location().clone(),
                    UpstreamDiagnostic::from(UpstreamSubscriptShapeError {
                        location: location.to_string(),
                        binding: binding.to_string(),
                        field: attribute.to_string(),
                        field_path: field_path.clone(),
                        mismatched_at: current.clone(),
                        bad_subscript: sub.clone(),
                    }),
                ));
                return;
            };
            current = next;
        }
    }));
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn non_utf8_upstream_require_rejection_is_not_omitted() {
        use std::os::unix::ffi::OsStringExt;
        let directory = tempfile::tempdir().unwrap();
        let upstream_dir = directory.path().join("upstream");
        std::fs::create_dir(&upstream_dir).unwrap();
        std::fs::write(
            upstream_dir.join("exports.crn"),
            "exports { value: Int = 1 }\n",
        )
        .unwrap();
        std::fs::write(
            upstream_dir.join("backend.crn"),
            "backend local { path = 'state.json' }\n",
        )
        .unwrap();
        let guard = upstream_dir.join(std::ffi::OsString::from_vec(b"\xff.crn".to_vec()));
        let physical = std::fs::write(&guard, "require false, 'upstream guard'\n").is_ok();
        let overrides = crate::config_loader::SourceOverrides::from([(
            guard.clone(),
            "require false, 'upstream guard'\n".to_string(),
        )]);
        let project = upstream_project(&[upstream("prior", "./upstream")]);
        let context = ctx();
        crate::validation::deferred_types::with_session_for_test(&context, |session| {
            let (_, errors) = super::resolve_upstream_exports_report_with_overrides(
                &ResolvedPath::new(directory.path()).unwrap(),
                &AnalysisTree::try_from_file(&project).unwrap(),
                session,
                None,
                &overrides,
            )
            .unwrap();
            assert_eq!(errors.len(), 1, "upstream require rejection was omitted");
            let (_, error) = errors[0].detail().module_failure().unwrap();
            let crate::module_resolver::ModuleError::RequireOutsideModule(finding) =
                error.root_cause()
            else {
                panic!("expected a located require-outside-module rejection: {error}");
            };
            assert_eq!(finding.location().file().path(), guard);
            assert_eq!(finding.location().span().start_line(), 1);
            assert_eq!(finding.location().span().start_column(), 1);
            assert_eq!(finding.location().span().end_column(), 32);
            assert_eq!(
                finding.detail().to_string(),
                format!(
                    "`require` is only allowed inside a module\nNote: directory is a root configuration because of the backend block at {}:1.",
                    crate::paths::DisplayPath::for_diagnostics(upstream_dir.join("backend.crn"))
                )
            );
            if physical {
                let (_, errors) = super::resolve_upstream_exports_report_with_overrides(
                    &ResolvedPath::new(directory.path()).unwrap(),
                    &AnalysisTree::try_from_file(&project).unwrap(),
                    session,
                    None,
                    &Default::default(),
                )
                .unwrap();
                assert_eq!(errors.len(), 1);
            }
        });
    }
    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
    };

    use super::*;
    use crate::parser::ParsedFile;
    use crate::resource::DeferredValue;
    use crate::schema::SchemaRegistry;
    use crate::static_type::ConsumerExportStaticType as ExportStaticType;
    use std::fs;
    use std::path::PathBuf;

    fn write_crn(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    fn ctx() -> LoadedProviderContext {
        LoadedProviderContext::empty_for_test()
    }

    #[test]
    fn round12_identity_load_rejects_module_templates_with_typed_evidence() {
        for declaration in ["arguments {}", "require false, 'PRIVATE_GUARD'"] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("mod");
            fs::create_dir(&source).unwrap();
            write_crn(
                &source,
                "declarations.crn",
                &format!("// module\n{declaration}\n"),
            );
            write_crn(&source, "locals.crn", "// sibling\n");
            let context = ctx();
            let base = ResolvedPath::new(directory.path()).unwrap();
            let caller = crate::parser::parse_file(
                "let prior = upstream_state { source = './mod' }\n",
                &directory.path().join("caller.crn"),
                &context,
            )
            .unwrap();
            let identity =
                UpstreamSourceIdentity::resolve(&caller.upstream_states[0], &base).unwrap();
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                let error = identity
                    .load(
                        &UpstreamBootstrapContext::from_runtime(&context),
                        session,
                        &SchemaRegistry::new(),
                    )
                    .unwrap_err();
                let message = error.to_string();
                assert!(
                    message
                        .contains("source `./mod` is a module definition and has no state to read"),
                    "{message}"
                );
                assert!(message.contains("mod/declarations.crn:2"), "{message}");
                assert!(!message.contains("PRIVATE_GUARD"), "{message}");
            });
        }
    }

    #[test]
    fn round12_legacy_template_failure_matches_display() {
        let directory = tempfile::tempdir().unwrap();
        write_crn(directory.path(), "arguments.crn", "arguments {}\n");
        write_crn(directory.path(), "locals.crn", "// sibling\n");
        let parsed = crate::config_loader::parse_directory(
            &ResolvedPath::new(directory.path()).unwrap(),
            &ctx(),
        )
        .unwrap();
        let crate::config_loader::DirectoryVerdict::ModuleTemplate(template) =
            parsed.complete_directory_verdict()
        else {
            panic!("template");
        };
        let failure =
            UpstreamResolveFailure::module_template("prior".into(), "../mod".into(), template);
        assert_eq!(
            failure.clone().into_legacy().to_string(),
            failure.to_string()
        );
    }

    #[test]
    fn module_template_upstream_rejection_retains_the_first_issued_evidence() {
        for declaration in [
            "arguments {}",
            "attributes {}",
            "require false, 'PRIVATE_GUARD'",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let evidence_path = directory.path().join("a-declarations.crn");
            fs::write(
                &evidence_path,
                format!("// first module evidence\n{declaration}\n"),
            )
            .unwrap();
            write_crn(
                directory.path(),
                "z-later.crn",
                "require true, 'later evidence'\n",
            );
            let root = ResolvedPath::new(directory.path()).unwrap();
            let context = ctx();
            let parsed = crate::config_loader::parse_directory(&root, &context).unwrap();
            let crate::config_loader::DirectoryVerdict::ModuleTemplate(template) =
                parsed.complete_directory_verdict()
            else {
                panic!("expected module template");
            };
            let evidence = template.evidence().clone();
            assert_eq!(evidence.file_locator().to_path_buf(), evidence_path);
            assert_eq!(evidence.location().span().start_line(), 2);
            // Removing the authored file after admission must not change the
            // verdict or cause a consumer to search for replacement evidence.
            fs::remove_file(evidence_path).unwrap();
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                let error = resolve_upstream_exports_from_parsed(
                    parsed,
                    &root,
                    session,
                    None,
                    &Default::default(),
                )
                .unwrap_err();
                let UpstreamExportSourceError::ModuleTemplate(template) = error else {
                    panic!("expected typed module-template rejection: {error}");
                };
                assert_eq!(template.evidence(), &evidence);
            });
        }
    }

    fn round4_discovery_authority() -> (LoadedProviderContext, SchemaRegistry) {
        let identity = crate::schema::TypeIdentity::from_schema_type("test", "types", "Identifier");
        let owned = crate::parser::OwnedTypeIdentity::provider(
            "test",
            identity,
            crate::schema::RefinedScalarKind::String,
        );
        let mut catalog = ProviderContext::new(crate::EnvironmentSnapshot::new([(
            "ROUND4_VALUE".into(),
            "captured-value".into(),
        )]));
        catalog.decryptor = Some(std::sync::Arc::new(|_, _| Ok("decrypted-value".into())));
        catalog.custom_types = crate::parser::ProviderCustomTypes::loaded(
            std::collections::HashSet::from([owned.clone()]),
            Box::new(|_target, _value| Ok(())),
        );
        catalog
            .resource_types
            .insert(("test".into(), "Item".into()));
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            PatternResourceSchema::new("Item")
                .attribute(
                    PatternAttributeSchema::provider_populated(
                        "identifier",
                        PatternAttributeType::refined_string(
                            Some(owned.identity().clone()),
                            None,
                            None,
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        (LoadedProviderContext::for_test(catalog), schemas)
    }

    fn round4_directory(source: &str) -> (tempfile::TempDir, ResolvedPath) {
        let directory = tempfile::tempdir().unwrap();
        let upstream = directory.path().join("upstream");
        let downstream = directory.path().join("downstream");
        fs::create_dir(&upstream).unwrap();
        fs::create_dir(&downstream).unwrap();
        write_crn(&upstream, "exports.crn", source);
        write_crn(
            &downstream,
            "main.crn",
            "let ups = upstream_state { source = '../upstream' }\n",
        );
        let base = ResolvedPath::new(downstream).unwrap();
        (directory, base)
    }

    fn round4_directory_discovery(
        source: &str,
        context: &LoadedProviderContext,
        schemas: &SchemaRegistry,
    ) -> (UpstreamExports, Vec<Located<UpstreamResolveError>>) {
        let (_directory, base) = round4_directory(source);
        round4_loaded_directory_discovery(&base, context, schemas)
    }

    fn round4_loaded_directory_discovery(
        base: &ResolvedPath,
        context: &LoadedProviderContext,
        schemas: &SchemaRegistry,
    ) -> (UpstreamExports, Vec<Located<UpstreamResolveError>>) {
        let parsed = crate::config_loader::parse_directory_for_metadata(
            base,
            context,
            &Default::default(),
            schemas,
        )
        .unwrap();
        let tree = AnalysisTree::try_from_file(parsed.parsed()).unwrap();
        crate::validation::deferred_types::with_session_for_test(context, |session| {
            super::resolve_upstream_exports_with_schemas(base, &tree, session, Some(schemas))
                .unwrap()
        })
    }

    #[test]
    fn round4_discovery_preserves_known_schema_export_types() {
        let (context, schemas) = round4_discovery_authority();
        let (exports, errors) = round4_directory_discovery(
            "let item = test.Item {}\nexports { declared: test.types.Identifier = item.identifier\ninferred = item.identifier }\n",
            &context,
            &schemas,
        );
        assert!(errors.is_empty(), "{errors:?}");
        for key in ["declared", "inferred"] {
            let UpstreamExportType::Static(static_type) = &exports["ups"][key].static_type else {
                panic!("source inference must run for {key}");
            };
            assert_eq!(
                static_type.known().map(ToString::to_string).as_deref(),
                Some("test.types.Identifier"),
                "{key}: {static_type:?}"
            );
        }
    }

    #[test]
    fn round4_discovery_reports_unknown_source_type_as_on_main() {
        let (context, schemas) = round4_discovery_authority();
        let (exports, errors) = round4_directory_discovery(
            "exports { value: Foo = 'upstream-only' }\n",
            &context,
            &schemas,
        );
        assert!(!exports.contains_key("ups"), "{exports:?}");
        assert_eq!(errors.len(), 1, "{errors:?}");
        let error = &errors[0];
        assert_eq!(error.binding, "ups");
        assert_eq!(error.source, PathBuf::from("../upstream"));
        assert!(
            error
                .reason
                .ends_with("exports.crn: Invalid expression at line 1: unknown custom type 'Foo'"),
            "{error}"
        );
        assert!(
            error
                .detail()
                .to_string()
                .starts_with("upstream_state `ups`: failed to parse source `../upstream`: ")
        );
    }

    #[test]
    fn round4_discovery_registers_resource_type_argument_but_rejects_template_source() {
        let (context, schemas) = round4_discovery_authority();
        let source = "arguments { item: test.Item }\nexports { identifier = item.identifier }\n";
        let (directory, base) = round4_directory(source);
        crate::validation::deferred_types::with_session_for_test(&context, |session| {
            let discovery = UpstreamDiscoveryContext {
                context: session,
                schemas: Some(&schemas),
            };
            let parsed = discovery
                .parse_source(&base.join("../upstream").unwrap(), &Default::default())
                .unwrap();
            assert!(
                parsed
                    .parser_binding_metadata
                    .structural_handles()
                    .contains_key("item"),
                "the resource argument must register as a structural handle"
            );
        });
        let (exports, errors) = round4_loaded_directory_discovery(&base, &context, &schemas);
        assert!(exports.is_empty(), "{exports:?}");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(
            errors[0].detail().reason,
            format!(
                "is a module definition and has no state to read\nNote: directory is a module definition because of the arguments block at {}:1.",
                crate::paths::DisplayPath::for_diagnostics(
                    directory.path().join("upstream/exports.crn")
                )
            )
        );
    }

    #[test]
    fn upstream_root_infers_a_called_modules_resource_argument() {
        let (context, schemas) = round4_discovery_authority();
        let (directory, base) = round4_directory(
            "let item = test.Item {}\nlet reader = use { source = './reader' }\nlet result = reader { item = item }\nexports { identifier = result.identifier }\n",
        );
        let module = directory.path().join("upstream/reader");
        fs::create_dir(&module).unwrap();
        write_crn(&module, "arguments.crn", "arguments { item: test.Item }\n");
        write_crn(
            &module,
            "attributes.crn",
            "attributes { identifier = item.identifier }\n",
        );
        let (exports, errors) = round4_loaded_directory_discovery(&base, &context, &schemas);
        assert!(errors.is_empty(), "{errors:?}");
        let UpstreamExportType::Static(ExportStaticType::Inferred(StaticType::Known(known))) =
            &exports["ups"]["identifier"].static_type
        else {
            panic!("resource argument member must be inferable: {exports:?}");
        };
        assert_eq!(known.type_expr().to_string(), "test.types.Identifier");
    }

    #[test]
    fn round4_bootstrap_from_loaded_catalog_retains_only_runtime_hooks() {
        let (catalog, _schemas) = round4_discovery_authority();
        assert!(catalog.custom_types_loaded());
        assert!(!catalog.known_custom_types().is_empty());
        assert!(!catalog.resource_types.is_empty());

        let bootstrap = UpstreamBootstrapContext::from_runtime(&catalog);
        assert!(!bootstrap.0.custom_types_loaded());
        assert!(bootstrap.0.known_custom_types().is_empty());
        assert!(bootstrap.0.resource_types.is_empty());
        assert_eq!(
            bootstrap.0.environment.get("ROUND4_VALUE"),
            catalog.environment.get("ROUND4_VALUE"),
        );
        assert!(std::sync::Arc::ptr_eq(
            bootstrap.0.decryptor.as_ref().unwrap(),
            catalog.decryptor.as_ref().unwrap(),
        ));
    }

    fn consumer_type(type_expr: TypeExpr) -> crate::static_type::ConsumerStaticType {
        crate::validation::resolve_consumer_static_type_for_signature(
            &type_expr,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
    }

    fn consumer_inferred_type(
        type_expr: TypeExpr,
    ) -> crate::static_type::ConsumerInferredStaticType {
        match consumer_type(type_expr) {
            StaticType::Known(resolved) => StaticType::Known(resolved),
            StaticType::Unchecked(reason) => StaticType::Unchecked(
                crate::static_type::ConsumerInferredUncheckedReason::UnnameableInConsumer(reason),
            ),
        }
    }

    fn upstream_project(states: &[UpstreamState]) -> ParsedFile {
        let source = states
            .iter()
            .map(|state| {
                format!(
                    "let {} = upstream_state {{ source = {} }}\n",
                    state.binding,
                    crate::parser::display_string_literal(&state.source.to_string_lossy())
                )
            })
            .collect::<String>();
        crate::parser::parse_file(
            &source,
            Path::new("downstream.crn"),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap()
    }
    fn resolve_upstream_exports(
        base: &ResolvedPath,
        states: &[UpstreamState],
        config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Result<
        (UpstreamExports, Vec<Located<UpstreamResolveError>>),
        crate::parser::AnalysisTreeError,
    > {
        super::resolve_upstream_exports(
            base,
            &AnalysisTree::try_from_file(&upstream_project(states))?,
            config,
        )
    }
    fn resolve_upstream_exports_with_schemas(
        base: &ResolvedPath,
        states: &[UpstreamState],
        config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
        schemas: Option<&SchemaRegistry>,
    ) -> Result<
        (UpstreamExports, Vec<Located<UpstreamResolveError>>),
        crate::parser::AnalysisTreeError,
    > {
        super::resolve_upstream_exports_with_schemas(
            base,
            &AnalysisTree::try_from_file(&upstream_project(states))?,
            config,
            schemas,
        )
    }
    fn resolve_upstream_exports_providerless_with_schemas(
        base: &ResolvedPath,
        states: &[UpstreamState],
        config: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
        schemas: Option<&SchemaRegistry>,
    ) -> Result<
        (UpstreamExports, Vec<Located<UpstreamResolveError>>),
        crate::parser::AnalysisTreeError,
    > {
        resolve_upstream_exports_with_schemas(base, states, config, schemas)
    }

    fn upstream(binding: &str, source: &str) -> UpstreamState {
        UpstreamState {
            binding: binding.into(),
            source: PathBuf::from(source),
        }
    }

    fn parse_project(source: &str) -> ParsedFile {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("main.crn"), source).unwrap();
        parse_directory(
            &crate::paths::ResolvedPath::new(tmp.path()).unwrap(),
            &ctx(),
        )
        .expect("parse_directory")
        .into_parsed()
    }

    fn inference_not_run_entry() -> UpstreamExportEntry {
        UpstreamExportEntry {
            static_type: UpstreamExportType::InferenceNotRun,
            value: None,
        }
    }

    fn declared_entry(type_expr: TypeExpr) -> UpstreamExportEntry {
        UpstreamExportEntry {
            static_type: UpstreamExportType::Static(ExportStaticType::Declared(consumer_type(
                type_expr,
            ))),
            value: None,
        }
    }

    fn public<D>(diagnostic: &Located<UpstreamDiagnostic<D>>) -> &D {
        diagnostic.public_detail().expect("public diagnostic")
    }

    fn diagnostic_fixtures(
        binding: &str,
    ) -> (
        UpstreamFieldError,
        UpstreamForIterableShapeError,
        UpstreamAttributeAccessShapeError,
        UpstreamSubscriptShapeError,
    ) {
        (
            UpstreamFieldError {
                location: "private_location".into(),
                binding: binding.into(),
                field: "private_field".into(),
                suggestion: Some("private_suggestion".into()),
            },
            UpstreamForIterableShapeError {
                location: "private_location".into(),
                binding: binding.into(),
                field: "private_field".into(),
                export_type: TypeExpr::StringLiteral("private_type".into()),
                binding_kind: ForIterableBindingKind::List,
            },
            UpstreamAttributeAccessShapeError {
                location: "private_location".into(),
                binding: binding.into(),
                field: "private_field".into(),
                field_path: vec!["private_path".into()],
                mismatched_at: TypeExpr::StringLiteral("private_type".into()),
                bad_segment: "private_segment".into(),
            },
            UpstreamSubscriptShapeError {
                location: "private_location".into(),
                binding: binding.into(),
                field: "private_field".into(),
                field_path: vec!["private_path".into()],
                mismatched_at: TypeExpr::StringLiteral("private_type".into()),
                bad_subscript: Subscript::Str {
                    key: "private_key".into(),
                },
            },
        )
    }

    #[test]
    fn public_upstream_diagnostics_do_not_infer_scope_from_mask_text() {
        let (field, iterable, attribute, subscript) = diagnostic_fixtures("(secret)");
        let diagnostics: [&dyn UpstreamRefDiagnostic; 4] =
            [&field, &iterable, &attribute, &subscript];
        for error in diagnostics {
            assert_eq!(error.binding(), "(secret)");
            assert_eq!(error.field(), "private_field");
            assert_eq!(error.location(), "private_location");
            let message = error.diagnostic_message();
            assert!(message.starts_with("upstream_state `(secret)"), "{message}");
            assert!(message.contains("private_field"), "{message}");
        }
    }

    #[test]
    fn protected_upstream_diagnostics_discard_all_private_details() {
        use crate::resource::InsideSecret;

        fn check(error: impl UpstreamRefDiagnostic + std::fmt::Debug) {
            let error = UpstreamDiagnostic::from(error)
                .inside_secret()
                .inside_secret();
            assert!(error.public_detail().is_none());
            assert_eq!(error.location(), "(secret)");
            assert_eq!(error.binding(), "(secret)");
            assert_eq!(error.field(), "(secret)");
            assert_eq!(
                error.diagnostic_message(),
                crate::resource::INSIDE_SECRET_MESSAGE
            );
            assert_eq!(
                error.to_string(),
                "(secret): invalid value inside secret(...)"
            );
            assert!(!format!("{error:?}").contains("private_"), "{error:?}");
        }

        let (field, iterable, attribute, subscript) = diagnostic_fixtures("private_binding");
        check(field);
        check(iterable);
        check(attribute);
        check(subscript);
    }

    #[test]
    fn review3_secret_upstream_lookup_protects_findings() {
        let parsed = parse_project(
            r#"
            let private_orgs = upstream_state { source = "../organizations" }
            test.resource { name = secret(private_orgs.private_field) }
        "#,
        );
        let exports = mk_exports(&[("private_orgs", &[])]);
        let tree = AnalysisTree::try_from_file(&parsed).unwrap();
        let errors = check_upstream_state_field_references(&tree, &exports);
        assert_eq!(errors.len(), 1);
        for text in [errors[0].to_string(), format!("{:?}", errors[0])] {
            assert!(!text.contains("private_orgs"), "{text}");
            assert!(!text.contains("private_field"), "{text}");
        }
    }

    #[test]
    fn resolve_upstream_exports_preserves_attached_inference_failure() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("upstream");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("main.crn"), "exports {\n  x = [1, 'a']\n}\n").unwrap();

        let (exports, errors) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports_with_schemas(
                    &crate::paths::ResolvedPath::new(root.path()).unwrap(),
                    &[upstream("up", "upstream")],
                    session,
                    Some(&SchemaRegistry::new()),
                )
                .unwrap()
            });

        assert!(errors.is_empty(), "unexpected resolve errors: {errors:?}");
        let entry = &exports["up"]["x"];
        assert!(matches!(
            &entry.static_type,
            UpstreamExportType::Static(ExportStaticType::Inferred(StaticType::Unchecked(
                crate::static_type::ConsumerInferredUncheckedReason::InferenceFailed(
                    crate::static_type::InferenceError::HeterogeneousCollection { .. }
                )
            )))
        ));
    }

    #[test]
    fn upstream_load_rejects_declared_and_inferred_resource_handle_exports() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("upstream");
        fs::create_dir(&source).unwrap();
        write_crn(
            &source,
            "resource.crn",
            "let record = mock.test.Record { name = 'record' }\n",
        );
        write_crn(
            &source,
            "exports.crn",
            "exports {\n  inferred = record\n  declared: mock.test.Record = record\n}\n",
        );
        let context = LoadedProviderContext::for_test(crate::parser::ProviderContext {
            resource_types: std::collections::HashSet::from([(
                "mock".to_string(),
                "test.Record".to_string(),
            )]),
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                std::collections::HashSet::new(),
                Box::new(|_target, _value| Ok(())),
            ),
            ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
        });

        let schemas = SchemaRegistry::new();
        for schemas in [None, Some(&schemas)] {
            let (exports, errors) =
                crate::validation::deferred_types::with_session_for_test(&context, |session| {
                    resolve_upstream_exports_with_context(
                        &crate::paths::ResolvedPath::new(root.path()).unwrap(),
                        &[upstream("up", "upstream")],
                        session,
                        schemas,
                        &Default::default(),
                    )
                })
                .unwrap();
            assert!(
                !exports.contains_key("up"),
                "handle exports leaked: {exports:?}"
            );
            assert_eq!(errors.len(), 1, "unexpected upstream errors: {errors:?}");
            let rendered = errors[0].to_string();
            assert!(
                rendered.contains(
                    "export 'inferred': inferred resource handle mock.test.Record cannot cross a state boundary"
                ) && rendered.contains(
                    "export 'declared': resource handle mock.test.Record cannot cross a state boundary"
                ),
                "upstream loading must reject both handle export forms: {rendered}",
            );
        }
    }

    fn mk_exports(pairs: &[(&str, &[&str])]) -> UpstreamExports {
        pairs
            .iter()
            .map(|(binding, keys)| {
                (
                    RelativeBindingName::from(*binding),
                    keys.iter()
                        .map(|s| (s.to_string(), inference_not_run_entry()))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn resolve_uses_base_dir_regardless_of_declaring_file() {
        // Issue #1997: the `source` path is resolved against the project's
        // base directory (the directory passed to validate/plan/apply), not
        // against the specific .crn file that happens to declare the
        // `upstream_state`. Two declarations in sibling files in the same
        // project must therefore resolve identically.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: String = "x"
            }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();
        // Two sibling .crn files living at different depths is not possible
        // within one base_dir (directory-scoped parse is flat), but the rule
        // is that file position inside the base dir is irrelevant: the same
        // relative source string resolves the same way for every declaration.
        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        assert!(got.get("orgs").unwrap().contains_key("accounts"));

        // Same call a second time with an identical UpstreamState produces
        // the same result — guards against any accidental dependence on
        // declaring-file state.
        let (got2, errs2) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });
        assert!(errs2.is_empty());
        assert_eq!(got, got2);
    }

    #[test]
    fn resolve_reads_exports_from_default_exports_file() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: String = "x"
                region: String = "ap-northeast-1"
            }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });

        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let keys = got.get("orgs").expect("resolved");
        assert!(keys.contains_key("accounts"));
        assert!(keys.contains_key("region"));
    }

    #[test]
    fn resolve_reads_exports_from_any_crn_file() {
        // exports.crn is convention; the block can live in any .crn file.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "main.crn",
            r#"exports {
                accounts: String = "x"
            }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let keys = got.get("orgs").expect("resolved");
        assert!(keys.contains_key("accounts"));
    }

    #[test]
    fn resolve_merges_exports_across_multiple_crn_files_in_upstream() {
        // Issue #1997: when the upstream project is multi-file, the resolver
        // must merge every .crn file's exports — not just read one privileged
        // file. A downstream that references a field declared in a sibling
        // of the upstream's exports file must still validate.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        // Two sibling files each contributing their own exports block.
        write_crn(
            &upstream_dir,
            "accounts.crn",
            r#"exports {
                accounts: String = "x"
            }"#,
        );
        write_crn(
            &upstream_dir,
            "region.crn",
            r#"exports {
                region: String = "ap-northeast-1"
            }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let keys = got.get("orgs").expect("resolved");
        assert!(
            keys.contains_key("accounts"),
            "export from accounts.crn must be merged, got {:?}",
            keys
        );
        assert!(
            keys.contains_key("region"),
            "export from region.crn must be merged, got {:?}",
            keys
        );
    }

    #[test]
    fn resolve_reads_struct_typed_export_from_multi_file_directory() {
        // Multi-file upstream: `exports.crn` carries the struct-typed export
        // while a sibling `backend.crn` is parsed in the same directory.
        // The resolver must surface the struct annotation on the export.
        use crate::parser::TypeExpr;

        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: struct {
                    registry_prod: String,
                    registry_dev: String,
                } = {
                    registry_prod = "111111111111"
                    registry_dev  = "222222222222"
                }
            }"#,
        );
        write_crn(
            &upstream_dir,
            "backend.crn",
            r#"backend local { path = "carina.state.json" }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let keys = got.get("orgs").expect("resolved");
        let entry = keys.get("accounts").expect("accounts export");
        let ty = entry
            .static_type
            .declared_known()
            .expect("type annotation present");
        assert!(
            matches!(ty, TypeExpr::Struct { fields } if fields.len() == 2),
            "expected struct with 2 fields, got {ty:?}"
        );
    }

    #[test]
    fn resolve_carries_map_literal_value_alongside_type() {
        // #2492: each resolved export entry must carry both its declared
        // `TypeExpr` and the literal `Value` from the upstream's
        // `exports { }` block. Without the value, downstream consumers
        // (LSP map-literal-key completion, future doc-extracting tools)
        // have to re-parse the upstream directory a second time per call.
        use crate::resource::{ConcreteValue, Value};

        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: map(String) = {
                    registry_prod = "111111111111"
                    registry_dev  = "222222222222"
                }
            }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let entries = got.get("orgs").expect("resolved");
        let entry = entries.get("accounts").expect("accounts export");
        assert!(
            entry.static_type.declared_known().is_some(),
            "type annotation must still be present"
        );
        let value = entry.value.as_ref().expect("export value must be carried");
        let Value::Concrete(ConcreteValue::Map(entries)) = value else {
            panic!(
                "expected Value::Concrete(ConcreteValue::Map) for map literal export, got {value:?}"
            );
        };
        let mut keys: Vec<&String> = entries.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["registry_dev", "registry_prod"],
            "map literal keys must be reachable from UpstreamExports without re-parsing"
        );
    }

    #[test]
    fn resolve_skips_missing_source_directory() {
        // Missing-directory is `check_upstream_state_sources`' job; this
        // resolver stays quiet about it.
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../missing")],
                    session,
                )
                .unwrap()
            });
        assert!(got.is_empty());
        assert!(errs.is_empty());
    }

    #[test]
    fn resolve_returns_empty_set_when_source_has_no_exports_block() {
        // An upstream without `exports { }` exports nothing — valid static
        // answer. Downstream refs against it will then fail the checker.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "main.crn",
            r#"backend local { path = "carina.state.json" }"#,
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });
        assert!(errs.is_empty(), "unexpected resolve errors: {errs:?}");
        let keys = got.get("orgs").expect("binding should be resolved");
        assert!(keys.is_empty());
    }

    #[test]
    fn resolve_treats_empty_directory_as_empty_exports() {
        // Empty dir (user is mid-setup) — not a parse failure.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });
        assert!(errs.is_empty());
        assert!(got.get("orgs").expect("resolved").is_empty());
    }

    #[test]
    fn resolve_reports_parse_errors_for_broken_upstream() {
        // A readable upstream whose own main.crn fails to parse must surface as
        // a resolve error — otherwise downstream typos get silently masked.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(&upstream_dir, "main.crn", "not valid crn syntax {{{");
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });
        assert!(!got.contains_key("orgs"));
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].binding, "orgs");
        let rendered = errs[0].detail().to_string();
        assert!(rendered.contains("orgs"), "{rendered}");
        assert!(rendered.contains("organizations"), "{rendered}");
        assert!(rendered.contains("Syntax error"), "{rendered}");
        assert!(rendered.contains("--> 1:"), "{rendered}");
        assert_eq!(
            rendered.matches("--> 1:").count(),
            1,
            "pest location must appear exactly once: {rendered}"
        );
    }

    #[test]
    fn resolve_reports_parse_errors_for_broken_imported_module() {
        // A readable upstream whose imported module fails to parse must
        // preserve the ModuleError leaf through the String boundary.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        let broken_module = upstream_dir.join("broken");
        fs::create_dir(&broken_module).unwrap();
        write_crn(
            &upstream_dir,
            "main.crn",
            "let broken = use { source = './broken' }\nlet instance = broken {}\n",
        );
        write_crn(&broken_module, "main.crn", "let =\n");
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();

        let (got, errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &[upstream("orgs", "../organizations")],
                    session,
                )
                .unwrap()
            });
        assert!(!got.contains_key("orgs"));
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].binding, "orgs");
        let rendered = errs[0].detail().to_string();
        assert!(rendered.contains("orgs"), "{rendered}");
        assert!(rendered.contains("organizations"), "{rendered}");
        assert!(rendered.contains("Syntax error"), "{rendered}");
        assert!(rendered.contains("--> 1:"), "{rendered}");
        assert_eq!(
            rendered.matches("--> 1:").count(),
            1,
            "pest location must appear exactly once: {rendered}"
        );
    }

    #[test]
    fn check_rejects_unexported_field_with_suggestion()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            exports {
                bad: String = orgs.account
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);

        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert_eq!(public(&errs[0]).binding, "orgs");
        assert_eq!(public(&errs[0]).field, "account");
        assert_eq!(public(&errs[0]).suggestion.as_deref(), Some("accounts"));
        assert!(errs[0].detail().to_string().contains("exports.bad"));
        assert!(
            errs[0]
                .diagnostic_message()
                .contains("Did you mean `accounts`?")
        );
        Ok(())
    }

    #[test]
    fn check_accepts_exported_field() -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            exports {
                good: String = orgs.accounts
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);
        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(errs.is_empty(), "got: {errs:?}");
        Ok(())
    }

    #[test]
    fn coalesce_lhs_suppresses_only_missing_export_existence_diagnostic()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            exports {
                selected: list(String) = orgs.optional ?? orgs.required
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &[])]);

        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert_eq!(public(&errs[0]).field, "required");
        Ok(())
    }

    #[test]
    fn coalesce_lhs_nested_references_remain_required()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            exports {
                list_value: list(String) = [orgs.in_list] ?? []
                interpolated: String = "${orgs.in_interpolation}" ?? "fallback"
                function_arg: String = upper(orgs.in_function) ?? "fallback"
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &[])]);

        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        let fields = errs
            .iter()
            .map(|error| public(error).field.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            fields,
            std::collections::BTreeSet::from(["in_function", "in_interpolation", "in_list",]),
            "only a direct reference/access-chain on the left of ?? is optional: {errs:?}",
        );
        Ok(())
    }

    #[test]
    fn deferred_for_coalesce_lhs_may_be_absent() -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for account in (orgs.accounts ?? []) {
                test.resource.item { name = account }
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &[])]);

        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(errs.is_empty(), "got: {errs:?}");
        Ok(())
    }

    #[test]
    fn deferred_for_coalesce_default_keeps_iterable_shape_validation()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for account in (orgs.accounts ?? orgs.fallback) {
                test.resource.item { name = account }
            }
            "#,
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[
                ("accounts", TypeExpr::List(Box::new(TypeExpr::String))),
                ("fallback", TypeExpr::String),
            ],
        )]);

        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert_eq!(public(&errs[0]).field, "fallback");
        Ok(())
    }

    #[test]
    fn deferred_for_coalesce_value_keeps_access_shape_validation()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for account in (orgs.account.missing ?? []) {
                test.resource.item { name = account }
            }
            "#,
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);

        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert_eq!(public(&errs[0]).bad_segment, "missing");
        Ok(())
    }

    #[test]
    fn check_rejects_any_field_when_upstream_declares_no_exports()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // Empty key set → every ref is invalid. Provable statically.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            exports {
                x: String = orgs.anything
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &[])]);

        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert_eq!(errs.len(), 1);
        assert_eq!(public(&errs[0]).field, "anything");
        assert!(public(&errs[0]).suggestion.is_none());
        Ok(())
    }

    #[test]
    fn check_skips_bindings_without_resolved_exports()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // Binding absent from exports map → checker ignores it.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }
            let other = upstream_state { source = "../other" }

            exports {
                x: String = other.foo
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);
        assert!(
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports)
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn check_rejects_unexported_field_in_let_binding()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `let x = orgs.acc` stores the ref in `variables`, not in
        // `resources` — walker must cover that surface too.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            let x = orgs.acc
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);
        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert_eq!(errs.len(), 1, "got: {errs:?}");
        assert_eq!(public(&errs[0]).field, "acc");
        assert!(public(&errs[0]).location.contains("let"));
        Ok(())
    }

    #[test]
    fn upstream_state_sibling_reference_no_empty_export_error()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // Regression: #2847. `bootstrap` declared in `backend.crn`,
        // referenced from `main.crn`. Pre-fix the seeded placeholder
        // surfaced as a `ResourceRef { attribute: "" }` and tripped the
        // walker into emitting `does not export ``.`. Type-split makes
        // the offending shape unrepresentable: a sibling-only seed is a
        // `Value::Deferred(DeferredValue::BindingRef)`, which has no attribute slot for the
        // walker to read, so the empty-field diagnostic class is
        // statically eliminated.
        let tmp = tempfile::tempdir().unwrap();
        write_crn(
            tmp.path(),
            "backend.crn",
            r#"
            let bootstrap = upstream_state { source = "../bootstrap" }
            "#,
        );
        write_crn(
            tmp.path(),
            "main.crn",
            r#"
            let arn = bootstrap.oidc_provider_arn
            "#,
        );
        let parsed = parse_directory(
            &crate::paths::ResolvedPath::new(tmp.path()).unwrap(),
            &ctx(),
        )
        .expect("parse_directory");
        // Whitebox: the bare-binding seed surfaces as `BindingRef`,
        // not `ResourceRef`. The type itself proves the seeded shape
        // can no longer impersonate an attribute reference.
        let bootstrap_var = parsed.variables.get("bootstrap");
        assert!(
            matches!(
                bootstrap_var,
                Some(Value::Deferred(DeferredValue::BindingRef { .. })) | None
            ),
            "sibling-only `bootstrap` must be either absent or a \
             BindingRef, got: {bootstrap_var:?}"
        );
        let exports = mk_exports(&[("bootstrap", &["oidc_provider_arn"])]);
        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(
            errs.is_empty(),
            "consumer-only sibling reference must not synthesize an \
             empty-field error, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn check_rejects_unexported_field_in_for_expression_body()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // The iterable is valid (`orgs.accounts`) but a ref inside the body
        // points at a non-exported field. Must still be flagged — body refs
        // don't reach `parsed.resources` until plan-time expansion.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for name, _ in orgs.accounts {
                awscc.ec2.Vpc {
                    name = name
                    cidr_block = orgs.NONEXISTENT
                }
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);
        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(
            errs.iter().any(|e| public(e).field == "NONEXISTENT"),
            "for-body ref to non-exported field must be flagged, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn for_body_field_error_reported_once_not_twice() -> Result<(), crate::parser::AnalysisTreeError>
    {
        // Regression: the old duplicate display projection could cause the
        // canonical deferred template attribute to be reported twice.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }
            for name, _ in orgs.accounts {
                aws.s3.Bucket {
                    name = orgs.missing
                }
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);
        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        let missing: Vec<_> = errs
            .iter()
            .filter(|e| public(e).field == "missing")
            .collect();
        assert_eq!(missing.len(), 1, "must not double-report, got: {missing:?}");
        Ok(())
    }

    #[test]
    fn check_rejects_unexported_field_in_for_expression_iterable()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `for name, _ in orgs.account` — issue #1990's canonical repro.
        let parsed = parse_project(
            r#"
            let orgs = upstream_state { source = "../organizations" }

            for name, _ in orgs.account {
                awscc.ec2.Vpc {
                    name = name
                    cidr_block = '10.0.0.0/16'
                }
            }
            "#,
        );
        let exports = mk_exports(&[("orgs", &["accounts"])]);

        let errs =
            check_upstream_state_field_references(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert_eq!(errs.len(), 1);
        assert_eq!(public(&errs[0]).binding, "orgs");
        assert_eq!(public(&errs[0]).field, "account");
        assert_eq!(public(&errs[0]).suggestion.as_deref(), Some("accounts"));
        assert!(public(&errs[0]).location.contains("for"));
        Ok(())
    }

    // ================================================================
    // Phase 2 of #1992: type compatibility through the unified binding resolver
    // ================================================================

    /// Build an `UpstreamExports` with typed entries.
    fn mk_typed_exports(pairs: &[(&str, &[(&str, TypeExpr)])]) -> UpstreamExports {
        pairs
            .iter()
            .map(|(binding, fields)| {
                (
                    RelativeBindingName::from(*binding),
                    fields
                        .iter()
                        .map(|(name, ty)| (name.to_string(), declared_entry(ty.clone())))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn upstream_expanded_field_and_subscript_findings_keep_context()
    -> Result<(), crate::parser::AnalysisTreeError> {
        for (expression, field_error) in [("up.missing", true), ("up.names['bad']", false)] {
            let parsed = parse_project_with_provider(
                &format!(
                    "let up = upstream_state {{ source = './up' }}\nlet copies = for n in [1, 2] {{ test.r.res {{ name = {expression} }} }}\n"
                ),
                "test",
            );
            let tree = AnalysisTree::try_from_file(&parsed)?;
            let exports = mk_typed_exports(&[(
                "up",
                &[("names", TypeExpr::List(Box::new(TypeExpr::String)))],
            )]);
            let contexts = if field_error {
                check_upstream_state_field_references(&tree, &exports)
                    .into_iter()
                    .map(|finding| finding.detail().location().to_string())
                    .collect::<Vec<_>>()
            } else {
                check_upstream_state_subscript_shapes(&tree, &exports)
                    .into_iter()
                    .map(|finding| finding.detail().location().to_string())
                    .collect::<Vec<_>>()
            };
            assert_eq!(contexts.len(), 2, "{expression}: {contexts:?}");
            assert_ne!(contexts[0], contexts[1]);
        }
        Ok(())
    }

    #[test]
    fn upstream_iterable_aliases_preserve_each_occurrence()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project_with_provider(
            "let up = upstream_state { source = './up' }\nlet values = up.scalar\nlet copies = for n in (values ?? values) { test.r.res { name = n } }\n",
            "test",
        );
        let tree = AnalysisTree::try_from_file(&parsed)?;
        let exports = mk_typed_exports(&[("up", &[("scalar", TypeExpr::String)])]);
        let findings = check_upstream_state_for_iterable_shapes(&tree, &exports);
        assert_eq!(findings.len(), 2, "{findings:?}");
        Ok(())
    }

    #[test]
    fn upstream_shapes_keep_main_project_value_scope()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project_with_provider(
            "let up = upstream_state { source = './up' }\nlet copies = for n in up.names { read test.r.res { name = up.names.bad } }\n",
            "test",
        );
        let tree = AnalysisTree::try_from_file(&parsed)?;
        let exports = mk_typed_exports(&[(
            "up",
            &[("names", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);
        let findings = check_upstream_state_attribute_access_shapes(&tree, &exports);
        assert!(findings.is_empty(), "{findings:?}");
        Ok(())
    }

    /// Minimal schema registry for test resources: `test.r.res` with a single
    /// typed attribute, registered under provider `test`.
    fn schema_with_attr(
        attr_name: &str,
        attr_type: crate::schema::AttributeType,
    ) -> SchemaRegistry {
        let schema = PatternResourceSchema::new("r.res")
            .attribute(PatternAttributeSchema::optional(attr_name, attr_type).build());
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            schema.validate().expect("schema fixture must be valid"),
        );
        registry
    }

    fn parse_project_with_provider(source: &str, provider_name: &str) -> ParsedFile {
        let tmp = tempfile::tempdir().unwrap();
        let full = format!(
            "provider {} {{\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}}\n{}",
            provider_name, source
        );
        fs::write(tmp.path().join("main.crn"), full).unwrap();
        parse_directory(
            &crate::paths::ResolvedPath::new(tmp.path()).unwrap(),
            &ctx(),
        )
        .expect("parse_directory")
        .into_parsed()
    }

    /// Preserve the historical positional-walker coverage while exercising
    /// the unified binding resolver used by production validation.
    fn check_upstream_ref_types_via_binding_index(
        parsed: &ParsedFile,
        exports: &UpstreamExports,
        schemas: &SchemaRegistry,
    ) -> Vec<String> {
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| {
                let signatures = crate::module_resolver::ResolvedModuleSignatures::default();
                let bindings = crate::binding_index::BindingIndex::from_parsed_with_upstream_exports_and_module_signatures_in_session(parsed, schemas, Some(exports), &signatures, session);
                let configuration = crate::config_loader::load_buffer_configuration(
                    parsed,
                    schemas,
                    session.as_provider_context(),
                );
                crate::validation::validate_resource_ref_types(
                    configuration.judged(),
                    &std::collections::HashSet::new(),
                    &bindings,
                    &mut Vec::new(),
                )
                .err()
                .map(|joined| joined.to_string().lines().map(str::to_string).collect())
                .unwrap_or_default()
            },
        )
    }

    #[test]
    fn type_check_flags_string_consumer_with_int_export() {
        // Export is `int`, consumer expects string-compatible — mismatch.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.count
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr("name", PatternAttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(errs.len(), 1, "unexpected: {errs:?}");
        assert!(errs[0].contains("orgs.count"), "unexpected: {errs:?}");
        assert!(errs[0].contains("expected String"), "unexpected: {errs:?}");
        assert!(errs[0].contains("got Int"), "unexpected: {errs:?}");
    }

    #[test]
    fn type_check_passes_when_export_type_matches_consumer() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.region
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("region", TypeExpr::String)])]);
        let schemas = schema_with_attr("name", PatternAttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    #[test]
    fn type_check_skips_exports_without_type_annotation() {
        // Phase 2 requires an annotation on the export to compare against.
        // Without one (export parsed with no `: T`), skip — don't false-flag.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.count
                }
            "#,
            "test",
        );
        let mut exports: UpstreamExports = HashMap::new();
        let mut fields = HashMap::new();
        fields.insert("count".to_string(), inference_not_run_entry());
        exports.insert(RelativeBindingName::new("orgs"), fields);
        let schemas = schema_with_attr("name", PatternAttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    #[test]
    fn unified_ref_check_reports_unknown_field() {
        // Existence and type now share `BindingIndex::ref_type`, so the same
        // pass emits the canonical missing-export diagnostic exactly once.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.missing
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr("name", PatternAttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(errs.len(), 1, "unexpected errors: {errs:?}");
        assert!(
            errs[0].contains("upstream_state `orgs` does not export `missing`"),
            "unexpected errors: {errs:?}",
        );
    }

    #[test]
    fn type_check_list_element_mismatch() {
        // Export is `list(int)`, consumer expects `list(string)`.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.counts
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("counts", TypeExpr::List(Box::new(TypeExpr::Int)))],
        )]);
        let schemas = schema_with_attr(
            "name",
            PatternAttributeType::list(PatternAttributeType::string()),
        );
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(errs.len(), 1, "unexpected: {errs:?}");
    }

    #[test]
    fn type_check_does_not_apply_function_result_sink_to_function_arguments() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = join(",", orgs.counts)
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("counts", TypeExpr::List(Box::new(TypeExpr::Int)))],
        )]);
        let schemas = schema_with_attr("name", PatternAttributeType::string());

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert!(
            errs.is_empty(),
            "a function argument must not be checked against the function result's String sink: {errs:?}",
        );
    }

    #[test]
    fn type_check_selects_unique_list_member_of_union_for_nested_refs() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = [orgs.count]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr(
            "name",
            PatternAttributeType::union(vec![
                PatternAttributeType::list(PatternAttributeType::string()),
                PatternAttributeType::string(),
            ]),
        );

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert_eq!(
            errs.len(),
            1,
            "the list value must select the union's List member and check its String element sink: {errs:?}",
        );
    }

    #[test]
    fn type_check_selects_unique_map_member_of_union_for_nested_refs() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = { count = orgs.count }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr(
            "name",
            PatternAttributeType::union(vec![
                PatternAttributeType::map(PatternAttributeType::string()),
                PatternAttributeType::string(),
            ]),
        );

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert_eq!(
            errs.len(),
            1,
            "the map value must select the union's Map member and check its String value sink: {errs:?}",
        );
    }

    #[test]
    fn type_check_selects_unique_struct_member_of_union_for_map_value() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = { count = orgs.count }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr(
            "name",
            PatternAttributeType::union(vec![
                PatternAttributeType::struct_(
                    "Count".to_string(),
                    vec![
                        PatternStructField::optional("count", PatternAttributeType::string())
                            .build(),
                    ],
                ),
                PatternAttributeType::string(),
            ]),
        );

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert_eq!(
            errs.len(),
            1,
            "the map value must select the union's Struct member and check its String field sink: {errs:?}",
        );
    }

    #[test]
    fn type_check_rejects_reference_fitting_no_member_of_ambiguous_map_shaped_union() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = { count = orgs.count }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::Int)])]);
        let schemas = schema_with_attr(
            "name",
            PatternAttributeType::union(vec![
                PatternAttributeType::map(PatternAttributeType::string()),
                PatternAttributeType::struct_(
                    "Count".to_string(),
                    vec![
                        PatternStructField::optional("count", PatternAttributeType::string())
                            .build(),
                    ],
                ),
            ]),
        );

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert_eq!(
            errs.len(),
            1,
            "Int fits neither the Map value nor the Struct field, so validation must reject it: {errs:?}",
        );
        assert!(
            errs[0].contains(
                "cannot assign Int to 'name': expected String, got Int (from orgs.count)"
            ),
            "the rejection must name the attribute and reference origin: {errs:?}",
        );
    }

    #[test]
    fn type_check_accepts_reference_fitting_all_members_of_ambiguous_map_shaped_union() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = { count = orgs.count }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("count", TypeExpr::String)])]);
        let schemas = schema_with_attr(
            "name",
            PatternAttributeType::union(vec![
                PatternAttributeType::map(PatternAttributeType::string()),
                PatternAttributeType::struct_(
                    "Count".to_string(),
                    vec![
                        PatternStructField::optional("count", PatternAttributeType::string())
                            .build(),
                    ],
                ),
            ]),
        );

        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);

        assert!(
            errs.is_empty(),
            "String fits both candidate members without guessing a nested sink: {errs:?}",
        );
    }

    #[test]
    fn type_check_flags_for_body_attribute() {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                for _, account_id in orgs.counts {
                    test.r.res {
                        name = orgs.count
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[
                ("count", TypeExpr::Int),
                ("counts", TypeExpr::List(Box::new(TypeExpr::Int))),
            ],
        )]);
        let schemas = schema_with_attr("name", PatternAttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(
            errs.len(),
            1,
            "expected one error inside for body, got {errs:?}"
        );
        assert!(errs[0].contains("for-body"), "unexpected: {errs:?}");
    }

    // Issue #2663: a semantic-ARN refinement (e.g. `IamOidcProviderArn`)
    // declared on an upstream export must be assignable to a
    // `Union<Struct(Principal), String>` receiver — the canonical IAM
    // policy `principal` slot. Before the fix, the Struct member made
    // the union fall through to the catch-all reject so the arn was
    // refused at the very position it was designed for.
    #[test]
    fn type_check_accepts_arn_refinement_into_principal_union() {
        let parsed = parse_project_with_provider(
            r#"
                let bootstrap = upstream_state { source = "../bootstrap" }
                test.r.res {
                    name = { federated = bootstrap.oidc_provider_arn }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "bootstrap",
            &[(
                "oidc_provider_arn",
                crate::parser::bare_type_for_test("iam_oidc_provider_arn".to_string()),
            )],
        )]);
        // Mirrors the canonical IAM `principal` slot shape:
        // `Union<Struct(IamPolicyPrincipal), String>`. The map walker
        // hits the Union catch-all and walks each value against the
        // whole union, so the leaf `Simple(iam_oidc_provider_arn)`
        // must be assignable to the union receiver directly.
        let principal_union = PatternAttributeType::union(vec![
            PatternAttributeType::struct_(
                "IamPolicyPrincipal".to_string(),
                vec![
                    PatternStructField::optional("federated", PatternAttributeType::string())
                        .build(),
                ],
            ),
            PatternAttributeType::string(),
        ]);
        let schemas = schema_with_attr("name", principal_union);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "IamOidcProviderArn must be assignable into Union<Struct, String>, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_rejects_distinct_bare_refinement_kinds() {
        // `KmsKeyArn` and `Arn` are distinct bare identity kinds; the shared
        // relation cannot infer a subtype edge from their spelling. #1874's
        // safe widening is represented by structured identities that retain
        // kind `Arn` and place `kms.Key` on the segments axis (covered by the
        // core directional-relation tests).
        use crate::schema::TypeIdentity;
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.key_arn
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "key_arn",
                crate::parser::bare_type_for_test("kms_key_arn".to_string()),
            )],
        )]);
        let arn = PatternAttributeType::refined_string_with_validator(
            TypeIdentity::bare("Arn"),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        );
        let schemas = schema_with_attr("name", arn);
        let catalog = crate::parser::ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                ["Arn", "KmsKeyArn"]
                    .into_iter()
                    .map(|kind| {
                        crate::parser::OwnedTypeIdentity::provider(
                            "test",
                            TypeIdentity::bare(kind),
                            crate::schema::RefinedScalarKind::String,
                        )
                    })
                    .collect(),
                Box::new(|_target, _value| Ok(())),
            ),
            ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
        };
        let signatures = crate::module_resolver::ResolvedModuleSignatures::default();
        let bindings = crate::binding_index::BindingIndex::from_parsed_with_upstream_exports_and_module_signatures(&parsed, &schemas, &exports, &signatures, &catalog);
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let errs = crate::validation::validate_resource_ref_types(
            configuration.judged(),
            &std::collections::HashSet::new(),
            &bindings,
            &mut Vec::new(),
        )
        .err()
        .map(|error| {
            error
                .to_string()
                .lines()
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
        assert_eq!(
            errs.len(),
            1,
            "distinct bare identity kinds must not be treated as a subtype chain: {errs:?}"
        );
    }

    // ================================================================
    // #2447: subscript narrows the export type before the top-level
    // type-compatibility check
    // ================================================================

    #[test]
    fn type_check_map_subscript_narrows_to_value_type() {
        // `orgs.accounts['k']` against export `map(AwsAccountId)` narrows to
        // the value type before being compared to the receiver. Without the
        // narrowing the comparison fires `map(AwsAccountId)` against the
        // receiver and (when the receiver is anything but a compatible map)
        // false-flags the position. Issue #2447.
        use crate::schema::TypeIdentity;
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts['k']
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        // Receiver is `String`. Without narrowing, `map(AwsAccountId)`
        // compares against `String` and fails. With narrowing,
        // `AwsAccountId` (Custom over String) is accepted.
        let aws_account_id = PatternAttributeType::refined_string_with_validator(
            TypeIdentity::bare("AwsAccountId"),
            None,
            None,
            crate::schema::legacy_validator(|_| Ok(())),
            None,
        );
        let schemas = schema_with_attr("name", aws_account_id);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "subscript narrows map(T) to T before compare, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_subscript_inside_struct_position_does_not_false_flag() {
        // The bucket-policy repro from #2447: a deeply-nested ref inside a
        // struct attribute (`policy: Struct(IamPolicyDocument)`) where
        // narrowing through a subscript produces a scalar that *could*
        // legitimately occupy a leaf string position inside the struct.
        // The top-level check must not reject the narrowed scalar against
        // the outer struct type — positional walking is the precise tool
        // for that, and the existing top-level check is too coarse.

        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    policy = {
                        statement = "arn:aws:iam::${orgs.accounts['registry_dev']}:root"
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let policy_struct = PatternAttributeType::struct_(
            "IamPolicyDocument".to_string(),
            vec![PatternStructField::required("statement", PatternAttributeType::string()).build()],
        );
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "subscript-narrowed scalar must not be rejected against outer \
             struct attr, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_dot_form_chains_through_map_into_struct_field() {
        // `orgs.accounts.k.account_id` where `accounts: map(struct{
        // account_id: aws_account_id })`. The dot walk descends
        // map(T).k → T (struct), then struct.account_id → aws_account_id.
        // Both legs of `walk_type_expr_path` exercised by one path.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts.alpha.account_id
                }
            "#,
            "test",
        );
        let account_struct = TypeExpr::Struct {
            fields: vec![(
                "account_id".to_string(),
                crate::parser::bare_type_for_test("aws_account_id".to_string()),
            )],
        };
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::Map(Box::new(account_struct)))],
        )]);
        let schemas = schema_with_attr("name", PatternAttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "dot-walk through map into struct field must resolve to leaf type, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_list_subscript_narrows_to_element_type() {
        // Same shape as the map case for `list(T)[idx]`: subscript narrows
        // to T before the top-level check.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.regions[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("regions", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);
        let schemas = schema_with_attr("name", PatternAttributeType::string());
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "list subscript narrows list(T) to T before compare, got: {errs:?}"
        );
    }

    // ================================================================
    // #2475: positional walker — narrowed scalar is compared against the
    // *position* type, not the outer attribute type.
    // ================================================================

    #[test]
    fn type_check_walker_resolves_struct_block_name_alias() {
        // The walker's `Struct` arm resolves field keys via
        // `struct_field_for_key`, which honours `block_name`
        // aliases. Without that resolution a ref written under the
        // alias would silently skip the type check (the alias key
        // wouldn't match any `field.name`). Pin the path: a field
        // declared as `name="statements", block_name="statement"`
        // accessed via the alias must still get its leaf ref checked
        // — here the leaf is a narrowed `Int` against a `String`
        // declaration, so the walker must flag the mismatch.

        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    policy = {
                        statement = orgs.counts['k']
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("counts", TypeExpr::Map(Box::new(TypeExpr::Int)))],
        )]);
        let policy_struct = PatternAttributeType::struct_(
            "PolicyDoc".to_string(),
            vec![
                PatternStructField::required("statements", PatternAttributeType::string())
                    .with_block_name("statement")
                    .build(),
            ],
        );
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(
            errs.len(),
            1,
            "ref under block_name alias `statement` must be type-checked, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_narrowed_int_in_int_field_passes_without_string_shape_leniency() {
        // Counterpart to the mismatched-leaf test below: narrowed Int
        // against an Int-typed struct field must pass via the strict
        // compatibility check, not via the string-shape leniency arm
        // (which only fires for `is_string_compatible_type` receivers).
        // Pin the path so a regression that wrongly broadened leniency
        // would still surface.

        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    config = {
                        port = orgs.ports['k']
                    }
                }
            "#,
            "test",
        );
        let exports =
            mk_typed_exports(&[("orgs", &[("ports", TypeExpr::Map(Box::new(TypeExpr::Int)))])]);
        let cfg_struct = PatternAttributeType::struct_(
            "Cfg".to_string(),
            vec![PatternStructField::required("port", PatternAttributeType::int()).build()],
        );
        let schemas = schema_with_attr("config", cfg_struct);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "narrowed Int in Int-typed struct field must pass strict compat, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_narrowed_scalar_in_struct_field_with_mismatched_leaf_flags() {
        // Acceptance criterion 1 from #2475: a narrowed `Int` (from a
        // subscript on `map(Int)`) sitting in a struct field whose
        // declared type is `String` must be flagged. Pre-#2475 the
        // `is_scalar_type_expr` short-circuit silently accepted this.

        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    policy = {
                        statement = orgs.counts['k']
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("counts", TypeExpr::Map(Box::new(TypeExpr::Int)))],
        )]);
        let policy_struct = PatternAttributeType::struct_(
            "PolicyDoc".to_string(),
            vec![PatternStructField::required("statement", PatternAttributeType::string()).build()],
        );
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(
            errs.len(),
            1,
            "narrowed Int in String-typed struct field must flag, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_narrowed_scalar_in_struct_field_with_matching_leaf_passes() {
        // Acceptance criterion 2 from #2475: the same shape but the
        // struct field's declared type matches the narrowed scalar —
        // the bucket-policy repro from #2447 — must continue to pass.
        // (Pre-fix this passed via the `is_scalar_type_expr`
        // short-circuit; post-fix it must pass via the positional
        // walker comparing AwsAccountId against the field's String.)

        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    policy = {
                        statement = "arn:aws:iam::${orgs.accounts['registry_dev']}:root"
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let policy_struct = PatternAttributeType::struct_(
            "IamPolicyDocument".to_string(),
            vec![PatternStructField::required("statement", PatternAttributeType::string()).build()],
        );
        let schemas = schema_with_attr("policy", policy_struct);
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            errs.is_empty(),
            "narrowed AwsAccountId in String-shaped position must pass, got: {errs:?}"
        );
    }

    #[test]
    fn type_check_interpolation_operands_accept_scalars_and_reject_collections() {
        let schemas = schema_with_attr(
            "tags",
            PatternAttributeType::map(PatternAttributeType::string()),
        );
        for (ty, accepted) in [
            (TypeExpr::String, true),
            (TypeExpr::Int, true),
            (TypeExpr::Float, true),
            (TypeExpr::Bool, true),
            (TypeExpr::Duration, true),
            (TypeExpr::Map(Box::new(TypeExpr::String)), false),
            (TypeExpr::List(Box::new(TypeExpr::String)), false),
            (
                TypeExpr::Struct {
                    fields: vec![("name".into(), TypeExpr::String)],
                },
                false,
            ),
        ] {
            let exports = mk_typed_exports(&[(
                "orgs",
                &[
                    ("value", ty.clone()),
                    ("values", TypeExpr::Map(Box::new(ty.clone()))),
                ],
            )]);
            for reference in ["orgs.value", "orgs.values['k']"] {
                for prefix in ["", "value: "] {
                    let parsed = parse_project_with_provider(
                        &format!(
                            "let orgs = upstream_state {{ source = '../organizations' }}\n\
                             test.r.res {{ tags = {{ Foo = \"{prefix}${{{reference}}}\" }} }}"
                        ),
                        "test",
                    );
                    let errors =
                        check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
                    assert_eq!(
                        errors.len(),
                        usize::from(!accepted),
                        "{ty}: {prefix}${{{reference}}}: {errors:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn type_check_narrowed_map_inside_interpolation_flags() {
        // Acceptance criterion 3 from #2475: a `map(_)` ref embedded
        // inside a string interpolation cannot satisfy the String
        // position the interpolation forces. The outer attribute is
        // `tags: Map(String, String)`, so the *outer* compare
        // `map(...)` vs `map(String)` would happily succeed (inner
        // String ⊑ String). Only a positional walker, descending into
        // the map value's interpolation, can spot that the
        // interpolation position is String and the embedded ref is a
        // `map(...)`. Pre-#2475 this regression slipped through.

        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    tags = { Foo = "all accounts: ${orgs.accounts}" }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let schemas = schema_with_attr(
            "tags",
            PatternAttributeType::map(PatternAttributeType::string()),
        );
        let errs = check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert_eq!(
            errs.len(),
            1,
            "map embedded in tag-value interpolation must flag, got: {errs:?}"
        );
    }

    // ================================================================
    // #1894: for-iterable shape compatibility
    // (`check_upstream_state_for_iterable_shapes`)
    // ================================================================

    #[test]
    fn for_iterable_simple_binding_against_list_export_is_ok()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `for x in orgs.accounts` over a `list(...)` export — fine.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::List(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(
            errs.is_empty(),
            "list export + simple binding must pass, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn for_iterable_map_binding_against_map_export_is_ok()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `for k, v in orgs.accounts` over a `map(...)` export — fine.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for name, account_id in orgs.accounts {
                    test.r.res {
                        name = name
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(
            errs.is_empty(),
            "map export + map binding must pass, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn for_iterable_simple_binding_against_map_export_flags_mismatch()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // Upstream changed `accounts: list(_)` to `map(_)`. Downstream
        // still iterates as `for x in ...` — old shape. The check must
        // flag this so the cross-directory refactor surfaces in the
        // downstream plan/validate output rather than blowing up at
        // apply time. This is the canonical #1894 repro.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(
            errs.len(),
            1,
            "map export + simple binding must fail, got: {errs:?}"
        );
        assert_eq!(public(&errs[0]).binding, "orgs");
        assert_eq!(public(&errs[0]).field, "accounts");
        assert_eq!(public(&errs[0]).binding_kind, ForIterableBindingKind::List);
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("map(AwsAccountId)") && msg.contains("requires a list"),
            "message must show map export and list-iterable expectation: {msg}"
        );
        assert!(
            msg.contains("for k, v in"),
            "message must suggest the map binding form: {msg}"
        );
        Ok(())
    }

    #[test]
    fn for_iterable_map_binding_against_list_export_flags_mismatch()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // Inverse of the above: downstream uses `for k, v in ...` but
        // upstream still exports a list. Same class of bug.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for name, account_id in orgs.accounts {
                    test.r.res {
                        name = name
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::List(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(
            errs.len(),
            1,
            "list export + map binding must fail, got: {errs:?}"
        );
        assert_eq!(public(&errs[0]).field, "accounts");
        assert_eq!(public(&errs[0]).binding_kind, ForIterableBindingKind::Map);
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("list(AwsAccountId)") && msg.contains("requires a map"),
            "message must show list export and map-iterable expectation: {msg}"
        );
        assert!(
            msg.contains("for x in"),
            "message must suggest the list binding form: {msg}"
        );
        Ok(())
    }

    #[test]
    fn for_iterable_skipped_when_export_has_no_declared_type()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `accounts` declared without `: T` annotation — nothing to
        // compare against, the field-name check still runs but the
        // shape check stays silent.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let mut exports = UpstreamExports::new();
        let mut fields = HashMap::new();
        fields.insert("accounts".to_string(), inference_not_run_entry());
        exports.insert(RelativeBindingName::new("orgs"), fields);
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(errs.is_empty(), "no annotation → silent, got: {errs:?}");
        Ok(())
    }

    #[test]
    fn for_iterable_skipped_when_binding_unknown() -> Result<(), crate::parser::AnalysisTreeError> {
        // The binding isn't in `exports` at all (resolve failed
        // upstream). Field-name check handles that case via
        // UpstreamFieldError; the shape check stays silent so the
        // user only sees one diagnostic.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[]);
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(errs.is_empty(), "missing binding → silent, got: {errs:?}");
        Ok(())
    }

    #[test]
    fn for_iterable_shape_check_against_real_directory_fixture()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // Directory-scoped acceptance: upstream's `exports.crn` lives in
        // a sibling directory and `resolve_upstream_exports` parses it
        // off disk, then the shape check fires when downstream's
        // `for` binding doesn't match the exported `list ↔ map`. This
        // is the realistic shape of #1894 (cross-directory refactor).
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: map(String) = {
                    alpha = "111111111111"
                }
            }"#,
        );
        write_crn(
            &upstream_dir,
            "providers.crn",
            "provider test {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();
        write_crn(
            &base,
            "providers.crn",
            "provider test {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
        );
        write_crn(
            &base,
            "main.crn",
            r#"
                let orgs = upstream_state { source = "../organizations" }
                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
        );
        let parsed = parse_directory(&crate::paths::ResolvedPath::new(&base).unwrap(), &ctx())
            .expect("parse_directory");
        let (exports, resolve_errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &parsed.upstream_states,
                    session,
                )
                .unwrap()
            });
        assert!(
            resolve_errs.is_empty(),
            "unexpected resolve errors: {resolve_errs:?}"
        );
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(
            errs.len(),
            1,
            "expected one shape mismatch from list↔map upstream change, got: {errs:?}"
        );
        assert_eq!(public(&errs[0]).field, "accounts");
        assert_eq!(public(&errs[0]).binding_kind, ForIterableBindingKind::List);
        Ok(())
    }

    #[test]
    fn for_iterable_against_scalar_export_flags_mismatch()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // Upstream exports a scalar (e.g. `accounts: String`); downstream
        // tries to iterate. There is no valid binding form to suggest —
        // the upstream contract has to change first. The check fires
        // and the message stays honest by omitting the suggestion.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("accounts", TypeExpr::String)])]);
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(
            errs.len(),
            1,
            "scalar export + for must fail, got: {errs:?}"
        );
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("String"),
            "message must show scalar export type: {msg}"
        );
        assert!(
            msg.contains("requires a list"),
            "message must say list expected: {msg}"
        );
        assert!(
            !msg.contains("for k, v in") && !msg.contains("for x in"),
            "scalar export must not suggest a binding form: {msg}"
        );
        Ok(())
    }

    #[test]
    fn for_iterable_indexed_binding_classified_as_list()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `for (i, x) in orgs.accounts` is a *list* iterable — the
        // 2-name binding pattern is `(index, value)`, not `(key, value)`.
        // The kind classifier must collapse Simple+Indexed into List.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for (i, account_id) in orgs.accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        // List export — indexed binding is fine.
        let list_exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::List(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        assert!(
            check_upstream_state_for_iterable_shapes(
                &AnalysisTree::try_from_file(&parsed)?,
                &list_exports
            )
            .is_empty(),
            "indexed binding against list export must pass"
        );
        // Map export — indexed binding is the wrong shape.
        let map_exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &map_exports,
        );
        assert_eq!(
            errs.len(),
            1,
            "indexed binding + map export must fail, got: {errs:?}"
        );
        assert_eq!(public(&errs[0]).binding_kind, ForIterableBindingKind::List);
        Ok(())
    }

    #[test]
    fn for_iterable_errors_sort_stably() -> Result<(), crate::parser::AnalysisTreeError> {
        // Two `for` expressions in a single project produce errors in
        // a deterministic order regardless of the underlying parse order.
        // Mirrors the existing sort guarantee on `UpstreamFieldError`.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.alpha_accounts {
                    test.r.res {
                        name = account_id
                    }
                }

                for account_id in orgs.beta_accounts {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[
                (
                    "alpha_accounts",
                    TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                        "aws_account_id".to_string(),
                    ))),
                ),
                (
                    "beta_accounts",
                    TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                        "aws_account_id".to_string(),
                    ))),
                ),
            ],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(errs.len(), 2, "expected two errors, got: {errs:?}");
        assert_eq!(public(&errs[0]).field, "alpha_accounts");
        assert_eq!(public(&errs[1]).field, "beta_accounts");
        Ok(())
    }

    #[test]
    fn for_iterable_skipped_when_field_not_exported() -> Result<(), crate::parser::AnalysisTreeError>
    {
        // The binding exists but the iterable field isn't exported.
        // `UpstreamFieldError` already surfaces this; shape check
        // stays silent to avoid duplicate diagnostics.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }

                for account_id in orgs.NONEXISTENT {
                    test.r.res {
                        name = account_id
                    }
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::List(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let errs = check_upstream_state_for_iterable_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(errs.is_empty(), "missing field → silent, got: {errs:?}");
        Ok(())
    }

    // ================================================================
    // #1894 follow-up: attribute-access shape compatibility
    // (`check_upstream_state_attribute_access_shapes`)
    // ================================================================

    #[test]
    fn attribute_access_struct_with_known_field_is_ok()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // Upstream exports `account: struct { id: String, region: String }`,
        // downstream writes `orgs.account.id` — the `id` field is declared,
        // shape check stays silent.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.id
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![
                        ("id".to_string(), TypeExpr::String),
                        ("region".to_string(), TypeExpr::String),
                    ],
                },
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(
            errs.is_empty(),
            "known struct field must pass, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn list_field_access_keeps_the_dedicated_upstream_shape_diagnostic()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let source = concat!(
            "provider test {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
            r#"
                let orgs = upstream_state { source = "../organizations" }
                let consumer = test.r.res {
                    name = orgs.names.foo
                }
            "#,
        );
        let source_file = Path::new("main.crn");
        let parsed = crate::parser::parse_file(source, source_file, &ctx()).unwrap();
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("names", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);
        let schemas = schema_with_attr("name", PatternAttributeType::string());

        let generic_errors =
            check_upstream_ref_types_via_binding_index(&parsed, &exports, &schemas);
        assert!(
            generic_errors.is_empty(),
            "generic reference validation must leave this case to the dedicated upstream diagnostic: {generic_errors:?}"
        );

        let errors = check_upstream_state_attribute_access_shape_fallbacks(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].location().file().path(), source_file);
        let span = errors[0].location().span();
        assert_eq!(&source[span.start_byte()..span.end_byte()], "orgs.names");
        assert_eq!(
            errors[0].to_string(),
            "main.crn:9:28: test.r.res.consumer attribute `name`: upstream_state `orgs.names` is declared as `list(String)` but `.foo` reads it as a struct; iterate the list with `for x in orgs.names` to access elements"
        );
        Ok(())
    }

    #[test]
    fn attribute_access_struct_with_unknown_field_flags_mismatch()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.account.region` against a struct that doesn't declare
        // `region` is the canonical "downstream depends on upstream's
        // schema and the upstream changed" failure.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.region
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(
            errs.len(),
            1,
            "unknown struct field must fail, got: {errs:?}"
        );
        assert_eq!(public(&errs[0]).binding, "orgs");
        assert_eq!(public(&errs[0]).field, "account");
        assert_eq!(public(&errs[0]).field_path, vec!["region".to_string()]);
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("region"),
            "message must name the missing field: {msg}"
        );
        Ok(())
    }

    #[test]
    fn attribute_access_against_list_export_flags_mismatch()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.accounts.foo` against `accounts: list(_)` is a category
        // error — the user has to iterate first.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts.foo
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::List(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(
            errs.len(),
            1,
            "field access on list must fail, got: {errs:?}"
        );
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("list"),
            "message must mention list shape: {msg}"
        );
        Ok(())
    }

    #[test]
    fn attribute_access_against_map_export_resolves_as_key_access()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.accounts.alpha` against `accounts: map(_)` is valid
        // map-key access, equivalent to `orgs.accounts['alpha']`. The
        // shape walk descends `map(T).k → T` for the dot form, the
        // same way it does for the subscript form. Issue #2447 — both
        // notations must resolve a single map entry.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts.alpha
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "accounts",
                TypeExpr::Map(Box::new(crate::parser::bare_type_for_test(
                    "aws_account_id".to_string(),
                ))),
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(
            errs.is_empty(),
            "dot-notation against map must be accepted as key access, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn attribute_access_against_scalar_export_flags_mismatch()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.region.foo` against `region: String` — scalars have no
        // fields.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.region.foo
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("region", TypeExpr::String)])]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(
            errs.len(),
            1,
            "field access on scalar must fail, got: {errs:?}"
        );
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("String"),
            "message must show scalar export type: {msg}"
        );
        Ok(())
    }

    #[test]
    fn attribute_access_skipped_when_export_has_no_declared_type()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.id
                }
            "#,
            "test",
        );
        let mut exports = UpstreamExports::new();
        let mut fields = HashMap::new();
        fields.insert("account".to_string(), inference_not_run_entry());
        exports.insert(RelativeBindingName::new("orgs"), fields);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(errs.is_empty(), "no annotation → silent, got: {errs:?}");
        Ok(())
    }

    #[test]
    fn attribute_access_skipped_when_binding_unknown()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.id
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(errs.is_empty(), "missing binding → silent, got: {errs:?}");
        Ok(())
    }

    #[test]
    fn attribute_access_skipped_when_field_not_exported()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.NONEXISTENT.id` — field-name check already surfaces
        // the missing top-level export; the shape check stays silent.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.NONEXISTENT.id
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(errs.is_empty(), "missing field → silent, got: {errs:?}");
        Ok(())
    }

    #[test]
    fn attribute_access_skipped_when_field_path_empty()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.account` (no `.foo`) — that's just a top-level field
        // ref, handled by the typed consumer through `BindingIndex::ref_type`.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(errs.is_empty(), "no field_path → silent, got: {errs:?}");
        Ok(())
    }

    #[test]
    fn attribute_access_nested_struct_field_walks_path()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.account.network.vpc_id` against
        // `account: struct { network: struct { vpc_id: String } }` — the
        // walker must descend into nested structs.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.network.vpc_id
                }
            "#,
            "test",
        );
        let inner = TypeExpr::Struct {
            fields: vec![("vpc_id".to_string(), TypeExpr::String)],
        };
        let outer = TypeExpr::Struct {
            fields: vec![("network".to_string(), inner)],
        };
        let exports = mk_typed_exports(&[("orgs", &[("account", outer)])]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert!(
            errs.is_empty(),
            "nested struct path must pass, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn attribute_access_nested_struct_unknown_field_flagged()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // Same fixture but the deep field is wrong — must still be
        // caught.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.network.bad_field
                }
            "#,
            "test",
        );
        let inner = TypeExpr::Struct {
            fields: vec![("vpc_id".to_string(), TypeExpr::String)],
        };
        let outer = TypeExpr::Struct {
            fields: vec![("network".to_string(), inner)],
        };
        let exports = mk_typed_exports(&[("orgs", &[("account", outer)])]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(
            errs.len(),
            1,
            "nested unknown field must fail, got: {errs:?}"
        );
        assert_eq!(
            public(&errs[0]).field_path,
            vec!["network".to_string(), "bad_field".to_string()]
        );
        assert_eq!(public(&errs[0]).bad_segment, "bad_field");
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("bad_field"),
            "message must name the missing leaf: {msg}"
        );
        Ok(())
    }

    #[test]
    fn attribute_access_struct_containing_list_flags_at_list_position()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `account: struct { tags: list(String) }` — `orgs.account.tags`
        // resolves cleanly (struct → list, no further `.field`); but
        // `orgs.account.tags.foo` walks past the list and must flag
        // there. The `mismatched_at` is the list, not the outer struct.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.tags.foo
                }
            "#,
            "test",
        );
        let outer = TypeExpr::Struct {
            fields: vec![(
                "tags".to_string(),
                TypeExpr::List(Box::new(TypeExpr::String)),
            )],
        };
        let exports = mk_typed_exports(&[("orgs", &[("account", outer)])]);
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(errs.len(), 1, "list-at-depth must flag, got: {errs:?}");
        assert_eq!(public(&errs[0]).bad_segment, "foo");
        assert!(
            matches!(public(&errs[0]).mismatched_at, TypeExpr::List(_)),
            "mismatched_at must be the inner list, got: {:?}",
            public(&errs[0]).mismatched_at
        );
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("list") && msg.contains("for x in"),
            "message must mention list and suggest iteration: {msg}"
        );
        Ok(())
    }

    #[test]
    fn attribute_access_shape_check_against_real_directory_fixture()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // Directory-scoped acceptance: upstream's `exports.crn` lives
        // in a sibling directory and `resolve_upstream_exports` parses
        // it off disk. Mirrors the parity test for the for-iterable
        // sibling (`for_iterable_shape_check_against_real_directory_fixture`).
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("organizations");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "exports.crn",
            r#"exports {
                accounts: list(String) = ["111111111111"]
            }"#,
        );
        write_crn(
            &upstream_dir,
            "providers.crn",
            "provider test {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
        );
        let base = tmp.path().join("downstream");
        fs::create_dir(&base).unwrap();
        write_crn(
            &base,
            "providers.crn",
            "provider test {\n  source = 'x/y'\n  version = '0.1'\n  region = 'ap-northeast-1'\n}\n",
        );
        write_crn(
            &base,
            "main.crn",
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts.foo
                }
            "#,
        );
        let parsed = parse_directory(&crate::paths::ResolvedPath::new(&base).unwrap(), &ctx())
            .expect("parse_directory");
        let (exports, resolve_errs) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports(
                    &crate::paths::ResolvedPath::new(&base).unwrap(),
                    &parsed.upstream_states,
                    session,
                )
                .unwrap()
            });
        assert!(
            resolve_errs.is_empty(),
            "unexpected resolve errors: {resolve_errs:?}"
        );
        let errs = check_upstream_state_attribute_access_shapes(
            &AnalysisTree::try_from_file(&parsed)?,
            &exports,
        );
        assert_eq!(
            errs.len(),
            1,
            "expected one shape mismatch from list export + .field access, got: {errs:?}"
        );
        assert_eq!(public(&errs[0]).field, "accounts");
        assert_eq!(public(&errs[0]).bad_segment, "foo");
        assert!(matches!(public(&errs[0]).mismatched_at, TypeExpr::List(_)));
        Ok(())
    }

    // ================================================================
    // #2318: subscript-after-field shape compatibility
    // (`check_upstream_state_subscript_shapes`)
    // ================================================================

    #[test]
    fn subscript_int_against_list_export_is_ok() -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.accounts[0]` against `accounts: list(String)` — integer
        // subscripts read list elements; shape check stays silent.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(errs.is_empty(), "[int] on list(_) must pass, got: {errs:?}");
        Ok(())
    }

    #[test]
    fn subscript_str_against_map_export_is_ok() -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.accounts["alpha"]` against `accounts: map(String)` —
        // string subscripts read map entries; shape check stays silent.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts["alpha"]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::Map(Box::new(TypeExpr::String)))],
        )]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(
            errs.is_empty(),
            "[\"k\"] on map(_) must pass, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn subscript_int_against_map_export_flags_mismatch()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.accounts[0]` against `accounts: map(String)` — wrong
        // syntax; should fire with a hint to use `["k"]`.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::Map(Box::new(TypeExpr::String)))],
        )]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert_eq!(errs.len(), 1, "[int] on map must fail, got: {errs:?}");
        assert_eq!(public(&errs[0]).binding, "orgs");
        assert_eq!(public(&errs[0]).field, "accounts");
        assert!(matches!(
            public(&errs[0]).bad_subscript,
            Subscript::Int { index: 0 }
        ));
        let msg = errs[0].diagnostic_message();
        assert!(msg.contains("map"), "message must mention map shape: {msg}");
        assert!(
            msg.contains("[0]"),
            "message must echo the user's literal subscript, got: {msg}"
        );
        Ok(())
    }

    #[test]
    fn subscript_str_against_list_export_flags_mismatch()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.accounts["alpha"]` against `accounts: list(String)` —
        // wrong syntax; should fire with a hint to use `[i]`.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts["alpha"]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert_eq!(errs.len(), 1, "[\"k\"] on list must fail, got: {errs:?}");
        assert!(matches!(
            public(&errs[0]).bad_subscript,
            Subscript::Str { .. }
        ));
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("list"),
            "message must mention list shape: {msg}"
        );
        Ok(())
    }

    #[test]
    fn postfix_access_on_upstream_ref_participates_in_subscript_shape_checks()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                let accounts = orgs.accounts
                test.r.res {
                    direct = (orgs.accounts)[0]
                    alias  = accounts["alpha"]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[("accounts", TypeExpr::List(Box::new(TypeExpr::String)))],
        )]);

        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);

        assert_eq!(
            errs.len(),
            1,
            "the valid integer postfix access should pass and the string alias access should fail: {errs:?}"
        );
        assert!(matches!(
            public(&errs[0]).bad_subscript,
            Subscript::Str { .. }
        ));
        assert_eq!(public(&errs[0]).binding, "orgs");
        assert_eq!(public(&errs[0]).field, "accounts");
        Ok(())
    }

    #[test]
    fn subscript_against_struct_export_flags_mismatch()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.account[0]` against `account: struct {...}` — structs
        // don't host any subscript form. The diagnostic stays silent on
        // the "use the other syntax" hint because neither would help.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert_eq!(
            errs.len(),
            1,
            "subscript on struct must fail, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn subscript_against_scalar_export_flags_mismatch()
    -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.region[0]` against `region: String` — scalars are not
        // subscriptable.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.region[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[("orgs", &[("region", TypeExpr::String)])]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert_eq!(
            errs.len(),
            1,
            "subscript on scalar must fail, got: {errs:?}"
        );
        let msg = errs[0].diagnostic_message();
        assert!(
            msg.contains("String"),
            "message must show scalar export type: {msg}"
        );
        Ok(())
    }

    #[test]
    fn subscript_after_struct_field_walks_path() -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.account.children[0]` against
        // `account: struct { children: list(String) }` — the walker
        // must descend through the struct field before checking the
        // subscript.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.children[0]
                }
            "#,
            "test",
        );
        let outer = TypeExpr::Struct {
            fields: vec![(
                "children".to_string(),
                TypeExpr::List(Box::new(TypeExpr::String)),
            )],
        };
        let exports = mk_typed_exports(&[("orgs", &[("account", outer)])]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(
            errs.is_empty(),
            "subscript after struct field walks path, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn subscript_skipped_when_export_has_no_declared_type()
    -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts[0]
                }
            "#,
            "test",
        );
        let mut exports = UpstreamExports::new();
        let mut fields = HashMap::new();
        fields.insert("accounts".to_string(), inference_not_run_entry());
        exports.insert(RelativeBindingName::new("orgs"), fields);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(errs.is_empty(), "no annotation → silent, got: {errs:?}");
        Ok(())
    }

    #[test]
    fn subscript_skipped_when_binding_unknown() -> Result<(), crate::parser::AnalysisTreeError> {
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.accounts[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(errs.is_empty(), "missing binding → silent, got: {errs:?}");
        Ok(())
    }

    #[test]
    fn subscript_skipped_when_field_path_mismatches() -> Result<(), crate::parser::AnalysisTreeError>
    {
        // `orgs.account.bad[0]` — the `.bad` already mismatches the
        // declared struct type. Don't double-fire on the subscript;
        // the attribute-access check owns that diagnostic.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.account.bad[0]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "account",
                TypeExpr::Struct {
                    fields: vec![("id".to_string(), TypeExpr::String)],
                },
            )],
        )]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(
            errs.is_empty(),
            "field-path mismatch is the attribute-access check's job, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn subscript_chained_descends_into_inner_type() -> Result<(), crate::parser::AnalysisTreeError>
    {
        // `orgs.matrix[0][1]` against `matrix: list(list(String))` —
        // each integer subscript peels one `list(_)` layer.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.matrix[0][1]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "matrix",
                TypeExpr::List(Box::new(TypeExpr::List(Box::new(TypeExpr::String)))),
            )],
        )]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert!(
            errs.is_empty(),
            "list(list(_))[0][1] must pass, got: {errs:?}"
        );
        Ok(())
    }

    #[test]
    fn subscript_chained_flags_inner_mismatch() -> Result<(), crate::parser::AnalysisTreeError> {
        // `orgs.matrix[0]["k"]` against `matrix: list(list(String))` —
        // first `[0]` peels to `list(String)`; second `["k"]` then
        // mismatches.
        let parsed = parse_project_with_provider(
            r#"
                let orgs = upstream_state { source = "../organizations" }
                test.r.res {
                    name = orgs.matrix[0]["k"]
                }
            "#,
            "test",
        );
        let exports = mk_typed_exports(&[(
            "orgs",
            &[(
                "matrix",
                TypeExpr::List(Box::new(TypeExpr::List(Box::new(TypeExpr::String)))),
            )],
        )]);
        let errs =
            check_upstream_state_subscript_shapes(&AnalysisTree::try_from_file(&parsed)?, &exports);
        assert_eq!(
            errs.len(),
            1,
            "second subscript mismatch must fire, got: {errs:?}"
        );
        assert!(matches!(
            public(&errs[0]).bad_subscript,
            Subscript::Str { .. }
        ));
        Ok(())
    }

    // ================================================================
    // #2319: UpstreamRefDiagnostic trait
    // ================================================================

    #[test]
    fn upstream_ref_diagnostic_trait_covers_all_four_error_types() {
        // Building a `Vec<&dyn UpstreamRefDiagnostic>` proves each type
        // implements the trait and that the LSP/CLI can iterate them
        // uniformly. The CLI/LSP wirings rely on this being possible.
        let field_err = UpstreamFieldError {
            location: "loc-a".to_string(),
            binding: "orgs".to_string(),
            field: "missing".to_string(),
            suggestion: Some("accounts".to_string()),
        };
        let shape_err = UpstreamForIterableShapeError {
            location: "for-expression `for x in orgs.accounts`".to_string(),
            binding: "orgs".to_string(),
            field: "accounts".to_string(),
            export_type: TypeExpr::Map(Box::new(TypeExpr::String)),
            binding_kind: ForIterableBindingKind::List,
        };
        let attr_err = UpstreamAttributeAccessShapeError {
            location: "loc-d".to_string(),
            binding: "orgs".to_string(),
            field: "account".to_string(),
            field_path: vec!["bad".to_string()],
            mismatched_at: TypeExpr::Struct {
                fields: vec![("id".to_string(), TypeExpr::String)],
            },
            bad_segment: "bad".to_string(),
        };
        let subscript_err = UpstreamSubscriptShapeError {
            location: "loc-e".to_string(),
            binding: "orgs".to_string(),
            field: "accounts".to_string(),
            field_path: vec![],
            mismatched_at: TypeExpr::Map(Box::new(TypeExpr::String)),
            bad_subscript: Subscript::Int { index: 0 },
        };
        // Tuples of (dyn-trait reference, expected location, expected
        // binding, expected field, expected diagnostic_message). Looping
        // catches a future struct that silently desyncs trait impls
        // from inherent fields.
        let cases: Vec<(&dyn UpstreamRefDiagnostic, &str, &str, &str, String)> = vec![
            (
                &field_err,
                "loc-a",
                "orgs",
                "missing",
                field_err.diagnostic_message(),
            ),
            (
                &shape_err,
                "for-expression `for x in orgs.accounts`",
                "orgs",
                "accounts",
                shape_err.diagnostic_message(),
            ),
            (
                &attr_err,
                "loc-d",
                "orgs",
                "account",
                attr_err.diagnostic_message(),
            ),
            (
                &subscript_err,
                "loc-e",
                "orgs",
                "accounts",
                subscript_err.diagnostic_message(),
            ),
        ];
        for (d, expected_location, expected_binding, expected_field, expected_message) in cases {
            assert_eq!(d.location(), expected_location);
            assert_eq!(d.binding(), expected_binding);
            assert_eq!(d.field(), expected_field);
            assert_eq!(d.diagnostic_message(), expected_message);
            // `Display` supertrait must produce the canonical
            // `"location: message"` form — the CLI's combined-error
            // path relies on this format.
            assert_eq!(
                d.to_string(),
                format!("{}: {}", d.location(), d.diagnostic_message())
            );
        }
    }

    #[test]
    fn resolve_upstream_exports_with_schemas_uses_inference_for_unannotated() {
        // Stage 2 (#2360): the resolver hoists the upstream parse
        // through `apply_inference`, so an unannotated literal export
        // surfaces as a typed entry to consumers without the user
        // having to write `: String`.
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("upstream");
        fs::create_dir(&upstream_dir).unwrap();
        write_crn(
            &upstream_dir,
            "main.crn",
            "exports {\n  region = \"us-east-1\"\n}\n",
        );

        let upstream_state = upstream("ups", "upstream");
        let (exports, _errors) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports_with_schemas(
                    &crate::paths::ResolvedPath::new(tmp.path()).unwrap(),
                    &[upstream_state],
                    session,
                    Some(&crate::schema::SchemaRegistry::new()),
                )
                .unwrap()
            });
        let region_entry = exports
            .get("ups")
            .and_then(|m| m.get("region"))
            .expect("region should be inferred");
        assert_eq!(
            region_entry.static_type,
            UpstreamExportType::Static(ExportStaticType::Inferred(consumer_inferred_type(
                TypeExpr::String,
            )))
        );
    }

    #[test]
    fn resolve_upstream_exports_resolves_declarations_before_inference() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("upstream");
        let module_dir = upstream_dir.join("typed");
        fs::create_dir_all(&module_dir).unwrap();
        write_crn(
            &upstream_dir,
            "main.crn",
            "let seed = test.Item {}\nlet typed = use { source = './typed' }\nlet result = typed { value = seed.identifier }\nexports { out = result.out reversed = result.reversed }\n",
        );
        write_crn(
            &module_dir,
            "main.crn",
            "arguments { value: test.types.Identifier }\n\
             let item = test.Item {}\n\
             attributes {\n\
               out = [value, item.identifier]\n\
               reversed = [item.identifier, value]\n\
             }\n",
        );

        let identity = crate::schema::TypeIdentity::from_schema_type("test", "types", "Identifier");
        let owned = crate::parser::OwnedTypeIdentity::provider(
            "test",
            identity,
            crate::schema::RefinedScalarKind::String,
        );
        let config = LoadedProviderContext::for_test(ProviderContext {
            custom_types: crate::parser::ProviderCustomTypes::loaded(
                std::collections::HashSet::from([owned.clone()]),
                Box::new(|_target, _value| Ok(())),
            ),
            ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
        });
        let mut schemas = crate::schema::SchemaRegistry::new();
        let schema = PatternResourceSchema::new("Item")
            .attribute(
                PatternAttributeSchema::optional(
                    "identifier",
                    PatternAttributeType::refined_string(
                        Some(owned.identity().clone()),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .expect("schema fixture must be valid");
        schemas.insert("test", schema);

        let (exports, errors) =
            crate::validation::deferred_types::with_session_for_test(&config, |session| {
                resolve_upstream_exports_with_schemas(
                    &crate::paths::ResolvedPath::new(tmp.path()).unwrap(),
                    &[upstream("ups", "upstream")],
                    session,
                    Some(&schemas),
                )
                .unwrap()
            });

        assert!(errors.is_empty(), "unexpected resolve errors: {errors:?}");
        for name in ["out", "reversed"] {
            assert_eq!(
                exports["ups"][name].static_type,
                UpstreamExportType::Static(ExportStaticType::Inferred(consumer_inferred_type(
                    TypeExpr::List(Box::new(TypeExpr::SchemaType {
                        owned: owned.clone().unbounded_witness_for_test(),
                    })),
                ))),
                "annotation and attribute reads share one identity-level value type"
            );
        }
    }

    #[test]
    fn module_expansion_failure_is_reported_instead_of_inferred_as_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("upstream");
        let module_dir = upstream_dir.join("module");
        fs::create_dir_all(&module_dir).unwrap();
        write_crn(
            &module_dir,
            "main.crn",
            "arguments { count: Int }\nattributes { out: Int = count }\n",
        );
        write_crn(
            &upstream_dir,
            "main.crn",
            r#"let module = use { source = './module' }
let instance = module { count = 'wrong' }
exports { out: Int = instance.out }
"#,
        );

        let (exports, errors) =
            crate::validation::deferred_types::with_session_for_test(&ctx(), |session| {
                resolve_upstream_exports_with_schemas(
                    &crate::paths::ResolvedPath::new(tmp.path()).unwrap(),
                    &[upstream("ups", "upstream")],
                    session,
                    Some(&crate::schema::SchemaRegistry::new()),
                )
                .unwrap()
            });

        assert!(
            !exports.contains_key("ups"),
            "a failed expansion must not publish Unknown exports: {exports:?}"
        );
        assert_eq!(errors.len(), 1, "errors: {errors:?}");
        assert!(
            errors[0]
                .reason
                .contains("expected Int, got string 'wrong'"),
            "{}",
            errors[0]
        );
    }

    #[test]
    fn providerless_upstream_module_unknown_consumer_custom_type_is_unchecked() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream_dir = tmp.path().join("upstream");
        let module_dir = upstream_dir.join("module");
        fs::create_dir_all(&module_dir).unwrap();
        write_crn(
            &module_dir,
            "main.crn",
            "arguments { vpc: awscc.ec2.VpcId }\nattributes { out: String = vpc }\n",
        );
        write_crn(
            &upstream_dir,
            "main.crn",
            r#"let module = use { source = './module' }
let instance = module { vpc = 'vpc-123' }
exports { out: String = instance.out }
"#,
        );

        let (exports, errors) = crate::validation::deferred_types::with_session_for_test(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| {
                resolve_upstream_exports_providerless_with_schemas(
                    &crate::paths::ResolvedPath::new(tmp.path()).unwrap(),
                    &[upstream("ups", "upstream")],
                    session,
                    Some(&crate::schema::SchemaRegistry::new()),
                )
                .unwrap()
            },
        );

        assert!(
            errors.is_empty(),
            "unknown consumer type must defer: {errors:?}"
        );
        assert!(
            exports
                .get("ups")
                .is_some_and(|values| values.contains_key("out")),
            "the upstream export must remain available: {exports:?}"
        );
    }
}
