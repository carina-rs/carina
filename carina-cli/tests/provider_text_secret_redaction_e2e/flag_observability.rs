use super::*;
use carina_core::provider::{
    CanonicalCustomTypeValue, CanonicalProviderConfigAttributes, InitializeOutcomeMint,
    ProviderFactory, ProviderInitialization, ProviderResult, ProviderSession,
    ProviderValidationInputs,
};
use carina_core::provider_text::{EventLevel, ProviderDiagnostic, ProviderEvent};
use carina_core::schema::{
    AttributeSchema, AttributeType, PatternMatch, PatternSpec, ResourceSchema, TypeIdentity,
    ValidatedProviderSchemaWitness, ValidatedResourceSchema, legacy_validator,
};

#[test]
fn dispatch_debug_error_is_masked_in_cli_output() {
    const NAME: &str = "flag_observability::dispatch_debug_error_is_masked_in_cli_output";
    if let Some(output) = in_child_output(NAME, || crud(Failure::DebugCreate, SECRET, true)) {
        assert!(!String::from_utf8(output.stdout).unwrap().contains(SECRET));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains(SECRET), "{stderr}");
        let messages = stderr
            .lines()
            .filter(|line| line.contains("mock create debug password"))
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            ["      → mock create debug password Concrete(String(<sensitive string>))"]
        );
    }
}

#[test]
fn received_secret_errors_are_masked_in_cli_create_and_data_source_read() {
    let selected = std::env::var(CHILD).ok();
    for (name, failure, json, expected) in [
        (
            "flag_observability::received_secret_errors_are_masked_in_cli_create_and_data_source_read::create",
            Failure::CreateMessage,
            false,
            "      → mock create rejected password (secret); public=public-value-4305",
        ),
        (
            "flag_observability::received_secret_errors_are_masked_in_cli_create_and_data_source_read::read",
            Failure::DataSource,
            false,
            "Error: mock data source read rejected password (secret); public=public-value-4305",
        ),
        (
            "flag_observability::received_secret_errors_are_masked_in_cli_create_and_data_source_read::json",
            Failure::DataSource,
            true,
            "Error: mock data source read rejected password (secret); public=public-value-4305",
        ),
    ] {
        // Each child runs this test, selecting one provider operation. The
        // exact libtest name remains the same for both operation modes.
        const TEST: &str = "flag_observability::received_secret_errors_are_masked_in_cli_create_and_data_source_read";
        if let Some(selected) = selected.as_deref() {
            if selected != name {
                continue;
            }
            if failure == Failure::DataSource {
                plan_failure_using(failure, json);
            } else {
                crud(failure, SECRET, true);
            }
            return;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, name)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{stdout}{stderr}");
        assert!(!stdout.contains(SECRET), "{stdout}");
        assert!(!stderr.contains(SECRET), "{stderr}");
        let messages = stderr
            .lines()
            .filter(|line| line.contains("rejected password"))
            .collect::<Vec<_>>();
        assert_eq!(messages, [expected]);
    }
}

#[test]
fn observed_data_source_events_mask_changed_values_at_secret_paths() {
    const NAME: &str =
        "flag_observability::observed_data_source_events_mask_changed_values_at_secret_paths";
    if let Some(stderr) = in_child(NAME, || plan_failure_using(Failure::DataSourceEvents, true)) {
        assert!(!stderr.contains(SECRET), "{stderr}");
        assert!(
            !stderr.contains(&SECRET.chars().rev().collect::<String>()),
            "{stderr}"
        );
        let messages = stderr
            .lines()
            .filter(|line| line.starts_with("warn [mock]"))
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            [
                "warn [mock] mock.test.lookup.probe password: (secret)",
                "warn [mock] mock.test.lookup.probe strings: (secret)",
            ]
        );
    }
}

#[derive(Clone, Copy)]
enum Constraint {
    Pattern,
    Length,
    Enum,
    Custom,
    ProviderEvent,
}

struct ConstraintFactory(Constraint);

impl ProviderFactory for ConstraintFactory {
    fn name(&self) -> &str {
        "mock"
    }

    fn display_name(&self) -> &str {
        "Secret diagnostic display fixture"
    }

    fn validate_config(
        &self,
        attributes: &CanonicalProviderConfigAttributes,
        inputs: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        EchoFactory(Failure::CreateMessage).validate_config(attributes, inputs)
    }

    fn validate_custom_type(
        &self,
        _identity: &TypeIdentity,
        value: &CanonicalCustomTypeValue,
        inputs: &ProviderValidationInputs,
    ) -> ProviderResult<()> {
        if matches!(self.0, Constraint::ProviderEvent) {
            let diagnostic = ProviderDiagnostic::message(format!("received {value}").into());
            inputs.diagnostics().emit(ProviderEvent::diagnostic(
                EventLevel::Warn,
                "custom validation".into(),
                &diagnostic,
            ));
            Err(carina_core::provider::ProviderError::invalid_input(
                format!("validator rejected {value}"),
            ))
        } else {
            Ok(())
        }
    }

    fn create_provider(
        &self,
        _config: &ProviderSession,
        _mint: InitializeOutcomeMint,
    ) -> carina_core::provider::BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(EchoMock::new(Failure::CreateMessage))))
        })
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let identity = || TypeIdentity::new(Some("mock"), ["test", "Constraint"], "Password");
        let attribute = match self.0 {
            Constraint::Pattern => AttributeType::refined_string(
                None,
                Some(PatternSpec::new("allowed", PatternMatch::Full)),
                None,
                None,
            ),
            Constraint::Length => {
                AttributeType::refined_string(None, None, Some((None, Some(3))), None)
            }
            Constraint::Enum => {
                AttributeType::enum_(identity(), Some(vec!["Allowed".into()]), vec![], None, None)
            }
            Constraint::Custom => AttributeType::refined_string_with_validator(
                identity(),
                None,
                None,
                legacy_validator(|value| {
                    Err(format!(
                        "custom validator rejected {}",
                        carina_core::value::format_value(value)
                    )
                    .into())
                }),
                None,
            ),
            Constraint::ProviderEvent => {
                AttributeType::refined_string(Some(identity()), None, None, None)
            }
        };
        vec![
            ResourceSchema::new("test.constraint")
                .attribute(AttributeSchema::required("password", attribute).build())
                .validate()
                .unwrap(),
        ]
    }

    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            carina_core::schema::ValidatedConfigSchema::validate(Default::default()).unwrap(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
}

fn constraint_directory() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("main.crn"),
        format!("mock.test.constraint {{ password = secret('{SECRET}') }}\n"),
    )
    .unwrap();
    directory
}

#[test]
fn secret_schema_failures_have_exact_messages_in_validate_plan_and_lsp() {
    use carina_core::EnvironmentSnapshot;
    use carina_core::provider::loaded_provider_snapshot_from_factories;
    use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};
    use std::sync::Arc;

    for (constraint, expected) in [
        (
            Constraint::Pattern,
            "Invalid value (secret) for 'password': does not match required pattern /allowed/ (Full)",
        ),
        (
            Constraint::Length,
            "Invalid value (secret) for 'password': length 26 is outside allowed range unbounded..=3",
        ),
        (
            Constraint::Enum,
            "'password' (Password) expects an enum identifier, got a string literal (secret). Use one of: mock.test.Constraint.Password.Allowed",
        ),
        (
            Constraint::Custom,
            "Resource validation failed: (secret) failed validation",
        ),
    ] {
        let directory = constraint_directory();
        let factories =
            || LoadedProviderFactory::for_test(vec![Box::new(ConstraintFactory(constraint))]);
        let report = carina_cli::commands::validate::validate_diagnostics_with_factories(
            &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
            factories(),
            Default::default(),
        );
        let messages = report.json["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| error["message"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(messages, [expected]);
        let text = report
            .errors
            .iter()
            .map(|error| carina_cli::error::format_error_with_source_chain(error))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(expected), "{text}");
        for output in [
            text,
            format!("{:?}", report.errors),
            report.json.to_string(),
        ] {
            assert!(!output.contains(SECRET), "{output}");
        }
        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(
                carina_cli::test_support::run_saved_json_plan_with_factories(
                    &TestCommandRuntime::new(),
                    directory.path(),
                    &directory.path().join("plan.json"),
                    factories(),
                ),
            )
            .unwrap_err();
        let output = carina_cli::error::format_error_with_source_chain(&error);
        assert!(!output.contains(SECRET), "{output}");
        assert!(output.contains(expected), "{output}");
        assert!(!directory.path().join("plan.json").exists());
        assert!(!directory.path().join("carina.state.json").exists());

        let providers = loaded_provider_snapshot_from_factories(
            Arc::new(vec![
                Box::new(ConstraintFactory(constraint)) as Box<dyn ProviderFactory>
            ]),
            carina_core::schema::SchemaRegistry::new(),
            EnvironmentSnapshot::empty(),
        );
        let context = Arc::new(providers.provider_context().as_provider_context().clone());
        let engine = DiagnosticEngine::new(Arc::new(providers), vec!["mock".into()]);
        let path = directory.path().join("main.crn");
        let document = Document::new_with_path(
            std::fs::read_to_string(&path).unwrap(),
            path.clone(),
            context,
        );
        let diagnostics = engine.analyze(&document, Some(&path));
        assert!(!format!("{diagnostics:?}").contains(SECRET));
        assert_eq!(
            diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>(),
            [expected]
        );
    }
}

#[test]
fn secret_custom_validator_event_masks_the_received_value() {
    const NAME: &str = "flag_observability::secret_custom_validator_event_masks_the_received_value";
    if let Some(stderr) = in_child(NAME, || {
        let directory = constraint_directory();
        let report = carina_cli::commands::validate::validate_diagnostics_with_factories(
            &carina_core::paths::ResolvedPath::new(directory.path()).unwrap(),
            LoadedProviderFactory::for_test(vec![Box::new(ConstraintFactory(
                Constraint::ProviderEvent,
            ))]),
            Default::default(),
        );
        let messages = report.json["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|error| error["message"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            ["mock rejected `mock.test.Constraint.Password`: (secret) failed validation"]
        );
        assert!(!report.json.to_string().contains(SECRET));
    }) {
        assert!(!stderr.contains(SECRET), "{stderr}");
        assert_eq!(
            stderr.lines().collect::<Vec<_>>(),
            ["warn [mock] custom validation: received (secret)"]
        );
    }
}
