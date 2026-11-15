#[path = "support/conversion_factory.rs"]
mod base;

use carina_core::{paths::ResolvedPath, provider::*, schema::*};
use std::{collections::HashMap, sync::Arc};
use tower_lsp::lsp_types::{DiagnosticSeverity, NumberOrString};

struct ChildFactory {
    inner: Box<dyn ProviderFactory>,
}
impl ProviderFactory for ChildFactory {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn display_name(&self) -> &str {
        "Syntax child admission"
    }
    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        inputs: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        self.inner.validate_config(attributes, inputs)
    }
    fn create_provider(
        &self,
        session: &ProviderSession,
        mint: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        self.inner.create_provider(session, mint)
    }
    fn create_normalizer(
        &self,
        session: &ProviderSession,
        mint: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        self.inner.create_normalizer(session, mint)
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            vec![],
        )
        .unwrap()
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let expected = || {
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("mock.Expected")),
                None,
                None,
                None,
            )
        };
        let structure =
            || AttributeType::struct_("B", vec![StructField::optional("b", expected()).build()]);
        let mut schemas = self.inner.schemas();
        schemas.push(
            ResourceSchema::new("PendingSink")
                .attribute(
                    AttributeSchema::optional(
                        "union",
                        AttributeType::union(vec![
                            AttributeType::struct_(
                                "A",
                                vec![StructField::optional("a", AttributeType::string()).build()],
                            ),
                            structure(),
                        ]),
                    )
                    .build(),
                )
                .attribute(AttributeSchema::optional("direct", structure()).build())
                .attribute(
                    AttributeSchema::optional(
                        "locked",
                        AttributeType::struct_(
                            "Locked",
                            vec![
                                StructField::provider_populated("b", expected()).build(),
                                StructField::optional("selected", expected()).build(),
                            ],
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        schemas
    }
}
fn factories() -> Vec<Box<dyn ProviderFactory>> {
    base::factories()
        .into_iter()
        .map(|inner| Box::new(ChildFactory { inner }) as Box<dyn ProviderFactory>)
        .collect()
}
fn source(attributes: &str) -> String {
    format!(
        "provider mock {{}}\nlet p = mock.Source {{ output = 'ok' }}\nmock.PendingSink {{ {attributes} }}\n"
    )
}
fn write(directory: &std::path::Path, source: &str) {
    std::fs::write(directory.join("main.crn"), source).unwrap();
    std::fs::write(
        directory.join("backend.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .unwrap();
}
fn validate(directory: &std::path::Path) -> carina_cli::commands::validate::ValidateDiagnostics {
    carina_cli::commands::validate::validate_diagnostics_with_factories(
        &ResolvedPath::new(directory).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
        HashMap::new(),
    )
}
fn lsp(directory: &std::path::Path, source: String) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let loaded = loaded_provider_snapshot_from_factories(
        Arc::new(factories()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = carina_lsp::test_support::Diagnostics::new(
        directory,
        Arc::new(loaded),
        vec!["mock".into()],
    );
    let path = directory.join("main.crn");
    let document = carina_lsp::document::Document::new_with_path(
        source,
        path.clone(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    engine.analyze(&document, Some(&path))
}
fn assert_rejection(attributes: &str, reason: &str) {
    let directory = tempfile::tempdir().unwrap();
    let source = source(attributes);
    write(directory.path(), &source);
    let cli = validate(directory.path());
    assert_eq!(
        cli.json["errors"].as_array().map_or(0, Vec::len),
        1,
        "{}",
        cli.json
    );
    let errors = cli.json["errors"].as_array().unwrap();
    assert_eq!(errors[0]["code"], "conversion_destination");
    let message = errors[0]["message"].as_str().unwrap();
    assert!(message.contains(reason), "{}", cli.json);
    let lsp = lsp(directory.path(), source.clone());
    let lsp_errors = lsp
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect::<Vec<_>>();
    assert_eq!(lsp_errors.len(), 1, "{lsp:#?}");
    assert_eq!(
        lsp_errors[0].code,
        Some(NumberOrString::String("conversion_destination".into()))
    );
    assert_eq!(lsp_errors[0].message, message);
    assert_eq!(
        errors[0]["location"]["range"]["start"]["line"],
        lsp_errors[0].range.start.line + 1
    );
    assert_eq!(
        errors[0]["location"]["range"]["start"]["column"],
        lsp_errors[0].range.start.character + 1
    );
    let start = errors[0]["location"]["range"]["start"]["column"]
        .as_u64()
        .unwrap() as usize
        - 1;
    let operand = &source
        .lines()
        .nth(lsp_errors[0].range.start.line as usize)
        .unwrap()[start..];
    assert!(
        operand.starts_with("'ok' as") || operand.starts_with("p as"),
        "{}",
        cli.json
    );
}

#[test]
fn r7_union_rejection_has_cli_lsp_parity() {
    // The pending pairing is pinned in core; this native path pins the same
    // destination rejection, owner and message across CLI and LSP.
    assert_rejection(
        "union = { b = 'ok' as mock.Name }",
        "conversion destination must have exactly one matching type identity",
    );
}
#[test]
fn r7_discarded_struct_decline_reports_its_field_in_cli_and_lsp() {
    assert_rejection(
        "direct = if false { { unknown = 'ok' as mock.Expected } } else { { b = 'ok' as mock.Expected } }",
        "field 'unknown' is not declared by the destination",
    );
}
#[test]
fn r7_discarded_nonwritable_decline_reports_its_assignment_reason_in_cli_and_lsp() {
    assert_rejection(
        "locked = if false { { b = 'ok' as mock.Expected } } else { { selected = 'ok' as mock.Expected } }",
        "Attribute 'b' is populated by the provider and cannot be set",
    );
}
#[tokio::test]
async fn r7_valid_union_is_accepted_in_validate_lsp_and_plan() {
    let directory = tempfile::tempdir().unwrap();
    let source = source("union = { b = 'ok' as mock.Expected }");
    write(directory.path(), &source);
    let cli = validate(directory.path());
    assert_eq!(cli.json["status"], "ok", "{}", cli.json);
    let lsp = lsp(directory.path(), source);
    assert!(
        lsp.iter()
            .all(|d| d.severity != Some(DiagnosticSeverity::ERROR)),
        "{lsp:#?}"
    );
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    carina_cli::commands::init::run_init(&runtime, directory.path(), false, false, false, false)
        .await
        .unwrap();
    carina_cli::test_support::run_plan_with_factories(
        &runtime,
        directory.path(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn r7_no_as_structural_handle_twin_keeps_mains_early_failure() {
    let directory = tempfile::tempdir().unwrap();
    write(directory.path(), &source("direct = { unknown = p }"));
    let cli = validate(directory.path());
    assert_eq!(cli.json["status"], "error", "{}", cli.json);
    let messages = cli.json.to_string();
    assert!(
        messages.contains("is a resource, not a value"),
        "{messages}"
    );
    assert!(!messages.contains("conversion_"), "{messages}");
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    carina_cli::commands::init::run_init(&runtime, directory.path(), false, false, false, false)
        .await
        .unwrap();
    let error = carina_cli::test_support::run_plan_with_factories(
        &runtime,
        directory.path(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
    )
    .await
    .unwrap_err();
    let output = carina_cli::error::format_error_with_source_chain(&error);
    assert!(output.contains("is a resource, not a value"), "{output}");
    assert!(!output.contains("conversion"), "{output}");
}

#[tokio::test]
async fn r7_no_as_discarded_struct_keeps_validate_and_plan_success() {
    let directory = tempfile::tempdir().unwrap();
    let source = source("direct = if false { { unknown = 'ok' } } else { { b = 'ok' } }");
    write(directory.path(), &source);
    let cli = validate(directory.path());
    assert_eq!(cli.json["status"], "ok", "{}", cli.json);
    let lsp = lsp(directory.path(), source);
    assert!(
        lsp.iter()
            .all(|d| d.severity != Some(DiagnosticSeverity::ERROR)),
        "{lsp:#?}"
    );
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    carina_cli::commands::init::run_init(&runtime, directory.path(), false, false, false, false)
        .await
        .unwrap();
    carina_cli::test_support::run_plan_with_factories(
        &runtime,
        directory.path(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn r7_no_as_scalar_reference_keeps_mains_ordinary_field_error() {
    let directory = tempfile::tempdir().unwrap();
    let source = source("direct = { unknown = p.output }");
    write(directory.path(), &source);
    let cli = validate(directory.path());
    let errors = cli.json["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{}", cli.json);
    assert_eq!(errors[0]["message"], "Unknown field 'unknown' in B");
    assert!(errors[0]["code"].is_null(), "{}", cli.json);
    let lsp = lsp(directory.path(), source);
    let errors = lsp
        .iter()
        .filter(|diagnostic| diagnostic.message == "Unknown field 'unknown' in B")
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "{lsp:?}");
    assert_eq!(errors[0].severity, Some(DiagnosticSeverity::WARNING));
    assert!(lsp.iter().all(|diagnostic| !matches!(&diagnostic.code, Some(NumberOrString::String(code)) if code.starts_with("conversion_"))), "{lsp:?}");
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    carina_cli::commands::init::run_init(&runtime, directory.path(), false, false, false, false)
        .await
        .unwrap();
    let error = carina_cli::test_support::run_plan_with_factories(
        &runtime,
        directory.path(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories()),
    )
    .await
    .unwrap_err();
    let output = carina_cli::error::format_error_with_source_chain(&error);
    assert!(output.contains("Unknown field 'unknown' in B"), "{output}");
    assert!(!output.contains("conversion"), "{output}");
}
