//! Saved-plan apply must discharge attribute admission through basic.rs.

use super::*;
#[path = "../../../../tests/support/attribute_admission_infra.rs"]
mod infra;

#[tokio::test]
async fn saved_plan_unknown_role_arn_is_checked_before_schedule_dispatch() {
    use carina_core::executor::{EffectFailureKind, ProviderPreparationError};

    for (output, fits) in [
        ("arn:aws:iam::123456789012:role/scheduler".to_string(), true),
        ("x".repeat(1601), false),
        (String::new(), false),
    ] {
        let fixture = ApplyCancellationFixture::new().with_raw_config(
            "provider aws {}\nprovider awscc {}\nlet role = aws.iam.Role {}\nlet schedule = awscc.scheduler.Schedule { target = { role_arn = role.arn } }\n",
        );
        let creates = Arc::new(Mutex::new(Vec::new()));
        let planned = tokio::task::spawn_blocking({
            let path = fixture.config_path().to_path_buf();
            let output = output.clone();
            let creates = creates.clone();
            move || {
                crate::fixture_plan::build_plan_from_fixture_path_using(
                    &crate::test_support::TestCommandRuntime::new(),
                    &path,
                    |_| {
                        crate::wiring::LoadedProviderFactory::for_test(infra::runtime_factories(
                            &output, &creates,
                        ))
                    },
                )
                .unwrap()
            }
        })
        .await
        .unwrap();
        assert!(
            planned.plan.errors().is_empty(),
            "{:?}",
            planned.plan.errors()
        );
        assert!(
            planned
                .managed_current
                .values()
                .all(|value| matches!(value, carina_core::resource::Current::Absent(_)))
        );
        let destination = planned
            .desired_resources
            .iter()
            .find(|resource| resource.as_inner().id.resource_type == "scheduler.Schedule")
            .unwrap()
            .as_inner()
            .id
            .clone();
        let literal = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "role_arn".into(),
            string_value(&output),
        )])));
        let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(infra::runtime_factories(&output, &creates)),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        let expected = infra::literal_errors(
            &planned.schemas,
            loaded.provider_context(),
            &destination,
            "target",
            literal.clone(),
        );
        let configs: Vec<carina_core::parser::ExpandedProviderConfig> = ["aws", "awscc"]
            .into_iter()
            .map(|name| ProviderConfig {
                name: name.into(),
                attributes: IndexMap::new(),
                default_tags: IndexMap::new(),
                source: Some("file:///admission-fixture.wasm".parse().unwrap()),
                version: None,
                revision: None,
                unresolved_attributes: IndexMap::new(),
                binding: None,
            })
            .collect();
        let validated = crate::wiring::ValidatedConfig::for_test_with_provider_phase(
            fixture.runtime(),
            (),
            crate::wiring::LoadedProviderFactory::for_test(infra::runtime_factories(
                &output, &creates,
            )),
            &configs,
        );
        let contents = PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: env!("CARGO_PKG_VERSION").into(),
            timestamp: "2026-11-09T00:00:00Z".into(),
            source_path: carina_core::paths::PersistedPath::new(fixture.config_path()).unwrap(),
            source_identity: carina_core::paths::CanonicalPath::new(fixture.config_path()).unwrap(),
            state_lineage: None,
            state_serial: None,
            provider_configs: saved_provider_configs(configs.clone()),
            backend_config: None,
            plan: planned.plan.to_snapshot(),
            sorted_resources: planned
                .desired_resources
                .iter()
                .map(|resource| resource.as_inner().to_snapshot())
                .collect(),
            sorted_value_destroy_dependencies: planned
                .desired_resources
                .iter()
                .map(|resource| resource.persisted_value_destroy_dependencies().clone())
                .collect(),
            sorted_explicit_destroy_dependencies: planned
                .desired_resources
                .iter()
                .map(|resource| resource.persisted_explicit_destroy_dependencies().clone())
                .collect(),
            legacy_dependency_classification: None,
            unresolved_resources: planned
                .unresolved_resources
                .iter()
                .map(|resource| resource.as_resource().to_snapshot())
                .collect(),
            compositions: Vec::new(),
            data_sources: Vec::new(),
            data_source_origins: Vec::new(),
            data_source_values: Vec::new(),
            current_states: planned
                .desired_resources
                .iter()
                .map(|resource| {
                    crate::commands::plan::CurrentStateEntry::managed(
                        resource.as_inner().id.clone(),
                        State::<carina_core::resource::NoPlaceholder>::not_found(
                            resource.as_inner().id.clone(),
                        ),
                    )
                })
                .collect(),
            upstream_snapshot: HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        };
        let saved = contents.record_for_test(&validated, Vec::new()).unwrap();
        let plan_path = fixture.config_path().join("admission.plan.json");
        std::fs::write(&plan_path, serde_json::to_vec(&saved).unwrap()).unwrap();
        let decoded = PlanFile::decode_json(&std::fs::read(&plan_path).unwrap()).unwrap();
        let source = ResolvedPath::new(fixture.config_path()).unwrap();
        let providers = ProviderLoad::for_test_with_provider_phase::<
            carina_core::resource::NoPlaceholder,
            carina_core::resource::AbsoluteBindingAddress,
        >(
            fixture.runtime(),
            crate::wiring::LoadedProviderFactory::for_test(infra::runtime_factories(
                &output, &creates,
            )),
            &configs,
        )
        .into_complete()
        .unwrap();
        let bound = ProviderBoundPlanFile::for_test(
            InspectedPlanFile::inspect(decoded),
            providers,
            &source,
        )
        .unwrap();
        let backend = fixture.backend_handle();
        let outcome = run_apply_from_plan_locked_owned(
            fixture.runtime().color_mode(),
            bound,
            fixture.provider_context(),
            true,
            backend.clone(),
            StateLock::disabled_for_test(),
            ApplyTarget::SavedPlan {
                plan_file: &plan_path,
                source_dir: fixture.config_path(),
            },
            &source,
            fixture.cancel_token(),
            &fixture.observer_factory(),
            NonZeroUsize::new(1).unwrap(),
            false,
        )
        .await;
        let result = finish_lock_command(
            fixture.runtime().color_mode(),
            backend.as_ref(),
            outcome,
            &fixture.cancel_token(),
        )
        .await;
        {
            let creates = creates.lock().unwrap();
            assert_eq!(
                creates
                    .iter()
                    .filter(|resource| resource.id.resource_type == "iam.Role")
                    .count(),
                1
            );
            if fits {
                result.unwrap();
                assert_eq!(creates.len(), 2);
                assert_eq!(creates[1].get_attr("target"), Some(&literal));
            } else {
                assert_eq!(
                    creates.len(),
                    1,
                    "invalid schedule crossed the provider gate"
                );
                let error = result.unwrap_err();
                let AppError::ExecutionFailed(report) = &error else {
                    panic!("{error:?}")
                };
                assert_eq!(report.failures().len(), 1);
                let failure = &report.failures()[0];
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
                        .all(|error| error.attribute_name() == Some("target"))
                );
                assert_eq!(
                    errors.to_string(),
                    expected[0]
                        .clone()
                        .with_attribute("target.role_arn")
                        .to_string()
                );
                assert_eq!(failure.to_string().matches("target.role_arn").count(), 1);
            }
        }
        let persisted = fixture.read_state().await;
        assert_eq!(persisted.resources().len(), if fits { 2 } else { 1 });
        assert_eq!(
            persisted
                .resources()
                .iter()
                .filter(|resource| resource.resource_type == "scheduler.Schedule")
                .count(),
            usize::from(fits)
        );
    }
}
