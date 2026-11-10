use std::collections::BTreeMap;

use carina_core::schema::{
    AttributeSchema, AttributeType, IntIdentityLayer, IntMemberConstraints, PatternMatch,
    PatternSpec, ProviderSchemaValidationError, RawShape, ResourceSchema, StringIdentityLayer,
    StringIdentityParts, StringMemberConstraints, TypeIdentity, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness, types, validator,
};

fn provider(
    declaration: AttributeType,
) -> Result<ValidatedProviderSchemaWitness, ProviderSchemaValidationError> {
    ValidatedProviderSchemaWitness::new(
        vec![
            ResourceSchema::new("test.Contact")
                .attribute(AttributeSchema::optional("email", declaration).build())
                .validate()
                .unwrap(),
        ],
        ValidatedConfigSchema::validate(BTreeMap::new()).unwrap(),
        BTreeMap::new(),
        vec![],
    )
}

#[test]
fn email_with_member_length_admits() {
    let email = types::email();
    let member = AttributeType::string_member(
        email.string_identity_layer().unwrap().clone(),
        StringMemberConstraints::new(None, Some((Some(3), Some(80)))),
    );
    assert!(provider(member).is_ok());
}

#[test]
fn email_with_altered_identity_constraints_still_collides() {
    let email = types::email();
    let StringIdentityParts {
        identity,
        pattern,
        length: _length,
        validate,
        to_dsl,
    } = email.string_identity_layer().unwrap().parts();
    let altered = AttributeType::refined_string_with_validator(
        identity.unwrap().clone(),
        pattern.cloned(),
        Some((Some(3), Some(80))),
        validate.unwrap().clone(),
        to_dsl.cloned(),
    );
    assert!(matches!(
        provider(altered),
        Err(ProviderSchemaValidationError::BuiltinTypeCollision { .. })
    ));
}

fn text(value: &str) -> carina_core::resource::Value {
    carina_core::resource::Value::Concrete(carina_core::resource::ConcreteValue::String(
        value.into(),
    ))
}

#[test]
fn string_layers_report_only_the_first_failure_then_run_semantics() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let ty = AttributeType::string_member(
        StringIdentityLayer::with_validator(
            TypeIdentity::bare("Layered"),
            Some(PatternSpec::new("[a-z]+", PatternMatch::Full)),
            Some((Some(2), Some(12))),
            validator(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            None,
        ),
        StringMemberConstraints::new(
            Some(PatternSpec::new("a.*", PatternMatch::Full)),
            Some((Some(3), Some(6))),
        ),
    );
    let schema = ValidatedConfigSchema::validate(BTreeMap::from([("value".into(), ty)])).unwrap();
    let ty = schema.rootless_type("value").unwrap();
    for (value, expected) in [
        ("B", "[a-z]+"),
        ("a", "length"),
        ("bb", "a.*"),
        ("ab", "length"),
        ("abcdefg", "length"),
    ] {
        let errors = ty.validate_collect(&text(value));
        assert_eq!(errors.len(), 1, "{value}: {errors:?}");
        assert!(
            errors[0].1.to_string().contains(expected),
            "{value}: {:?}",
            errors
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(ty.validate_collect(&text("abc")).is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn int_identity_and_member_ranges_are_both_enforced() {
    use carina_core::resource::{ConcreteValue, Value};
    let ty = AttributeType::int_member(
        IntIdentityLayer::new(
            Some(TypeIdentity::bare("Count")),
            Some((Some(1), Some(100))),
        ),
        IntMemberConstraints::new(Some((Some(10), Some(20)))),
    );
    let schema = ValidatedConfigSchema::validate(BTreeMap::from([("value".into(), ty)])).unwrap();
    let ty = schema.rootless_type("value").unwrap();
    for value in [0, 5, 21, 101] {
        let value: Value = Value::Concrete(ConcreteValue::Int(value));
        assert_eq!(ty.validate_collect(&value).len(), 1);
    }
    let value: Value = Value::Concrete(ConcreteValue::Int(15));
    assert!(ty.validate_collect(&value).is_empty());
}

#[test]
fn protocol_round_trip_preserves_both_layers_and_builtin_semantics() {
    use carina_core::protocol_convert::{config_schema_to_protocol, protocol_to_config_schema};
    let values = BTreeMap::from([
        (
            "email".into(),
            AttributeType::string_member(
                types::email().string_identity_layer().unwrap().clone(),
                StringMemberConstraints::new(None, Some((Some(3), Some(80)))),
            ),
        ),
        (
            "name".into(),
            AttributeType::string_member(
                StringIdentityLayer::new(
                    Some(TypeIdentity::bare("Name")),
                    Some(PatternSpec::new("[a-z]+", PatternMatch::Full)),
                    Some((Some(1), Some(20))),
                    None,
                ),
                StringMemberConstraints::new(
                    Some(PatternSpec::new("a.*", PatternMatch::Full)),
                    Some((Some(2), Some(10))),
                ),
            ),
        ),
        (
            "count".into(),
            AttributeType::int_member(
                IntIdentityLayer::new(
                    Some(TypeIdentity::bare("Count")),
                    Some((Some(1), Some(100))),
                ),
                IntMemberConstraints::new(Some((Some(10), Some(20)))),
            ),
        ),
    ]);
    let schema = ValidatedConfigSchema::validate(values).unwrap();
    let wire = config_schema_to_protocol(&schema).unwrap();
    let encoded = serde_json::to_value(&wire).unwrap();
    let restored = protocol_to_config_schema(wire).unwrap();
    assert_eq!(
        serde_json::to_value(config_schema_to_protocol(&restored).unwrap()).unwrap(),
        encoded
    );
    assert!(
        !restored
            .rootless_type("email")
            .unwrap()
            .validate_collect(&text("abc"))
            .is_empty()
    );
    assert!(
        restored
            .rootless_type("email")
            .unwrap()
            .validate_collect(&text("a@b.com"))
            .is_empty()
    );
    assert_eq!(
        restored
            .rootless_type("name")
            .unwrap()
            .validate_collect(&text("bb"))
            .len(),
        1
    );
    assert!(matches!(
        restored.parts()["name"].raw_shape(),
        RawShape::String { .. }
    ));
}

#[test]
fn admission_rejects_empty_member_bounds_and_identity_intersections() {
    for (identity, member) in [
        (None, (Some(8), Some(2))),
        (Some((Some(1), Some(4))), (Some(8), Some(12))),
    ] {
        let string = AttributeType::string_member(
            StringIdentityLayer::new(None, None, identity, None),
            StringMemberConstraints::new(None, Some(member)),
        );
        let error = ValidatedConfigSchema::validate(BTreeMap::from([("name".into(), string)]))
            .expect_err("empty String member bounds must fail schema admission")
            .to_string();
        assert!(
            error.contains("empty intersection of scalar constraints"),
            "{error}"
        );
        assert!(error.contains("name"), "{error}");
    }
    for (identity, member) in [
        (None, (Some(8), Some(2))),
        (Some((Some(-4), Some(4))), (Some(8), Some(12))),
    ] {
        let int = AttributeType::int_member(
            IntIdentityLayer::new(None, identity),
            IntMemberConstraints::new(Some(member)),
        );
        let error = ResourceSchema::new("test.Count")
            .attribute(AttributeSchema::optional("count", int).build())
            .validate()
            .expect_err("empty Int member bounds must fail schema admission")
            .to_string();
        assert!(
            error.contains("empty intersection of scalar constraints"),
            "{error}"
        );
        assert!(error.contains("count"), "{error}");
    }
}

#[test]
fn anonymous_member_strings_are_custom_and_display_their_constraints() {
    for (pattern, length, expected) in [
        (
            Some(PatternSpec::new("[a-z]+", PatternMatch::Full)),
            None,
            "String(pattern)",
        ),
        (None, Some((Some(2), Some(4))), "String(len: 2..=4)"),
    ] {
        let ty = AttributeType::string_member(
            StringIdentityLayer::new(None, None, None, None),
            StringMemberConstraints::new(pattern, length),
        );
        assert!(ty.is_string_based_custom());
        assert_eq!(ty.type_name(), expected);
        let schema =
            ValidatedConfigSchema::validate(BTreeMap::from([("value".into(), ty)])).unwrap();
        let ty = schema.rootless_type("value").unwrap();
        assert!(ty.validate_collect(&text("abc")).is_empty());
        assert_eq!(ty.validate_collect(&text("X")).len(), 1);
    }
}
