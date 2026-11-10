use carina_core::schema::{
    AttributeSchema, AttributeType, PatternMatch, PatternSpec, ProviderSchemaValidationError,
    ResourceSchema, StringIdentityLayer, StringMemberConstraints, TypeIdentity,
    ValidatedConfigSchema, ValidatedProviderSchemaWitness,
};
use std::collections::BTreeMap;

#[test]
fn round15_float_identity_ranges_must_agree_within_one_owner() {
    let rate = |max: f64| {
        AttributeType::refined_float(
            Some(TypeIdentity::bare("Rate")),
            Some((
                Some(0.0_f64.try_into().unwrap()),
                Some(max.try_into().unwrap()),
            )),
        )
    };
    let resource = ResourceSchema::new("Rates")
        .attribute(AttributeSchema::optional("rate", rate(100.0)).build())
        .validate()
        .unwrap();
    let config =
        ValidatedConfigSchema::validate(BTreeMap::from([("rate".into(), rate(10.0))])).unwrap();
    assert!(matches!(
        ValidatedProviderSchemaWitness::new(vec![resource], config, Default::default(), vec![]),
        Err(ProviderSchemaValidationError::ConflictingIdentityDeclarations { .. })
    ));
}

#[test]
fn round15_empty_bounds_do_not_hide_either_pattern_error() {
    let invalid = AttributeType::string_member(
        StringIdentityLayer::new(
            None,
            Some(PatternSpec::new("[", PatternMatch::Full)),
            Some((Some(1), Some(10))),
            None,
        ),
        StringMemberConstraints::new(
            Some(PatternSpec::new("(", PatternMatch::Full)),
            Some((Some(8), Some(2))),
        ),
    );
    let error = ResourceSchema::new("Invalid")
        .attribute(AttributeSchema::optional("name", invalid).build())
        .validate()
        .unwrap_err();
    assert!(
        error
            .issues()
            .any(|(_, message)| message.contains("empty intersection")),
        "{error}"
    );
    assert_eq!(error.pattern_errors().count(), 2, "{error}");
}

#[test]
fn round15_member_failures_name_the_slot_layer_and_preserve_the_typed_cause() {
    use carina_core::resource::{ConcreteValue, Value};
    let string = AttributeType::string_member(
        StringIdentityLayer::new(
            Some(TypeIdentity::from_dotted("aws.Name")),
            Some(PatternSpec::new("[a-z]+", PatternMatch::Full)),
            Some((Some(1), Some(64))),
            None,
        ),
        StringMemberConstraints::new(
            Some(PatternSpec::new("a.*", PatternMatch::Full)),
            Some((Some(1), Some(10))),
        ),
    );
    let schema =
        ValidatedConfigSchema::validate(BTreeMap::from([("name".into(), string)])).unwrap();
    let cases: [(&str, &str, Value); 2] = [
        (
            "name",
            "aws.Name",
            Value::Concrete(ConcreteValue::String("bad".into())),
        ),
        (
            "name",
            "aws.Name",
            Value::Concrete(ConcreteValue::String("a".repeat(50).into())),
        ),
    ];
    for (key, identity, value) in cases {
        let errors = schema.rootless_type(key).unwrap().validate_collect(&value);
        assert_eq!(errors.len(), 1);
        let error = &errors[0].1;
        let message = error.to_string();
        assert!(
            message.contains("slot member constraint") && message.contains(identity),
            "{message}"
        );
        assert!(
            matches!(
                error,
                carina_core::schema::TypeError::PatternMismatch { .. }
                    | carina_core::schema::TypeError::LengthOutOfRange { .. }
            ),
            "the structural category and fields remain typed"
        );
    }
    for value in ["X", "a".repeat(65).as_str()] {
        let value: Value = Value::Concrete(ConcreteValue::String(value.into()));
        let errors = schema
            .rootless_type("name")
            .unwrap()
            .validate_collect(&value);
        assert_eq!(errors.len(), 1);
        assert!(
            !errors[0].1.to_string().contains("member"),
            "identity fails first"
        );
    }
}
