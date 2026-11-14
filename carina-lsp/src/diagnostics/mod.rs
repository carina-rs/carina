#[cfg(feature = "test-utils")]
use crate::admission_coverage::{
    self as replay_coverage, ExpandedDeclineReason, Path as AdmissionPath,
};

mod buffer;
mod checks;
mod validation;

#[cfg(test)]
mod tests;

use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

use crate::document::DiagnosticDocument as Document;
use crate::position;
use carina_core::parser::retained::JudgedExpr;
use carina_core::parser::{
    ParseError, ProviderConfig, SourceSpan, WarningKind, contains_function_call,
};
use carina_core::paths::PathIdentity;
use carina_core::provider::{
    HostErrorCause, LoadedProviderSnapshot, ProviderFactory, SuspendedValidationId,
};
use carina_core::resource::{
    AbsoluteBindingAddress, BindingAddress, ConcreteValue, ParserPlaceholder, RelativeBindingName,
    Value as GenericValue,
};
use carina_core::schema::{SchemaRegistry, TypeError};
use carina_core::validation::DeferredAccessTypeError;
#[cfg(test)]
use carina_core::validation::admission::attributes::uses_resource_attribute_admission;

type Value = GenericValue<ParserPlaceholder, RelativeBindingName>;

fn authored_validation_resources<'t, 'a>(
    tree: &'t carina_core::parser::AnalysisTree<'a>,
) -> HashMap<
    carina_core::parser::AuthoredResourceInstanceKey<'a>,
    &'t carina_core::parser::AnalysisResourceRef<'a>,
> {
    tree.project_resources()
        .filter_map(|resource| resource.authored_instance_key().map(|key| (key, resource)))
        .collect()
}

fn select_admission_inputs<'r, 'a>(
    resource: &'r carina_core::parser::AnalysisResourceRef<'a>,
    authored_resources: Option<
        &HashMap<
            carina_core::parser::AuthoredResourceInstanceKey<'a>,
            &'r carina_core::parser::AnalysisResourceRef<'a>,
        >,
    >,
    values: &IndexMap<String, Value>,
    report: &carina_core::parser::retained::ExpressionReport,
) -> (
    &'r carina_core::parser::AnalysisResourceRef<'a>,
    carina_core::validation::admission::attributes::ResourceAttributeInputs<RelativeBindingName>,
) {
    use carina_core::validation::admission::attributes::ResourceAttributeInputs;
    let authored = resource
        .authored_instance_key()
        .and_then(|key| authored_resources?.get(&key).copied());
    match authored.and_then(|source| {
        ResourceAttributeInputs::from_resolved(source, values, report)
            .ok()
            .map(|inputs| (source, inputs))
    }) {
        Some(authored) => authored,
        // Main checks the expanded owner against its resolved values. Only
        // unchecked-slot metadata comes from the matched source tree.
        None => (
            resource,
            ResourceAttributeInputs::from_report_with_evidence(
                resource,
                values,
                report,
                carina_core::parser::retained::NoAuthoredEvidence::for_resource(
                    authored.unwrap_or(resource),
                ),
            ),
        ),
    }
}

type ExpandedAnalysis<'a> = carina_core::parser::AnalysisTree<
    'a,
    ParserPlaceholder,
    ParserPlaceholder,
    carina_core::parser::ResolvedDeferredForPhase<
        ParserPlaceholder,
        ParserPlaceholder,
        AbsoluteBindingAddress,
    >,
    AbsoluteBindingAddress,
>;
type ExpandedResource<'a> = carina_core::parser::AnalysisResourceRef<
    'a,
    ParserPlaceholder,
    ParserPlaceholder,
    carina_core::parser::ResolvedDeferredForPhase<
        ParserPlaceholder,
        ParserPlaceholder,
        AbsoluteBindingAddress,
    >,
    AbsoluteBindingAddress,
>;
type RootAnalysis<'a> = carina_core::parser::AnalysisTree<
    'a,
    ParserPlaceholder,
    ParserPlaceholder,
    carina_core::parser::ParsedDeferredForPhase<ParserPlaceholder, AbsoluteBindingAddress>,
    AbsoluteBindingAddress,
>;

/// Seal each input at the parse/expansion boundary before dependent checks run.
fn analysis_tree<'a, E, D, B: BindingAddress>(
    doc: &Document,
    parsed: &'a carina_core::parser::File<E, ParserPlaceholder, ParserPlaceholder, D, B>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<carina_core::parser::AnalysisTree<'a, ParserPlaceholder, ParserPlaceholder, D, B, E>>
where
    E: carina_core::parser::ExportParamLike<ParserPlaceholder, B>,
    D: carina_core::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
{
    match carina_core::parser::AnalysisTree::try_from_file(parsed) {
        Ok(tree) => Some(tree),
        Err(error) => {
            let diagnostic = render_analysis_invariant(doc, &error);
            if let Some(diagnostic) = diagnostic
                && !diagnostics.contains(&diagnostic)
            {
                diagnostics.push(diagnostic);
            }
            None
        }
    }
}

fn analysis_result<T>(
    doc: &Document,
    result: Result<T, carina_core::parser::AnalysisTreeError>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            if let Some(diagnostic) = render_analysis_invariant(doc, &error)
                && !diagnostics.contains(&diagnostic)
            {
                diagnostics.push(diagnostic);
            }
            None
        }
    }
}

fn render_analysis_invariant(
    doc: &Document,
    error: &carina_core::parser::AnalysisTreeError,
) -> Option<Diagnostic> {
    let message = error.to_string();
    match error.diagnostic() {
        carina_core::parser::AnalysisTreeDiagnostic::File(finding) => {
            render_merged(doc, &finding, DiagnosticSeverity::ERROR, message)
        }
        carina_core::parser::AnalysisTreeDiagnostic::Directory(finding) => {
            render_merged(doc, &finding, DiagnosticSeverity::ERROR, message)
        }
    }
}

fn render_analysis_resolution_error(
    doc: &Document,
    error: &carina_core::parser::AnalysisResolutionError,
) -> Option<Diagnostic> {
    match error {
        carina_core::parser::AnalysisResolutionError::Invariant(error) => {
            render_analysis_invariant(doc, error)
        }
        carina_core::parser::AnalysisResolutionError::Diagnostic(finding) => render_merged(
            doc,
            finding.as_ref(),
            DiagnosticSeverity::ERROR,
            finding.detail().to_string(),
        ),
    }
}

/// An unknown directory grants no expanded-root readiness. Keep declared
/// argument placeholders provisional while ordinary expression checks run;
/// this does not mint template scope or exempt unknown requires from inference.
fn parser_placeholder_boundary(
    verdict: &carina_core::config_loader::DirectoryVerdict,
) -> carina_core::parser::ParserPlaceholderBoundary {
    use carina_core::config_loader::DirectoryVerdict;
    use carina_core::parser::ParserPlaceholderBoundary;
    match verdict {
        DirectoryVerdict::Root(_) | DirectoryVerdict::NoEvidence(_) => {
            ParserPlaceholderBoundary::ExpandedFile
        }
        DirectoryVerdict::ModuleTemplate(_) | DirectoryVerdict::Unknown(_) => {
            ParserPlaceholderBoundary::ModuleDefinition
        }
    }
}

/// The verdict supplies both the source and UTF-16 columns from its sealed
/// snapshot, including unsaved siblings. Rendering never rereads evidence.
fn attach_directory_evidence(
    diagnostic: &mut Diagnostic,
    evidence: &impl carina_core::config_loader::DirectoryDeclarationEvidence,
) {
    let location = evidence.location();
    let (start, end) = evidence.utf16_columns();
    let file = evidence.file_locator();
    let Ok(uri) = tower_lsp::lsp_types::Url::from_file_path(file.to_path_buf()) else {
        return;
    };
    // URL parsing removes dot segments while retaining percent-encoded OS bytes.
    let Ok(uri) = tower_lsp::lsp_types::Url::parse(uri.as_str()) else {
        return;
    };
    let span = location.span();
    diagnostic.related_information =
        Some(vec![tower_lsp::lsp_types::DiagnosticRelatedInformation {
            location: tower_lsp::lsp_types::Location {
                uri,
                range: Range::new(
                    Position::new((span.start_line() - 1) as u32, start),
                    Position::new((span.end_line() - 1) as u32, end),
                ),
            },
            message: evidence.related_message(),
        }]);
}

fn parser_span_range(doc: &Document, span: SourceSpan) -> Option<Range> {
    // SourceSpan records parser-owned Unicode-scalar columns. Convert those
    // columns to UTF-16, including when a source view uses multibyte whitespace.
    let position = |line: usize, column: usize| {
        let line = u32::try_from(line.checked_sub(1)?).ok()?;
        let column = column.checked_sub(1)?;
        (column <= doc.line_scalar_len(line)?).then(|| {
            Position::new(
                line,
                doc.scalar_utf16_column(line, column)
                    .expect("line was checked"),
            )
        })
    };
    Some(Range {
        start: position(span.start_line(), span.start_column())?,
        end: position(span.end_line(), span.end_column())?,
    })
}

mod merged_findings {
    use carina_core::parser::{
        DeclarationLocation, DirectoryScoped, FileScoped, Located, SourceFileId,
    };

    pub(super) enum Scope<'a> {
        Located(&'a DeclarationLocation),
        File(&'a SourceFileId),
        Directory,
    }

    // Consumers cannot implement this trait for an unowned diagnostic type.
    mod sealed {
        pub trait Sealed {}
        impl<D> Sealed for carina_core::parser::Located<D> {}
        impl<D> Sealed for carina_core::parser::FileScoped<D> {}
        impl<D> Sealed for carina_core::parser::DirectoryScoped<D> {}
    }
    pub(super) trait Finding: sealed::Sealed {
        fn scope(&self) -> Scope<'_>;
    }
    impl<D> Finding for Located<D> {
        fn scope(&self) -> Scope<'_> {
            Scope::Located(self.location())
        }
    }
    impl<D> Finding for FileScoped<D> {
        fn scope(&self) -> Scope<'_> {
            Scope::File(self.file())
        }
    }
    impl<D> Finding for DirectoryScoped<D> {
        fn scope(&self) -> Scope<'_> {
            Scope::Directory
        }
    }
}
use merged_findings::{Finding as MergedFinding, Scope as MergedScope};

/// DirectoryScoped findings appear at each buffer's start because they have no owning declaration or file.
fn render_merged<F: MergedFinding>(
    doc: &Document,
    finding: &F,
    severity: DiagnosticSeverity,
    message: String,
) -> Option<Diagnostic> {
    let range = match finding.scope() {
        MergedScope::Located(location) => {
            if location.file().path() != doc.source_file() {
                return None;
            }
            let Some(range) = parser_span_range(doc, location.span()) else {
                log::debug!(
                    "dropping diagnostic with unmappable span in {}: {:?}",
                    doc.source_file().display(),
                    location.span()
                );
                return None;
            };
            range
        }
        MergedScope::File(file) => {
            if file.path() != doc.source_file() {
                return None;
            }
            Range::default()
        }
        MergedScope::Directory => Range::default(),
    };
    Some(Diagnostic {
        range,
        severity: Some(severity),
        source: Some("carina".to_string()),
        message,
        ..Default::default()
    })
}

/// Syntax errors can use coordinates only from this buffer's own parser.
/// A directory parse failure without a local syntax error remains file scoped.
fn render_parse(
    doc: &Document,
    finding: &carina_core::parser::FileScoped<&ParseError>,
    severity: DiagnosticSeverity,
    message: String,
) -> Option<Diagnostic> {
    if finding.file().path() != doc.source_file() {
        return None;
    }
    if let Some(mut diagnostic) = buffer::render_document_parse_error(doc) {
        diagnostic.severity = Some(severity);
        diagnostic.message = message;
        Some(diagnostic)
    } else {
        render_merged(doc, finding, severity, message)
    }
}

fn render_module_call_findings(
    doc: &Document,
    findings: &[carina_core::validation::ModuleCallFinding],
) -> Vec<Diagnostic> {
    findings
        .iter()
        .flat_map(|finding| finding.diagnostics())
        .filter_map(|finding| {
            let mut diagnostic = render_merged(
                doc,
                &finding,
                DiagnosticSeverity::ERROR,
                finding.detail().message().to_string(),
            )?;
            if let Some(type_error) = finding.detail().message().type_error() {
                decorate_schema_diagnostic(&mut diagnostic, type_error);
            }
            Some(diagnostic)
        })
        .collect()
}

#[derive(Default)]
pub(super) struct ProviderValidationDeduplicator {
    suspended_inputs: Mutex<HashSet<(String, SuspendedValidationId)>>,
}

impl ProviderValidationDeduplicator {
    fn should_emit(&self, provider: &str, error: &carina_core::provider::HostError) -> bool {
        let Some(HostErrorCause::ValidationSuspended(input_id)) = error.detail().host_cause()
        else {
            return true;
        };
        self.suspended_inputs
            .lock()
            .expect("provider validation diagnostic lock must not be poisoned")
            .insert((provider.to_string(), input_id))
    }

    fn should_emit_type_error(&self, error: &TypeError) -> bool {
        let Some(HostErrorCause::ValidationSuspended(input_id)) = error.host_cause() else {
            return true;
        };
        self.suspended_inputs
            .lock()
            .expect("provider validation diagnostic lock must not be poisoned")
            .insert((
                error
                    .provider_name()
                    .as_deref()
                    .unwrap_or("<unknown>")
                    .to_string(),
                input_id,
            ))
    }
}

enum DiagnosticProviderContext {
    Loaded(carina_core::parser::LoadedProviderContext),
    Unloaded(Arc<carina_core::parser::ProviderContext>),
}

struct DiagnosticValidationInputs<'a> {
    binding_index: carina_core::binding_index::BindingIndex<'a, RelativeBindingName>,
    inference_bindings: carina_core::validation::inference::InferenceBindings<
        ParserPlaceholder,
        RelativeBindingName,
    >,
    binding_use_report: carina_core::validation::BindingUseReport,
    export_value_bindings:
        carina_core::binding_index::ResolvedBindings<ParserPlaceholder, RelativeBindingName>,
}

/// Expanded values and their binding authority stay in the absolute phase.
/// Authored values are used separately for source positions and lexical checks.
type ExpandedConfiguration<'a> = carina_core::parser::retained::JudgedFile<
    'a,
    carina_core::parser::ParsedExportParam<ParserPlaceholder, AbsoluteBindingAddress>,
    carina_core::parser::ResolvedDeferredForPhase<
        ParserPlaceholder,
        ParserPlaceholder,
        AbsoluteBindingAddress,
    >,
    AbsoluteBindingAddress,
>;
struct ExpandedDiagnosticValidationInputs<'a> {
    configuration: &'a ExpandedConfiguration<'a>,
    resources: HashMap<&'a carina_core::resource::ResourceId, &'a ExpandedResource<'a>>,
    coverage: carina_core::validation::admission::WalkCoverage,
    binding_index: carina_core::binding_index::BindingIndex<'a, AbsoluteBindingAddress>,
}

impl ExpandedDiagnosticValidationInputs<'_> {
    fn resource_admissions(
        &mut self,
        resource_id: &carina_core::resource::ResourceId,
        session: &dyn carina_core::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Option<carina_core::schema::ResourceValueValidation> {
        let Some(&resource) = self.resources.get(resource_id) else {
            #[cfg(feature = "test-utils")]
            replay_coverage::declined(ExpandedDeclineReason::MissingResource);
            return None;
        };
        let schemas = self.configuration.schemas();
        let Some(schema) = resource.schema(schemas) else {
            #[cfg(feature = "test-utils")]
            {
                replay_coverage::missing_schema(AdmissionPath::Expanded, &resource_id.provider);
                replay_coverage::declined(ExpandedDeclineReason::MissingSchema);
            }
            return None;
        };
        let Ok(inputs) =
            carina_core::validation::admission::attributes::ResourceAttributeInputs::for_resource(
                resource,
                self.configuration.report(),
            )
        else {
            #[cfg(feature = "test-utils")]
            replay_coverage::declined(ExpandedDeclineReason::MissingReport);
            return None;
        };
        let mut classified = inputs.classify(
            &carina_core::resource::knowledge::KnowledgeContext::validation(),
            |_, _| {},
        );
        let evaluated = classified
            .iter()
            .filter_map(|(name, proof)| {
                carina_core::parser::evaluate_expanded_value_for_validation(proof, session)
                    .filter(|value| inputs.value(name).is_some_and(|source| source != value))
                    .map(|value| (name.clone(), value))
            })
            .collect::<IndexMap<_, _>>();
        let mut evaluated_inputs = inputs.clone();
        let (evaluated_inputs, updated) = evaluated_inputs.materialize_and_classify(
            &evaluated,
            &carina_core::resource::knowledge::KnowledgeContext::validation(),
            |_, _| {},
        );
        classified.extend(updated);
        #[cfg(feature = "test-utils")]
        replay_coverage::reached(AdmissionPath::Expanded);
        Some(
            carina_core::validation::admission::attributes::resource_attribute_admissions(
                resource,
                self.configuration.file(),
                schema,
                &classified,
                resource.quoted_string_attrs(),
                evaluated_inputs,
                &self.binding_index,
                session,
                schemas,
                &mut self.coverage,
            ),
        )
    }
}

impl DiagnosticProviderContext {
    #[cfg(test)]
    fn loaded(&self) -> Option<&carina_core::parser::LoadedProviderContext> {
        match self {
            Self::Loaded(context) => Some(context),
            Self::Unloaded(_) => None,
        }
    }

    #[cfg(test)]
    fn set_decryptor_for_test(&mut self, decryptor: carina_core::parser::DecryptorFn) {
        match self {
            Self::Loaded(context) => context.set_decryptor_for_test(decryptor),
            Self::Unloaded(context) => {
                Arc::get_mut(context)
                    .expect("unshared test context")
                    .decryptor = Some(decryptor)
            }
        }
    }
}

impl std::ops::Deref for DiagnosticProviderContext {
    type Target = carina_core::parser::ProviderContext;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Loaded(context) => context.as_provider_context(),
            Self::Unloaded(context) => context,
        }
    }
}

/// Analysis findings are rendered from parser-owned locations. Raw coordinate
/// constructors are private to buffer syntax and style lint rendering.
///
/// ```compile_fail,E0432
/// use carina_lsp::diagnostics::carina_diagnostic;
/// use tower_lsp::lsp_types::DiagnosticSeverity;
/// let finding = carina_diagnostic(3, 7, 8, DiagnosticSeverity::ERROR, "finding".into());
/// ```
///
/// ```compile_fail,E0603
/// use carina_lsp::diagnostics::buffer::carina_diagnostic_range;
/// use tower_lsp::lsp_types::{DiagnosticSeverity, Range};
/// let finding = carina_diagnostic_range(Range::default(), DiagnosticSeverity::ERROR, "finding".into());
/// ```
///
/// Arithmetic findings cannot be rendered from file-free line/column spans.
///
/// ```compile_fail,E0432
/// use carina_lsp::diagnostics::integer_operator_range;
/// ```
pub struct DiagnosticEngine {
    schemas: Arc<SchemaRegistry>,
    loaded_providers: Option<Arc<LoadedProviderSnapshot>>,
    /// Declarations and directory that produced this engine's loaded snapshot.
    /// Ancestor, imported, and empty states cannot establish authority for a
    /// different directory when its declarations fail to parse.
    directory_providers: Option<DirectoryProviderDeclarations>,
    provider_names: Vec<String>,
    known_provider_names: HashSet<String>,
    factories: Arc<Vec<Box<dyn ProviderFactory>>>,
    configuration_factories: Vec<Box<dyn ProviderFactory>>,
    /// Providers that failed to load: name -> error reason.
    provider_errors: HashMap<String, String>,
    /// Cached provider context with custom type validators from schemas.
    provider_context: DiagnosticProviderContext,
}

struct DirectoryProviderDeclarations {
    directory: PathIdentity,
    providers: Vec<ProviderConfig>,
}

struct LspTypeCheckSink<'a> {
    doc: &'a Document<'a>,
    diagnostics: Vec<Diagnostic>,
}
impl carina_core::validation::deferred_types::ProviderTypeCheckSink for LspTypeCheckSink<'_> {
    fn record(
        &mut self,
        diagnostic: carina_core::validation::deferred_types::ProviderTypeCheckDiagnostic,
    ) {
        let (warning, severity, code) = match diagnostic {
                carina_core::validation::deferred_types::ProviderTypeCheckDiagnostic::Deferred(warning) => (warning, DiagnosticSeverity::WARNING, "providers_not_loaded"),
                carina_core::validation::deferred_types::ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(error) => (error, DiagnosticSeverity::ERROR, "unsettled_provider_type_check"),
            };
        let diagnostic = match &warning {
            carina_core::validation::deferred_types::ProviderTypeCheckFinding::Declaration(
                finding,
            ) => render_merged(self.doc, finding, severity, warning.message.clone()),
            carina_core::validation::deferred_types::ProviderTypeCheckFinding::Directory(
                finding,
            ) => render_merged(self.doc, finding, severity, warning.message.clone()),
        };
        let Some(mut diagnostic) = diagnostic else {
            return;
        };
        diagnostic.code = Some(tower_lsp::lsp_types::NumberOrString::String(code.into()));
        self.diagnostics.push(diagnostic);
    }
}

pub(super) struct DiagnosticRun<'a> {
    engine: &'a DiagnosticEngine,
    provider_context: &'a dyn carina_core::validation::deferred_types::ProviderTypeCheckContext,
}

impl std::ops::Deref for DiagnosticRun<'_> {
    type Target = DiagnosticEngine;
    fn deref(&self) -> &Self::Target {
        self.engine
    }
}

impl DiagnosticRun<'_> {
    fn resolve_diagnostic_upstream_exports<D, B: BindingAddress>(
        &self,
        base: &carina_core::paths::ResolvedPath,
        tree: &carina_core::parser::AnalysisTree<'_, ParserPlaceholder, ParserPlaceholder, D, B>,
        overrides: &carina_core::config_loader::SourceOverrides,
    ) -> Result<
        (
            carina_core::upstream_exports::UpstreamExports<B>,
            Vec<
                carina_core::parser::Located<carina_core::upstream_exports::UpstreamResolveFailure>,
            >,
        ),
        carina_core::parser::AnalysisTreeError,
    >
    where
        D: carina_core::parser::DeferredForPhaseTraversal<ParserPlaceholder, ParserPlaceholder, B>,
    {
        carina_core::upstream_exports::resolve_upstream_exports_report_with_overrides(
            base,
            tree,
            self.provider_context,
            Some(&self.schemas),
            overrides,
        )
    }

    fn validation_inputs<'a, D>(
        &'a self,
        configuration: &'a carina_core::parser::retained::JudgedFile<
            '_,
            carina_core::parser::ParsedExportParam,
            D,
            RelativeBindingName,
        >,
        upstream_exports: &'a carina_core::upstream_exports::UpstreamExports,
        module_signatures: &'a carina_core::module_resolver::ResolvedModuleSignatures,
    ) -> DiagnosticValidationInputs<'a>
    where
        D: carina_core::parser::DeferredForPhaseTraversal<
                ParserPlaceholder,
                ParserPlaceholder,
                RelativeBindingName,
            >,
    {
        let binding_input = configuration.file();
        let binding_index = carina_core::binding_index::BindingIndex::from_parsed_with_upstream_exports_and_module_signatures_in_session(
            binding_input,
            configuration.schemas(),
            Some(upstream_exports),
            module_signatures,
            self.provider_context,
        );
        let inference_bindings =
            carina_core::validation::inference::bindings_from_parsed(binding_input);
        let context = carina_core::validation::ValidationTypeContext::in_session(
            configuration,
            self.provider_context,
            module_signatures,
        );
        let binding_use_report = carina_core::validation::check_bare_binding_uses(&context);
        let export_value_bindings =
            carina_core::binding_index::ResolvedBindings::for_authored_validation(binding_input);
        DiagnosticValidationInputs {
            binding_index,
            inference_bindings,
            binding_use_report,
            export_value_bindings,
        }
    }

    fn collect_diagnostics(
        &self,
        doc: &Document,
        current_file_path: Option<&Path>,
    ) -> Vec<Diagnostic> {
        #[cfg(test)]
        if let Some(current_file_path) = current_file_path {
            assert_eq!(
                doc.source_file(),
                current_file_path,
                "document source path must match the analyzed path",
            );
        }
        let mut diagnostics = Vec::new();
        let provider_validation_deduplicator = ProviderValidationDeduplicator::default();
        let text = doc.text();
        let buffer_parsed = doc.parsed();
        // Validate the current AST before a cached parser snapshot can supply
        // directory evidence. Recovery must not hide a malformed current view.
        let buffer_analysis =
            buffer_parsed.and_then(|parsed| analysis_tree(doc, parsed, &mut diagnostics));
        let pinned_base = current_file_path
            .and_then(|path| path.parent())
            .and_then(|path| carina_core::paths::ResolvedPath::new(path).ok());
        let base_path = pinned_base.as_ref();
        let current_file_name = current_file_path.map(|path| {
            path.file_name()
                .expect("a current file path must name a file")
                .to_string_lossy()
        });
        let current_file_name = current_file_name.as_deref();

        // Build the directory-scoped merged parse once up-front — it is
        // the authoritative source of truth for every check that needs
        // cross-file context: undefined-identifier (#2132), upstream-
        // state field references, for-iterable bindings, unused-let
        // filtering, and exports type validation (#2134). The buffer is
        // substituted for its on-disk copy so diagnostics update on
        // keystrokes.
        let merged_attempt =
            base_path.map(|base| self.parse_merged_with_buffer(doc, current_file_name, base));
        let overrides = carina_core::config_loader::SourceOverrides::from([(
            doc.source_file().to_path_buf(),
            text.clone(),
        )]);
        // A failed semantic parse can still have a complete declaration
        // inventory. Admit that snapshot before any source-oriented consumer,
        // then reuse its judged file for placement and expression reporting.
        let recovery_configuration = match (
            merged_attempt.as_ref(),
            doc.parsed_buffer(),
            buffer_analysis.as_ref(),
        ) {
            (Some(Ok(_)), _, _) | (_, None, _) | (_, _, None) => None,
            (Some(Err(_)) | None, Some(buffer), Some(_)) => base_path
                .and_then(|base| {
                    carina_core::config_loader::DirectoryBufferSnapshot::capture(
                        base,
                        buffer.clone(),
                        &overrides,
                    )
                    .ok()
                })
                .map(|snapshot| {
                    carina_core::config_loader::load_buffer_configuration_in_directory(
                        snapshot,
                        &self.schemas,
                        self.provider_context.catalog(),
                    )
                })
                .or_else(|| {
                    carina_core::config_loader::load_buffer_configuration_without_directory(
                        buffer,
                        &self.schemas,
                        self.provider_context.catalog(),
                    )
                    .ok()
                }),
        };
        let recovered_parsed = recovery_configuration
            .as_ref()
            .map(|configuration| configuration.parsed());
        let buffer_parsed = recovered_parsed.or(buffer_parsed);
        let buffer_analysis = if let Some(parsed) = recovered_parsed {
            analysis_tree(doc, parsed, &mut diagnostics)
        } else {
            buffer_analysis
        };
        let duplicates = match merged_attempt
            .as_ref()
            .and_then(|attempt| attempt.as_ref().ok())
        {
            Some(result) => result.directory.duplicate_declarations.clone(),
            None => match buffer_analysis.as_ref() {
                Some(tree) => carina_core::config_loader::find_duplicate_declarations_in_analysis(
                    doc.source_file(),
                    tree,
                ),
                None => carina_core::config_loader::find_duplicate_declarations_in_source(
                    &text,
                    doc.source_file(),
                ),
            },
        };
        let mut failed_directory_type_resolution = None;
        let directory_error_emitted = if let (Some(Err(error)), Some(current_file_path)) =
            (merged_attempt.as_ref(), current_file_path)
            && error
                .source_file()
                .is_some_and(|source_file| source_file == current_file_path)
            && let Some(parse_error) = error.parse_error()
        {
            // The directory parse has the classified sibling bindings and is
            // therefore more authoritative than the buffer-only parse (which
            // may have treated a sibling name as an enum identifier). One
            // exception is an unresolved declaration type: the bootstrap AST
            // retains that declaration and the typed resolver below can attach
            // its context and exact token span. In that case the typed finding
            // covers this coarse parser backstop.
            let type_resolution = buffer_analysis.as_ref().map(|tree| {
                carina_core::validation::resolve_file_type_exprs_from_analysis(
                    tree,
                    self.provider_context,
                    Some(carina_core::validation::TypeResolutionParserError::new(
                        current_file_path,
                        parse_error,
                    )),
                )
                .1
            });
            let covered_by_typed_resolution = type_resolution
                .as_ref()
                .is_some_and(|report| report.covered_parser_error());
            if !covered_by_typed_resolution
                && !duplicates
                    .iter()
                    .any(|duplicate| duplicate.covers_parse_error(parse_error))
            {
                if let Some(finding) = error.element_diagnostic() {
                    diagnostics.extend(render_merged(
                        doc,
                        finding,
                        DiagnosticSeverity::ERROR,
                        buffer::parse_error_message(parse_error),
                    ));
                } else if let Some(finding) = error.diagnostic() {
                    diagnostics.extend(render_parse(
                        doc,
                        &finding,
                        DiagnosticSeverity::ERROR,
                        buffer::parse_error_message(parse_error),
                    ));
                }
            }
            failed_directory_type_resolution = type_resolution;
            true
        } else {
            false
        };
        let reported_directory_failure = merged_attempt
            .as_ref()
            .and_then(|attempt| attempt.as_ref().err())
            .and_then(|error| error.element_diagnostic())
            .filter(|_| directory_error_emitted)
            .map(|finding| finding.location().clone());
        let mut merged_result = merged_attempt.and_then(Result::ok);
        // Keep the parsed input stable while module-analysis results are filled
        // in. Cloning this Arc preserves the exact sealed directory without
        // cloning or revalidating the AST.
        let directory = merged_result
            .as_ref()
            .map(|result| Arc::clone(&result.directory));
        // A buffer parsed by itself cannot resolve sibling user functions.
        // When the authoritative directory parse succeeds, use its per-file
        // semantic AST for source-oriented checks instead of dropping every
        // check merely because the provisional document parse failed.
        let merged_current_file = current_file_path.and_then(|path| {
            directory
                .as_ref()
                .and_then(|directory| directory.semantic_file(path))
        });
        let current_parsed = buffer_parsed.or(merged_current_file);
        // A source parsed in isolation cannot know sibling function
        // declarations. Prefer the successful directory parse over that
        // provisional failure (notably a sibling user function used as a
        // `for` iterable). When the directory parse also fails—or there is no
        // directory context—the document-owned parse error remains
        // authoritative.
        if merged_result.is_none()
            && !directory_error_emitted
            && let Some(error) = doc.parse_error()
            && !duplicates
                .iter()
                .any(|duplicate| duplicate.covers_parse_error(error))
        {
            diagnostics.extend(buffer::render_document_parse_error(doc));
        }
        diagnostics.extend(self.duplicate_declaration_diagnostics(doc, &duplicates));
        let merged = directory
            .as_ref()
            .map(|directory| directory.parsed.parsed());
        let merged_analysis =
            merged.and_then(|parsed| analysis_tree(doc, parsed, &mut diagnostics));
        let current_analysis = if buffer_parsed.is_some() {
            buffer_analysis
        } else {
            current_parsed.and_then(|parsed| analysis_tree(doc, parsed, &mut diagnostics))
        };
        let merged = merged.filter(|_| merged_analysis.is_some());
        let current_parsed = current_parsed.filter(|_| current_analysis.is_some());
        let preferred_analysis = merged_analysis.as_ref().or(current_analysis.as_ref());

        // Module checks need declaration and upstream authority before their
        // single run. Both use the same sealed directory throughout analysis.
        let type_resolution = failed_directory_type_resolution.take().or_else(|| {
            preferred_analysis.map(|tree| {
                carina_core::validation::resolve_file_type_exprs_from_analysis(
                    tree,
                    self.provider_context,
                    None,
                )
                .1
            })
        });
        let upstream_resolution = match (base_path, merged_analysis.as_ref()) {
            (Some(base), Some(merged)) => analysis_result(
                doc,
                self.resolve_diagnostic_upstream_exports(base, merged, &overrides),
                &mut diagnostics,
            ),
            (Some(_base), None) => None,
            (None, Some(_merged)) => None,
            (None, None) => None,
        };

        if let (Some(base), Some(result), Some(tree)) =
            (base_path, merged_result.as_mut(), merged_analysis.as_ref())
        {
            let empty_exports = Default::default();
            let exports = upstream_resolution
                .as_ref()
                .map(|(exports, _)| exports)
                .unwrap_or(&empty_exports);
            self.resolve_merged_modules(result, tree, base, exports, &overrides);
        }
        let expanded_graph =
            merged_result
                .as_ref()
                .and_then(|result| match &result.module_resolution {
                    checks::MergedModuleResolution::Expanded { graph } => Some(graph.as_ref()),
                    _ => None,
                });
        let expanded_analysis =
            expanded_graph.and_then(|graph| analysis_tree(doc, graph, &mut diagnostics));
        let expanded_graph = expanded_graph.filter(|_| expanded_analysis.is_some());
        if let (Some(base), Some(result), Some(tree)) =
            (base_path, merged_result.as_ref(), merged_analysis.as_ref())
        {
            diagnostics.extend(self.check_mixed_tag_key_styles(doc, base, &result.directory, tree));
        }
        if let Some(boundary_file) = current_parsed {
            let boundary =
                parser_placeholder_boundary(merged.unwrap_or(boundary_file).directory_verdict());
            if let Some(merged) = merged {
                diagnostics.extend(self.check_expansion_boundary(
                    doc,
                    merged,
                    boundary,
                    checks::ExpansionBoundaryScope::Authored,
                ));
                diagnostics.extend(self.check_expansion_boundary(
                    doc,
                    boundary_file,
                    boundary,
                    checks::ExpansionBoundaryScope::Unlocated,
                ));
            } else {
                diagnostics.extend(self.check_expansion_boundary(
                    doc,
                    boundary_file,
                    boundary,
                    checks::ExpansionBoundaryScope::All,
                ));
            }
        }
        if let (Some(current_file), Some(tree)) = (current_parsed, preferred_analysis) {
            diagnostics.extend(self.attribute_derived_anonymous_resource_diagnostics(
                doc,
                merged.unwrap_or(current_file),
                tree,
            ));
        }
        if let (Some(result), Some(current_file_path)) = (merged_result.as_ref(), current_file_path)
            && let Some(current_file) = result
                .directory
                .root_expanded_semantic_file(current_file_path)
        {
            let directory = result.directory.root_expanded();
            let current_tree = analysis_tree(doc, &current_file, &mut diagnostics);
            let directory_tree = analysis_tree(doc, &directory, &mut diagnostics);
            if let (Some(current_tree), Some(directory_tree)) =
                (current_tree.as_ref(), directory_tree.as_ref())
            {
                diagnostics.extend(self.loop_anonymous_identity_diagnostics(
                    doc,
                    (&current_file, current_tree),
                    (&directory, directory_tree),
                    expanded_graph.zip(expanded_analysis.as_ref()),
                    base_path.map(|base| base.source_label()).as_deref(),
                ));
            }
        }
        if let Some(result) = merged_result.as_ref() {
            for error in &result.analysis_errors {
                diagnostics.extend(render_analysis_invariant(doc, error));
            }
        }

        if let (Some(merged), Some((exports, resolve_errors))) =
            (merged_analysis.as_ref(), upstream_resolution.as_ref())
        {
            diagnostics.extend(self.check_upstream_state_field_references(
                doc,
                merged,
                exports,
                resolve_errors,
            ));
            diagnostics.extend(self.check_for_iterable_bindings(doc, merged));
        }

        // carina#2191 Phase 5: surface `check_provider_instance_routing`
        // errors as editor diagnostics. CLI commands run the same pass in
        // their shared validation pipeline; LSP runs it against the merged
        // parse so users see the same "unknown provider instance" / "no
        // default instance for kind" message before `carina validate`.
        if let Some(tree) = merged_analysis.as_ref() {
            for finding in carina_core::parser::check_provider_instance_routing(tree) {
                if let Some(diagnostic) = render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                ) {
                    diagnostics.push(diagnostic);
                }
            }
        }

        // The diagnostic directory parse deliberately preserves a user-function
        // call when evaluating its body fails, so the rest of the editor checks
        // can still inspect the authored call. Run the strict resolver on a
        // clone after declaration resolution so the session already owns any
        // failed signatures. This surfaces source-owned body errors (for example, a
        // sibling call that binds a structural handle and then uses the
        // parameter bare). Resolution retains the parser-owned expression
        // location through the merged pass.
        if let Some(tree) = merged_analysis.as_ref()
            && let Err(error) = carina_core::parser::resolve_resource_refs_from_analysis(
                tree,
                self.provider_context,
            )
        {
            diagnostics.extend(render_analysis_resolution_error(doc, &error));
        }
        // Every catalog runs the same declaration producers. An incomplete
        // catalog records deferrals; a loaded owner produces strict diagnostics.
        {
            let custom_type_findings = match merged {
                Some(_) => type_resolution
                    .as_ref()
                    .map(|report| report.diagnostics().to_vec())
                    .unwrap_or_default(),
                None => {
                    if let Some(tree) = current_analysis.as_ref()
                        && let Err(error) = carina_core::parser::resolve_resource_refs_from_analysis(
                            tree,
                            self.provider_context,
                        )
                    {
                        diagnostics.extend(render_analysis_resolution_error(doc, &error));
                    }
                    type_resolution
                        .as_ref()
                        .map(|report| report.diagnostics().to_vec())
                        .unwrap_or_default()
                }
            };
            for finding in custom_type_findings {
                if let Some(diagnostic) = render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                ) {
                    diagnostics.push(diagnostic);
                }
            }
        }

        // Prefer the directory-merged parse so cross-file binding refs
        // resolve; fall back to the buffer-only parse otherwise.
        if let Some(tree) = preferred_analysis
            && let Some(argument_types) = type_resolution
                .as_ref()
                .map(|report| report.resolved_argument_types())
        {
            diagnostics.extend(self.check_depends_on(doc, tree, argument_types));
        }
        if let Some(tree) = preferred_analysis {
            diagnostics.extend(self.check_wait_bindings(doc, tree));
            diagnostics.extend(self.check_deferred_populate_refs(doc, tree));
        }

        // Provider-attribute finalize diagnostic (#2717 / #2753): if a
        // non-literal `default_tags` (or future deferred attribute) does
        // not resolve to the expected shape, surface that error as a
        // diagnostic so editors flag it the same way `carina validate`
        // does.
        if let Some(base) = base_path
            && let Some(err) = self.finalize_provider_diagnostic(doc, current_file_name, base)
            && let Some(diagnostic) = render_analysis_resolution_error(doc, &err)
            && !diagnostics.contains(&diagnostic)
        {
            diagnostics.push(diagnostic);
        }

        // Directory binding authority for parser-owned reference occurrences.
        // When the merged parse succeeds, `BindingNameSet::from_parsed`
        // yields every binding declared anywhere in the directory. When
        // it fails (`merged = None` because a parse error somewhere in
        // the directory blocked the merge), fall back to a per-file
        // resilient walk: parse each sibling `.crn` independently,
        // swallow per-file errors, run `BindingNameSet::from_parsed` on
        // each successful parse. Without this, sibling-defined
        // bindings (including `arguments`, `import`, `fn`, ...) redline
        // as Unknown the moment any sibling fails to parse (#2445).
        // When no `base_path` is available at all, the buffer-only scan
        // remains the last resort.
        let declared_providers = self.extract_declared_provider_names(&text);
        let known_bindings: HashSet<String> = match (merged, base_path) {
            (Some(merged), _) => carina_core::binding_index::BindingNameSet::from_parsed(merged)
                .iter_names()
                .map(String::from)
                .collect(),
            (None, Some(base)) => {
                self.collect_sibling_binding_names(&text, current_file_name, base)
            }
            (None, None) => current_parsed.map_or_else(
                || self.extract_resource_bindings(crate::completion::DslSource::BufferOnly(&text)),
                |parsed| {
                    carina_core::binding_index::BindingNameSet::from_parsed(parsed)
                        .iter_names()
                        .map(String::from)
                        .collect()
                },
            ),
        };
        if let Some(tree) = preferred_analysis {
            diagnostics.extend(self.check_undefined_references(
                doc,
                tree,
                &known_bindings,
                &declared_providers,
            ));
        }

        // Semantic analysis on parsed file
        if let (Some(parsed), Some(tree), Some(preferred_tree)) = (
            current_parsed,
            current_analysis.as_ref(),
            preferred_analysis,
        ) {
            diagnostics.extend(self.check_configuration_placement(doc, preferred_tree));
            diagnostics.extend(self.check_backend_config(
                doc,
                merged_result.as_ref(),
                preferred_analysis,
                buffer_parsed,
                reported_directory_failure.as_ref(),
            ));

            // Check provider region
            diagnostics.extend(self.check_provider_region(
                doc,
                preferred_tree,
                &provider_validation_deduplicator,
            ));
            let default_tag_diagnostics = if let Some(graph) = expanded_graph {
                self.check_provider_default_tags(
                    doc,
                    preferred_tree,
                    &graph.providers,
                    &carina_core::binding_index::ResolvedBindings::for_expanded_validation(graph),
                )
            } else {
                self.check_provider_default_tags(
                    doc,
                    preferred_tree,
                    preferred_tree.providers(),
                    &carina_core::binding_index::ResolvedBindings::for_authored_validation(
                        merged.unwrap_or(parsed),
                    ),
                )
            };
            diagnostics.extend(default_tag_diagnostics);

            // Check for unloaded providers
            diagnostics.extend(self.check_unloaded_providers(doc, preferred_tree));

            if let Some(base) = base_path {
                diagnostics.extend(self.check_upstream_state_sources(doc, preferred_tree, base));
            }
            // Build the canonical validation-target index from the merged,
            // module-expanded parse when available. The checks still walk the
            // buffer-only AST for source anchoring, but their binding authority
            // must include sibling resources and expanded compositions. Fall
            // back to the buffer parse only when directory merging failed. This
            // deliberately gives both composition-attribute existence checks
            // and `check_resource_ref_type_mismatch` directory-wide targets.
            let directive_boundary =
                parser_placeholder_boundary(merged.unwrap_or(parsed).directory_verdict());
            let directive_errors = if let Some(graph) = expanded_graph {
                graph.directive_resolution_errors(
                    directive_boundary,
                    &self.schemas,
                    self.provider_context.catalog(),
                )
            } else {
                merged.unwrap_or(parsed).directive_resolution_errors(
                    directive_boundary,
                    &self.schemas,
                    self.provider_context.catalog(),
                )
            };
            let directive_errors = match directive_errors {
                Ok(errors) => errors,
                Err(error) => {
                    diagnostics.push(Diagnostic {
                        range: Range::default(),
                        severity: Some(DiagnosticSeverity::ERROR),
                        source: Some("carina".into()),
                        message: error.to_string(),
                        ..Default::default()
                    });
                    Vec::new()
                }
            };
            let mut directive_sites = HashSet::new();
            for error in directive_errors {
                if !directive_sites.insert((error.resource().to_owned(), error.diagnostic_key())) {
                    continue;
                }
                for finding in error
                    .diagnostics()
                    .expect("expansion boundary provenance invariant")
                {
                    diagnostics.extend(render_merged(
                        doc,
                        &finding,
                        DiagnosticSeverity::ERROR,
                        error.message(),
                    ));
                }
            }
            let pre_expansion_input = merged.unwrap_or(parsed);
            let fallback_configuration;
            let pre_expansion_configuration =
                if let Some(configuration) = recovery_configuration.as_ref() {
                    configuration
                } else {
                    fallback_configuration = carina_core::config_loader::load_buffer_configuration(
                        pre_expansion_input,
                        &self.schemas,
                        self.provider_context.catalog(),
                    );
                    &fallback_configuration
                };
            let expression_report = pre_expansion_configuration.judged().report();
            for finding in expression_report.findings() {
                let root = match finding.detail() {
                    carina_core::static_type::InferenceError::RequireOutsideModule(root) => {
                        Some(root)
                    }
                    _ => None,
                };
                let message =
                    root.map_or_else(|| finding.detail().to_string(), |root| root.lsp_message());
                if let Some(mut diagnostic) =
                    render_merged(doc, finding, DiagnosticSeverity::ERROR, message)
                {
                    if let Some(root) = root {
                        attach_directory_evidence(&mut diagnostic, root.root_evidence());
                    }
                    if diagnostics.contains(&diagnostic) {
                        continue;
                    }
                    let owner = expression_report
                        .expression(finding.location())
                        .ok()
                        .and_then(JudgedExpr::boundary_owner);
                    let owned_range = owner.and_then(|owner| {
                        render_merged(
                            doc,
                            &carina_core::parser::Located::new(owner.clone(), ()),
                            DiagnosticSeverity::ERROR,
                            String::new(),
                        )
                        .map(|diagnostic| diagnostic.range)
                    });
                    if owned_range.is_some_and(|range| {
                        diagnostics.iter().any(|existing| {
                            existing.range == range
                                && existing.severity == Some(DiagnosticSeverity::ERROR)
                        })
                    }) {
                        continue;
                    }
                    diagnostics.push(diagnostic);
                }
            }
            let semantic_view = checks::SemanticView::for_analysis(
                if merged.is_none() {
                    pre_expansion_configuration.parsed()
                } else {
                    parsed
                },
                merged,
                base_path.is_some(),
            );
            let empty_upstream_exports = Default::default();
            let upstream_exports = upstream_resolution
                .as_ref()
                .map(|(exports, _)| exports)
                .unwrap_or(&empty_upstream_exports);
            let module_signatures = base_path
                .map(|base| {
                    expanded_graph.map_or_else(
                        || {
                            carina_core::module_resolver::load_resolved_module_signatures_with_overrides(
                                pre_expansion_input,
                                base,
                                self.provider_context.catalog(),
                                &overrides,
                            )
                        },
                        |graph| {
                            carina_core::module_resolver::load_resolved_module_signatures_with_overrides(
                                graph,
                                base,
                                self.provider_context.catalog(),
                                &overrides,
                            )
                        },
                    )
                })
                .unwrap_or_default();
            let validation_inputs = self.validation_inputs(
                pre_expansion_configuration.judged(),
                upstream_exports,
                &module_signatures,
            );
            let expanded_upstream_exports = match (base_path, expanded_analysis.as_ref()) {
                (Some(base), Some(graph)) => analysis_result(
                    doc,
                    self.resolve_diagnostic_upstream_exports(base, graph, &overrides),
                    &mut diagnostics,
                )
                .map(|(exports, _)| exports)
                .unwrap_or_default(),
                (Some(_base), None) => Default::default(),
                (None, Some(_graph)) => Default::default(),
                (None, None) => Default::default(),
            };
            let expanded_configuration = expanded_graph.map(|graph| {
                carina_core::config_loader::load_buffer_configuration(
                    graph,
                    &self.schemas,
                    self.provider_context.catalog(),
                )
            });
            let expanded_validation_tree =
                expanded_configuration.as_ref().and_then(|configuration| {
                    carina_core::parser::AnalysisTree::try_from_file(configuration.judged().file())
                        .ok()
                });
            let mut expanded_validation_inputs = expanded_configuration.as_ref().and_then(|configuration| {
                let configuration = configuration.judged();
                let graph = configuration.file();
                let tree = expanded_validation_tree.as_ref()?;
                let mut resources = HashMap::new();
                for resource in tree.resources() {
                    resources.entry(resource.id()).or_insert(resource);
                }
                Some(ExpandedDiagnosticValidationInputs {
                    configuration,
                    resources,
                    coverage: Default::default(),
                    binding_index:
                        carina_core::binding_index::BindingIndex::from_parsed_with_upstream_exports_and_module_signatures_in_session(
                            graph, &self.schemas, Some(&expanded_upstream_exports), &module_signatures, self.provider_context,
                        ),
                })
            });
            let DiagnosticValidationInputs {
                binding_index,
                inference_bindings,
                binding_use_report,
                export_value_bindings,
            } = validation_inputs;
            // Validate non-resource declarations authored in the open file,
            // even when no typed resource sink consumes them. Binding and
            // schema authority remains directory-wide, while source anchoring
            // comes from each authored operator.
            for finding in carina_core::validation::check_file_integer_arithmetic_values(
                pre_expansion_configuration.judged(),
                &binding_index,
            ) {
                if let Some(diagnostic) = render_merged(
                    doc,
                    &finding,
                    DiagnosticSeverity::ERROR,
                    finding.detail().to_string(),
                ) && !diagnostics.contains(&diagnostic)
                {
                    diagnostics.push(diagnostic);
                }
            }
            // The shared authority owns ordinary direct bare-binding sinks
            // even when they succeed. Admitted single builtin string
            // constructions retain their enclosing schema value checks.
            // Native interpolation leaves its enclosing schema check active.
            // An enum-typed argument must not
            // be reinterpreted by the LSP's legacy BindingIndex pass using a
            // differently enriched bootstrap identity. Nested uses keep the
            // surrounding schema walk to validate unrelated sibling leaves.
            let binding_error_sinks = parsed
                .bare_binding_uses()
                .uses()
                .filter(|use_site| {
                    carina_core::validation::binding_use_authority_owns_use(
                        &binding_use_report,
                        use_site,
                    )
                })
                .filter_map(|use_site| match &use_site.owner {
                    carina_core::parser::BindingUseOwner::Resource(address) => Some((
                        address.resource_id().clone(),
                        use_site.path.root_name().to_string(),
                    )),
                    carina_core::parser::BindingUseOwner::ModuleArgument { .. } => None,
                })
                .collect::<HashSet<_>>();
            let pending_resource_sinks = parsed
                .bare_binding_uses()
                .pending_slots()
                .filter_map(|slot| match &slot.owner {
                    carina_core::parser::BindingUseOwner::Resource(address) => Some((
                        address.resource_id().clone(),
                        slot.path.root_name().to_string(),
                    )),
                    carina_core::parser::BindingUseOwner::ModuleArgument { .. } => None,
                })
                .collect::<HashSet<_>>();
            for finding in binding_use_report.resource_findings() {
                let located = finding.diagnostic();
                let Some(mut diagnostic) = render_merged(
                    doc,
                    &located,
                    DiagnosticSeverity::ERROR,
                    finding.error.to_string(),
                ) else {
                    continue;
                };
                if let Some(type_error) = finding.error.type_error() {
                    decorate_schema_diagnostic(&mut diagnostic, type_error);
                }
                diagnostics.push(diagnostic);
            }
            let semantic_file = current_file_path.and_then(|path| {
                merged_result
                    .as_ref()
                    .and_then(|result| result.directory.semantic_file(path))
            });
            let semantic_tree = semantic_file
                .and_then(|file| carina_core::parser::AnalysisTree::try_from_file(file).ok());
            let authored_inputs = if let Some(graph) = expanded_graph {
                graph.authored_inputs().cloned()
            } else {
                match directory.as_ref() {
                    Some(directory) => Ok(
                        carina_core::input_rules::AuthoredInputs::for_directory_in_session(
                            &directory.parsed,
                            self.provider_context,
                        ),
                    ),
                    None => doc
                        .parsed()
                        .ok_or(carina_core::input_rules::AuthoredInputUnavailable)
                        .and_then(|parsed| parsed.authored_inputs())
                        .map(|inputs| inputs.resolved_in_session(self.provider_context)),
                }
            };
            let input_findings = authored_inputs
                .map_err(carina_core::provider::HostError::from)
                .map(|inputs| {
                    carina_core::input_rules::check_authored_inputs(&inputs, &self.schemas)
                });
            let input_owners = preferred_tree
                .resources()
                .map(|resource| (resource.id(), resource))
                .collect::<std::collections::HashMap<_, _>>();
            match input_findings {
                Ok(report) => {
                    for outcome in report {
                        use carina_core::input_rules::{
                            AuthoredInputOutcome, InputFindingSeverity,
                        };
                        let (resource_id, location, severity, message) = match &outcome {
                            AuthoredInputOutcome::Finding(finding) => (
                                finding.resource_id(),
                                finding.authored_location(),
                                match finding.severity() {
                                    InputFindingSeverity::Error => DiagnosticSeverity::ERROR,
                                    InputFindingSeverity::Warning => DiagnosticSeverity::WARNING,
                                },
                                finding.to_string(),
                            ),
                            AuthoredInputOutcome::Pending(_) => continue,
                            AuthoredInputOutcome::PreparationFailed(error) => (
                                error.resource_id(),
                                error.authored_location(),
                                DiagnosticSeverity::ERROR,
                                error.to_string(),
                            ),
                        };
                        let code = match &outcome {
                            AuthoredInputOutcome::Finding(finding) => {
                                Some(tower_lsp::lsp_types::NumberOrString::String(
                                    finding.violation().code().to_string(),
                                ))
                            }
                            _ => None,
                        };
                        let location = location.or_else(|| {
                            input_owners
                                .get(resource_id)
                                .map(|resource| resource.type_location())
                        });
                        if let Some(location) = location {
                            diagnostics.extend(
                                render_merged(
                                    doc,
                                    &carina_core::parser::Located::new(location.clone(), ()),
                                    severity,
                                    message,
                                )
                                .map(|mut diagnostic| {
                                    diagnostic.code = code.clone();
                                    diagnostic
                                }),
                            );
                        } else {
                            diagnostics.push(Diagnostic {
                                range: Range::default(),
                                severity: Some(severity),
                                source: Some("carina".into()),
                                code,
                                message,
                                ..Default::default()
                            });
                        }
                    }
                }
                Err(error) => diagnostics.push(Diagnostic {
                    range: Range::default(),
                    severity: Some(DiagnosticSeverity::ERROR),
                    source: Some("carina".into()),
                    message: error.to_string(),
                    ..Default::default()
                }),
            }
            diagnostics.extend(self.check_user_function_ref_types(
                doc,
                preferred_tree,
                &binding_index,
                expression_report,
            ));
            let expanded_compositions = expanded_graph.map(|graph| graph.compositions.as_slice());
            let resolver_constraint_findings = match merged_result.as_ref() {
                Some(result) => match &result.module_resolution {
                    checks::MergedModuleResolution::Expanded { .. } => Vec::new(),
                    checks::MergedModuleResolution::ConstraintFailed {
                        findings,
                        errors: _errors,
                    } => findings.clone(),
                    checks::MergedModuleResolution::Unowned { errors: _errors } => Vec::new(),
                    checks::MergedModuleResolution::Unanalyzed => Vec::new(),
                },
                None => Vec::new(),
            };
            let residual_errors: &[carina_core::module_resolver::ResidualModuleResolutionError] =
                match merged_result.as_ref() {
                    Some(result) => match &result.module_resolution {
                        checks::MergedModuleResolution::Expanded { .. } => &[],
                        checks::MergedModuleResolution::ConstraintFailed {
                            findings: _findings,
                            errors,
                        } => errors.as_slice(),
                        checks::MergedModuleResolution::Unowned { errors } => errors.as_slice(),
                        checks::MergedModuleResolution::Unanalyzed => &[],
                    },
                    None => &[],
                };
            let authored_findings = if let Some(result) = merged_result.as_ref() {
                result.authored_module_findings.clone()
            } else {
                let result = carina_core::validation::check_module_calls_with_overrides(
                    carina_core::validation::ModuleCallCheckInput {
                        authored: parsed,
                        semantic: merged,
                        base_dir: base_path,
                        imported_signatures: &module_signatures,
                        provider_context: self.provider_context,
                        schemas: &self.schemas,
                        binding_use_report: &binding_use_report,
                        bindings: &binding_index,
                        reference_checks:
                            carina_core::validation::ModuleCallReferenceChecks::Enabled,
                    },
                    &overrides,
                );
                analysis_result(doc, result, &mut diagnostics).unwrap_or_default()
            };
            let expanded_binding_findings = match expanded_compositions {
                Some(compositions) => carina_core::validation::check_expanded_module_calls(
                    carina_core::validation::ExpandedModuleCallCheckInput::BindingUses {
                        binding_use_report: &binding_use_report,
                        compositions,
                    },
                    self.provider_context.catalog(),
                ),
                None => Vec::new(),
            };
            let expanded_reference_findings = match expanded_validation_inputs.as_ref() {
                Some(inputs) => carina_core::validation::check_expanded_module_calls(
                    carina_core::validation::ExpandedModuleCallCheckInput::References {
                        compositions: &inputs.configuration.file().compositions,
                        bindings: &inputs.binding_index,
                    },
                    self.provider_context.catalog(),
                ),
                None => Vec::new(),
            };
            let findings = carina_core::validation::merge_module_call_findings([
                authored_findings,
                expanded_binding_findings,
                expanded_reference_findings,
                resolver_constraint_findings,
            ]);
            diagnostics.extend(render_module_call_findings(doc, &findings));
            if let Some(base_path) = base_path {
                for error in residual_errors {
                    if error.element_diagnostic().is_some_and(|element| {
                        expression_report
                            .findings()
                            .iter()
                            .any(|finding| finding.location() == element.location())
                    }) {
                        continue;
                    }
                    diagnostics.extend(
                        self.module_resolver_error_diagnostics(doc, tree, error, base_path),
                    );
                }
            }
            if let Some(inputs) = expanded_validation_inputs.as_mut() {
                // A rejected call already owns the diagnostic. Do not add a
                // downstream resource consequence of that same rejection.
                let rejected_calls: Vec<_> = diagnostics
                    .iter()
                    .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
                    .map(|diagnostic| diagnostic.range)
                    .collect();
                let authored_ids: HashSet<_> = tree
                    .project_resources()
                    .map(|resource| resource.id().clone())
                    .collect();
                let configuration = inputs.configuration;
                for resource in &configuration.file().resources {
                    if authored_ids.contains(&resource.id) {
                        continue;
                    }
                    let Some(carina_core::resource::ModuleSource::Module(origin)) =
                        &resource.module_source
                    else {
                        continue;
                    };
                    let call = &origin.module_call_path().root().id;
                    let Some(report) =
                        inputs.resource_admissions(&resource.id, self.provider_context)
                    else {
                        #[cfg(feature = "test-utils")]
                        replay_coverage::declined_outcome(false);
                        continue;
                    };
                    for error in report.errors() {
                        let finding = carina_core::parser::Located::new(
                            call.declaration_location().clone(),
                            error,
                        );
                        if let Some(diagnostic) = render_merged(
                            doc,
                            &finding,
                            DiagnosticSeverity::ERROR,
                            format!("{}: {}", resource.id, finding.detail()),
                        ) && !rejected_calls.contains(&diagnostic.range)
                            && !diagnostics.contains(&diagnostic)
                        {
                            diagnostics.push(diagnostic);
                        }
                    }
                }
            }
            // Admission reads the sealed slots and projections owned by the
            // judged directory. Source locations still anchor each diagnostic.
            // Authored inventory is optional on main's buffer path. Its
            // absence selects report-based admission, not a new diagnostic.
            let authored_tree = carina_core::parser::AnalysisTree::try_from_file(
                pre_expansion_configuration.judged().file(),
            )
            .ok();
            let authored_resources = authored_tree.as_ref().map(authored_validation_resources);
            // The document AST intentionally preserves function calls for
            // source-oriented checks. Build a parallel, same-order view with
            // locally resolvable calls evaluated so schema diagnostics see
            // the actual value type (not a deferred FunctionCall). The value
            // itself carries any diagnostic-sensitivity provenance.
            let resolved_buffer = {
                match carina_core::parser::resolve_resource_refs_from_analysis(
                    tree,
                    self.provider_context,
                ) {
                    Ok(resolved) => Some(resolved),
                    Err(error) => {
                        if matches!(&error, carina_core::parser::AnalysisResolutionError::Diagnostic(finding)
                            if matches!(finding.detail(), ParseError::SecretDerivedBackendAttribute { .. }))
                        {
                            diagnostics.extend(render_analysis_resolution_error(doc, &error));
                        }
                        None
                    }
                }
            };
            let resolved_buffer_resources = resolved_buffer
                .as_ref()
                .map(|resolved| resolved.iter_all_resources().collect::<Vec<_>>());
            // Every schema finding is anchored by the sealed parser view,
            // including the semantic per-file fallback after a buffer parse fails.
            let mut authored_coverage = carina_core::validation::admission::WalkCoverage::default();
            for (resource_index, rref) in tree.project_resources().enumerate() {
                let resource_id = rref.id();
                let resource_attributes = rref.attributes();
                let resolved_resource_attributes = resolved_buffer_resources
                    .as_ref()
                    .and_then(|resources| resources.get(resource_index))
                    .map(|resource| resource.attributes());
                let semantic_resource_attributes =
                    semantic_tree.as_ref().and_then(|semantic_tree| {
                        semantic_tree
                            .resources()
                            .find(|candidate| {
                                candidate.location() == rref.location()
                                    && candidate.binding_address() == rref.binding_address()
                            })
                            .map(|candidate| candidate.attributes().into_owned())
                    });
                let resource_quoted_string_attrs = rref.quoted_string_attrs();
                let resource_schema = || rref.schema(&self.schemas);
                let provider = &resource_id.provider;
                let full_resource_type = format!("{}.{}", provider, resource_id.resource_type);

                if resource_schema().is_none() {
                    #[cfg(feature = "test-utils")]
                    replay_coverage::missing_schema(AdmissionPath::Relative, provider);
                    let provider_loaded = self.provider_names.contains(&provider.to_string());

                    let (severity, message) = if let Some(reason) =
                        self.provider_errors.get(provider)
                    {
                        (
                            DiagnosticSeverity::INFORMATION,
                            format!("Provider '{}' is not loaded: {}", provider, reason),
                        )
                    } else if self.known_provider_names.contains(provider) {
                        (
                            DiagnosticSeverity::ERROR,
                            format!("Unknown resource type: {full_resource_type}"),
                        )
                    } else if !provider_loaded {
                        match preferred_tree.directory_verdict() {
                            carina_core::config_loader::DirectoryVerdict::Root(_)
                            | carina_core::config_loader::DirectoryVerdict::NoEvidence(_) => {}
                            carina_core::config_loader::DirectoryVerdict::ModuleTemplate(_)
                            | carina_core::config_loader::DirectoryVerdict::Unknown(_) => continue,
                        }
                        (
                            DiagnosticSeverity::ERROR,
                            format!(
                                "Provider '{}' has no loaded schema for this file: no provider block for it was found in this file's configuration.",
                                provider
                            ),
                        )
                    } else {
                        (
                            DiagnosticSeverity::WARNING,
                            format!(
                                "No schema for {}.{} — attribute validation skipped",
                                provider, resource_id.resource_type
                            ),
                        )
                    };
                    let finding =
                        carina_core::parser::Located::new(rref.type_location().clone(), message);
                    diagnostics.extend(render_merged(
                        doc,
                        &finding,
                        severity,
                        finding.detail().clone(),
                    ));
                }

                // Semantic validation using schema
                let validated_schema = resource_schema();
                if let Some(validated_schema) = validated_schema {
                    let schema = validated_schema.as_schema();
                    if schema.is_data_source()
                        && rref.kind() != carina_core::parser::AnalysisItemKind::DataSource
                    {
                        let finding = carina_core::parser::Located::new(
                            rref.type_location().clone(),
                            format!(
                                "{} is a data source and must be used with the `read` keyword:\n  let <name> = read {} {{ }}",
                                full_resource_type, full_resource_type,
                            ),
                        );
                        diagnostics.extend(render_merged(
                            doc,
                            &finding,
                            DiagnosticSeverity::ERROR,
                            finding.detail().clone(),
                        ));
                    }
                    for finding in rref.ignore_changes_findings(validated_schema.schema()) {
                        diagnostics.extend(render_merged(
                            doc,
                            &finding,
                            DiagnosticSeverity::ERROR,
                            format!("{}: {}", resource_id, finding.detail()),
                        ));
                    }
                }
                if let Some(validated_schema) = validated_schema {
                    let schema = validated_schema.as_schema();
                    // Build block_name -> canonical_name map for this schema
                    let bn_map = schema.block_name_map();
                    let resolved_attrs = semantic_resource_attributes
                        .as_ref()
                        .cloned()
                        .unwrap_or_else(|| rref.attributes().into_owned());
                    let (admission_resource, inputs) = select_admission_inputs(
                        rref,
                        authored_resources.as_ref(),
                        &resolved_attrs,
                        expression_report,
                    );
                    let knowledge_context =
                        carina_core::resource::knowledge::KnowledgeContext::validation();
                    let mut classification_error =
                        |name: &str,
                         error: carina_core::resource::knowledge::KnowledgeError<
                            carina_core::resource::RelativeBindingName,
                        >| {
                            let finding = carina_core::parser::Located::new(
                                rref.attribute_value_location(
                                    name,
                                    &carina_core::schema::FieldPath::new(),
                                )
                                .clone(),
                                error.to_string(),
                            );
                            diagnostics.extend(render_merged(
                                doc,
                                &finding,
                                DiagnosticSeverity::ERROR,
                                finding.detail().clone(),
                            ));
                        };
                    let mut classified_attrs =
                        inputs.classify(&knowledge_context, &mut classification_error);
                    // Reuse each authored classification. Fully known inputs
                    // already contain their value; only a newly evaluated
                    // candidate needs its own classification.
                    let evaluated_attrs = classified_attrs
                        .iter()
                        .filter_map(|(name, knowledge)| {
                            if matches!(
                                knowledge,
                                carina_core::resource::knowledge::Classification::Known(_)
                            ) {
                                return None;
                            }
                            carina_core::parser::evaluate_static_value_for_validation(
                                knowledge,
                                self.provider_context.catalog(),
                            )
                            .filter(|value| {
                                inputs.value(name).is_some_and(|source| source != value)
                            })
                            .map(|value| (name.clone(), value))
                        })
                        .collect::<IndexMap<_, _>>();
                    let mut evaluated_inputs = inputs.clone();
                    let (evaluated_inputs, updated) = evaluated_inputs.materialize_and_classify(
                        &evaluated_attrs,
                        &knowledge_context,
                        classification_error,
                    );
                    classified_attrs.extend(updated);
                    // Expansion supplies the binding authority for composition
                    // exports. Select that input before admitting attributes;
                    // the authored tree below supplies only rendering locations.
                    let uses_composition_output = rref.references().iter().any(|reference| {
                        pre_expansion_input.module_calls.iter().any(|call| {
                            call.binding_name.as_ref().is_some_and(|binding| {
                                reference.binding_matches(|key| key.matches(binding))
                            })
                        })
                    });
                    let mut value_validation = expanded_validation_inputs.as_mut()
                        .filter(|_| uses_composition_output)
                        .and_then(|inputs| {
                            let result = inputs.resource_admissions(resource_id, self.provider_context);
                            #[cfg(feature = "test-utils")]
                            if result.is_none() {
                                replay_coverage::declined_outcome(true);
                            }
                            result
                        })
                        .unwrap_or_else(|| {
                            #[cfg(feature = "test-utils")]
                            replay_coverage::reached(AdmissionPath::Relative);
                            carina_core::validation::admission::attributes::resource_attribute_admissions(
                                admission_resource, parsed, validated_schema, &classified_attrs, resource_quoted_string_attrs, evaluated_inputs, &binding_index, self.provider_context, &self.schemas, &mut authored_coverage,
                            )
                        });
                    // Attribute rendering consumes its report entries. Preserve
                    // core's diagnostic ownership before taking those entries.
                    let resource_diagnostic_errors = value_validation
                        .diagnostic_errors()
                        .map(|(error, _, coverage)| (error.clone(), coverage))
                        .collect::<Vec<_>>();
                    let mut reported_attribute_diagnostics = HashSet::new();
                    let retained_failures = resource_attributes
                        .keys()
                        .filter(|name| value_validation.is_retained_failure(name))
                        .map(String::as_str)
                        .collect::<HashSet<_>>();
                    for (attr_name, authored_attr_value) in resource_attributes.iter() {
                        let location = rref.attribute_value_location(
                            attr_name,
                            &carina_core::schema::FieldPath::new(),
                        );
                        let judged_expression = expression_report.expression(location);
                        let judged_expression = match judged_expression {
                            Ok(expression) => expression,
                            Err(error) => {
                                let finding =
                                    carina_core::parser::Located::new(location.clone(), error);
                                diagnostics.extend(render_merged(
                                    doc,
                                    &finding,
                                    DiagnosticSeverity::ERROR,
                                    finding.detail().to_string(),
                                ));
                                continue;
                            }
                        };
                        // Constructed results use the directory's evaluated
                        // values; the buffer copy cannot resolve sibling lets.
                        let attr_value = if contains_function_call(authored_attr_value) {
                            semantic_resource_attributes
                                .as_ref()
                                .filter(|_| {
                                    validated_schema
                                        .effective_attribute_by_authored_name(attr_name)
                                        .is_some_and(|attribute| {
                                            let sink =
                                                carina_core::validation::expression_sink_type(
                                                    attribute.field_type(),
                                                    validated_schema.owner(),
                                                    validated_schema.schema().as_schema().defs(),
                                                );
                                            let expression = judged_expression
                                                .for_sink(&sink, self.provider_context.catalog());
                                            expression.is_single_string_construction()
                                                || expression.has_constructed_members()
                                        })
                                })
                                .and_then(|attributes| attributes.get(attr_name))
                                .or_else(|| {
                                    resolved_resource_attributes
                                        .as_ref()
                                        .and_then(|attributes| attributes.get(attr_name))
                                })
                                .unwrap_or(authored_attr_value)
                        } else {
                            authored_attr_value
                        };
                        let typecheck_value = semantic_resource_attributes
                            .as_ref()
                            .and_then(|attributes| attributes.get(attr_name))
                            .unwrap_or(attr_value);
                        // Resolve block_name to canonical attribute name
                        let canonical_name = bn_map
                            .get(attr_name)
                            .map(|s| s.as_str())
                            .unwrap_or(attr_name);

                        // Check for mixed syntax: both block_name and canonical name present
                        // Skip when block_name == canonical name (singular names like "statement")
                        if let Some(canon) = bn_map.get(attr_name)
                            && canon != attr_name
                            && resource_attributes.contains_key(canon)
                        {
                            let finding = carina_core::parser::Located::new(
                                rref.attribute_key_location(attr_name).clone(),
                                format!(
                                    "Cannot use both '{}' and '{}' (they refer to the same attribute)",
                                    attr_name, canon,
                                ),
                            );
                            diagnostics.extend(render_merged(
                                doc,
                                &finding,
                                DiagnosticSeverity::ERROR,
                                finding.detail().clone(),
                            ));
                            continue;
                        }

                        // Core's resource validator owns unknown attributes and
                        // their spelling suggestions. Nothing else applies here.
                        if !schema.attributes().contains_key(canonical_name) {
                            continue;
                        }

                        // Type validation
                        if let Some(attr_schema) = schema.attributes().get(canonical_name) {
                            let effective_attribute = validated_schema
                                .effective_attribute(canonical_name)
                                .expect("validated schema attributes must have effective views");
                            let effective_type = effective_attribute.value_type();
                            // The shared resource-level validator below owns read-only
                            // assignments. Skip type and authoring guidance here so it
                            // emits the single writability diagnostic, matching core.
                            if effective_attribute.is_read_only() {
                                continue;
                            }

                            if binding_error_sinks
                                .contains(&(resource_id.clone(), attr_name.clone()))
                            {
                                continue;
                            }

                            let attr_findings = value_validation
                                .take_attribute(attr_name)
                                .unwrap_or_default();
                            // Admission owns the syntax verdict. Parser-owned
                            // block keys supply the established guidance ranges.
                            if matches!(
                                attr_findings.local_error(),
                                Some(TypeError::BlockSyntaxNotAllowed { .. })
                            ) {
                                let search_name = attr_schema.block_name().unwrap_or(attr_name);
                                let mut reported_block_syntax = false;
                                for location in rref.attribute_block_keys(attr_name) {
                                    reported_block_syntax = true;
                                    let finding = carina_core::parser::Located::new(
                                        location.clone(),
                                        format!(
                                            "'{}' cannot use block syntax; use map assignment: {} = {{ ... }}",
                                            search_name, search_name,
                                        ),
                                    );
                                    diagnostics.extend(render_merged(
                                        doc,
                                        &finding,
                                        DiagnosticSeverity::ERROR,
                                        finding.detail().clone(),
                                    ));
                                }
                                if reported_block_syntax {
                                    continue;
                                }
                            }

                            let assignment_value =
                                judged_expression.assignment_value(typecheck_value);
                            let symbolic = attr_findings.symbolic_findings().to_vec();
                            let has_exact_findings =
                                symbolic.iter().any(|finding| !finding.covers_path());
                            let mut handled_enum_diagnostic = false;
                            if matches!(
                                effective_type.shape(),
                                carina_core::schema::Shape::Enum { .. }
                            ) && let Some(diag) = build_enum_diagnostic(
                                attr_findings.local_error(),
                                attr_name,
                                resource_quoted_string_attrs.contains(attr_name),
                                doc,
                                rref,
                            ) {
                                diagnostics.push(diag);
                                handled_enum_diagnostic = true;
                                reported_attribute_diagnostics.insert(canonical_name.to_string());
                                // Continue past the generic match: this attribute
                                // is already reported with the richer diagnostic.
                                if !has_exact_findings {
                                    continue;
                                }
                            }
                            if !handled_enum_diagnostic
                                && let Some(error) = attr_findings.local_error()
                            {
                                let tagged = error.clone().with_attribute(attr_name);
                                if crate::code_action::enum_diagnostic_data_for_error(&tagged)
                                    .is_some()
                                {
                                    let finding = carina_core::parser::Located::new(
                                        rref.attribute_value_location(
                                            attr_name,
                                            &carina_core::schema::FieldPath::new(),
                                        )
                                        .clone(),
                                        tagged,
                                    );
                                    if let Some(diagnostic) = render_schema_diagnostic(
                                        doc,
                                        &finding,
                                        rref.replaceable_attribute_value(attr_name),
                                    ) {
                                        diagnostics.push(diagnostic);
                                        reported_attribute_diagnostics
                                            .insert(canonical_name.to_string());
                                    }
                                    handled_enum_diagnostic = true;
                                    if !has_exact_findings {
                                        continue;
                                    }
                                }
                            }
                            for finding in &symbolic {
                                let covers_path = finding.covers_path();
                                if handled_enum_diagnostic && covers_path {
                                    continue;
                                }
                                let (path, error) = finding.diagnostic();
                                use carina_core::resource::SecretDiagnosticError;
                                use carina_core::validation::admission::AdmissionReferenceError;
                                let (location, severity, message) = match error {
                                    TypeError::Reference { error, .. } => match error.as_ref() {
                                        SecretDiagnosticError::Public(
                                            AdmissionReferenceError::Arithmetic(finding),
                                        ) => (
                                            finding.location(),
                                            DiagnosticSeverity::ERROR,
                                            finding.detail().to_string(),
                                        ),
                                        SecretDiagnosticError::Public(
                                            AdmissionReferenceError::Computed(error),
                                        ) => (
                                            rref.attribute_key_location(attr_name),
                                            match error {
                                                DeferredAccessTypeError::InvalidAccess {
                                                    ..
                                                } => DiagnosticSeverity::ERROR,
                                                DeferredAccessTypeError::TypeMismatch {
                                                    ..
                                                } => DiagnosticSeverity::WARNING,
                                            },
                                            finding.diagnostic_message(),
                                        ),
                                        // The reference existence pass owns these same findings.
                                        SecretDiagnosticError::Public(
                                            AdmissionReferenceError::UnknownBinding { .. }
                                            | AdmissionReferenceError::Path(_)
                                            | AdmissionReferenceError::ProtectedBinding
                                            | AdmissionReferenceError::ProtectedAttribute,
                                        ) => continue,
                                        SecretDiagnosticError::Public(
                                            AdmissionReferenceError::Authored(finding),
                                        ) if finding.is_lookup_error() => continue,
                                        SecretDiagnosticError::Public(
                                            AdmissionReferenceError::ResourceHandle {
                                                reference,
                                                types,
                                            },
                                        ) => (
                                            rref.attribute_key_location(attr_name),
                                            DiagnosticSeverity::WARNING,
                                            format!("Type mismatch: {types} (from {reference})"),
                                        ),
                                        SecretDiagnosticError::Public(
                                            AdmissionReferenceError::Resolution(_)
                                            | AdmissionReferenceError::Authored(_),
                                        )
                                        | SecretDiagnosticError::InsideSecret => (
                                            rref.attribute_key_location(attr_name),
                                            DiagnosticSeverity::ERROR,
                                            error.to_string(),
                                        ),
                                    },
                                    TypeError::TypeMismatch { types } => {
                                        let message = validation::reference_mismatch_message(
                                            assignment_value,
                                            path,
                                            attr_name,
                                            types,
                                        );
                                        (
                                            rref.attribute_key_location(attr_name),
                                            DiagnosticSeverity::WARNING,
                                            message,
                                        )
                                    }
                                    _ => (
                                        rref.attribute_key_location(attr_name),
                                        DiagnosticSeverity::WARNING,
                                        error.to_string(),
                                    ),
                                };
                                let finding =
                                    carina_core::parser::Located::new(location.clone(), message);
                                if let Some(diagnostic) =
                                    render_merged(doc, &finding, severity, finding.detail().clone())
                                {
                                    if !diagnostics.iter().any(|existing| {
                                        existing.range == diagnostic.range
                                            && existing.message == diagnostic.message
                                    }) {
                                        diagnostics.push(diagnostic);
                                    }
                                    if covers_path {
                                        reported_attribute_diagnostics
                                            .insert(canonical_name.to_string());
                                    }
                                }
                            }

                            if handled_enum_diagnostic {
                                continue;
                            }

                            let container_findings = attr_findings
                                .local_findings()
                                .iter()
                                .any(|(path, _)| !path.is_empty())
                                || matches!(
                                    attr_findings.structure(),
                                    carina_core::schema::ValueStructure::List
                                        | carina_core::schema::ValueStructure::Map
                                )
                                || matches!(
                                    attr_findings.local_error(),
                                    Some(
                                        TypeError::ListItemError { .. }
                                            | TypeError::MapValueError { .. }
                                            | TypeError::StructFieldError { .. }
                                    )
                                );
                            if container_findings {
                                let findings = attr_findings
                                    .into_local_findings()
                                    .into_iter()
                                    .filter(|(path, error)| {
                                        (!path.is_empty()
                                            || !contains_function_call(authored_attr_value))
                                            && !validation::covered_by_symbolic_findings(
                                                path, error, &symbolic,
                                            )
                                    })
                                    .collect();
                                let (nested, reported_duplicate) =
                                    self.render_container_findings(doc, rref, attr_name, findings);
                                if reported_duplicate {
                                    reported_attribute_diagnostics
                                        .insert(canonical_name.to_string());
                                }
                                diagnostics.extend(nested);
                            } else if !symbolic.iter().any(|finding| finding.covers_path()) {
                                let error = attr_findings.local_error();
                                let message = match error {
                                    Some(TypeError::TypeMismatch { types }) => {
                                        match (types.expected(), assignment_value) {
                                            ("Bool", Value::Concrete(ConcreteValue::String(_))) => {
                                                Some(format!(
                                                    "Type mismatch: expected Bool, got {}. Use true or false.",
                                                    carina_core::ValueDescription::of(attr_value)
                                                ))
                                            }
                                            ("Int", Value::Concrete(ConcreteValue::String(_))) => {
                                                Some(format!(
                                                    "Type mismatch: expected Int, got {}.",
                                                    carina_core::ValueDescription::of(attr_value)
                                                ))
                                            }
                                            (
                                                "Float",
                                                Value::Concrete(ConcreteValue::String(_)),
                                            ) => Some(format!(
                                                "Type mismatch: expected Float, got {}.",
                                                carina_core::ValueDescription::of(attr_value)
                                            )),
                                            _ => error
                                                .filter(|_| {
                                                    !contains_function_call(authored_attr_value)
                                                })
                                                .map(ToString::to_string),
                                        }
                                    }
                                    Some(
                                        TypeError::InvalidEnumVariant { .. }
                                        | TypeError::BindingShadowsEnumMember(_)
                                        | TypeError::UnionStructMismatch { .. }
                                        | TypeError::UnionAlternativesMismatch { .. }
                                        | TypeError::BlockSyntaxNotAllowed { .. },
                                    ) => error
                                        .filter(|_| !contains_function_call(authored_attr_value))
                                        .map(|error| {
                                            error.clone().with_attribute(attr_name).to_string()
                                        }),
                                    _ => None,
                                };
                                if let Some(message) = message {
                                    let finding = carina_core::parser::Located::new(
                                        rref.attribute_key_location(attr_name).clone(),
                                        message,
                                    );
                                    if let Some(diagnostic) = render_merged(
                                        doc,
                                        &finding,
                                        DiagnosticSeverity::WARNING,
                                        finding.detail().clone(),
                                    ) {
                                        diagnostics.push(diagnostic);
                                        reported_attribute_diagnostics
                                            .insert(canonical_name.to_string());
                                    }
                                }
                            }
                        }
                    }

                    // Render the report produced by the single owner-bound walk.
                    {
                        for (error, symbolic_coverage) in resource_diagnostic_errors {
                            if error
                                .attribute_name()
                                .is_some_and(|name| retained_failures.contains(name))
                            {
                                continue;
                            }
                            // Existence checks render lookup failures at their
                            // reference tokens, even when admission emits none.
                            if let TypeError::Reference { error, .. } = &error
                                && let carina_core::resource::SecretDiagnosticError::Public(error) = error.as_ref()
                                && match error {
                                    carina_core::validation::admission::AdmissionReferenceError::UnknownBinding { .. }
                                    | carina_core::validation::admission::AdmissionReferenceError::Path(_)
                                    | carina_core::validation::admission::AdmissionReferenceError::ProtectedBinding
                                    | carina_core::validation::admission::AdmissionReferenceError::ProtectedAttribute => true,
                                    carina_core::validation::admission::AdmissionReferenceError::Authored(finding) => finding.is_lookup_error(),
                                    _ => false,
                                }
                            {
                                continue;
                            }
                            if let carina_core::schema::TypeError::MissingRequired { name } = &error
                                && pending_resource_sinks.iter().any(
                                    |(pending_resource, authored_name)| {
                                        pending_resource == resource_id
                                            && bn_map
                                                .get(authored_name)
                                                .map(String::as_str)
                                                .unwrap_or(authored_name)
                                                == name
                                    },
                                )
                            {
                                continue;
                            }
                            if !provider_validation_deduplicator.should_emit_type_error(&error) {
                                continue;
                            }
                            // Skip errors that are already reported with precise positions
                            // by the attribute-level checks above, including intrinsic secrets.
                            if matches!(
                                &error,
                                carina_core::schema::TypeError::FieldGroupViolation { path, .. }
                                    if !path.is_empty()
                            ) {
                                continue;
                            }
                            // Statically evaluated expressions may first become known
                            // in this pass. Suppress list refinements only when
                            // the attribute pass emitted them.
                            if is_duplicate_list_error(&error)
                                && error.attribute_name().is_some_and(|attribute| {
                                    let canonical = bn_map
                                        .get(attribute)
                                        .map(String::as_str)
                                        .unwrap_or(attribute);
                                    reported_attribute_diagnostics.contains(canonical)
                                })
                            {
                                continue;
                            }
                            if !is_duplicate_list_error(&error)
                                && matches!(
                                    &error,
                                    TypeError::BlockSyntaxNotAllowed { .. }
                                        | TypeError::TypeMismatch { .. }
                                        | TypeError::BindingShadowsEnumMember(_)
                                        | TypeError::InvalidEnumVariant { .. }
                                        | TypeError::UnknownStructField { .. }
                                        | TypeError::UnionStructMismatch { .. }
                                        | TypeError::UnionAlternativesMismatch { .. }
                                        | TypeError::StructFieldError { .. }
                                        | TypeError::ListItemError { .. }
                                        | TypeError::MapValueError { .. }
                                )
                            {
                                continue;
                            }
                            // The binding-use authority checks the resolved value,
                            // including its destination member constraints. Its
                            // diagnostic owns this sink even when the schema pass
                            // sees a concrete local error instead of a reference.
                            if error.attribute_name().is_some_and(|name| {
                                binding_error_sinks
                                    .contains(&(resource_id.clone(), name.to_string()))
                            }) {
                                continue;
                            }
                            if symbolic_coverage.owns_finding()
                                || error.attribute_name().is_some_and(|name| {
                                    reported_attribute_diagnostics.contains(name)
                                })
                            {
                                continue;
                            }
                            let location = error
                                .attribute_name()
                                .filter(|name| resource_attributes.contains_key(*name))
                                .map_or(rref.type_location(), |name| {
                                    let assignment = rref.attribute_assignment_location(name);
                                    let span = assignment.span();
                                    if matches!(
                                        error,
                                        carina_core::schema::TypeError::UnknownAttribute { .. }
                                    ) && span.start_line() == span.end_line()
                                    {
                                        assignment
                                    } else {
                                        // Multiline containers retain the precise key instead
                                        // of widening beyond main's single-line anchor.
                                        rref.attribute_key_location(name)
                                    }
                                });
                            let finding =
                                carina_core::parser::Located::new(location.clone(), error);
                            diagnostics.extend(render_merged(
                                doc,
                                &finding,
                                DiagnosticSeverity::ERROR,
                                finding.detail().to_string(),
                            ));
                        }
                    }

                    // Lint: prefer block syntax for List<Struct> attributes
                    diagnostics.extend(self.check_list_struct_syntax(
                        doc,
                        rref,
                        validated_schema.schema(),
                    ));
                }
            }

            // Check the current buffer's attributes against directory-wide
            // binding authority while keeping diagnostics owned by this file.
            diagnostics.extend(self.check_attributes_blocks(
                doc,
                (parsed, tree),
                &semantic_view,
                &binding_index,
                &known_bindings,
                &inference_bindings,
            ));

            diagnostics.extend(self.check_exports_blocks(
                doc,
                parsed,
                tree,
                &semantic_view,
                &export_value_bindings,
                expanded_graph,
            ));

            if let (Some(configuration), Some(tree)) =
                (expanded_configuration.as_ref(), expanded_analysis.as_ref())
            {
                diagnostics.extend(self.check_inferred_export_types(
                    doc,
                    configuration.judged(),
                    tree,
                ));
            } else if merged
                .or(if base_path.is_none() {
                    Some(parsed)
                } else {
                    None
                })
                .is_some()
            {
                diagnostics.extend(self.check_inferred_export_types(
                    doc,
                    pre_expansion_configuration.judged(),
                    preferred_tree,
                ));
            }

            // Unused `let` detection. When a directory parse is available,
            // run the core check against its authored view (so references
            // from sibling files count as usage) and filter the result to bindings
            // declared in the current file — the LSP warning surface is
            // per-document. With no base path, parse the current buffer through
            // the same authored-view constructor. A failed directory parse must
            // not fall back to that buffer-only view because it cannot see uses
            // from sibling files.
            let unused_binding_names: Vec<String> = match (merged_result.as_ref(), base_path) {
                (Some(result), _) => {
                    let current_file_bindings: HashSet<String> = parsed
                        .iter_all_resources()
                        .filter_map(|rref| rref.binding().map(str::to_string))
                        .collect();
                    carina_core::validation::check_unused_bindings_with_identity_requirements(
                        &result.directory.authored_bindings,
                        &self.schemas,
                    )
                    .into_iter()
                    .filter(|b| current_file_bindings.contains(b))
                    .collect()
                }
                (None, None) => carina_core::config_loader::AuthoredBindingView::parse_file_source(
                    &text,
                    current_file_path.unwrap_or_else(|| doc.source_file()),
                    self.provider_context.catalog(),
                )
                .map(|view| {
                    carina_core::validation::check_unused_bindings_with_identity_requirements(
                        &view,
                        &self.schemas,
                    )
                })
                .unwrap_or_default(),
                (None, Some(_)) => Vec::new(),
            };
            diagnostics.extend(self.unused_binding_diagnostics(
                doc,
                preferred_tree,
                unused_binding_names,
            ));

            // Surface unused-for-binding warnings (recorded in parsed.warnings by
            // the parser) as LSP diagnostics. Other ParseWarnings (e.g. the
            // upstream_state "validate does not inspect" note) are informational
            // context meant for CLI output only and are not elevated here.
            diagnostics.extend(self.check_unused_for_bindings(doc));
            diagnostics.extend(self.check_single_quoted_interpolation_warnings(doc));

            // Check for unknown attributes on resource references (typo detection)
            diagnostics.extend(self.check_resource_ref_attributes(
                doc,
                preferred_tree,
                &binding_index,
            ));
        }

        // Check for duplicate attribute keys by parsing this document's source.
        // Invalid source yields no duplicate warnings; its parse error is reported above.
        diagnostics.extend(self.check_duplicate_attrs(doc));

        // Check for direct calls to pipe-preferred functions
        diagnostics.extend(self.check_pipe_preferred_direct_calls(doc));

        // Check for non-snake_case binding names
        diagnostics.extend(self.check_non_snake_case_bindings(doc));

        diagnostics
    }
}

impl DiagnosticEngine {
    /// Construct diagnostics backed by one loaded-provider snapshot.
    pub fn new(loaded_providers: Arc<LoadedProviderSnapshot>, provider_names: Vec<String>) -> Self {
        Self::new_with_known_providers(loaded_providers, provider_names, HashSet::new())
    }

    /// Construct diagnostics for the orphaned-file fallback with no loaded
    /// providers. This path deliberately cannot mint provider-schema inference
    /// evidence.
    pub fn without_loaded_provider_schemas(environment: carina_core::EnvironmentSnapshot) -> Self {
        let schemas = Arc::new(SchemaRegistry::new());
        Self {
            schemas: Arc::clone(&schemas),
            loaded_providers: None,
            directory_providers: None,
            provider_names: Vec::new(),
            known_provider_names: HashSet::new(),
            factories: Arc::new(Vec::new()),
            configuration_factories: Vec::new(),
            provider_errors: HashMap::new(),
            provider_context: DiagnosticProviderContext::Unloaded(Arc::new(
                carina_core::parser::ProviderContext::new(environment),
            )),
        }
    }

    pub(crate) fn new_with_known_providers(
        loaded_providers: Arc<LoadedProviderSnapshot>,
        provider_names: Vec<String>,
        known_provider_names: HashSet<String>,
    ) -> Self {
        let schemas = loaded_providers.schemas().schemas_arc();
        let factories = loaded_providers.factories_arc();
        let provider_context =
            DiagnosticProviderContext::Loaded(loaded_providers.provider_context().clone());
        Self {
            schemas,
            loaded_providers: Some(loaded_providers),
            directory_providers: None,
            provider_names,
            known_provider_names,
            factories,
            configuration_factories: Vec::new(),
            provider_errors: HashMap::new(),
            provider_context,
        }
    }

    pub fn schema_count(&self) -> usize {
        self.schemas.len()
    }

    /// Host-owned resources retain their catalog; configuration still needs
    /// the native factory's declaration and semantic admission hook.
    pub(crate) fn with_configuration_factory(mut self, factory: Box<dyn ProviderFactory>) -> Self {
        self.configuration_factories.push(factory);
        self
    }

    pub fn with_provider_errors(mut self, errors: HashMap<String, String>) -> Self {
        self.provider_errors = errors;
        self
    }

    pub(crate) fn with_directory_providers(
        mut self,
        directory: PathIdentity,
        providers: Vec<ProviderConfig>,
    ) -> Self {
        self.directory_providers = Some(DirectoryProviderDeclarations {
            directory,
            providers,
        });
        self
    }

    #[cfg(test)]
    fn with_run_for_test<T>(&self, produce: impl FnOnce(&DiagnosticRun<'_>) -> T) -> T {
        carina_core::validation::deferred_types::with_session_for_test(
            &self.provider_context,
            |session| {
                produce(&DiagnosticRun {
                    engine: self,
                    provider_context: session,
                })
            },
        )
    }

    /// Analyze an open document in its directory context.
    ///
    /// `current_file_path` is the document's full path. It is the sole source
    /// of file identity; the directory and basename used for merged parsing
    /// are derived from it rather than inferred from buffer contents.
    pub fn analyze(
        &self,
        raw: &crate::document::Document,
        current_file_path: Option<&Path>,
    ) -> Vec<Diagnostic> {
        let doc = match raw.for_diagnostics() {
            Ok(doc) => doc,
            Err(protected) => return vec![buffer::protected_parse_error(protected)],
        };
        let context = self.catalog_for_public_document(&doc, current_file_path);
        let reparsed;
        let doc = if doc.provider_context().custom_types_loaded() && !context.custom_types_loaded()
        {
            reparsed = crate::document::Document::new_with_path(
                doc.text(),
                doc.source_file().to_path_buf(),
                Arc::new(context.clone()),
            );
            match reparsed.for_diagnostics() {
                Ok(doc) => doc,
                Err(protected) => return vec![buffer::protected_parse_error(protected)],
            }
        } else {
            doc
        };
        let terminal =
            carina_core::validation::deferred_types::TypeCheckSession::run(&context, |session| {
                DiagnosticRun {
                    engine: self,
                    provider_context: session,
                }
                .collect_diagnostics(&doc, current_file_path)
            });
        Self::publish_type_diagnostics(terminal, &doc)
    }

    pub(crate) fn catalog_for_document(
        &self,
        doc: &crate::document::Document,
        current_file_path: Option<&Path>,
    ) -> carina_core::parser::ProviderContext {
        match doc.for_diagnostics() {
            Ok(doc) => self.catalog_for_public_document(&doc, current_file_path),
            Err(_) => carina_core::parser::ProviderContext::new(
                doc.provider_context().environment.clone(),
            ),
        }
    }

    fn catalog_for_public_document(
        &self,
        doc: &Document,
        current_file_path: Option<&Path>,
    ) -> carina_core::parser::ProviderContext {
        let mut context = match current_file_path.and_then(Path::parent) {
            Some(directory) => carina_core::paths::ResolvedPath::new(directory)
                .ok()
                .and_then(|directory| self.directory_catalog(doc, &directory))
                .unwrap_or_else(|| {
                    if let Some(scope) = &self.directory_providers
                        && scope.directory == PathIdentity::new(directory)
                    {
                        // A resource edit may break declaration parsing without
                        // invalidating this directory's loaded provider snapshot.
                        self.catalog_for_provider_declarations(&scope.providers)
                    } else {
                        carina_core::parser::ProviderContext::new(
                            doc.provider_context().environment.clone(),
                        )
                    }
                }),
            None => (*self.provider_context).clone(),
        };
        context.environment = doc.provider_context().environment.clone();
        context
    }

    fn publish_type_diagnostics(
        terminal: carina_core::validation::deferred_types::ProviderTypeCheckResult<Vec<Diagnostic>>,
        doc: &Document,
    ) -> Vec<Diagnostic> {
        let mut sink = LspTypeCheckSink {
            doc,
            diagnostics: Vec::new(),
        };
        let mut diagnostics = terminal.publish(&mut sink);
        diagnostics.extend(sink.diagnostics);
        diagnostics
    }

    /// Successful directory discovery establishes a loaded catalog, including
    /// when no providers are declared. Only failed owners make it partial.
    fn directory_catalog(
        &self,
        doc: &Document,
        directory: &carina_core::paths::ResolvedPath,
    ) -> Option<carina_core::parser::ProviderContext> {
        let files = carina_core::config_loader::find_crn_files_in_dir(directory).ok()?;
        let mut providers = Vec::new();
        let mut has_buffer = false;
        let buffer_identity = carina_core::paths::ResolvedPath::new(doc.source_file())
            .ok()
            .map(|path| path.identity());
        for file in files {
            let source = if Some(file.identity()) == buffer_identity {
                has_buffer = true;
                doc.text()
            } else {
                file.read_to_string().ok()?
            };
            providers.extend(
                carina_core::parser::parse_provider_factory_declarations(
                    &source,
                    &file.source_label(),
                    doc.provider_context(),
                )
                .ok()?,
            );
        }
        if !has_buffer {
            providers.extend(
                carina_core::parser::parse_provider_factory_declarations(
                    &doc.text(),
                    doc.source_file(),
                    doc.provider_context(),
                )
                .ok()?,
            );
        }
        Some(self.catalog_for_provider_declarations(&providers))
    }

    /// Apply the same default-provider failure policy to freshly parsed
    /// declarations and the declarations retained with an exact-directory state.
    fn catalog_for_provider_declarations<P: carina_core::resource::PlaceholderKind>(
        &self,
        providers: &[ProviderConfig<P>],
    ) -> carina_core::parser::ProviderContext {
        let mut loaded = self
            .provider_names
            .iter()
            .cloned()
            .chain(
                self.factories
                    .iter()
                    .map(|factory| factory.name().to_string()),
            )
            .chain(
                self.schemas
                    .iter()
                    .map(|(provider, _, _, _)| provider.to_string()),
            )
            .collect::<HashSet<_>>();
        if providers.iter().any(|provider| {
            provider.name == carina_provider_mock::BUILTIN_PROVIDER_NAME
                && provider.source.is_none()
        }) {
            loaded.insert(carina_provider_mock::BUILTIN_PROVIDER_NAME.to_string());
        }
        let failed = carina_core::parser::unavailable_provider_kinds(
            providers,
            &loaded,
            &self.provider_errors,
        );
        let mut catalog = (*self.provider_context).clone();
        let loaded_catalog = self
            .loaded_providers
            .as_ref()
            .map(|snapshot| snapshot.provider_context().custom_types.clone())
            .unwrap_or_else(|| {
                carina_core::provider::provider_custom_types_from_factories(Arc::new(Vec::new()))
            });
        catalog.custom_types = loaded_catalog.with_failed_providers(failed);
        catalog
    }
}

/// Build a `Diagnostic` for a failed direct `Enum` attribute validation,
/// carrying the structured candidate list on `Diagnostic.data` and a
/// range over the offending value text. Returns `None` when validation
/// passed, when the value range can't be located (block-syntax,
/// missing value), or when the failing error isn't an enum-variant
/// shape (e.g. a `TypeMismatch` from a non-string `Value`).
///
/// See #2309. The structured payload is consumed by
/// [`crate::code_action::code_actions_for_diagnostic`].
fn build_enum_diagnostic(
    error: Option<&TypeError>,
    attr_name: &str,
    is_quoted_literal: bool,
    doc: &Document,
    resource: &carina_core::parser::AnalysisResourceRef<'_>,
) -> Option<Diagnostic> {
    let tagged = error?.clone().with_attribute(attr_name);
    // Reshape into the shape-mismatch diagnostic when the value came
    // from a quoted string literal — mirrors CLI behavior (#2094).
    let reshaped = if is_quoted_literal {
        tagged.into_string_literal_diagnostic()
    } else {
        tagged
    };
    // Non-enum shape errors keep the generic type-validation wording.
    crate::code_action::enum_diagnostic_data_for_error(&reshaped)?;
    let finding = carina_core::parser::Located::new(
        resource
            .attribute_value_location(attr_name, &carina_core::schema::FieldPath::new())
            .clone(),
        reshaped,
    );
    render_schema_diagnostic(
        doc,
        &finding,
        resource.replaceable_attribute_value(attr_name),
    )
}

fn is_duplicate_list_error(error: &TypeError) -> bool {
    match error {
        TypeError::DuplicateListElement {
            value: _,
            index: _,
            first_index: _,
            attribute: _,
        } => true,
        TypeError::ListItemError { inner, index: _ }
        | TypeError::MapKeyError { inner, key: _ }
        | TypeError::MapValueError { inner, key: _ }
        | TypeError::StructFieldError { inner, field: _ } => is_duplicate_list_error(inner),
        TypeError::TypeMismatch { types: _ }
        | TypeError::BindingShadowsEnumMember(_)
        | TypeError::InvalidEnumVariant {
            value: _,
            value_kind: _,
            attribute: _,
            type_name: _,
            expected: _,
        }
        | TypeError::PatternMismatch {
            value: _,
            pattern: _,
            attribute: _,
            type_name: _,
        }
        | TypeError::LengthOutOfRange {
            value: _,
            length: _,
            min: _,
            max: _,
            attribute: _,
            type_name: _,
        }
        | TypeError::ValueOutOfRange {
            value: _,
            bounds: _,
            attribute: _,
            type_name: _,
        }
        | TypeError::StringLiteralExpectedEnum {
            user_typed: _,
            attribute: _,
            type_name: _,
            expected: _,
            extra_message: _,
        }
        | TypeError::ValidationFailed { message: _ }
        | TypeError::Knowledge {
            error: _,
            attribute: _,
        }
        | TypeError::Reference {
            error: _,
            attribute: _,
        }
        | TypeError::CustomTypeValidation {
            error: _,
            attribute: _,
        }
        | TypeError::ResourceValidationFailed {
            message: _,
            attribute: _,
        }
        | TypeError::MissingRequired { name: _ }
        | TypeError::FieldGroupViolation { path: _, group: _ }
        | TypeError::ReadOnlyAttribute { name: _ }
        | TypeError::UnknownAttribute {
            name: _,
            suggestion: _,
        }
        | TypeError::UnknownStructField {
            struct_name: _,
            field: _,
            suggestion: _,
        }
        | TypeError::UnionStructMismatch {
            reason: _,
            supplied_fields: _,
            alternatives: _,
        }
        | TypeError::UnionAlternativesMismatch { failures: _ }
        | TypeError::BlockSyntaxNotAllowed { attribute: _ } => false,
    }
}

/// Structured schema data is decoration on a location-rendered finding; it
/// never selects the range. Only replaceable parser tokens get quick fixes.
fn render_schema_diagnostic(
    doc: &Document,
    finding: &carina_core::parser::Located<TypeError>,
    replaceable: Option<&carina_core::parser::DeclarationLocation>,
) -> Option<Diagnostic> {
    let severity = if is_duplicate_list_error(finding.detail()) {
        DiagnosticSeverity::ERROR
    } else {
        DiagnosticSeverity::WARNING
    };
    let mut diagnostic = render_merged(doc, finding, severity, finding.detail().to_string())?;
    if replaceable == Some(finding.location())
        && let Some(data) = crate::code_action::enum_diagnostic_data_for_error(finding.detail())
    {
        diagnostic.data = Some(serde_json::to_value(data).expect("EnumDiagnosticData serialize"));
    }
    Some(diagnostic)
}

fn decorate_schema_diagnostic(diagnostic: &mut Diagnostic, error: &carina_core::schema::TypeError) {
    if let Some(data) = crate::code_action::enum_diagnostic_data_for_error(error) {
        diagnostic.data = Some(serde_json::to_value(data).expect("EnumDiagnosticData serialize"));
    }
}
