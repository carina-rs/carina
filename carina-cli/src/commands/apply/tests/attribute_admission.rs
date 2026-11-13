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
        let literal = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "role_arn".into(),
            string_value(&output),
        )])));
        let expected = infra::expected_literal_errors("target", &output);
        let configs: Vec<carina_core::parser::ExpandedProviderConfig> = ["aws", "awscc"]
            .into_iter()
            .map(|name| ProviderConfig {
                name: name.into(),
                attributes: Default::default(),
                default_tags: Default::default(),
                source: Some("file:///admission-fixture.wasm".parse().unwrap()),
                version: None,
                revision: None,
                unresolved_attributes: Default::default(),
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
            backend_config: crate::commands::plan::SavedBackendConfig::default(),
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

// Saved module constraints contain ValidateExpr values, not TypeExpr records.
// Layer records are reloaded from the fingerprint-checked provider schemas.
#[test]
fn saved_plan_layered_scalars_reload_and_validate_both_layers() {
    use carina_core::schema::{
        IntIdentityLayer, IntMemberConstraints, PatternMatch, PatternSpec, StringIdentityLayer,
        StringMemberConstraints, TypeIdentity,
    };
    let tmp = tempfile::tempdir().unwrap();
    let schema = ResourceSchema::new("Layered")
        .attribute(
            AttributeSchema::required(
                "name",
                AttributeType::string_member(
                    StringIdentityLayer::new(
                        Some(TypeIdentity::from_dotted("mock.Name")),
                        Some(PatternSpec::new("[A-Z]+", PatternMatch::Full)),
                        Some((Some(1), Some(32))),
                        None,
                    ),
                    StringMemberConstraints::new(None, Some((Some(2), Some(4)))),
                ),
            )
            .build(),
        )
        .attribute(
            AttributeSchema::required(
                "count",
                AttributeType::int_member(
                    IntIdentityLayer::new(
                        Some(TypeIdentity::from_dotted("mock.Count")),
                        Some((Some(0), Some(100))),
                    ),
                    IntMemberConstraints::new(Some((Some(1), Some(10)))),
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let schemas = mock_schema_registry([schema.clone()]);
    for (name, count, valid_name, valid_count) in [
        ("AB", 5, true, true),
        ("ab", -1, false, false), // identity pattern and identity range
        ("A", 11, false, false),  // member length and member range
    ] {
        let resource = Resource::with_provider("mock", "Layered", "value", None)
            .with_attribute("name", string_value(name))
            .with_attribute("count", Value::Concrete(ConcreteValue::Int(count)));
        let contents = PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: env!("CARGO_PKG_VERSION").into(),
            timestamp: "2026-11-10T00:00:00Z".into(),
            source_path: carina_core::paths::PersistedPath::new(tmp.path()).unwrap(),
            source_identity: carina_core::paths::CanonicalPath::new(tmp.path()).unwrap(),
            state_lineage: None,
            state_serial: None,
            provider_configs: deserialize_saved_provider_configs(vec![ProviderConfig {
                name: "mock".into(),
                attributes: Default::default(),
                default_tags: Default::default(),
                source: None,
                version: None,
                revision: None,
                unresolved_attributes: Default::default(),
                binding: None,
            }]),
            backend_config: crate::commands::plan::SavedBackendConfig::default(),
            plan: carina_core::plan::SnapshotPlan::default(),
            sorted_resources: vec![resolved(resource).as_inner().to_snapshot()],
            sorted_value_destroy_dependencies: vec![Default::default()],
            sorted_explicit_destroy_dependencies: vec![Default::default()],
            legacy_dependency_classification: None,
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: Vec::new(),
            data_source_origins: Vec::new(),
            data_source_values: Vec::new(),
            current_states: Vec::new(),
            upstream_snapshot: HashMap::new(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        };
        let saved = record_with_test_schemas(contents, &schemas);
        let path = tmp.path().join("layered.v45.plan.json");
        std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let restored: PlanFile = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            serde_json::to_value(&restored).unwrap(),
            serde_json::to_value(&saved).unwrap()
        );
        let decoded = PlanFile::decode_json(&bytes).unwrap();
        let references = restored.referenced_schema_types();
        let reloaded = decoded.reload_in_flight_from_locked_state(None).unwrap();
        reloaded
            .verify_schema_fingerprints(&references, &schemas)
            .unwrap();
        let live = reloaded
            .resolve_with_schemas(&schemas)
            .unwrap()
            .into_live_without_secrets()
            .unwrap();
        let value = &live.sorted_resources()[0];
        for (attribute, valid) in [("name", valid_name), ("count", valid_count)] {
            let findings = schema
                .effective_attribute(attribute)
                .unwrap()
                .validate_collect(value.get_attr(attribute).unwrap());
            assert_eq!(
                findings.len(),
                usize::from(!valid),
                "{name}/{count}: {findings:?}"
            );
        }
    }
}
