use carina_core::resource::{ConcreteValue, DeferredValue, SnapshotValue, Value};

fn root_resource_ref(
    binding: &str,
    attribute: &str,
    segments: Vec<carina_core::resource::PathSegment>,
) -> Value {
    Value::Deferred(DeferredValue::ResourceRef {
        path: carina_core::resource::AccessPath::with_segments(binding, attribute, segments)
            .resolve_root_for_test(),
    })
}

fn hash_wire() -> serde_json::Value {
    serde_json::to_value(
        carina_core::value::redact_secrets_in_value(&Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String("password".into()))).into(),
        )))
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn snapshot_root_hash_is_unavailable_to_evaluation() {
    let snapshot: SnapshotValue = serde_json::from_value(hash_wire()).unwrap();
    let error = snapshot.to_value_at("state.password").unwrap_err();
    assert_eq!(error.path(), "state.password");
    assert_eq!(
        error.to_string(),
        "secret value unavailable at state.password"
    );
}

#[test]
fn snapshot_nested_hash_is_unavailable_to_evaluation() {
    let snapshot: SnapshotValue = serde_json::from_value(serde_json::json!({
        "Map": {"credentials": {"List": [{"Map": {"password": hash_wire()}}]}}
    }))
    .unwrap();
    let error = snapshot.to_value_at("state").unwrap_err();
    assert_eq!(error.path(), "state.credentials[0].password");
}

#[test]
fn snapshot_ordinary_sibling_remains_available() {
    let snapshot: SnapshotValue = serde_json::from_value(serde_json::json!({
        "Map": {"password": hash_wire(), "name": {"String": "ordinary"}}
    }))
    .unwrap();
    let ordinary = snapshot
        .get("name")
        .unwrap()
        .to_value_at("state.name")
        .unwrap();
    assert_eq!(
        ordinary,
        Value::Concrete(ConcreteValue::String("ordinary".into()))
    );
    assert!(
        snapshot
            .get("password")
            .unwrap()
            .to_value_at("state.password")
            .is_err()
    );
}

#[test]
fn snapshot_root_and_nested_wire_bytes_round_trip() {
    for wire in [
        hash_wire(),
        serde_json::json!({"Map": {"nested": {"List": [hash_wire()]}}}),
    ] {
        let bytes = serde_json::to_vec(&wire).unwrap();
        let snapshot: SnapshotValue = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&snapshot).unwrap(), bytes);
        assert!(serde_json::from_slice::<Value>(&bytes).is_err());
    }
}

#[test]
fn snapshot_persisted_protocol_root_and_nested_hash_round_trip() {
    use carina_core::protocol_convert::{
        protocol_to_core_persisted_value, try_core_to_protocol_persisted_value,
    };
    use carina_core::value::PersistedValue;
    let root: SnapshotValue = serde_json::from_value(hash_wire()).unwrap();
    let nested = SnapshotValue::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
        "nested".into(),
        SnapshotValue::Concrete(ConcreteValue::List(vec![root.clone()])),
    )])));
    for snapshot in [root, nested] {
        let before = PersistedValue::encode_provider_value(&snapshot, None).unwrap();
        let protocol = try_core_to_protocol_persisted_value(&snapshot).unwrap();
        let decoded = protocol_to_core_persisted_value(&protocol).unwrap();
        let after = PersistedValue::encode_provider_value(&decoded, None).unwrap();
        assert_eq!(
            serde_json::to_vec(&before).unwrap(),
            serde_json::to_vec(&after).unwrap()
        );
    }
}

#[test]
fn validated_export_decode_returns_evaluable_values() {
    let value = Value::Concrete(ConcreteValue::String("exported".into()));
    let persisted = carina_core::value::PersistedExportValue::encode(&value).unwrap();
    let decoded: Value = persisted.decode().unwrap();
    assert_eq!(decoded, value);
}

#[test]
fn snapshot_saved_plan_root_and_nested_hash_round_trip() {
    use carina_core::effect::Effect;
    use carina_core::plan::{Plan, SavedPlan, UnresolvedSavedPlan};
    use carina_core::resource::{ResolvedResource, Resource, SavedModuleSource};
    let secret = Value::Deferred(DeferredValue::Secret(
        Box::new(Value::Concrete(ConcreteValue::String(
            "plan-password".into(),
        )))
        .into(),
    ));
    for value in [
        secret.clone(),
        Value::Concrete(ConcreteValue::List(vec![secret])),
    ] {
        let resource = Resource::new("mock.Widget", "example").with_attribute("password", value);
        let mut plan = Plan::new();
        plan.add(Effect::Create(ResolvedResource::new(
            resource.into_value_snapshot(),
        )));
        let saved: SavedPlan = carina_core::value::redact_secrets_in_plan(&plan)
            .unwrap()
            .into();
        let bytes = serde_json::to_vec(&saved).unwrap();
        let decoded = serde_json::from_slice::<UnresolvedSavedPlan>(&bytes)
            .unwrap()
            .resolve_with_schemas(&carina_core::schema::SchemaRegistry::new())
            .unwrap();
        assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
        assert!(
            serde_json::from_value::<Effect<SavedModuleSource>>(
                serde_json::to_value(&decoded.effects()[0]).unwrap()
            )
            .is_err()
        );
    }
}

fn historical_siblings() -> SnapshotValue {
    let desired = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
        (
            "password".into(),
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String("password".into()))).into(),
            )),
        ),
        (
            "name".into(),
            Value::Concrete(ConcreteValue::String("ordinary".into())),
        ),
        (
            "items".into(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("one".into()),
            )])),
        ),
    ])));
    let redacted = carina_core::value::redact_secrets_in_value(&desired).unwrap();
    carina_core::value::PersistedValue::encode_provider_value(&redacted, None)
        .unwrap()
        .decode()
        .unwrap()
}

fn historical_bindings() -> carina_core::binding_index::ResolvedBindings {
    use carina_core::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use carina_core::resource::{
        Current, Resource, SnapshotState, absolute_binding_address_for_test,
    };
    use std::collections::HashMap;

    let mut resource = Resource::new("mock.Source", "state");
    resource.binding = Some(absolute_binding_address_for_test("state"));
    let state = SnapshotState::existing(
        resource.id.clone(),
        HashMap::from([("config".into(), historical_siblings())]),
    )
    .with_identifier("source-id");
    let schemas = carina_core::schema::SchemaRegistry::new();
    let current = carina_core::resource::into_plan_input_current_map(
        HashMap::from([(resource.id.clone(), Current::from_snapshot(state))]),
        &schemas,
        std::slice::from_ref(&resource),
    );
    ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &[resource],
        compositions: &[],
        managed_current: &current,
        data_source_values: &carina_core::data_source_value::DataSourceValues::empty(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    })
}

fn resolve_historical_field(field: &str) -> Result<Value, carina_core::resolver::ResolutionError> {
    let expression = root_resource_ref(
        "state",
        "config",
        vec![carina_core::resource::PathSegment::Field { name: field.into() }],
    );
    carina_core::validation::deferred_types::with_session_for_test(
        &carina_core::parser::LoadedProviderContext::empty_for_test(),
        |session| {
            carina_core::resolver::resolve_ref_value(&expression, &historical_bindings(), session)
        },
    )
}

#[test]
fn snapshot_binding_selects_ordinary_attribute_with_hashed_sibling() {
    assert_eq!(
        resolve_historical_field("name").unwrap(),
        Value::Concrete(ConcreteValue::String("ordinary".into()))
    );
}

#[test]
fn snapshot_binding_rejects_selected_hash_with_typed_error() {
    let error = resolve_historical_field("password").unwrap_err();
    let carina_core::resolver::ResolutionError::UnavailableSecret(error) = error else {
        panic!("expected a typed unavailable-secret error");
    };
    assert_eq!(error.path(), "state.config.password");
}

#[test]
fn snapshot_binding_hash_does_not_become_plan_unknown() {
    let expression = root_resource_ref(
        "state",
        "config",
        vec![carina_core::resource::PathSegment::Field {
            name: "password".into(),
        }],
    );
    let result = carina_core::validation::deferred_types::with_session_for_test(
        &carina_core::parser::LoadedProviderContext::empty_for_test(),
        |session| {
            carina_core::resolver::resolve_value_for_plan(
                &expression,
                &historical_bindings(),
                &Default::default(),
                session,
            )
        },
    );
    assert!(
        matches!(result, Err(carina_core::resolver::ResolutionError::UnavailableSecret(error))
        if error.path() == "state.config.password")
    );
}

#[test]
fn snapshot_bulk_resolvers_keep_unavailable_secret_typed() {
    use carina_core::resolver::{
        ResolutionError, resolve_refs_for_plan, resolve_refs_with_state_and_remote,
    };
    use carina_core::resource::Resource;

    for plan in [false, true] {
        let expression = root_resource_ref(
            "state",
            "config",
            vec![carina_core::resource::PathSegment::Field {
                name: "password".into(),
            }],
        );
        let mut resources =
            [Resource::new("mock.Consumer", "consumer").with_attribute("password", expression)];
        let bindings = historical_bindings();
        let error = carina_core::validation::deferred_types::with_session_for_test(
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                if plan {
                    resolve_refs_for_plan(&mut resources, &bindings, &Default::default(), session)
                } else {
                    resolve_refs_with_state_and_remote(&mut resources, &bindings, session)
                }
            },
        )
        .unwrap_err();
        assert!(matches!(error, ResolutionError::UnavailableSecret(error)
            if error.path() == "state.config.password"));
    }
}

#[test]
fn snapshot_preserves_real_loop_slots() {
    use carina_core::resource::LoopSlot;
    let expression: Value<LoopSlot> = Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value));
    let bytes = serde_json::to_vec(&expression).unwrap();
    let snapshot = expression.to_snapshot();
    assert_eq!(serde_json::to_vec(&snapshot).unwrap(), bytes);
    let decoded: SnapshotValue<LoopSlot> = serde_json::from_slice(&bytes).unwrap();
    let restored = decoded.to_value_at("plan.loop").unwrap();
    assert_eq!(serde_json::to_vec(&restored).unwrap(), bytes);
}

#[test]
fn snapshot_state_typecheck_uses_checked_conversion() {
    use carina_core::parser::{ProviderContext, TypeExpr};
    use carina_core::validation::typecheck::check_type_expr_value_providerless;
    let state = historical_siblings();
    let ordinary = resolve_historical_field("name").unwrap();
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::new([]));
    let parsed = carina_core::parser::parse_file(
        "let ordinary = 'ordinary'",
        std::path::Path::new("test.crn"),
        &context,
    )
    .unwrap();
    let schemas = carina_core::schema::SchemaRegistry::new();
    let loaded = carina_core::config_loader::load_buffer_configuration(&parsed, &schemas, &context);
    let report = loaded.judged();
    let expression = report
        .report()
        .binding(&carina_core::resource::RelativeBindingName::new("ordinary"))
        .unwrap();
    check_type_expr_value_providerless(&TypeExpr::String, &ordinary, &context, expression)
        .require_provider_schemas()
        .unwrap();
    assert!(
        check_type_expr_value_providerless(&TypeExpr::Int, &ordinary, &context, expression)
            .require_provider_schemas()
            .is_err()
    );
    assert_eq!(
        state
            .get("password")
            .unwrap()
            .to_value_at("state.password")
            .unwrap_err()
            .path(),
        "state.password"
    );
}

#[test]
fn snapshot_state_unresolved_filter_uses_checked_conversion() {
    let state = historical_siblings();
    let ordinary = resolve_historical_field("name").unwrap();
    assert!(matches!(
        ordinary
            .knowledge(&carina_core::resource::knowledge::KnowledgeContext::validation())
            .map(|proof| proof.into_classification()),
        Ok(carina_core::resource::knowledge::Classification::Known(_))
    ));
    assert!(
        state
            .get("password")
            .unwrap()
            .to_value_at("state.password")
            .is_err()
    );
}

#[test]
fn snapshot_saved_plan_loop_expansion_uses_checked_conversion() {
    use carina_core::parser::{
        DeferredForExpression, DeferredForTarget, ForBinding, expand_deferred_children,
    };
    use carina_core::resource::{
        AccessMissPolicy, LoopSlot, ModuleSource, Resource, absolute_binding_address_for_test,
    };
    let mut template: Resource<ModuleSource, LoopSlot> = Resource::new("mock.Target", "children");
    template.binding = Some(absolute_binding_address_for_test("children"));
    template.set_attr(
        "name",
        Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value)),
    );
    let deferred = DeferredForExpression::new(
        None,
        1,
        "for item in state.items".into(),
        DeferredForTarget::bound(template.binding.clone().unwrap()).unwrap(),
        root_resource_ref("state", "items", vec![]),
        ForBinding::Simple("item".into()),
        template,
    );
    // Decode the same snapshot representation used inside saved-plan values.
    let saved: SnapshotValue =
        serde_json::from_slice(&serde_json::to_vec(&historical_siblings()).unwrap()).unwrap();
    let items = resolve_historical_field("items").unwrap();
    let children = carina_core::validation::deferred_types::with_session_for_test(
        &carina_core::parser::LoadedProviderContext::empty_for_test(),
        |session| {
            expand_deferred_children(
                &deferred,
                match items
                    .knowledge(&carina_core::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap()
                {
                    carina_core::resource::knowledge::Classification::Known(value) => {
                        carina_core::resource::knowledge::PlanIterable::from_known(value).unwrap()
                    }
                    carina_core::resource::knowledge::Classification::ShapeKnown(value) => {
                        carina_core::resource::knowledge::PlanIterable::from_shape(value).unwrap()
                    }
                    carina_core::resource::knowledge::Classification::Pending(_)
                    | carina_core::resource::knowledge::Classification::Failed(_) => {
                        panic!("fixture collection has exact shape")
                    }
                },
                AccessMissPolicy::Error,
                session,
            )
        },
    )
    .unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(
        children[0].get_attr("name"),
        Some(&Value::Concrete(ConcreteValue::String("one".into())))
    );
    assert!(
        saved
            .get("password")
            .unwrap()
            .to_value_at("state.password")
            .is_err()
    );
}

#[test]
fn snapshot_saved_plan_hash_mismatch_is_rejected() {
    let saved: SnapshotValue = serde_json::from_value(hash_wire()).unwrap();
    let changed = Value::Deferred(DeferredValue::Secret(
        Box::new(Value::Concrete(ConcreteValue::String("changed".into()))).into(),
    ));
    assert!(matches!(
        carina_core::value::rehydrate_redacted_secrets(&saved, &changed),
        Err(carina_core::value::SecretRehydrationError::HashMismatch)
    ));
}
