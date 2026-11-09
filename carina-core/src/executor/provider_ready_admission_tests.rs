use super::*;
use crate::resource::ConcreteValue;
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema as DraftSchema, TypeIdentity};

const PLAINTEXT: &str = "apply-admission-secret-plaintext";
const MAP_KEY: &str = "apply-admission-private-map-key";

fn text(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}

fn assert_hidden(rendered: &str) {
    for secret in [PLAINTEXT, MAP_KEY] {
        assert!(!rendered.contains(secret), "secret exposed: {rendered}");
    }
}

fn assert_protected_constraint(error: ProviderPreparationError) {
    assert_hidden(&error.to_string());
    assert_hidden(&format!("{error:?}"));
    let ProviderPreparationError::ValueConstraint { mut errors, .. } = error else {
        panic!("expected an apply value constraint: {error:?}");
    };
    assert!(!errors.errors().is_empty());
    assert_hidden(&errors.to_string());
    assert_hidden(&format!("{errors:?}"));
    for error in errors.errors() {
        assert_hidden(&error.to_string());
        assert_hidden(&format!("{error:?}"));
    }
    let findings = errors.0.take_attribute("value").expect("retained findings");
    assert_hidden(&format!("{findings:?}"));
    let located = findings.into_located();
    assert!(
        !located.is_empty(),
        "apply must retain the failing location"
    );
    for (path, error) in located {
        assert_hidden(&path.to_string());
        assert_hidden(&format!("{path:?}"));
        assert_hidden(&error.to_string());
        assert_hidden(&format!("{error:?}"));
    }
}

/// Exercise both managed/data-source gates, with authored and plan-normalized
/// inputs. Resolve first so every apply check sees the real protected payload.
async fn check_apply_errors(attr: AttributeType, authored: Value) {
    let mut schemas = SchemaRegistry::new();
    for schema in [
        DraftSchema::new("Sink"),
        DraftSchema::new("Sink").as_data_source(),
    ] {
        schemas.insert(
            "test",
            schema
                .attribute(AttributeSchema::optional("value", attr.clone()).build())
                .validate()
                .unwrap(),
        );
    }
    let mut bindings = ResolvedBindings::default();
    bindings.set(
        "producer",
        HashMap::from([("output".into(), text(PLAINTEXT))]),
        crate::binding_index::BindingValueSource::Local,
    );
    let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
    crate::validation::deferred_types::with_session_for_test_async(
        &crate::parser::LoadedProviderContext::empty_for_test(),
        async |session| {
            let context = ProviderPreparationContext::new(
                &bindings,
                &module_gate,
                &[],
                &crate::provider::NoopNormalizer,
                &[],
                &schemas,
                session,
            );
            let resolved =
                crate::resolver::resolve_ref_value(&authored, &bindings, session).unwrap();
            for normalized in [false, true] {
                let managed = Resource::with_provider("test", "Sink", "sink", None)
                    .with_attribute("value", resolved.clone());
                let source = DataSource::with_provider("test", "Sink", "source", None)
                    .with_attribute("value", resolved.clone());
                let authored_attributes = HashMap::from([("value".into(), authored.clone())]);
                let names = HashSet::from(["value".into()]);
                let (managed_error, source_error) = if normalized {
                    (
                        prepare_provider_ready_resource_after_resolution(
                            ResolvedCheckInput::plan_normalized(
                                managed,
                                authored_attributes.clone(),
                                names.clone(),
                            ),
                            &context,
                        )
                        .await
                        .unwrap_err(),
                        prepare_provider_ready_data_source_after_resolution(
                            ResolvedCheckInput::plan_normalized(source, authored_attributes, names),
                            &context,
                        )
                        .unwrap_err(),
                    )
                } else {
                    (
                        prepare_provider_ready_resource(managed, &context)
                            .await
                            .unwrap_err(),
                        prepare_provider_ready_data_source(source, &context).unwrap_err(),
                    )
                };
                assert_protected_constraint(managed_error);
                assert_protected_constraint(source_error);
            }
        },
    )
    .await;
}

#[tokio::test]
async fn apply_secret_errors_remain_protected() {
    for attr in [
        AttributeType::refined_string(None, None, Some((None, Some(3))), None),
        AttributeType::enum_(
            TypeIdentity::bare("Mode"),
            Some(vec!["allowed".into()]),
            vec![],
            None,
            None,
        ),
    ] {
        check_apply_errors(
            attr,
            Value::root_resource_ref("producer", "output", vec![]).into_secret(),
        )
        .await;
    }
}

#[tokio::test]
async fn apply_protected_paths_never_expose_secret_map_keys() {
    let map = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
        MAP_KEY.into(),
        Value::root_resource_ref("producer", "output", vec![]),
    )])));
    let map_type = AttributeType::map(AttributeType::refined_string(
        None,
        None,
        Some((None, Some(3))),
        None,
    ));
    check_apply_errors(map_type.clone(), map.clone().into_secret()).await;
    check_apply_errors(
        AttributeType::list(map_type.clone()),
        Value::Concrete(ConcreteValue::List(vec![map.clone()])).into_secret(),
    )
    .await;
    check_apply_errors(
        AttributeType::list(map_type),
        Value::Concrete(ConcreteValue::List(vec![map.into_secret()])),
    )
    .await;
}
