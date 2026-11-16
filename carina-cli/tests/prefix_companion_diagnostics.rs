//! Directory-scoped validate/LSP parity for generated-name companions (#4429).

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use carina_core::{EnvironmentSnapshot, UserDirs, parser::ProviderContext};
use carina_lsp::backend::{
    FactoryBuilder, ProviderStates, analyze_document, reload_provider_states,
};
use carina_lsp::document::Document;
use carina_lsp::factories::{build_factories, install_prober};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

async fn editor_diagnostics(root: &Path) -> Vec<Diagnostic> {
    editor_diagnostics_for(root, None).await
}

async fn editor_diagnostics_for(root: &Path, member: Option<&str>) -> Vec<Diagnostic> {
    let home = tempfile::tempdir().unwrap();
    let dirs = UserDirs::under(home.path());
    let builder: FactoryBuilder =
        Arc::new(move |configs| build_factories(Ok(&dirs), configs, &Default::default()));
    let states = tokio::sync::RwLock::new(ProviderStates::new(EnvironmentSnapshot::empty()));
    let reload = reload_provider_states(root, &builder, Some(&install_prober()), &states).await;
    assert!(reload.warnings.is_empty(), "{:?}", reload.warnings);
    assert!(
        reload.schemas > 0,
        "the built-in mock provider must be loaded"
    );
    let states = states.into_inner();
    let mut findings = Vec::new();
    let files = member.map_or_else(
        || vec!["providers.crn", "backend.crn", "main.crn"],
        |file| vec![file],
    );
    for name in files {
        let path = root.join(name);
        let document = Document::new_with_path(
            std::fs::read_to_string(&path).unwrap(),
            path.clone(),
            Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
        );
        let diagnostics = analyze_document(&states, &document, Some(&path));
        if member.is_none() && name != "main.crn" {
            assert!(diagnostics.is_empty(), "{name}: {diagnostics:#?}");
        }
        findings.extend(diagnostics);
    }
    findings
}

async fn assert_prefix_parity(source: &str, expected: Option<&str>) {
    let directory = tempfile::tempdir().unwrap();
    let root = carina_core::paths::ResolvedPath::new(directory.path())
        .unwrap()
        .source_label();
    for (name, text) in [
        ("providers.crn", "provider mock {}\n"),
        (
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        ),
        ("main.crn", source),
    ] {
        std::fs::write(root.join(name), text).unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", "--json"])
        .arg(&root)
        .current_dir(&root)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .unwrap();
    let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!("{error}: {output:?}");
    });
    assert!(output.stderr.is_empty(), "{output:?}");
    let lsp = editor_diagnostics(&root).await;
    let Some(expected) = expected else {
        assert!(output.status.success(), "validate: {cli:#}");
        assert_eq!(cli["status"], "ok", "{cli:#}");
        assert_eq!(cli["resource_count"], 1, "{cli:#}");
        assert!(
            lsp.is_empty(),
            "validate accepts the prefix companion, but LSP reports: {lsp:#?}"
        );
        return;
    };

    let lsp = lsp
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
        .collect::<Vec<_>>();
    assert!(!output.status.success(), "validate: {cli:#}");
    let cli_errors = cli["errors"].as_array().unwrap();
    assert_eq!(cli_errors.len(), 1, "validate: {cli:#}");
    let error = &cli_errors[0];
    let message = error["message"].as_str().unwrap();
    assert!(message.ends_with(expected), "validate: {cli:#}");
    assert_eq!(lsp.len(), 1, "validate: {cli:#}; LSP: {lsp:#?}");
    assert_eq!(lsp[0].message, message, "validate: {cli:#}; LSP: {lsp:#?}");
    assert_eq!(
        error["location"]["file"],
        root.join("main.crn").to_str().unwrap()
    );
    let range = &error["location"]["range"];
    let position = |name: &str| {
        Position::new(
            range[name]["line"].as_u64().unwrap() as u32 - 1,
            range[name]["column"].as_u64().unwrap() as u32 - 1,
        )
    };
    let cli_range = Range::new(position("start"), position("end"));
    assert_eq!(lsp[0].range, cli_range);
}

#[tokio::test]
async fn prefix_companion_directory_lsp_matches_validate() {
    assert_prefix_parity("mock.test.resource { name_prefix = 'ok' }\n", None).await;
}

#[tokio::test]
async fn prefix_companion_directory_unknown_sibling_matches_validate() {
    assert_prefix_parity(
        "mock.test.resource {\n  name_prefix = 'ok'\n  bogus = 'bad'\n}\n",
        Some("Unknown attribute 'bogus'"),
    )
    .await;
}

#[tokio::test]
async fn prefix_companion_directory_conflict_matches_validate() {
    assert_prefix_parity(
        "mock.test.resource {\n  name_prefix = 'ok'\n  name = 'explicit'\n}\n",
        Some("cannot specify both 'name_prefix' and 'name'"),
    )
    .await;
}

#[tokio::test]
async fn prefix_companion_directory_unsupported_attribute_matches_validate() {
    assert_prefix_parity(
        "mock.test.resource {\n  name = 'explicit'\n  tags_prefix = 'ok'\n}\n",
        Some("Unknown attribute 'tags_prefix'"),
    )
    .await;
    assert_prefix_parity(
        "mock.test.resource {\n  name = 'explicit'\n  name_prefix = 7\n}\n",
        Some("Unknown attribute 'name_prefix'"),
    )
    .await;
}

struct PrefixDirectory(tempfile::TempDir);

impl PrefixDirectory {
    fn new(files: &[(&str, &str)]) -> Self {
        let fixture = Self(tempfile::tempdir().unwrap());
        for (name, text) in [
            ("providers.crn", "provider mock {}\n"),
            (
                "backend.crn",
                "backend local { path = 'carina.state.json' }\n",
            ),
        ]
        .into_iter()
        .chain(files.iter().copied())
        {
            let path = fixture.0.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        fixture
    }

    fn cli(&self, native: bool) -> serde_json::Value {
        self.cli_at(".", native)
    }

    fn cli_at(&self, relative: &str, native: bool) -> serde_json::Value {
        let directory = self.0.path().join(relative);
        if native {
            return carina_cli::commands::validate::validate_diagnostics_with_factories(
                &carina_core::paths::ResolvedPath::new(&directory).unwrap(),
                carina_cli::wiring::LoadedProviderFactory::for_test(vec![Box::new(PrefixFactory)]),
                Default::default(),
            )
            .json;
        }
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .args(["validate", "--json"])
            .arg(&directory)
            .current_dir(&directory)
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(output.stderr.is_empty(), "{output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    async fn lsp(&self, native: bool) -> Vec<Diagnostic> {
        if !native {
            return editor_diagnostics(self.0.path()).await;
        }
        self.native_lsp_at("main.crn")
    }

    fn native_lsp_at(&self, relative: &str) -> Vec<Diagnostic> {
        let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(vec![
                Box::new(PrefixFactory) as Box<dyn carina_core::provider::ProviderFactory>
            ]),
            carina_core::schema::SchemaRegistry::new(),
            EnvironmentSnapshot::empty(),
        );
        let engine = carina_lsp::test_support::Diagnostics::new(
            self.0.path(),
            Arc::new(loaded),
            vec!["mock".into()],
        );
        let path = self.0.path().join(relative);
        let document = Document::new_with_path(
            std::fs::read_to_string(&path).unwrap(),
            path.clone(),
            Arc::new(ProviderContext::new(EnvironmentSnapshot::empty())),
        );
        engine.analyze(&document, Some(&path))
    }
}

struct PrefixFactory;

impl carina_core::provider::ProviderFactory for PrefixFactory {
    fn name(&self) -> &str {
        "mock"
    }
    fn display_name(&self) -> &str {
        "Prefix diagnostic fixture"
    }
    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _: &carina_core::provider::ProviderValidationInputs,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> carina_core::provider::BoxFuture<
        '_,
        carina_core::provider::ProviderResult<carina_core::provider::ProviderInitialization>,
    > {
        panic!("validation must not initialize the fixture provider")
    }
    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        use carina_core::schema::{
            AttributeSchema, AttributeType, PatternMatch, PatternSpec, ResourceSchema,
        };
        [
            ResourceSchema::new("PrefixSink")
                .attribute(AttributeSchema::required("attr", AttributeType::string()).build())
                .attribute(AttributeSchema::optional("count", AttributeType::int()).build()),
            ResourceSchema::new("IdentityPrefixSink").attribute(
                AttributeSchema::optional(
                    "attr",
                    AttributeType::refined_string(
                        Some(carina_core::schema::TypeIdentity::from_dotted(
                            "mock.PrefixName",
                        )),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            ),
            ResourceSchema::new("LengthSink").attribute(
                AttributeSchema::required(
                    "attr",
                    AttributeType::refined_string(None, None, Some((None, Some(12))), None),
                )
                .build(),
            ),
            ResourceSchema::new("PatternSink").attribute(
                AttributeSchema::required(
                    "attr",
                    AttributeType::refined_string(
                        None,
                        Some(PatternSpec::new("^ok-$", PatternMatch::Full)),
                        None,
                        None,
                    ),
                )
                .build(),
            ),
            ResourceSchema::new("IntSink")
                .attribute(AttributeSchema::required("attr", AttributeType::int()).build()),
        ]
        .into_iter()
        .map(|schema| schema.validate().unwrap())
        .collect()
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            vec![],
        )
        .unwrap()
    }
}

fn error_messages(diagnostics: &[Diagnostic]) -> Vec<&str> {
    diagnostics
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .map(|d| d.message.as_str())
        .collect()
}

async fn assert_directory_accepted(files: &[(&str, &str)]) {
    let fixture = PrefixDirectory::new(files);
    let cli = fixture.cli(false);
    let lsp = fixture.lsp(false).await;
    assert_eq!(cli["status"], "ok", "{cli:#}");
    assert!(error_messages(&lsp).is_empty(), "{lsp:#?}");
}

#[tokio::test]
async fn prefix_companion_cross_file_literal_binding() {
    assert_directory_accepted(&[
        ("lets.crn", "let p = 'lit'\n"),
        ("main.crn", "mock.test.resource { name_prefix = p }\n"),
    ])
    .await;
}

#[tokio::test]
async fn prefix_companion_cross_file_resource_attribute() {
    assert_directory_accepted(&[
        (
            "lets.crn",
            "let src = mock.test.resource { name = 'source' }\n",
        ),
        (
            "main.crn",
            "mock.test.resource { name_prefix = src.name }\n",
        ),
    ])
    .await;
}

#[tokio::test]
async fn prefix_companion_module_argument_and_instance() {
    assert_directory_accepted(&[
        (
            "main.crn",
            "let child = use { source = './child' }\nlet instance = child { prefix = 'mod' }\n",
        ),
        (
            "child/main.crn",
            "arguments { prefix: String }\nmock.test.resource { name_prefix = prefix }\n",
        ),
    ])
    .await;
}

#[tokio::test]
async fn prefix_companion_data_source_stays_unknown() {
    let fixture = PrefixDirectory::new(&[(
        "main.crn",
        "let roles = read mock.iam.Roles { name_regex_prefix = 'a' }\n",
    )]);
    let cli = fixture.cli(false);
    let lsp = fixture.lsp(false).await;
    assert_eq!(cli["status"], "error");
    assert_eq!(cli["errors"].as_array().unwrap().len(), 1, "{cli:#}");
    assert_eq!(
        error_messages(&lsp),
        ["Unknown attribute 'name_regex_prefix'"]
    );
    assert_eq!(cli["errors"][0]["message"], error_messages(&lsp)[0]);
}

#[tokio::test]
async fn prefix_companion_deferred_reference_rejected_at_plan_time() {
    for attribute in ["identifier", "id"] {
        let main = format!("mock.test.resource {{ name_prefix = src.{attribute} }}\n");
        let fixture = PrefixDirectory::new(&[
            (
                "lets.crn",
                "let src = mock.test.resource { name = 'source' }\n",
            ),
            ("main.crn", &main),
        ]);
        let cli = fixture.cli(false);
        let lsp = fixture.lsp(false).await;
        let messages = error_messages(&lsp);
        assert_eq!(cli["status"], "error", "{cli:#}");
        let expected = format!(
            "'name_prefix' must be known at plan time to generate 'name', but it depends on 'src.{attribute}', which is known only after apply"
        );
        assert!(messages.contains(&expected.as_str()), "{lsp:#?}");
        assert!(
            !messages.contains(&"Unknown attribute 'name_prefix'"),
            "{lsp:#?}"
        );
        assert!(
            !messages.contains(&"Required attribute 'name' is missing"),
            "{lsp:#?}"
        );
        if attribute == "identifier" {
            assert_eq!(messages.len(), 1, "{lsp:#?}");
            assert_eq!(cli["errors"].as_array().unwrap().len(), 1, "{cli:#}");
        } else {
            assert!(
                lsp.iter()
                    .any(|d| d.message.contains("Unknown attribute 'id'")),
                "nonexistent id needs its existing reference finding: {lsp:#?}"
            );
        }
    }
}

#[tokio::test]
async fn prefix_companion_deferred_for_remains_rejected() {
    let fixture = PrefixDirectory::new(&[
        (
            "lets.crn",
            "let src = mock.test.resource { name = 'source' }\n",
        ),
        (
            "main.crn",
            "let children = for item in src.domain_validation_options {\n  mock.test.resource { name_prefix = 'x' }\n}\n",
        ),
    ]);
    let cli = fixture.cli(false);
    let lsp = fixture.lsp(false).await;
    assert_eq!(cli["status"], "error", "{cli:#}");
    assert_eq!(error_messages(&lsp).len(), 2, "{lsp:#?}");
    for message in [
        "Required attribute 'name' is missing",
        "Unknown attribute 'name_prefix'",
    ] {
        assert!(error_messages(&lsp).contains(&message), "{lsp:#?}");
        assert!(
            cli["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["message"] == message),
            "{cli:#}"
        );
    }
}

#[tokio::test]
async fn prefix_companion_prefix_failure_is_per_owner_in_lsp() {
    let fixture = PrefixDirectory::new(&[(
        "main.crn",
        "mock.test.resource { name_prefix = '' }\nmock.test.resource { name = 'second' bogus = 'bad' }\n",
    )]);
    let cli = fixture.cli(false);
    let lsp = fixture.lsp(false).await;
    assert_eq!(cli["errors"].as_array().unwrap().len(), 1, "{cli:#}");
    let prefix = cli["errors"][0]["message"].as_str().unwrap();
    assert!(prefix.ends_with("'name_prefix' cannot be empty"), "{cli:#}");
    let messages = error_messages(&lsp);
    assert_eq!(messages.len(), 2, "{lsp:#?}");
    assert!(messages.contains(&prefix), "{lsp:#?}");
    assert!(messages.contains(&"Unknown attribute 'bogus'"), "{lsp:#?}");
}

#[tokio::test]
async fn prefix_companion_length_suffix_boundary_matches_cli() {
    for (prefix, rejected) in [("abcd", false), ("abcde", true)] {
        let main = format!("mock.LengthSink {{\n  attr_prefix = '{prefix}'\n}}\n");
        let fixture = PrefixDirectory::new(&[("main.crn", &main)]);
        let cli = fixture.cli(true);
        let lsp = fixture.lsp(true).await;
        let messages = error_messages(&lsp);
        assert_eq!(cli["status"] == "error", rejected, "{cli:#}");
        if rejected {
            assert_eq!(messages.len(), 1, "{lsp:#?}");
            assert_eq!(cli["errors"].as_array().unwrap().len(), 1, "{cli:#}");
            let cli_message = cli["errors"][0]["message"].as_str().unwrap();
            let generated = cli_message
                .strip_prefix("Invalid value '")
                .unwrap()
                .split_once("' for ")
                .unwrap()
                .0;
            assert!(generated.starts_with(prefix), "{cli:#}");
            let suffix = generated.strip_prefix(prefix).unwrap();
            assert_eq!(suffix.len(), 8);
            assert!(
                suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            // Only the sampled suffix differs; the finding and its boundary stay identical.
            assert_eq!(
                cli_message.replace(generated, &format!("{prefix}00000000")),
                messages[0],
                "{lsp:#?}"
            );
        } else {
            assert!(messages.is_empty(), "exactly 12 characters fit: {lsp:#?}");
        }
    }
}

#[tokio::test]
async fn prefix_companion_generated_value_pattern_is_checked() {
    let fixture =
        PrefixDirectory::new(&[("main.crn", "mock.PatternSink { attr_prefix = 'ok-' }\n")]);
    let cli = fixture.cli(true);
    let lsp = fixture.lsp(true).await;
    assert_eq!(
        cli["status"], "error",
        "the literal fits but generated name does not: {cli:#}"
    );
    let messages = error_messages(&lsp);
    assert_eq!(messages.len(), 1, "{lsp:#?}");
    assert!(!messages[0].contains("Unknown attribute"), "{lsp:#?}");
    assert!(!messages[0].contains("missing"), "{lsp:#?}");
    assert!(messages[0].contains("^ok-$"), "{lsp:#?}");
}

#[tokio::test]
async fn prefix_companion_expansion_failure_retains_merged_values() {
    let fixture = PrefixDirectory::new(&[
        ("lets.crn", "let p = 'lit'\n"),
        (
            "main.crn",
            "let child = use { source = './missing' }\nlet broken = child {}\nmock.test.resource { name_prefix = p }\n",
        ),
    ]);
    let cli = fixture.cli(false);
    let lsp = fixture.lsp(false).await;
    assert_eq!(cli["status"], "error", "{cli:#}");
    let messages = error_messages(&lsp);
    assert!(!messages.is_empty(), "the expansion failure must remain");
    assert!(
        !messages
            .iter()
            .any(|m| m.contains("Unknown attribute 'name_prefix'")
                || m.contains("Required attribute 'name'")),
        "{lsp:#?}"
    );
}

#[tokio::test]
async fn prefix_companion_recovery_does_not_invent_sibling_values() {
    let fixture = PrefixDirectory::new(&[
        ("lets.crn", "let p = 'lit'\nlet broken = [\n"),
        ("main.crn", "mock.test.resource { name_prefix = p }\n"),
    ]);
    let cli = fixture.cli(false);
    let lsp = fixture.lsp(false).await;
    assert_eq!(cli["status"], "error", "{cli:#}");
    assert!(
        error_messages(&lsp).contains(&"Unknown attribute 'name_prefix'"),
        "{lsp:#?}"
    );
    assert!(
        error_messages(&lsp).contains(&"Required attribute 'name' is missing"),
        "{lsp:#?}"
    );
}

#[tokio::test]
async fn prefix_companion_module_preflight_reference_findings_are_preserved() {
    let fixture = PrefixDirectory::new(&[
        (
            "main.crn",
            "let child = use { source = './child' }\nlet instance = child { marker = 'root' }\n",
        ),
        (
            "child/main.crn",
            "arguments { marker: String }\nlet src = mock.PrefixSink { attr_prefix = 'ok' }\nmock.IntSink { attr = src.attr }\n",
        ),
    ]);
    let cli = fixture.cli(true);
    assert_eq!(cli["status"], "error", "{cli:#}");
    println!(
        "PREFIX_PREFLIGHT_REFERENCE {}",
        cli["errors"]
            .to_string()
            .replace(fixture.0.path().to_str().unwrap(), "<fixture>")
    );
    let errors = cli["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{cli:#}");
    let message = errors[0]["message"].as_str().unwrap();
    assert!(
        message.contains("Int") && message.contains("String"),
        "{cli:#}"
    );
    assert!(!message.contains("Unknown attribute"), "{cli:#}");
    let lsp = fixture.lsp(true).await;
    assert!(
        error_messages(&lsp)
            .iter()
            .any(|m| m.contains("Int") && m.contains("String")),
        "{lsp:#?}"
    );
}

#[tokio::test]
async fn prefix_companion_module_outputs_on_unnamed_roots_rejected_once_per_owner() {
    let fixture = PrefixDirectory::new(&[
        (
            "main.crn",
            "let child = use { source = './child' }\nlet net = child {}\nmock.test.resource { name_prefix = net.first bogus = 'one' }\nmock.test.resource { name_prefix = net.second bogus = 'two' }\n",
        ),
        (
            "child/main.crn",
            "attributes { first: String = 'first' second: String = 'second' }\n",
        ),
    ]);
    let cli = fixture.cli(false);
    let lsp = fixture.lsp(false).await;
    let cli_messages = cli["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| error["message"].as_str().unwrap())
        .collect::<Vec<_>>();
    let expected = ["first", "second"].map(|attribute| format!(
        "'name_prefix' must be known at plan time to generate 'name', but it depends on 'net.{attribute}', which is known only after apply"
    ));
    assert_eq!(cli_messages, expected, "{cli:#}");
    assert_eq!(error_messages(&lsp), cli_messages, "{lsp:#?}");
}

#[tokio::test]
async fn prefix_companion_optional_named_string_matches_issue_4429() {
    let fixture = PrefixDirectory::new(&[(
        "main.crn",
        "mock.IdentityPrefixSink { attr_prefix = 'ok' }\n",
    )]);
    let cli = fixture.cli(true);
    assert_eq!(cli["status"], "ok", "{cli:#}");
    assert_eq!(cli["resource_count"], 1, "{cli:#}");
    let lsp = fixture.lsp(true).await;
    assert!(lsp.is_empty(), "{lsp:#?}");
}

#[tokio::test]
async fn prefix_companion_sibling_interpolation_matches_validate() {
    assert_directory_accepted(&[
        (
            "main.crn",
            "mock.test.resource { name_prefix = \"${p}-x\" }\n",
        ),
        ("lets.crn", "let p = 'lit'\n"),
    ])
    .await;
}

#[tokio::test]
async fn prefix_companion_builtin_string_call_matches_validate() {
    assert_directory_accepted(&[(
        "main.crn",
        "mock.test.resource { name_prefix = lower('AB') }\n",
    )])
    .await;
}

#[tokio::test]
async fn prefix_companion_with_binding_error_keeps_the_same_owner_finding() {
    let fixture = PrefixDirectory::new(&[
        (
            "main.crn",
            "mock.PrefixSink { attr_prefix = p count = bad }\n",
        ),
        ("lets.crn", "let p = 'lit'\nlet bad = 'wrong'\n"),
    ]);
    let cli = fixture.cli(true);
    let lsp = fixture.lsp(true).await;
    assert_eq!(cli["status"], "error", "{cli:#}");
    let expected = cli["errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|error| error["message"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(expected.len(), 1, "{cli:#}");
    assert!(expected[0].contains("expected Int"), "{cli:#}");
    assert_eq!(error_messages(&lsp), expected, "{lsp:#?}");
}

#[tokio::test]
async fn prefix_companion_module_deferred_if_matches_validate() {
    let fixture = PrefixDirectory::new(&[
        (
            "main.crn",
            "let child = use { source = './child' }\nlet instance = child { enabled = true }\n",
        ),
        ("child/arguments.crn", "arguments { enabled: Bool }\n"),
        (
            "child/main.crn",
            "let r = if enabled { mock.PrefixSink { attr_prefix = 'ok' } } else { mock.PrefixSink { attr_prefix = 'other' } }\n",
        ),
    ]);
    let cli = fixture.cli(true);
    assert_eq!(cli["status"], "ok", "{cli:#}");
    let root = fixture.lsp(true).await;
    assert!(error_messages(&root).is_empty(), "root: {root:#?}");
    let module = fixture.native_lsp_at("child/main.crn");
    assert!(
        error_messages(&module).is_empty(),
        "module-own diagnostics must agree with validate: {module:#?}"
    );
}

#[tokio::test]
async fn prefix_companion_root_static_if_matches_validate() {
    let fixture = PrefixDirectory::new(&[
        ("lets.crn", "let enabled = true\n"),
        (
            "main.crn",
            "let r = if enabled { mock.PrefixSink { attr_prefix = 'ok' } }\n",
        ),
    ]);
    let cli = fixture.cli(true);
    let lsp = fixture.lsp(true).await;
    assert_eq!(cli["status"], "ok", "{cli:#}");
    assert!(error_messages(&lsp).is_empty(), "{lsp:#?}");
}

async fn assert_unselected_prefix_rejection(attributes: &str, expected: &str) {
    let source = format!(
        "let src = mock.test.resource {{ name = 'source' }}\nlet r = if enabled {{\n  mock.test.resource {{\n    {attributes}\n    tags = src.name\n  }}\n}}\n"
    );
    let fixture = PrefixDirectory::new(&[
        (
            "main.crn",
            "let child = use { source = './child' }\nlet instance = child { enabled = false }\n",
        ),
        ("child/arguments.crn", "arguments { enabled: Bool }\n"),
        ("child/main.crn", &source),
    ]);
    let cli = fixture.cli(false);
    assert_eq!(
        cli["status"], "error",
        "a rejected, unselected template owner cannot make validation succeed: {cli:#}"
    );
    let errors = cli["errors"].as_array().unwrap();
    let finding = errors
        .iter()
        .find(|finding| {
            finding["message"]
                .as_str()
                .is_some_and(|message| message.contains(expected))
        })
        .unwrap_or_else(|| panic!("prefix rejection must be retained: {cli:#}"));
    assert!(
        finding["location"]["file"]
            .as_str()
            .unwrap()
            .ends_with("child/main.crn"),
        "{finding:#}"
    );
    assert_eq!(
        finding["location"]["range"]["start"]["line"], 3,
        "{finding:#}"
    );
}

#[tokio::test]
async fn prefix_companion_unselected_module_if_empty_prefix_is_reported() {
    assert_unselected_prefix_rejection("name_prefix = ''", "'name_prefix' cannot be empty").await;
}

#[tokio::test]
async fn prefix_companion_unselected_module_if_conflict_is_reported() {
    assert_unselected_prefix_rejection(
        "name = 'fixed'\n    name_prefix = 'bad'",
        "cannot specify both 'name_prefix' and 'name'",
    )
    .await;
}

async fn assert_selected_prefix_rejection(attributes: &str, expected: &str) {
    let source = format!("let r = if enabled {{ mock.test.resource {{ {attributes} }} }}\n");
    let fixture = PrefixDirectory::new(&[
        (
            "main.crn",
            "let child = use { source = './child' }\nlet instance = child { enabled = true }\n",
        ),
        ("child/arguments.crn", "arguments { enabled: Bool }\n"),
        ("child/main.crn", &source),
    ]);
    let cli = fixture.cli(false);
    assert_eq!(cli["status"], "error", "{cli:#}");
    let errors = cli["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{cli:#}");
    let message = format!("mock.test.resource.r: {expected}");
    assert_eq!(errors[0]["message"], message, "{cli:#}");
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .arg("validate")
        .arg(fixture.0.path())
        .current_dir(fixture.0.path())
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        format!("Error: child: {message}")
    );
    let lsp = fixture.lsp(false).await;
    let errors = error_messages(&lsp);
    assert_eq!(errors.len(), 1, "{lsp:#?}");
    assert!(errors[0].ends_with(expected), "{lsp:#?}");
}

#[tokio::test]
async fn prefix_companion_selected_module_if_empty_reports_once() {
    assert_selected_prefix_rejection("name_prefix = ''", "'name_prefix' cannot be empty").await;
}

#[tokio::test]
async fn prefix_companion_selected_module_if_conflict_reports_once() {
    assert_selected_prefix_rejection(
        "name_prefix = 'bad'\nname = 'fixed'",
        "cannot specify both 'name_prefix' and 'name'",
    )
    .await;
}

async fn assert_standalone_module_prefix(attributes: &str, expected: Option<&str>) {
    let source =
        format!("let r = if enabled {{\n  mock.test.resource {{\n    {attributes}\n  }}\n}}\n");
    let fixture = PrefixDirectory::new(&[
        ("main.crn", ""),
        ("child/arguments.crn", "arguments { enabled: Bool }\n"),
        ("child/main.crn", &source),
    ]);
    let cli = fixture.cli_at("child", false);
    let lsp = editor_diagnostics_for(fixture.0.path(), Some("child/main.crn")).await;
    match expected {
        None => {
            assert_eq!(cli["status"], "ok", "{cli:#}");
            assert!(error_messages(&lsp).is_empty(), "{lsp:#?}");
        }
        Some(expected) => {
            assert_eq!(cli["status"], "error", "{cli:#}");
            let errors = cli["errors"].as_array().unwrap();
            assert_eq!(errors.len(), 1, "{cli:#}");
            let message = errors[0]["message"].as_str().unwrap();
            assert!(message.contains(expected), "{cli:#}");
            assert_eq!(error_messages(&lsp), [message], "{lsp:#?}");
        }
    }
}

#[tokio::test]
async fn prefix_companion_standalone_module_if_valid_matches_lsp() {
    assert_standalone_module_prefix("name_prefix = 'ok'", None).await;
}

#[tokio::test]
async fn prefix_companion_standalone_module_if_empty_matches_lsp() {
    assert_standalone_module_prefix("name_prefix = ''", Some("'name_prefix' cannot be empty"))
        .await;
}

#[tokio::test]
async fn prefix_companion_standalone_module_if_conflict_matches_lsp() {
    assert_standalone_module_prefix(
        "name = 'fixed'\n    name_prefix = 'bad'",
        Some("cannot specify both 'name_prefix' and 'name'"),
    )
    .await;
}
