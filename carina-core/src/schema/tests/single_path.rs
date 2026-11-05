use crate::parser::{CustomTypeValidationError, CustomTypeValidationTarget};
use crate::resource::{ConcreteValue, RawEnumIdentifier, Value};
use crate::schema::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn round7_nested_coalesce_callbacks_are_reused_across_union_arms() {
    for native in [true, false] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let identity = TypeIdentity::new(Some("mock"), ["test"], "Counted");
        let leaf = if native {
            AttributeType::refined_string_with_validator(
                identity.clone(),
                None,
                None,
                validator(move |_| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Err(TypeError::ValidationFailed {
                        message: "rejected".into(),
                    })
                }),
                None,
            )
        } else {
            AttributeType::refined_string(Some(identity.clone()), None, None, None)
        };
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "mock",
            ResourceSchema::new("test.Sink")
                .attribute(
                    AttributeSchema::optional(
                        "params",
                        AttributeType::union(vec![
                            AttributeType::list(leaf.clone()),
                            AttributeType::refined_list(
                                leaf,
                                true,
                                Some((Some(2), None)),
                                ListElements::AllowDuplicates,
                            ),
                        ]),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let schema = registry
            .get("mock", "test.Sink", SchemaKind::Resource)
            .unwrap();
        let value = Value::Concrete(ConcreteValue::String("bad".into()));
        let values: HashMap<String, Value> = HashMap::from([(
            "params".into(),
            Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
                crate::resource::DeferredValue::Coalesce {
                    value: Box::new(value.clone()),
                    default: Box::new(value),
                },
            )])),
        )]);
        let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| {
            assert!(!native);
            calls.fetch_add(1, Ordering::SeqCst);
            Err(CustomTypeValidationError::Rejected {
                owner: "mock".into(),
                identity: identity.clone(),
                message: "rejected".into(),
            })
        };
        assert!(
            !schema
                .collect_validation_with_resolver(&values, &|_| false, &resolver)
                .is_empty()
        );
        assert_eq!(calls.swap(0, Ordering::SeqCst), 2, "native={native}");
        assert!(
            schema
                .validate_with_resolver(&values, &|_| false, &resolver)
                .is_err()
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "aggregate: native={native}"
        );
    }
}

#[test]
fn round7_union_map_keys_keep_value_only_bridge_coverage() {
    let identity = TypeIdentity::new(Some("mock"), ["test"], "Key");
    for (index, leaf) in [
        AttributeType::refined_string(Some(identity.clone()), None, None, None),
        AttributeType::enum_(
            identity.clone(),
            Some(vec!["bad".into()]),
            vec![],
            None,
            None,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "mock",
            ResourceSchema::new("test.Sink")
                .attribute(
                    AttributeSchema::optional(
                        "params",
                        AttributeType::map_with_key(
                            AttributeType::union(vec![leaf.clone(), leaf]),
                            AttributeType::string(),
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let schema = registry
            .get("mock", "test.Sink", SchemaKind::Resource)
            .unwrap();
        let values: HashMap<String, Value> = HashMap::from([(
            "params".into(),
            Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                "bad".into() => Value::Concrete(ConcreteValue::String("ok".into()))
            })),
        )]);
        let calls = AtomicUsize::new(0);
        let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(CustomTypeValidationError::Rejected {
                owner: "mock".into(),
                identity: identity.clone(),
                message: "rejected".into(),
            })
        };
        let findings = schema.collect_validation_with_resolver(&values, &|_| false, &resolver);
        assert_eq!(findings.is_empty(), index == 0, "{findings:#?}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            schema
                .validate_with_resolver(&values, &|_| false, &resolver)
                .is_ok(),
            index == 0
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn round7_quoted_closed_enum_keeps_native_diagnostic_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::optional(
                    "params",
                    AttributeType::enum_(
                        TypeIdentity::new(Some("mock"), ["test"], "Event"),
                        Some(vec!["ok".into()]),
                        vec![],
                        Some(validator(move |_| {
                            counter.fetch_add(1, Ordering::SeqCst);
                            Err(TypeError::ValidationFailed {
                                message: "closed enum rejection".into(),
                            })
                        })),
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let schema = registry
        .get("mock", "test.Sink", SchemaKind::Resource)
        .unwrap();
    for text in ["ok", "bad"] {
        let values: HashMap<String, Value> = HashMap::from([(
            "params".into(),
            Value::Concrete(ConcreteValue::String(text.into())),
        )]);
        let findings = schema.collect_validation_with_resolver(
            &values,
            &|_| false,
            &|_: CustomTypeValidationTarget<'_>, _: &Value| {
                panic!("native enum must not be bridged")
            },
        );
        assert_eq!(findings.len(), 1, "{findings:#?}");
        let TypeError::StringLiteralExpectedEnum {
            expected,
            extra_message,
            ..
        } = &findings[0].error
        else {
            panic!("{findings:#?}")
        };
        assert_eq!(expected.len(), 1);
        assert_eq!(extra_message.as_deref(), Some("closed enum rejection"));
        assert_eq!(calls.swap(0, Ordering::SeqCst), 1);
    }
}

#[test]
fn round6_map_key_bridges_preserve_value_only_coverage() {
    let identity = TypeIdentity::new(Some("mock"), ["test"], "Key");
    for (leaf, value) in [
        (
            AttributeType::refined_string(Some(identity.clone()), None, None, None),
            Value::Concrete(ConcreteValue::String("bad".into())),
        ),
        (
            AttributeType::enum_(
                identity.clone(),
                Some(vec!["bad".into()]),
                vec![],
                None,
                None,
            ),
            Value::Concrete(ConcreteValue::EnumIdentifier(RawEnumIdentifier::parse(
                "bad",
            ))),
        ),
    ] {
        for key in [true, false] {
            let calls = AtomicUsize::new(0);
            let map = if key {
                AttributeType::map_with_key(leaf.clone(), AttributeType::string())
            } else {
                AttributeType::map(leaf.clone())
            };
            let mut registry = SchemaRegistry::new();
            registry.insert(
                "mock",
                ResourceSchema::new("test.Sink")
                    .attribute(AttributeSchema::optional("params", map).build())
                    .validate()
                    .unwrap(),
            );
            let schema = registry
                .get("mock", "test.Sink", SchemaKind::Resource)
                .unwrap();
            let values: HashMap<String, Value> = HashMap::from([(
                "params".into(),
                Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                    "bad".into() => if key {
                        Value::Concrete(ConcreteValue::String("bad".into()))
                    } else {
                        value.clone()
                    }
                })),
            )]);
            let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(CustomTypeValidationError::Rejected {
                    owner: "mock".into(),
                    identity: identity.clone(),
                    message: "rejected".into(),
                })
            };
            let findings = schema.collect_validation_with_resolver(&values, &|_| false, &resolver);
            assert_eq!(
                findings.len(),
                usize::from(!key),
                "key={key}: {findings:#?}"
            );
            assert_eq!(calls.swap(0, Ordering::SeqCst), usize::from(!key));
            assert_eq!(
                schema
                    .validate_with_resolver(&values, &|_| false, &resolver)
                    .is_ok(),
                key
            );
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(!key));
        }
    }
}

#[test]
fn round6_rejected_union_arms_keep_independent_provider_findings_once() {
    let calls = AtomicUsize::new(0);
    let identity = TypeIdentity::new(Some("mock"), ["test"], "VpcId");
    let arm = |name| {
        AttributeType::struct_(
            name,
            vec![
                StructField::required("required", AttributeType::int()).build(),
                StructField::optional(
                    "id",
                    AttributeType::refined_string(Some(identity.clone()), None, None, None),
                )
                .build(),
            ],
        )
    };
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::optional("params", AttributeType::union(vec![arm("A"), arm("B")]))
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let schema = registry
        .get("mock", "test.Sink", SchemaKind::Resource)
        .unwrap();
    let values: HashMap<String, Value> = HashMap::from([(
        "params".into(),
        Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
            "id".into() => Value::Concrete(ConcreteValue::String("bad".into()))
        })),
    )]);
    let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| {
        calls.fetch_add(1, Ordering::SeqCst);
        Err(CustomTypeValidationError::Rejected {
            owner: "mock".into(),
            identity: identity.clone(),
            message: "independent provider rejection".into(),
        })
    };
    let findings = schema.collect_validation_with_resolver(&values, &|_| false, &resolver);
    assert_eq!(calls.swap(0, Ordering::SeqCst), 1);
    assert_eq!(findings.len(), 2, "{findings:#?}");
    assert!(
        findings
            .iter()
            .any(|finding| matches!(finding.error, TypeError::CustomTypeValidation { .. }))
    );
    let errors = schema
        .validate_with_resolver(&values, &|_| false, &resolver)
        .unwrap_err();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(errors.len(), 2, "{errors:#?}");
}

#[test]
fn round5_open_enum_map_keys_recognize_only_the_declarations_namespace() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = Arc::clone(&seen);
    let key = AttributeType::enum_(
        TypeIdentity::new(Some("mock"), ["test"], "Key"),
        None,
        vec![],
        Some(validator(move |value| {
            let Value::Concrete(ConcreteValue::String(text)) = value else {
                panic!("{value:?}")
            };
            let text = text.expose_plaintext().to_string();
            calls.lock().unwrap().push(text.clone());
            if ["ok", "A.json", "other.thing"].contains(&text.as_str()) {
                Ok(())
            } else {
                Err(TypeError::ValidationFailed {
                    message: "literal API key or member spelling required".into(),
                })
            }
        })),
        None,
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::optional(
                    "params",
                    AttributeType::map_with_key(key, AttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let schema = registry
        .get("mock", "test.Sink", SchemaKind::Resource)
        .unwrap();
    for (authored, expected) in [
        ("mock.test.Key.ok", "ok"),
        ("A.json", "A.json"),
        ("other.thing", "other.thing"),
    ] {
        let values: HashMap<String, Value> = HashMap::from([(
            "params".into(),
            Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                authored.into() => Value::Concrete(ConcreteValue::String("value".into()))
            })),
        )]);
        let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| Ok(());
        let findings = schema.collect_validation_with_resolver(&values, &|_| false, &resolver);
        assert!(findings.is_empty(), "{authored}: {findings:#?}");
        assert_eq!(
            std::mem::take(&mut *seen.lock().unwrap()),
            [expected],
            "{authored}"
        );
        let result = schema.validate_with_resolver(&values, &|_| false, &resolver);
        assert!(result.is_ok(), "{authored}: {result:#?}");
        assert_eq!(
            std::mem::take(&mut *seen.lock().unwrap()),
            [expected],
            "{authored}"
        );
    }
}

#[test]
fn round4_union_canonicalization_keeps_the_provider_accepted_arms_fit() {
    let first = TypeIdentity::new(Some("mock"), ["test"], "First");
    let second = TypeIdentity::new(Some("mock"), ["test"], "Second");
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::optional(
                    "params",
                    AttributeType::union(
                        [first.clone(), second.clone()]
                            .into_iter()
                            .map(|identity| {
                                AttributeType::enum_(
                                    identity,
                                    Some(vec!["ok".into()]),
                                    vec![],
                                    None,
                                    None,
                                )
                            })
                            .collect(),
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let context = crate::parser::ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            std::collections::HashSet::new(),
            Box::new(move |target: CustomTypeValidationTarget<'_>, _: &Value| {
                if target.identity() == &first {
                    Err(CustomTypeValidationError::Rejected {
                        owner: "mock".into(),
                        identity: first.clone(),
                        message: "first arm rejects".into(),
                    })
                } else {
                    Ok(())
                }
            }),
        ),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    };
    let mut parsed = crate::parser::parse_file(
        "mock.test.Sink { params = ok }\n",
        std::path::Path::new("main.crn"),
        &context,
    )
    .unwrap();
    let checked = crate::validation::validate_resources_for_canonicalization(
        &parsed,
        &registry,
        &std::collections::HashSet::from(["mock".into()]),
        &context,
    )
    .unwrap();
    checked.canonicalize(&mut parsed.resources).unwrap();
    let Value::Concrete(ConcreteValue::CanonicalEnum(canonical)) =
        &parsed.resources[0].attributes["params"]
    else {
        panic!("canonical enum")
    };
    assert_eq!(canonical.identity(), &second);
}

#[test]
fn round4_folded_coalesce_enum_shadow_has_only_authority_error() {
    let findings = review_binding_collection(
        AttributeSchema::optional(
            "value",
            AttributeType::enum_(
                TypeIdentity::new(Some("mock"), ["test", "Sink"], "Event"),
                Some(vec!["viewer_request".into(), "ok".into()]),
                vec![],
                None,
                None,
            ),
        )
        .build(),
        "let viewer_request = 5\nmock.test.Sink { value = viewer_request ?? ok }\n",
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert!(matches!(
        findings[0].error,
        TypeError::BindingShadowsEnumMember(_)
    ));
}

#[test]
fn round3_checked_canonicalization_consumes_the_validation_fit() {
    for change_input in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let key = AttributeType::enum_(
            TypeIdentity::new(Some("mock"), ["test"], "Key"),
            None,
            vec![],
            Some(validator(move |value| {
                counter.fetch_add(1, Ordering::SeqCst);
                assert_eq!(value, &Value::Concrete(ConcreteValue::String("ok".into())));
                Ok(())
            })),
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "mock",
            ResourceSchema::new("test.Sink")
                .attribute(
                    AttributeSchema::optional(
                        "params",
                        AttributeType::map_with_key(key, AttributeType::string()),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let mut parsed = crate::parser::parse_file(
            "mock.test.Sink { params = { \"mock.test.Key.ok\" = \"value\" } }\n",
            std::path::Path::new("main.crn"),
            &context,
        )
        .unwrap();
        let checked = crate::validation::validate_resources_for_canonicalization(
            &parsed,
            &registry,
            &std::collections::HashSet::from(["mock".into()]),
            &context,
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        if change_input {
            parsed.resources[0]
                .attributes
                .insert("params".into(), Value::Concrete(ConcreteValue::Int(1)));
        }
        assert_eq!(
            checked.canonicalize(&mut parsed.resources).is_err(),
            change_input
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        if !change_input {
            let Value::Concrete(ConcreteValue::Map(values)) =
                &parsed.resources[0].attributes["params"]
            else {
                panic!("map")
            };
            assert!(values.contains_key("ok"), "{values:?}");
        }
    }
}

#[test]
fn round3_checked_canonicalization_keeps_conditional_fits() {
    let mut registry = SchemaRegistry::new();
    let key = AttributeType::enum_(
        TypeIdentity::new(Some("mock"), ["test"], "Key"),
        Some(vec!["ok".into()]),
        vec![],
        Some(validator(|_| Ok(()))),
        None,
    );
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::optional(
                    "params",
                    AttributeType::map_with_key(key, AttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut parsed = crate::parser::parse_file("arguments { flag: Bool }\nmock.test.Sink { params = if flag { { \"mock.test.Key.ok\" = \"a\" } } else { { \"mock.test.Key.ok\" = \"b\" } } }\n", std::path::Path::new("main.crn"), &context).unwrap();
    let checked = crate::validation::validate_resources_for_canonicalization(
        &parsed,
        &registry,
        &std::collections::HashSet::from(["mock".into()]),
        &context,
    )
    .unwrap();
    assert!(checked.canonicalize(&mut parsed.resources).is_ok());
    let Value::Deferred(crate::resource::DeferredValue::ModuleIf(conditional)) =
        &parsed.resources[0].attributes["params"]
    else {
        panic!("conditional")
    };
    for branch in [conditional.then_value(), conditional.else_value()] {
        let Value::Concrete(ConcreteValue::Map(values)) = branch else {
            panic!("map")
        };
        assert!(values.contains_key("ok"), "{values:?}");
    }
}

#[test]
fn round3_enum_union_reuses_the_native_callback_outcome() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let callback = validator(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        Err(TypeError::ValidationFailed {
            message: "reject".into(),
        })
    });
    let member = |name| {
        AttributeType::enum_(
            TypeIdentity::new(Some("mock"), ["test"], name),
            None,
            vec![],
            Some(callback.clone()),
            None,
        )
    };
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::optional(
                    "params",
                    AttributeType::union(vec![member("First"), member("Second")]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let schema = registry
        .get("mock", "test.Sink", SchemaKind::Resource)
        .unwrap();
    let values: HashMap<String, Value> = HashMap::from([(
        "params".into(),
        Value::Concrete(ConcreteValue::enum_identifier("bad")),
    )]);
    let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| panic!("native");
    assert!(
        !schema
            .collect_validation_with_resolver(&values, &|_| false, &resolver)
            .is_empty()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    calls.store(0, Ordering::SeqCst);
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::optional(
                    "params",
                    AttributeType::map_with_key(
                        AttributeType::union(vec![member("First"), member("Second")]),
                        member("First"),
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let schema = registry
        .get("mock", "test.Sink", SchemaKind::Resource)
        .unwrap();
    let values: HashMap<String, Value> = HashMap::from([(
        "params".into(),
        Value::Concrete(ConcreteValue::Map(
            indexmap::indexmap! { "bad".into() => Value::Concrete(ConcreteValue::String("bad".into())) },
        )),
    )]);
    assert!(
        !schema
            .collect_validation_with_resolver(&values, &|_| false, &resolver)
            .is_empty()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "key and value are distinct authored leaves"
    );
}

#[test]
fn round3_union_caches_callbacks_independently_of_portable_refinements() {
    for native in [true, false] {
        for rejecting in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&calls);
            let callback = validator(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                if rejecting {
                    Err(TypeError::ValidationFailed {
                        message: "rejected".into(),
                    })
                } else {
                    Ok(())
                }
            });
            let identity = TypeIdentity::new(Some("mock"), ["test"], "Counted");
            let leaf = |max| {
                if native {
                    AttributeType::refined_string_with_validator(
                        identity.clone(),
                        None,
                        Some((None, Some(max))),
                        callback.clone(),
                        None,
                    )
                } else {
                    AttributeType::refined_string(
                        Some(identity.clone()),
                        None,
                        Some((None, Some(max))),
                        None,
                    )
                }
            };
            let mut registry = SchemaRegistry::new();
            registry.insert(
                "mock",
                ResourceSchema::new("test.Sink")
                    .attribute(
                        AttributeSchema::optional(
                            "params",
                            AttributeType::union(vec![leaf(3), leaf(4)]),
                        )
                        .build(),
                    )
                    .validate()
                    .unwrap(),
            );
            let schema = registry
                .get("mock", "test.Sink", SchemaKind::Resource)
                .unwrap();
            let values: HashMap<String, Value> = HashMap::from([(
                "params".into(),
                Value::Concrete(ConcreteValue::String("ok".into())),
            )]);
            let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| {
                assert!(!native, "native validator must not bridge");
                calls.fetch_add(1, Ordering::SeqCst);
                if rejecting {
                    Err(CustomTypeValidationError::Rejected {
                        owner: "mock".into(),
                        identity: identity.clone(),
                        message: "rejected".into(),
                    })
                } else {
                    Ok(())
                }
            };
            let findings = schema.collect_validation_with_resolver(&values, &|_| false, &resolver);
            assert_eq!(findings.is_empty(), !rejecting, "{findings:#?}");
            assert_eq!(
                calls.swap(0, Ordering::SeqCst),
                1,
                "rejecting={rejecting}: {findings:#?}"
            );
            assert_eq!(
                schema
                    .validate_with_resolver(&values, &|_| false, &resolver)
                    .is_err(),
                rejecting
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn round2_union_reuses_rejecting_leaf_across_arms_and_keeps_operands() {
    for native in [true, false] {
        for coalesce in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&calls);
            let identity = TypeIdentity::new(Some("mock"), ["test"], "Counted");
            let leaf = if native {
                AttributeType::refined_string_with_validator(
                    identity.clone(),
                    None,
                    None,
                    validator(move |_| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Err(TypeError::ValidationFailed {
                            message: "rejected".into(),
                        })
                    }),
                    None,
                )
            } else {
                AttributeType::refined_string(Some(identity.clone()), None, None, None)
            };
            let mut registry = SchemaRegistry::new();
            registry.insert(
                "mock",
                ResourceSchema::new("test.Sink")
                    .attribute(
                        AttributeSchema::optional(
                            "params",
                            AttributeType::union(vec![
                                AttributeType::list(leaf.clone()),
                                AttributeType::refined_list(
                                    leaf,
                                    true,
                                    Some((Some(2), None)),
                                    ListElements::AllowDuplicates,
                                ),
                            ]),
                        )
                        .build(),
                    )
                    .validate()
                    .unwrap(),
            );
            let operand: Value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("bad".into()),
            )]));
            let value = if coalesce {
                Value::Deferred(crate::resource::DeferredValue::Coalesce {
                    value: Box::new(operand.clone()),
                    default: Box::new(operand),
                })
            } else {
                operand
            };
            let values = HashMap::from([("params".into(), value)]);
            let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| {
                assert!(!native);
                calls.fetch_add(1, Ordering::SeqCst);
                Err(CustomTypeValidationError::Rejected {
                    owner: "mock".into(),
                    identity: identity.clone(),
                    message: "rejected".into(),
                })
            };
            let schema = registry
                .get("mock", "test.Sink", SchemaKind::Resource)
                .unwrap();
            assert!(
                !schema
                    .collect_validation_with_resolver(&values, &|_| false, &resolver)
                    .is_empty()
            );
            assert_eq!(
                calls.swap(0, Ordering::SeqCst),
                if coalesce { 2 } else { 1 },
                "native={native}, coalesce={coalesce}"
            );
            assert!(
                schema
                    .validate_with_resolver(&values, &|_| false, &resolver)
                    .is_err()
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                if coalesce { 2 } else { 1 },
                "aggregate: native={native}, coalesce={coalesce}"
            );
        }
    }
}

#[test]
fn round2_qualified_enum_map_keys_preserve_dsl_resolution_once() {
    for rejecting in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let key = AttributeType::enum_(
            TypeIdentity::new(Some("mock"), ["test"], "Key"),
            Some(vec!["ok".into()]),
            vec![],
            Some(validator(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                if rejecting {
                    Err(TypeError::ValidationFailed {
                        message: "rejected key".into(),
                    })
                } else {
                    Ok(())
                }
            })),
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "mock",
            ResourceSchema::new("test.Sink")
                .attribute(
                    AttributeSchema::optional(
                        "params",
                        AttributeType::map_with_key(key, AttributeType::string()),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let values: HashMap<String, Value> = HashMap::from([(
            "params".into(),
            Value::Concrete(ConcreteValue::Map(
                indexmap::indexmap! { if rejecting { "mock.test.Key.bad" } else { "mock.test.Key.ok" }.into() => Value::Concrete(ConcreteValue::String("ok".into())) },
            )),
        )]);
        let schema = registry
            .get("mock", "test.Sink", SchemaKind::Resource)
            .unwrap();
        let resolver = |_: CustomTypeValidationTarget<'_>, _: &Value| Ok(());
        let findings = schema.collect_validation_with_resolver(&values, &|_| false, &resolver);
        assert_eq!(calls.swap(0, Ordering::SeqCst), 1, "{findings:#?}");
        assert_eq!(findings.is_empty(), !rejecting, "{findings:#?}");
        assert_eq!(
            schema
                .validate_with_resolver(&values, &|_| false, &resolver)
                .is_err(),
            rejecting
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

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

fn review_binding_collection(
    attribute: AttributeSchema,
    source: &str,
    context: &crate::parser::ProviderContext,
) -> Vec<ResourceValueFinding> {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(attribute)
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse(source, context).unwrap();
    let signatures = crate::module_resolver::ResolvedModuleSignatures::default();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &registry, context);
    let bindings = crate::validation::check_bare_binding_uses(
        &crate::validation::ValidationTypeContext::new_providerless(
            configuration.judged(),
            context,
            &signatures,
        ),
    );
    let tree = crate::parser::AnalysisTree::try_from_file(&parsed).unwrap();
    let resource = tree.resources().next().unwrap();
    let values = resource.resolved_attributes();
    registry
        .get("mock", "test.Sink", SchemaKind::Resource)
        .unwrap()
        .collect_validation_inputs_with_resolver(
            bindings
                .resource_value_inputs(
                    parsed.bare_binding_uses(),
                    resource,
                    &values,
                    configuration.judged().report(),
                )
                .unwrap(),
            &|_| false,
            context.custom_type_resolver(),
        )
}

#[test]
fn binding_precheck_findings_keep_authored_targets_with_and_without_runtime_values() {
    let event = AttributeType::enum_(
        TypeIdentity::new(Some("mock"), ["test", "Sink"], "Event"),
        Some(vec!["viewer_request".into()]),
        vec![],
        None,
        None,
    );
    for (binding, attr_type, value, count) in [
        (
            "5",
            AttributeType::map(event.clone()),
            "{ default = viewer_request, other = 7 }",
            2,
        ),
        ("mock.test.Source {}", event.clone(), "viewer_request", 1),
        (
            "mock.test.Source {}",
            AttributeType::list(event.clone()),
            "[viewer_request, viewer_request]",
            2,
        ),
        (
            "mock.test.Source {}",
            AttributeType::map(event),
            "{ default = viewer_request }",
            1,
        ),
    ] {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("test.Source").validate().unwrap(),
        );
        schemas.insert(
            "mock",
            ResourceSchema::new("test.Sink")
                .attribute(AttributeSchema::required("value", attr_type).build())
                .validate()
                .unwrap(),
        );
        let source =
            format!("let viewer_request = {binding}\nmock.test.Sink {{ value = {value} }}\n");
        let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = crate::parser::parse(&source, &context).unwrap();
        let signatures = crate::module_resolver::ResolvedModuleSignatures::default();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &context);
        let bindings = crate::validation::check_bare_binding_uses(
            &crate::validation::ValidationTypeContext::new_providerless(
                configuration.judged(),
                &context,
                &signatures,
            ),
        );
        let authority: Vec<_> = bindings.resource_value_findings().collect();
        assert!(!authority.is_empty(), "{source}");
        let tree = crate::parser::AnalysisTree::try_from_file(&parsed).unwrap();
        let resource = tree.resources().last().unwrap();
        let values = resource.resolved_attributes();
        let findings = schemas
            .get("mock", "test.Sink", SchemaKind::Resource)
            .unwrap()
            .collect_validation_inputs_with_resolver(
                bindings
                    .resource_value_inputs(
                        parsed.bare_binding_uses(),
                        resource,
                        &values,
                        configuration.judged().report(),
                    )
                    .unwrap(),
                &|_| false,
                context.custom_type_resolver(),
            );
        assert_eq!(findings.len(), count, "{source}: {findings:#?}");
        for finding in authority {
            assert!(matches!(
                finding.error,
                TypeError::BindingShadowsEnumMember(_)
            ));
            assert_eq!(
                findings.iter().filter(|actual| **actual == finding).count(),
                1,
                "binding authority's typed error and parser target must survive: {source}: {findings:#?}",
            );
        }
    }
}

#[test]
fn review_direct_binding_preserves_writability() {
    let findings = review_binding_collection(
        AttributeSchema::provider_populated("value", AttributeType::string()).build(),
        "let input = \"ok\"\nmock.test.Sink { value = input }\n",
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert!(matches!(
        findings[0].error,
        TypeError::ReadOnlyAttribute { .. }
    ));
}

#[test]
fn review_direct_binding_runs_bridged_validator() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let identity = TypeIdentity::bare("Bridged");
    let context = crate::parser::ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            std::collections::HashSet::new(),
            Box::new(move |target: CustomTypeValidationTarget<'_>, _: &Value| {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(CustomTypeValidationError::Rejected {
                    owner: "mock".into(),
                    identity: target.identity().clone(),
                    message: "bridge rejected".into(),
                })
            }),
        ),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    };
    let findings = review_binding_collection(
        AttributeSchema::optional(
            "value",
            AttributeType::refined_string(Some(identity), None, None, None),
        )
        .build(),
        "let input = \"ok\"\nmock.test.Sink { value = input }\n",
        &context,
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "{findings:#?}");
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert!(findings[0].error.to_string().contains("bridge rejected"));
}

#[test]
fn review_failed_nested_binding_retains_count_and_sibling_errors() {
    let findings = review_binding_collection(
        AttributeSchema::optional(
            "values",
            AttributeType::refined_list(
                AttributeType::string(),
                true,
                Some((None, Some(1))),
                ListElements::AllowDuplicates,
            ),
        )
        .build(),
        "let bad = 3\nmock.test.Sink { values = [bad, 4] }\n",
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert_eq!(findings.len(), 3, "{findings:#?}");
    assert_eq!(
        findings
            .iter()
            .filter(|f| matches!(f.error, TypeError::LengthOutOfRange { .. }))
            .count(),
        1
    );
    assert_eq!(
        findings
            .iter()
            .filter(|f| matches!(f.error, TypeError::TypeMismatch { .. }))
            .count(),
        2
    );
}

#[test]
fn review_successful_nested_binding_callback_runs_once() {
    for rejecting in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let leaf = AttributeType::refined_string_with_validator(
            TypeIdentity::bare("Counted"),
            None,
            None,
            validator(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                if rejecting {
                    Err(TypeError::ValidationFailed {
                        message: "rejected".into(),
                    })
                } else {
                    Ok(())
                }
            }),
            None,
        );
        let findings = review_binding_collection(
            AttributeSchema::optional("values", AttributeType::list(leaf)).build(),
            "let input = \"ok\"\nmock.test.Sink { values = [input] }\n",
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "rejecting={rejecting}: {findings:#?}"
        );
        assert_eq!(findings.len(), usize::from(rejecting), "{findings:#?}");
    }
}

#[test]
fn review_quoted_open_enum_callback_receives_canonical_spelling_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let leaf = AttributeType::enum_(
        TypeIdentity::new(Some("mock"), ["test"], "Zone"),
        None,
        vec![],
        Some(validator(move |value| {
            counter.fetch_add(1, Ordering::SeqCst);
            if matches!(value, Value::Concrete(ConcreteValue::String(text)) if text.expose_plaintext() == "ap-northeast-1")
            {
                Ok(())
            } else {
                Err(TypeError::ValidationFailed {
                    message: "canonical API spelling required".into(),
                })
            }
        })),
        Some(DslTransform::HyphenToUnderscore),
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(AttributeSchema::optional("value", leaf).build())
            .validate()
            .unwrap(),
    );
    let values: HashMap<String, Value> = HashMap::from([(
        "value".into(),
        Value::Concrete(ConcreteValue::String("ap_northeast_1".into())),
    )]);
    let result = registry
        .get("mock", "test.Sink", SchemaKind::Resource)
        .unwrap()
        .validate_with_resolver(
            &values,
            &|_| true,
            &|_: CustomTypeValidationTarget<'_>, _: &Value| Ok(()),
        );
    assert!(result.is_ok(), "{result:#?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn review_invalid_enum_map_key_callback_runs_once() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let key = AttributeType::enum_(
        TypeIdentity::new(Some("mock"), ["test"], "Key"),
        None,
        vec![],
        Some(validator(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Err(TypeError::ValidationFailed {
                message: "bad key".into(),
            })
        })),
        None,
    );
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::optional(
                    "values",
                    AttributeType::map_with_key(key, AttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let values: HashMap<String, Value> = HashMap::from([(
        "values".into(),
        Value::Concrete(ConcreteValue::Map(
            indexmap::indexmap! { "bad".into() => Value::Concrete(ConcreteValue::String("ok".into())) },
        )),
    )]);
    let findings = registry
        .get("mock", "test.Sink", SchemaKind::Resource)
        .unwrap()
        .collect_validation_with_resolver(
            &values,
            &|_| false,
            &|_: CustomTypeValidationTarget<'_>, _: &Value| Ok(()),
        );
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn review_secret_map_path_debug_is_redacted() {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::optional(
                    "values",
                    AttributeType::map_with_key(
                        AttributeType::refined_string(None, None, Some((None, Some(1))), None),
                        AttributeType::string(),
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let values: HashMap<String, Value> = HashMap::from([(
        "values".into(),
        Value::Deferred(crate::resource::DeferredValue::Secret(Box::new(
            Value::Concrete(ConcreteValue::Map(
                indexmap::indexmap! { "private-key".into() => Value::Concrete(ConcreteValue::String("ok".into())) },
            )),
        ))),
    )]);
    let findings = registry
        .get("mock", "test.Sink", SchemaKind::Resource)
        .unwrap()
        .collect_validation_with_resolver(
            &values,
            &|_| false,
            &|_: CustomTypeValidationTarget<'_>, _: &Value| Ok(()),
        );
    assert_eq!(findings.len(), 1, "{findings:#?}");
    assert!(!format!("{findings:#?}").contains("private-key"));
    let ResourceValueTarget::Attribute { path, .. } = &findings[0].target else {
        panic!("attribute")
    };
    assert!(!path.to_string().contains("private-key"));
}

#[test]
fn review_bound_map_key_reuses_success_and_failure_results() {
    for rejecting in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let key = AttributeType::enum_(
            TypeIdentity::new(Some("mock"), ["test"], "Key"),
            None,
            vec![],
            Some(validator(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                if rejecting {
                    Err(TypeError::ValidationFailed {
                        message: "bad key".into(),
                    })
                } else {
                    Ok(())
                }
            })),
            None,
        );
        let findings = review_binding_collection(
            AttributeSchema::optional(
                "values",
                AttributeType::map_with_key(key, AttributeType::string()),
            )
            .build(),
            "let input = { bad = \"ok\" }\nmock.test.Sink { values = input }\n",
            &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(findings.len(), usize::from(rejecting), "{findings:#?}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "rejecting={rejecting}");
    }
}
