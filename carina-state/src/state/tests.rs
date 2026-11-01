use super::*;
use carina_core::resource::{
    DeferredValue, PendingReRead, ReReadReason, ResolvedDependencies, SavedModuleSource,
    SavedResource,
};
use indexmap::IndexMap;
use std::collections::{BTreeSet, HashMap, HashSet};

fn published_exports_from_state(state: &StateFile) -> PublishedExports {
    let bytes = carina_core::utils::pretty_with_newline_bytes(state)
        .expect("test state must serialize with the state writer encoding");
    PublishedExports::from_state_bytes(&bytes).expect("serialized test exports must decode")
}

fn persisted_prevent_destroy_json(directives: &PersistedDirectives) -> serde_json::Value {
    serde_json::to_value(directives)
        .expect("persisted directives must serialize")["prevent_destroy"]
        .clone()
}

fn exact_comparison_context() -> carina_core::differ::ComparisonContext<'static> {
    carina_core::differ::exact_comparison_context_for_tests()
}

fn resource_state_from_provider_state_for_resource_and_schema_for_test<S: Clone>(
    resource: &carina_core::resource::ResolvedResource<S>,
    state: &State,
    existing: Option<&ResourceState>,
    schema: Option<&carina_core::schema::ResourceSchema>,
    row_basis: RowBasis<'_>,
    preferred_hashes: Option<&PersistedAttributes>,
) -> Result<ResourceState, ResourceStateEncodingError> {
    ResourceState::from_provider_state_for_resource_and_schema(
        resource,
        state,
        existing,
        schema,
        row_basis,
        preferred_hashes,
        &exact_comparison_context(),
    )
}

fn resource_state_from_observation_for_test<S: Clone>(
    resource: &carina_core::resource::ResolvedResource<S>,
    created: &ObservedCreate,
    existing: Option<&ResourceState>,
    schema: Option<&carina_core::schema::ResourceSchema>,
    preferred_hashes: Option<&PersistedAttributes>,
) -> Result<ResourceState, ResourceStateEncodingError> {
    ResourceState::from_observation(
        resource,
        created,
        existing,
        schema,
        preferred_hashes,
        &exact_comparison_context(),
    )
}

fn diff(
    desired: &carina_core::resource::ResolvedResource,
    current: &State,
    saved_attrs: &carina_core::provider::LiftedSavedAttrs,
    prev_explicit: Option<&carina_core::explicit::ExplicitFields>,
    schema: Option<&carina_core::schema::ResourceSchema>,
) -> carina_core::differ::Diff {
    carina_core::differ::diff(
        desired,
        current,
        saved_attrs,
        prev_explicit,
        schema,
        &exact_comparison_context(),
    )
}

fn migrate_legacy_with_config(
    state: StateFile,
    resources: &[Resource],
) -> Result<(StateFile, ReportedLegacyMigration), LegacyDependencyMigrationError> {
    let parsed: carina_core::parser::ParsedFile = Default::default();
    let graph = carina_core::resolved_graph::resolve_expanded_graph(parsed, resources, &[])
        .expect("legacy migration fixture graph must resolve");
    let no_move_plan: carina_core::plan::Plan = carina_core::plan::Plan::new();
    let moved_pairs =
        carina_core::effect::ResolvedMovePairs::from_saved_plan(no_move_plan.move_effects());
    let configured =
        carina_core::resolved_graph::ConfiguredResources::from_test_resources(resources);
    let input = LegacyMigrationInput::planned(&graph, &configured, &moved_pairs);
    let classification = state.classify_legacy_dependencies(&input)?;
    let migration = state.with_legacy_classification(classification)?;
    let migrated_rows = migration.migrated_rows();
    let mut warnings = Vec::new();
    let state = migration.report(|warning| warnings.push(warning.clone()));
    Ok((
        state,
        ReportedLegacyMigration {
            migrated_rows,
            warnings,
        },
    ))
}

#[derive(Debug)]
struct ReportedLegacyMigration {
    migrated_rows: usize,
    warnings: Vec<LegacyDestroyOrderingWarning>,
}

impl ReportedLegacyMigration {
    fn migrated_rows(&self) -> usize {
        self.migrated_rows
    }

    fn warnings(&self) -> &[LegacyDestroyOrderingWarning] {
        &self.warnings
    }
}

fn finish_legacy_dependency_migration(state: StateFile) -> StateFile {
    if !state.requires_legacy_dependency_migration() {
        return state;
    }
    let (state, report) = migrate_legacy_with_config(state, &[])
        .expect("source-independent migration fixture must cross the v20 dependency seam");
    assert!(
        report.warnings().is_empty(),
        "migration fixture contains an unresolved legacy dependency: {:?}",
        report.warnings()
    );
    state
}

#[test]
fn v19_migrates_to_unwritable_v20_then_graph_aware_v21_without_losing_row_data() {
    let json = serde_json::json!({
        "version": 19,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 7,
        "lineage": "v19-lineage",
        "carina_version": "legacy",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "carried",
            "provider": "mock",
            "current": {
                "kind": "applied",
                "identifier": "provider-id",
                "attributes": {"name": "carried"},
                "nested_write_only": {}
            },
            "protected": true,
            "directives": {"prevent_destroy": false},
            "binding": "carried_binding"
        }],
        "exports": {"answer": 42}
    });

    let migrated = check_and_migrate(&json.to_string()).expect("v19 state must migrate");
    assert_eq!(
        migrated.migration(),
        Some(MigrationInfo { from: 19, to: 21 })
    );
    let state = migrated.into_state();
    assert_eq!(state.version, 20);
    assert!(state.requires_legacy_dependency_migration());
    assert!(serde_json::to_value(&state).is_err());
    let (state, report) = migrate_legacy_with_config(state, &[])
        .expect("an empty legacy dependency snapshot is graph-migratable");
    assert_eq!(report.migrated_rows(), 0);
    assert_eq!(state.version, 21);
    assert_eq!(state.serial, 7);
    assert_eq!(state.lineage, "v19-lineage");
    assert_eq!(state.resources().len(), 1);
    assert_eq!(state.resources()[0].identity.as_str(), "carried");
    assert!(state.resources()[0].protected);
    assert_eq!(
        persisted_prevent_destroy_json(&state.resources()[0].directives),
        serde_json::json!({"kind": "unspecified"})
    );
    assert_eq!(
        state.resources()[0].binding.as_deref(),
        Some("carried_binding")
    );
    assert_eq!(
        serde_json::to_value(&state).unwrap()["exports"]["answer"],
        serde_json::json!(42)
    );
}

#[test]
fn v19_migration_preserves_true_and_makes_false_or_missing_unspecified() {
    let row = |identity: &str, directives: Option<serde_json::Value>| {
        let mut row = serde_json::json!({
            "resource_type": "test.Resource",
            "identity": identity,
            "provider": "mock",
            "current": {
                "kind": "applied",
                "identifier": format!("provider-{identity}"),
                "attributes": {},
                "nested_write_only": {}
            }
        });
        if let Some(directives) = directives {
            row["directives"] = directives;
        }
        row
    };
    let json = serde_json::json!({
        "version": 19,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "prevent-destroy-migration",
        "carina_version": "legacy",
        "resources": [
            row("explicit-true", Some(serde_json::json!({"prevent_destroy": true}))),
            row("legacy-false", Some(serde_json::json!({"prevent_destroy": false}))),
            row("legacy-missing", None)
        ]
    });

    let state = check_and_migrate(&json.to_string())
        .expect("v19 directives must migrate")
        .into_state();
    let settings = state
        .resources()
        .iter()
        .map(|row| {
            (
                row.identity.as_str(),
                persisted_prevent_destroy_json(&row.directives),
            )
        })
        .collect::<HashMap<_, _>>();

    assert_eq!(
        settings["explicit-true"],
        serde_json::json!({"kind": "explicit", "value": true})
    );
    assert_eq!(
        settings["legacy-false"],
        serde_json::json!({"kind": "unspecified"})
    );
    assert_eq!(
        settings["legacy-missing"],
        serde_json::json!({"kind": "unspecified"})
    );
}

#[test]
fn v20_to_v21_preserves_all_three_prevent_destroy_states() {
    let row = |identity: &str, prevent_destroy: serde_json::Value| {
        serde_json::json!({
            "resource_type": "test.Resource",
            "identity": identity,
            "provider": "mock",
            "current": {
                "kind": "applied",
                "identifier": format!("provider-{identity}"),
                "attributes": {},
                "nested_write_only": {}
            },
            "directives": {"prevent_destroy": prevent_destroy}
        })
    };
    let json = serde_json::json!({
        "version": 20,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "tri-state-v20-to-v21",
        "carina_version": "test",
        "resources": [
            row("unspecified", serde_json::json!({"kind": "unspecified"})),
            row("explicit-false", serde_json::json!({"kind": "explicit", "value": false})),
            row("explicit-true", serde_json::json!({"kind": "explicit", "value": true}))
        ],
        "exports": {}
    });

    let state = check_and_migrate(&json.to_string())
        .expect("v20 tri-state directives must decode")
        .into_state();
    assert_eq!(state.version, 20);
    let (state, report) = migrate_legacy_with_config(state, &[])
        .expect("empty dependency sets must cross the v20 to v21 graph seam");
    assert_eq!(report.migrated_rows(), 0);
    assert_eq!(state.version, 21);

    let settings = state
        .resources()
        .iter()
        .map(|row| {
            (
                row.identity.as_str(),
                persisted_prevent_destroy_json(&row.directives),
            )
        })
        .collect::<HashMap<_, _>>();
    assert_eq!(
        settings["unspecified"],
        serde_json::json!({"kind": "unspecified"})
    );
    assert_eq!(
        settings["explicit-false"],
        serde_json::json!({"kind": "explicit", "value": false})
    );
    assert_eq!(
        settings["explicit-true"],
        serde_json::json!({"kind": "explicit", "value": true})
    );
    serde_json::to_value(&state).expect("the migrated v21 state must be writable");
}

#[test]
fn create_outcome_unknown_row_round_trips_without_identifier_or_token() {
    use carina_core::resource::Current;

    let json = serde_json::json!({
        "version": 20,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "unknown-create",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "unknown",
            "provider": "mock",
            "current": {
                "kind": "create_outcome_unknown",
                "started_at": "2026-10-23T08:00:00Z"
            },
            "directives": {"provider_instance": "issuing"},
            "binding": "unknown_binding",
            "deposed": [{
                "key": "00000000-0000-4000-8000-000000000001",
                "identifier": "old-id",
                "provider_instance": "old-route",
                "attributes": {},
                "dependency_bindings": []
            }]
        }],
        "exports": {}
    });
    let state = check_and_migrate(&json.to_string())
        .expect("v20 outcome-unknown state must load")
        .into_state();
    assert_eq!(state.version, 20);
    let (state, _report) = migrate_legacy_with_config(state, &[])
        .expect("token-less outcome rows must survive graph-aware migration");
    assert_eq!(state.version, 21);
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.Resource",
        "unknown",
        Some("issuing".into()),
    );

    let Current::CreateOutcomeUnknown(unknown) = state.build_state_for_resource(&id) else {
        panic!("the token-less protected row must retain its typed lifecycle state")
    };
    assert_eq!(unknown.id(), &id);
    assert_eq!(unknown.started_at().as_str(), "2026-10-23T08:00:00Z");
    let row = state
        .find_resource_for_id(&id)
        .expect("outcome-unknown row remains addressable");
    assert_eq!(row.identifier(), None);
    assert!(row.refreshable().is_none());
    assert_eq!(row.binding.as_deref(), Some("unknown_binding"));
    assert_eq!(row.deposed.len(), 1);

    let encoded = serde_json::to_value(&state).expect("v21 state must serialize");
    assert_eq!(encoded["version"], serde_json::json!(21));
    assert_eq!(
        encoded["resources"][0]["current"],
        serde_json::json!({
            "kind": "create_outcome_unknown",
            "started_at": "2026-10-23T08:00:00Z"
        })
    );
    let reloaded = check_and_migrate(&encoded.to_string())
        .expect("serialized outcome-unknown row must reload")
        .into_state();
    assert_eq!(
        serde_json::to_value(&state).unwrap(),
        serde_json::to_value(&reloaded).unwrap()
    );
}

#[test]
fn create_outcome_unknown_row_rejects_identifier_and_token_fields() {
    let base = serde_json::json!({
        "version": 20,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "exclusive-unknown-create",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "unknown",
            "provider": "mock",
            "current": {
                "kind": "create_outcome_unknown",
                "started_at": "2026-10-23T08:00:00Z"
            }
        }],
        "exports": {}
    });
    for forbidden in ["identifier", "token"] {
        let mut malformed = base.clone();
        malformed["resources"][0]["current"][forbidden] = serde_json::json!("forbidden");
        assert!(
            check_and_migrate(&malformed.to_string()).is_err(),
            "outcome-unknown current must reject {forbidden}"
        );
    }
}

#[test]
fn outcome_unknown_lookup_mints_the_only_forget_handle() {
    let json = serde_json::json!({
        "version": 20,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "forget-unknown-create",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "unknown",
            "provider": "mock",
            "current": {
                "kind": "create_outcome_unknown",
                "started_at": "2026-10-23T08:00:00Z"
            }
        }],
        "exports": {}
    });
    let mut state = check_and_migrate(&json.to_string()).unwrap().into_state();
    let address = StateBlockAddress::new("mock", "test.Resource", "unknown");
    let id = ResourceId::with_provider_identity("mock", "test.Resource", "unknown", None);

    assert_eq!(
        state.import_target_disposition_for_id(&id),
        ImportTargetDisposition::ReplaceCreateOutcomeUnknown
    );
    let handle = state
        .find_outcome_unknown(&address)
        .expect("the protected row must mint a typed forget handle");
    let forgotten = StateFile::forget(handle);

    assert_eq!(forgotten.identity.as_str(), "unknown");
    assert!(
        state
            .find_resource_by_state_block_address(&address)
            .is_none()
    );
    assert_eq!(
        state.import_target_disposition_for_id(&id),
        ImportTargetDisposition::Vacant
    );
}

#[test]
fn outcome_unknown_forget_keeps_deposed_generations_in_a_shell() {
    let json = serde_json::json!({
        "version": 20,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "forget-unknown-create-with-deposed",
        "carina_version": "test",
        "resources": [
            {
                "resource_type": "test.Resource",
                "identity": "unknown",
                "provider": "mock",
                "current": {
                    "kind": "create_outcome_unknown",
                    "started_at": "2026-10-23T08:00:00Z"
                },
                "directives": { "provider_instance": "issuing" },
                "deposed": [{
                    "key": "00000000-0000-4000-8000-000000000001",
                    "identifier": "old-id",
                    "provider_instance": "old-route",
                    "attributes": {},
                    "dependency_bindings": ["network"]
                }]
            },
            {
                "resource_type": "test.Network",
                "identity": "network",
                "provider": "mock",
                "binding": "network",
                "current": {
                    "kind": "applied",
                    "identifier": "network-id",
                    "attributes": {},
                    "write_only_attributes": [],
                    "nested_write_only": {}
                }
            }
        ],
        "exports": {}
    });
    let mut state = check_and_migrate(&json.to_string()).unwrap().into_state();
    let address = StateBlockAddress::new("mock", "test.Resource", "unknown");
    let id = ResourceId::with_provider_identity(
        "mock",
        "test.Resource",
        "unknown",
        Some("issuing".to_string()),
    );

    let forgotten = StateFile::forget(
        state
            .find_outcome_unknown(&address)
            .expect("the protected row must mint a typed forget handle"),
    );

    assert!(matches!(
        forgotten.current(),
        CurrentInstance::CreateOutcomeUnknown { .. }
    ));
    let shell = state
        .find_resource_for_id(&id)
        .expect("forget must retain a shell for the deposed generation");
    assert!(matches!(shell.current(), CurrentInstance::None));
    assert_eq!(shell.deposed.len(), 1);
    assert_eq!(shell.deposed[0].identifier, "old-id");
    assert_eq!(
        shell.deposed[0].provider_instance.as_deref(),
        Some("old-route")
    );
}

#[test]
fn outcome_unknown_lookup_rejects_normal_and_missing_rows_with_removed_hint() {
    let mut state = StateFile::new();
    state
        .upsert_resource(
            ResourceState::try_applied("test.Resource", "normal", "mock", "provider-id").unwrap(),
        )
        .unwrap();
    let normal = StateBlockAddress::new("mock", "test.Resource", "normal");
    let missing = StateBlockAddress::new("mock", "test.Resource", "missing");
    let normal_id = ResourceId::with_provider_identity("mock", "test.Resource", "normal", None);

    assert_eq!(
        state.import_target_disposition_for_id(&normal_id),
        ImportTargetDisposition::Occupied
    );
    let normal_error = state.find_outcome_unknown(&normal).unwrap_err();
    assert_eq!(
        normal_error.kind(),
        NotOutcomeUnknownKind::DifferentLifecycle
    );
    assert!(
        normal_error
            .display_with_project_dir(std::path::Path::new("infra/production project"))
            .to_string()
            .contains("`carina state forget mock.test.Resource normal 'infra/production project'`")
    );
    assert_eq!(
        normal_error.to_string(),
        "no state row at this address is a create whose outcome is unknown"
    );

    let missing_error = state.find_outcome_unknown(&missing).unwrap_err();
    assert_eq!(missing_error.kind(), NotOutcomeUnknownKind::Missing);
    assert!(
        missing_error
            .display_with_project_dir(std::path::Path::new("."))
            .to_string()
            .contains("removed { from = mock.test.Resource 'missing' }")
    );
    assert_eq!(
        missing_error.to_string(),
        "no state row exists at this address"
    );
}

#[test]
fn v15_migrates_to_current_and_round_trips_all_current_instance_variants() {
    let json = serde_json::json!({
        "version": 15,
        "serial": 4,
        "lineage": "test-lineage",
        "carina_version": "test",
        "resources": [
            {
                "resource_type": "test.Resource",
                "identity": "none",
                "provider": "mock",
                "current": { "kind": "none" },
                "deposed": [{
                    "key": "00000000-0000-4000-8000-000000000001",
                    "identifier": "old",
                    "attributes": {},
                    "dependency_bindings": []
                }]
            },
            {
                "resource_type": "test.Resource",
                "identity": "applied",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "cloud-id",
                    "attributes": { "name": "example" },
                    "write_only_attributes": ["password"],
                    "partial_read": null
                }
            },
            {
                "resource_type": "test.Resource",
                "identity": "pending",
                "provider": "mock",
                "current": {
                    "kind": "create_in_flight",
                    "envelope": {
                        "token": "opaque-token",
                        "identifier": null,
                        "write_only": {},
                        "secrets": {}
                    },
                    "started_at": "2026-10-23T08:00:00Z"
                }
            },
            {
                "resource_type": "test.Resource",
                "identity": "missing",
                "provider": "mock",
                "current": {
                    "kind": "missing_identifier",
                    "attributes": { "name": "unknown-owner" }
                }
            }
        ],
        "exports": {}
    });

    let migrated = check_and_migrate(&json.to_string()).expect("v15 state must load");
    assert_eq!(
        migrated.migration(),
        Some(MigrationInfo {
            from: 15,
            to: StateFile::CURRENT_VERSION,
        })
    );
    let state = finish_legacy_dependency_migration(migrated.into_state());
    assert!(matches!(
        state.resources()[0].current(),
        CurrentInstance::None
    ));
    assert!(matches!(
        state.resources()[1].current(),
        CurrentInstance::Applied { .. }
    ));
    assert!(matches!(
        state.resources()[2].current(),
        CurrentInstance::CreateInFlight { .. }
    ));
    assert!(matches!(
        state.resources()[3].current(),
        CurrentInstance::MissingIdentifier { .. }
    ));

    let encoded = serde_json::to_string(&state).expect("v15 state must serialize");
    let reloaded = check_and_migrate(&encoded)
        .expect("serialized v15 state must reload")
        .into_state();
    assert_eq!(
        serde_json::to_value(&state).unwrap(),
        serde_json::to_value(&reloaded).unwrap()
    );
}

#[test]
fn v17_migrates_nested_write_only_records_to_empty() {
    let json = serde_json::json!({
        "version": 17,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "v17-lineage",
        "carina_version": "legacy",
        "resources": [
            {
                "resource_type": "test.Resource",
                "identity": "example",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "cloud-id",
                    "attributes": { "name": "example" }
                }
            },
            {
                "resource_type": "test.Resource",
                "identity": "legacy",
                "provider": "mock",
                "current": {
                    "kind": "missing_identifier",
                    "attributes": { "name": "legacy" }
                }
            }
        ],
        "exports": {}
    });

    let migrated = check_and_migrate(&json.to_string()).expect("v17 state must migrate");
    assert_eq!(
        migrated.migration(),
        Some(MigrationInfo {
            from: 17,
            to: StateFile::CURRENT_VERSION,
        })
    );
    let state = finish_legacy_dependency_migration(migrated.into_state());
    assert!(state.resources()[0].nested_write_only().is_empty());
    assert!(state.resources()[1].nested_write_only().is_empty());
    let encoded = serde_json::to_value(&state).expect("migrated state serializes");
    assert_eq!(
        encoded["resources"][0]["current"]["nested_write_only"],
        serde_json::json!({})
    );
    assert_eq!(
        encoded["resources"][1]["current"]["nested_write_only"],
        serde_json::json!({})
    );
}

#[test]
fn v18_migrates_partial_read_detail_to_incomplete_reread_reason() {
    let json = serde_json::json!({
        "version": 18,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "v18-partial-read-lineage",
        "carina_version": "legacy",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "example",
            "provider": "mock",
            "current": {
                "kind": "applied",
                "identifier": "cloud-id",
                "attributes": { "name": "example" },
                "nested_write_only": {},
                "partial_read": {
                    "detail": "legacy partial read",
                    "missing_attributes": ["computed"]
                }
            }
        }],
        "exports": {}
    });

    let migrated = check_and_migrate(&json.to_string()).expect("v18 state must migrate");
    assert_eq!(
        migrated.migration(),
        Some(MigrationInfo {
            from: 18,
            to: StateFile::CURRENT_VERSION,
        })
    );
    let state = finish_legacy_dependency_migration(migrated.into_state());
    let marker = state.resources()[0]
        .partial_read()
        .expect("legacy marker survives migration");
    assert_eq!(
        marker.pending(),
        &PendingReRead::new(ReReadReason::Incomplete {
            detail: "legacy partial read".to_string(),
        })
    );
    assert_eq!(
        marker.missing_attributes(),
        &BTreeSet::from(["computed".to_string()])
    );

    let encoded = serde_json::to_value(&state).expect("migrated state serializes");
    assert_eq!(
        encoded["resources"][0]["current"]["partial_read"]["pending"]["reason"],
        serde_json::json!({
            "kind": "incomplete",
            "detail": "legacy partial read"
        })
    );
}

#[test]
fn v19_state_rejects_partial_read_marker_with_empty_missing_attributes() {
    let json = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "v19-empty-partial-read-lineage",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "example",
            "provider": "mock",
            "current": {
                "kind": "applied",
                "identifier": "cloud-id",
                "attributes": { "name": "example" },
                "nested_write_only": {},
                "partial_read": {
                    "pending": {
                        "reason": {
                            "kind": "incomplete",
                            "detail": "provider omitted every computed attribute"
                        }
                    },
                    "missing_attributes": []
                }
            }
        }],
        "exports": {}
    });

    let message = invalid_state_message(check_and_migrate(&json.to_string()));

    assert!(
        message.contains("partial-read marker must contain at least one missing attribute"),
        "unexpected state-load error: {message}",
    );
}

#[test]
fn v18_empty_partial_read_marker_migrates_to_no_marker() {
    let json = serde_json::json!({
        "version": 18,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "v18-empty-partial-read-lineage",
        "carina_version": "legacy",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "example",
            "provider": "mock",
            "current": {
                "kind": "applied",
                "identifier": "cloud-id",
                "attributes": { "name": "example" },
                "nested_write_only": {},
                "partial_read": {
                    "detail": "legacy empty partial read",
                    "missing_attributes": []
                }
            }
        }],
        "exports": {}
    });

    let migrated = check_and_migrate(&json.to_string())
        .expect("a v18 empty partial-read marker must be dropped during migration");
    assert_eq!(
        migrated.migration(),
        Some(MigrationInfo {
            from: 18,
            to: StateFile::CURRENT_VERSION,
        })
    );
    let state = finish_legacy_dependency_migration(migrated.into_state());
    assert!(
        state.resources()[0].partial_read().is_none(),
        "an empty legacy marker must not survive migration"
    );

    let encoded = serde_json::to_value(state).expect("migrated state serializes");
    assert!(
        encoded["resources"][0]["current"]
            .get("partial_read")
            .is_none(),
        "the dropped marker must not be serialized"
    );
}

#[test]
fn restore_partial_read_markers_shrinks_and_clears_resolved_attributes() {
    let shrinking_id =
        ResourceId::with_provider_identity("mock", "test.Resource", "shrinking", None);
    let clearing_id = ResourceId::with_provider_identity("mock", "test.Resource", "clearing", None);
    let pending = PendingReRead::read_failed("temporary provider read failure");

    let mut shrinking_row =
        ResourceState::try_applied("test.Resource", "shrinking", "mock", "shrinking-id")
            .expect("fixture identifier is non-empty");
    shrinking_row.set_partial_read_for_test(PartialReadMarker::new(
        pending.clone(),
        BTreeSet::from(["resolved".to_string(), "still_missing".to_string()]),
    ));
    let mut clearing_row =
        ResourceState::try_applied("test.Resource", "clearing", "mock", "clearing-id")
            .expect("fixture identifier is non-empty");
    clearing_row.set_partial_read_for_test(PartialReadMarker::new(
        PendingReRead::not_found(),
        BTreeSet::from(["resolved".to_string()]),
    ));

    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(shrinking_row)
        .expect("shrinking fixture row is unique");
    state_file
        .upsert_resource(clearing_row)
        .expect("clearing fixture row is unique");

    let resolved_value = Value::Concrete(ConcreteValue::String("now-known".into()));
    let mut refreshed = HashMap::from([
        (
            shrinking_id.clone(),
            State::existing(
                shrinking_id.clone(),
                HashMap::from([("resolved".to_string(), resolved_value.clone())]),
            )
            .with_identifier("shrinking-id"),
        ),
        (
            clearing_id.clone(),
            State::existing(
                clearing_id.clone(),
                HashMap::from([("resolved".to_string(), resolved_value)]),
            )
            .with_identifier("clearing-id"),
        ),
    ]);

    state_file.restore_partial_read_markers_in(refreshed.iter_mut());

    let marker = refreshed[&shrinking_id]
        .partial_read
        .as_ref()
        .expect("an unresolved attribute keeps the marker");
    assert_eq!(marker.pending(), &pending);
    assert_eq!(
        marker.missing_attributes(),
        &BTreeSet::from(["still_missing".to_string()])
    );
    assert!(
        refreshed[&clearing_id].partial_read.is_none(),
        "resolving every missing attribute must clear the marker"
    );
}

#[test]
fn v18_applied_row_without_nested_write_only_is_rejected() {
    let json = serde_json::json!({
        "version": 18,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "v18-lineage",
        "carina_version": "current",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "example",
            "provider": "mock",
            "current": {
                "kind": "applied",
                "identifier": "cloud-id",
                "attributes": { "name": "example" }
            }
        }],
        "exports": {}
    });

    let error = check_and_migrate(&json.to_string())
        .expect_err("v18 must require explicit nested write-only provenance");

    assert!(
        error.to_string().contains("nested_write_only"),
        "decode error must name the missing required field: {error}"
    );
}

#[test]
fn v15_in_flight_row_has_stable_golden_serialization() {
    let json = serde_json::json!({
        "version": 15,
        "serial": 4,
        "lineage": "golden-lineage",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "pending",
            "provider": "mock",
            "current": {
                "kind": "create_in_flight",
                "envelope": {
                    "token": "opaque-token",
                    "identifier": null,
                    "write_only": {},
                    "secrets": {}
                },
                "started_at": "2026-10-23T08:00:00Z"
            }
        }],
        "exports": {}
    });
    let state = check_and_migrate(&json.to_string())
        .expect("golden v15 state must load")
        .into_state();

    assert_eq!(
        serde_json::to_string(&state.resources()[0]).expect("v15 row serializes"),
        r#"{"resource_type":"test.Resource","identity":"pending","provider":"mock","current":{"kind":"create_in_flight","envelope":{"token":"opaque-token","identifier":null,"write_only":{},"secrets":{}},"started_at":"2026-10-23T08:00:00Z"},"protected":false,"directives":{"force_delete":false,"create_before_destroy":false,"prevent_destroy":{"kind":"unspecified"}},"prefixes":{},"name_overrides":{},"binding":null,"dependency_bindings":[]}"#
    );
}

#[test]
fn v14_to_current_preserves_write_only_partial_and_deposed_fields() {
    let json = serde_json::json!({
        "version": 14,
        "serial": 2,
        "lineage": "legacy",
        "carina_version": "legacy",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "main",
            "provider": "mock",
            "identifier": "cloud-id",
            "attributes": { "name": "example", "password": "masked" },
            "write_only_attributes": ["password"],
            "partial_read": {
                "detail": "provider read omitted status",
                "missing_attributes": ["status"]
            },
            "deposed": [{
                "key": "00000000-0000-4000-8000-000000000002",
                "identifier": "old-id",
                "attributes": { "name": "old" },
                "dependency_bindings": []
            }]
        }],
        "exports": {}
    });

    let migrated = check_and_migrate(&json.to_string()).expect("v14 state must migrate");
    assert_eq!(
        migrated.migration(),
        Some(MigrationInfo {
            from: 14,
            to: StateFile::CURRENT_VERSION,
        })
    );
    let state = migrated.into_state();
    let CurrentInstance::Applied {
        identifier,
        attributes,
        write_only_attributes,
        nested_write_only,
        partial_read,
    } = state.resources()[0].current()
    else {
        panic!("v14 applied row must migrate to CurrentInstance::Applied");
    };
    assert_eq!(identifier.as_str(), "cloud-id");
    assert!(attributes.contains_key("password"));
    assert_eq!(write_only_attributes, &["password"]);
    assert!(nested_write_only.is_empty());
    assert!(partial_read.is_some());
    assert_eq!(state.resources()[0].deposed.len(), 1);
}

const LEGACY_SECRET_PREFIX: &str = "_secret:argon2:";

fn persisted_attributes_from_state_json(
    attributes: &HashMap<String, serde_json::Value>,
) -> PersistedAttributes {
    persisted_attributes_from_test_json(attributes.clone())
}

fn state_json_from_persisted_attributes(
    attributes: &PersistedAttributes,
) -> HashMap<String, serde_json::Value> {
    serde_json::from_value(
        serde_json::to_value(attributes).expect("persisted attributes are serializable"),
    )
    .expect("persisted attributes serialize as a JSON object")
}

fn persisted_json_attribute(
    attributes: &PersistedAttributes,
    key: &str,
) -> Option<serde_json::Value> {
    state_json_from_persisted_attributes(attributes).remove(key)
}

fn structural_legacy_secret(hash_hex: &str) -> serde_json::Value {
    serde_json::json!({
        carina_core::value::PERSISTED_SECRET_KEY:
            format!("argon2-legacy:73616c74:{hash_hex}"),
    })
}

fn structural_secret_digest(value: &serde_json::Value) -> &str {
    let object = value.as_object().expect("secret is a structural object");
    assert_eq!(object.len(), 1, "secret tag must have exactly one key");
    let digest = object
        .get(carina_core::value::PERSISTED_SECRET_KEY)
        .and_then(serde_json::Value::as_str)
        .expect("secret tag carries a digest");
    assert!(
        carina_core::resource::RedactedSecretHash::from_encoded_digest(digest).is_some(),
        "invalid secret digest",
    );
    digest
}

fn valid_legacy_marker_shaped_string(plaintext: &str) -> String {
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String(plaintext.into()),
    ))));
    let encoded = PersistedValue::encode_provider_value(&secret, None)
        .expect("test secret must be persistable");
    format!(
        "_secret:{}",
        encoded
            .as_json()
            .as_object()
            .and_then(|tag| tag.get(carina_core::value::PERSISTED_SECRET_KEY))
            .and_then(serde_json::Value::as_str)
            .expect("structural test secret carries a digest")
    )
}

fn resolved(resource: &carina_core::resource::Resource) -> carina_core::resource::ResolvedResource {
    carina_core::resource::ResolvedResource::collect_resolved([resource.clone()])
        .expect("fixture identity resolves")
        .remove(0)
}

fn list_rule(port: i64, description: Option<&str>) -> Value {
    let mut fields = IndexMap::from([(
        "port".to_string(),
        Value::Concrete(ConcreteValue::Int(port)),
    )]);
    if let Some(description) = description {
        fields.insert(
            "description".to_string(),
            Value::Concrete(ConcreteValue::String(description.to_string().into())),
        );
    }
    Value::Concrete(ConcreteValue::Map(fields))
}

fn list_rule_with_tag(port: i64, description: Option<&str>, tag: Option<&str>) -> Value {
    let Value::Concrete(ConcreteValue::Map(mut fields)) = list_rule(port, description) else {
        unreachable!("list_rule always returns a map");
    };
    if let Some(tag) = tag {
        fields.insert(
            "tag".to_string(),
            Value::Concrete(ConcreteValue::String(tag.to_string().into())),
        );
    }
    Value::Concrete(ConcreteValue::Map(fields))
}

fn with_provider_status(mut rule: Value) -> Value {
    let Value::Concrete(ConcreteValue::Map(fields)) = &mut rule else {
        unreachable!("test rule is a map");
    };
    fields.insert(
        "provider_status".to_string(),
        Value::Concrete(ConcreteValue::String("ready".into())),
    );
    rule
}

fn carried_rules_fixture(
    applied_rules: Vec<Value>,
    desired_rules: Vec<Value>,
    fresh_rules: Vec<Value>,
) -> (Resource, State, ResourceState) {
    let applied = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(applied_rules.clone())),
        );
    let prior_provider = State::existing(
        applied.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(applied_rules)),
        )]),
    )
    .with_identifier("listener-id");
    let prior = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&applied),
        &prior_provider,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .expect("initial applied row");

    let desired = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute("rules", Value::Concrete(ConcreteValue::List(desired_rules)));
    let fresh = State::existing(
        desired.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(fresh_rules)),
        )]),
    )
    .with_identifier("listener-id");
    let carried = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&desired),
        &fresh,
        Some(&prior),
        None,
        RowBasis::Carried,
        None,
    )
    .expect("carried row");

    (desired, fresh, carried)
}

fn assert_explicit_struct_keys(explicit: &ExplicitFields, expected: &[&str]) {
    let ExplicitFields::Struct { children } = explicit else {
        panic!("expected Struct, got {explicit:?}");
    };
    assert_eq!(children.len(), expected.len());
    for key in expected {
        assert!(children.contains_key(*key), "missing authored key {key}");
    }
}

fn whole_value(paths: &[&[&str]]) -> ExplicitFields {
    ExplicitFields::WholeValue {
        pending: carina_core::explicit::PendingPaths::try_new(
            paths
                .iter()
                .map(|path| path.iter().map(|segment| (*segment).to_string()).collect()),
        )
        .expect("test pending paths are non-empty and valid"),
    }
}

#[test]
fn test_state_file_new() {
    let state = StateFile::new();
    assert_eq!(state.version, StateFile::CURRENT_VERSION);
    assert_eq!(StateFile::CURRENT_VERSION, 21);
    assert_eq!(state.serial, 0);
    assert!(!state.lineage.is_empty());
    assert!(state.resources.is_empty());
}

#[test]
fn v15_state_round_trip_keeps_user_and_provider_marker_strings_plain() {
    use carina_core::resource::{Resource, State as ProviderState};

    assert_eq!(StateFile::CURRENT_VERSION, 21);
    let marker = valid_legacy_marker_shaped_string("state-marker-collision");
    let plain = Value::Concrete(ConcreteValue::String(marker.clone().into()));
    let resource = Resource::with_provider("mock", "test.Resource", "main", None)
        .with_attribute("authored", plain.clone());
    let provider_state = ProviderState::existing(
        resource.id.clone(),
        HashMap::from([
            ("authored".to_string(), plain.clone()),
            ("provider_only".to_string(), plain.clone()),
        ]),
    )
    .with_identifier("provider-id");
    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .expect("marker-shaped plaintext is valid provider state");
    let mut state = StateFile::new();
    state.upsert_resource(row).expect("insert test row");

    let json = serde_json::to_string(&state).expect("serialize current state");
    let reloaded = check_and_migrate(&json)
        .expect("reload current state")
        .into_state();
    let loaded = match reloaded.build_state_for_resource(&resource.id) {
        Current::Present(present) => present.state().clone(),
        Current::Absent(state) => panic!("expected present state, got {state:?}"),
        Current::CreateInFlight(_pending) => panic!("fixture cannot be in flight"),
        Current::CreateOutcomeUnknown(_unknown) => {
            panic!("fixture cannot have an unknown create outcome")
        }
        Current::MissingIdentifier(state) => {
            panic!("expected valid provider identifier, got {state:?}")
        }
    };

    assert_eq!(loaded.attributes.get("authored"), Some(&plain));
    assert_eq!(loaded.attributes.get("provider_only"), Some(&plain));
    let raw: serde_json::Value = serde_json::from_str(&json).expect("state JSON");
    assert_eq!(
        raw["resources"][0]["current"]["attributes"]["authored"],
        marker
    );
    assert_eq!(
        raw["resources"][0]["current"]["attributes"]["provider_only"],
        marker
    );
}

#[test]
fn test_state_file_increment_serial() {
    let mut state = StateFile::new();
    assert_eq!(state.serial, 0);
    state.increment_serial();
    assert_eq!(state.serial, 1);
    state.increment_serial();
    assert_eq!(state.serial, 2);
}

#[test]
fn resource_state_identity_is_typed_and_serializes_as_a_plain_string() {
    let resource = ResourceState::new("s3.Bucket", "main", "aws");
    let identity: &ResourceIdentity = &resource.identity;

    assert_eq!(identity.as_str(), "main");
    assert_eq!(
        serde_json::to_value(&resource).expect("resource state must serialize")["identity"],
        serde_json::json!("main")
    );
}

#[test]
fn release_list_keys_only_demotes_matching_authoring_entries() {
    use carina_core::effect::{Effect, ReleasedListKey, ReleasedListKeys};
    use carina_core::schema::{AttributeSchema, AttributeType, StructField};

    let schema = ResourceSchema::new("test.KeyedSettings")
        .attribute(
            AttributeSchema::optional(
                "settings",
                AttributeType::list(AttributeType::struct_(
                    "Setting",
                    vec![
                        StructField::required("key", AttributeType::string()).build(),
                        StructField::required("value", AttributeType::string()).build(),
                    ],
                )),
            )
            .keyed_partial_update("key")
            .build(),
        )
        .validate()
        .expect("release fixture schema is valid");
    let key_field = schema
        .effective_attribute("settings")
        .and_then(|attribute| attribute.keyed_partial_update())
        .expect("release fixture has keyed semantics")
        .key_field()
        .clone();
    let stored = serde_json::json!([
        { "key": "a", "value": "1" },
        { "key": "b", "value": "2" },
        { "key": "service-default", "value": "on" }
    ]);
    let mut row =
        ResourceState::try_applied("test.KeyedSettings", "settings", "mock", "provider-id")
            .expect("fixture identifier is valid")
            .with_attribute("settings", stored.clone());
    let authored = || ExplicitFields::Struct {
        children: HashMap::from([
            ("key".to_string(), ExplicitFields::Leaf),
            ("value".to_string(), ExplicitFields::Leaf),
        ]),
    };
    row.explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "settings".to_string(),
            ExplicitFields::ListElements {
                elements: vec![authored(), authored(), ExplicitFields::Unrecorded],
            },
        )]),
    };
    let mut state = StateFile::new();
    state.upsert_resource(row).expect("insert release fixture");
    let effect: Effect = Effect::ReleaseListKeys {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "mock",
            "test.KeyedSettings",
            "settings",
            None,
        )),
        attribute: "settings".to_string(),
        key_field: key_field.clone(),
        keys: ReleasedListKeys::try_new(vec![ReleasedListKey::new(
            "b",
            Value::Concrete(ConcreteValue::String("2".into())),
        )])
        .expect("release fixture has one key"),
    };

    state
        .release_list_keys(
            effect.as_release_list_keys().expect("release capability"),
            &key_field,
        )
        .expect("release authoring metadata");

    let row = state
        .find_resource("mock", "test.KeyedSettings", "settings")
        .expect("released row remains in state");
    assert_eq!(
        persisted_json_attribute(
            row.persisted_attributes()
                .expect("released row keeps provider attributes"),
            "settings",
        ),
        Some(stored),
        "release must not change provider-observed list values",
    );
    let ExplicitFields::Struct { children } = &row.explicit else {
        panic!("released row authoring root must remain a struct");
    };
    let ExplicitFields::ListElements { elements } = &children["settings"] else {
        panic!("released list authoring must remain index-aligned");
    };
    assert!(matches!(elements[0], ExplicitFields::Struct { .. }));
    assert!(matches!(elements[1], ExplicitFields::Unrecorded));
    assert!(matches!(elements[2], ExplicitFields::Unrecorded));
}

#[test]
fn test_state_file_upsert_resource() {
    let mut state = StateFile::new();

    let resource1 = ResourceState::new("s3.Bucket", "my-bucket", "aws")
        .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"));

    state
        .upsert_resource(resource1)
        .expect("test state setup must be valid");
    assert_eq!(state.resources.len(), 1);

    // Update the same resource
    let resource2 = ResourceState::new("s3.Bucket", "my-bucket", "aws")
        .with_attribute("region".to_string(), serde_json::json!("us-west-2"));

    state
        .upsert_resource(resource2)
        .expect("test state setup must be valid");
    assert_eq!(state.resources.len(), 1);
    assert_eq!(
        persisted_json_attribute(
            state.resources[0]
                .persisted_attributes()
                .expect("fixture has persisted attributes"),
            "region",
        ),
        Some(serde_json::json!("us-west-2"))
    );
}

#[test]
fn upsert_resource_returns_error_for_duplicate_deposed_key() {
    let mut state = StateFile::new();
    let duplicate_key = DeposedKey::new_unique();
    let mut resource = ResourceState::new("ec2.Vpc", "main", "aws");
    resource.deposed = vec![
        DeposedInstance {
            key: duplicate_key.clone(),
            identifier: "vpc-old-a".to_string(),
            provider_instance: Some("west".to_string()),
            attributes: PersistedAttributes::default(),
            dependency_bindings: Default::default(),
        },
        DeposedInstance {
            key: duplicate_key,
            identifier: "vpc-old-b".to_string(),
            provider_instance: Some("east".to_string()),
            attributes: PersistedAttributes::default(),
            dependency_bindings: Default::default(),
        },
    ];

    let error = state
        .upsert_resource(resource)
        .expect_err("duplicate deposed keys must be rejected");

    assert!(
        matches!(error, BackendError::InvalidState(message) if message.contains("duplicate deposed generation key"))
    );
    assert!(state.resources().is_empty());
}

#[test]
fn upsert_deposed_generation_returns_error_when_key_and_identity_cross_generations() {
    let mut state = StateFile::new();
    let first_key = DeposedKey::new_unique();
    let second_key = DeposedKey::new_unique();
    let mut resource = ResourceState::new("ec2.Vpc", "main", "aws");
    resource.deposed = vec![
        DeposedInstance {
            key: first_key.clone(),
            identifier: "vpc-old-a".to_string(),
            provider_instance: Some("west".to_string()),
            attributes: PersistedAttributes::default(),
            dependency_bindings: Default::default(),
        },
        DeposedInstance {
            key: second_key,
            identifier: "vpc-old-b".to_string(),
            provider_instance: Some("east".to_string()),
            attributes: PersistedAttributes::default(),
            dependency_bindings: Default::default(),
        },
    ];
    state
        .upsert_resource(resource)
        .expect("setup generations are unique on both axes");

    let identity = ResourceIdentity::from("main");
    let error = state
        .upsert_deposed_generation(
            "aws",
            "ec2.Vpc",
            &identity,
            None,
            DeposedInstance {
                key: first_key,
                identifier: "vpc-old-b".to_string(),
                provider_instance: Some("east".to_string()),
                attributes: PersistedAttributes::default(),
                dependency_bindings: Default::default(),
            },
        )
        .expect_err("key and identity cannot select different generations");

    assert!(
        matches!(error, BackendError::InvalidState(message) if message.contains("match different existing generations"))
    );
    let row = state
        .find_resource("aws", "ec2.Vpc", "main")
        .expect("failed upsert must preserve the existing row");
    assert_eq!(row.deposed.len(), 2);
}

#[test]
fn upsert_resource_preserves_existing_deposed_generations() {
    let mut state = StateFile::new();
    let mut existing = ResourceState::try_applied("ec2.Vpc", "main", "aws", "vpc-old")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
    let deposed_key = DeposedKey::new_unique();
    existing.deposed.push(DeposedInstance {
        key: deposed_key.clone(),
        identifier: "vpc-older".to_string(),
        provider_instance: Some("west".to_string()),
        attributes: persisted_attributes_from_state_json(&HashMap::from([(
            "cidr_block".to_string(),
            serde_json::json!("10.255.0.0/16"),
        )])),
        dependency_bindings: fixture_persisted_dependencies(["igw"]),
    });
    state
        .upsert_resource(existing)
        .expect("test state setup must be valid");

    let incoming = ResourceState::try_applied("ec2.Vpc", "main", "aws", "vpc-new")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.1.0.0/16"));
    state
        .upsert_resource(incoming)
        .expect("test state setup must be valid");

    let row = state
        .find_resource("aws", "ec2.Vpc", "main")
        .expect("resource should still exist");
    assert_eq!(
        row.identifier().map(AppliedIdentifier::as_str),
        Some("vpc-new")
    );
    assert_eq!(
        persisted_json_attribute(
            row.persisted_attributes()
                .expect("fixture has persisted attributes"),
            "cidr_block",
        ),
        Some(serde_json::json!("10.1.0.0/16"))
    );
    assert_eq!(row.deposed.len(), 1);
    assert_eq!(row.deposed[0].key, deposed_key);
    assert_eq!(row.deposed[0].identifier, "vpc-older");
    assert_eq!(row.deposed[0].provider_instance.as_deref(), Some("west"));
}

#[test]
fn upsert_resource_drops_deposed_generation_matching_current_identifier() {
    let mut state = StateFile::new();
    let retained_key = DeposedKey::new_unique();
    let mut existing = ResourceState::try_applied("ec2.Vpc", "main", "aws", "vpc-current")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
    existing.deposed.push(DeposedInstance {
        key: DeposedKey::new_unique(),
        identifier: "vpc-reused".to_string(),
        provider_instance: Some("west".to_string()),
        attributes: persisted_attributes_from_state_json(&HashMap::from([(
            "cidr_block".to_string(),
            serde_json::json!("10.2.0.0/16"),
        )])),
        dependency_bindings: Default::default(),
    });
    existing.deposed.push(DeposedInstance {
        key: retained_key.clone(),
        identifier: "vpc-older".to_string(),
        provider_instance: Some("west".to_string()),
        attributes: persisted_attributes_from_state_json(&HashMap::from([(
            "cidr_block".to_string(),
            serde_json::json!("10.255.0.0/16"),
        )])),
        dependency_bindings: fixture_persisted_dependencies(["igw"]),
    });
    state
        .upsert_resource(existing)
        .expect("test state setup must be valid");

    let mut incoming = ResourceState::try_applied("ec2.Vpc", "main", "aws", "vpc-reused")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.2.0.0/16"));
    incoming.directives.provider_instance = Some("west".to_string());
    state
        .upsert_resource(incoming)
        .expect("test state setup must be valid");

    let row = state
        .find_resource("aws", "ec2.Vpc", "main")
        .expect("resource should still exist");
    assert_eq!(
        row.identifier().map(AppliedIdentifier::as_str),
        Some("vpc-reused")
    );
    assert_eq!(row.deposed.len(), 1);
    assert_eq!(row.deposed[0].key, retained_key);
    assert_eq!(row.deposed[0].identifier, "vpc-older");
}

#[test]
fn upsert_resource_keeps_deposed_generation_with_same_identifier_on_other_provider_instance() {
    let mut state = StateFile::new();
    let retained_key = DeposedKey::new_unique();
    let mut existing = ResourceState::try_applied("ec2.Vpc", "main", "aws", "vpc-current")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.0.0.0/16"));
    existing.deposed.push(DeposedInstance {
        key: retained_key.clone(),
        identifier: "vpc-reused".to_string(),
        provider_instance: Some("west".to_string()),
        attributes: persisted_attributes_from_state_json(&HashMap::from([(
            "cidr_block".to_string(),
            serde_json::json!("10.2.0.0/16"),
        )])),
        dependency_bindings: Default::default(),
    });
    state
        .upsert_resource(existing)
        .expect("test state setup must be valid");

    let mut incoming = ResourceState::try_applied("ec2.Vpc", "main", "aws", "vpc-reused")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.2.0.0/16"));
    incoming.directives.provider_instance = Some("east".to_string());
    state
        .upsert_resource(incoming)
        .expect("test state setup must be valid");

    let row = state
        .find_resource("aws", "ec2.Vpc", "main")
        .expect("resource should still exist");
    assert_eq!(
        row.identifier().map(AppliedIdentifier::as_str),
        Some("vpc-reused")
    );
    assert_eq!(row.directives.provider_instance.as_deref(), Some("east"));
    assert_eq!(row.deposed.len(), 1);
    assert_eq!(row.deposed[0].key, retained_key);
    assert_eq!(row.deposed[0].identifier, "vpc-reused");
    assert_eq!(row.deposed[0].provider_instance.as_deref(), Some("west"));
}

#[test]
fn test_state_file_remove_resource() {
    let mut state = StateFile::new();

    let resource = ResourceState::new("s3.Bucket", "my-bucket", "aws");
    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");
    assert_eq!(state.resources.len(), 1);

    let removed = state.remove_resource("aws", "s3.Bucket", "my-bucket");
    assert!(removed.is_some());
    assert_eq!(state.resources.len(), 0);

    // Removing non-existent resource returns None
    let removed = state.remove_resource("aws", "s3.Bucket", "other-bucket");
    assert!(removed.is_none());
}

#[test]
fn remove_resource_clears_current_instance_and_keeps_deposed_generations() {
    let mut state = StateFile::new();
    let mut resource = ResourceState::try_applied("ec2.Vpc", "main", "aws", "vpc-new")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block", serde_json::json!("10.1.0.0/16"));
    resource.protected = true;
    resource.directives.create_before_destroy = true;
    resource.directives.provider_instance = Some("west".to_string());
    resource
        .prefixes
        .insert("name".to_string(), "main-".to_string());
    resource.name_overrides.insert(
        "name".to_string(),
        NameOverride {
            temp_value: "main-cbd".to_string(),
            original_value: Some("main".to_string()),
        },
    );
    resource.explicit = ExplicitFields::Struct {
        children: HashMap::from([("cidr_block".to_string(), ExplicitFields::Leaf)]),
    };
    resource.binding = Some("vpc".to_string());
    resource.dependency_bindings =
        PersistedDestroyDependencies::from_test_dependencies([resolved_dependency(
            "mock",
            "test.Dependency",
            "internet",
            None,
        )]);
    resource.set_write_only_attributes_for_test(vec!["token".to_string()]);
    resource.set_partial_read_for_test(PartialReadMarker::new(
        PendingReRead::incomplete("partial"),
        BTreeSet::from(["token".to_string()]),
    ));
    resource.deposed.push(DeposedInstance {
        key: DeposedKey::new_unique(),
        identifier: "vpc-old".to_string(),
        provider_instance: Some("west".to_string()),
        attributes: persisted_attributes_from_state_json(&HashMap::from([(
            "cidr_block".to_string(),
            serde_json::json!("10.0.0.0/16"),
        )])),
        dependency_bindings: fixture_persisted_dependencies(["network"]),
    });
    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");

    let removed = state.remove_resource("aws", "ec2.Vpc", "main");

    assert_eq!(
        removed.and_then(|rs| rs.identifier().map(ToString::to_string)),
        Some("vpc-new".to_string())
    );
    let retained = state
        .find_resource("aws", "ec2.Vpc", "main")
        .expect("row with deposed generations should be retained");
    assert_eq!(retained.identifier(), None);
    assert!(retained.persisted_attributes().is_none());
    assert_eq!(retained.deposed.len(), 1);
    assert_eq!(retained.deposed[0].identifier, "vpc-old");
    assert_eq!(
        retained.deposed[0].provider_instance.as_deref(),
        Some("west")
    );
    assert!(!retained.protected);
    let expected_directives = PersistedDirectives::from_resolved(
        &carina_core::resource::Directives::default().with_provider_instance("west"),
        PersistedDestroyDependencies::default(),
    );
    assert_eq!(retained.directives, expected_directives);
    assert_eq!(
        StateFile::id_for_resource_state(retained),
        ResourceId::with_provider_identity("aws", "ec2.Vpc", "main", Some("west".to_string()))
    );
    assert!(retained.prefixes.is_empty());
    assert!(retained.name_overrides.is_empty());
    assert_eq!(retained.explicit, ExplicitFields::default());
    assert_eq!(retained.binding, None);
    assert!(retained.dependency_bindings.is_empty());
    assert!(retained.write_only_attributes().is_empty());
    assert_eq!(retained.partial_read(), None);
}

#[test]
fn test_resource_state_protected() {
    let resource = ResourceState::new("s3.Bucket", "state-bucket", "aws").with_protected(true);
    assert!(resource.protected);
}

#[test]
fn test_resource_state_managed_state_bucket_shape() {
    // The seed must use the desired resource's resolved anonymous identifier
    // as `identity` (so the differ matches the seed against the desired resource)
    // AND set the AWS bucket name as `identifier`. Conflating the two
    // reproduces #2533: phantom Delete on the seed's identity plus phantom
    // Create on the desired's identity.
    let resource = ResourceState::managed_state_bucket(
        "aws",
        "s3.Bucket",
        "aws_s3_bucket_a3f2b1c8",
        AppliedIdentifier::new("my-state-bucket").unwrap(),
    )
    .unwrap();
    assert_eq!(resource.provider, "aws");
    assert_eq!(resource.resource_type, "s3.Bucket");
    assert_eq!(
        resource.identity.as_str(),
        "aws_s3_bucket_a3f2b1c8",
        "identity must match the desired resource's anonymous identifier"
    );
    assert_eq!(
        resource.identifier().map(AppliedIdentifier::as_str),
        Some("my-state-bucket"),
        "identifier must be the AWS bucket name so the provider can Read/Update it"
    );
    assert!(resource.protected);
    assert_eq!(
        persisted_json_attribute(
            resource
                .persisted_attributes()
                .expect("managed bucket has persisted attributes"),
            "bucket",
        ),
        Some(serde_json::json!("my-state-bucket"))
    );
}

#[test]
fn test_state_file_with_managed_state_bucket_contains_one_resource() {
    let state = StateFile::with_managed_state_bucket(
        "aws",
        "s3.Bucket",
        "aws_s3_bucket_a3f2b1c8",
        AppliedIdentifier::new("my-state-bucket").unwrap(),
    )
    .unwrap();
    assert_eq!(state.resources.len(), 1);
    let bucket = &state.resources[0];
    assert_eq!(bucket.identity.as_str(), "aws_s3_bucket_a3f2b1c8");
    assert_eq!(
        bucket.identifier().map(AppliedIdentifier::as_str),
        Some("my-state-bucket")
    );
    assert!(bucket.protected);
}

#[test]
fn test_state_file_serialization() {
    let mut state = StateFile::new();
    let resource = ResourceState::new("s3.Bucket", "my-bucket", "aws")
        .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"))
        .with_attribute("versioning".to_string(), serde_json::json!("Enabled"));

    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");

    let json = serde_json::to_string_pretty(&state).unwrap();
    let deserialized = StateFile::from_json_str(&json).unwrap();

    assert_eq!(deserialized.version, state.version);
    assert_eq!(deserialized.serial, state.serial);
    assert_eq!(deserialized.lineage, state.lineage);
    assert_eq!(deserialized.resources.len(), 1);
}

#[test]
fn test_resource_state_prefixes_serialization() {
    let mut resource = ResourceState::new("s3.Bucket", "test-bucket", "awscc").with_attribute(
        "bucket_name".to_string(),
        serde_json::json!("my-app-abcd1234"),
    );
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());

    let json = serde_json::to_string_pretty(&resource).unwrap();
    let deserialized: ResourceState = serde_json::from_str(&json).unwrap();

    assert_eq!(
        deserialized.prefixes.get("bucket_name"),
        Some(&"my-app-".to_string())
    );
}

#[test]
fn name_override_deserializes_legacy_bare_string_to_original_none() {
    let deserialized: NameOverride = serde_json::from_str(r#""foo""#).unwrap();

    assert_eq!(
        deserialized,
        NameOverride {
            temp_value: "foo".to_string(),
            original_value: None,
        }
    );
}

#[test]
fn name_override_deserializes_struct_form() {
    let original = NameOverride {
        temp_value: "foo-cbd".to_string(),
        original_value: Some("foo".to_string()),
    };

    let json = serde_json::to_string(&original).unwrap();
    let deserialized: NameOverride = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized, original);
}

#[test]
fn state_file_has_legacy_name_overrides_detects_v7_shape() {
    let legacy_json = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 0,
        "lineage": "lineage",
        "carina_version": "test",
        "resources": [
            {
                "resource_type": "test.resource",
                "identity": "legacy",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "legacy-id",
                    "attributes": {},
                    "nested_write_only": {}
                },
                "name_overrides": {
                    "name": "legacy-temp"
                }
            },
            {
                "resource_type": "test.resource",
                "identity": "current",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "current-id",
                    "attributes": {},
                    "nested_write_only": {}
                },
                "name_overrides": {
                    "name": {
                        "temp_value": "current-temp",
                        "original_value": "current"
                    }
                }
            }
        ]
    });
    let state = StateFile::from_json_str(&legacy_json.to_string()).unwrap();

    assert!(state.has_legacy_name_overrides());
    let affected = state.legacy_name_override_resources();
    assert_eq!(affected.len(), 1);
    assert_eq!(affected[0].identity.as_str(), "legacy");

    let typed_only_json = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 0,
        "lineage": "lineage",
        "carina_version": "test",
        "resources": [
            {
                "resource_type": "test.resource",
                "identity": "current",
                "provider": "mock",
                "current": {
                    "kind": "applied",
                    "identifier": "current-id",
                    "attributes": {},
                    "nested_write_only": {}
                },
                "name_overrides": {
                    "name": {
                        "temp_value": "current-temp",
                        "original_value": "current"
                    }
                }
            }
        ]
    });
    let typed_only = StateFile::from_json_str(&typed_only_json.to_string()).unwrap();
    assert!(!typed_only.has_legacy_name_overrides());
    assert!(typed_only.legacy_name_override_resources().is_empty());
}

#[test]
fn should_apply_override_returns_apply_when_dsl_matches_recorded() {
    let override_ = NameOverride {
        temp_value: "foo-cbd".to_string(),
        original_value: Some("foo".to_string()),
    };

    assert_eq!(
        should_apply_override(Some("foo"), &override_),
        ApplyDecision::Apply
    );
}

#[test]
fn should_apply_override_returns_skip_when_dsl_diverges() {
    let override_ = NameOverride {
        temp_value: "foo-cbd".to_string(),
        original_value: Some("foo".to_string()),
    };

    assert_eq!(
        should_apply_override(Some("bar"), &override_),
        ApplyDecision::Skip
    );
}

#[test]
fn should_apply_override_returns_apply_with_unknown_dsl_when_dsl_unresolved() {
    let override_ = NameOverride {
        temp_value: "foo-cbd".to_string(),
        original_value: Some("foo".to_string()),
    };

    assert_eq!(
        should_apply_override(None, &override_),
        ApplyDecision::ApplyWithUnknownDsl
    );
}

#[test]
fn should_apply_override_returns_apply_legacy_for_pre_phase_5_state() {
    let override_ = NameOverride {
        temp_value: "foo-cbd".to_string(),
        original_value: None,
    };

    assert_eq!(
        should_apply_override(Some("renamed"), &override_),
        ApplyDecision::ApplyLegacy
    );
}

#[test]
fn test_build_state_for_resource_carries_typed_identifier() {
    use carina_core::resource::Resource;

    let mut state = StateFile::new();
    let rs = ResourceState::try_applied("s3.Bucket", "my-bucket", "awscc", "my-bucket-abcd1234")
        .expect("fixture identifier is non-empty");
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    let loaded = state.build_state_for_resource(&resource.id);
    let Current::Present(present) = loaded else {
        panic!("state row must load as present");
    };
    assert_eq!(present.identifier().as_str(), "my-bucket-abcd1234");
}

#[test]
fn test_build_state_for_resource_distinguishes_absent_address() {
    use carina_core::resource::Resource;

    let state = StateFile::new();
    let resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    assert!(matches!(
        state.build_state_for_resource(&resource.id),
        Current::Absent(_state)
    ));
}

#[test]
fn test_build_directives() {
    use carina_core::resource::ResourceId;

    let mut state = StateFile::new();
    let mut rs = ResourceState::new("s3.Bucket", "my-bucket", "awscc");
    rs.directives.force_delete = true;
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let directives_map = state.build_directives();
    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "my-bucket", None);
    assert!(directives_map.get(&id).unwrap().force_delete);
}

#[test]
fn test_build_saved_attrs() {
    use carina_core::resource::{ConcreteValue, ResourceId, Value};

    let mut state = StateFile::new();
    let rs = ResourceState::new("s3.Bucket", "my-bucket", "awscc")
        .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"));
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let saved = state.build_saved_attrs();
    let id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "my-bucket", None);
    let attrs = saved.get(&id).unwrap();
    assert_eq!(
        attrs.get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "ap-northeast-1".into()
        )))
    );
}

#[test]
fn test_resource_state_serialization_with_binding_and_deps() {
    let dependencies = PersistedDestroyDependencies::from_test_dependencies([
        resolved_dependency("aws", "ec2.Vpc", "vpc", None),
        resolved_dependency("aws", "ec2.Subnet", "subnet", None),
    ]);
    let mut row = ResourceState::new("s3.Bucket", "my-bucket", "aws");
    row.binding = Some("my_bucket".to_string());
    row.dependency_bindings = dependencies.clone();
    let json = serde_json::to_string(&row).expect("current resource state serializes");
    let deserialized: ResourceState = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.identity.as_str(), "my-bucket");
    assert_eq!(deserialized.binding, Some("my_bucket".to_string()));
    assert_eq!(deserialized.dependency_bindings, dependencies);
}

#[test]
fn test_resource_state_deserialization_without_v3_fields() {
    // v2 state files don't have binding or dependency_bindings fields
    let json = r#"{
        "resource_type": "s3.Bucket",
        "name": "my-bucket",
        "provider": "aws",
        "current": {
            "kind": "missing_identifier",
            "attributes": {"region": "ap-northeast-1"},
            "nested_write_only": {}
        },
        "protected": false,
        "directives": {},
        "prefixes": {},
        "name_overrides": {},
        "desired_keys": []
    }"#;

    let deserialized: ResourceState = serde_json::from_str(json).unwrap();
    assert_eq!(deserialized.identity.as_str(), "my-bucket");
    assert_eq!(deserialized.binding, None);
    assert!(deserialized.dependency_bindings.is_empty());
    assert!(deserialized.write_only_attributes().is_empty());
}

#[test]
fn resource_state_name_overrides_migrates_from_v7_bare_string() {
    let json = r#"{
        "resource_type": "s3.Bucket",
        "identity": "my-bucket",
        "provider": "aws",
        "current": {
            "kind": "missing_identifier",
            "attributes": {"bucket": "old-name"},
            "nested_write_only": {}
        },
        "protected": false,
        "directives": {},
        "prefixes": {},
        "name_overrides": {"bucket": "old-name-cbd"},
        "binding": "my_bucket",
        "dependency_bindings": []
    }"#;

    let deserialized: ResourceState = serde_json::from_str(json).unwrap();

    assert_eq!(
        deserialized.name_overrides.get("bucket"),
        Some(&NameOverride {
            temp_value: "old-name-cbd".to_string(),
            original_value: None,
        })
    );
}

#[test]
fn test_from_provider_state() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let mut resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    resource.directives.force_delete = true;
    resource
        .prefixes
        .insert("bucket_name".to_string(), "my-app-".to_string());

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("my-bucket-abcd1234".to_string()),
        attributes: [(
            "region".to_string(),
            Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let existing = ResourceState::new("s3.Bucket", "my-bucket", "awscc").with_protected(true);

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();

    assert_eq!(
        rs.identifier().map(ToString::to_string),
        Some("my-bucket-abcd1234".to_string())
    );
    assert_eq!(
        persisted_json_attribute(
            rs.persisted_attributes()
                .expect("provider row has persisted attributes"),
            "region",
        ),
        Some(serde_json::json!("ap-northeast-1"))
    );
    assert!(rs.protected);
    assert!(rs.directives.force_delete);
    assert_eq!(rs.prefixes.get("bucket_name"), Some(&"my-app-".to_string()));
}

#[test]
fn test_from_provider_state_without_existing() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let resource = Resource::with_provider("aws", "s3.Bucket", "test", None);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("test-id".to_string()),
        attributes: [(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();
    assert!(!rs.protected);
    assert_eq!(
        rs.identifier().map(ToString::to_string),
        Some("test-id".to_string())
    );
}

#[test]
fn from_provider_state_aligns_reordered_nested_and_provider_added_list_elements() {
    let nested_group = |rules| {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("id".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            (
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(rules)),
            ),
        ])))
    };
    let mut resource = Resource::with_provider("mock", "listener.Listener", "listener", None);
    resource.set_attr(
        "rules".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            list_rule(80, Some("web")),
            list_rule(443, None),
        ])),
    );
    resource.set_attr(
        "groups".to_string(),
        Value::Concrete(ConcreteValue::List(vec![nested_group(vec![
            list_rule(80, Some("web")),
            list_rule(443, None),
        ])])),
    );
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([
            (
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    list_rule(443, Some("provider-default")),
                    list_rule(22, Some("provider-added")),
                    list_rule(80, Some("web")),
                ])),
            ),
            (
                "groups".to_string(),
                Value::Concrete(ConcreteValue::List(vec![nested_group(vec![
                    list_rule(443, Some("provider-default")),
                    list_rule(80, Some("web")),
                ])])),
            ),
        ]),
    )
    .with_identifier("listener-id");

    let accepted = AcceptedRequest::create_for_test();
    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Applied(&accepted),
        None,
    )
    .unwrap();

    let stored_rules_json = persisted_json_attribute(
        row.persisted_attributes()
            .expect("provider row has persisted attributes"),
        "rules",
    )
    .expect("stored rules attribute");
    let stored_rules = stored_rules_json.as_array().expect("stored rules");
    assert_eq!(stored_rules.len(), 3);
    assert_eq!(stored_rules[0]["port"], 443);
    assert_eq!(stored_rules[1]["port"], 22);
    assert_eq!(stored_rules[2]["port"], 80);
    let ExplicitFields::Struct { children } = &row.explicit else {
        panic!("expected resource-root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("expected aligned rules ListElements");
    };
    assert_eq!(elements.len(), stored_rules.len());
    assert_explicit_struct_keys(&elements[0], &["port"]);
    assert_eq!(elements[1], ExplicitFields::Unrecorded);
    assert_explicit_struct_keys(&elements[2], &["port", "description"]);

    let ExplicitFields::ListElements { elements: groups } = &children["groups"] else {
        panic!("expected aligned groups ListElements");
    };
    let ExplicitFields::Struct {
        children: group_children,
    } = &groups[0]
    else {
        panic!("expected group Struct");
    };
    let ExplicitFields::ListElements {
        elements: nested_rules,
    } = &group_children["rules"]
    else {
        panic!("expected nested aligned ListElements");
    };
    assert_explicit_struct_keys(&nested_rules[0], &["port"]);
    assert_explicit_struct_keys(&nested_rules[1], &["port", "description"]);
}

#[test]
fn state_writeback_uses_explicit_basis_per_sent_top_level_attribute() {
    let desired_rules = vec![list_rule(80, Some("authored"))];
    let resource = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "description",
            Value::Concrete(ConcreteValue::String("before".into())),
        )
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(desired_rules.clone())),
        );
    let prior_provider = State::existing(
        resource.id.clone(),
        HashMap::from([
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("before".into())),
            ),
            (
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    desired_rules[0].clone(),
                    list_rule(443, Some("server-default")),
                ])),
            ),
        ]),
    )
    .with_identifier("listener-id");
    let prior = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &prior_provider,
        None,
        None,
        RowBasis::Applied(&AcceptedRequest::create_for_test()),
        None,
    )
    .expect("create writeback records the server default");

    let updated = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "description",
            Value::Concrete(ConcreteValue::String("after".into())),
        )
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(desired_rules.clone())),
        );
    let updated_provider = State::existing(
        updated.id.clone(),
        HashMap::from([
            (
                "description".to_string(),
                Value::Concrete(ConcreteValue::String("after".into())),
            ),
            (
                "rules".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    desired_rules[0].clone(),
                    list_rule(443, Some("server-default")),
                    list_rule(22, Some("out-of-band")),
                ])),
            ),
        ]),
    )
    .with_identifier("listener-id");
    let update = AcceptedRequest::update_for_test(&["description"], &[]);
    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&updated),
        &updated_provider,
        Some(&prior),
        None,
        RowBasis::Applied(&update),
        None,
    )
    .expect("update writeback");

    let ExplicitFields::Struct { children } = &row.explicit else {
        panic!("expected resource-root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("expected aligned rules ListElements");
    };
    assert_eq!(elements[1], ExplicitFields::Unrecorded);
    assert_eq!(
        elements[2],
        ExplicitFields::Leaf,
        "the unmatched element belongs to an unsent list attribute"
    );
}

#[test]
fn test_from_provider_state_repairs_unrecorded_from_desired_resource() {
    // carina#3280: a legacy `Unrecorded` row is repaired from the
    // authoritative desired resource. Provider-returned attributes are not
    // evidence that the user authored those keys.
    use carina_core::explicit::ExplicitFields;
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let resource = Resource::with_provider("awscc", "sso.Assignment", "x", None);
    // The desired resource intentionally has no authored attributes.
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("identifier".to_string()),
        attributes: [
            (
                "principal_type".to_string(),
                Value::Concrete(ConcreteValue::String("GROUP".into())),
            ),
            (
                "target_id".to_string(),
                Value::Concrete(ConcreteValue::String("123".into())),
            ),
        ]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let mut existing = ResourceState::new("sso.Assignment", "x", "awscc");
    existing.explicit = ExplicitFields::Unrecorded;

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();

    assert_eq!(
        rs.explicit,
        ExplicitFields::Struct {
            children: HashMap::new()
        },
        "the empty desired resource must replace Unrecorded without promoting provider keys"
    );
}

#[test]
fn test_from_provider_state_carry_prior_seeds_unrecorded_from_desired_without_provider_promotion() {
    use carina_core::differ::Diff;
    use carina_core::schema::{AttributeSchema, AttributeType};

    let resource = Resource::with_provider("mock", "test.Resource", "main", None)
        .with_attribute("name", Value::Concrete(ConcreteValue::String("x".into())));
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("x".into())),
            ),
            (
                "tags".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            ),
        ]),
    )
    .with_identifier("identifier");
    let mut existing = ResourceState::new("test.Resource", "main", "mock");
    existing.explicit = ExplicitFields::Unrecorded;
    let schema = ResourceSchema::new("test.Resource")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
        );

    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        Some(&schema),
        RowBasis::Carried,
        None,
    )
    .unwrap();

    assert_eq!(
        row.explicit,
        ExplicitFields::Struct {
            children: HashMap::from([("name".to_string(), ExplicitFields::Leaf)])
        },
        "an unknown prior record must be seeded only from desired keys"
    );

    let mut state = StateFile::new();
    state.upsert_resource(row).unwrap();
    let saved = state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    let stored = state.find_resource_for_id(&resource.id).unwrap();
    assert!(matches!(
        diff(
            &resolved(&resource),
            &provider_state,
            &saved,
            Some(&stored.explicit),
            Some(&schema),
        ),
        Diff::NoChange(_)
    ));

    let removed_name = resolved(&Resource::with_provider(
        "mock",
        "test.Resource",
        "main",
        None,
    ));
    let Diff::Update {
        changed_attributes, ..
    } = diff(
        &removed_name,
        &provider_state,
        &saved,
        Some(&stored.explicit),
        Some(&schema),
    )
    else {
        panic!("removing a desired key after repair must plan an update");
    };
    assert_eq!(changed_attributes, vec!["name".to_string()]);
}

#[test]
fn test_from_provider_state_carry_prior_without_prior_seeds_from_desired() {
    let resource = Resource::with_provider("mock", "test.Resource", "main", None)
        .with_attribute("name", Value::Concrete(ConcreteValue::String("x".into())));
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("x".into())),
            ),
            (
                "tags".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            ),
        ]),
    )
    .with_identifier("identifier");

    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Carried,
        None,
    )
    .unwrap();

    assert_eq!(
        row.explicit,
        ExplicitFields::Struct {
            children: HashMap::from([("name".to_string(), ExplicitFields::Leaf)])
        }
    );
}

#[test]
fn carried_without_existing_row_uses_read_basis_for_unmatched_list_elements() {
    let resource = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![list_rule(80, Some("authored"))])),
        );
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, Some("authored")),
                list_rule(22, Some("out-of-band")),
            ])),
        )]),
    )
    .with_identifier("listener-id");

    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Carried,
        None,
    )
    .expect("carried row without prior state");

    let ExplicitFields::Struct { children } = &row.explicit else {
        panic!("expected resource-root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("expected aligned rules ListElements");
    };
    assert_eq!(elements[1], ExplicitFields::Leaf);
}

#[test]
fn test_from_provider_state_carry_prior_preserves_partially_removed_authored_key() {
    let resource = Resource::with_provider("mock", "test.Resource", "main", None).with_attribute(
        "name",
        Value::Concrete(ConcreteValue::String("kept".into())),
    );
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("kept".into())),
            ),
            (
                "tags".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            ),
        ]),
    )
    .with_identifier("resource-id");
    let prior_explicit = ExplicitFields::Struct {
        children: HashMap::from([
            ("name".to_string(), ExplicitFields::Leaf),
            (
                "tags".to_string(),
                ExplicitFields::Struct {
                    children: HashMap::new(),
                },
            ),
        ]),
    };
    let mut existing = ResourceState::new("test.Resource", "main", "mock");
    existing.explicit = prior_explicit.clone();

    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        None,
        RowBasis::Carried,
        None,
    )
    .unwrap();

    assert_eq!(
        row.explicit, prior_explicit,
        "a failed partial removal must retain the removed key until a desired write applies"
    );
}

#[test]
fn test_from_provider_state_carry_prior_demotes_retained_list_alignment() {
    let resource = Resource::with_provider("mock", "test.Resource", "main", None).with_attribute(
        "name",
        Value::Concrete(ConcreteValue::String("kept".into())),
    );
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("kept".into())),
            ),
            (
                "k".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::Int(2)),
                    Value::Concrete(ConcreteValue::Int(1)),
                ])),
            ),
        ]),
    )
    .with_identifier("resource-id");
    let mut existing = ResourceState::new("test.Resource", "main", "mock");
    existing.explicit = ExplicitFields::Struct {
        children: HashMap::from([
            ("name".to_string(), ExplicitFields::Leaf),
            (
                "k".to_string(),
                ExplicitFields::ListElements {
                    elements: vec![ExplicitFields::Leaf, ExplicitFields::Leaf],
                },
            ),
        ]),
    };

    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        None,
        RowBasis::Carried,
        None,
    )
    .unwrap();

    let ExplicitFields::Struct { children } = row.explicit else {
        panic!("expected carried root Struct");
    };
    assert_eq!(
        children["k"],
        ExplicitFields::List {
            element: Box::new(ExplicitFields::Leaf)
        },
        "a retained pending-removal child must not keep alignment to the prior row's list"
    );
}

#[test]
fn test_from_provider_state_carry_prior_drops_removed_key_absent_from_fresh_state() {
    let resource = Resource::with_provider("mock", "test.Resource", "main", None).with_attribute(
        "name",
        Value::Concrete(ConcreteValue::String("kept".into())),
    );
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("kept".into())),
        )]),
    )
    .with_identifier("resource-id");
    let mut existing = ResourceState::new("test.Resource", "main", "mock");
    existing.explicit = ExplicitFields::Struct {
        children: HashMap::from([
            ("name".to_string(), ExplicitFields::Leaf),
            ("foo".to_string(), ExplicitFields::Leaf),
        ]),
    };

    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        None,
        RowBasis::Carried,
        None,
    )
    .unwrap();

    assert_eq!(
        row.explicit,
        ExplicitFields::Struct {
            children: HashMap::from([("name".to_string(), ExplicitFields::Leaf)])
        },
        "a carried removal stops being pending once the provider no longer returns the key"
    );
}

#[test]
fn carried_top_level_unrecorded_child_still_plans_its_removal() {
    use carina_core::differ::Diff;

    let authored = Resource::with_provider("mock", "test.Resource", "main", None).with_attribute(
        "legacy",
        Value::Concrete(ConcreteValue::String("provider-value".into())),
    );
    let provider_state = State::existing(
        authored.id.clone(),
        HashMap::from([(
            "legacy".to_string(),
            Value::Concrete(ConcreteValue::String("provider-value".into())),
        )]),
    )
    .with_identifier("resource-id");
    let mut prior = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&authored),
        &provider_state,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .expect("prior row");
    prior.explicit = ExplicitFields::Struct {
        children: HashMap::from([("legacy".to_string(), ExplicitFields::Unrecorded)]),
    };

    let desired = Resource::with_provider("mock", "test.Resource", "main", None);
    let carried = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&desired),
        &provider_state,
        Some(&prior),
        None,
        RowBasis::Carried,
        None,
    )
    .expect("carried row");
    let ExplicitFields::Struct { children } = &carried.explicit else {
        panic!("expected carried root Struct");
    };
    assert_eq!(children.get("legacy"), Some(&ExplicitFields::Unrecorded));

    let mut state = StateFile::new();
    state.upsert_resource(carried.clone()).unwrap();
    let saved = state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    assert!(matches!(
        diff(
            &resolved(&desired),
            &provider_state,
            &saved,
            Some(&carried.explicit),
            None,
        ),
        Diff::Update {
            ref changed_attributes,
            ..
        } if changed_attributes == &["legacy".to_string()]
    ));
}

#[test]
fn carried_list_pending_removal_transfers_across_unique_reorder() {
    use carina_core::differ::Diff;

    let (desired, fresh, carried) = carried_rules_fixture(
        vec![list_rule(80, Some("web")), list_rule(443, None)],
        vec![list_rule(443, None), list_rule(80, None)],
        vec![list_rule(443, None), list_rule(80, Some("web"))],
    );
    let explicit = carried.explicit.clone();
    let ExplicitFields::Struct { children } = &explicit else {
        panic!("expected carried root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("a uniquely paired removal must retain aligned ListElements");
    };
    assert_explicit_struct_keys(&elements[0], &["port"]);
    assert_explicit_struct_keys(&elements[1], &["port", "description"]);

    let mut state = StateFile::new();
    state.upsert_resource(carried).unwrap();
    let saved = state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    assert!(matches!(
        diff(
            &resolved(&desired),
            &fresh,
            &saved,
            Some(&explicit),
            None,
        ),
        Diff::Update {
            ref changed_attributes,
            ..
        } if changed_attributes == &["rules".to_string()]
    ));
}

#[test]
fn carried_list_with_secret_field_still_pairs_by_ordinary_identity_fields() {
    let authored_rule = |port: i64, description: Option<&str>, token: &str| {
        let Value::Concrete(ConcreteValue::Map(mut fields)) = list_rule(port, description) else {
            unreachable!("list_rule always returns a map");
        };
        fields.insert(
            "token".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String(token.to_string().into()),
            )))),
        );
        Value::Concrete(ConcreteValue::Map(fields))
    };
    let provider_rule = |port: i64, description: Option<&str>, token: &str| {
        let Value::Concrete(ConcreteValue::Map(mut fields)) = list_rule(port, description) else {
            unreachable!("list_rule always returns a map");
        };
        fields.insert(
            "token".to_string(),
            Value::Concrete(ConcreteValue::String(token.to_string().into())),
        );
        Value::Concrete(ConcreteValue::Map(fields))
    };

    let applied = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                authored_rule(80, Some("web"), "alpha"),
                authored_rule(443, None, "beta"),
            ])),
        );
    let prior_provider = State::existing(
        applied.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                provider_rule(80, Some("web"), "alpha"),
                provider_rule(443, None, "beta"),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let prior = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&applied),
        &prior_provider,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .expect("initial row with secret-bearing list elements");

    let desired = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                authored_rule(443, None, "beta"),
                authored_rule(80, None, "alpha"),
            ])),
        );
    let fresh = State::existing(
        desired.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                provider_rule(443, None, "beta"),
                provider_rule(80, Some("web"), "alpha"),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let carried = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&desired),
        &fresh,
        Some(&prior),
        None,
        RowBasis::Carried,
        None,
    )
    .expect("secret hashes must not prevent ordinary unique pairing");
    let ExplicitFields::Struct { children } = &carried.explicit else {
        panic!("expected carried root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("unique port identity should retain aligned ListElements");
    };
    assert_explicit_struct_keys(&elements[0], &["port", "token"]);
    assert_explicit_struct_keys(&elements[1], &["port", "description", "token"]);
}

#[test]
fn carried_ambiguous_heterogeneous_removal_retries_the_whole_list() {
    use carina_core::differ::Diff;

    let applied_rules = vec![list_rule(80, Some("a")), list_rule(80, Some("b"))];
    let desired_rules = vec![list_rule(80, None), list_rule(80, Some("b"))];
    let (desired, fresh, carried) =
        carried_rules_fixture(applied_rules.clone(), desired_rules, applied_rules);
    let ExplicitFields::Struct { children } = &carried.explicit else {
        panic!("expected carried root Struct");
    };
    assert_eq!(children["rules"], whole_value(&[&["description"]]));

    let explicit = carried.explicit.clone();
    let mut state = StateFile::new();
    state.upsert_resource(carried).unwrap();
    let saved = state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    assert!(matches!(
        diff(
            &resolved(&desired),
            &fresh,
            &saved,
            Some(&explicit),
            None,
        ),
        Diff::Update {
            ref changed_attributes,
            ..
        } if changed_attributes == &["rules".to_string()]
    ));
}

#[test]
fn carried_list_pending_removal_with_duplicates_falls_back_to_whole_value() {
    let (_desired, _fresh, carried) = carried_rules_fixture(
        vec![list_rule(80, Some("web")), list_rule(80, None)],
        vec![list_rule(80, None), list_rule(80, None)],
        vec![list_rule(80, Some("web")), list_rule(80, None)],
    );
    let ExplicitFields::Struct { children } = &carried.explicit else {
        panic!("expected carried root Struct");
    };
    assert_eq!(children["rules"], whole_value(&[&["description"]]));

    let mut state = StateFile::new();
    state.upsert_resource(carried).unwrap();
    let encoded = serde_json::to_string(&state).expect("WholeValue state serializes");
    assert!(encoded.contains(r#""kind":"whole-value""#));
    let reloaded = check_and_migrate(&encoded)
        .expect("current WholeValue state reloads")
        .into_state();
    assert_eq!(
        reloaded.resources()[0].explicit,
        state.resources()[0].explicit
    );
}

#[test]
fn carried_converged_duplicate_elements_stay_aligned_across_refresh() {
    use carina_core::differ::Diff;

    let rules = vec![
        list_rule(80, Some("web")),
        list_rule(80, Some("web")),
        list_rule(80, None),
    ];
    let (desired, fresh, first_refresh) =
        carried_rules_fixture(rules.clone(), rules.clone(), rules);

    let assert_aligned = |row: &ResourceState| {
        let ExplicitFields::Struct { children } = &row.explicit else {
            panic!("expected carried root Struct");
        };
        assert!(
            matches!(children["rules"], ExplicitFields::ListElements { .. }),
            "a converged duplicate list must not acquire WholeValue: {:?}",
            children["rules"]
        );
    };
    assert_aligned(&first_refresh);

    // A stale WholeValue from an earlier ambiguous removal must clear under
    // the same converged counts, even though the duplicate elements still
    // cannot be assigned identities.
    let mut stale_marker = first_refresh.clone();
    let ExplicitFields::Struct { children } = &mut stale_marker.explicit else {
        panic!("expected carried root Struct");
    };
    children.insert("rules".to_string(), whole_value(&[&["description"]]));
    let marker_refresh = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&desired),
        &fresh,
        Some(&stale_marker),
        None,
        RowBasis::Carried,
        None,
    )
    .expect("converged authored counts should clear a stale WholeValue");
    assert_aligned(&marker_refresh);

    let second_refresh = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&desired),
        &fresh,
        Some(&marker_refresh),
        None,
        RowBasis::Carried,
        None,
    )
    .expect("a second refresh should remain converged");
    assert_aligned(&second_refresh);

    let mut state = StateFile::new();
    state.upsert_resource(second_refresh.clone()).unwrap();
    let saved = state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    assert!(matches!(
        diff(
            &resolved(&desired),
            &fresh,
            &saved,
            Some(&second_refresh.explicit),
            None,
        ),
        Diff::NoChange(_)
    ));
}

#[test]
fn carried_list_pending_removal_with_reorder_and_mutation_falls_back_to_whole_value() {
    let with_protocol = |mut rule: Value| {
        let Value::Concrete(ConcreteValue::Map(fields)) = &mut rule else {
            unreachable!("test rule is a map");
        };
        fields.insert(
            "protocol".to_string(),
            Value::Concrete(ConcreteValue::String("tcp".into())),
        );
        rule
    };
    let (_desired, _fresh, carried) = carried_rules_fixture(
        vec![
            with_protocol(list_rule(80, Some("web"))),
            with_protocol(list_rule(443, None)),
        ],
        vec![
            with_protocol(list_rule(8443, None)),
            with_protocol(list_rule(8080, None)),
        ],
        vec![
            with_protocol(list_rule(443, None)),
            with_protocol(list_rule(80, Some("web"))),
        ],
    );
    let ExplicitFields::Struct { children } = carried.explicit else {
        panic!("expected carried root Struct");
    };
    assert_eq!(children["rules"], whole_value(&[&["description"]]));
}

#[test]
fn carried_whole_value_keeps_remaining_pending_removal_after_partial_success() {
    use carina_core::differ::Diff;

    let applied_rules = vec![
        list_rule_with_tag(80, Some("web"), None),
        list_rule_with_tag(80, None, Some("legacy")),
    ];
    let desired_rules = vec![
        list_rule_with_tag(80, None, None),
        list_rule_with_tag(80, None, None),
    ];
    let (desired, _fresh, first_carried) =
        carried_rules_fixture(applied_rules.clone(), desired_rules, applied_rules);
    let ExplicitFields::Struct { children } = &first_carried.explicit else {
        panic!("expected carried root Struct");
    };
    assert_eq!(
        children["rules"],
        whole_value(&[&["description"], &["tag"]])
    );

    // The description removal completed out of band, but the tag removal did
    // not. Clearing WholeValue here would hide the remaining removal again.
    let partially_updated = State::existing(
        desired.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule_with_tag(80, None, None),
                list_rule_with_tag(80, None, Some("legacy")),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let carried = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&desired),
        &partially_updated,
        Some(&first_carried),
        None,
        RowBasis::Carried,
        None,
    )
    .expect("partial provider progress should remain carryable");
    let ExplicitFields::Struct { children } = &carried.explicit else {
        panic!("expected carried root Struct");
    };
    assert_eq!(children["rules"], whole_value(&[&["tag"]]));

    let explicit = carried.explicit.clone();
    let mut state = StateFile::new();
    state.upsert_resource(carried).unwrap();
    let saved = state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    assert!(matches!(
        diff(
            &resolved(&desired),
            &partially_updated,
            &saved,
            Some(&explicit),
            None,
        ),
        Diff::Update {
            ref changed_attributes,
            ..
        } if changed_attributes == &["rules".to_string()]
    ));
}

#[test]
fn carried_whole_value_clears_when_readded_field_has_a_unique_pair() {
    use carina_core::differ::Diff;
    use carina_core::schema::{AttributeSchema, AttributeType, StructField};

    let authored_rules = vec![list_rule(80, Some("web")), list_rule(80, None)];
    let provider_rules: Vec<_> = authored_rules
        .iter()
        .cloned()
        .map(with_provider_status)
        .collect();
    let schema = ResourceSchema::new("listener.Listener").attribute(
        AttributeSchema::optional(
            "rules",
            AttributeType::list(AttributeType::struct_(
                "Rule",
                vec![
                    StructField::optional("port", AttributeType::int()).build(),
                    StructField::optional("description", AttributeType::string()).build(),
                    StructField::provider_populated("provider_status", AttributeType::string())
                        .build(),
                ],
            )),
        )
        .build(),
    );
    let applied = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(authored_rules.clone())),
        );
    let provider = State::existing(
        applied.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(provider_rules)),
        )]),
    )
    .with_identifier("listener-id");
    let prior = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&applied),
        &provider,
        None,
        Some(&schema),
        RowBasis::Imported,
        None,
    )
    .expect("initial applied row");
    let removed = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, None),
                list_rule(80, None),
            ])),
        );
    let marker = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&removed),
        &provider,
        Some(&prior),
        Some(&schema),
        RowBasis::Carried,
        None,
    )
    .expect("ambiguous removal should be carried");
    let ExplicitFields::Struct { children } = &marker.explicit else {
        panic!("expected carried root Struct");
    };
    assert_eq!(
        serde_json::to_value(&children["rules"]).unwrap(),
        serde_json::json!({
            "kind": "whole-value",
            "pending": [["description"]]
        })
    );

    // The fresh element that still has description now pairs uniquely to the
    // desired element that re-authored it. A carried refresh can therefore
    // clear the marker without guessing about the other duplicate.
    let refreshed = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&applied),
        &provider,
        Some(&marker),
        Some(&schema),
        RowBasis::Carried,
        None,
    )
    .expect("re-added field should converge on refresh");
    let ExplicitFields::Struct { children } = &refreshed.explicit else {
        panic!("expected refreshed root Struct");
    };
    assert!(matches!(
        children["rules"],
        ExplicitFields::ListElements { .. }
    ));

    let explicit = refreshed.explicit.clone();
    let mut state = StateFile::new();
    state.upsert_resource(refreshed.clone()).unwrap();
    let saved = state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    assert!(matches!(
        diff(
            &resolved(&applied),
            &provider,
            &saved,
            Some(&explicit),
            Some(&schema),
        ),
        Diff::NoChange(_)
    ));

    let accepted = AcceptedRequest::create_for_test();
    let applied_row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&applied),
        &provider,
        Some(&refreshed),
        Some(&schema),
        RowBasis::Applied(&accepted),
        None,
    )
    .expect("successful apply rebuilds the authoring record");
    let ExplicitFields::Struct { children } = &applied_row.explicit else {
        panic!("expected applied root Struct");
    };
    assert!(matches!(
        children["rules"],
        ExplicitFields::ListElements { .. }
    ));

    let mut state = StateFile::new();
    state.upsert_resource(applied_row.clone()).unwrap();
    let saved = state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    assert!(matches!(
        diff(
            &resolved(&applied),
            &provider,
            &saved,
            Some(&applied_row.explicit),
            Some(&schema),
        ),
        Diff::NoChange(_)
    ));
}

#[test]
fn carried_legacy_list_mixed_refresh_rebuilds_alignment_and_plans_clean() {
    use carina_core::differ::Diff;
    use carina_core::schema::{AttributeSchema, AttributeType, StructField};

    let authored_rules = vec![list_rule(80, Some("web")), list_rule(443, None)];
    let provider_rules: Vec<_> = authored_rules
        .iter()
        .cloned()
        .map(with_provider_status)
        .collect();
    let schema = ResourceSchema::new("listener.Listener").attribute(
        AttributeSchema::optional(
            "rules",
            AttributeType::list(AttributeType::struct_(
                "Rule",
                vec![
                    StructField::optional("port", AttributeType::int()).build(),
                    StructField::optional("description", AttributeType::string()).build(),
                    // Deliberately not read-only: WholeValue would expose this
                    // provider-populated field as a phantom removal.
                    StructField::optional("provider_status", AttributeType::string()).build(),
                ],
            )),
        )
        .build(),
    );
    let desired = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(authored_rules)),
        );
    let provider = State::existing(
        desired.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(provider_rules)),
        )]),
    )
    .with_identifier("listener-id");
    let mut prior = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&desired),
        &provider,
        None,
        Some(&schema),
        RowBasis::Imported,
        None,
    )
    .expect("initial row");
    prior.explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "rules".to_string(),
            ExplicitFields::List {
                element: Box::new(ExplicitFields::Struct {
                    children: HashMap::from([
                        ("port".to_string(), ExplicitFields::Leaf),
                        ("description".to_string(), ExplicitFields::Leaf),
                    ]),
                }),
            },
        )]),
    };

    let refreshed = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&desired),
        &provider,
        Some(&prior),
        Some(&schema),
        RowBasis::Carried,
        None,
    )
    .expect("legacy row refresh");
    let ExplicitFields::Struct { children } = &refreshed.explicit else {
        panic!("expected refreshed root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("a mixed list without a removal must upgrade to ListElements");
    };
    assert_explicit_struct_keys(&elements[0], &["port", "description"]);
    assert_explicit_struct_keys(&elements[1], &["port"]);

    let mut state = StateFile::new();
    state.upsert_resource(refreshed.clone()).unwrap();
    let saved = state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    assert!(matches!(
        diff(
            &resolved(&desired),
            &provider,
            &saved,
            Some(&refreshed.explicit),
            Some(&schema),
        ),
        Diff::NoChange(_)
    ));
}

#[test]
fn carried_list_without_pending_removal_keeps_rebuilt_alignment() {
    let (_desired, _fresh, carried) = carried_rules_fixture(
        vec![list_rule(80, Some("web")), list_rule(443, None)],
        vec![list_rule(80, Some("web")), list_rule(443, None)],
        vec![list_rule(443, None), list_rule(80, Some("web"))],
    );
    let ExplicitFields::Struct { children } = carried.explicit else {
        panic!("expected carried root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("a list without a pending removal must stay aligned");
    };
    assert_explicit_struct_keys(&elements[0], &["port"]);
    assert_explicit_struct_keys(&elements[1], &["port", "description"]);
}

#[test]
fn test_from_provider_state_records_empty_struct_for_fresh_empty_body_resource() {
    // An empty desired resource is an authoritative record that the user
    // authored no attributes, including for green-field writes and imports.
    use carina_core::explicit::ExplicitFields;
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let resource = Resource::with_provider("aws", "sts.CallerIdentity", "caller", None);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("identifier".to_string()),
        attributes: [(
            "account_id".to_string(),
            Value::Concrete(ConcreteValue::String("123456789012".into())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();

    assert_eq!(
        rs.explicit,
        ExplicitFields::Struct {
            children: HashMap::new()
        },
        "first-apply empty-body resource must record an empty authored set"
    );
}

#[test]
fn test_from_provider_state_replaces_prior_authored_keys_after_all_attributes_are_deleted() {
    use carina_core::differ::Diff;
    use carina_core::explicit::ExplicitFields;
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let resource = Resource::with_provider("awscc", "ec2.InternetGateway", "igw", None);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("id".to_string()),
        attributes: [(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let mut existing = ResourceState::new("ec2.InternetGateway", "igw", "awscc");
    existing.explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "tags".into(),
            ExplicitFields::Struct {
                children: HashMap::new(),
            },
        )]),
    };
    let schema = ResourceSchema::new("ec2.InternetGateway").attribute(
        AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
    );

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        Some(&schema),
        RowBasis::Imported,
        None,
    )
    .unwrap();

    assert_eq!(
        rs.explicit,
        ExplicitFields::Struct {
            children: HashMap::new()
        },
        "deleting all desired attributes must clear the prior authored keys"
    );

    let mut state = StateFile::new();
    state.upsert_resource(rs).unwrap();
    let encoded = serde_json::to_string(&state).expect("current state serializes");
    let checked = check_and_migrate(&encoded).expect("current state reloads without migration");
    assert!(checked.migration().is_none());
    let reloaded = checked.into_state();
    let reloaded_row = reloaded
        .find_resource_for_id(&resource.id)
        .expect("reloaded IGW row");
    assert_eq!(
        reloaded_row.explicit,
        ExplicitFields::Struct {
            children: HashMap::new()
        },
        "a current-version empty authored set must round-trip without becoming Unrecorded"
    );

    let saved = reloaded
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    assert!(matches!(
        diff(
            &resolved(&resource),
            &provider_state,
            &saved,
            Some(&reloaded_row.explicit),
            Some(&schema),
        ),
        Diff::NoChange(_)
    ));
    assert!(matches!(
        diff(
            &resolved(&resource),
            &provider_state,
            &saved,
            Some(&ExplicitFields::Unrecorded),
            Some(&schema),
        ),
        Diff::NoChange(_)
    ));
}

#[test]
fn test_from_provider_state_replaces_prior_top_level_list_when_resource_attrs_empty() {
    let resource = Resource::with_provider("mock", "test.Resource", "empty", None);
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([(
            "provider_value".to_string(),
            Value::Concrete(ConcreteValue::String("value".into())),
        )]),
    )
    .with_identifier("resource-id");
    let mut existing = ResourceState::new("test.Resource", "empty", "mock");
    let populated = ExplicitFields::ListElements {
        elements: vec![ExplicitFields::Struct {
            children: HashMap::from([("id".to_string(), ExplicitFields::Leaf)]),
        }],
    };
    existing.explicit = populated.clone();

    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();

    assert_eq!(
        row.explicit,
        ExplicitFields::Struct {
            children: HashMap::new(),
        }
    );
}

#[test]
fn bodyless_refresh_demotes_reordered_list_elements_and_merge_stays_conservative() {
    use carina_core::differ::Diff;

    let resource = Resource::with_provider("mock", "listener.Listener", "listener", None);
    let reordered_provider = State::existing(
        resource.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(443, Some("provider-default")),
                list_rule(80, Some("web")),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let mut existing = ResourceState::new("listener.Listener", "listener", "mock");
    existing.explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "rules".to_string(),
            ExplicitFields::ListElements {
                elements: vec![
                    ExplicitFields::Struct {
                        children: HashMap::from([
                            ("port".to_string(), ExplicitFields::Leaf),
                            ("description".to_string(), ExplicitFields::Leaf),
                        ]),
                    },
                    ExplicitFields::Struct {
                        children: HashMap::from([("port".to_string(), ExplicitFields::Leaf)]),
                    },
                ],
            },
        )]),
    };

    let row = existing
        .refreshable()
        .expect("an orphan row without an in-flight create can refresh")
        .refreshed_from_provider_state(&reordered_provider, None)
        .unwrap();
    let ExplicitFields::Struct { children } = &row.explicit else {
        panic!("expected preserved root Struct");
    };
    let ExplicitFields::List { element } = &children["rules"] else {
        panic!("reordered provider values must demote nested ListElements");
    };
    assert_explicit_struct_keys(element, &["port", "description"]);

    let mut state = StateFile::new();
    state.upsert_resource(row.clone()).unwrap();
    let saved = state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    let reauthored = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, None),
                list_rule(443, None),
            ])),
        );
    assert!(matches!(
        diff(
            &resolved(&reauthored),
            &reordered_provider,
            &saved,
            Some(&row.explicit),
            None,
        ),
        Diff::NoChange(_)
    ));
}

#[test]
fn bodyless_refresh_preserves_whole_value_without_guessing_alignment() {
    let resource = Resource::with_provider("mock", "listener.Listener", "listener", None);
    let provider = State::existing(
        resource.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, Some("web")),
                list_rule(80, None),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let mut existing = ResourceState::new("listener.Listener", "listener", "mock");
    existing.explicit = ExplicitFields::Struct {
        children: HashMap::from([("rules".to_string(), whole_value(&[&["description"]]))]),
    };

    let refreshed = existing
        .refreshable()
        .expect("orphan row is refreshable")
        .refreshed_from_provider_state(&provider, None)
        .expect("orphan refresh succeeds");

    assert_eq!(refreshed.explicit, existing.explicit);
}

#[test]
fn test_from_provider_state_replaces_unrecorded_when_desired_and_state_attrs_are_empty() {
    // Even with no stored provider attributes, the empty desired resource is
    // authoritative and records an empty authored set.
    use carina_core::explicit::ExplicitFields;
    use carina_core::resource::{Resource, State as ProviderState};

    let resource = Resource::with_provider("awscc", "sso.Assignment", "x", None);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("id".to_string()),
        attributes: HashMap::new(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let mut existing = ResourceState::new("sso.Assignment", "x", "awscc");
    existing.explicit = ExplicitFields::Unrecorded;

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();

    assert_eq!(
        rs.explicit,
        ExplicitFields::Struct {
            children: HashMap::new()
        }
    );
}

#[test]
fn test_migrate_v6_empty_struct_to_unrecorded() {
    // carina#3280: state files at v6 carry the corrupt-row shape as
    // `Struct { children: {} }`. The v6 → v7 migration rewrites
    // every top-level empty Struct to `Unrecorded` so callers never
    // encounter the ambiguous shape after read.
    use carina_core::explicit::ExplicitFields;
    let v6 = r#"{
        "version": 6,
        "serial": 1,
        "lineage": "test-lineage",
        "carina_version": "0.1.0",
        "resources": [
            {
                "resource_type": "sso.Assignment",
                "name": "x",
                "provider": "awscc",
                "identifier": "id",
                "attributes": { "target_id": "123" },
                "protected": false,
                "directives": {},
                "prefixes": {},
                "name_overrides": {},
                "binding": "x",
                "dependency_bindings": [],
                "explicit": { "kind": "struct", "children": {} }
            }
        ]
    }"#;
    let state = finish_legacy_dependency_migration(
        check_and_migrate(v6)
            .expect("migration should succeed")
            .into_state(),
    );
    assert_eq!(state.version, StateFile::CURRENT_VERSION);
    let rs = state
        .resources
        .iter()
        .find(|r| r.identity.as_str() == "x")
        .expect("test resource");
    assert!(
        matches!(rs.explicit, ExplicitFields::Unrecorded),
        "v6 empty-Struct row must migrate to Unrecorded, got {:?}",
        rs.explicit
    );
}

#[test]
fn test_migrate_v6_preserves_populated_explicit() {
    // carina#3280 sibling: the v6 → v7 migration must leave populated
    // `Struct` rows untouched. Only the top-level empty-Struct
    // (corruption) shape is rewritten.
    use carina_core::explicit::ExplicitFields;
    let v6 = r#"{
        "version": 6,
        "serial": 1,
        "lineage": "test-lineage",
        "carina_version": "0.1.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "name": "vpc",
                "provider": "awscc",
                "identifier": "vpc-1",
                "attributes": { "cidr_block": "10.0.0.0/16" },
                "protected": false,
                "directives": {},
                "prefixes": {},
                "name_overrides": {},
                "binding": "vpc",
                "dependency_bindings": [],
                "explicit": {
                    "kind": "struct",
                    "children": { "cidr_block": { "kind": "leaf" } }
                }
            }
        ]
    }"#;
    let state = check_and_migrate(v6)
        .expect("migration should succeed")
        .into_state();
    let rs = state
        .resources
        .iter()
        .find(|r| r.identity.as_str() == "vpc")
        .unwrap();
    let ExplicitFields::Struct { children } = &rs.explicit else {
        panic!(
            "populated v6 Struct must survive migration, got {:?}",
            rs.explicit
        );
    };
    assert!(children.contains_key("cidr_block"));
}

#[test]
fn test_migrate_v6_does_not_rewrite_nested_empty_struct() {
    // carina#3280: the migration is top-level-only by design. A
    // nested empty `Struct { children: {} }` (the legitimate
    // "user wrote `tags = {}`" shape) must survive the migration
    // unchanged — it is structurally meaningful at that position
    // (recursive `project` correctly drops every field), unlike the
    // top-level legacy-corruption case.
    use carina_core::explicit::ExplicitFields;
    let v6 = r#"{
        "version": 6,
        "serial": 1,
        "lineage": "test-lineage",
        "carina_version": "0.1.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "name": "vpc",
                "provider": "awscc",
                "identifier": "vpc-1",
                "attributes": {},
                "protected": false,
                "directives": {},
                "prefixes": {},
                "name_overrides": {},
                "binding": "vpc",
                "dependency_bindings": [],
                "explicit": {
                    "kind": "struct",
                    "children": {
                        "tags": { "kind": "struct", "children": {} }
                    }
                }
            }
        ]
    }"#;
    let state = check_and_migrate(v6)
        .expect("migration should succeed")
        .into_state();
    let rs = state
        .resources
        .iter()
        .find(|r| r.identity.as_str() == "vpc")
        .unwrap();
    let ExplicitFields::Struct { children } = &rs.explicit else {
        panic!("expected top-level Struct, got {:?}", rs.explicit);
    };
    let tags = children.get("tags").expect("tags child");
    assert!(
        matches!(tags, ExplicitFields::Struct { children } if children.is_empty()),
        "nested empty Struct must survive migration (legitimate `tags = {{}}` shape); got {:?}",
        tags
    );
}

#[test]
fn test_multi_provider_resources_do_not_collide() {
    use carina_core::resource::Resource;

    let mut state = StateFile::new();

    // Store two resources with the same resource_type and name but different providers
    let aws_resource = ResourceState::try_applied("s3.Bucket", "main", "aws", "aws-bucket-id")
        .expect("fixture identifier is non-empty");
    let awscc_resource =
        ResourceState::try_applied("s3.Bucket", "main", "awscc", "awscc-bucket-id")
            .expect("fixture identifier is non-empty");

    state
        .upsert_resource(aws_resource)
        .expect("test state setup must be valid");
    state
        .upsert_resource(awscc_resource)
        .expect("test state setup must be valid");

    // Both should be stored independently
    assert_eq!(state.resources.len(), 2);

    // find_resource should return the correct one for each provider
    let found_aws = state.find_resource("aws", "s3.Bucket", "main").unwrap();
    assert_eq!(
        found_aws.identifier().map(ToString::to_string),
        Some("aws-bucket-id".to_string())
    );

    let found_awscc = state.find_resource("awscc", "s3.Bucket", "main").unwrap();
    assert_eq!(
        found_awscc.identifier().map(ToString::to_string),
        Some("awscc-bucket-id".to_string())
    );

    // Loaded identifiers remain provider-scoped.
    let aws_res = Resource::with_provider("aws", "s3.Bucket", "main", None);
    assert!(matches!(
        state.build_state_for_resource(&aws_res.id),
        Current::Present(ref present)
            if present.identifier().as_str() == "aws-bucket-id"
    ));

    let awscc_res = Resource::with_provider("awscc", "s3.Bucket", "main", None);
    assert!(matches!(
        state.build_state_for_resource(&awscc_res.id),
        Current::Present(ref present)
            if present.identifier().as_str() == "awscc-bucket-id"
    ));

    // Upsert should only update the matching provider's entry
    let updated_aws = ResourceState::try_applied("s3.Bucket", "main", "aws", "aws-bucket-id-v2")
        .expect("fixture identifier is non-empty");
    state
        .upsert_resource(updated_aws)
        .expect("test state setup must be valid");
    assert_eq!(state.resources.len(), 2);
    assert_eq!(
        state
            .find_resource("aws", "s3.Bucket", "main")
            .unwrap()
            .identifier()
            .map(ToString::to_string),
        Some("aws-bucket-id-v2".to_string())
    );
    assert_eq!(
        state
            .find_resource("awscc", "s3.Bucket", "main")
            .unwrap()
            .identifier()
            .map(ToString::to_string),
        Some("awscc-bucket-id".to_string())
    );

    // remove_resource should only remove the matching provider's entry
    let removed = state.remove_resource("aws", "s3.Bucket", "main");
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().provider, "aws");
    assert_eq!(state.resources.len(), 1);

    // The awscc entry should still exist
    assert!(state.find_resource("awscc", "s3.Bucket", "main").is_some());
    assert!(state.find_resource("aws", "s3.Bucket", "main").is_none());
}

#[test]
fn state_block_address_lookup_ignores_provider_instance() {
    use carina_core::parser::StateBlockAddress;

    let mut state = StateFile::new();
    let mut resource = ResourceState::new("test.Resource", "target", "mock");
    resource.directives.provider_instance = Some("secondary".to_string());
    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");

    let address = StateBlockAddress::new("mock", "test.Resource", "target");
    assert!(
        state
            .find_resource_by_state_block_address(&address)
            .is_some()
    );

    let wrong_provider = StateBlockAddress::new("other", "test.Resource", "target");
    assert!(
        state
            .find_resource_by_state_block_address(&wrong_provider)
            .is_none()
    );
}

#[test]
fn directive_only_update_requires_the_exact_provider_route() {
    let mut state = StateFile::new();
    let mut resource = ResourceState::new("test.Resource", "target", "mock");
    resource.directives.provider_instance = Some("secondary".to_string());
    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");

    let default_id =
        ResolvedResourceId::with_provider_identity("mock", "test.Resource", "target", None);
    let mut desired = PersistedDirectives::default();
    desired.force_delete = true;
    assert!(
        !state.update_resource_directives(&default_id, desired.clone()),
        "a display-equal address on another route must not be rewritten"
    );

    let secondary_id = ResolvedResourceId::with_provider_identity(
        "mock",
        "test.Resource",
        "target",
        Some("secondary".to_string()),
    );
    desired.provider_instance = Some("secondary".to_string());
    assert!(state.update_resource_directives(&secondary_id, desired));
    assert!(
        state
            .find_resource_for_id(&secondary_id)
            .expect("secondary row")
            .directives
            .force_delete
    );
}

#[test]
fn test_build_directives_provider_scoped() {
    use carina_core::resource::ResourceId;

    let mut state = StateFile::new();
    let mut aws_rs = ResourceState::new("s3.Bucket", "main", "aws");
    aws_rs.directives.force_delete = true;
    let awscc_rs = ResourceState::new("s3.Bucket", "main", "awscc");

    state
        .upsert_resource(aws_rs)
        .expect("test state setup must be valid");
    state
        .upsert_resource(awscc_rs)
        .expect("test state setup must be valid");

    let directives_map = state.build_directives();
    let aws_id = ResourceId::with_provider_identity("aws", "s3.Bucket", "main", None);
    let awscc_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "main", None);

    assert!(directives_map.get(&aws_id).unwrap().force_delete);
    assert!(!directives_map.get(&awscc_id).unwrap().force_delete);
}

#[test]
fn test_build_saved_attrs_provider_scoped() {
    use carina_core::resource::{ConcreteValue, ResourceId, Value};

    let mut state = StateFile::new();
    let aws_rs = ResourceState::new("s3.Bucket", "main", "aws")
        .with_attribute("region".to_string(), serde_json::json!("us-east-1"));
    let awscc_rs = ResourceState::new("s3.Bucket", "main", "awscc")
        .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1"));

    state
        .upsert_resource(aws_rs)
        .expect("test state setup must be valid");
    state
        .upsert_resource(awscc_rs)
        .expect("test state setup must be valid");

    let saved = state.build_saved_attrs();
    let aws_id = ResourceId::with_provider_identity("aws", "s3.Bucket", "main", None);
    let awscc_id = ResourceId::with_provider_identity("awscc", "s3.Bucket", "main", None);

    assert_eq!(
        saved.get(&aws_id).unwrap().get("region"),
        Some(&Value::Concrete(ConcreteValue::String("us-east-1".into())))
    );
    assert_eq!(
        saved.get(&awscc_id).unwrap().get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "ap-northeast-1".into()
        )))
    );
}

#[test]
fn test_build_state_for_resource_existing() {
    use carina_core::resource::{ConcreteValue, Resource, Value};

    let mut state = StateFile::new();
    state
        .upsert_resource(
            ResourceState::try_applied("s3.Bucket", "my-bucket", "awscc", "my-bucket-id")
                .expect("fixture identifier is non-empty")
                .with_attribute("region".to_string(), serde_json::json!("ap-northeast-1")),
        )
        .expect("test state setup must be valid");

    let resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    let result = match state.build_state_for_resource(&resource.id) {
        Current::Present(present) => present.state().clone(),
        Current::Absent(state) => panic!("expected present state, got {state:?}"),
        Current::CreateInFlight(_pending) => panic!("fixture cannot be in flight"),
        Current::CreateOutcomeUnknown(_unknown) => {
            panic!("fixture cannot have an unknown create outcome")
        }
        Current::MissingIdentifier(state) => {
            panic!("expected valid provider identifier, got {state:?}")
        }
    };

    assert!(result.exists);
    assert_eq!(result.identifier, Some("my-bucket-id".to_string()));
    assert_eq!(
        result.attributes.get("region"),
        Some(&Value::Concrete(ConcreteValue::String(
            "ap-northeast-1".into()
        )))
    );
}

#[test]
fn persisted_secret_hash_enters_plan_state_as_a_typed_placeholder() {
    use carina_core::resource::{DeferredValue, Resource, Value};

    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mut state = StateFile::new();
    state
        .upsert_resource(
            ResourceState::try_applied("test.resource", "old", "mock", "mock-id")
                .expect("fixture identifier is non-empty")
                .with_attribute("comment".to_string(), structural_legacy_secret(hash)),
        )
        .expect("test state setup must be valid");

    let resource = Resource::with_provider("mock", "test.resource", "old", None);
    let loaded = match state.build_state_for_resource(&resource.id) {
        Current::Present(present) => present.state().clone(),
        Current::Absent(state) => panic!("expected present state, got {state:?}"),
        Current::CreateInFlight(_pending) => panic!("fixture cannot be in flight"),
        Current::CreateOutcomeUnknown(_unknown) => {
            panic!("fixture cannot have an unknown create outcome")
        }
        Current::MissingIdentifier(state) => {
            panic!("expected valid provider identifier, got {state:?}")
        }
    };
    let saved_attrs = state.build_saved_attrs();

    for loaded in [
        loaded.attributes.get("comment"),
        saved_attrs
            .get(&resource.id)
            .and_then(|attributes| attributes.get("comment")),
    ] {
        match loaded {
            Some(Value::Deferred(DeferredValue::RedactedSecret { hash: loaded })) => {
                assert!(
                    loaded.encoded_digest().starts_with("argon2-legacy:"),
                    "state load must retain the typed explicit legacy digest",
                );
            }
            other => panic!("persisted secret hash lost its type at the state boundary: {other:?}"),
        }
    }
}

#[test]
fn persisted_attribute_maps_are_the_typed_value_decode_boundary() {
    use carina_core::resource::{DeferredValue, Value};

    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let tag = structural_legacy_secret(hash);
    let row =
        ResourceState::new("test.resource", "main", "mock").with_attribute("token", tag.clone());
    let deposed = DeposedInstance {
        key: DeposedKey::new_unique(),
        identifier: "old-id".to_string(),
        provider_instance: None,
        attributes: persisted_attributes_from_state_json(&HashMap::from([(
            "token".to_string(),
            tag,
        )])),
        dependency_bindings: Default::default(),
    };

    for decoded in [
        row.persisted_attributes()
            .expect("fixture has persisted attributes")
            .to_values(),
        deposed.persisted_attributes().to_values(),
    ] {
        assert!(matches!(
            decoded.get("token"),
            Some(Value::Deferred(DeferredValue::RedactedSecret { hash: loaded }))
                if loaded.encoded_digest().starts_with("argon2-legacy:")
        ));
    }
}

#[test]
fn state_file_persisted_attribute_type_preserves_serialized_bytes() {
    let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let tag = structural_legacy_secret(hash);
    let mut row = ResourceState::try_applied("test.resource", "main", "mock", "current-id")
        .expect("fixture identifier is non-empty")
        .with_attribute("token", tag.clone());
    row.deposed.push(DeposedInstance {
        key: DeposedKey::new_unique(),
        identifier: "old-id".to_string(),
        provider_instance: None,
        attributes: persisted_attributes_from_state_json(&HashMap::from([(
            "token".to_string(),
            tag,
        )])),
        dependency_bindings: Default::default(),
    });

    let mut state = StateFile::new();
    state.lineage = "typed-persisted-attributes".to_string();
    state
        .upsert_resource(row)
        .expect("test state setup must be valid");
    let before = serde_json::to_vec(&state).expect("serialize initial state");
    let reloaded =
        StateFile::from_json_str(std::str::from_utf8(&before).expect("serialized state is UTF-8"))
            .expect("reload serialized state");

    let current: &PersistedAttributes = reloaded.resources()[0]
        .persisted_attributes()
        .expect("reloaded fixture has persisted attributes");
    let deposed: &PersistedAttributes = &reloaded.resources()[0].deposed[0].attributes;
    assert!(matches!(
        current.get("token"),
        Some(Value::Deferred(DeferredValue::RedactedSecret { .. }))
    ));
    assert!(matches!(
        deposed.get("token"),
        Some(Value::Deferred(DeferredValue::RedactedSecret { .. }))
    ));
    assert_eq!(
        serde_json::to_vec(&reloaded).expect("serialize reloaded state"),
        before,
        "changing the in-memory attribute type must not change state-file bytes",
    );
}

#[test]
fn test_build_state_for_resource_not_found() {
    let state = StateFile::new();
    let resource =
        carina_core::resource::Resource::with_provider("awscc", "s3.Bucket", "missing", None);
    let result = match state.build_state_for_resource(&resource.id) {
        Current::Absent(state) => state.into_state(),
        Current::Present(present) => {
            panic!("expected absent state, got {:?}", present.state())
        }
        Current::CreateInFlight(_pending) => panic!("fixture cannot be in flight"),
        Current::CreateOutcomeUnknown(_unknown) => {
            panic!("fixture cannot have an unknown create outcome")
        }
        Current::MissingIdentifier(state) => {
            panic!("expected absent state, got {state:?}")
        }
    };

    assert!(!result.exists);
    assert!(result.identifier.is_none());
    assert!(result.attributes.is_empty());
}

#[test]
fn v15_loader_classifies_none_present_and_missing_without_flattening() {
    use carina_core::resource::Current;

    let absent_id = ResourceId::with_provider_identity("mock", "test.resource", "absent", None);
    let present_id = ResourceId::with_provider_identity("mock", "test.resource", "present", None);
    let missing_id = ResourceId::with_provider_identity("mock", "test.resource", "missing", None);
    let mut state = StateFile::new();
    state
        .upsert_resource(
            ResourceState::try_applied("test.resource", "present", "mock", "provider-present")
                .expect("fixture identifier is non-empty"),
        )
        .expect("present fixture row is valid");
    let mut missing_row = ResourceState::new("test.resource", "missing", "mock");
    missing_row.set_persisted_attribute("name", serde_json::json!("legacy-without-id"));
    state
        .upsert_resource(missing_row)
        .expect("identifier-less fixture row is valid v15 state");

    let absent = state.build_state_for_resource(&absent_id);
    let present = state.build_state_for_resource(&present_id);
    let missing = state.build_state_for_resource(&missing_id);

    assert!(matches!(absent, Current::Absent(ref state) if !state.state().exists));
    assert!(matches!(
        present,
        Current::Present(ref state) if state.identifier().as_ref() == "provider-present"
    ));
    assert!(matches!(missing, Current::MissingIdentifier(ref state) if state.state().exists));
}

#[test]
fn v15_none_is_absent_even_when_address_metadata_is_retained() {
    use carina_core::resource::Current;

    let json = serde_json::json!({
        "version": 15,
        "serial": 1,
        "lineage": "exact-shell",
        "carina_version": "test",
        "resources": [
            {
                "resource_type": "test.Resource",
                "identity": "shell",
                "provider": "mock",
                "current": { "kind": "none" },
                "deposed": [{
                    "key": "00000000-0000-4000-8000-000000000001",
                    "identifier": "old",
                    "attributes": {},
                    "dependency_bindings": []
                }]
            },
            {
                "resource_type": "test.Resource",
                "identity": "malformed",
                "provider": "mock",
                "current": { "kind": "none" },
                "binding": "still_current",
                "deposed": [{
                    "key": "00000000-0000-4000-8000-000000000002",
                    "identifier": "old",
                    "attributes": {},
                    "dependency_bindings": []
                }]
            }
        ],
        "exports": {}
    });
    let state = check_and_migrate(&json.to_string())
        .expect("v15 state must load")
        .into_state();
    let shell = ResourceId::with_provider_identity("mock", "test.Resource", "shell", None);
    let malformed = ResourceId::with_provider_identity("mock", "test.Resource", "malformed", None);

    assert!(matches!(
        state.build_state_for_resource(&shell),
        Current::Absent(ref absent) if !absent.state().exists
    ));
    assert!(matches!(
        state.build_state_for_resource(&malformed),
        Current::Absent(ref absent) if !absent.state().exists
    ));
}

#[test]
fn in_flight_state_keeps_the_persisted_provider_route() {
    use carina_core::resource::Current;

    let json = serde_json::json!({
        "version": 15,
        "serial": 1,
        "lineage": "persisted-route",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "pending",
            "provider": "mock",
            "current": {
                "kind": "create_in_flight",
                "envelope": {
                    "token": "opaque-token",
                    "identifier": null,
                    "write_only": {},
                    "secrets": {}
                },
                "started_at": "2026-10-23T08:00:00Z"
            },
            "directives": { "provider_instance": "issuing" }
        }],
        "exports": {}
    });
    let state = check_and_migrate(&json.to_string())
        .expect("v15 state must load")
        .into_state();
    let desired =
        ResourceId::with_provider_identity("mock", "test.Resource", "pending", Some("new".into()));

    let Current::CreateInFlight(pending) = state.build_state_for_resource(&desired) else {
        panic!("persisted in-flight row must remain visible across a route mismatch");
    };
    assert_eq!(pending.id().provider_instance.as_deref(), Some("issuing"));
}

#[test]
fn v15_rejects_a_route_embedded_inside_an_in_flight_record() {
    let json = serde_json::json!({
        "version": 15,
        "serial": 1,
        "lineage": "mismatched-persisted-route",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "pending",
            "provider": "mock",
            "current": {
                "kind": "create_in_flight",
                "id": {
                    "provider": "mock",
                    "resource_type": "test.Resource",
                    "identity": "another-resource"
                },
                "envelope": {
                    "token": "opaque-token",
                    "identifier": null,
                    "write_only": {},
                    "secrets": {}
                },
                "started_at": "2026-10-23T08:00:00Z"
            }
        }],
        "exports": {}
    });

    assert!(
        check_and_migrate(&json.to_string()).is_err(),
        "the ID-less envelope must derive its route only from the containing state row"
    );
}

#[test]
fn v15_rejects_token_plus_attributes_and_identifier_plus_token() {
    let base = serde_json::json!({
        "version": 15,
        "serial": 1,
        "lineage": "exclusive-current",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "pending",
            "provider": "mock",
            "current": {
                "kind": "create_in_flight",
                "envelope": {
                    "token": "opaque-token",
                    "identifier": null,
                    "write_only": {},
                    "secrets": {}
                },
                "started_at": "2026-10-23T08:00:00Z"
            }
        }],
        "exports": {}
    });

    let mut token_plus_attributes = base.clone();
    token_plus_attributes["resources"][0]["current"]["attributes"] =
        serde_json::json!({"name": "must-not-coexist"});
    assert!(
        check_and_migrate(&token_plus_attributes.to_string()).is_err(),
        "an in-flight current slot cannot also carry resource attributes"
    );

    let mut identifier_plus_token = base;
    identifier_plus_token["resources"][0]["current"]["identifier"] =
        serde_json::json!("applied-id");
    assert!(
        check_and_migrate(&identifier_plus_token.to_string()).is_err(),
        "an in-flight current slot cannot also carry an applied identifier"
    );
}

#[test]
fn in_flight_load_preserves_route_started_at_and_deposed_generations() {
    use carina_core::resource::Current;

    let json = serde_json::json!({
        "version": 15,
        "serial": 1,
        "lineage": "complete-in-flight-load",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "pending",
            "provider": "mock",
            "current": {
                "kind": "create_in_flight",
                "envelope": {
                    "token": "opaque-token",
                    "identifier": "issued-id",
                    "write_only": {"password": "sent"},
                    "secrets": {}
                },
                "started_at": "2026-10-23T08:00:00Z"
            },
            "directives": {"provider_instance": "issuing"},
            "deposed": [{
                "key": "00000000-0000-4000-8000-000000000001",
                "identifier": "old-id",
                "provider_instance": "old-route",
                "attributes": {"name": "old"},
                "dependency_bindings": []
            }]
        }],
        "exports": {}
    });
    let state = check_and_migrate(&json.to_string())
        .expect("v15 in-flight state must load")
        .into_state();
    let desired = ResourceId::with_provider_identity(
        "mock",
        "test.Resource",
        "pending",
        Some("issuing".into()),
    );
    let Current::CreateInFlight(pending) = state.build_state_for_resource(&desired) else {
        panic!("expected in-flight current");
    };

    assert_eq!(pending.id(), &desired);
    assert_eq!(pending.started_at().as_str(), "2026-10-23T08:00:00Z");
    assert_eq!(
        pending.identifier().map(|value| value.as_str()),
        Some("issued-id")
    );
    let row = state
        .find_resource_for_id(&desired)
        .expect("persisted row remains addressable");
    assert_eq!(row.deposed.len(), 1);
    assert_eq!(row.deposed[0].identifier, "old-id");
    assert_eq!(
        row.deposed[0].provider_instance.as_deref(),
        Some("old-route")
    );
}

#[test]
fn test_build_state_for_resource_without_identifier() {
    let mut state = StateFile::new();
    // Resource in state but without identifier (not yet created)
    state
        .upsert_resource(
            ResourceState::new("s3.Bucket", "pending", "awscc")
                .with_attribute("region".to_string(), serde_json::json!("us-east-1")),
        )
        .expect("test state setup must be valid");

    let resource =
        carina_core::resource::Resource::with_provider("awscc", "s3.Bucket", "pending", None);
    let result = state.build_state_for_resource(&resource.id);

    assert!(matches!(
        result,
        Current::MissingIdentifier(ref state)
            if state.state().exists && state.state().identifier.is_none()
    ));
}

#[test]
fn build_state_for_resource_does_not_hide_current_content_behind_deposed_generations() {
    let id = ResourceId::with_provider_identity("mock", "test.resource", "current", None);
    let mut row = ResourceState::new("test.resource", "current", "mock")
        .with_attribute("name", serde_json::json!("current"));
    row.binding = Some("current".to_string());
    row.deposed.push(DeposedInstance::from_persisted_json(
        DeposedKey::new_unique(),
        "old-id".to_string(),
        None,
        HashMap::new(),
        Default::default(),
    ));
    let mut state = StateFile::new();
    state
        .upsert_resource(row)
        .expect("test state setup must be valid");

    assert!(matches!(
        state.build_state_for_resource(&id),
        Current::MissingIdentifier(_state)
    ));
}

#[test]
fn removing_last_deposed_generation_retains_malformed_current_instance() {
    let identity = ResourceIdentity::from("current");
    let id = ResourceId::with_provider_identity("mock", "test.resource", identity.clone(), None);
    let key = DeposedKey::new_unique();
    let mut row = ResourceState::new("test.resource", identity, "mock")
        .with_attribute("name", serde_json::json!("current"));
    row.binding = Some("current".to_string());
    row.deposed.push(DeposedInstance::from_persisted_json(
        key.clone(),
        "old-id".to_string(),
        None,
        HashMap::new(),
        Default::default(),
    ));
    let mut state = StateFile::new();
    state
        .upsert_resource(row)
        .expect("test state setup must be valid");

    let removed = state.remove_deposed_generation("mock", "test.resource", "current", &key);

    assert!(removed.is_some());
    let retained = state
        .find_resource("mock", "test.resource", "current")
        .expect("malformed current instance must remain visible");
    assert!(retained.deposed.is_empty());
    assert!(matches!(
        state.build_state_for_resource(&id),
        Current::MissingIdentifier(_state)
    ));
}

#[test]
fn removing_last_deposed_generation_prunes_explicit_false_shell() {
    let key = DeposedKey::new_unique();
    let mut row = ResourceState::new("test.resource", "shell", "mock");
    row.directives.set_prevent_destroy(false);
    row.deposed.push(DeposedInstance::from_persisted_json(
        key.clone(),
        "old-id".to_string(),
        None,
        HashMap::new(),
        PersistedDestroyDependencies::default(),
    ));
    let mut state = StateFile::new();
    state
        .upsert_resource(row)
        .expect("test state setup must be valid");

    let removed = state.remove_deposed_generation("mock", "test.resource", "shell", &key);

    assert!(removed.is_some());
    assert!(
        state
            .find_resource("mock", "test.resource", "shell")
            .is_none(),
        "an explicit-false row with no current or deposed instance is still an empty shell"
    );
}

#[test]
fn removing_last_deposed_generation_retains_explicit_true_shell() {
    let key = DeposedKey::new_unique();
    let mut row = ResourceState::new("test.resource", "shell", "mock");
    row.directives.set_prevent_destroy(true);
    row.deposed.push(DeposedInstance::from_persisted_json(
        key.clone(),
        "old-id".to_string(),
        None,
        HashMap::new(),
        PersistedDestroyDependencies::default(),
    ));
    let mut state = StateFile::new();
    state
        .upsert_resource(row)
        .expect("test state setup must be valid");

    let removed = state.remove_deposed_generation("mock", "test.resource", "shell", &key);

    assert!(removed.is_some());
    assert!(
        state
            .find_resource("mock", "test.resource", "shell")
            .is_some(),
        "an explicit-true shell must retain the old non-default semantics"
    );
}

#[test]
fn test_from_provider_state_stores_binding_and_dependencies() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let mut resource = Resource::with_provider("awscc", "ec2.Subnet", "my-subnet", None);
    resource.binding = Some("my_subnet".to_string());
    resource.set_attr(
        "vpc_id".to_string(),
        Value::resource_ref("my_vpc".to_string(), "vpc_id".to_string(), vec![]),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("subnet-123".to_string()),
        attributes: [(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-abc".into())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let resolved_resource = ResolvedResource::from_resolved_parts(
        resource.clone().into_resolved_dependency_phase_for_test(),
        fixture_dependencies(["my_vpc"]),
        Default::default(),
    )
    .with_saved_destroy_dependencies(
        fixture_persisted_dependencies(["my_vpc"]),
        Default::default(),
    );
    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved_resource,
        &provider_state,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();
    assert_eq!(rs.binding, Some("my_subnet".to_string()));
    assert_eq!(
        rs.dependency_bindings,
        fixture_persisted_dependencies(["my_vpc"])
    );
}

#[test]
fn state_json_cannot_persist_ignore_changes_paths() {
    let runtime: Directives = serde_json::from_value(serde_json::json!({
        "prevent_destroy": {"kind": "explicit", "value": true},
        "ignore_changes": [{
            "segments": [{ "kind": "attribute", "value": "comment" }]
        }]
    }))
    .expect("runtime directives decode");
    assert_eq!(runtime.ignore_changes.len(), 1);

    let mut row = ResourceState::new("test.Widget", "example", "mock");
    row.directives = PersistedDirectives::from_resolved(&runtime, Default::default());
    let encoded = serde_json::to_string(&row).expect("state row serializes");

    assert!(row.directives.explicitly_prevents_destroy());
    assert!(!encoded.contains("ignore_changes"), "{encoded}");
    assert!(!encoded.contains("comment"), "{encoded}");
}

#[test]
fn test_build_orphans_pairs_provider_state_with_typed_host_metadata() {
    use carina_core::resource::ResourceId;

    let mut state = StateFile::new();
    let mut rs = ResourceState::try_applied("ec2.Subnet", "orphan-subnet", "awscc", "subnet-123")
        .expect("fixture identifier is non-empty");
    rs.binding = Some("my_subnet".to_string());
    rs.dependency_bindings = fixture_persisted_dependencies(["my_vpc"]);
    rs.directives.set_prevent_destroy(true);
    rs.directives.depends_on = fixture_persisted_dependencies(["security_group"]);
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let desired_ids = std::collections::HashSet::new();
    let orphans = state.build_orphans(&desired_ids);

    let id = ResourceId::with_provider_identity("awscc", "ec2.Subnet", "orphan-subnet", None);
    let mut loaded = orphans.into_iter();
    let (loaded_id, loaded_orphan) = loaded.next().expect("orphan remains visible");
    assert_eq!(loaded_id, id);
    assert!(loaded.next().is_none());
    let orphan = loaded_orphan;
    let Current::Present(present) = orphan.current() else {
        panic!("expected valid orphan identifier: {orphan:?}");
    };
    assert_eq!(present.identifier().as_str(), "subnet-123");
    let orphan_state = match orphan.current() {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::CreateInFlight(_pending) => panic!("fixture cannot be in flight"),
        Current::CreateOutcomeUnknown(_unknown) => {
            panic!("fixture cannot have an unknown create outcome")
        }
        Current::MissingIdentifier(state) => state.state(),
    };
    assert!(orphan_state.exists);
    assert!(
        orphan_state
            .attributes
            .keys()
            .all(|key| !key.starts_with('_')),
        "provider-facing orphan state must not contain host metadata"
    );
    assert_eq!(orphan.binding(), Some("my_subnet"));
    assert!(orphan.directives().explicitly_prevents_destroy());
    assert_eq!(
        orphan.directives().depends_on,
        fixture_persisted_dependencies(["security_group"])
    );
    assert_eq!(
        orphan_state.dependency_bindings,
        fixture_dependencies(["my_vpc"])
    );

    let fresh = carina_core::resource::State::existing(
        id.clone(),
        HashMap::from([(
            "cidr_block".to_string(),
            carina_core::resource::Value::Concrete(carina_core::resource::ConcreteValue::String(
                "10.0.1.0/24".into(),
            )),
        )]),
    )
    .with_identifier("subnet-123");
    let refreshed = orphan.with_refreshed_state(fresh);
    let refreshed_state = match refreshed.current() {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::CreateInFlight(_pending) => panic!("fixture cannot be in flight"),
        Current::CreateOutcomeUnknown(_unknown) => {
            panic!("fixture cannot have an unknown create outcome")
        }
        Current::MissingIdentifier(state) => state.state(),
    };
    assert_eq!(refreshed.binding(), Some("my_subnet"));
    assert_eq!(refreshed_state.id, id);
    assert!(refreshed.directives().explicitly_prevents_destroy());
    assert_eq!(
        refreshed.directives().depends_on,
        fixture_persisted_dependencies(["security_group"])
    );
    assert_eq!(
        refreshed_state.dependency_bindings,
        fixture_dependencies(["my_vpc"])
    );
    assert!(refreshed_state.attributes.contains_key("cidr_block"));
}

#[test]
fn refreshing_loaded_orphan_to_absent_cannot_retain_an_identifier() {
    let id = ResourceId::with_provider_identity("mock", "test.Resource", "orphan", None);
    let mut state = StateFile::new();
    state
        .upsert_resource(
            ResourceState::try_applied("test.Resource", "orphan", "mock", "resource-123")
                .expect("fixture identifier is non-empty"),
        )
        .expect("test state setup must be valid");
    let (_id, loaded) = state
        .build_orphans(&HashSet::new())
        .into_iter()
        .next()
        .expect("orphan remains visible");

    let refreshed = loaded.with_refreshed_state(State::not_found(id));
    match refreshed.current() {
        Current::Absent(state) => assert!(state.state().identifier.is_none()),
        Current::Present(present) => panic!(
            "an absent refreshed orphan retained identifier {:?}",
            present.identifier()
        ),
        Current::CreateInFlight(_pending) => panic!("fixture cannot be in flight"),
        Current::CreateOutcomeUnknown(_unknown) => {
            panic!("fixture cannot have an unknown create outcome")
        }
        Current::MissingIdentifier(state) => {
            panic!("an absent refreshed orphan remained existing: {state:?}")
        }
    }
}

#[test]
fn build_orphans_retains_row_without_identifier_as_typed_invalid_orphan() {
    let id = ResourceId::with_provider_identity("mock", "test.resource", "legacy", None);
    let json = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "missing-provider-identifier",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "test.resource",
            "identity": "legacy",
            "provider": "mock",
            "current": {
                "kind": "missing_identifier",
                "attributes": { "name": "legacy" },
                "nested_write_only": {}
            }
        }],
        "exports": {}
    });
    let state = check_and_migrate(&json.to_string())
        .expect("load state JSON")
        .into_state();

    let loaded = state.build_orphans(&HashSet::new());

    let mut entries = loaded.iter();
    let (loaded_id, loaded_orphan) = entries.next().expect("invalid orphan remains visible");
    assert_eq!(loaded_id, &id);
    assert!(entries.next().is_none());
    let orphan = loaded_orphan;
    let state = match orphan.current() {
        Current::Absent(state) => state.state(),
        Current::Present(present) => present.state(),
        Current::CreateInFlight(_pending) => panic!("fixture cannot be in flight"),
        Current::CreateOutcomeUnknown(_unknown) => {
            panic!("fixture cannot have an unknown create outcome")
        }
        Current::MissingIdentifier(state) => state.state(),
    };
    assert!(state.exists);
    assert!(state.identifier.is_none());
}

#[test]
fn orphan_provider_refresh_demotes_aligned_list_explicit_tree() {
    let id = ResourceId::with_provider_identity("mock", "test.Resource", "orphan", None);
    let mut row = ResourceState::try_applied("test.Resource", "orphan", "mock", "resource-123")
        .expect("fixture identifier is non-empty");
    row.explicit = ExplicitFields::ListElements {
        elements: vec![ExplicitFields::Struct {
            children: HashMap::from([("name".to_string(), ExplicitFields::Leaf)]),
        }],
    };
    let fresh = State::existing(
        id,
        HashMap::from([(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("fresh".into())),
        )]),
    )
    .with_identifier("resource-123");

    let refreshed = row
        .refreshable()
        .unwrap()
        .refreshed_from_provider_state(&fresh, None)
        .unwrap();

    assert_eq!(
        refreshed.explicit,
        ExplicitFields::List {
            element: Box::new(ExplicitFields::Struct {
                children: HashMap::from([("name".to_string(), ExplicitFields::Leaf)]),
            }),
        }
    );
}

#[test]
fn orphan_provider_refresh_never_promotes_provider_keys_into_authoring_metadata() {
    let id = ResourceId::with_provider_identity("mock", "test.Resource", "orphan", None);
    let fresh = State::existing(
        id,
        HashMap::from([(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        )]),
    )
    .with_identifier("resource-123");

    let mut unrecorded =
        ResourceState::try_applied("test.Resource", "orphan", "mock", "resource-123")
            .expect("fixture identifier is non-empty");
    unrecorded.explicit = ExplicitFields::Unrecorded;
    let refreshed_unrecorded = unrecorded
        .refreshable()
        .unwrap()
        .refreshed_from_provider_state(&fresh, None)
        .unwrap();
    assert_eq!(refreshed_unrecorded.explicit, ExplicitFields::Unrecorded);

    let mut recorded_empty =
        ResourceState::try_applied("test.Resource", "orphan", "mock", "resource-123")
            .expect("fixture identifier is non-empty");
    recorded_empty.explicit = ExplicitFields::Struct {
        children: HashMap::new(),
    };
    let refreshed_recorded_empty = recorded_empty
        .refreshable()
        .unwrap()
        .refreshed_from_provider_state(&fresh, None)
        .unwrap();
    assert_eq!(
        refreshed_recorded_empty.explicit,
        ExplicitFields::Struct {
            children: HashMap::new()
        }
    );
}

#[test]
fn v10_reserved_attributes_are_removed_at_load_before_they_reach_consumers() {
    use carina_core::schema::{AttributeSchema, AttributeType};

    let legacy_keys = [
        "_binding",
        "_dependency_bindings",
        "_virtual",
        "_type",
        "_default_tag_keys",
    ];
    let attributes = serde_json::Map::from_iter(
        std::iter::once(("name".to_string(), serde_json::json!("kept"))).chain(
            legacy_keys
                .iter()
                .map(|key| ((*key).to_string(), serde_json::json!("legacy"))),
        ),
    );
    let explicit_children = serde_json::Map::from_iter(
        std::iter::once(("name".to_string(), serde_json::json!({ "kind": "leaf" }))).chain(
            legacy_keys
                .iter()
                .map(|key| ((*key).to_string(), serde_json::json!({ "kind": "leaf" }))),
        ),
    );
    let json = serde_json::json!({
        "version": 10,
        "serial": 1,
        "lineage": "legacy-reserved-attributes",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "legacy",
            "provider": "mock",
            "identifier": "resource-123",
            "attributes": attributes,
            "explicit": {
                "kind": "struct",
                "children": explicit_children,
            },
            "deposed": [{
                "key": "old",
                "identifier": "resource-old",
                "attributes": {
                    "name": "old",
                    "_binding": "legacy_binding",
                    "_type": "test.Resource",
                },
                "dependency_bindings": [],
            }],
        }],
        "exports": {},
    });

    let state = check_and_migrate(&json.to_string())
        .expect("legacy state should migrate")
        .into_state();
    let desired = Resource::with_provider("mock", "test.Resource", "legacy", None).with_attribute(
        "name",
        Value::Concrete(ConcreteValue::String("kept".into())),
    );
    let mut schemas = carina_core::schema::SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .expect("legacy fixture schema is valid"),
    );
    let resolved = resolved(&desired);
    let id = resolved.id.clone();
    let current = match state.build_state_for_resource(&id) {
        Current::Absent(state) => carina_core::resource::Current::Absent(state),
        Current::Present(present) => carina_core::resource::Current::Present(present),
        Current::CreateInFlight(pending) => carina_core::resource::Current::CreateInFlight(pending),
        Current::CreateOutcomeUnknown(unknown) => {
            carina_core::resource::Current::CreateOutcomeUnknown(unknown)
        }
        Current::MissingIdentifier(state) => {
            carina_core::resource::Current::MissingIdentifier(state)
        }
    };
    let plan_input_states = carina_core::resource::into_plan_input_current_map::<
        carina_core::resource::ModuleSource,
        carina_core::resource::UnexpandedDependencyFields,
    >(HashMap::from([(id.clone(), current)]), &schemas, &[]);
    let plan = carina_core::differ::create_plan_for_test(
        &[resolved],
        &[],
        &carina_core::provider::ProviderRouter::new(),
        carina_core::differ::PlanPriorStates::new(
            &plan_input_states,
            &carina_core::differ::Orphans::new(),
        ),
        &state.build_directives(),
        &schemas,
        &state.build_saved_attrs().lift(&schemas),
        &state.build_explicit(),
        &[],
    )
    .expect("legacy state plan should build");
    assert!(
        !plan
            .effects()
            .iter()
            .any(|effect| matches!(effect, carina_core::effect::Effect::Update { .. })),
        "legacy host attributes must not produce an Update or provider patch: {:?}",
        plan.effects()
    );

    let row = state
        .find_resource("mock", "test.Resource", "legacy")
        .expect("legacy row should remain");
    assert_eq!(
        row.persisted_attributes()
            .expect("legacy row has persisted attributes")
            .keys()
            .collect::<Vec<_>>(),
        vec![&"name".to_string()]
    );
    assert_eq!(
        row.deposed[0]
            .persisted_attributes()
            .keys()
            .collect::<Vec<_>>(),
        vec![&"name".to_string()]
    );
    assert_explicit_struct_keys(&row.explicit, &["name"]);
    assert!(!row.format_generations_for_state_show().contains("_binding"));
    assert!(!row.format_generations_for_state_show().contains("_type"));

    let generation = carina_core::effect::EffectGeneration::Deposed(row.deposed[0].key.clone());
    let delete = carina_core::effect::Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(id.clone()),
        identifier: crate::fixture_applied_identifier("resource-old"),
        generation: generation.clone(),
        directives: Directives::default(),
        binding: None,
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    };
    let mut delete_plan = carina_core::plan::Plan::new();
    delete_plan.add(delete.clone());
    let mut historical =
        carina_core::detail_rows::HistoricalDisplayAttributes::for_plan(&delete_plan);
    historical.set_delete_from_persisted(
        carina_core::effect::DeletedInstanceKey::new(
            id.clone(),
            generation,
            "resource-old".to_string(),
        ),
        row.deposed[0].persisted_attributes(),
    );
    let (schemas, provider) = carina_core::provider::comparison_fixture_for_test(
        &carina_core::schema::SchemaRegistry::new(),
        std::iter::once(&id),
    );
    let delete_rows = carina_core::detail_rows::build_detail_rows_by_instance(
        &delete,
        Some(&schemas),
        carina_core::detail_rows::DetailLevel::Full,
        &historical,
        None,
        &provider,
    )
    .expect("delete display must use the production detail-row builder");
    assert_eq!(
        delete_rows,
        vec![carina_core::detail_rows::DetailRow::Attribute {
            key: "name".to_string(),
            value: carina_core::value::render_value(&Value::Concrete(ConcreteValue::String(
                "old".into(),
            ))),
            ref_binding: None,
            annotation: None,
        }]
    );
}

#[test]
fn v12_persisted_values_migrate_to_tagged_enum_encoding_in_every_state_tree() {
    let state = finish_legacy_dependency_migration(
        check_and_migrate(include_str!(
            "../../tests/fixtures/state-v12-tagged-enum-json.json"
        ))
        .expect("v12 fixture should migrate")
        .into_state(),
    );

    assert_eq!(state.version, StateFile::CURRENT_VERSION);
    let row = state
        .find_resource("mock", "test.Resource", "main")
        .expect("fixture resource exists");

    for attributes in [
        row.persisted_attributes()
            .expect("migrated row has persisted attributes"),
        row.deposed[0].persisted_attributes(),
    ] {
        assert!(matches!(
            attributes.get("legacy_enum"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(_)))
        ));
        assert!(matches!(
            attributes.get("dollar_map"),
            Some(Value::Concrete(ConcreteValue::Map(map))) if map.contains_key("$x")
        ));
    }
    assert_eq!(
        row.format_attribute_for_state_show("dollar_map", true),
        Some("{\n  \"$x\": \"current\"\n}".to_string())
    );

    let remote = published_exports_from_state(&state).build_remote_bindings();
    assert!(matches!(
        remote.get("legacy_enum"),
        Some(Value::Concrete(ConcreteValue::CanonicalEnum(_)))
    ));
    assert!(matches!(
        remote.get("dollar_map"),
        Some(Value::Concrete(ConcreteValue::Map(map))) if map.contains_key("$x")
    ));

    let migrated_json = serde_json::to_value(&state).expect("migrated state serializes");
    let current = &migrated_json["resources"][0]["current"]["attributes"];
    let deposed = &migrated_json["resources"][0]["deposed"][0]["attributes"];
    let exports = &migrated_json["exports"];
    for tree in [current, deposed, exports] {
        assert!(tree["legacy_enum"].get("$enum").is_some());
        assert!(tree["dollar_map"].get("$$x").is_some());
        assert!(tree["dollar_map"].get("$x").is_none());
    }
}

#[test]
fn v13_resource_secret_markers_migrate_to_structural_tags_but_exports_stay_literal() {
    let marker = valid_legacy_marker_shaped_string("v13-self-contained");
    let digest = marker
        .strip_prefix("_secret:")
        .expect("test marker uses the legacy boundary prefix");
    let fixture = include_str!("../../tests/fixtures/state-v13-structural-secret-marker.json")
        .replace("__SELF_CONTAINED_MARKER__", &marker);
    let outcome = check_and_migrate(&fixture).expect("v13 fixture should migrate");

    assert_eq!(
        outcome.migration(),
        Some(MigrationInfo {
            from: 13,
            to: StateFile::CURRENT_VERSION,
        })
    );
    let state = finish_legacy_dependency_migration(outcome.into_state());
    let row = state
        .find_resource("mock", "test.Resource", "main")
        .expect("fixture resource exists");
    for attributes in [
        row.persisted_attributes()
            .expect("migrated row has persisted attributes"),
        row.deposed[0].persisted_attributes(),
    ] {
        assert!(matches!(
            attributes.get("self_contained"),
            Some(Value::Deferred(DeferredValue::RedactedSecret { .. }))
        ));
        assert!(matches!(
            attributes.get("raw_legacy"),
            Some(Value::Deferred(DeferredValue::RedactedSecret { .. }))
        ));
    }
    assert!(matches!(
        row.persisted_attributes()
            .expect("migrated row has persisted attributes")
            .get("malformed"),
        Some(Value::Concrete(ConcreteValue::String(value)))
            if value.expose_plaintext() == "_secret:argon2:not-a-hash"
    ));
    assert!(matches!(
        row.persisted_attributes()
            .expect("migrated row has persisted attributes")
            .get("nested"),
        Some(Value::Concrete(ConcreteValue::Map(values)))
            if matches!(
                values.get("$secret"),
                Some(Value::Concrete(ConcreteValue::String(value)))
                    if value.expose_plaintext() == "user-value"
            )
    ));

    let encoded = serde_json::to_value(&state).expect("migrated state serializes");
    let current = &encoded["resources"][0]["current"]["attributes"];
    let deposed = &encoded["resources"][0]["deposed"][0]["attributes"];
    assert_eq!(
        current["self_contained"],
        serde_json::json!({"$secret": digest})
    );
    assert_eq!(
        deposed["self_contained"],
        serde_json::json!({"$secret": digest})
    );
    assert!(
        current["explicit_legacy"]["$secret"]
            .as_str()
            .is_some_and(|value| value.starts_with("argon2-legacy:"))
    );
    assert!(
        current["raw_legacy"]["$secret"]
            .as_str()
            .is_some_and(|value| value.starts_with("argon2-legacy:"))
    );
    assert!(current["nested"]["items"][0].get("$secret").is_some());
    assert!(current["nested"]["map"]["value"].get("$secret").is_some());
    assert_eq!(current["nested"]["$$secret"], "user-value");

    assert_eq!(encoded["exports"]["self_contained"], marker);
    assert_eq!(
        encoded["exports"]["raw_legacy"],
        "_secret:argon2:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    );
    assert_eq!(
        encoded["exports"]["user_map"]["$$secret"],
        "export-user-value"
    );
    assert!(matches!(
        published_exports_from_state(&state)
            .build_remote_bindings()
            .get("self_contained"),
        Some(Value::Concrete(ConcreteValue::String(value)))
            if value.expose_plaintext() == marker
    ));
}

#[test]
fn v13_raw_row_salted_secret_still_verifies_after_structural_migration() {
    use argon2::Argon2;

    let plaintext = "pre-v12-row-secret";
    let material = serde_json::to_vec(&serde_json::json!(plaintext)).expect("secret material");
    let salt = b"carina:mock.test.Resource:main:raw_legacy";
    let mut output = [0_u8; 32];
    Argon2::default()
        .hash_password_into(&material, salt, &mut output)
        .expect("hash legacy fixture");
    let json = serde_json::json!({
        "version": 13,
        "serial": 1,
        "lineage": "v13-row-salt-verification",
        "carina_version": "0.8.0",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "main",
            "provider": "mock",
            "identifier": "current-id",
            "attributes": {
                "raw_legacy": format!("{LEGACY_SECRET_PREFIX}{}", hex::encode(output)),
            },
        }],
        "exports": {},
    });
    let state = finish_legacy_dependency_migration(
        check_and_migrate(&json.to_string())
            .expect("v13 raw legacy state migrates")
            .into_state(),
    );
    let hash = state.resources()[0]
        .persisted_attributes()
        .expect("migrated row has persisted attributes")
        .redacted_secret_hash("raw_legacy")
        .expect("raw legacy value remains typed");

    assert!(hash.verify(&material));
    assert!(
        serde_json::to_value(&state).expect("serialize migrated state")["resources"][0]["current"]
            ["attributes"]["raw_legacy"]
            .get("$secret")
            .is_some()
    );
}

#[test]
fn current_state_rejects_unknown_reserved_payloads_in_attributes_and_exports() {
    let base = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "reserved-payload-validation",
        "carina_version": "0.8.0",
        "resources": [],
        "exports": {},
    });
    let invalid = serde_json::json!({"$future": {}});

    let mut attribute_state = base.clone();
    attribute_state["resources"] = serde_json::json!([{
        "resource_type": "test.Resource",
        "identity": "main",
        "provider": "mock",
        "current": {
            "kind": "applied",
            "identifier": "current-id",
            "attributes": {"bad": invalid.clone()},
            "nested_write_only": {}
        },
    }]);

    let mut export_state = base;
    export_state["exports"] = serde_json::json!({"bad": invalid});

    for (state, expected) in [
        (attribute_state, "not valid persisted JSON"),
        (export_state, "host-reserved"),
    ] {
        let error = check_and_migrate(&state.to_string())
            .expect_err("unknown host payload must fail closed");
        assert!(
            error.to_string().contains(expected),
            "unexpected error: {error}"
        );
    }
}

#[test]
fn current_state_attribute_errors_include_the_attribute_and_reserved_payload_reason() {
    let mut state = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "attribute-error-detail",
        "carina_version": "test",
        "resources": [{
            "resource_type": "test.Resource",
            "identity": "main",
            "provider": "mock",
            "current": {
                "kind": "applied",
                "identifier": "current-id",
                "attributes": {"event_pattern": {"$foo": 1}},
                "nested_write_only": {}
            },
        }],
        "exports": {},
    });

    let message = invalid_state_message(check_and_migrate(&state.to_string()));
    assert!(
        message.contains("persisted attribute \"event_pattern\""),
        "{message}"
    );
    assert!(message.contains("host-reserved"), "{message}");
    assert!(message.contains("$foo"), "{message}");

    state["resources"][0]["current"]["attributes"] = serde_json::json!({});
    state["resources"][0]["deposed"] = serde_json::json!([{
        "key": "old-generation",
        "identifier": "old-id",
        "attributes": {
            "mode": {"$enum": {"identity": {"provider": "test"}, "api_value": "x"}}
        },
        "dependency_bindings": [],
    }]);
    let message = invalid_state_message(check_and_migrate(&state.to_string()));
    assert!(
        message.contains("persisted attribute \"mode\""),
        "{message}"
    );
    assert!(
        message.contains("malformed $enum persisted enum payload"),
        "{message}"
    );
}

#[test]
fn current_state_export_errors_include_the_export_name_and_payload_reason() {
    let state = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "export-error-detail",
        "carina_version": "test",
        "resources": [],
        "exports": {
            "broken_mode": {
                "$enum": {"identity": {"provider": "test"}, "api_value": "x"}
            }
        },
    });

    let message = invalid_state_message(check_and_migrate(&state.to_string()));
    assert!(
        message.contains("persisted export \"broken_mode\""),
        "{message}"
    );
    assert!(
        message.contains("malformed $enum persisted enum payload"),
        "{message}"
    );
}

#[test]
fn v8_state_migrates_to_current_with_empty_deposed() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 8,
        "serial": 5,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "identity": "vpc",
                "provider": "awscc",
                "identifier": "vpc-current",
                "attributes": { "vpc_id": "vpc-current" }
            }
        ]
    }"#;

    let outcome = check_and_migrate(json).expect("v8 state should migrate to current");
    let migration = outcome
        .migration
        .expect("v8 read should report a migration");
    assert_eq!(migration.from, 8);
    assert_eq!(migration.to, StateFile::CURRENT_VERSION);
    let row = outcome
        .state
        .find_resource("awscc", "ec2.Vpc", "vpc")
        .expect("resource should survive migration");
    let row_json = serde_json::to_value(row).expect("resource state should serialize");
    assert!(
        row_json.get("deposed").is_none(),
        "empty deposed vec should use serde default and skip serialization"
    );
}

#[test]
fn v11_state_migrates_to_current_and_upgrades_raw_legacy_secrets_at_row_load() {
    use super::check_and_migrate;

    assert_eq!(StateFile::CURRENT_VERSION, 21);
    let json = format!(
        r#"{{
        "version": 11,
        "serial": 5,
        "lineage": "v11-secret-lineage",
        "carina_version": "0.4.0",
        "resources": [
            {{
                "resource_type": "test.resource",
                "identity": "legacy",
                "provider": "mock",
                "identifier": "legacy-id",
                "attributes": {{
                    "comment": "{LEGACY_SECRET_PREFIX}{}"
                }}
            }}
        ]
    }}"#,
        "ab".repeat(32),
    );

    let outcome = check_and_migrate(&json).expect("v11 state should migrate to current");
    assert_eq!(
        outcome.migration(),
        Some(MigrationInfo {
            from: 11,
            to: StateFile::CURRENT_VERSION,
        })
    );
    assert_eq!(outcome.state().version, 20);
    let state = finish_legacy_dependency_migration(outcome.into_state());
    let row = state
        .find_resource("mock", "test.resource", "legacy")
        .expect("legacy row should survive migration");
    let encoded = row
        .persisted_attributes()
        .expect("migrated row has persisted attributes")
        .encoded_value("comment")
        .expect("legacy secret should remain persisted")
        .as_json();
    let digest = structural_secret_digest(encoded);
    assert!(
        digest.starts_with("argon2-legacy:"),
        "row load must make the legacy digest self-contained: {digest}",
    );
}

#[test]
fn v11_fixture_migrates_nested_tagged_values_and_dollar_keys() {
    let state = check_and_migrate(include_str!(
        "../../tests/fixtures/state-v11-nested-persisted-values.json"
    ))
    .expect("v11 fixture should migrate")
    .into_state();
    let row = state
        .find_resource("mock", "test.Resource", "main")
        .expect("fixture resource exists");
    let settings = row
        .persisted_attributes()
        .expect("migrated row has persisted attributes")
        .get("settings")
        .expect("settings should decode");
    let Value::Concrete(ConcreteValue::Map(settings)) = settings else {
        panic!("settings should remain a map");
    };

    assert!(matches!(
        settings.get("$k"),
        Some(Value::Deferred(DeferredValue::RedactedSecret { .. }))
    ));
    assert!(matches!(
        settings.get("legacy_enum"),
        Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
            if value.api_value() == "active"
    ));
    assert!(matches!(
        settings.get("items"),
        Some(Value::Concrete(ConcreteValue::List(items)))
            if matches!(
                items.first(),
                Some(Value::Concrete(ConcreteValue::Map(item)))
                    if matches!(
                        item.get("$nested"),
                        Some(Value::Concrete(ConcreteValue::String(value)))
                            if value.expose_plaintext() == "preserved"
                    )
            )
    ));

    let encoded = row
        .persisted_attributes()
        .expect("migrated row has persisted attributes")
        .encoded_value("settings")
        .expect("settings should remain persisted")
        .as_json();
    assert!(
        encoded["$$k"][carina_core::value::PERSISTED_SECRET_KEY]
            .as_str()
            .is_some_and(|value| value.starts_with("argon2-legacy:")),
        "the raw v11 marker should be upgraded after its key is escaped"
    );
    assert!(encoded["legacy_enum"].get("$enum").is_some());
    assert_eq!(encoded["items"][0]["$$nested"], "preserved");
}

#[test]
fn state_row_load_rejects_raw_legacy_salt_that_cannot_round_trip() {
    let json = serde_json::json!({
        "version": 13,
        "serial": 1,
        "lineage": "oversized-legacy-salt",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "test.resource",
            "identity": "x".repeat(5000),
            "provider": "mock",
            "identifier": "legacy-id",
            "attributes": {
                "comment": format!("{LEGACY_SECRET_PREFIX}{}", "ab".repeat(32)),
            },
        }],
    })
    .to_string();

    let error = check_and_migrate(&json).expect_err(
        "state load must reject a legacy salt that its encoded-digest parser cannot reload",
    );
    let message = error.to_string();
    assert!(message.contains("comment"), "{message}");
    assert!(message.contains("maximum is 4096 bytes"), "{message}");
}

#[test]
fn v9_deposed_entries_round_trip() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 9,
        "serial": 7,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "identity": "vpc",
                "provider": "awscc",
                "identifier": "vpc-new",
                "attributes": { "vpc_id": "vpc-new" },
                "deposed": [
                    {
                        "key": "deposed-1",
                        "identifier": "vpc-old",
                        "provider_instance": "west",
                        "attributes": { "vpc_id": "vpc-old" },
                        "dependency_bindings": ["igw"]
                    }
                ]
            },
            {
                "resource_type": "ec2.InternetGateway",
                "identity": "igw",
                "provider": "awscc",
                "identifier": "igw-1",
                "attributes": {},
                "binding": "igw"
            }
        ]
    }"#;

    let state = finish_legacy_dependency_migration(
        check_and_migrate(json)
            .expect("v9 state with deposed entries should load")
            .into_state(),
    );
    let serialized = serde_json::to_value(&state).expect("state should serialize");
    let reloaded = StateFile::from_json_str(&serialized.to_string())
        .expect("serialized v9 state should reload");
    assert_eq!(
        serde_json::to_value(reloaded).expect("reloaded state should serialize"),
        serialized
    );
    let row = state
        .find_resource("awscc", "ec2.Vpc", "vpc")
        .expect("resource should exist");
    let row_json = serde_json::to_value(row).expect("resource state should serialize");
    assert_eq!(row_json["deposed"][0]["identifier"], "vpc-old");
    assert_eq!(row_json["deposed"][0]["provider_instance"], "west");
    assert_eq!(
        row_json["deposed"][0]["dependency_bindings"][0]["identity"],
        "igw"
    );
}

#[test]
fn check_and_migrate_retains_identifier_none_rows_for_typed_classification() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 9,
        "serial": 7,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [
            {
                "resource_type": "ec2.Vpc",
                "identity": "deposed_only",
                "provider": "awscc",
                "identifier": null,
                "attributes": {},
                "deposed": [
                    {
                        "key": "deposed-1",
                        "identifier": "vpc-old",
                        "attributes": { "vpc_id": "vpc-old" },
                        "dependency_bindings": []
                    }
                ]
            },
            {
                "resource_type": "ec2.Vpc",
                "identity": "empty_artifact",
                "provider": "awscc",
                "identifier": null,
                "attributes": {}
            }
        ]
    }"#;

    let state = check_and_migrate(json)
        .expect("v9 deposed-only row should load")
        .into_state();
    assert!(
        state
            .find_resource("awscc", "ec2.Vpc", "deposed_only")
            .is_some(),
        "identifier=None row must survive while deposed entries remain"
    );
    let invalid_id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "empty_artifact", None);
    assert!(
        state
            .find_resource("awscc", "ec2.Vpc", "empty_artifact")
            .is_some(),
        "an identifier-less managed row must survive loading so planning can fail closed"
    );
    assert!(matches!(
        state.build_state_for_resource(&invalid_id),
        Current::MissingIdentifier(_state)
    ));
}

#[test]
fn test_build_orphans_excludes_desired() {
    use carina_core::resource::ResourceId;

    let mut state = StateFile::new();
    let mut rs = ResourceState::try_applied("ec2.Subnet", "kept-subnet", "awscc", "subnet-456")
        .expect("fixture identifier is non-empty");
    rs.dependency_bindings = fixture_persisted_dependencies(["my_vpc"]);
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let id = ResourceId::with_provider_identity("awscc", "ec2.Subnet", "kept-subnet", None);
    let mut desired_ids = std::collections::HashSet::new();
    desired_ids.insert(id.clone());

    let orphans = state.build_orphans(&desired_ids);
    assert!(orphans.is_empty());
}

#[test]
fn test_check_and_migrate_current_version() {
    use super::check_and_migrate;

    let state = StateFile::new();
    let json = serde_json::to_string_pretty(&state).unwrap();
    let result = check_and_migrate(&json).unwrap().into_state();
    assert_eq!(result.version, StateFile::CURRENT_VERSION);
    assert_eq!(result.lineage, state.lineage);
}

fn persisted_resource(provider: &str, resource_type: &str, identity: &str) -> serde_json::Value {
    serde_json::json!({
        "resource_type": resource_type,
        "identity": identity,
        "provider": provider,
        "current": {
            "kind": "applied",
            "identifier": format!("{provider}-{resource_type}-{identity}"),
            "attributes": {},
            "nested_write_only": {}
        }
    })
}

fn legacy_persisted_resource(
    provider: &str,
    resource_type: &str,
    identity: &str,
) -> serde_json::Value {
    serde_json::json!({
        "resource_type": resource_type,
        "identity": identity,
        "provider": provider,
        "identifier": format!("{provider}-{resource_type}-{identity}"),
        "attributes": {}
    })
}

fn state_json_with_resources(version: u32, resources: Vec<serde_json::Value>) -> String {
    let mut state = serde_json::json!({
        "version": version,
        "serial": 1,
        "lineage": "duplicate-identity-test",
        "carina_version": "test",
        "resources": resources
    });
    if version >= StateFile::EXPORTS_VERSION_INTRODUCED {
        state["exports_version"] = serde_json::json!(PublishedExports::CURRENT_VERSION);
    }
    state.to_string()
}

fn resolved_dependency(
    provider: &str,
    resource_type: &str,
    identity: &str,
    provider_instance: Option<&str>,
) -> ResolvedResourceId {
    ResolvedResourceId::new(ResourceId::with_provider_identity(
        provider.to_string(),
        resource_type.to_string(),
        ResourceIdentity::try_from(identity.to_string()).expect("fixture identity is non-empty"),
        provider_instance.map(str::to_string),
    ))
}

fn fixture_dependencies<const N: usize>(identities: [&str; N]) -> ResolvedDependencies {
    identities
        .into_iter()
        .map(|identity| resolved_dependency("mock", "test.Dependency", identity, None))
        .collect()
}

fn fixture_persisted_dependencies<const N: usize>(
    identities: [&str; N],
) -> PersistedDestroyDependencies {
    PersistedDestroyDependencies::from_test_dependencies(
        identities
            .into_iter()
            .map(|identity| resolved_dependency("mock", "test.Dependency", identity, None)),
    )
}

#[test]
fn legacy_dependency_spellings_do_not_double_an_existing_scope_prefix() {
    assert_eq!(
        legacy_dependency_spellings(Some("module_instance"), "module_instance.target"),
        vec!["module_instance.target"],
    );
    assert_eq!(
        legacy_dependency_spellings(Some("module_instance"), "target"),
        vec!["module_instance.target", "target"],
    );
    assert_eq!(legacy_dependency_spellings(None, "target"), vec!["target"],);
}

#[test]
fn v20_without_dependency_names_can_complete_before_a_mutating_command_early_write() {
    let json = state_json_with_resources(
        20,
        vec![persisted_resource("mock", "test.Resource", "independent")],
    );
    let mut state = check_and_migrate(&json)
        .expect("v20 state without dependency names must load")
        .into_state();

    assert!(state.requires_legacy_dependency_migration());
    assert!(serde_json::to_value(&state).is_err());
    assert!(state.complete_legacy_dependency_migration_without_names());
    assert!(!state.requires_legacy_dependency_migration());
    assert!(serde_json::to_value(&state).is_ok());
    assert!(!state.complete_legacy_dependency_migration_without_names());
}

#[test]
fn v20_dependency_migration_resolves_root_module_loop_origin_and_deposed_edges() {
    let mut root = persisted_resource("mock", "test.Root", "root_target");
    root["binding"] = serde_json::json!("root_target");

    let mut module = persisted_resource("mock", "test.Module", "module_instance.target");
    module["binding"] = serde_json::json!("module_instance.target");
    module["directives"] = serde_json::json!({ "provider_instance": "west" });

    let mut loop_target = persisted_resource("mock", "test.Loop", "module_instance.items[0]");
    loop_target["binding"] = serde_json::json!("module_instance.items[0]");

    let origin = persisted_resource("mock", "test.Origin", "moved_origin");

    let mut consumer = persisted_resource("mock", "test.Consumer", "consumer");
    consumer["binding"] = serde_json::json!("consumer");
    consumer["dependency_bindings"] = serde_json::json!([
        "root_target",
        "module_instance.target",
        "module_instance.items[0]",
        "moved_origin"
    ]);
    consumer["directives"] = serde_json::json!({
        "depends_on": ["module_instance.target"]
    });
    consumer["deposed"] = serde_json::json!([{
        "key": "old-generation",
        "identifier": "consumer-old",
        "attributes": {},
        "dependency_bindings": ["module_instance.items[0]"]
    }]);

    let json = state_json_with_resources(20, vec![root, module, loop_target, origin, consumer]);
    let loaded = check_and_migrate(&json)
        .expect("legacy dependency names must load into the unwritable typestate")
        .into_state();
    assert!(loaded.requires_legacy_dependency_migration());
    assert!(serde_json::to_value(&loaded).is_err());
    let configured_consumer =
        Resource::with_provider("mock", "test.Consumer", "consumer", None).with_binding("consumer");
    let (migrated, report) = migrate_legacy_with_config(loaded, &[configured_consumer])
        .expect("every legacy address has one exact persisted origin");
    assert!(report.warnings().is_empty());
    let consumer = &migrated.resources()[4];
    let expected = PersistedDestroyDependencies::from_test_dependencies([
        resolved_dependency("mock", "test.Root", "root_target", None),
        resolved_dependency(
            "mock",
            "test.Module",
            "module_instance.target",
            Some("west"),
        ),
        resolved_dependency("mock", "test.Loop", "module_instance.items[0]", None),
        resolved_dependency("mock", "test.Origin", "moved_origin", None),
    ]);
    assert_eq!(consumer.dependency_bindings, expected);
    assert_eq!(
        consumer.directives.depends_on,
        PersistedDestroyDependencies::from_test_dependencies([resolved_dependency(
            "mock",
            "test.Module",
            "module_instance.target",
            Some("west"),
        )])
    );
    assert_eq!(
        consumer.deposed[0].dependency_bindings,
        PersistedDestroyDependencies::from_test_dependencies([resolved_dependency(
            "mock",
            "test.Loop",
            "module_instance.items[0]",
            None,
        )])
    );

    let encoded = serde_json::to_value(&migrated).expect("migrated state serializes");
    assert!(encoded["resources"][4]["dependency_bindings"][0].is_object());
    assert!(encoded["resources"][4]["directives"]["depends_on"][0].is_object());
    assert!(encoded["resources"][4]["deposed"][0]["dependency_bindings"][0].is_object());
}

#[test]
fn v20_dependency_migration_rejects_missing_target_with_row_and_text() {
    let mut consumer = persisted_resource("mock", "test.Consumer", "consumer");
    consumer["binding"] = serde_json::json!("consumer");
    consumer["dependency_bindings"] = serde_json::json!(["gone_target"]);
    let json = state_json_with_resources(20, vec![consumer]);

    let state = check_and_migrate(&json)
        .expect("legacy strings load before configuration-aware classification")
        .into_state();
    let configured =
        Resource::with_provider("mock", "test.Consumer", "consumer", None).with_binding("consumer");
    let error = migrate_legacy_with_config(state, &[configured])
        .expect_err("missing edges on configured rows must never be dropped");
    assert!(matches!(
        error,
        LegacyDependencyMigrationError::Missing {
            row_index: 0,
            ref row,
            ref location,
            ref dependency,
        } if row.contains("test.Consumer")
            && location == "resources[0].dependency_bindings"
            && dependency == "gone_target"
    ));
}

#[test]
fn v20_dependency_migration_classifies_moved_row_as_configured() {
    let mut target = persisted_resource("mock", "test.Target", "module_instance.target");
    target["binding"] = serde_json::json!("module_instance.target");
    let mut consumer = persisted_resource("mock", "test.Consumer", "old_consumer");
    consumer["binding"] = serde_json::json!("old_consumer");
    consumer["dependency_bindings"] = serde_json::json!(["target"]);
    let json = state_json_with_resources(20, vec![target, consumer]);
    let state = check_and_migrate(&json)
        .expect("moved-row fixture must load as pending migration")
        .into_state();

    let target: SavedResource =
        Resource::with_provider("mock", "test.Target", "module_instance.target", None)
            .with_binding("module_instance.target")
            .into();
    let mut configured_consumer: SavedResource =
        Resource::with_provider("mock", "test.Consumer", "module_instance.consumer", None)
            .with_binding("module_instance.consumer")
            .into();
    configured_consumer.module_source = Some(SavedModuleSource::Module {
        name: "consumer_module".to_string(),
        instance: "module_instance".to_string(),
        scope_key: Some("module_instance".to_string()),
        scope_description: None,
    });
    let resources = vec![target, configured_consumer];
    let graph_resources: Vec<Resource> = vec![
        Resource::with_provider("mock", "test.Target", "module_instance.target", None)
            .with_binding("module_instance.target"),
        Resource::with_provider("mock", "test.Consumer", "module_instance.consumer", None)
            .with_binding("module_instance.consumer"),
    ];
    let parsed: carina_core::parser::ParsedFile = Default::default();
    let graph = carina_core::resolved_graph::resolve_expanded_graph(parsed, &graph_resources, &[])
        .expect("moved-row graph resolves");
    let from = resolved_dependency("mock", "test.Consumer", "old_consumer", None);
    let to = resolved_dependency("mock", "test.Consumer", "module_instance.consumer", None);

    let mut move_plan: carina_core::plan::Plan<SavedModuleSource> =
        carina_core::plan::Plan::default();
    move_plan.add(carina_core::effect::Effect::Move {
        from,
        to,
        origin: None,
    });
    let moved_pairs =
        carina_core::effect::ResolvedMovePairs::from_saved_plan(move_plan.move_effects());
    let configured =
        carina_core::resolved_graph::ConfiguredResources::from_test_resources(&resources);
    let input = LegacyMigrationInput::planned(&graph, &configured, &moved_pairs);
    let classification = state
        .classify_legacy_dependencies(&input)
        .expect("the moved row must classify with its configured destination scope");
    let migration = state
        .with_legacy_classification(classification)
        .expect("the moved row must use its configured destination scope");

    assert!(migration.warnings().is_empty());
    let state = migration.report(|_| panic!("fixture must not report warnings"));
    let row = state
        .find_resource("mock", "test.Consumer", "old_consumer")
        .expect("migration classifies but does not itself move the row");
    assert_eq!(
        row.dependency_bindings,
        PersistedDestroyDependencies::from_test_dependencies([resolved_dependency(
            "mock",
            "test.Target",
            "module_instance.target",
            None,
        )])
    );
}

#[test]
fn with_legacy_classification_rejects_stale_sources() {
    let mut target = persisted_resource("mock", "test.Target", "target");
    target["binding"] = serde_json::json!("target");
    let mut consumer = persisted_resource("mock", "test.Consumer", "consumer");
    consumer["binding"] = serde_json::json!("consumer");
    consumer["dependency_bindings"] = serde_json::json!(["target"]);
    let original = check_and_migrate(&state_json_with_resources(
        20,
        vec![target.clone(), consumer.clone()],
    ))
    .expect("original v20 state loads")
    .into_state();

    let configured = vec![
        Resource::with_provider("mock", "test.Target", "target", None).with_binding("target"),
        Resource::with_provider("mock", "test.Consumer", "consumer", None).with_binding("consumer"),
    ];
    let parsed: carina_core::parser::ParsedFile = Default::default();
    let graph = carina_core::resolved_graph::resolve_expanded_graph(parsed, &configured, &[])
        .expect("stale-classification graph resolves");
    let no_move_plan: carina_core::plan::Plan = carina_core::plan::Plan::new();
    let moved_pairs =
        carina_core::effect::ResolvedMovePairs::from_saved_plan(no_move_plan.move_effects());
    let configured =
        carina_core::resolved_graph::ConfiguredResources::from_test_resources(&configured);
    let input = LegacyMigrationInput::planned(&graph, &configured, &moved_pairs);
    let classification = original
        .classify_legacy_dependencies(&input)
        .expect("original source classifies");

    consumer["dependency_bindings"] = serde_json::json!(["different_target"]);
    let changed = check_and_migrate(&state_json_with_resources(20, vec![target, consumer]))
        .expect("changed v20 state loads")
        .into_state();
    let error = changed
        .clone()
        .with_legacy_classification(classification)
        .expect_err("changed source names must reject the saved classification");

    assert_eq!(error, LegacyDependencyMigrationError::StaleClassification);
    assert_eq!(changed.version, 20);
    assert!(changed.requires_legacy_dependency_migration());
}

#[test]
fn v20_dependency_migration_rejects_ambiguous_binding_with_row_and_text() {
    let mut first = persisted_resource("mock", "test.First", "first");
    first["binding"] = serde_json::json!("shared");
    let mut second = persisted_resource("mock", "test.Second", "second");
    second["binding"] = serde_json::json!("shared");
    let mut consumer = persisted_resource("mock", "test.Consumer", "consumer");
    consumer["binding"] = serde_json::json!("consumer");
    consumer["directives"] = serde_json::json!({ "depends_on": ["shared"] });
    let json = state_json_with_resources(20, vec![first, second, consumer]);

    let state = check_and_migrate(&json)
        .expect("legacy strings load before configuration-aware classification")
        .into_state();
    let configured =
        Resource::with_provider("mock", "test.Consumer", "consumer", None).with_binding("consumer");
    let error = migrate_legacy_with_config(state, &[configured])
        .expect_err("ambiguous edges on configured rows must never be guessed");
    assert!(matches!(
        error,
        LegacyDependencyMigrationError::Ambiguous {
            row_index: 2,
            ref row,
            ref location,
            ref dependency,
            ref candidates,
        } if row.contains("test.Consumer")
            && location == "resources[2].directives.depends_on"
            && dependency == "shared"
            && candidates.len() == 2
    ));
}

#[test]
fn v20_dependency_migration_keeps_known_orphan_edges_and_reports_each_unknown_generation() {
    let mut target = persisted_resource("mock", "test.Target", "remaining");
    target["binding"] = serde_json::json!("remaining");
    let mut orphan = persisted_resource("mock", "test.Orphan", "removed_module.consumer");
    orphan["binding"] = serde_json::json!("removed_module.consumer");
    orphan["dependency_bindings"] = serde_json::json!(["remaining", "removed_module.gone"]);
    orphan["deposed"] = serde_json::json!([{
        "key": "old-generation",
        "identifier": "orphan-old",
        "attributes": {},
        "dependency_bindings": ["removed_module.old_gone"]
    }]);
    let json = state_json_with_resources(20, vec![target, orphan]);
    let state = check_and_migrate(&json)
        .expect("orphan legacy names load into the migration typestate")
        .into_state();
    let configured_target =
        Resource::with_provider("mock", "test.Target", "remaining", None).with_binding("remaining");

    let (migrated, report) = migrate_legacy_with_config(state, &[configured_target])
        .expect("unresolved orphan names are warnings rather than migration failures");
    assert_eq!(report.warnings().len(), 2);
    assert!(report.warnings().iter().any(|warning| {
        warning.generation.is_none()
            && warning.legacy_names == BTreeSet::from(["removed_module.gone".to_string()])
    }));
    assert!(report.warnings().iter().any(|warning| {
        warning
            .generation
            .as_ref()
            .is_some_and(|key| key.as_str() == "old-generation")
            && warning.legacy_names == BTreeSet::from(["removed_module.old_gone".to_string()])
    }));

    let orphan = &migrated.resources()[1];
    assert_eq!(
        orphan.dependency_bindings,
        PersistedDestroyDependencies::from_test_dependencies([resolved_dependency(
            "mock",
            "test.Target",
            "remaining",
            None,
        )])
    );
    assert!(orphan.deposed[0].dependency_bindings.is_empty());
    let encoded = serde_json::to_string(&migrated).expect("v21 state must be writable");
    assert!(!encoded.contains("removed_module.gone"));
    assert!(!encoded.contains("removed_module.old_gone"));
}

#[test]
fn report_invokes_sink_for_every_current_and_deposed_warning_before_returning_state() {
    let mut orphan = persisted_resource("mock", "test.Orphan", "removed.consumer");
    orphan["binding"] = serde_json::json!("removed.consumer");
    orphan["dependency_bindings"] = serde_json::json!(["gone-current"]);
    orphan["deposed"] = serde_json::json!([{
        "key": "old-generation",
        "identifier": "orphan-old",
        "attributes": {},
        "dependency_bindings": ["gone-deposed"]
    }]);
    let state = check_and_migrate(&state_json_with_resources(20, vec![orphan]))
        .expect("warning fixture loads into the migration typestate")
        .into_state();
    let parsed: carina_core::parser::ParsedFile = Default::default();
    let graph = carina_core::resolved_graph::resolve_expanded_graph(parsed, &[], &[])
        .expect("empty warning graph resolves");
    let no_move_plan: carina_core::plan::Plan = carina_core::plan::Plan::new();
    let moved_pairs =
        carina_core::effect::ResolvedMovePairs::from_saved_plan(no_move_plan.move_effects());
    let configured: Vec<Resource> = Vec::new();
    let configured =
        carina_core::resolved_graph::ConfiguredResources::from_test_resources(&configured);
    let input = LegacyMigrationInput::planned(&graph, &configured, &moved_pairs);
    let classification = state
        .classify_legacy_dependencies(&input)
        .expect("orphan unknown names classify as warnings");
    let migration = state
        .with_legacy_classification(classification)
        .expect("classification applies to its source state");

    let mut reported = Vec::new();
    let state = migration.report(|warning| {
        reported.push((
            warning
                .generation
                .as_ref()
                .map(|key| key.as_str().to_string()),
            warning.legacy_names.clone(),
        ));
    });

    assert_eq!(reported.len(), 2);
    assert!(reported.iter().any(|(generation, names)| {
        generation.is_none() && names == &BTreeSet::from(["gone-current".to_string()])
    }));
    assert!(reported.iter().any(|(generation, names)| {
        generation.as_deref() == Some("old-generation")
            && names == &BTreeSet::from(["gone-deposed".to_string()])
    }));
    assert_eq!(state.version, StateFile::CURRENT_VERSION);
}

fn invalid_state_message(result: Result<MigratedStateFile, BackendError>) -> String {
    match result.expect_err("invalid state must be rejected") {
        BackendError::InvalidState(message) => message,
        other => panic!("expected InvalidState, got {other}"),
    }
}

#[test]
fn check_and_migrate_rejects_empty_identity_with_actionable_row_context() {
    let valid = persisted_resource("mock", "test.resource", "valid");
    let mut invalid = persisted_resource("mock", "test.resource", "");
    invalid["current"]["identifier"] = serde_json::json!("legacy-resource-123");
    let json = state_json_with_resources(StateFile::CURRENT_VERSION, vec![valid, invalid]);

    let error = check_and_migrate(&json).expect_err("empty identities must be rejected on load");
    let message = error.to_string();

    assert_eq!(
        message,
        "Invalid state file: resources[1] has an empty identity \
         (provider=\"mock\", resource_type=\"test.resource\", \
         identifier=\"legacy-resource-123\"). It was written by an older Carina version and is \
         rejected because an empty identity can no longer be matched to a resource. Back up the \
         state file, then remove this row. Run `carina plan`; the resource that owned it appears \
         as a create with its newly assigned identity. Put the row back with `identity` set to \
         that value (keep its `identifier` and attributes), or leave it removed if the resource \
         is no longer managed.\nCause: resource identity cannot be empty"
    );
    assert_eq!(
        message.matches("resource identity cannot be empty").count(),
        1,
        "{message}"
    );
    assert!(error.invalid_resource_identity().is_some());
    assert!(std::error::Error::source(&error).is_none());
}

#[test]
fn check_and_migrate_rejects_duplicate_resource_identity() {
    let resource = persisted_resource("aws", "ec2.Vpc", "network");
    let json =
        state_json_with_resources(StateFile::CURRENT_VERSION, vec![resource.clone(), resource]);

    let message = invalid_state_message(check_and_migrate(&json));

    assert!(message.contains("duplicate resource identity"), "{message}");
    assert!(message.contains("aws"), "{message}");
    assert!(message.contains("ec2.Vpc"), "{message}");
    assert!(message.contains("network"), "{message}");
}

#[test]
fn check_and_migrate_rejects_duplicate_resource_identity_across_provider_instances() {
    let mut west = persisted_resource("aws", "ec2.Vpc", "network");
    west["directives"] = serde_json::json!({ "provider_instance": "west" });
    let mut east = persisted_resource("aws", "ec2.Vpc", "network");
    east["directives"] = serde_json::json!({ "provider_instance": "east" });
    let json = state_json_with_resources(StateFile::CURRENT_VERSION, vec![west, east]);

    let message = invalid_state_message(check_and_migrate(&json));

    assert!(message.contains("duplicate resource identity"), "{message}");
    assert!(message.contains("network"), "{message}");
}

#[test]
fn check_and_migrate_accepts_resource_identities_differing_on_each_axis() {
    let json = state_json_with_resources(
        StateFile::CURRENT_VERSION,
        vec![
            persisted_resource("aws", "ec2.Vpc", "network"),
            persisted_resource("awscc", "ec2.Vpc", "network"),
            persisted_resource("aws", "ec2.Subnet", "network"),
            persisted_resource("aws", "ec2.Vpc", "other-network"),
        ],
    );

    let state = check_and_migrate(&json)
        .expect("all three resource identity axes must participate in uniqueness")
        .into_state();

    assert_eq!(state.resources.len(), 4);
}

#[test]
fn v21_state_rejects_destroy_dependencies_that_are_not_managed_rows() {
    let invalid_dependencies = [
        (
            "synthetic wait",
            resolved_dependency("", "__wait", "ready", None),
        ),
        (
            "empty provider",
            resolved_dependency("", "test.Thing", "x", None),
        ),
    ];

    for (kind, dependency) in invalid_dependencies {
        let mut consumer = persisted_resource("mock", "test.Consumer", "consumer");
        consumer["dependency_bindings"] =
            serde_json::json!([serde_json::to_value(&dependency).expect("encode dependency")]);
        let json = state_json_with_resources(StateFile::CURRENT_VERSION, vec![consumer]);

        let message = invalid_state_message(check_and_migrate(&json));
        assert!(
            message.contains("dependency_bindings") && message.contains("not a managed state row"),
            "{kind} dependency must be rejected with row context: {message}",
        );
    }
}

fn deposed_generation(
    key: &str,
    identifier: &str,
    provider_instance: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "key": key,
        "identifier": identifier,
        "provider_instance": provider_instance,
        "attributes": {},
        "dependency_bindings": []
    })
}

fn resource_with_deposed(deposed: Vec<serde_json::Value>) -> serde_json::Value {
    let mut resource = persisted_resource("awscc", "ec2.Vpc", "network");
    resource["deposed"] = serde_json::Value::Array(deposed);
    resource
}

#[test]
fn check_and_migrate_rejects_duplicate_deposed_key() {
    let json = state_json_with_resources(
        StateFile::CURRENT_VERSION,
        vec![resource_with_deposed(vec![
            deposed_generation("generation-a", "vpc-old-a", Some("west")),
            deposed_generation("generation-a", "vpc-old-b", Some("east")),
        ])],
    );

    let message = invalid_state_message(check_and_migrate(&json));

    assert!(
        message.contains("duplicate deposed generation key"),
        "{message}"
    );
    assert!(message.contains("awscc"), "{message}");
    assert!(message.contains("ec2.Vpc"), "{message}");
    assert!(message.contains("network"), "{message}");
    assert!(message.contains("generation-a"), "{message}");
}

#[test]
fn check_and_migrate_rejects_duplicate_deposed_identifier_and_provider_instance() {
    let json = state_json_with_resources(
        StateFile::CURRENT_VERSION,
        vec![resource_with_deposed(vec![
            deposed_generation("generation-a", "vpc-old", Some("west")),
            deposed_generation("generation-b", "vpc-old", Some("west")),
        ])],
    );

    let message = invalid_state_message(check_and_migrate(&json));

    assert!(
        message.contains("duplicate deposed generation identity"),
        "{message}"
    );
    assert!(message.contains("awscc"), "{message}");
    assert!(message.contains("ec2.Vpc"), "{message}");
    assert!(message.contains("network"), "{message}");
    assert!(message.contains("vpc-old"), "{message}");
    assert!(message.contains("west"), "{message}");
}

#[test]
fn check_and_migrate_loads_legacy_name_alias_and_checks_it_for_duplicates() {
    let mut legacy = persisted_resource("aws", "s3.Bucket", "logs");
    let identity = legacy.as_object_mut().unwrap().remove("identity").unwrap();
    legacy["name"] = identity;
    let valid_json = state_json_with_resources(StateFile::CURRENT_VERSION, vec![legacy.clone()]);

    let state = check_and_migrate(&valid_json)
        .expect("legacy name alias must remain readable")
        .into_state();
    assert_eq!(state.resources[0].identity.as_str(), "logs");

    let duplicate_json = state_json_with_resources(
        StateFile::CURRENT_VERSION,
        vec![legacy, persisted_resource("aws", "s3.Bucket", "logs")],
    );
    let message = invalid_state_message(check_and_migrate(&duplicate_json));
    assert!(message.contains("duplicate resource identity"), "{message}");
    assert!(message.contains("logs"), "{message}");
}

#[test]
fn check_and_migrate_bytes_rejects_duplicate_resource_identity() {
    let resource = persisted_resource("aws", "ec2.Vpc", "network");
    let json =
        state_json_with_resources(StateFile::CURRENT_VERSION, vec![resource.clone(), resource]);

    let message = invalid_state_message(check_and_migrate_bytes(json.as_bytes()));

    assert!(message.contains("duplicate resource identity"), "{message}");
    assert!(message.contains("network"), "{message}");
}

#[test]
fn published_exports_ignore_future_state_version_and_resource_shape() {
    let found = StateFile::CURRENT_VERSION + 1;
    let json = serde_json::json!({
        "version": found,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "future-state",
        "carina_version": "future",
        "resources": [{ "shape_from_the_future": [true, 42] }],
        "exports": { "account_id": "123456789012" }
    });

    let exports = PublishedExports::from_state_bytes(json.to_string().as_bytes())
        .expect("supported exports must load independently of resource state");
    assert_eq!(
        exports
            .values()
            .get("account_id")
            .expect("fixture export exists")
            .decoded_json(),
        serde_json::json!("123456789012")
    );

    let error = check_and_migrate(&json.to_string())
        .expect_err("the full state path must retain its state-version gate");
    assert!(matches!(
        error,
        BackendError::StateVersionTooNew {
            found: actual_found,
            supported,
        } if actual_found == found && supported == StateFile::CURRENT_VERSION
    ));
}

#[test]
fn published_exports_reject_newer_exports_version() {
    let found = PublishedExports::CURRENT_VERSION + 1;
    let json = serde_json::json!({
        "version": StateFile::CURRENT_VERSION + 1,
        "exports_version": found,
        "resources": [{ "shape_from_the_future": true }],
        "exports": { "account_id": "123456789012" }
    });

    let error = PublishedExports::from_state_bytes(json.to_string().as_bytes())
        .expect_err("a newer exports encoding must be rejected");
    assert!(matches!(
        &error,
        BackendError::ExportsVersionTooNew {
            found: actual_found,
            supported,
        } if *actual_found == found && *supported == PublishedExports::CURRENT_VERSION
    ));
    assert!(error.to_string().contains("Please upgrade Carina"));
}

#[test]
fn published_exports_reject_exports_version_before_it_was_introduced() {
    let json = serde_json::json!({
        "version": 12,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "resources": [],
        "exports": { "user_map": { "$$user": true } }
    });

    let error = PublishedExports::from_state_bytes(json.to_string().as_bytes())
        .expect_err("legacy state must not opt into the current exports decoder");
    let BackendError::InvalidState(message) = error else {
        panic!("expected InvalidState, got {error}");
    };
    assert_eq!(
        message,
        format!(
            "Failed to parse state file: top-level exports_version is invalid before state version {}",
            StateFile::EXPORTS_VERSION_INTRODUCED
        )
    );
}

#[test]
fn full_state_rejects_exports_version_before_it_was_introduced() {
    let json = serde_json::json!({
        "version": 14,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "invalid-legacy-state",
        "carina_version": "legacy",
        "resources": [],
        "exports": {}
    });

    let error = check_and_migrate(&json.to_string())
        .expect_err("legacy state must reject exports_version before migration");
    let BackendError::InvalidState(message) = error else {
        panic!("expected InvalidState, got {error}");
    };
    assert_eq!(
        message,
        format!(
            "Failed to parse state file: top-level exports_version is invalid before state version {}",
            StateFile::EXPORTS_VERSION_INTRODUCED
        )
    );
}

#[test]
fn published_exports_load_v16_without_exports_version_through_legacy_migration() {
    let json = serde_json::json!({
        "version": 16,
        "serial": 1,
        "lineage": "legacy-state",
        "carina_version": "legacy",
        "resources": [],
        "exports": {
            "enum_value": "aws.ec2.Eip.Domain.vpc",
            "user_map": { "$$user": true }
        }
    });

    let exports = PublishedExports::from_state_bytes(json.to_string().as_bytes())
        .expect("v16 exports must use the legacy full-state migration path");
    assert_eq!(
        exports
            .values()
            .get("user_map")
            .expect("fixture export exists")
            .decoded_json(),
        serde_json::json!({ "$user": true })
    );
}

#[test]
fn published_exports_reject_current_state_without_exports_version() {
    let json = serde_json::json!({
        "version": StateFile::CURRENT_VERSION,
        "serial": 1,
        "lineage": "invalid-current-state",
        "carina_version": "current",
        "resources": [],
        "exports": { "account_id": "123456789012" }
    });

    let error = PublishedExports::from_state_bytes(json.to_string().as_bytes())
        .expect_err("v17 and newer state must declare exports_version");
    let BackendError::InvalidState(message) = error else {
        panic!("expected InvalidState, got {error}");
    };
    assert_eq!(
        message,
        "Failed to parse state file: missing required top-level exports_version"
    );
}

#[test]
fn published_exports_reject_redacted_secret_from_v17_state() {
    let json = serde_json::json!({
        "version": 17,
        "exports_version": PublishedExports::CURRENT_VERSION,
        "serial": 1,
        "lineage": "invalid-secret-export",
        "carina_version": "current",
        "resources": [],
        "exports": {
            "redacted_secret": { "$secret": "redacted" }
        }
    });

    let error = PublishedExports::from_state_bytes(json.to_string().as_bytes())
        .expect_err("v17 exports must reject redacted secrets");
    let BackendError::InvalidState(message) = error else {
        panic!("expected invalid state error, got {error}");
    };
    assert!(
        message.contains("invalid host-reserved persisted JSON object with keys [\"$secret\"]"),
        "{message}"
    );
}

#[test]
fn full_state_and_published_exports_share_the_exports_version_gate() {
    for exports_version in [
        None,
        Some(PublishedExports::CURRENT_VERSION - 1),
        Some(PublishedExports::CURRENT_VERSION + 1),
    ] {
        let mut json = serde_json::json!({
            "version": StateFile::CURRENT_VERSION,
            "serial": 1,
            "lineage": "shared-exports-version-gate",
            "carina_version": "current",
            "resources": [],
            "exports": {}
        });
        if let Some(exports_version) = exports_version {
            json["exports_version"] = serde_json::json!(exports_version);
        }

        let full_state_error = check_and_migrate(&json.to_string())
            .expect_err("the full-state decoder must reject this exports version");
        let published_exports_error =
            PublishedExports::from_state_bytes(json.to_string().as_bytes())
                .expect_err("the exports-only decoder must reject this exports version");

        assert_eq!(
            full_state_error.to_string(),
            published_exports_error.to_string()
        );
    }
}

#[test]
fn state_file_serializes_current_exports_version_after_reload() {
    let state = StateFile::new();
    let encoded = serde_json::to_string(&state).expect("state serializes");
    let encoded_json: serde_json::Value =
        serde_json::from_str(&encoded).expect("serialized state is JSON");
    assert_eq!(
        encoded_json["exports_version"],
        PublishedExports::CURRENT_VERSION
    );

    let reloaded = check_and_migrate(&encoded)
        .expect("written state reloads")
        .into_state();
    assert_eq!(
        serde_json::to_value(reloaded).expect("reloaded state serializes")["exports_version"],
        PublishedExports::CURRENT_VERSION
    );
}

#[test]
fn persisted_exports_encoding_golden() {
    use carina_core::resource::EnumValueResolver;
    use carina_core::schema::{AttributeType, TypeIdentity};
    use std::collections::BTreeMap;

    fn encode(value: Value) -> serde_json::Value {
        serde_json::to_value(
            PersistedExportValue::encode(&value).expect("golden export value is persistable"),
        )
        .expect("persisted export serializes")
    }

    let enum_type = AttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
        Some(vec!["vpc".to_string()]),
        Vec::new(),
        None,
        None,
    );
    let enum_value = Value::Concrete(ConcreteValue::CanonicalEnum(
        EnumValueResolver::new(&enum_type)
            .resolve_state_text("vpc")
            .expect("golden enum value resolves"),
    ));
    let encoded = BTreeMap::from([
        ("bool", encode(Value::Concrete(ConcreteValue::Bool(true)))),
        ("canonical_enum", encode(enum_value)),
        (
            "list",
            encode(Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("item".into())),
                Value::Concrete(ConcreteValue::Int(2)),
            ]))),
        ),
        (
            "map",
            encode(Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "nested".to_string(),
                Value::Concrete(ConcreteValue::String("value".into())),
            )])))),
        ),
        ("number", encode(Value::Concrete(ConcreteValue::Int(42)))),
        (
            "string",
            encode(Value::Concrete(ConcreteValue::String("hello".into()))),
        ),
        (
            "user_dollar_key",
            encode(Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "$user".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            )])))),
        ),
    ]);

    let actual = serde_json::to_string_pretty(&encoded).expect("golden values serialize");
    let expected = r#"{
  "bool": true,
  "canonical_enum": {
    "$enum": {
      "api_value": "vpc",
      "identity": {
        "kind": "Domain",
        "provider": "aws",
        "segments": [
          "ec2",
          "Eip"
        ]
      }
    }
  },
  "list": [
    "item",
    2
  ],
  "map": {
    "nested": "value"
  },
  "number": 42,
  "string": "hello",
  "user_dollar_key": {
    "$$user": true
  }
}"#;

    assert_eq!(
        actual, expected,
        "if this encoding changed intentionally, bump the exports version"
    );
}

fn assert_migration_rejects_canonicalized_duplicate(version: u32) {
    let mut legacy = legacy_persisted_resource("awscc", "sso.Assignment", "placeholder");
    legacy["name"] = serde_json::json!("_accounts[\"registry_prod\"]");
    legacy.as_object_mut().unwrap().remove("identity");
    let current = legacy_persisted_resource("awscc", "sso.Assignment", "_accounts.registry_prod");
    let json = state_json_with_resources(version, vec![legacy, current]);

    let message = invalid_state_message(check_and_migrate(&json));

    assert!(message.contains("duplicate resource identity"), "{message}");
    assert!(message.contains("awscc"), "{message}");
    assert!(message.contains("sso.Assignment"), "{message}");
    assert!(message.contains("_accounts.registry_prod"), "{message}");
}

#[test]
fn v5_migration_rejects_duplicate_created_by_canonicalization() {
    assert_migration_rejects_canonicalized_duplicate(5);
}

#[test]
fn v6_migration_rejects_duplicate_created_by_canonicalization() {
    assert_migration_rejects_canonicalized_duplicate(6);
}

#[test]
fn older_migration_rejects_duplicate_created_by_canonicalization() {
    assert_migration_rejects_canonicalized_duplicate(3);
}

#[test]
fn test_check_and_migrate_future_version_returns_error() {
    use super::check_and_migrate;

    let found = StateFile::CURRENT_VERSION + 1;
    let json = format!(
        r#"{{
        "version": {found},
        "serial": 0,
        "lineage": "test-lineage",
        "carina_version": "0.1.0",
        "resources": []
    }}"#
    );

    let error = check_and_migrate(&json).expect_err("future state version must be rejected");
    assert!(
        matches!(
            &error,
            BackendError::StateVersionTooNew {
                found: actual_found,
                supported,
            } if *actual_found == found && *supported == StateFile::CURRENT_VERSION
        ),
        "future state versions must return the typed version-gate error"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "State file version {found} is newer than supported version {}. Please upgrade Carina.",
            StateFile::CURRENT_VERSION
        )
    );
}

#[test]
fn test_check_and_migrate_older_version_migrates() {
    use super::check_and_migrate;

    // v3 state file — should be migrated to current version
    let json = r#"{
        "version": 3,
        "serial": 5,
        "lineage": "old-lineage",
        "carina_version": "0.0.1",
        "resources": []
    }"#;

    let result = finish_legacy_dependency_migration(check_and_migrate(json).unwrap().into_state());
    assert_eq!(
        result.version,
        StateFile::CURRENT_VERSION,
        "version should be bumped to current"
    );
    assert_eq!(result.serial, 5, "serial should be preserved");
    assert_eq!(result.lineage, "old-lineage", "lineage should be preserved");
}

#[test]
fn test_check_and_migrate_invalid_json_returns_error() {
    use super::check_and_migrate;

    let result = check_and_migrate("not valid json at all");
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Failed to parse state version"),
        "error should mention version parsing failure"
    );
}

#[test]
fn test_check_and_migrate_bytes_works() {
    use super::check_and_migrate_bytes;

    let state = StateFile::new();
    let json = serde_json::to_string_pretty(&state).unwrap();
    let result = check_and_migrate_bytes(json.as_bytes())
        .unwrap()
        .into_state();
    assert_eq!(result.version, StateFile::CURRENT_VERSION);
}

#[test]
fn test_check_and_migrate_bytes_invalid_utf8() {
    use super::check_and_migrate_bytes;

    let bytes: &[u8] = &[0xff, 0xfe, 0xfd];
    let result = check_and_migrate_bytes(bytes);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("UTF-8"), "error should mention UTF-8 issue");
}

// carina#3283: check_and_migrate is library-level and must not write to
// stderr. The migration event is returned as a typed value so the caller
// (a backend impl) can decide *when* and *how often* to log it. Without
// this, every read_state call on the same physical state file emits a
// fresh "Migrating state file..." line — for `carina plan` that means
// the warning fires twice (T0 + T1 drift re-read) and even three times
// when the run also crosses the refresh phase (#3283 repro).
#[test]
fn test_check_and_migrate_returns_migration_info_for_older_version() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 6,
        "serial": 14,
        "lineage": "test-lineage",
        "carina_version": "0.0.1",
        "resources": []
    }"#;

    let outcome = check_and_migrate(json).unwrap();
    let migration = outcome
        .migration()
        .expect("migration info should be present for v6 → v7");
    assert_eq!(migration.from, 6);
    assert_eq!(migration.to, StateFile::CURRENT_VERSION);
    assert_eq!(outcome.state().version, 20);
    assert_eq!(
        finish_legacy_dependency_migration(outcome.into_state()).version,
        StateFile::CURRENT_VERSION
    );
}

#[test]
fn test_check_and_migrate_no_migration_info_for_current_version() {
    use super::check_and_migrate;

    let state = StateFile::new();
    let json = serde_json::to_string_pretty(&state).unwrap();
    let outcome = check_and_migrate(&json).unwrap();
    assert!(
        outcome.migration().is_none(),
        "current-version reads must not report a migration"
    );
    assert_eq!(outcome.state().version, StateFile::CURRENT_VERSION);
}

// `state.resources` has been managed-only since #3181. Historical releases
// could persist transient data-source results here. The parser now supplies a
// typed data-source address at the load boundary, so cleanup can target that
// exact row without treating every identifier-less managed row as disposable.
#[test]
fn typed_data_source_cleanup_removes_historical_null_identifier_row() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 7,
        "serial": 14,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [
            {
                "resource_type": "iam.Roles",
                "name": "admin_access_roles",
                "provider": "aws",
                "identifier": null,
                "attributes": { "arns": ["arn:aws:iam::1:role/OLD"] },
                "binding": "admin_access_roles"
            },
            {
                "resource_type": "s3.Bucket",
                "name": "log",
                "provider": "awscc",
                "identifier": "log-bucket",
                "attributes": { "name": "log-bucket" }
            }
        ]
    }"#;

    let mut state = check_and_migrate(json).expect("read").into_state();
    let data_source = carina_core::resource::DataSource::with_provider(
        "aws",
        "iam.Roles",
        "admin_access_roles",
        None,
    );
    assert_eq!(state.remove_legacy_data_source_rows(&[data_source]), 1);
    assert!(state.find_resource("awscc", "s3.Bucket", "log").is_some());
    assert!(
        state
            .find_resource("aws", "iam.Roles", "admin_access_roles")
            .is_none()
    );
}

#[test]
fn typed_data_source_cleanup_preserves_managed_row_with_identifier() {
    use super::check_and_migrate;

    let json = r#"{
        "version": 7,
        "serial": 14,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [
            {
                "resource_type": "shared.Type",
                "name": "shared",
                "provider": "test",
                "identifier": "provider-id",
                "attributes": { "name": "old-managed-value" },
                "binding": "shared"
            }
        ]
    }"#;

    let mut state = check_and_migrate(json).expect("read").into_state();
    let data_source =
        carina_core::resource::DataSource::with_provider("test", "shared.Type", "shared", None);

    assert_eq!(state.remove_legacy_data_source_rows(&[data_source]), 0);
    assert!(
        state
            .find_resource("test", "shared.Type", "shared")
            .is_some()
    );
}

#[test]
fn accepted_create_records_write_only_attributes() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};
    use carina_core::schema::{AttributeSchema, AttributeType};

    // Simulate a VPC resource with a write-only attribute (ipv4_netmask_length)
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    resource.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    resource.set_attr(
        "ipv4_netmask_length".to_string(),
        Value::Concrete(ConcreteValue::Int(16)),
    );

    // Provider returns state without write-only attributes (API doesn't return them)
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("vpc-123".to_string()),
        attributes: [(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let schema = ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("ipv4_netmask_length", AttributeType::int())
                .write_only()
                .build(),
        );
    let request = AcceptedRequest::create_for_test();
    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        Some(&schema),
        RowBasis::Applied(&request),
        None,
    )
    .unwrap();

    // The write-only attribute should be persisted in state
    assert_eq!(
        persisted_json_attribute(
            rs.persisted_attributes()
                .expect("provider row has persisted attributes"),
            "ipv4_netmask_length",
        ),
        Some(serde_json::json!(16))
    );
    assert_eq!(rs.write_only_attributes(), ["ipv4_netmask_length"]);

    // The regular attribute should still be there
    assert_eq!(
        persisted_json_attribute(
            rs.persisted_attributes()
                .expect("provider row has persisted attributes"),
            "cidr_block",
        ),
        Some(serde_json::json!("10.0.0.0/16"))
    );
}

#[test]
fn desired_redacted_secret_is_rejected_consistently_when_provider_omits_attribute() {
    use carina_core::resource::{ConcreteValue, DeferredValue, Resource, State, Value};
    use carina_core::value::SerializationError;

    let historical = persisted_attributes_from_state_json(&HashMap::from([(
        "placeholder".to_string(),
        structural_legacy_secret(&"ab".repeat(32)),
    )]));
    let redacted = historical
        .get("placeholder")
        .expect("structural tag should decode to a redacted secret");
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "a".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("x".into()),
            )))),
        ),
        ("b".to_string(), redacted),
    ])));
    let resource = Resource::with_provider("mock", "test.Resource", "main", None)
        .with_attribute("settings", desired);

    for provider_attributes in [
        HashMap::new(),
        HashMap::from([(
            "settings".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        )]),
    ] {
        let provider_state = State::existing(resource.id.clone(), provider_attributes);
        let error = resource_state_from_provider_state_for_resource_and_schema_for_test(
            &resolved(&resource),
            &provider_state,
            None,
            None,
            RowBasis::Applied(&AcceptedRequest::create_for_test()),
            None,
        )
        .expect_err("desired redacted secrets must never be written as new state");

        assert!(matches!(
            error,
            ResourceStateEncodingError::Serialization(
                SerializationError::RedactedSecretNotAllowed { .. }
            )
        ));
    }
}

#[test]
fn accepted_write_only_rejects_desired_redacted_secret() {
    use carina_core::resource::{ConcreteValue, DeferredValue, Resource, State, Value};
    use carina_core::schema::{AttributeSchema, AttributeType};
    use carina_core::value::SerializationError;

    let historical = persisted_attributes_from_state_json(&HashMap::from([(
        "placeholder".to_string(),
        structural_legacy_secret(&"ab".repeat(32)),
    )]));
    let redacted = historical
        .get("placeholder")
        .expect("structural tag should decode to a redacted secret");
    let desired = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "a".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("x".into()),
            )))),
        ),
        ("b".to_string(), redacted),
    ])));
    let resource = Resource::with_provider("mock", "test.Resource", "main", None)
        .with_attribute("settings", desired);
    let schema = ResourceSchema::new("test.Resource").attribute(
        AttributeSchema::optional("settings", AttributeType::map(AttributeType::string()))
            .write_only()
            .build(),
    );
    let error = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &State::existing(resource.id.clone(), HashMap::new()).with_identifier("main-id"),
        None,
        Some(&schema),
        RowBasis::Applied(&AcceptedRequest::create_for_test()),
        None,
    )
    .expect_err("accepted redacted values cannot be encoded as new state");

    assert!(matches!(
        error,
        ResourceStateEncodingError::Serialization(
            SerializationError::RedactedSecretNotAllowed { .. }
        )
    ));
}

#[test]
fn accepted_create_does_not_record_write_only_attribute_absent_from_desired() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};
    use carina_core::schema::{AttributeSchema, AttributeType};

    // Resource without write-only attribute specified
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    resource.set_attr(
        "cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("vpc-123".to_string()),
        attributes: [(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let schema = ResourceSchema::new("ec2.Vpc").attribute(
        AttributeSchema::optional("ipv4_netmask_length", AttributeType::int())
            .write_only()
            .build(),
    );
    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        Some(&schema),
        RowBasis::Applied(&AcceptedRequest::create_for_test()),
        None,
    )
    .unwrap();

    // Should NOT be in state since user didn't specify it
    assert!(
        !rs.persisted_attributes()
            .expect("provider row has persisted attributes")
            .contains_key("ipv4_netmask_length")
    );
    assert!(rs.write_only_attributes().is_empty());
}

#[test]
fn accepted_write_only_does_not_overwrite_protected_provider_value() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};
    use carina_core::schema::{AttributeSchema, AttributeType};

    // Resource with a write-only attribute
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    resource.set_attr(
        "some_attr".to_string(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("desired".into()),
        )))),
    );

    // Provider happens to return this attribute (unusual for write-only but possible)
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("vpc-123".to_string()),
        attributes: [(
            "some_attr".to_string(),
            Value::Concrete(ConcreteValue::String("from-api".into())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let schema = ResourceSchema::new("ec2.Vpc").attribute(
        AttributeSchema::optional("some_attr", AttributeType::string())
            .write_only()
            .build(),
    );
    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        Some(&schema),
        RowBasis::Applied(&AcceptedRequest::create_for_test()),
        None,
    )
    .unwrap();

    let stored = rs
        .persisted_attributes()
        .expect("provider row has persisted attributes")
        .redacted_secret_hash("some_attr")
        .expect("provider-returned secret value is protected");
    // The provider entry wins, but its value is still protected according to
    // the desired secret shape. Provenance overlay must not replace it with
    // the desired material or classify it as a host-carried value.
    let material = serde_json::to_vec(&serde_json::json!("from-api")).unwrap();
    assert!(stored.verify(material));
    // It remains marked write-only so a later read that omits it can carry the
    // accepted value instead of losing its provenance.
    assert_eq!(rs.write_only_attributes(), ["some_attr"]);
}

#[test]
fn test_write_only_attributes_serialization() {
    let mut rs = ResourceState::try_applied("ec2.Vpc", "my-vpc", "awscc", "vpc-123")
        .expect("fixture identifier is non-empty")
        .with_attribute("cidr_block".to_string(), serde_json::json!("10.0.0.0/16"))
        .with_attribute("ipv4_netmask_length".to_string(), serde_json::json!(16));
    rs.set_write_only_attributes_for_test(vec!["ipv4_netmask_length".to_string()]);

    let json = serde_json::to_string_pretty(&rs).unwrap();
    let deserialized: ResourceState = serde_json::from_str(&json).unwrap();

    assert_eq!(
        deserialized.write_only_attributes(),
        ["ipv4_netmask_length"]
    );
    assert_eq!(
        persisted_json_attribute(
            deserialized
                .persisted_attributes()
                .expect("deserialized row has persisted attributes"),
            "ipv4_netmask_length",
        ),
        Some(serde_json::json!(16))
    );
}

#[test]
fn test_write_only_attributes_omitted_when_empty() {
    let rs = ResourceState::new("s3.Bucket", "my-bucket", "awscc");
    let json = serde_json::to_string(&rs).unwrap();

    // write_only_attributes should not appear in JSON when empty
    assert!(
        !json.contains("write_only_attributes"),
        "write_only_attributes should be omitted when empty"
    );
}

#[test]
fn test_from_provider_state_secret_stored_as_hash() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, Resource, State as ProviderState, Value,
    };

    let mut resource = Resource::with_provider("awscc", "rds.db_instance", "my-db", None);
    resource.set_attr(
        "master_password".to_string(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        )))),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("my-db-id".to_string()),
        // Provider returns the actual password (since secret was unwrapped before sending)
        attributes: [(
            "master_password".to_string(),
            Value::Concrete(ConcreteValue::String("my-password".into())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();

    // State should store the hash, not the plain password
    let stored_json = persisted_json_attribute(
        rs.persisted_attributes()
            .expect("provider row has persisted attributes"),
        "master_password",
    )
    .expect("master_password should be stored");
    let stored = structural_secret_digest(&stored_json);
    assert!(
        !stored.contains("my-password"),
        "State should not contain the plain password"
    );
}

#[test]
fn from_provider_state_rehashes_previous_secret_hash_without_desired_secret() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let resource = Resource::with_provider("awscc", "db.Instance", "main", None);
    let existing = ResourceState::try_applied("db.Instance", "main", "awscc", "db-old")
        .expect("fixture identifier is non-empty")
        .with_attribute("password", structural_legacy_secret(&"ab".repeat(32)));
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("db-old".to_string()),
        attributes: [(
            "password".to_string(),
            Value::Concrete(ConcreteValue::String("plain-secret".into())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        None,
        RowBasis::Imported,
        None,
    )
    .expect("provider state should serialize");
    let stored_json = persisted_json_attribute(
        rs.persisted_attributes()
            .expect("provider row has persisted attributes"),
        "password",
    )
    .expect("password should be stored");
    let stored = structural_secret_digest(&stored_json);

    assert!(stored.starts_with("$argon2id$"), "got {stored}");
    assert!(!stored.contains("plain-secret"));
}

#[test]
fn lossy_state_json_rehashes_previous_secret_hash_without_desired_secret() {
    use carina_core::resource::{ConcreteValue, Resource, Value};

    let resource = Resource::with_provider("awscc", "db.Instance", "main", None);
    let attrs = HashMap::from([(
        "password".to_string(),
        Value::Concrete(ConcreteValue::String("plain-secret".into())),
    )]);
    let existing = persisted_attributes_from_state_json(&HashMap::from([(
        "password".to_string(),
        structural_legacy_secret(&"ab".repeat(32)),
    )]));

    let stored = ResourceState::deposed_attributes_without_previous(
        &resolved(&resource),
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&existing),
    );
    let stored = state_json_from_persisted_attributes(stored.as_persisted());
    let password =
        structural_secret_digest(stored.get("password").expect("password remains stored"));

    assert!(password.starts_with("$argon2id$"), "got {password}");
    assert!(!password.contains("plain-secret"));
}

#[test]
fn with_attributes_from_state_reuses_matching_secret_digest() {
    use carina_core::resource::{ConcreteValue, DeferredValue, ResourceId, State, Value};

    let id = ResourceId::with_provider_identity("mock", "test.resource", "stable", None);
    let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("stable-secret".into()),
    ))));
    let provider_state = State::existing(id, HashMap::from([("comment".to_string(), secret)]));
    let first =
        ResourceState::from_provider_state("test.resource", "stable", "mock", &provider_state)
            .unwrap();
    let first_encoded = persisted_json_attribute(
        first
            .persisted_attributes()
            .expect("first row has persisted attributes"),
        "comment",
    )
    .expect("first encoded secret");

    let second = first
        .into_writable()
        .expect("fixture is not in flight")
        .with_attributes_from_state(&provider_state)
        .unwrap();
    let second_encoded = persisted_json_attribute(
        second
            .persisted_attributes()
            .expect("second row has persisted attributes"),
        "comment",
    )
    .expect("second encoded secret");

    assert_eq!(
        first_encoded, second_encoded,
        "unchanged secret encoding churned"
    );
}

#[test]
fn with_attributes_from_state_keeps_raw_legacy_shaped_text_plain() {
    use carina_core::resource::{ConcreteValue, ResourceId, State, Value};

    let id = ResourceId::with_provider_identity("mock", "test.resource", "plain", None);
    let marker = format!("{LEGACY_SECRET_PREFIX}{}", "ab".repeat(32));
    let provider_state = State::existing(
        id,
        HashMap::from([(
            "comment".to_string(),
            Value::Concrete(ConcreteValue::String(marker.clone().into())),
        )]),
    );

    let row = ResourceState::from_provider_state("test.resource", "plain", "mock", &provider_state)
        .expect("marker-shaped provider plaintext is ordinary text");
    assert_eq!(
        persisted_json_attribute(
            row.persisted_attributes()
                .expect("provider row has persisted attributes"),
            "comment",
        ),
        Some(serde_json::json!(marker)),
    );
}

#[test]
fn accepted_write_only_keeps_raw_legacy_shaped_text_plain() {
    use carina_core::resource::{ConcreteValue, Resource, State, Value};
    use carina_core::schema::{AttributeSchema, AttributeType};

    let marker = format!("{LEGACY_SECRET_PREFIX}{}", "ab".repeat(32));
    let resource = Resource::with_provider("mock", "test.resource", "plain", None).with_attribute(
        "password",
        Value::Concrete(ConcreteValue::String(marker.clone().into())),
    );
    let schema = ResourceSchema::new("test.resource").attribute(
        AttributeSchema::optional("password", AttributeType::string())
            .write_only()
            .build(),
    );
    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &State::existing(resource.id.clone(), HashMap::new()).with_identifier("plain-id"),
        None,
        Some(&schema),
        RowBasis::Applied(&AcceptedRequest::create_for_test()),
        None,
    )
    .expect("marker-shaped desired plaintext is ordinary text");
    assert_eq!(
        persisted_json_attribute(
            row.persisted_attributes()
                .expect("provider row has persisted attributes"),
            "password",
        ),
        Some(serde_json::json!(marker)),
    );
}

fn nested_write_only_state_fixture(
    secret: &str,
) -> (carina_core::resource::Resource, ResourceSchema) {
    use carina_core::resource::{ConcreteValue, DeferredValue, Resource, Value};
    use carina_core::schema::{AttributeSchema, AttributeType, StructField};

    let config = Value::Concrete(ConcreteValue::Map(IndexMap::from([
        (
            "issuer".to_string(),
            Value::Concrete(ConcreteValue::String("https://issuer.example.com".into())),
        ),
        (
            "client_secret".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String(secret.into()),
            )))),
        ),
    ])));
    let resource = Resource::with_provider("mock", "test.Resource", "nested", None)
        .with_attribute("config", config);
    let schema = ResourceSchema::new("test.Resource").attribute(
        AttributeSchema::optional(
            "config",
            AttributeType::struct_(
                "Config",
                vec![
                    StructField::optional("issuer", AttributeType::string()).build(),
                    StructField::optional("client_secret", AttributeType::string())
                        .write_only()
                        .build(),
                ],
            ),
        )
        .build(),
    );
    (resource, schema)
}

#[test]
fn nested_write_only_record_lifecycle_hashes_sent_secrets_and_clears_on_remove() {
    use carina_core::resource::State as ProviderState;

    let provider_state = |resource: &carina_core::resource::Resource| {
        ProviderState::existing(
            resource.id.clone(),
            HashMap::from([(
                "config".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "issuer".to_string(),
                    Value::Concrete(ConcreteValue::String("https://issuer.example.com".into())),
                )]))),
            )]),
        )
        .with_identifier("resource-id")
    };

    // Add: a sent attribute without prior leaf provenance establishes it.
    let (added_resource, schema) = nested_write_only_state_fixture("add-secret");
    let add = AcceptedRequest::update_for_test(&["config"], &[]);
    let added = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&added_resource),
        &provider_state(&added_resource),
        None,
        Some(&schema),
        RowBasis::Applied(&add),
        None,
    )
    .expect("sent nested write-only attribute records provenance");
    let added_leaves = added
        .nested_write_only()
        .get("config")
        .expect("config provenance exists");
    assert_eq!(added_leaves.len(), 1);
    assert!(
        added_leaves[0]
            .1
            .as_json()
            .get(carina_core::value::PERSISTED_SECRET_KEY)
            .is_some(),
        "nested secret must be stored as an opaque hash"
    );
    assert!(
        !serde_json::to_string(&added)
            .unwrap()
            .contains("add-secret")
    );

    // Replace and Reassert both arrive at state writeback as a sent key. A
    // replacement rotates the recorded hash; a reassert preserves it.
    let (replaced_resource, _) = nested_write_only_state_fixture("replace-secret");
    let replace = AcceptedRequest::update_for_test(&["config"], &[]);
    let replaced = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&replaced_resource),
        &provider_state(&replaced_resource),
        Some(&added),
        Some(&schema),
        RowBasis::Applied(&replace),
        None,
    )
    .expect("replace refreshes nested write-only provenance");
    assert_ne!(
        added.nested_write_only().get("config"),
        replaced.nested_write_only().get("config")
    );

    let reassert = AcceptedRequest::update_for_test(&["config"], &[]);
    let reasserted = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&replaced_resource),
        &provider_state(&replaced_resource),
        Some(&replaced),
        Some(&schema),
        RowBasis::Applied(&reassert),
        None,
    )
    .expect("reassert refreshes nested write-only provenance");
    assert_eq!(
        replaced.nested_write_only().get("config"),
        reasserted.nested_write_only().get("config")
    );

    let removed_resource =
        carina_core::resource::Resource::with_provider("mock", "test.Resource", "nested", None);
    let remove = AcceptedRequest::update_for_test(&[], &["config"]);
    let removed = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&removed_resource),
        &ProviderState::existing(removed_resource.id.clone(), HashMap::new())
            .with_identifier("resource-id"),
        Some(&reasserted),
        Some(&schema),
        RowBasis::Applied(&remove),
        None,
    )
    .expect("remove clears nested write-only provenance");
    assert!(removed.nested_write_only().get("config").is_none());
}

#[test]
fn nested_write_only_record_hashes_leaf_under_secret_wrapped_struct() {
    use carina_core::resource::{ConcreteValue, DeferredValue, Resource, State, Value};
    use carina_core::schema::{AttributeSchema, AttributeType, StructField};

    let config = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::Map(IndexMap::from([
            (
                "issuer".to_string(),
                Value::Concrete(ConcreteValue::String("https://issuer.example.com".into())),
            ),
            (
                "client_secret".to_string(),
                Value::Concrete(ConcreteValue::String("wrapped-secret".into())),
            ),
        ])),
    ))));
    let resource = Resource::with_provider("mock", "test.Resource", "nested", None)
        .with_attribute("config", config);
    let schema = ResourceSchema::new("test.Resource").attribute(
        AttributeSchema::optional(
            "config",
            AttributeType::struct_(
                "Config",
                vec![
                    StructField::optional("issuer", AttributeType::string()).build(),
                    StructField::optional("client_secret", AttributeType::string())
                        .write_only()
                        .build(),
                ],
            ),
        )
        .build(),
    );
    let provider_state = State::existing(
        resource.id.clone(),
        HashMap::from([(
            "config".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "issuer".to_string(),
                Value::Concrete(ConcreteValue::String("https://issuer.example.com".into())),
            )]))),
        )]),
    )
    .with_identifier("resource-id");
    let accepted = AcceptedRequest::update_for_test(&["config"], &[]);

    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        Some(&schema),
        RowBasis::Applied(&accepted),
        None,
    )
    .expect("secret-wrapped nested write-only leaf is persistable");

    let leaves = row
        .nested_write_only()
        .get("config")
        .expect("config provenance exists");
    assert_eq!(leaves.len(), 1);
    assert!(
        leaves[0]
            .1
            .as_json()
            .get(carina_core::value::PERSISTED_SECRET_KEY)
            .is_some(),
        "the leaf inherited from a secret wrapper must be hashed"
    );
    assert!(
        !serde_json::to_string(&row)
            .unwrap()
            .contains("wrapped-secret")
    );
}

#[test]
fn resumed_persisted_create_records_nested_write_only_sent_values() {
    use carina_core::provider::{
        AppliedIdentifier, AppliedState, CreateAttempt, CreateObservation, ObservedCreate,
        PendingToken, PersistedAttributes, Sent,
    };

    let (resource, schema) = nested_write_only_state_fixture("resumed-secret");
    let identifier = AppliedIdentifier::new("resource-id").unwrap();
    let sent_config = PersistedValue::encode_desired_value(
        resource
            .attributes
            .get("config")
            .expect("fixture has a sent config"),
        None,
    )
    .unwrap();
    let sent_secrets = PersistedAttributes::from_persisted_values(HashMap::from([(
        "config".to_string(),
        sent_config,
    )]))
    .unwrap();
    let pending = CreateAttempt::for_test(resource.id.clone())
        .issued(identifier.clone(), Sent::AllRequestAttributes)
        .pending(PendingToken::new("resume-token"), 1)
        .persist(PersistedAttributes::default(), sent_secrets);
    let observed_state = AppliedState::new(
        resource.id.clone(),
        identifier.as_str(),
        HashMap::from([(
            "config".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "issuer".to_string(),
                Value::Concrete(ConcreteValue::String("https://issuer.example.com".into())),
            )]))),
        )]),
    )
    .unwrap();
    let created = ObservedCreate::from_in_flight_for_test(
        pending,
        identifier,
        CreateObservation::Observed(observed_state),
    );

    let row = resource_state_from_observation_for_test(
        &resolved(&resource),
        &created,
        None,
        Some(&schema),
        None,
    )
    .expect("resumed create observation must be persistable");

    let leaves = row
        .nested_write_only()
        .get("config")
        .expect("persisted sent envelope must establish nested provenance");
    assert_eq!(leaves.len(), 1);
    assert!(
        leaves[0]
            .1
            .as_json()
            .get(carina_core::value::PERSISTED_SECRET_KEY)
            .is_some(),
        "the resumed sent secret must remain hashed"
    );
}

#[test]
fn unobserved_not_found_create_records_identifier_and_partial_read() {
    use carina_core::provider::{
        AppliedIdentifier, CreateAttempt, CreateFailureKind, CreateObservation, Observation,
        ProviderError, ReadOutcome, Sent, UnobservedReason,
    };

    let resource = Resource::with_provider("awscc", "logs.LogGroup", "logs", None).with_attribute(
        "log_group_name",
        Value::Concrete(ConcreteValue::String("application-logs".into())),
    );
    let identifier = AppliedIdentifier::new("application-logs").unwrap();
    let failure = CreateAttempt::for_test(resource.id.clone())
        .issued(identifier.clone(), Sent::only(Vec::<String>::new()))
        .failed(
            Observation::Read(ReadOutcome::not_found(resource.id.clone())),
            ProviderError::api_error("create access denied"),
        );
    let created = match failure.into_kind() {
        CreateFailureKind::Created { created, error: _ } => created,
        CreateFailureKind::Untracked(error) => {
            panic!("identifier-bearing failure became untracked: {error}")
        }
        CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
            "identifier-bearing failure became outcome unknown for {}: {error}",
            unknown.id()
        ),
        CreateFailureKind::InFlight { pending, error } => {
            panic!("identifier-bearing failure remained in flight {pending:?}: {error}")
        }
    };

    match created.observation() {
        CreateObservation::Observed(state) => {
            panic!("not-found read-back became observed: {state:?}")
        }
        CreateObservation::Unobserved(reason) => match reason {
            UnobservedReason::NotFound => {}
            UnobservedReason::ReadFailed(error) => {
                panic!("not-found read-back became a read failure: {error}")
            }
        },
    }

    let row =
        resource_state_from_observation_for_test(&resolved(&resource), &created, None, None, None)
            .expect("unobserved create must remain persistable");

    assert_eq!(
        row.identifier().map(AppliedIdentifier::as_str),
        Some("application-logs")
    );
    assert_eq!(
        row.partial_read(),
        PartialReadMarker::new(
            PendingReRead::new(ReReadReason::NotFound),
            BTreeSet::from(["log_group_name".to_string()]),
        )
        .as_ref()
    );
}

#[test]
fn unobserved_read_failed_observation_records_identifier_and_provider_error_detail() {
    use carina_core::provider::{
        AppliedIdentifier, CreateAttempt, CreateFailureKind, Observation, ProviderError, Sent,
    };

    let resource = Resource::with_provider("awscc", "logs.LogGroup", "logs", None).with_attribute(
        "log_group_name",
        Value::Concrete(ConcreteValue::String("application-logs".into())),
    );
    let identifier = AppliedIdentifier::new("application-logs").unwrap();
    let read_error = ProviderError::api_error("read-back access denied");
    let read_error_display = read_error.to_string();
    let failure = CreateAttempt::for_test(resource.id.clone())
        .issued(identifier, Sent::only(Vec::<String>::new()))
        .failed(
            Observation::ReadFailed(read_error),
            ProviderError::api_error("create finishing failed"),
        );
    let created = match failure.into_kind() {
        CreateFailureKind::Created { created, error: _ } => created,
        CreateFailureKind::Untracked(error) => {
            panic!("identifier-bearing failure became untracked: {error}")
        }
        CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
            "identifier-bearing failure became outcome unknown for {}: {error}",
            unknown.id()
        ),
        CreateFailureKind::InFlight { pending, error } => {
            panic!("identifier-bearing failure remained in flight {pending:?}: {error}")
        }
    };

    let row =
        resource_state_from_observation_for_test(&resolved(&resource), &created, None, None, None)
            .expect("read-failed create must remain persistable");

    assert_eq!(
        row.identifier().map(AppliedIdentifier::as_str),
        Some("application-logs")
    );
    assert_eq!(
        row.partial_read().map(PartialReadMarker::pending),
        Some(&PendingReRead::new(ReReadReason::ReadFailed {
            detail: read_error_display,
        }))
    );
}

#[test]
fn observed_create_records_nested_leaf_from_desired_under_ancestor_secret_wrappers() {
    use carina_core::differ::Diff;
    use carina_core::provider::{
        AppliedIdentifier, AppliedState, CreateAttempt, CreateFailureKind, CreateObservation,
        Observation, ObservedCreate, PendingToken, PersistedAttributes, ProviderError, ReadOutcome,
        Sent,
    };
    use carina_core::resource::{ConcreteValue, DeferredValue, Resource, State, Value};
    use carina_core::schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
    };

    let issuer = || Value::Concrete(ConcreteValue::String("https://issuer.example.com".into()));
    let action = |config| {
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(IndexMap::from([(
                "authenticate_oidc_config".to_string(),
                config,
            )])),
        )]))
    };
    let secret = |value| Value::Deferred(DeferredValue::Secret(Box::new(value)));

    for (persisted_envelope, wrap_default_actions) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let desired_config = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("issuer".to_string(), issuer()),
            (
                "client_secret".to_string(),
                Value::Concrete(ConcreteValue::String("accepted-secret".into())),
            ),
        ])));
        let desired_actions = if wrap_default_actions {
            secret(action(desired_config))
        } else {
            action(secret(desired_config))
        };
        let wrapper = if wrap_default_actions {
            "wrapped-actions"
        } else {
            "wrapped-config"
        };
        let identity = match (persisted_envelope, wrap_default_actions) {
            (false, false) => "request-wrapped-config",
            (false, true) => "request-wrapped-actions",
            (true, false) => "persisted-wrapped-config",
            (true, true) => "persisted-wrapped-actions",
        };
        let resource =
            Resource::with_provider("mock", "elasticloadbalancingv2.Listener", identity, None)
                .with_attribute("default_actions", desired_actions);
        let schema = ResourceSchema::new("elasticloadbalancingv2.Listener").attribute(
            AttributeSchema::optional(
                "default_actions",
                AttributeType::list(AttributeType::struct_(
                    "Action",
                    vec![
                        StructField::optional(
                            "authenticate_oidc_config",
                            AttributeType::struct_(
                                "AuthenticateOidcConfig",
                                vec![
                                    StructField::optional("issuer", AttributeType::string())
                                        .build(),
                                    StructField::optional("client_secret", AttributeType::string())
                                        .write_only()
                                        .build(),
                                ],
                            ),
                        )
                        .build(),
                    ],
                )),
            )
            .create_only()
            .build(),
        );
        let identifier = AppliedIdentifier::new(format!("{identity}-id")).unwrap();
        let observed_actions = action(Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "issuer".to_string(),
            issuer(),
        )]))));
        let observed_state = || {
            AppliedState::new(
                resource.id.clone(),
                identifier.as_str(),
                HashMap::from([("default_actions".to_string(), observed_actions.clone())]),
            )
            .unwrap()
        };
        let created = if persisted_envelope {
            let sent_actions = PersistedValue::encode_desired_value(
                resource
                    .attributes
                    .get("default_actions")
                    .expect("fixture has sent default actions"),
                None,
            )
            .unwrap();
            let sent_secrets = PersistedAttributes::from_persisted_values(HashMap::from([(
                "default_actions".to_string(),
                sent_actions,
            )]))
            .unwrap();
            let pending = CreateAttempt::for_test(resource.id.clone())
                .issued(identifier.clone(), Sent::AllRequestAttributes)
                .pending(PendingToken::new(format!("resume-{wrapper}")), 1)
                .persist(PersistedAttributes::default(), sent_secrets);
            ObservedCreate::from_in_flight_for_test(
                pending,
                identifier.clone(),
                CreateObservation::Observed(observed_state()),
            )
        } else {
            let failed = CreateAttempt::for_test(resource.id.clone())
                .issued(identifier.clone(), Sent::AllRequestAttributes)
                .failed(
                    Observation::Read(ReadOutcome::found(observed_state())),
                    ProviderError::internal("synthetic post-create finishing failure"),
                );
            match failed.into_kind() {
                CreateFailureKind::Created {
                    created,
                    error: _error,
                } => created,
                CreateFailureKind::Untracked(error) => {
                    panic!("expected an observed create, got untracked failure: {error}")
                }
                CreateFailureKind::OutcomeUnknown { unknown, error } => panic!(
                    "expected an observed create, got unknown outcome for {}: {error}",
                    unknown.id()
                ),
                CreateFailureKind::InFlight {
                    pending: _pending,
                    error,
                } => panic!("expected an observed create, got in-flight failure: {error}"),
            }
        };

        let row = resource_state_from_observation_for_test(
            &resolved(&resource),
            &created,
            None,
            Some(&schema),
            None,
        )
        .expect("observed create must be persistable");
        let leaves = row
            .nested_write_only()
            .get("default_actions")
            .expect("sent bearing attribute must have a provenance entry");
        assert_eq!(
            leaves.len(),
            1,
            "ancestor secret wrapper must not hide its write-only leaf"
        );
        assert!(
            leaves[0]
                .1
                .as_json()
                .get(carina_core::value::PERSISTED_SECRET_KEY)
                .is_some(),
            "the projected client secret must be hashed"
        );

        let current = State::existing(
            resource.id.clone(),
            row.persisted_attributes()
                .expect("observed create has stored attributes")
                .to_values(),
        )
        .with_identifier(identifier.as_str());
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(row.clone())
            .expect("observed row has a unique address");
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            schema
                .clone()
                .validate()
                .expect("test schema must be valid"),
        );
        let saved = state_file.build_saved_attrs().lift(&schemas);

        assert!(
            matches!(
                diff(
                    &resolved(&resource),
                    &current,
                    &saved,
                    Some(&row.explicit),
                    Some(&schema),
                ),
                Diff::NoChange(_)
            ),
            "the next plan must converge instead of replacing the create-only bearing attribute"
        );
    }
}

#[test]
fn lossy_state_json_preserves_hydrated_secret_hash_without_double_hashing() {
    use carina_core::resource::Resource;

    let resource = Resource::with_provider("awscc", "db.Instance", "main", None);
    let previous = structural_legacy_secret(&"ab".repeat(32));
    let existing = persisted_attributes_from_state_json(&HashMap::from([(
        "password".to_string(),
        previous.clone(),
    )]));
    let upgraded = existing
        .encoded_value("password")
        .expect("legacy state marker is upgraded")
        .clone();
    let attrs = HashMap::from([(
        "password".to_string(),
        existing
            .get("password")
            .expect("structural secret decodes to a typed value"),
    )]);

    let stored = ResourceState::deposed_attributes_without_previous(
        &resolved(&resource),
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&existing),
    );

    let stored = state_json_from_persisted_attributes(stored.as_persisted());
    assert_eq!(stored.get("password"), Some(upgraded.as_json()));
}

#[test]
fn from_provider_state_stores_plain_value_when_secret_is_demoted() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};

    let resource = Resource::with_provider("awscc", "db.Instance", "main", None).with_attribute(
        "password",
        Value::Concrete(ConcreteValue::String("plain-secret".into())),
    );
    let existing = ResourceState::try_applied("db.Instance", "main", "awscc", "db-old")
        .expect("fixture identifier is non-empty")
        .with_attribute("password", structural_legacy_secret(&"ab".repeat(32)));
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("db-old".to_string()),
        attributes: [(
            "password".to_string(),
            Value::Concrete(ConcreteValue::String("plain-secret".into())),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        None,
        RowBasis::Imported,
        None,
    )
    .expect("provider state should serialize");

    assert_eq!(
        persisted_json_attribute(
            rs.persisted_attributes()
                .expect("provider row has persisted attributes"),
            "password",
        ),
        Some(serde_json::json!("plain-secret"))
    );
}

#[test]
fn write_only_secret_to_plain_demotion_merges_plain_desired_value() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};
    use carina_core::schema::{AttributeSchema, AttributeType};

    let resource = Resource::with_provider("awscc", "db.Instance", "main", None).with_attribute(
        "password",
        Value::Concrete(ConcreteValue::String("plain-secret".into())),
    );
    let mut existing = ResourceState::try_applied("db.Instance", "main", "awscc", "db-old")
        .expect("fixture identifier is non-empty")
        .with_attribute("password", structural_legacy_secret(&"ab".repeat(32)));
    existing.set_write_only_attributes_for_test(vec!["password".to_string()]);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("db-old".to_string()),
        attributes: HashMap::new(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let schema = ResourceSchema::new("db.Instance").attribute(
        AttributeSchema::optional("password", AttributeType::string())
            .write_only()
            .build(),
    );
    let request = AcceptedRequest::update_for_test(&["password"], &[]);
    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        Some(&existing),
        Some(&schema),
        RowBasis::Applied(&request),
        None,
    )
    .expect("provider state should serialize");

    assert_eq!(
        persisted_json_attribute(
            rs.persisted_attributes()
                .expect("provider row has persisted attributes"),
            "password",
        ),
        Some(serde_json::json!("plain-secret"))
    );
    assert_eq!(rs.write_only_attributes(), ["password"]);
}

#[test]
fn accepted_update_ignores_sent_non_write_only_key_absent_from_writeback_resource() {
    use carina_core::resource::{ConcreteValue, Resource, State as ProviderState, Value};
    use carina_core::schema::{AttributeSchema, AttributeType};

    let resource = Resource::with_provider("mock", "test.Resource", "main", None).with_attribute(
        "password",
        Value::Concrete(ConcreteValue::String("old".into())),
    );
    let mut existing = ResourceState::try_applied("test.Resource", "main", "mock", "resource-id")
        .expect("fixture identifier is non-empty")
        .with_attribute("password", serde_json::json!("old"));
    existing.set_write_only_attributes_for_test(vec!["password".to_string()]);
    let schema = ResourceSchema::new("test.Resource").attribute(
        AttributeSchema::optional("password", AttributeType::string())
            .write_only()
            .build(),
    );
    let request = AcceptedRequest::update_for_test(&["apply_time_normalized_key"], &[]);

    let row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &ProviderState::existing(resource.id.clone(), HashMap::new())
            .with_identifier("resource-id"),
        Some(&existing),
        Some(&schema),
        RowBasis::Applied(&request),
        None,
    )
    .expect("non-write-only normalized patch keys must not fail state writeback");

    assert_eq!(
        persisted_json_attribute(
            row.persisted_attributes()
                .expect("row has persisted attributes"),
            "password",
        ),
        Some(serde_json::json!("old"))
    );
}

#[test]
fn orphan_refresh_without_schema_carries_recorded_write_only_value() {
    use carina_core::resource::{ResourceId, State as ProviderState};

    let id = ResourceId::with_provider_identity("mock", "test.Resource", "orphan", None);
    let mut existing = ResourceState::try_applied("test.Resource", "orphan", "mock", "resource-id")
        .expect("fixture identifier is non-empty")
        .with_attribute("password", serde_json::json!("old"));
    existing.set_write_only_attributes_for_test(vec!["password".to_string()]);
    let fresh = ProviderState::existing(id, HashMap::new()).with_identifier("resource-id");

    let refreshed = existing
        .refreshable()
        .expect("applied row can refresh")
        .refreshed_from_provider_state(&fresh, None)
        .expect("schema-less orphan refresh should write state");

    assert_eq!(
        persisted_json_attribute(
            refreshed
                .persisted_attributes()
                .expect("refreshed row has attributes"),
            "password",
        ),
        Some(serde_json::json!("old"))
    );
    assert_eq!(refreshed.write_only_attributes(), ["password"]);
}

#[test]
fn orphan_refresh_drops_provider_echoed_write_only_value_absent_from_previous() {
    use carina_core::resource::{ConcreteValue, ResourceId, State as ProviderState, Value};
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let id = ResourceId::with_provider_identity("mock", "test.Resource", "orphan", None);
    let existing = ResourceState::try_applied("test.Resource", "orphan", "mock", "resource-id")
        .expect("fixture identifier is non-empty");
    let schema = ResourceSchema::new("test.Resource").attribute(
        AttributeSchema::optional("password", AttributeType::string())
            .write_only()
            .build(),
    );
    let fresh = ProviderState::existing(
        id,
        HashMap::from([(
            "password".to_string(),
            Value::Concrete(ConcreteValue::String("provider-plaintext".into())),
        )]),
    )
    .with_identifier("resource-id");

    let refreshed = existing
        .refreshable()
        .expect("applied row can refresh")
        .refreshed_from_provider_state(&fresh, Some(&schema))
        .expect("orphan refresh should write state");

    assert_eq!(
        persisted_json_attribute(
            refreshed
                .persisted_attributes()
                .expect("refreshed row has attributes"),
            "password",
        ),
        None
    );
    assert!(refreshed.write_only_attributes().is_empty());
}

#[test]
fn managed_refresh_infers_legacy_unlisted_write_only_values_from_schema() {
    use carina_core::resource::{ConcreteValue, ResourceId, State, Value};
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};

    let id = ResourceId::with_provider_identity("mock", "test.resource", "main", None);
    let row = ResourceState::try_applied("test.resource", "main", "mock", "provider-id")
        .expect("fixture identifier is non-empty")
        .with_attribute("password", serde_json::json!("last-sent"));
    assert!(row.write_only_attributes().is_empty());
    let mut state_file = StateFile::new();
    state_file
        .upsert_resource(row)
        .expect("fixture row should be valid");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(
                AttributeSchema::optional("password", AttributeType::string())
                    .write_only()
                    .build(),
            )
            .validate()
            .expect("test schema must be valid"),
    );

    let mut omitted = State::existing(id.clone(), HashMap::new()).with_identifier("provider-id");
    state_file.restore_recorded_write_only_attributes_in_state(&mut omitted, &schemas);
    assert_eq!(
        omitted.attributes.get("password"),
        Some(&Value::Concrete(ConcreteValue::String("last-sent".into())))
    );

    let provider_value = Value::Concrete(ConcreteValue::String("provider-value".into()));
    let mut returned = State::existing(
        id,
        HashMap::from([("password".to_string(), provider_value.clone())]),
    )
    .with_identifier("provider-id");
    state_file.restore_recorded_write_only_attributes_in_state(&mut returned, &schemas);
    assert_eq!(returned.attributes.get("password"), Some(&provider_value));
}

#[test]
fn lossy_state_json_merges_previous_nested_secret_hash_per_leaf() {
    use carina_core::resource::{ConcreteValue, Resource, Value};
    use indexmap::IndexMap;

    let resource = Resource::with_provider("awscc", "ec2.Vpc", "main", None);
    let previous_tags = serde_json::json!({
        "Name": "old-name",
        "SecretTag": structural_legacy_secret(&"ab".repeat(32)),
    });
    let mut provider_tags = IndexMap::new();
    provider_tags.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("new-name".into())),
    );
    provider_tags.insert(
        "SecretTag".to_string(),
        Value::Concrete(ConcreteValue::String("plain-secret".into())),
    );
    let attrs = HashMap::from([(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(provider_tags)),
    )]);
    let existing = persisted_attributes_from_state_json(&HashMap::from([(
        "tags".to_string(),
        previous_tags.clone(),
    )]));

    let stored = ResourceState::deposed_attributes_without_previous(
        &resolved(&resource),
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&existing),
    );
    let stored_json = state_json_from_persisted_attributes(stored.as_persisted());
    let stored_tags = stored_json
        .get("tags")
        .and_then(|value| value.as_object())
        .expect("tags should remain a map");
    let secret = structural_secret_digest(
        stored_tags
            .get("SecretTag")
            .expect("secret tag remains stored"),
    );

    assert_eq!(
        stored_tags.get("Name"),
        Some(&serde_json::json!("new-name"))
    );
    assert!(secret.starts_with("$argon2id$"), "got {secret}");
    assert!(!secret.contains("plain-secret"));

    let second_existing = persisted_attributes_from_state_json(&HashMap::from([(
        "tags".to_string(),
        stored_json["tags"].clone(),
    )]));
    let stored_again = ResourceState::deposed_attributes_without_previous(
        &resolved(&resource),
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&second_existing),
    );
    assert_eq!(
        stored_again.as_persisted().get("tags"),
        stored.as_persisted().get("tags")
    );
}

#[test]
fn lossy_state_json_drops_omitted_non_secret_sibling_but_keeps_missing_hash_leaf() {
    use carina_core::resource::{ConcreteValue, Resource, Value};
    use indexmap::IndexMap;

    let resource = Resource::with_provider("awscc", "ec2.Vpc", "main", None);
    let previous_tags = serde_json::json!({
        "Name": "old-name",
        "SecretTag": structural_legacy_secret(&"ab".repeat(32)),
    });
    let attrs = HashMap::from([(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(IndexMap::new())),
    )]);
    let existing =
        persisted_attributes_from_state_json(&HashMap::from([("tags".to_string(), previous_tags)]));

    let stored = ResourceState::deposed_attributes_without_previous(
        &resolved(&resource),
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&existing),
    );
    let stored = state_json_from_persisted_attributes(stored.as_persisted());
    let stored_tags = stored
        .get("tags")
        .and_then(|value| value.as_object())
        .expect("tags should remain a map");
    let secret = structural_secret_digest(
        stored_tags
            .get("SecretTag")
            .expect("missing secret tag keeps the previous hash"),
    );

    assert!(!stored_tags.contains_key("Name"));
    assert!(secret.starts_with("argon2-legacy:"), "got {secret}");
}

#[test]
fn lossy_state_json_hashes_whole_array_when_secret_hash_alignment_is_untrusted() {
    use carina_core::resource::{ConcreteValue, Resource, Value};
    use indexmap::IndexMap;

    let resource = Resource::with_provider("awscc", "service.Widget", "main", None);
    let previous_items = serde_json::json!([
        {
            "Name": "one",
            "Token": structural_legacy_secret(&"ab".repeat(32)),
        },
        {
            "Name": "two",
            "Public": "kept",
        }
    ]);
    let mut first_provider_item = IndexMap::new();
    first_provider_item.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("two".into())),
    );
    first_provider_item.insert(
        "Public".to_string(),
        Value::Concrete(ConcreteValue::String("kept".into())),
    );
    let mut second_provider_item = IndexMap::new();
    second_provider_item.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("one".into())),
    );
    second_provider_item.insert(
        "Token".to_string(),
        Value::Concrete(ConcreteValue::String("plain-secret".into())),
    );
    let attrs = HashMap::from([(
        "items".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(first_provider_item)),
            Value::Concrete(ConcreteValue::Map(second_provider_item)),
        ])),
    )]);
    let existing = persisted_attributes_from_state_json(&HashMap::from([(
        "items".to_string(),
        previous_items,
    )]));

    let stored = ResourceState::deposed_attributes_without_previous(
        &resolved(&resource),
        None,
        &attrs,
        PreviousSecretHashAuthority::AllPreviouslyHashedKeys(&existing),
    );
    let stored = state_json_from_persisted_attributes(stored.as_persisted());
    let stored_items = structural_secret_digest(
        stored
            .get("items")
            .expect("untrusted array alignment hashes the whole array"),
    );

    assert!(stored_items.starts_with("$argon2id$"), "got {stored_items}");
    assert!(!stored_items.contains("plain-secret"));
}

#[test]
fn test_from_provider_state_secret_in_map_stored_as_hash() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, Resource, State as ProviderState, Value,
    };

    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    let mut tags_map = IndexMap::new();
    tags_map.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("test".into())),
    );
    tags_map.insert(
        "SecretTag".to_string(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("super-secret-value".into()),
        )))),
    );
    resource.set_attr(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(tags_map)),
    );

    let mut state_tags = IndexMap::new();
    state_tags.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("test".into())),
    );
    state_tags.insert(
        "SecretTag".to_string(),
        Value::Concrete(ConcreteValue::String("super-secret-value".into())),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("vpc-123".to_string()),
        attributes: [(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::Map(state_tags)),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();

    // The tags map in state should have the hash for SecretTag
    let tags_json = persisted_json_attribute(
        rs.persisted_attributes()
            .expect("provider row has persisted attributes"),
        "tags",
    )
    .unwrap();
    let tags_obj = tags_json.as_object().unwrap();

    // Name should be plain
    assert_eq!(tags_obj.get("Name").unwrap().as_str().unwrap(), "test");

    // SecretTag should be stored as a hash, not the plain value
    let secret_stored = structural_secret_digest(tags_obj.get("SecretTag").unwrap());
    assert!(
        !secret_stored.contains("super-secret-value"),
        "State should not contain the plain secret value in map"
    );
}

#[test]
fn test_from_provider_state_secret_in_map_preserves_provider_extra_keys() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, Resource, State as ProviderState, Value,
    };

    // User specifies only SecretTag in tags
    let mut resource = Resource::with_provider("awscc", "ec2.Vpc", "my-vpc", None);
    let mut tags_map = IndexMap::new();
    tags_map.insert(
        "SecretTag".to_string(),
        Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("super-secret-value".into()),
        )))),
    );
    resource.set_attr(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(tags_map)),
    );

    // Provider returns extra keys (e.g., CloudControl adds Name automatically)
    let mut state_tags = IndexMap::new();
    state_tags.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("test".into())),
    );
    state_tags.insert(
        "ExtraTag".to_string(),
        Value::Concrete(ConcreteValue::String("extra-value".into())),
    );
    state_tags.insert(
        "SecretTag".to_string(),
        Value::Concrete(ConcreteValue::String("super-secret-value".into())),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("vpc-123".to_string()),
        attributes: [(
            "tags".to_string(),
            Value::Concrete(ConcreteValue::Map(state_tags)),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();

    let tags_json = persisted_json_attribute(
        rs.persisted_attributes()
            .expect("provider row has persisted attributes"),
        "tags",
    )
    .unwrap();
    let tags_obj = tags_json.as_object().unwrap();

    // Provider-only keys should be preserved from the provider state
    assert_eq!(tags_obj.get("Name").unwrap().as_str().unwrap(), "test");
    assert_eq!(
        tags_obj.get("ExtraTag").unwrap().as_str().unwrap(),
        "extra-value"
    );

    // SecretTag should be stored as a hash, not the plain value
    let secret_stored = structural_secret_digest(tags_obj.get("SecretTag").unwrap());
    assert!(
        !secret_stored.contains("super-secret-value"),
        "State should not contain the plain secret value in map"
    );
}

#[test]
fn test_from_provider_state_secret_in_list_stored_as_hash() {
    use carina_core::resource::{
        ConcreteValue, DeferredValue, Resource, State as ProviderState, Value,
    };

    let mut resource = Resource::with_provider("awscc", "test.resource", "my-res", None);
    resource.set_attr(
        "values".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("public".into())),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("secret-item".into()),
            )))),
        ])),
    );

    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("res-123".to_string()),
        attributes: [(
            "values".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("public".into())),
                Value::Concrete(ConcreteValue::String("secret-item".into())),
            ])),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let rs = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();

    let values_json = persisted_json_attribute(
        rs.persisted_attributes()
            .expect("provider row has persisted attributes"),
        "values",
    )
    .unwrap();
    let values_arr = values_json.as_array().unwrap();

    // First item should be plain
    assert_eq!(values_arr[0].as_str().unwrap(), "public");

    // Second item should be stored as a hash
    let secret_stored = structural_secret_digest(&values_arr[1]);
    assert!(secret_stored.starts_with("$argon2id$"));
}

#[test]
fn published_exports_build_remote_bindings_returns_exports() {
    let mut state = StateFile::new();
    state.exports.insert(
        "account_id".to_string(),
        PersistedExportValue::encode(&Value::Concrete(ConcreteValue::String(
            "123456789012".into(),
        )))
        .expect("test export is persistable"),
    );
    let bindings = published_exports_from_state(&state).build_remote_bindings();
    assert_eq!(
        bindings.get("account_id"),
        Some(&Value::Concrete(ConcreteValue::String(
            "123456789012".into()
        )))
    );
}

#[test]
fn published_exports_build_remote_bindings_keeps_secret_shaped_export_strings_literal() {
    let marker = valid_legacy_marker_shaped_string("not-an-export-secret");
    let literal = Value::Concrete(ConcreteValue::String(marker.clone().into()));

    let mut state = StateFile::new();
    state.exports.insert(
        "literal_marker".to_string(),
        PersistedExportValue::encode(&literal).expect("literal export is persistable"),
    );

    assert_eq!(
        published_exports_from_state(&state)
            .build_remote_bindings()
            .get("literal_marker"),
        Some(&Value::Concrete(ConcreteValue::String(marker.into()))),
    );
}

#[test]
fn published_exports_build_remote_bindings_empty_when_no_exports() {
    let state = StateFile::new();
    let bindings = published_exports_from_state(&state).build_remote_bindings();
    assert!(bindings.is_empty());
}

#[test]
fn published_null_export_is_absent_for_coalesce_resolution() {
    let mut state = StateFile::new();
    state.exports.insert(
        "accounts".to_string(),
        serde_json::from_value(serde_json::Value::Null)
            .expect("JSON null is a valid absent persisted export"),
    );

    let remote = published_exports_from_state(&state).build_remote_bindings();
    assert!(
        !remote.contains_key("accounts"),
        "JSON null must be omitted from a successfully loaded export map",
    );

    let mut bindings = carina_core::binding_index::ResolvedBindings::default();
    bindings.set(
        "up",
        remote,
        carina_core::binding_index::BindingValueSource::Upstream,
    );
    let default = Value::Concrete(ConcreteValue::List(Vec::new()));
    let expression = Value::Deferred(DeferredValue::Coalesce {
        value: Box::new(Value::resource_ref("up", "accounts", Vec::new())),
        default: Box::new(default.clone()),
    });

    assert_eq!(
        carina_core::resolver::resolve_ref_value_status(&expression, &bindings),
        Ok(carina_core::resolver::ValueResolution::Present(default)),
    );
}

#[test]
fn published_exports_build_remote_bindings_ignores_resource_bindings() {
    let mut state = StateFile::new();
    // Add a resource with a binding — should NOT appear in remote bindings
    let mut resource = ResourceState::try_applied("ec2.Vpc", "vpc_123", "awscc", "vpc-123")
        .expect("fixture identifier is non-empty")
        .with_attribute("vpc_id", serde_json::Value::String("vpc-123".to_string()));
    resource.binding = Some("vpc".to_string());
    state
        .upsert_resource(resource)
        .expect("test state setup must be valid");
    let bindings = published_exports_from_state(&state).build_remote_bindings();
    assert!(
        bindings.is_empty(),
        "resource bindings should not be exposed"
    );
}

#[test]
fn check_and_migrate_canonicalizes_legacy_map_key_addresses() {
    // State files written by older Carina builds embed the map key in
    // `binding["key"]` form. After #1903 the canonical address is the
    // dot form for identifier-safe keys; non-identifier-safe keys move
    // from double quotes to single. The `check_and_migrate` load path
    // rewrites these so existing state resolves against new emissions
    // without a `moved` block.
    let json = format!(
        r#"{{
            "version": {ver},
            "serial": 1,
            "lineage": "abc",
            "carina_version": "test",
            "resources": [
                {{
                    "resource_type": "test.Dependency",
                    "name": "other[\"a\"]",
                    "provider": "mock",
                    "identifier": "other-a",
                    "attributes": {{}},
                    "binding": "other[\"a\"]"
                }},
                {{
                    "resource_type": "test.Dependency",
                    "name": "_envs[\"prod-east\"]",
                    "provider": "mock",
                    "identifier": "prod-east",
                    "attributes": {{}},
                    "binding": "_envs[\"prod-east\"]"
                }},
                {{
                    "resource_type": "sso.Assignment",
                    "name": "_accounts[\"registry_prod\"]",
                    "provider": "awscc",
                    "identifier": "x",
                    "attributes": {{}},
                    "binding": "_accounts[\"registry_prod\"]",
                    "dependency_bindings": ["other[\"a\"]", "_envs[\"prod-east\"]"]
                }}
            ]
        }}"#,
        ver = 14,
    );
    let state = finish_legacy_dependency_migration(
        check_and_migrate(&json).expect("load state").into_state(),
    );
    let r = &state.resources[2];
    assert_eq!(r.identity.as_str(), "_accounts.registry_prod");
    assert_eq!(r.binding.as_deref(), Some("_accounts.registry_prod"));
    let deps: Vec<&str> = r
        .dependency_bindings
        .iter()
        .map(ResolvedResourceId::identity_str)
        .collect();
    assert!(deps.contains(&"other.a"));
    assert!(deps.contains(&"_envs['prod-east']"));
}

/// RFC #2371 #2385: state writeback rejects unresolved `Value` variants
/// surfaced from a buggy provider that returns a `Value::Deferred(DeferredValue::ResourceRef)`
/// in `state.attributes`. Provider-returned states must be concrete; a
/// resolver / provider bug produces a typed `UnresolvedResourceRef`
/// error rather than a debug-formatted string in state JSON.
#[test]
fn from_provider_state_rejects_resource_ref_in_provider_attributes() {
    use carina_core::resource::{
        AccessPath, DeferredValue, Resource, State as ProviderState, Value,
    };

    let resource = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None);
    let provider_state = ProviderState {
        id: resource.id.clone(),
        identifier: Some("my-bucket".to_string()),
        attributes: [(
            "owner".to_string(),
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::with_fields("net", "vpc", vec!["vpc_id".into()]),
            }),
        )]
        .into_iter()
        .collect(),
        exists: true,
        dependency_bindings: Default::default(),

        partial_read: None,
    };

    let err = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&resource),
        &provider_state,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap_err();
    let rendered = err.to_string();
    assert!(
        rendered.contains("unresolved reference") && rendered.contains("net.vpc.vpc_id"),
        "expected UnresolvedResourceRef diagnostic in error, got: {err}"
    );
}

#[test]
fn v5_state_read_converts_desired_keys_to_explicit_leaves() {
    // A v5 state file carries a `"desired_keys": ["..."]` array.
    // Reading it under v6 must (a) bump the version to 6 and
    // (b) lift each top-level key to a `Leaf` child of the root
    // `ExplicitFields::Struct`.
    // carina#3266: production v5 managed rows carry an identifier (the
    // provider returns one on every successful apply). Historical data-source
    // rows are pruned later, once the parser supplies their typed addresses;
    // migration itself must preserve this production-shaped managed row.
    let v5 = r#"{
        "version": 5,
        "serial": 0,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "s3.Bucket",
            "name": "my-bucket",
            "provider": "aws",
            "identifier": "my-bucket",
            "attributes": {"region": "ap-northeast-1"},
            "protected": false,
            "directives": {},
            "prefixes": {},
            "name_overrides": {},
            "desired_keys": ["region", "tags"],
            "binding": null,
            "dependency_bindings": []
        }]
    }"#;

    let state = finish_legacy_dependency_migration(
        check_and_migrate(v5)
            .expect("migration must succeed")
            .into_state(),
    );
    assert_eq!(state.version, StateFile::CURRENT_VERSION);
    assert_eq!(state.resources.len(), 1);

    let ExplicitFields::Struct { children } = &state.resources[0].explicit else {
        panic!(
            "v5 desired_keys should lift to ExplicitFields::Struct, got: {:?}",
            state.resources[0].explicit
        );
    };
    assert_eq!(children.len(), 2);
    assert!(matches!(children["region"], ExplicitFields::Leaf));
    assert!(matches!(children["tags"], ExplicitFields::Leaf));
}

#[test]
fn current_state_writes_and_reads_full_explicit_tree() {
    // A current-version state file with a nested `explicit` tree
    // round-trips through serde without loss.
    let mut state = StateFile::new();
    // carina#3266: current managed rows carry an identifier from the provider's
    // applied result. Identifier-less historical data-source artifacts are
    // removed only at the typed configuration/state boundary, not by serde.
    let mut rs = ResourceState::try_applied("s3.Bucket", "my-bucket", "aws", "my-bucket")
        .expect("fixture identifier is non-empty");
    rs.explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "lifecycle_configuration".into(),
            ExplicitFields::Struct {
                children: HashMap::from([(
                    "rules".into(),
                    ExplicitFields::List {
                        element: Box::new(ExplicitFields::Struct {
                            children: HashMap::from([("id".into(), ExplicitFields::Leaf)]),
                        }),
                    },
                )]),
            },
        )]),
    };
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let json = serde_json::to_string(&state).expect("serialize");
    let back = check_and_migrate(&json).expect("read").into_state();
    assert_eq!(back.version, StateFile::CURRENT_VERSION);
    assert_eq!(back.resources[0].explicit, state.resources[0].explicit);
}

#[test]
fn current_list_elements_state_round_trip_preserves_empty_and_populated_vectors() {
    let mut state = StateFile::new();
    let mut populated = ResourceState::try_applied("listener.Listener", "populated", "mock", "one")
        .expect("fixture identifier is non-empty");
    populated.explicit = ExplicitFields::Struct {
        children: HashMap::from([(
            "rules".to_string(),
            ExplicitFields::ListElements {
                elements: vec![ExplicitFields::Leaf, ExplicitFields::Unrecorded],
            },
        )]),
    };
    let mut empty = ResourceState::try_applied("listener.Listener", "empty", "mock", "two")
        .expect("fixture identifier is non-empty");
    empty.explicit = ExplicitFields::ListElements {
        elements: Vec::new(),
    };
    state.upsert_resource(populated).unwrap();
    state.upsert_resource(empty).unwrap();

    let json = serde_json::to_string(&state).unwrap();
    assert!(json.contains(r#""kind":"list-elements""#));
    assert!(json.contains(r#""elements":[]"#));
    let back = check_and_migrate(&json).unwrap().into_state();

    assert_eq!(back.version, StateFile::CURRENT_VERSION);
    assert_eq!(back.resources[0].explicit, state.resources[0].explicit);
    assert_eq!(back.resources[1].explicit, state.resources[1].explicit);
}

#[test]
fn v9_legacy_list_survives_current_lift_stays_conservative_and_self_heals_on_writeback() {
    use carina_core::differ::Diff;

    let json = r#"{
        "version": 9,
        "serial": 1,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "listener.Listener",
            "identity": "listener",
            "provider": "mock",
            "identifier": "listener-id",
            "attributes": {
                "rules": [
                    {"port": 80, "description": "web"},
                    {"port": 443, "description": "provider-default"}
                ]
            },
            "explicit": {
                "kind": "struct",
                "children": {
                    "rules": {
                        "kind": "list",
                        "element": {
                            "kind": "struct",
                            "children": {
                                "port": {"kind": "leaf"},
                                "description": {"kind": "leaf"}
                            }
                        }
                    }
                }
            }
        }]
    }"#;
    let outcome = check_and_migrate(json).expect("v9 state must lift without a custom migration");
    assert_eq!(outcome.migration.unwrap().from, 9);
    assert_eq!(outcome.state.version, 20);
    let state = finish_legacy_dependency_migration(outcome.state);
    let row = state
        .find_resource("mock", "listener.Listener", "listener")
        .expect("legacy row");
    let legacy_explicit = row.explicit.clone();
    let ExplicitFields::Struct { children } = &legacy_explicit else {
        panic!("expected legacy root Struct");
    };
    assert!(matches!(children["rules"], ExplicitFields::List { .. }));

    let serialized = serde_json::to_string(&state).unwrap();
    let round_tripped = check_and_migrate(&serialized).unwrap().into_state();
    assert_eq!(round_tripped.resources[0].explicit, legacy_explicit);

    let desired = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, None),
                list_rule(443, None),
            ])),
        );
    let current_attributes = HashMap::from([(
        "rules".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            list_rule(80, Some("web")),
            list_rule(443, Some("provider-default")),
        ])),
    )]);
    let current = State::existing(desired.id.clone(), current_attributes.clone())
        .with_identifier("listener-id");
    let persisted =
        PersistedAttributes::from_typed_persisted_wire_values(current_attributes.clone())
            .expect("fixture attributes are persistable");
    let saved = RawSavedAttrs::from_persisted(
        HashMap::from([(desired.id.clone(), persisted)]),
        HashMap::new(),
    )
    .lift(&carina_core::schema::SchemaRegistry::new());
    assert!(matches!(
        diff(
            &resolved(&desired),
            &current,
            &saved,
            Some(&legacy_explicit),
            None,
        ),
        Diff::NoChange(_)
    ));

    let accepted = AcceptedRequest::update_for_test(&["rules"], &[]);
    let rewritten = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&desired),
        &current,
        Some(row),
        None,
        RowBasis::Applied(&accepted),
        None,
    )
    .unwrap();
    let ExplicitFields::Struct { children } = rewritten.explicit else {
        panic!("expected rewritten root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("v10 writeback must replace legacy List with ListElements");
    };
    assert_eq!(elements.len(), 2);
    assert_explicit_struct_keys(&elements[0], &["port"]);
    assert_explicit_struct_keys(&elements[1], &["port"]);
}

#[test]
fn repeated_writeback_realigns_reordered_elements_and_plan_uses_first_row_alignment() {
    use carina_core::differ::Diff;

    let authored = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, Some("web")),
                list_rule(443, None),
            ])),
        );
    let first_provider = State::existing(
        authored.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, Some("web")),
                list_rule(443, Some("provider-default")),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let first_row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&authored),
        &first_provider,
        None,
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();
    let ExplicitFields::Struct {
        children: first_children,
    } = &first_row.explicit
    else {
        panic!("expected first root Struct");
    };
    let ExplicitFields::ListElements {
        elements: first_elements,
    } = &first_children["rules"]
    else {
        panic!("expected first aligned ListElements");
    };
    assert_explicit_struct_keys(&first_elements[0], &["port", "description"]);
    assert_explicit_struct_keys(&first_elements[1], &["port"]);

    let mut first_state = StateFile::new();
    first_state.upsert_resource(first_row.clone()).unwrap();
    let saved = first_state
        .build_saved_attrs()
        .lift(&carina_core::schema::SchemaRegistry::new());
    let removed = Resource::with_provider("mock", "listener.Listener", "listener", None)
        .with_attribute(
            "rules",
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, None),
                list_rule(443, None),
            ])),
        );
    let removal = diff(
        &resolved(&removed),
        &first_provider,
        &saved,
        Some(&first_row.explicit),
        None,
    );
    let Diff::Update {
        changed_attributes, ..
    } = removal
    else {
        panic!("removing the authored port 80 description must update rules");
    };
    assert_eq!(changed_attributes, vec!["rules".to_string()]);

    // This current view is exactly the effective merged list the plan must compare against: the
    // authored field is gone from port 80, while port 443 keeps its provider default. `NoChange`
    // here discriminates the saved index used for both per-element decisions.
    let expected_effective = State::existing(
        authored.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(80, None),
                list_rule(443, Some("provider-default")),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let Value::Concrete(ConcreteValue::List(expected_rules)) =
        &expected_effective.attributes["rules"]
    else {
        panic!("expected effective rules list");
    };
    let Value::Concrete(ConcreteValue::Map(port_80)) = &expected_rules[0] else {
        panic!("expected port 80 rule");
    };
    let Value::Concrete(ConcreteValue::Map(port_443)) = &expected_rules[1] else {
        panic!("expected port 443 rule");
    };
    assert!(!port_80.contains_key("description"));
    assert_eq!(
        port_443.get("description"),
        Some(&Value::Concrete(ConcreteValue::String(
            "provider-default".into()
        )))
    );
    assert!(matches!(
        diff(
            &resolved(&removed),
            &expected_effective,
            &saved,
            Some(&first_row.explicit),
            None,
        ),
        Diff::NoChange(_)
    ));

    let reordered_provider = State::existing(
        authored.id.clone(),
        HashMap::from([(
            "rules".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                list_rule(443, Some("provider-default")),
                list_rule(80, Some("web")),
            ])),
        )]),
    )
    .with_identifier("listener-id");
    let second_row = resource_state_from_provider_state_for_resource_and_schema_for_test(
        &resolved(&authored),
        &reordered_provider,
        Some(&first_row),
        None,
        RowBasis::Imported,
        None,
    )
    .unwrap();
    let stored_json = persisted_json_attribute(
        second_row
            .persisted_attributes()
            .expect("provider row has persisted attributes"),
        "rules",
    )
    .expect("stored reordered rules attribute");
    let stored = stored_json.as_array().expect("stored reordered rules");
    assert_eq!(stored[0]["port"], 443);
    assert_eq!(stored[1]["port"], 80);
    let ExplicitFields::Struct { children } = &second_row.explicit else {
        panic!("expected root Struct");
    };
    let ExplicitFields::ListElements { elements } = &children["rules"] else {
        panic!("expected realigned ListElements");
    };
    assert_explicit_struct_keys(&elements[0], &["port"]);
    assert_explicit_struct_keys(&elements[1], &["port", "description"]);
}

#[test]
fn build_explicit_yields_per_resource_trees() {
    let mut state = StateFile::new();
    let mut rs = ResourceState::new("s3.Bucket", "my-bucket", "aws");
    rs.explicit = ExplicitFields::Struct {
        children: HashMap::from([("region".into(), ExplicitFields::Leaf)]),
    };
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let map = state.build_explicit();
    let id = ResourceId::with_provider_identity("aws", "s3.Bucket", "my-bucket", None);
    assert!(map.contains_key(&id));
    assert!(matches!(map[&id], ExplicitFields::Struct { .. }));
}

#[test]
fn build_directives_keys_include_provider_instance() {
    // ResourceState carries `directives.provider_instance` through serde
    // (Phase 3a). The state→ResourceId reconstruction in build_directives
    // must propagate that binding name into the ResourceId so map lookups
    // by `ResourceId` match the freshly-parsed key. Without this, two
    // resources with the same kind/type/name but different instances
    // collide in `HashMap<ResourceId, _>` (Phase 3b-2a).
    use carina_core::resource::ResourceId;

    let mut state = StateFile::new();
    let mut rs = ResourceState::new("s3.Bucket", "shared-name", "aws");
    rs.directives.provider_instance = Some("us".to_string());
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let map = state.build_directives();
    let expected = ResourceId::with_provider_identity(
        "aws",
        "s3.Bucket",
        "shared-name",
        Some("us".to_string()),
    );
    assert!(
        map.contains_key(&expected),
        "build_directives must construct ResourceId with provider_instance, got keys: {:?}",
        map.keys().collect::<Vec<_>>()
    );
    // Without the instance, the legacy ResourceId must NOT match.
    let legacy = ResourceId::with_provider_identity("aws", "s3.Bucket", "shared-name", None);
    assert!(
        !map.contains_key(&legacy),
        "ResourceId without provider_instance must not match a Some(_) entry"
    );
}

#[test]
fn partial_read_marker_round_trips_through_state_json() {
    let mut state = StateFile::new();
    let mut rs = ResourceState::try_applied("test.resource", "r1", "mock", "mock-id")
        .expect("fixture identifier is non-empty");
    rs.set_partial_read_for_test(PartialReadMarker::new(
        PendingReRead::incomplete("mock partial create"),
        ["a".to_string(), "b".to_string()].into_iter().collect(),
    ));
    state
        .upsert_resource(rs)
        .expect("test state setup must be valid");

    let json = serde_json::to_string(&state).expect("serialize");
    let back = check_and_migrate(&json).expect("read").into_state();

    assert_eq!(
        back.resources[0].partial_read(),
        state.resources[0].partial_read()
    );
}

#[test]
fn missing_partial_read_field_defaults_to_none() {
    let json = r#"{
        "version": 7,
        "serial": 0,
        "lineage": "test-lineage",
        "carina_version": "0.4.0",
        "resources": [{
            "resource_type": "test.resource",
            "name": "r1",
            "provider": "mock",
            "identifier": "mock-id",
            "attributes": {"name": "r1"},
            "protected": false,
            "directives": {},
            "prefixes": {},
            "name_overrides": {},
            "desired_keys": [],
            "explicit": {"kind": "struct", "children": {}},
            "binding": null,
            "dependency_bindings": [],
            "write_only_attributes": []
        }]
    }"#;

    let state = check_and_migrate(json).expect("read").into_state();

    assert_eq!(state.resources.len(), 1);
    assert_eq!(state.resources[0].partial_read(), None);
}
