//! #4304: the four infrastructure assignments share validation and discharge.

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use carina_cli::fixture_plan::{FixturePlan, build_plan_from_fixture_path_using};
use carina_cli::test_support::TestCommandRuntime;
use carina_cli::wiring::LoadedProviderFactory;
use carina_core::binding_index::{BindingValueSource, ResolvedBindings};
use carina_core::executor::{
    ModuleConstraintGate, ProviderPreparationContext, ProviderPreparationError,
    prepare_provider_ready_resource,
};
use carina_core::plan::PlanErrorKind;
use carina_core::provider::ProviderFactory;
use carina_core::resource::{ConcreteValue, SavedResource, Value};
use carina_core::schema::{SchemaKind, SchemaRegistry};
use carina_state::{ResourceState, StateFile};
use tower_lsp::lsp_types::DiagnosticSeverity;

#[path = "support/attribute_admission_infra.rs"]
mod infra;
use infra::InfraFactory;

fn factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![
        Box::new(InfraFactory("aws")),
        Box::new(InfraFactory("awscc")),
    ]
}

struct Case {
    directory: tempfile::TempDir,
    group: bool,
    source_type: &'static str,
    output: &'static str,
    destination: &'static str,
    attribute: &'static str,
    path: &'static str,
    file: &'static str,
}

impl Case {
    fn new(name: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/attribute_admission")
            .join(name);
        for file in [
            "main.crn",
            "exports.crn",
            "providers.crn",
            "backend.crn",
            "publish/main.crn",
            "publish/attributes.crn",
        ] {
            if source.join(file).exists() {
                let target = directory.path().join(file);
                fs::create_dir_all(target.parent().unwrap()).unwrap();
                fs::copy(source.join(file), target).unwrap();
            }
        }
        let group = name.starts_with("assignment");
        Self {
            directory,
            group,
            source_type: if group {
                "identitystore.Group"
            } else {
                "iam.Role"
            },
            output: if group { "group_id" } else { "arn" },
            destination: if group {
                "sso.Assignment"
            } else {
                "scheduler.Schedule"
            },
            attribute: if group { "principal_id" } else { "target" },
            path: if group {
                "principal_id"
            } else {
                "target.role_arn"
            },
            file: if group {
                "main.crn"
            } else {
                "publish/main.crn"
            },
        }
    }
    fn plan(&self) -> FixturePlan {
        build_plan_from_fixture_path_using(
            &TestCommandRuntime::new(),
            self.directory.path(),
            |_| LoadedProviderFactory::for_test(factories()),
        )
        .unwrap()
    }
    fn validate(&self) -> Vec<String> {
        carina_cli::commands::validate::validate_with_factories(
            &carina_core::paths::ResolvedPath::new(self.directory.path()).unwrap(),
            LoadedProviderFactory::for_test(factories()),
        )
    }
    fn lsp(&self, filename: &str) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(factories()),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        let engine = carina_lsp::diagnostics::DiagnosticEngine::new(
            Arc::new(loaded),
            vec!["aws".into(), "awscc".into()],
        );
        let path = self.directory.path().join(filename);
        let document = carina_lsp::document::Document::new_with_path(
            fs::read_to_string(&path).unwrap(),
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        engine
            .analyze(&document, Some(&path))
            .into_iter()
            .filter(|diagnostic| {
                matches!(
                    diagnostic.severity,
                    Some(DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING)
                ) && !diagnostic.message.starts_with("Unused let binding")
            })
            .collect()
    }
    fn values(&self) -> Vec<(String, bool)> {
        if self.group {
            vec![
                ("12345678-1234-1234-1234-123456789abc".into(), true),
                ("a".repeat(64), false),
                ("invalid-uuid".into(), false),
            ]
        } else {
            vec![
                ("arn:aws:iam::123456789012:role/scheduler".into(), true),
                ("x".repeat(1601), false),
                (String::new(), false),
            ]
        }
    }
    fn seed(&self, fixture: &FixturePlan, output: &str) {
        let source = fixture
            .unresolved_resources
            .iter()
            .find(|resource| resource.as_resource().id.resource_type == self.source_type)
            .unwrap()
            .as_resource();
        let mut row = ResourceState::try_applied(
            self.source_type,
            source.id.identity().unwrap().clone(),
            "aws",
            "source-id",
        )
        .unwrap()
        .with_attribute(self.output, serde_json::json!(output));
        row.binding = source.binding.clone();
        let mut state = StateFile::new();
        state.upsert_resource(row).unwrap();
        fs::write(
            self.directory.path().join("carina.state.json"),
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
    }
    fn literal(&self, output: &str) -> Value {
        let scalar = Value::Concrete(ConcreteValue::String(output.into()));
        if self.group {
            scalar
        } else {
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "role_arn".into(),
                scalar,
            )])))
        }
    }
}

fn timing_matrix(name: &str) {
    let case = Case::new(name);
    assert!(case.validate().is_empty(), "{name}: {:?}", case.validate());
    for file in [
        "main.crn",
        "exports.crn",
        "providers.crn",
        "backend.crn",
        case.file,
    ] {
        assert!(
            case.lsp(file).is_empty(),
            "{name}/{file}: {:?}",
            case.lsp(file)
        );
    }
    let pending = case.plan();
    assert!(
        pending.plan.errors().is_empty(),
        "{:?}",
        pending.plan.errors()
    );
    let source = pending
        .unresolved_resources
        .iter()
        .find(|origin| origin.as_resource().id.resource_type == case.source_type)
        .unwrap()
        .as_resource();
    let sink = pending
        .unresolved_resources
        .iter()
        .find(|origin| origin.as_resource().id.resource_type == case.destination)
        .unwrap()
        .as_resource()
        .clone();
    let source_binding = source.binding.as_ref().unwrap();
    let origin = format!("{source_binding}.{}", case.output);
    let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    tokio::runtime::Runtime::new().unwrap().block_on(carina_core::validation::deferred_types::with_session_for_test_async(loaded.provider_context(), async |session| {
        let bindings = ResolvedBindings::default();
        let gate = ModuleConstraintGate::from_live_compositions(&pending.compositions);
        let native_factories = factories();
        let context = ProviderPreparationContext::new(&bindings, &gate, &[], &carina_core::provider::NoopNormalizer, &native_factories, &pending.schemas, session);
        let saved: SavedResource = sink.clone().into();
        let live = prepare_provider_ready_resource(sink.clone(), &context).await;
        let saved = prepare_provider_ready_resource(saved, &context).await;
        for result in [live, saved] {
            assert!(matches!(result.unwrap_err(), ProviderPreparationError::Serialization(carina_core::value::SerializationError::Knowledge { error, .. })
                if matches!(*error, carina_core::resource::knowledge::KnowledgeDiagnostic::Absolute(carina_core::resource::knowledge::KnowledgeError::UnresolvedAtBoundary { .. }))));
        }
    }));
    for (output, fits) in case.values() {
        case.seed(&pending, &output);
        let planned = case.plan();
        let schema = pending
            .schemas
            .get("awscc", case.destination, SchemaKind::Resource)
            .unwrap();
        let literal: HashMap<String, _> =
            HashMap::from([(case.attribute.into(), case.literal(&output))]);
        let literal = literal
            .iter()
            .map(|(name, value)| {
                (
                    name.clone(),
                    value
                        .knowledge(
                            &carina_core::resource::knowledge::KnowledgeContext::validation(),
                        )
                        .unwrap()
                        .into_classification(),
                )
            })
            .collect();
        let literal_errors = schema
            .validate_with_resolver(
                &literal,
                &|_| false,
                loaded.provider_context().custom_type_resolver(),
            )
            .err()
            .unwrap_or_default();
        if fits {
            assert!(
                planned.plan.errors().is_empty(),
                "{:?}",
                planned.plan.errors()
            );
        } else {
            assert_eq!(
                planned.plan.errors().len(),
                1,
                "{:?}",
                planned.plan.errors()
            );
            let PlanErrorKind::ResolvedValueConstraint {
                attributes,
                origins,
                message,
            } = &planned.plan.errors()[0].kind
            else {
                panic!("expected resolved constraint: {:?}", planned.plan.errors());
            };
            assert_eq!(attributes, &[case.attribute]);
            assert_eq!(origins, &[origin.as_str()]);
            assert_eq!(
                message.as_str(),
                literal_errors[0]
                    .clone()
                    .with_attribute(case.path)
                    .to_string()
            );
            assert_eq!(message.as_str().matches(case.path).count(), 1, "{message}");
            let path = case.directory.path().join(case.file);
            let authored = fs::read_to_string(&path).unwrap();
            let reference = if case.group {
                "admins.group_id"
            } else {
                "scheduler_role.arn"
            };
            fs::write(&path, authored.replace(reference, &format!("'{output}'"))).unwrap();
            let cli = case.validate();
            assert_eq!(cli.len(), 1, "{cli:#?}");
            assert!(cli[0].contains(&literal_errors[0].to_string()), "{cli:#?}");
            let lsp = case.lsp(case.file);
            assert_eq!(lsp.len(), 1, "{lsp:#?}");
            let (line, start, end) = match name {
                "assignment_loop" => (4, 4, 16),
                "assignment_direct" => (2, 2, 14),
                "publish_first" | "publish_second" => (2, 13, 21),
                fixture => panic!("unexpected fixture {fixture}"),
            };
            assert_eq!(
                lsp[0].range,
                tower_lsp::lsp_types::Range::new(
                    tower_lsp::lsp_types::Position::new(line, start),
                    tower_lsp::lsp_types::Position::new(line, end)
                )
            );
            fs::write(&path, authored).unwrap();
        }
        tokio::runtime::Runtime::new().unwrap().block_on(
            carina_core::validation::deferred_types::with_session_for_test_async(
                loaded.provider_context(),
                async |session| {
                    let mut bindings = ResolvedBindings::default();
                    bindings.set(
                        source_binding.as_str(),
                        HashMap::from([(
                            case.output.into(),
                            Value::Concrete(ConcreteValue::String(output.clone().into())),
                        )]),
                        BindingValueSource::Local,
                    );
                    let gate = ModuleConstraintGate::from_live_compositions(&pending.compositions);
                    let native_factories = factories();
                    let context = ProviderPreparationContext::new(
                        &bindings,
                        &gate,
                        &[],
                        &carina_core::provider::NoopNormalizer,
                        &native_factories,
                        &pending.schemas,
                        session,
                    );
                    let mut resolved = sink.clone();
                    for (name, value) in &sink.attributes {
                        resolved.set_attr(
                            name,
                            carina_core::resolver::resolve_ref_value(value, &bindings, session)
                                .unwrap(),
                        );
                    }
                    let saved: SavedResource = resolved.clone().into();
                    let saved: SavedResource =
                        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();
                    let live = prepare_provider_ready_resource(resolved, &context).await;
                    let saved = prepare_provider_ready_resource(saved, &context).await;
                    if fits {
                        assert_eq!(
                            live.unwrap().as_resource().get_attr(case.attribute),
                            Some(&case.literal(&output))
                        );
                        assert_eq!(
                            saved.unwrap().as_resource().get_attr(case.attribute),
                            Some(&case.literal(&output))
                        );
                    } else {
                        for result in [live, saved] {
                            let ProviderPreparationError::ValueConstraint { errors, .. } =
                                result.unwrap_err()
                            else {
                                panic!("expected typed local constraint failure");
                            };
                            assert_eq!(errors.errors(), literal_errors);
                            assert_eq!(errors.to_string().matches(case.path).count(), 1);
                        }
                    }
                },
            ),
        );
    }
}

#[test]
fn assignment_direct_timing_and_saved_live_apply() {
    timing_matrix("assignment_direct");
}
#[test]
fn assignment_loop_timing_and_saved_live_apply() {
    timing_matrix("assignment_loop");
}
#[test]
fn publish_first_timing_and_saved_live_apply() {
    timing_matrix("publish_first");
}
#[test]
fn publish_second_timing_and_saved_live_apply() {
    timing_matrix("publish_second");
}

struct ApplyObserver;

impl carina_core::executor::ExecutionObserver for ApplyObserver {
    fn on_event(&self, _: &carina_core::executor::ExecutionEvent) {}
}

/// Exercise basic.rs resolution and ApplyResolved, with an output unavailable
/// until the upstream create in this execution has completed.
fn executor_timing_matrix(name: &str) {
    use carina_core::data_source_value::ExecutableDataSourceValues;
    use carina_core::executor::{
        EffectFailureKind, ExecutionCheckpointSink, ExecutionOutcome, LiveExecutionInput,
        ProviderCheckInputs, execute_live_plan_for_test,
    };
    use carina_core::resource::{Current, ResolvedResourceId};
    use std::sync::Mutex;

    let case = Case::new(name);
    for (output, fits) in case.values() {
        let fixture = case.plan();
        assert!(
            fixture.plan.errors().is_empty(),
            "{:?}",
            fixture.plan.errors()
        );
        assert!(
            fixture
                .managed_current
                .values()
                .all(|value| matches!(value, Current::Absent(_)))
        );
        let destinations: Vec<_> = fixture
            .unresolved_resources
            .iter()
            .filter(|origin| origin.as_resource().id.resource_type == case.destination)
            .map(|origin| origin.as_resource().id.clone())
            .collect();
        assert_eq!(
            destinations.len(),
            1,
            "the fixture contains one destination"
        );
        let origins = fixture
            .unresolved_resources
            .iter()
            .map(|origin| {
                (
                    ResolvedResourceId::new(origin.as_resource().id.clone()),
                    origin.clone(),
                )
            })
            .collect();
        let creates = Arc::new(Mutex::new(Vec::new()));
        let factories = infra::runtime_factories(&output, &creates);
        let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(infra::runtime_factories(&output, &creates)),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        let expected = infra::literal_errors(
            &fixture.schemas,
            loaded.provider_context(),
            &destinations[0],
            case.attribute,
            case.literal(&output),
        );
        let provider = infra::OutputProvider {
            name: "aws",
            output: output.clone(),
            creates: creates.clone(),
        };
        let outcome = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(execute_live_plan_for_test(
                &carina_cli::test_support::boundary(Box::new(provider)),
                LiveExecutionInput {
                    plan: fixture
                        .checked
                        .as_ref()
                        .expect("unknown output is admitted at plan"),
                    provider_check_inputs: ProviderCheckInputs::PlanNormalized {
                        resource_origins: &origins,
                        data_source_origins: &[],
                    },
                    compositions: &fixture.compositions,
                    bindings: ResolvedBindings::default(),
                    managed_current: fixture.managed_current,
                    data_source_values: ExecutableDataSourceValues::empty(),
                    normalizer: &carina_core::provider::NoopNormalizer,
                    provider_configs: &[],
                    provider_context: carina_core::parser::ProviderContext::clone(
                        loaded.provider_context(),
                    ),
                    factories: &factories,
                    schemas: &fixture.schemas,
                    parallelism: carina_cli::DEFAULT_PARALLELISM,
                    precompleted_success_count: 0,
                    checkpoint_sink: ExecutionCheckpointSink::noop(),
                },
                &ApplyObserver,
                carina_core::shutdown::ShutdownToken::running(),
            ));
        let ExecutionOutcome::Completed(result) = outcome else {
            panic!("fixture apply cancelled")
        };
        let creates = creates.lock().unwrap();
        assert_eq!(
            creates
                .iter()
                .filter(|resource| resource.id.resource_type == case.source_type)
                .count(),
            1
        );
        if fits {
            assert_eq!(result.failure_count(), 0, "{:?}", result.failures());
            assert_eq!(result.success_count, destinations.len() + 1);
            assert_eq!(creates.len(), destinations.len() + 1);
            for destination in &destinations {
                let request = creates
                    .iter()
                    .find(|resource| &resource.id == destination)
                    .unwrap();
                assert_eq!(
                    request.get_attr(case.attribute),
                    Some(&case.literal(&output))
                );
            }
        } else {
            assert_eq!(
                creates.len(),
                1,
                "invalid destinations crossed the provider gate"
            );
            assert_eq!(result.success_count, 1);
            assert_eq!(
                result.failure_count(),
                destinations.len(),
                "{:?}",
                result.failures()
            );
            for failure in result.failures() {
                let EffectFailureKind::Execution { error } = failure.kind() else {
                    panic!("{failure:?}")
                };
                let Some(ProviderPreparationError::ValueConstraint { errors, .. }) =
                    error.as_error().downcast_ref::<ProviderPreparationError>()
                else {
                    panic!("{failure:?}")
                };
                assert_eq!(errors.errors(), expected);
                assert!(
                    errors
                        .errors()
                        .iter()
                        .all(|error| error.attribute_name() == Some(case.attribute))
                );
                let message = failure.to_string();
                assert_eq!(message.matches(case.path).count(), 1, "{message}");
                assert_eq!(
                    errors.to_string(),
                    expected[0].clone().with_attribute(case.path).to_string()
                );
            }
        }
    }
}

#[test]
fn assignment_direct_unknown_at_plan_resolves_in_executor() {
    executor_timing_matrix("assignment_direct");
}

#[test]
fn assignment_loop_unknown_at_plan_resolves_in_executor() {
    executor_timing_matrix("assignment_loop");
}

#[test]
fn publish_first_unknown_at_plan_resolves_in_executor() {
    executor_timing_matrix("publish_first");
}

#[test]
fn publish_second_unknown_at_plan_resolves_in_executor() {
    executor_timing_matrix("publish_second");
}
