//! Provider-dependent evidence recorded by producers and consumed at a terminal boundary.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::parser::{
    ParseWarning, ParseWarningSpan, ProviderContext, SourceSpan, TypeExpr, WarningKind,
};

/// Authored ownership of a check, retained independently of evaluated values.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProviderTypeCheckSite {
    source: PathBuf,
    line: usize,
    column: usize,
    end_line: usize,
    end_column: usize,
    context: String,
}

impl ProviderTypeCheckSite {
    pub fn new(source: &Path, span: SourceSpan, context: impl Into<String>) -> Self {
        Self {
            source: crate::utils::normalize_source_path(source),
            line: span.start_line(),
            column: span.start_column(),
            end_line: span.end_line(),
            end_column: span.end_column(),
            context: context.into(),
        }
    }

    pub fn at_line(source: &Path, line: usize, column: usize, context: impl Into<String>) -> Self {
        Self {
            source: crate::utils::normalize_source_path(source),
            line: line.max(1),
            column: column.max(1),
            end_line: line.max(1),
            end_column: column.max(1) + 1,
            context: context.into(),
        }
    }

    fn unlocated(context: impl Into<String>) -> Self {
        Self::at_line(Path::new("<config>"), 1, 1, context)
    }
}

/// A sink of unresolved checks. It has no conversion to success or free clearing API.
/// The only publication path consumes it in [`ProviderTypeCheckResult::settle`].
#[derive(Debug, Default)]
pub struct DeferredProviderTypeChecks {
    pending: BTreeMap<(ProviderTypeCheckSite, String), TypeExpr>,
    declarations: BTreeMap<String, Vec<ProviderTypeCheckSite>>,
    queries: BTreeMap<String, (TypeExpr, String)>,
}

impl DeferredProviderTypeChecks {
    pub fn record(&mut self, site: ProviderTypeCheckSite, expected: &TypeExpr) {
        self.pending
            .insert((site, expected.to_string()), expected.clone());
    }
}

/// Shared recording within a single catalog. Taking evidence transfers it by value;
/// it cannot convert a deferral into an unrecorded successful check.
#[derive(Debug, Default)]
pub struct ProviderTypeCheckRecorder {
    evidence: Mutex<DeferredProviderTypeChecks>,
}

impl ProviderTypeCheckRecorder {
    /// Register provenance during normal declaration resolution, never by a second AST walk.
    pub fn declaration(&self, expected: &TypeExpr, site: ProviderTypeCheckSite) {
        let mut evidence = self.evidence.lock().expect("provider type evidence lock");
        let sites = evidence
            .declarations
            .entry(expected.to_string())
            .or_default();
        if !sites.contains(&site) {
            sites.push(site);
        }
    }

    pub fn record(&self, expected: &TypeExpr, site: ProviderTypeCheckSite) {
        self.evidence
            .lock()
            .expect("provider type evidence lock")
            .record(site, expected);
    }

    /// Queries without an expression span retain the declaration's provenance.
    /// If no declaration owns the evidence, the result records that fact explicitly.
    pub fn record_type(&self, expected: &TypeExpr, context: &str) {
        let mut evidence = self.evidence.lock().expect("provider type evidence lock");
        evidence.queries.insert(
            expected.to_string(),
            (expected.clone(), context.to_string()),
        );
    }

    pub fn take(&self) -> DeferredProviderTypeChecks {
        let mut evidence = self.evidence.lock().expect("provider type evidence lock");
        std::mem::take(&mut *evidence)
    }
}

/// A terminal output can only be constructed by consuming its unsettled evidence.
/// No `Default`, public fields, or constructor without the sink is provided.
///
/// ```compile_fail
/// use carina_core::validation::deferred_types::ProviderTypeCheckResult;
/// let result = ProviderTypeCheckResult { output: Vec::<String>::new(), diagnostics: vec![] };
/// ```
/// ```compile_fail
/// use carina_core::validation::deferred_types::ProviderTypeCheckResult;
/// use carina_core::parser::ProviderContext;
/// let result = ProviderTypeCheckResult::settle(ProviderContext::unloaded(), Vec::<String>::new());
/// ```
pub struct ProviderTypeCheckResult<T> {
    output: T,
    diagnostics: Vec<ProviderTypeCheckDiagnostic>,
}

#[derive(Debug)]
pub enum ProviderTypeCheckDiagnostic {
    Deferred(ParseWarning),
    UnsettledLoadedCheck(ParseWarning),
}

impl<T> ProviderTypeCheckResult<T> {
    /// The sole constructor of terminal provider-type results.
    pub fn settle(
        mut checks: DeferredProviderTypeChecks,
        catalog: &ProviderContext,
        output: T,
    ) -> Self {
        // Queries retain their evidence until the same sink can attach declaration
        // provenance. Settlement joins recorded data; it never re-walks an AST.
        for (label, (expected, context)) in std::mem::take(&mut checks.queries) {
            let sites = checks
                .declarations
                .get(&label)
                .cloned()
                .unwrap_or_else(|| vec![ProviderTypeCheckSite::unlocated(context)]);
            for site in sites {
                checks.record(site, &expected);
            }
        }
        let diagnostics = checks.pending.into_iter().map(|((site, _label), expected)| {
            let loaded = catalog.custom_types_loaded();
            let message = if loaded {
                format!("internal error: {}: type check for {expected} remained deferred in a fully loaded provider catalog", site.context)
            } else {
                format!("{}: type check for {expected} was deferred because provider schemas were not loaded", site.context)
            };
            let warning = ParseWarning {
                kind: WarningKind::ProviderSchemasNotLoaded,
                file: Some(site.source.display().to_string()), line: site.line,
                span: Some(ParseWarningSpan { start_line: site.line, start_column: site.column,
                    end_line: site.end_line, end_column: site.end_column }), message,
            };
            if loaded { ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(warning) }
            else { ProviderTypeCheckDiagnostic::Deferred(warning) }
        }).collect();
        Self {
            output,
            diagnostics,
        }
    }

    pub fn output(&self) -> &T {
        &self.output
    }
    pub fn diagnostics(&self) -> &[ProviderTypeCheckDiagnostic] {
        &self.diagnostics
    }
    pub fn into_parts(self) -> (T, Vec<ProviderTypeCheckDiagnostic>) {
        (self.output, self.diagnostics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settlement_value_producer_retains_all_provider_leaves() {
        let context = ProviderContext::default();
        let expected = TypeExpr::Struct {
            fields: vec![
                (
                    "first".into(),
                    crate::parser::parse_type_expr_str("FirstId", &context).unwrap(),
                ),
                (
                    "second".into(),
                    crate::parser::parse_type_expr_str("SecondId", &context).unwrap(),
                ),
            ],
        };
        let value = crate::resource::AuthoredValue::Concrete(
            crate::resource::AuthoredConcreteValue::Map(indexmap::IndexMap::from([
                (
                    "first".into(),
                    crate::resource::AuthoredValue::Concrete(
                        crate::resource::AuthoredConcreteValue::String("a".into()),
                    ),
                ),
                (
                    "second".into(),
                    crate::resource::AuthoredValue::Concrete(
                        crate::resource::AuthoredConcreteValue::String("b".into()),
                    ),
                ),
            ])),
        );
        super::super::typecheck::check_type_expr_value_providerless(
            &expected,
            &value,
            &context,
            super::super::typecheck::TypecheckInference::Unavailable,
        )
        .record_at(
            context.type_checks(),
            ProviderTypeCheckSite::at_line(Path::new("/project/main.crn"), 1, 1, "argument 'ids'"),
        )
        .unwrap();
        let result = ProviderTypeCheckResult::settle(context.type_checks().take(), &context, ());
        assert_eq!(result.diagnostics().len(), 2, "{:?}", result.diagnostics());
    }

    #[test]
    fn loaded_settlement_rejects_artificially_pending_check() {
        let context = crate::parser::LoadedProviderContext::empty_for_test();
        let mut sink = DeferredProviderTypeChecks::default();
        sink.record(
            ProviderTypeCheckSite::at_line(Path::new("/project/main.crn"), 3, 7, "attribute 'x'"),
            &crate::parser::parse_type_expr_str("Typo", &ProviderContext::default()).unwrap(),
        );
        let result = ProviderTypeCheckResult::settle(sink, &context, ());
        assert!(
            matches!(result.diagnostics(), [ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(error)] if error.message.contains("fully loaded"))
        );
    }

    #[test]
    fn sink_deduplicates_authored_sites_without_an_ast_rewalk() {
        let mut sink = DeferredProviderTypeChecks::default();
        let site =
            ProviderTypeCheckSite::at_line(Path::new("/project/main.crn"), 3, 7, "attribute 'x'");
        let expected =
            crate::parser::parse_type_expr_str("Arn", &ProviderContext::default()).unwrap();
        sink.record(site.clone(), &expected);
        sink.record(site, &expected);
        let result = ProviderTypeCheckResult::settle(sink, &ProviderContext::default(), ());
        assert_eq!(result.diagnostics().len(), 1);
    }
}

#[cfg(test)]
mod partial_catalog_tests {
    use super::*;
    use crate::parser::{OwnedTypeIdentity, ProviderCustomTypes, parse_file};
    use crate::schema::{RefinedScalarKind, TypeIdentity};

    fn partial() -> ProviderContext {
        let loaded = ProviderCustomTypes::loaded(
            std::collections::HashSet::from([
                OwnedTypeIdentity::provider(
                    "ready",
                    TypeIdentity::bare("ReadyId"),
                    RefinedScalarKind::String,
                ),
                OwnedTypeIdentity::provider(
                    "ready",
                    TypeIdentity::from_dotted("ready.Value.Id"),
                    RefinedScalarKind::String,
                ),
            ]),
            Box::new(|_, _| Ok(())),
        );
        ProviderContext {
            custom_types: loaded
                .with_failed_providers(std::collections::BTreeSet::from(["broken".into()])),
            ..Default::default()
        }
    }

    #[test]
    fn settlement_partial_catalog_loaded_owner_cannot_defer() {
        let catalog = ProviderCustomTypes::loaded(
            std::collections::HashSet::from([OwnedTypeIdentity::provider(
                "ready",
                TypeIdentity::bare("ReadyId"),
                RefinedScalarKind::String,
            )]),
            Box::new(|_, _| Err(crate::parser::CustomTypeValidationError::ProvidersNotLoaded)),
        );
        let context = ProviderContext {
            custom_types: catalog
                .with_failed_providers(std::collections::BTreeSet::from(["broken".into()])),
            ..Default::default()
        };
        let mut parsed = parse_file(
            "attributes { x: ReadyId = 'ok' }",
            Path::new("/project/main.crn"),
            &ProviderContext::default(),
        )
        .unwrap();
        assert!(crate::validation::resolve_file_type_exprs(&mut parsed, &context).is_empty());
        assert!(
            crate::validation::validate_attribute_params_with_context(
                &parsed.attribute_params,
                &context
            )
            .is_err(),
            "a loaded owner's missing evidence must be an error, not a deferred warning"
        );
        assert!(
            ProviderTypeCheckResult::settle(context.type_checks().take(), &context, ())
                .diagnostics()
                .is_empty()
        );
    }

    #[test]
    fn settlement_partial_catalog_preserves_owner_authority_and_unknown_bare_names() {
        let context = partial();
        for (source, expected_errors, expected_warnings) in [
            ("attributes { x: ReadyId = 'ok' }", 0, 0),
            ("attributes { x: ready.Value.Id = 'ok' }", 0, 0),
            ("attributes { x: ready.Value.Typo = 'ok' }", 1, 0),
            ("attributes { x: broken.Value.Id = 'ok' }", 0, 1),
            ("attributes { x: broken.Count = 42 }", 0, 1),
            ("attributes { x: BareTypo = 'ok' }", 0, 1),
            ("fn f(x: Arnn) { x }\nfn g(): Bogus { 'x' }", 0, 2),
        ] {
            let mut parsed = parse_file(
                source,
                Path::new("/project/main.crn"),
                &ProviderContext::default(),
            )
            .unwrap();
            let errors = crate::validation::resolve_file_type_exprs(&mut parsed, &context);
            assert_eq!(errors.len(), expected_errors, "{source}: {errors:#?}");
            if errors.is_empty() {
                crate::validation::validate_attribute_params_with_context(
                    &parsed.attribute_params,
                    &context,
                )
                .unwrap();
            }
            let result =
                ProviderTypeCheckResult::settle(context.type_checks().take(), &context, errors);
            assert_eq!(
                result.diagnostics().len(),
                expected_warnings,
                "{source}: {:?}",
                result.diagnostics()
            );
        }
    }
}

#[cfg(test)]
mod producer_tests {
    use super::*;
    use crate::parser::parse_file;

    fn warnings<T>(context: &ProviderContext, output: T) -> (T, Vec<ParseWarning>) {
        let (output, diagnostics) =
            ProviderTypeCheckResult::settle(context.type_checks().take(), context, output)
                .into_parts();
        (
            output,
            diagnostics
                .into_iter()
                .map(|diagnostic| match diagnostic {
                    ProviderTypeCheckDiagnostic::Deferred(warning) => warning,
                    other => panic!("unexpected settlement: {other:?}"),
                })
                .collect(),
        )
    }

    fn resolve<E: crate::parser::ExportParamLike>(
        parsed: &mut crate::parser::File<E>,
        context: &ProviderContext,
    ) {
        assert!(crate::validation::resolve_file_type_exprs(parsed, context).is_empty());
        crate::parser::resolve_resource_refs_with_config(parsed, context).unwrap();
    }

    #[test]
    fn settlement_module_failure_preserves_root_evidence() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("main.crn"), "let child = use { source = './missing' }\nchild {}\nattributes { id: AwsAccountId = '123' }\n").unwrap();
        let context = ProviderContext::default();
        let schemas = crate::schema::SchemaRegistry::new();
        let loaded = crate::config_loader::load_configuration_with_config(
            directory.path(),
            &context,
            &schemas,
        )
        .unwrap();
        let resolution = crate::module_resolver::resolve_module_template_with_providerless_config(
            loaded.parsed,
            directory.path(),
            &context,
            &schemas,
        );
        let (resolution, warnings) = warnings(&context, resolution);
        assert!(resolution.is_err());
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].message.contains("AwsAccountId"));
        assert_eq!(
            warnings[0].file.as_deref(),
            directory.path().join("main.crn").to_str()
        );
    }

    #[test]
    fn settlement_uncalled_and_repeated_module_imports_record_once() {
        for calls in ["", "let a = child {}\nlet b = child {}\n"] {
            let directory = tempfile::tempdir().unwrap();
            let child = directory.path().join("child");
            std::fs::create_dir(&child).unwrap();
            std::fs::write(
                child.join("main.crn"),
                "attributes { id: AwsAccountId = '123' }\n",
            )
            .unwrap();
            std::fs::write(
                directory.path().join("main.crn"),
                format!("let child = use {{ source = './child' }}\n{calls}"),
            )
            .unwrap();
            let context = ProviderContext::default();
            let schemas = crate::schema::SchemaRegistry::new();
            let loaded = crate::config_loader::load_configuration_with_config(
                directory.path(),
                &context,
                &schemas,
            )
            .unwrap();
            let resolution =
                crate::module_resolver::resolve_module_template_with_providerless_config(
                    loaded.parsed,
                    directory.path(),
                    &context,
                    &schemas,
                );
            let (resolution, warnings) = warnings(&context, resolution);
            resolution.unwrap();
            assert_eq!(warnings.len(), 1, "{warnings:#?}");
            assert_eq!(warnings[0].file.as_deref(), child.join("main.crn").to_str());
        }
    }

    #[test]
    fn settlement_calls_and_annotations_keep_their_own_files() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("functions.crn"),
            "fn f(x: Arn) { x }\n",
        )
        .unwrap();
        let path = directory.path().join("calls.crn");
        std::fs::write(
            &path,
            "let a = f('one')\nattributes { x: AwsAccountId = '123' }\n",
        )
        .unwrap();
        let context = ProviderContext::default();
        let loaded = crate::config_loader::load_configuration_with_config(
            directory.path(),
            &context,
            &crate::schema::SchemaRegistry::new(),
        )
        .unwrap();
        let mut parsed = loaded.parsed.parsed().clone();
        resolve(&mut parsed, &context);
        let (_, warnings) = warnings(&context, parsed);
        assert_eq!(warnings.len(), 3, "{warnings:#?}");
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.file.as_deref() == path.to_str())
                .count(),
            2,
            "{warnings:#?}"
        );
    }

    #[test]
    fn settlement_records_unchecked_assignment_at_its_declaration() {
        let context = ProviderContext::default();
        let mut parsed = parse_file(
            "arguments { id: AwsAccountId }\nattributes { value: String = id }\n",
            Path::new("/project/main.crn"),
            &context,
        )
        .unwrap();
        resolve(&mut parsed, &context);
        let schemas = crate::schema::SchemaRegistry::new();
        let bindings = crate::binding_index::BindingIndex::from_parsed(&parsed, &schemas, &context);
        let crate::binding_index::RefType::Typed(source) =
            bindings.ref_type(&crate::resource::RelativeBindingName::new("id"))
        else {
            panic!("typed argument")
        };
        source
            .is_assignable_to_sink(
                crate::schema::TypeInSchema::schemaless(
                    &crate::schema::CompiledAttributeType::string(),
                ),
                &bindings,
            )
            .unwrap();
        let (_, warnings) = warnings(&context, ());
        assert_eq!(warnings.len(), 1, "{warnings:#?}");
        assert!(warnings[0].message.contains("AwsAccountId"));
        assert_eq!(warnings[0].line, 1);
    }

    #[test]
    fn settlement_retains_nested_annotation_leaves_and_loaded_retry_is_strict() {
        let context = ProviderContext::default();
        let path = Path::new("/project/main.crn");
        let mut parsed = parse_file(
            "exports { ids: list(AwsAccountId) = ['123', '456'] }",
            path,
            &context,
        )
        .unwrap();
        resolve(&mut parsed, &context);
        let (_, warnings) = warnings(&context, ());
        assert_eq!(warnings.len(), 1, "one authored annotation: {warnings:#?}");
        assert_eq!(warnings[0].file.as_deref(), Some("/project/main.crn"));
        let loaded = crate::parser::LoadedProviderContext::empty_for_test();
        assert_eq!(
            crate::validation::resolve_file_type_exprs(&mut parsed, &loaded).len(),
            1
        );
        let result = ProviderTypeCheckResult::settle(loaded.type_checks().take(), &loaded, ());
        assert!(result.diagnostics().is_empty());
    }

    #[test]
    fn settlement_keeps_distinct_calls_on_the_same_line() {
        for annotation in ["Arn", "missing.Id"] {
            let context = ProviderContext::default();
            let mut parsed = parse_file(
                &format!("fn f(x: {annotation}) {{ x }}\nlet pair = [f('one'), f('two')]\n"),
                Path::new("/project/main.crn"),
                &context,
            )
            .unwrap();
            resolve(&mut parsed, &context);
            let (_, warnings) = warnings(&context, parsed);
            assert_eq!(
                warnings.len(),
                3,
                "one annotation and two calls: {warnings:#?}"
            );
            let calls = warnings
                .iter()
                .filter(|warning| warning.line == 2)
                .collect::<Vec<_>>();
            assert_eq!(calls.len(), 2);
            assert_ne!(
                calls[0].span.as_ref().unwrap().start_column,
                calls[1].span.as_ref().unwrap().start_column
            );
        }
    }
}
