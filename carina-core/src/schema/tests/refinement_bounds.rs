use super::*;

fn assert_bounds_message(error: &TypeError, expected: &str) {
    let message = error.to_string();
    assert_eq!(message, expected);
    for forbidden in ["Some(", "None", "[, "] {
        assert!(!message.contains(forbidden), "{message}");
    }
    assert_eq!(
        crate::provider_text::ProviderDiagnostic::new(error.clone())
            .into_host_for_test()
            .to_string(),
        message,
        "the provider diagnostic boundary must use the same rendering"
    );
}

#[test]
fn refinement_bounds_int_range_diagnostic() {
    for (bounds, expected) in [
        (
            (Some(10), Some(20)),
            "Invalid Count value '5': outside allowed range 10..=20",
        ),
        (
            (Some(10), None),
            "Invalid Count value '5': outside allowed range 10..unbounded",
        ),
        (
            (None, Some(4)),
            "Invalid Count value '5': outside allowed range unbounded..=4",
        ),
        (
            (Some(10), Some(10)),
            "Invalid Count value '5': outside allowed range 10..=10",
        ),
    ] {
        // Each layer is exercised in isolation; the other layer is unbounded.
        for ty in [
            AttributeType::int_member(
                IntIdentityLayer::new(Some(TypeIdentity::bare("Count")), Some(bounds)),
                IntMemberConstraints::new(Some((None, None))),
            ),
            AttributeType::int_member(
                IntIdentityLayer::new(Some(TypeIdentity::bare("Count")), Some((None, None))),
                IntMemberConstraints::new(Some(bounds)),
            ),
        ] {
            let error = ty
                .validate(&Value::Concrete(ConcreteValue::Int(5)))
                .unwrap_err();
            assert_bounds_message(&error, expected);
            let TypeError::ValueOutOfRange {
                value,
                bounds: retained,
                ..
            } = &error
            else {
                panic!("expected typed int range: {error:?}");
            };
            assert_eq!(*value, DiagnosticNumber::Int(5));
            assert_eq!(
                *retained,
                NumericRangeBounds::Int {
                    min: bounds.0,
                    max: bounds.1
                }
            );
        }
    }
}

#[test]
fn refinement_bounds_float_range_diagnostic() {
    // Float refinements have a single range layer.
    for (bounds, expected) in [
        (
            (Some(crate::finite(10.25)), Some(crate::finite(20.75))),
            "Invalid Ratio value '5.5': outside allowed range 10.25..=20.75",
        ),
        (
            (Some(crate::finite(10.25)), None),
            "Invalid Ratio value '5.5': outside allowed range 10.25..unbounded",
        ),
        (
            (None, Some(crate::finite(4.75))),
            "Invalid Ratio value '5.5': outside allowed range unbounded..=4.75",
        ),
        (
            (Some(crate::finite(10.25)), Some(crate::finite(10.25))),
            "Invalid Ratio value '5.5': outside allowed range 10.25..=10.25",
        ),
    ] {
        let ty = AttributeType::refined_float(Some(TypeIdentity::bare("Ratio")), Some(bounds));
        let error = ty
            .validate(&Value::Concrete(ConcreteValue::Float(crate::finite(5.5))))
            .unwrap_err();
        assert_bounds_message(&error, expected);
        let TypeError::ValueOutOfRange {
            value,
            bounds: retained,
            ..
        } = &error
        else {
            panic!("expected typed float range: {error:?}");
        };
        assert_eq!(*value, DiagnosticNumber::Float(crate::finite(5.5)));
        assert_eq!(
            *retained,
            NumericRangeBounds::Float {
                min: bounds.0,
                max: bounds.1
            }
        );
    }
}

#[test]
fn refinement_bounds_int_in_float_range_diagnostic() {
    for (bounds, expected) in [
        (
            (Some(crate::finite(10.25)), Some(crate::finite(20.75))),
            "Invalid Ratio value '5': outside allowed range 10.25..=20.75",
        ),
        (
            (Some(crate::finite(10.25)), None),
            "Invalid Ratio value '5': outside allowed range 10.25..unbounded",
        ),
        (
            (None, Some(crate::finite(4.75))),
            "Invalid Ratio value '5': outside allowed range unbounded..=4.75",
        ),
        (
            (Some(crate::finite(10.25)), Some(crate::finite(10.25))),
            "Invalid Ratio value '5': outside allowed range 10.25..=10.25",
        ),
    ] {
        let ty = AttributeType::refined_float(Some(TypeIdentity::bare("Ratio")), Some(bounds));
        let error = ty
            .validate(&Value::Concrete(ConcreteValue::Int(5)))
            .unwrap_err();
        assert_bounds_message(&error, expected);
        let TypeError::ValueOutOfRange {
            value,
            bounds: retained,
            ..
        } = &error
        else {
            panic!("expected typed int-in-float range: {error:?}");
        };
        assert_eq!(*value, DiagnosticNumber::Int(5));
        assert_eq!(
            *retained,
            NumericRangeBounds::Float {
                min: bounds.0,
                max: bounds.1
            }
        );
    }
}

#[test]
fn refinement_bounds_string_length_diagnostic() {
    for (bounds, expected) in [
        (
            (Some(2), Some(5)),
            "length 6 is outside allowed range 2..=5",
        ),
        (
            (Some(10), None),
            "length 6 is outside allowed range 10..unbounded",
        ),
        (
            (None, Some(5)),
            "length 6 is outside allowed range unbounded..=5",
        ),
        (
            (Some(5), Some(5)),
            "length 6 is outside allowed range 5..=5",
        ),
    ] {
        for (ty, type_name) in [
            (
                AttributeType::string_member(
                    StringIdentityLayer::new(
                        Some(TypeIdentity::bare("Label")),
                        None,
                        Some(bounds),
                        None,
                    ),
                    StringMemberConstraints::new(None, Some((None, None))),
                ),
                "Label",
            ),
            (
                AttributeType::string_member(
                    StringIdentityLayer::new(
                        Some(TypeIdentity::bare("Label")),
                        None,
                        Some((None, None)),
                        None,
                    ),
                    StringMemberConstraints::new(None, Some(bounds)),
                ),
                "Label (slot member constraint)",
            ),
        ] {
            let error = ty.validate(&string_value("abcdef")).unwrap_err();
            assert_bounds_message(
                &error,
                &format!("Invalid {type_name} value 'abcdef': {expected}"),
            );
        }
    }
}

#[test]
fn refinement_bounds_list_length_diagnostic() {
    // Lists and maps have collection constraints, without identity/member layers.
    for (bounds, expected) in [
        (
            (Some(2), Some(5)),
            "length 6 is outside allowed range 2..=5",
        ),
        (
            (Some(10), None),
            "length 6 is outside allowed range 10..unbounded",
        ),
        (
            (None, Some(5)),
            "length 6 is outside allowed range unbounded..=5",
        ),
        (
            (Some(5), Some(5)),
            "length 6 is outside allowed range 5..=5",
        ),
    ] {
        let ty = AttributeType::refined_list(
            AttributeType::string(),
            true,
            Some(bounds),
            ListElements::AllowDuplicates,
        );
        for value in [
            Value::Concrete(ConcreteValue::List(vec![string_value("item"); 6])),
            Value::Concrete(ConcreteValue::StringList(vec!["item".into(); 6])),
        ] {
            let error = ty.validate(&value).unwrap_err();
            assert_bounds_message(&error, &format!("Invalid List value '6 items': {expected}"));
        }
    }
}

#[test]
fn refinement_bounds_map_length_diagnostic() {
    for (bounds, expected) in [
        (
            (Some(2), Some(5)),
            "length 6 is outside allowed range 2..=5",
        ),
        (
            (Some(10), None),
            "length 6 is outside allowed range 10..unbounded",
        ),
        (
            (None, Some(5)),
            "length 6 is outside allowed range unbounded..=5",
        ),
        (
            (Some(5), Some(5)),
            "length 6 is outside allowed range 5..=5",
        ),
    ] {
        let ty = AttributeType::refined_map(
            AttributeType::string(),
            AttributeType::string(),
            Some(bounds),
        );
        let error = ty.validate(&map_entries(6)).unwrap_err();
        assert_bounds_message(
            &error,
            &format!("Invalid Map value '6 entries': {expected}"),
        );
    }
}

#[test]
fn refinement_bounds_secret_int_masks_only_the_value() {
    let ty = AttributeType::refined_int(None, Some((Some(10), Some(20))));
    let value = Value::Deferred(DeferredValue::Secret(
        Box::new(Value::Concrete(ConcreteValue::Int(5))).into(),
    ));
    let error = ty.validate(&value).unwrap_err();
    assert_bounds_message(
        &error,
        "Invalid value (secret): outside allowed range 10..=20",
    );
    assert!(!error.to_string().contains("'5'"));
    let TypeError::ValueOutOfRange { value, bounds, .. } = &error else {
        panic!("the secret must retain its numeric range category: {error:?}");
    };
    assert_eq!(*value, DiagnosticNumber::Masked);
    assert_eq!(
        *bounds,
        NumericRangeBounds::Int {
            min: Some(10),
            max: Some(20)
        }
    );
}

#[test]
fn refinement_bounds_resource_attribute_has_the_same_context_as_length() {
    let schema = PatternResourceSchema::new("test.RangeContext")
        .attribute(
            PatternAttributeSchema::required(
                "period",
                PatternAttributeType::refined_int(
                    Some(TypeIdentity::bare("Period")),
                    Some((Some(10), Some(20))),
                ),
            )
            .build(),
        )
        .attribute(
            PatternAttributeSchema::required(
                "label",
                PatternAttributeType::refined_string(
                    Some(TypeIdentity::bare("Label")),
                    None,
                    Some((Some(10), Some(20))),
                    None,
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let attributes = HashMap::from([
        ("period".to_string(), Value::Concrete(ConcreteValue::Int(5))),
        ("label".to_string(), string_value("short")),
    ]);
    let errors = schema.validate(&attributes).unwrap_err();
    let length = errors
        .iter()
        .find(|error| error.attribute_name() == Some("label"))
        .unwrap();
    assert_bounds_message(
        length,
        "Invalid value 'short' for 'label' (Label): length 5 is outside allowed range 10..=20",
    );
    let numeric = errors
        .iter()
        .find(|error| error.attribute_name() == Some("period"))
        .unwrap();
    assert_bounds_message(
        numeric,
        "Invalid value '5' for 'period' (Period): outside allowed range 10..=20",
    );
    assert!(
        matches!(numeric, TypeError::ValueOutOfRange { attribute: Some(name), type_name: Some(kind), .. } if name == "period" && kind == "Period")
    );
    let reason: ProviderValidation = numeric.clone().into();
    assert_eq!(reason.category(), Some(numeric));
}

#[test]
fn refinement_bounds_length_message_budget_covers_the_complete_sentence() {
    use crate::provider_text::MAX_PROVIDER_TEXT_BYTES;

    for attribute_length in [MAX_PROVIDER_TEXT_BYTES - 64, MAX_PROVIDER_TEXT_BYTES + 10] {
        let attribute = "a".repeat(attribute_length);
        let error = TypeError::LengthOutOfRange {
            value: "abcdef".into(),
            length: 6,
            min: None,
            max: Some(5),
            attribute: Some(attribute.clone()),
            type_name: None,
        };
        let rendered = error.to_string();
        assert!(
            rendered.len() <= MAX_PROVIDER_TEXT_BYTES,
            "the complete sentence exceeded the diagnostic budget: {} bytes",
            rendered.len()
        );
        let (prefix, marker) = rendered
            .split_once("… (truncated, ")
            .expect("the complete sentence must be truncated");
        let omitted: usize = marker
            .strip_suffix(" bytes)")
            .expect("no suffix may follow the complete-message truncation marker")
            .parse()
            .unwrap();
        let complete = format!(
            "Invalid value 'abcdef' for '{attribute}': length 6 is outside allowed range unbounded..=5"
        );
        assert!(complete.starts_with(prefix));
        assert_eq!(prefix.len() + omitted, complete.len());
        assert_eq!(
            crate::provider_text::ProviderDiagnostic::new(error.clone())
                .into_host_for_test()
                .to_string(),
            rendered
        );
        let TypeError::LengthOutOfRange {
            min,
            max,
            attribute: retained,
            ..
        } = error
        else {
            panic!("expected typed length bounds");
        };
        assert_eq!((min, max), (None, Some(5)));
        assert_eq!(retained.as_deref(), Some(attribute.as_str()));
    }
}
