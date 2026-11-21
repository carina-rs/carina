//! carina#4479 projection contract; projection and compatibility regression coverage.
use super::*;

fn authority_document() -> serde_json::Value {
    serde_json::json!({
        "version": crate::state_document::CURRENT_STATE_VERSION,
        "exports_version": crate::state_document::CURRENT_EXPORTS_VERSION,
        "serial": 0,
        "lineage": "test",
        "carina_version": "test",
        "exports": {},
        "resources": [{
            "provider": "test",
            "resource_type": "Item",
            "identity": "item",
            "current": {
                "kind": "applied",
                "identifier": "item-id",
                "attributes": {},
                "nested_write_only": {},
            },
        }],
    })
}

#[test]
fn observed_secret_state_authority_uses_state_version_gate() {
    use crate::state_document::StateDocumentDecodeError;
    let document = authority_document();
    assert!(ObservationState::read_state_file(&serde_json::to_vec(&document).unwrap()).is_ok());
    let mut future = document.clone();
    future["version"] = serde_json::json!(crate::state_document::CURRENT_STATE_VERSION + 1);
    assert!(matches!(
        ObservationState::read_state_file(&serde_json::to_vec(&future).unwrap()),
        Err(StateDocumentDecodeError::StateVersionTooNew { .. })
    ));
    let mut future_exports = document;
    future_exports["exports_version"] =
        serde_json::json!(crate::state_document::CURRENT_EXPORTS_VERSION + 1);
    assert!(matches!(
        ObservationState::read_state_file(&serde_json::to_vec(&future_exports).unwrap()),
        Err(StateDocumentDecodeError::ExportsVersionTooNew { .. })
    ));
}

#[test]
fn observed_secret_state_authority_decodes_whole_document() {
    let mut incomplete = authority_document();
    incomplete.as_object_mut().unwrap().remove("serial");
    let mut invalid_generation = authority_document();
    invalid_generation["resources"][0]["deposed"] = serde_json::json!([{
        "key": "old",
        "identifier": "old-id",
        "attributes": {"payload": {"$secret": "invalid"}},
    }]);
    let mut invalid_export = authority_document();
    invalid_export["exports"] = serde_json::json!({"payload": {"$secret": "invalid"}});
    let mut invalid_continuation = authority_document();
    invalid_continuation["resources"][0]["current"] =
        serde_json::json!({"kind": "create_in_flight"});
    for document in [
        incomplete,
        invalid_generation,
        invalid_export,
        invalid_continuation,
    ] {
        assert!(
            ObservationState::read_state_file(&serde_json::to_vec(&document).unwrap()).is_err(),
            "partial or invalid state must not mint authority: {document}",
        );
    }
}

#[test]
fn observed_secret_state_authority_rejects_duplicate_empty_lifecycle_rows() {
    let mut document = authority_document();
    document["resources"][0]["current"] = serde_json::json!({"kind": "none"});
    let duplicate = document["resources"][0].clone();
    document["resources"]
        .as_array_mut()
        .unwrap()
        .push(duplicate);
    assert!(matches!(
        ObservationState::read_state_file(&serde_json::to_vec(&document).unwrap()),
        Err(crate::state_document::StateDocumentDecodeError::Validation(
            _
        ))
    ));
}

#[test]
fn observed_secret_duplicate_views_share_hashes_and_preserve_distinct_views() {
    use crate::effect::Effect;
    use crate::resource::{Current, ResolvedResource, Resource, SnapshotState};
    let desired = ResolvedResource::new(
        Resource::new("test.Item", "item")
            .with_attribute("payload", map([("original", secret())]))
            .into_value_snapshot(),
    );
    let recorded_value = list(vec![string("recorded")]);
    let recorded = SnapshotState::existing(
        desired.id.clone(),
        HashMap::from([("payload".into(), recorded_value.to_snapshot())]),
    );
    let mut plan = crate::plan::Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("item-id"),
        from: Box::new(recorded),
        to: desired.clone(),
        changed_attributes: vec!["payload".into()],
    });
    for refreshed_value in [recorded_value.clone(), list(vec![string("refreshed")])] {
        let refreshed = SnapshotState::existing(
            desired.id.clone(),
            HashMap::from([("payload".into(), refreshed_value.to_snapshot())]),
        )
        .with_identifier("item-id");
        let observations = ObservationState::empty_for_test()
            .protect(ObservationInputs::Display {
                plan: &plan,
                current: &HashMap::from([(desired.id.clone(), Current::from_snapshot(refreshed))]),
                deleted: &HashMap::new(),
            })
            .unwrap();
        let current = observations
            .managed_for(&desired.id)
            .unwrap()
            .snapshot()
            .unwrap();
        let old = observations
            .update_from(&desired.id)
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(current.identifier.as_deref(), Some("item-id"));
        assert_eq!(old.identifier, None);
        assert!(
            old.attributes["payload"]
                .verify_secret(&recorded_value)
                .unwrap()
        );
        assert!(
            current.attributes["payload"]
                .verify_secret(&refreshed_value)
                .unwrap()
        );
        if recorded_value == refreshed_value {
            assert_eq!(old.attributes, current.attributes);
        } else {
            assert_ne!(old.attributes, current.attributes);
        }
    }
}

fn string(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}
fn secret() -> Value {
    string("authored").into_secret()
}
fn map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Concrete(ConcreteValue::Map(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    ))
}
fn list(values: Vec<Value>) -> Value {
    Value::Concrete(ConcreteValue::List(values))
}
fn assert_whole(observed: &Value, shape: &Value) {
    let masked = SecretProtection::for_output(shape).mask(observed);
    assert!(
        matches!(masked, Value::Deferred(DeferredValue::Secret(_))),
        "display leaked: {masked:?}"
    );
    let saved = redact_plaintext_using_secret_shape(observed, shape).unwrap();
    assert!(saved.secret_hash().is_some(), "snapshot leaked: {saved:?}");
    assert!(
        saved.verify_secret(observed).unwrap(),
        "whole digest must verify the observed subtree"
    );
    assert!(!saved.verify_secret(&string("different subtree")).unwrap());
}

// RED: both kind directions, scalar/container, union-shaped alternatives.
#[test]
fn observed_secret_kind_divergence_masks_whole_subtree() {
    for (observed, shape) in [
        (
            list(vec![string("observed")]),
            map([("original", secret())]),
        ),
        (
            map([("observed-key", string("observed"))]),
            list(vec![secret()]),
        ),
        (string("observed"), map([("original", secret())])),
        (
            Value::Concrete(ConcreteValue::Int(42)),
            list(vec![secret()]),
        ),
        (map([("public", string("observed"))]), secret()),
        (list(vec![string("observed")]), secret()),
    ] {
        assert_whole(&observed, &shape);
    }
}
// PIN: an absent secret-bearing key has no observed value to protect.
#[test]
fn observed_secret_missing_secret_key_keeps_siblings_visible() {
    let observed = map([("env", string("prod")), ("extra", string("public"))]);
    let shape = map([("k", secret()), ("env", string("prod"))]);
    assert_eq!(
        SecretProtection::for_output(&shape).mask(&observed),
        observed,
    );
    assert_eq!(
        redact_plaintext_using_secret_shape(&observed, &shape).unwrap(),
        observed.to_snapshot(),
    );
}
// PIN: missing public keys and extra observed keys leave public siblings visible.
#[test]
fn observed_secret_map_public_key_changes_keep_leaf_protection() {
    for observed in [
        map([("k", string("observed"))]),
        map([
            ("k", string("observed")),
            ("env", string("prod")),
            ("extra", string("public")),
        ]),
    ] {
        let shape = map([("k", secret()), ("env", string("prod"))]);
        let saved = redact_plaintext_using_secret_shape(&observed, &shape).unwrap();
        let Value::Concrete(ConcreteValue::Map(values)) = saved else {
            panic!("must retain map")
        };
        assert!(values["k"].verify_secret(&string("observed")).unwrap());
        if let Some(value) = values.get("extra") {
            assert_eq!(value, &string("public").to_snapshot());
        }
    }
}
// RED: shorter/longer secret-bearing lists promote, rather than guess positions.
#[test]
fn observed_secret_list_length_divergence_masks_whole_list() {
    for observed in [
        list(vec![]),
        list(vec![string("observed"), string("public"), string("extra")]),
    ] {
        assert_whole(&observed, &list(vec![secret(), string("public")]));
    }
}
// RED: promotion stops at the first divergent child, preserving outer siblings.
#[test]
fn observed_secret_deep_divergence_preserves_public_siblings() {
    let observed = map([
        ("inner", list(vec![string("observed")])),
        ("env", string("prod")),
    ]);
    let shape = map([
        ("inner", map([("original", secret())])),
        ("env", string("prod")),
    ]);
    let saved = redact_plaintext_using_secret_shape(&observed, &shape).unwrap();
    let Value::Concrete(ConcreteValue::Map(values)) = saved else {
        panic!("must retain outer map")
    };
    assert!(
        values["inner"]
            .verify_secret(&list(vec![string("observed")]))
            .unwrap()
    );
    assert_eq!(values["env"], string("prod").to_snapshot());
}
// RED snapshot / PIN display: normalize StringList and List before matching.
#[test]
fn observed_secret_string_list_equivalence_keeps_matching_leaf() {
    let observed: Value = Value::Concrete(ConcreteValue::StringList(vec![
        "observed".into(),
        "public".into(),
    ]));
    let shape = list(vec![secret(), string("public")]);
    let saved = redact_plaintext_using_secret_shape(&observed, &shape).unwrap();
    let Value::Concrete(ConcreteValue::List(values)) = saved else {
        panic!("must materialize protected leaf")
    };
    assert!(values[0].verify_secret(&string("observed")).unwrap());
    assert_eq!(values[1], string("public").to_snapshot());
}
// RED: do not reuse the descendant hash for a promoted, different subtree (A8).
#[test]
fn observed_secret_promoted_hash_is_fresh_and_covers_exact_subtree() {
    let shape = redact_secrets_only(&map([("original", secret())])).unwrap();
    let observed = list(vec![string("observed")]).to_snapshot();
    let first = redact_using_redacted_shape(&observed, &shape).unwrap();
    let second = redact_using_redacted_shape(&observed, &shape).unwrap();
    assert!(
        first
            .verify_secret(&list(vec![string("observed")]))
            .unwrap()
    );
    assert_ne!(first.secret_hash(), second.secret_hash());
    assert!(
        !first
            .verify_secret(&list(vec![string("different")]))
            .unwrap()
    );
}
// PIN: an absent protected attribute remains absent; no phantom hash is created.
#[test]
fn observed_secret_absent_attribute_stays_absent() {
    let state = crate::resource::SnapshotState::existing(
        crate::resource::ResourceId::with_identity("test", "item"),
        HashMap::new(),
    );
    let desired =
        crate::resource::Resource::new("test", "item").with_attribute("payload", secret());
    let mut plan = crate::plan::Plan::new();
    plan.retain_observation_desired([(&desired.id, &desired.attributes)]);
    let observations = ObservationState::empty_for_test()
        .protect(ObservationInputs::Plan {
            deleted: &std::collections::HashMap::new(),
            plan: &plan,
            current: &HashMap::from([(
                state.id.clone(),
                crate::resource::Current::from_snapshot(state.clone()),
            )]),
            reads: &crate::data_source_value::DataSourceValues::empty(),
        })
        .unwrap();
    assert!(
        observations.managed()[&state.id]
            .snapshot()
            .unwrap()
            .attributes
            .is_empty()
    );
}

// R5 PIN: canonical hashing already strips persisted inner Secret wrappers.
#[test]
fn observed_secret_promoted_whole_hash_strips_persisted_inner_wrappers() {
    use crate::provider::PersistedAttributes;
    use crate::resource::{ResolvedResource, SnapshotState};
    let stored = PersistedValue::encode_provider_value(&list(vec![secret()]), None).unwrap();
    let persisted =
        PersistedAttributes::from_persisted_values(HashMap::from([("payload".into(), stored)]))
            .unwrap();
    let observed = list(vec![string("different-observed")]);
    for shape in [map([("original", secret())]), secret()] {
        let desired = ResolvedResource::new(
            crate::resource::Resource::new("test.Item", "item")
                .with_attribute("payload", shape)
                .into_value_snapshot(),
        );
        let state = SnapshotState::existing(
            desired.id.clone(),
            HashMap::from([("payload".into(), observed.to_snapshot())]),
        );
        let snapshot = ProtectedObservation::for_resource(&state, &desired, Some(&persisted))
            .unwrap()
            .snapshot()
            .unwrap();
        assert!(
            snapshot.attributes["payload"]
                .verify_secret(&observed)
                .unwrap(),
            "the digest must verify against the plain observed list"
        );
    }
}

// R4 RED: malformed Update history must return an error, without panicking.
#[test]
fn observed_secret_update_identity_mismatch_returns_error() {
    let desired = crate::resource::ResolvedResource::new(
        crate::resource::Resource::new("test.Item", "new").into_value_snapshot(),
    );
    let mut plan = crate::plan::Plan::new();
    plan.add(crate::effect::Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("item"),
        from: Box::new(crate::resource::SnapshotState::existing(
            crate::resource::ResourceId::with_identity("test.Item", "old"),
            HashMap::new(),
        )),
        to: desired,
        changed_attributes: Vec::new(),
    });
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        redact_secrets_in_plan_with_desired_and_historical::<()>(
            &plan,
            &HashMap::new(),
            &HashMap::new(),
        )
    }));
    assert!(result.is_ok(), "identity mismatch must be a typed error");
    assert!(result.unwrap().is_err());
}

// R2 PIN on the previous behavior; RED on #4479's use of output policy for live bindings.
#[test]
fn binding_persisted_projection_preserves_legacy_correspondence() {
    use crate::provider::PersistedAttributes;
    for (shape, observed) in [
        (
            map([("original", secret())]),
            list(vec![string("public-kind-divergence")]),
        ),
        (
            list(vec![secret(), string("b")]),
            list(vec![string("observed-a"), string("b"), string("extra")]),
        ),
    ] {
        let persisted = PersistedAttributes::from_persisted_values(HashMap::from([(
            "l".into(),
            PersistedValue::encode_provider_value(&shape, None).unwrap(),
        )]))
        .unwrap();
        let masked = project_persisted_binding_attribute(&observed, &persisted, "l");
        match shape {
            Value::Concrete(ConcreteValue::List(_)) => {
                let Value::Concrete(ConcreteValue::List(values)) = masked else {
                    panic!("binding lists keep positional protection after growth")
                };
                assert!(contains_secret(&values[0]));
                assert_eq!(values[1], string("b"));
                assert_eq!(values[2], string("extra"));
            }
            _ => assert_eq!(
                masked, observed,
                "persisted binding kind mismatch stays ordinary"
            ),
        }
    }
}

// S5 RED: identity errors name both non-secret resource identities.
#[test]
fn observed_secret_identity_error_names_expected_and_actual() {
    let desired = crate::resource::ResolvedResource::new(
        crate::resource::Resource::new("test.Item", "expected").with_dependency_phase(()),
    );
    let raw = crate::resource::SnapshotState::existing(
        crate::resource::ResourceId::with_identity("test.Item", "actual"),
        HashMap::new(),
    );
    let error = ProtectedObservation::for_resource(&raw, &desired, None).unwrap_err();
    let message = error.to_string();
    assert!(message.contains(&desired.id.to_string()), "{message}");
    assert!(message.contains(&raw.id.to_string()), "{message}");
}

#[test]
fn observed_secret_comparison_products_are_named_and_missing_attributes_are_typed() {
    let desired = crate::resource::ResolvedResource::new(
        crate::resource::Resource::new("test.Item", "item")
            .with_attribute("payload", secret())
            .into_value_snapshot(),
    );
    let state = crate::resource::SnapshotState::existing(
        desired.id.clone(),
        HashMap::from([("payload".into(), string("observed").to_snapshot())]),
    );
    let mut plan = crate::plan::Plan::new();
    plan.add(crate::effect::Effect::Update {
        from: Box::new(state.clone()),
        to: desired,
        identifier: crate::effect::fixture_applied_identifier("item"),
        changed_attributes: vec!["payload".into()],
    });
    let observations = ObservationState::empty_for_test()
        .protect(ObservationInputs::SavedApply {
            plan: &plan,
            history: &HashMap::new(),
            planned: &state,
            actual: &state,
        })
        .unwrap();
    let pair = observations.comparisons().unwrap();
    assert_eq!(
        pair.planned
            .rendered_attribute("payload")
            .unwrap()
            .to_string(),
        "(secret)"
    );
    assert_eq!(
        pair.actual
            .rendered_attribute("payload")
            .unwrap()
            .to_string(),
        "(secret)"
    );
    assert!(matches!(
        pair.actual.rendered_attribute("absent"),
        Err(SerializationError::MissingObservationAttribute { .. })
    ));
}

#[test]
fn observed_secret_recording_rejects_a_batch_owned_by_another_plan() {
    let checked = crate::input_rules::finalize_plan_for_test(crate::plan::Plan::new())
        .check_inputs(&crate::schema::SchemaRegistry::new(), &HashMap::new())
        .unwrap();
    let other = crate::plan::Plan::new();
    let observations = ObservationState::empty_for_test()
        .protect(ObservationInputs::Display {
            deleted: &std::collections::HashMap::new(),
            plan: &other,
            current: &HashMap::new(),
        })
        .unwrap();
    let result = checked.record_snapshot(
        &HashMap::<
            crate::resource::ResourceId,
            crate::resource::Resource<crate::resource::ModuleSource, crate::resource::SnapshotLeaf>,
        >::new(),
        &observations,
    );
    assert!(matches!(
        result,
        Err(SerializationError::ObservationPlanMismatch)
    ));
}

#[test]
fn observed_secret_authored_inputs_survive_public_effect_substitution() {
    use crate::resource::{Current, ResolvedResource, Resource, SnapshotState};
    let authored =
        Resource::new("test", "item").with_attribute("payload", map([("original", secret())]));
    let public =
        Resource::new("test", "item").with_attribute("payload", list(vec![string("observed")]));
    let mut plan = crate::plan::Plan::new();
    plan.retain_observation_desired([(&authored.id, &authored.attributes)]);
    plan.add(crate::effect::Effect::Create(ResolvedResource::new(
        public.into_resolved_dependency_phase_for_test(),
    )));
    let state = SnapshotState::existing(
        authored.id.clone(),
        HashMap::from([(
            "payload".into(),
            list(vec![string("observed")]).to_snapshot(),
        )]),
    );
    let observations = ObservationState::empty_for_test()
        .protect(ObservationInputs::Display {
            deleted: &std::collections::HashMap::new(),
            plan: &plan,
            current: &HashMap::from([(state.id.clone(), Current::from_snapshot(state))]),
        })
        .unwrap();
    assert!(
        observations.managed()[&authored.id]
            .snapshot()
            .unwrap()
            .attributes["payload"]
            .verify_secret(&list(vec![string("observed")]))
            .unwrap()
    );
}

#[test]
fn for_value_keys_observed_secret_knowledge_plan_apply() {
    use crate::binding_index::{
        BindingCompleteness, LivePreApplyInputs, ResolvedBindings, SecretPreservingAttributes,
    };
    use crate::resource::{Current, RelativeBindingName, Resource, RootScope, SnapshotState};
    let authored = list(vec![secret()]);
    let observed = list(vec![string("OBSERVED_KEY_CANARY"), string("public-extra")]);
    let resource = Resource::new("test.Item", "item")
        .with_root_binding("item")
        .with_attribute("payload", authored.clone());
    let current = SnapshotState::existing(
        resource.id.clone(),
        HashMap::from([("payload".into(), observed.to_snapshot())]),
    )
    .with_identifier("item-id");
    let mut document = authority_document();
    document["resources"][0]["current"]["attributes"]["payload"] = serde_json::to_value(
        crate::value::PersistedValue::encode_provider_value(&authored, None).unwrap(),
    )
    .unwrap();
    let authority =
        ObservationState::read_state_file(&serde_json::to_vec(&document).unwrap()).unwrap();
    let mut plan = crate::plan::Plan::new();
    plan.add(crate::effect::Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("item-id"),
        from: Box::new(current.clone()),
        to: crate::resource::ResolvedResource::new(resource.clone().into_value_snapshot()),
        changed_attributes: vec![],
    });
    let currents = HashMap::from([(resource.id.clone(), Current::from_snapshot(current.clone()))]);
    let protected = authority
        .protect(ObservationInputs::Display {
            plan: &plan,
            current: &currents,
            deleted: &HashMap::new(),
        })
        .unwrap();
    assert!(
        protected
            .managed_for(&resource.id)
            .unwrap()
            .snapshot()
            .unwrap()
            .attributes["payload"]
            .verify_secret(&observed)
            .unwrap(),
        "output policy protects the whole grown list"
    );
    let schemas = crate::schema::SchemaRegistry::new();
    let managed = [resource.clone()];
    let current_inputs = crate::resource::into_plan_input_current_map(currents, &schemas, &managed);
    let planned = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &managed,
        compositions: &[],
        managed_current: &current_inputs,
        data_source_values: &crate::data_source_value::DataSourceValues::empty(),
        remote_bindings: &Default::default(),
        wait_aliases: &[],
    });
    let provider_values = HashMap::from([("payload".into(), observed.clone())]);
    let protected_attributes =
        SecretPreservingAttributes::from_provider_values(&resource.attributes, &provider_values);
    let mut applied = ResolvedBindings::default();
    applied.record_applied(
        Some("item"),
        &protected_attributes,
        &current,
        BindingCompleteness::Complete,
    );
    let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for (is_apply, bindings) in [(false, planned), (true, applied)] {
        let reference = Value::root_resource_ref("item", "payload", vec![]);
        let resolved =
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                crate::resolver::resolve_ref_value_status(&reference, &bindings, session)
            })
            .unwrap();
        let value = match resolved {
            crate::resolver::ValueResolution::Present(value)
            | crate::resolver::ValueResolution::Unknown(value) => value,
            crate::resolver::ValueResolution::Absent(_) => panic!("binding must contain payload"),
        };
        let classification = if is_apply {
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::apply(
                    &bindings,
                ))
                .unwrap()
                .into_classification()
        } else {
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                    &bindings,
                ))
                .unwrap()
                .into_classification()
        };
        let members = crate::resource::knowledge::ClassifiedRef::from(&classification);
        assert!(
            !members.is_protected(),
            "binding policy retains positional member protection, independently of whole output protection"
        );
        assert!(
            members.list().unwrap()[0].is_protected(),
            "classification must retain secret member evidence"
        );
        let target = crate::parser::BoundDeferredTarget::new(
            RootScope::new().resolve(RelativeBindingName::new("rs")),
        )
        .unwrap();
        let error = crate::resource::admit_bound_for_addresses(
            crate::parser::LoopAddressing::ValueKeyed,
            &target,
            members,
            if is_apply {
                crate::resource::ForAddressMoment::Apply
            } else {
                crate::resource::ForAddressMoment::Plan
            },
        )
        .unwrap();
        let crate::resource::ForAddressAdmission::Reject(error) = error else {
            panic!("protected keys must be rejected")
        };
        assert_eq!(error, crate::resource::ForKeyError::Protected);
        assert!(!format!("{error:?} {error}").contains("OBSERVED_KEY_CANARY"));
    }
}
