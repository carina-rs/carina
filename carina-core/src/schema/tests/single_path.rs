use crate::parser::{CustomTypeValidationError, CustomTypeValidationTarget};
use crate::resource::{ConcreteValue, Value};
use crate::schema::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn resource_collection_calls_native_and_bridged_validators_once_per_leaf() {
    for native in [false, true] {
        for rejecting in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let identity = TypeIdentity::new(Some("semantic"), ["test"], "Counted");
            let leaf = if native {
                let calls = Arc::clone(&calls);
                AttributeType::refined_string_with_validator(
                    identity.clone(),
                    None,
                    None,
                    validator(move |_| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        if rejecting {
                            Err(TypeError::ValidationFailed {
                                message: "rejected".into(),
                            })
                        } else {
                            Ok(())
                        }
                    }),
                    None,
                )
            } else {
                AttributeType::refined_string(Some(identity.clone()), None, None, None)
            };
            // Exercise all recursive routes without the old fit/error replay.
            let mut registry = SchemaRegistry::new();
            registry.insert(
                "owner",
                ResourceSchema::new("test.Counted")
                    .attribute(
                        AttributeSchema::optional(
                            "values",
                            AttributeType::list(AttributeType::map(AttributeType::struct_(
                                "Entry",
                                vec![StructField::required("value", leaf).build()],
                            ))),
                        )
                        .build(),
                    )
                    .validate()
                    .unwrap(),
            );
            let schema = registry
                .get("owner", "test.Counted", SchemaKind::Resource)
                .unwrap();
            let values: HashMap<String, Value> = HashMap::from([(
                "values".into(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::Map(indexmap::indexmap! {
                        "first".into() => Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                            "value".into() => Value::Concrete(ConcreteValue::String("one".into())),
                        })),
                        "second".into() => Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                            "value".into() => Value::Concrete(ConcreteValue::String("two".into())),
                        })),
                    }),
                )])),
            )]);
            let resolver = |target: CustomTypeValidationTarget<'_>, _: &Value| {
                assert!(!native, "native callbacks must not be bridged");
                let CustomTypeValidationTarget::Declaration { owner, .. } = target else {
                    panic!("expected a schema declaration");
                };
                assert_eq!(owner, "owner");
                calls.fetch_add(1, Ordering::SeqCst);
                if rejecting {
                    Err(CustomTypeValidationError::Rejected {
                        owner: owner.into(),
                        identity: identity.clone(),
                        message: "rejected".into(),
                    })
                } else {
                    Ok(())
                }
            };
            let result = schema.validate_with_resolver(&values, &|_| false, &resolver);
            assert_eq!(result.is_err(), rejecting);
            assert_eq!(
                calls.swap(0, Ordering::SeqCst),
                2,
                "native={native}, rejecting={rejecting}"
            );
            let findings = schema.collect_validation_with_resolver(&values, &|_| false, &resolver);
            assert_eq!(findings.is_empty(), !rejecting);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "native={native}, rejecting={rejecting}"
            );
        }
    }
}

#[test]
fn resource_collection_retains_distinct_list_leaves_and_duplicate_anchor() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "owner",
        ResourceSchema::new("test.List")
            .attribute(
                AttributeSchema::optional(
                    "values",
                    AttributeType::refined_list(
                        AttributeType::int(),
                        true,
                        None,
                        ListElements::Unique,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let values: HashMap<String, Value> = HashMap::from([(
        "values".into(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(3)),
            Value::Concrete(ConcreteValue::Int(3)),
            Value::Concrete(ConcreteValue::String("bad".into())),
            Value::Concrete(ConcreteValue::String("bad".into())),
        ])),
    )]);
    let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| Ok(());
    let findings = registry
        .get("owner", "test.List", SchemaKind::Resource)
        .unwrap()
        .collect_validation_with_resolver(&values, &|_| false, &resolver);
    assert_eq!(findings.len(), 3, "{findings:#?}");
    for (finding, (expected_path, expected_anchor)) in findings.iter().zip([
        ("[1]", ValueFindingAnchor::Value),
        ("[2]", ValueFindingAnchor::Key),
        ("[3]", ValueFindingAnchor::Key),
    ]) {
        let ResourceValueTarget::Attribute {
            authored_attribute,
            path,
            anchor,
        } = &finding.target
        else {
            panic!("expected attribute target");
        };
        assert_eq!(authored_attribute, "values");
        assert_eq!(path.to_string(), expected_path);
        assert_eq!(anchor, &expected_anchor);
    }
    assert!(matches!(
        findings[0].error,
        TypeError::DuplicateListElement {
            index: 1,
            first_index: 0,
            ..
        }
    ));
    assert!(matches!(findings[1].error, TypeError::TypeMismatch { .. }));
    assert!(matches!(findings[2].error, TypeError::TypeMismatch { .. }));
}

#[test]
fn resource_collection_checks_both_coalesce_operands_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let leaf = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("Operand"),
        None,
        None,
        validator(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(TypeError::ValidationFailed {
                message: "invalid operand".into(),
            })
        }),
        None,
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "owner",
        ResourceSchema::new("test.Coalesce")
            .attribute(AttributeSchema::optional("value", leaf).build())
            .validate()
            .unwrap(),
    );
    let values: HashMap<String, Value> = HashMap::from([(
        "value".into(),
        Value::Deferred(crate::resource::DeferredValue::Coalesce {
            value: Box::new(Value::Concrete(ConcreteValue::String("left".into()))),
            default: Box::new(Value::Concrete(ConcreteValue::String("default".into()))),
        }),
    )]);
    let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| Ok(());
    let findings = registry
        .get("owner", "test.Coalesce", SchemaKind::Resource)
        .unwrap()
        .collect_validation_with_resolver(&values, &|_| false, &resolver);
    assert_eq!(findings.len(), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
