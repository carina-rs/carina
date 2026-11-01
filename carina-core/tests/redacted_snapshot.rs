use carina_core::resource::{ConcreteValue, DeferredValue, SnapshotValue, Value};

fn hash_wire() -> serde_json::Value {
    serde_json::to_value(carina_core::value::redact_secrets_in_value(&Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(ConcreteValue::String("password".into())))))).unwrap()).unwrap()
}

#[test]
fn snapshot_root_hash_is_unavailable_to_evaluation() {
    let snapshot: SnapshotValue = serde_json::from_value(hash_wire()).unwrap();
    let error = snapshot.to_value_at("state.password").unwrap_err();
    assert_eq!(error.path(), "state.password");
    assert_eq!(error.to_string(), "secret value unavailable at state.password");
}

#[test]
fn snapshot_nested_hash_is_unavailable_to_evaluation() {
    let snapshot: SnapshotValue = serde_json::from_value(serde_json::json!({
        "Map": {"credentials": {"List": [{"Map": {"password": hash_wire()}}]}}
    })).unwrap();
    let error = snapshot.to_value_at("state").unwrap_err();
    assert_eq!(error.path(), "state.credentials[0].password");
}

#[test]
fn snapshot_ordinary_sibling_remains_available() {
    let snapshot: SnapshotValue = serde_json::from_value(serde_json::json!({
        "Map": {"password": hash_wire(), "name": {"String": "ordinary"}}
    })).unwrap();
    let ordinary = snapshot.get("name").unwrap().to_value_at("state.name").unwrap();
    assert_eq!(ordinary, Value::Concrete(ConcreteValue::String("ordinary".into())));
    assert!(snapshot.get("password").unwrap().to_value_at("state.password").is_err());
}

#[test]
fn snapshot_root_and_nested_wire_bytes_round_trip() {
    for wire in [hash_wire(), serde_json::json!({"Map": {"nested": {"List": [hash_wire()]}}})] {
        let bytes = serde_json::to_vec(&wire).unwrap();
        let snapshot: SnapshotValue = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&snapshot).unwrap(), bytes);
        assert!(serde_json::from_slice::<Value>(&bytes).is_err());
    }
}

fn historical_siblings() -> SnapshotValue {
    let desired = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
        ("password".into(), Value::Deferred(DeferredValue::Secret(Box::new(
            Value::Concrete(ConcreteValue::String("password".into())))))),
        ("name".into(), Value::Concrete(ConcreteValue::String("ordinary".into()))),
        ("items".into(), Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("one".into()))
        ]))),
    ])));
    let redacted = carina_core::value::redact_secrets_in_value(&desired).unwrap();
    carina_core::value::PersistedValue::encode_provider_value(&redacted,None).unwrap().decode().unwrap()
}

#[test]
fn snapshot_state_typecheck_uses_checked_conversion() {
    use carina_core::parser::{ProviderContext,TypeExpr};
    use carina_core::validation::typecheck::{check_type_expr_value_providerless,TypecheckInference};
    let state = historical_siblings();
    let ordinary = state.get("name").unwrap().to_value_at("state.name").unwrap();
    check_type_expr_value_providerless(&TypeExpr::String,&ordinary,&ProviderContext::default(),TypecheckInference::Unavailable).unwrap();
    assert!(check_type_expr_value_providerless(&TypeExpr::Int,&ordinary,&ProviderContext::default(),TypecheckInference::Unavailable).is_err());
    assert_eq!(state.get("password").unwrap().to_value_at("state.password").unwrap_err().path(),"state.password");
}

#[test]
fn snapshot_state_unresolved_filter_uses_checked_conversion() {
    let state = historical_siblings();
    let ordinary = state.get("name").unwrap().to_value_at("state.name").unwrap();
    assert!(!carina_core::validation::value_contains_unresolved_ref(&ordinary));
    assert!(state.get("password").unwrap().to_value_at("state.password").is_err());
}

#[test]
fn snapshot_saved_plan_loop_expansion_uses_checked_conversion() {
    use carina_core::parser::{DeferredForExpression,DeferredForTarget,ForBinding,expand_deferred_children};
    use carina_core::resource::{AccessMissPolicy,LoopSlot,ModuleSource,Resource,RootScope,RelativeBindingName};
    let mut template: Resource<ModuleSource,LoopSlot> = Resource::new("mock.Target","children");
    template.binding = Some(RootScope::new().resolve(RelativeBindingName::new("children")));
    template.set_attr("name",Value::Deferred(DeferredValue::Placeholder(LoopSlot::Value)));
    let deferred = DeferredForExpression::new(None,1,"for item in state.items".into(),
        DeferredForTarget::bound(template.binding.clone().unwrap()).unwrap(),
        Value::root_resource_ref("state","items",vec![]),ForBinding::Simple("item".into()),template);
    // Decode the same snapshot representation used inside saved-plan values.
    let saved: SnapshotValue = serde_json::from_slice(&serde_json::to_vec(&historical_siblings()).unwrap()).unwrap();
    let items = saved.get("items").unwrap().to_value_at("state.items").unwrap();
    let children = expand_deferred_children(&deferred,&items,AccessMissPolicy::Error).unwrap();
    assert_eq!(children.len(),1);
    assert_eq!(children[0].get_attr("name"),Some(&Value::Concrete(ConcreteValue::String("one".into()))));
    assert!(saved.get("password").unwrap().to_value_at("state.password").is_err());
}

#[test]
fn snapshot_saved_plan_hash_mismatch_is_rejected() {
    let saved: SnapshotValue = serde_json::from_value(hash_wire()).unwrap();
    let changed = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(ConcreteValue::String("changed".into())))));
    assert!(matches!(carina_core::value::rehydrate_redacted_secrets(&saved,&changed),Err(carina_core::value::SecretRehydrationError::HashMismatch)));
}
