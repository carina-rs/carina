use super::*;

fn text(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}

fn map(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Concrete(ConcreteValue::Map(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    ))
}

fn findings(ty: AttributeType, value: &Value) -> Vec<(String, String)> {
    Schema::flat(ty)
        .validate_collect(value)
        .into_iter()
        .map(|(path, error)| (path.to_string(), error.to_string()))
        .collect()
}

#[test]
fn two_unknown_keys_retain_each_path_and_message() {
    let ty = AttributeType::struct_(
        "Versioning",
        vec![StructField::optional("status", AttributeType::string()).build()],
    );
    let value = map([("statuus", text("Enabled")), ("mfa", text("Disabled"))]);

    assert_eq!(
        findings(ty, &value),
        vec![
            (
                "statuus".into(),
                "Unknown field 'statuus' in Versioning, did you mean 'status'?".into(),
            ),
            ("mfa".into(), "Unknown field 'mfa' in Versioning".into()),
        ]
    );
}

#[test]
fn single_unknown_key_stays_at_the_written_field() {
    let ty = AttributeType::struct_(
        "Outer",
        vec![
            StructField::required(
                "nested",
                AttributeType::struct_(
                    "Versioning",
                    vec![StructField::optional("status", AttributeType::string()).build()],
                ),
            )
            .build(),
        ],
    );
    let value = map([("nested", map([("statuus", text("Enabled"))]))]);

    assert_eq!(
        findings(ty, &value),
        vec![(
            "nested.statuus".into(),
            "Unknown field 'statuus' in Versioning, did you mean 'status'?".into(),
        )]
    );
}

#[test]
fn unknown_key_and_missing_required_field_are_both_retained() {
    let ty = AttributeType::struct_(
        "Versioning",
        vec![StructField::required("status", AttributeType::string()).build()],
    );
    let value = map([("statuus", text("Enabled"))]);

    assert_eq!(
        findings(ty, &value),
        vec![
            (
                "status".into(),
                "Required attribute 'status' is missing".into(),
            ),
            (
                "statuus".into(),
                "Unknown field 'statuus' in Versioning, did you mean 'status'?".into(),
            ),
        ]
    );
}

#[test]
fn nested_provider_populated_write_keeps_the_leaf_path_and_error() {
    let ty = AttributeType::struct_(
        "Config",
        vec![
            StructField::optional(
                "destination",
                AttributeType::struct_(
                    "Destination",
                    vec![StructField::provider_populated("arn", AttributeType::string()).build()],
                ),
            )
            .build(),
        ],
    );
    let value = map([("destination", map([("arn", text("provider-arn"))]))]);
    let errors = Schema::flat(ty.clone()).validate_collect(&value);

    assert!(matches!(&errors[..], [(_, TypeError::ReadOnlyAttribute { name })] if name == "arn"));
    assert_eq!(
        findings(ty, &value),
        vec![(
            "destination.arn".into(),
            "Attribute 'arn' is populated by the provider and cannot be set".into(),
        )]
    );
}

#[test]
fn map_field_group_and_unknown_key_keep_the_coarse_value_error() {
    let choice = AttributeType::struct_with_field_groups(
        "Choice",
        vec![
            StructField::optional("first", AttributeType::string()).build(),
            StructField::optional("second", AttributeType::string()).build(),
        ],
        vec![FieldGroup::exactly_one(["first", "second"]).unwrap()],
    );
    let ty = AttributeType::map(choice);
    let value = map([("primary", map([("other", text("value"))]))]);
    let errors = Schema::flat(ty.clone()).validate_collect(&value);

    assert!(
        matches!(&errors[..], [(_, TypeError::FieldGroupViolation { .. }), (_, TypeError::MapValueError { key, inner })]
        if key == "primary" && matches!(inner.as_ref(), TypeError::UnknownStructField { field, .. } if field == "other"))
    );
    assert_eq!(
        findings(ty, &value),
        vec![
            (
                "primary".into(),
                "primary: exactly one of [first, second] must be set".into(),
            ),
            (
                String::new(),
                "Map value for key 'primary': Unknown field 'other' in Choice".into(),
            ),
        ]
    );
}
