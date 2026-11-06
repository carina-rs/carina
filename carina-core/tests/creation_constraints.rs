use carina_core::protocol_convert::{protocol_to_resource_schema, resource_schema_to_protocol};
use carina_core::schema::{
    AttributeSchema, AttributeType, CharSet, InputPath, InputPredicate, InputRule, InputRuleCode,
    InputRuleError, NonEmptyString, ResourceSchema, StructField,
};

fn rule(path: &str, code: &str, predicate: InputPredicate<String>) -> InputRule {
    InputRule::value(
        carina_core::schema::InputRuleScope::CreateOnly,
        InputPath::new(path).unwrap(),
        InputRuleCode::new(code).unwrap(),
        predicate,
    )
}

fn schema(rules: Vec<InputRule>) -> ResourceSchema {
    rules.into_iter().fold(
        ResourceSchema::new("test.Bucket")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()),
        |schema, rule| schema.input_rule(rule),
    )
}

#[path = "support/creation.rs"]
#[allow(dead_code)]
mod fixture;

#[derive(Debug)]
struct CheckedRuleFixture(carina_core::schema::ValidatedResourceSchema);

impl CheckedRuleFixture {
    fn evaluate(&self, value: &str) -> Result<(), carina_core::schema::InputViolation> {
        evaluate_schema(&self.0, value)
    }
}

fn evaluate_schema(
    schema: &carina_core::schema::ValidatedResourceSchema,
    value: &str,
) -> Result<(), carina_core::schema::InputViolation> {
    use carina_core::{effect::Effect, input_rules::InputVerdict, plan::Plan, resource::*};
    let mut registry = carina_core::schema::SchemaRegistry::new();
    registry.insert("mock", schema.clone());
    let resource = Resource::with_provider("mock", "test.Bucket", "example", None)
        .with_attribute("name", Value::Concrete(ConcreteValue::String(value.into())));
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        ResolvedResource::resolve_raw_fixture(resource, &registry).unwrap(),
    ));
    let finalized = fixture::finalize(plan);
    let current = std::collections::HashMap::new();
    for site in finalized.write_sites(&registry, &current) {
        let site = site.unwrap();
        for rule in site.applicable_rules() {
            match rule.evaluate() {
                InputVerdict::Proven => {}
                InputVerdict::Violation(violation) => return Err(violation),
                InputVerdict::Undecided => panic!("literal fixture must be decidable"),
            }
        }
    }
    Ok(())
}

fn compiled(predicate: InputPredicate<String>) -> CheckedRuleFixture {
    CheckedRuleFixture(
        schema(vec![rule("name", "test.rule", predicate)])
            .validate()
            .unwrap(),
    )
}

fn token(value: &str) -> NonEmptyString {
    NonEmptyString::new(value).unwrap()
}

#[test]
fn creation_primitive_vectors() {
    let vectors = [
        (
            InputPredicate::Length {
                min: Some(2),
                max: Some(3),
            },
            vec!["é😀", "abc"],
            vec!["a", "abcd"],
        ),
        (
            InputPredicate::Length {
                min: None,
                max: Some(0),
            },
            vec![""],
            vec!["a"],
        ),
        (
            InputPredicate::Length {
                min: Some(2),
                max: None,
            },
            vec!["ab", "abc"],
            vec!["", "a"],
        ),
        (
            InputPredicate::AllowedChars(CharSet::ascii_lowercase()),
            vec!["", "abc"],
            vec!["A", "é"],
        ),
        (
            InputPredicate::FirstChar(CharSet::ascii_alphanumeric()),
            vec!["a-", "9."],
            vec!["", "-a", "éa"],
        ),
        (
            InputPredicate::LastChar(CharSet::ascii_alphanumeric()),
            vec!["-a", ".9"],
            vec!["", "a-", "aé"],
        ),
        (
            InputPredicate::NoSubstring(token("..")),
            vec!["", "a.b"],
            vec!["..", "a..b"],
        ),
        (
            InputPredicate::NoPrefix(token("xn--")),
            vec!["", "a-xn--"],
            vec!["xn--", "xn--bucket"],
        ),
        (
            InputPredicate::NoSuffix(token("--x-s3")),
            vec!["", "--x-s3-a"],
            vec!["--x-s3", "a--x-s3"],
        ),
        (
            InputPredicate::NoAdjacent {
                left: CharSet::ascii_lowercase(),
                right: CharSet::ascii_digits(),
            },
            vec!["", "a", "1a", "a-1"],
            vec!["a1", "-z9-"],
        ),
        (
            InputPredicate::NotIpv4,
            vec!["", "256.1.2.3", "1.2.3", "01.2.3.4"],
            vec!["0.0.0.0", "192.168.1.1", "255.255.255.255"],
        ),
        (
            InputPredicate::Matches("^\\d+$".into()),
            vec!["123"],
            vec!["", "١٢٣", "x123"],
        ),
        (
            InputPredicate::NotMatches("bad".into()),
            vec!["", "good"],
            vec!["bad", "a-bad-b"],
        ),
    ];
    for (predicate, accepted, rejected) in vectors {
        let rule = compiled(predicate);
        for value in accepted {
            assert!(rule.evaluate(value).is_ok(), "{rule:?}: {value:?}");
        }
        for value in rejected {
            let violation = rule.evaluate(value).expect_err(value);
            assert_eq!(violation.path().as_str(), "name");
            assert_eq!(violation.code().as_str(), "test.rule");
        }
    }
}

#[test]
fn creation_ipv4_uses_strict_decimal_dotted_quads() {
    let rule = compiled(InputPredicate::NotIpv4);
    for value in ["1.2.3.4", "0.10.100.255"] {
        assert!(rule.evaluate(value).is_err());
    }
    for value in [
        "01.2.3.4",
        "1.02.3.4",
        "000.0.0.0",
        "256.0.0.1",
        "999999999999.1.2.3",
        "1.2.3.4.5",
        "1.2.3.4.",
        ".1.2.3.4",
        "1..3.4",
        "1.2.3.-4",
        "+1.2.3.4",
        "１.2.3.4",
        "1.2.3.4\n",
        " 1.2.3.4",
        "0x1.2.3.4",
    ] {
        assert!(rule.evaluate(value).is_ok(), "{value:?}");
    }
}

fn s3_rules() -> Vec<InputRule> {
    let mut rules = vec![
        rule(
            "name",
            "s3.length",
            InputPredicate::Length {
                min: Some(3),
                max: Some(63),
            },
        ),
        rule(
            "name",
            "s3.chars",
            InputPredicate::AllowedChars(
                CharSet::new([('a', 'z'), ('0', '9'), ('.', '.'), ('-', '-')]).unwrap(),
            ),
        ),
        rule(
            "name",
            "s3.first",
            InputPredicate::FirstChar(CharSet::ascii_alphanumeric()),
        ),
        rule(
            "name",
            "s3.last",
            InputPredicate::LastChar(CharSet::ascii_alphanumeric()),
        ),
        rule(
            "name",
            "s3.dots",
            InputPredicate::NoAdjacent {
                left: CharSet::new([('.', '.')]).unwrap(),
                right: CharSet::new([('.', '.')]).unwrap(),
            },
        ),
        rule("name", "s3.ipv4", InputPredicate::NotIpv4),
    ];
    for (index, prefix) in ["xn--", "sthree-", "amzn-s3-demo-"].into_iter().enumerate() {
        rules.push(rule(
            "name",
            &format!("s3.prefix{index}"),
            InputPredicate::NoPrefix(token(prefix)),
        ));
    }
    for (index, suffix) in ["-s3alias", "--ol-s3", ".mrap", "--x-s3", "--table-s3"]
        .into_iter()
        .enumerate()
    {
        rules.push(rule(
            "name",
            &format!("s3.suffix{index}"),
            InputPredicate::NoSuffix(token(suffix)),
        ));
    }
    rules
}

#[test]
fn creation_s3_rule_boundaries_and_reserved_names() {
    let schema = schema(s3_rules()).validate().unwrap();
    let accepted = |value: &str| evaluate_schema(&schema, value).is_ok();
    for value in ["abc", "a.b-c9", "bucket-an", &"a".repeat(63)] {
        assert!(accepted(value), "{value}");
    }
    for value in [
        "ab",
        &"a".repeat(64),
        "aBc",
        "a_b",
        "-abc",
        "abc.",
        "a..b",
        "192.168.0.1",
        "xn--bucket",
        "sthree-bucket",
        "amzn-s3-demo-bucket",
        "bucket-s3alias",
        "bucket--ol-s3",
        "bucket.mrap",
        "bucket--x-s3",
        "bucket--table-s3",
    ] {
        assert!(!accepted(value), "{value}");
    }
}

#[test]
fn creation_charset_is_nonempty_canonical_and_unicode_scalar_based() {
    let set = CharSet::new([('d', 'f'), ('a', 'c'), ('b', 'e'), ('😀', '🙏')]).unwrap();
    assert_eq!(set.ranges(), &[('a', 'f'), ('😀', '🙏')]);
    assert!(set.contains('😀'));
    assert!(!set.contains('g'));
    assert_eq!(CharSet::new([]).unwrap_err(), InputRuleError::EmptyCharSet);
    assert_eq!(
        CharSet::new([('z', 'a')]).unwrap_err(),
        InputRuleError::InvertedCharRange
    );
    assert!(NonEmptyString::new("").is_err());
    for code in [
        "", "S3.name", "s3..name", ".name", "s3.name.", "s3.name-", "a\n",
    ] {
        assert!(InputRuleCode::new(code).is_err(), "{code:?}");
    }
    for path in [
        "", ".name", "name.", "a..b", "name[0]", " name", "a.*", "a\nb",
    ] {
        assert!(InputPath::new(path).is_err(), "{path:?}");
    }
}

#[test]
fn creation_admission_rejects_invalid_paths_types_bounds_and_duplicates() {
    for (path, expected) in [
        ("missing", InputRuleError::UnknownPath),
        ("output", InputRuleError::ProviderPopulatedPath),
        ("count", InputRuleError::NonStringPath),
    ] {
        let error = schema(vec![rule(path, "test.rule", InputPredicate::NotIpv4)])
            .attribute(
                AttributeSchema::provider_populated("output", AttributeType::string()).build(),
            )
            .attribute(AttributeSchema::optional("count", AttributeType::int()).build())
            .validate()
            .unwrap_err();
        assert!(
            error
                .input_rule_errors()
                .any(|(_, error)| *error == expected)
        );
    }
    for (predicate, expected) in [
        (
            InputPredicate::Length {
                min: None,
                max: None,
            },
            InputRuleError::EmptyLengthBounds,
        ),
        (
            InputPredicate::Length {
                min: Some(4),
                max: Some(3),
            },
            InputRuleError::InvertedLengthBounds,
        ),
    ] {
        let error = schema(vec![rule("name", "test.rule", predicate)])
            .validate()
            .unwrap_err();
        assert!(
            error
                .input_rule_errors()
                .any(|(_, error)| *error == expected)
        );
    }
    let same = rule("name", "test.rule", InputPredicate::NotIpv4);
    let error = schema(vec![same.clone(), same]).validate().unwrap_err();
    assert!(
        error
            .input_rule_errors()
            .any(|(_, error)| *error == InputRuleError::DuplicateRule)
    );
    let error = schema(vec![rule(
        "name",
        "test.regex",
        InputPredicate::Matches("[".into()),
    )])
    .validate()
    .unwrap_err();
    assert_eq!(error.pattern_errors().count(), 1);
}

#[test]
fn creation_nested_paths_follow_refs_and_inherit_provider_populated_mode() {
    let nested = AttributeType::struct_(
        "Nested",
        vec![StructField::optional("name", AttributeType::string()).build()],
    );
    for provider_populated in [false, true] {
        let attribute = if provider_populated {
            AttributeSchema::provider_populated("nested", AttributeType::ref_("Nested")).build()
        } else {
            AttributeSchema::optional("nested", AttributeType::ref_("Nested"))
                .with_block_name("alias")
                .build()
        };
        let result = ResourceSchema::new("test.Nested")
            .attribute(attribute)
            .with_def("Nested", nested.clone())
            .input_rule(rule("nested.name", "test.name", InputPredicate::NotIpv4))
            .validate();
        if provider_populated {
            assert!(
                result
                    .unwrap_err()
                    .input_rule_errors()
                    .any(|(_, error)| *error == InputRuleError::ProviderPopulatedPath)
            );
        } else {
            assert!(result.is_ok());
        }
    }
    let error = ResourceSchema::new("test.Nested")
        .attribute(
            AttributeSchema::optional("nested", nested)
                .with_block_name("alias")
                .build(),
        )
        .input_rule(rule("alias.name", "test.name", InputPredicate::NotIpv4))
        .validate()
        .unwrap_err();
    assert!(
        error
            .input_rule_errors()
            .any(|(_, error)| *error == InputRuleError::UnknownPath)
    );
}

#[test]
fn creation_protocol_round_trip_retains_every_primitive_and_empty_collection() {
    let mut rules = s3_rules();
    rules.extend([
        rule(
            "name",
            "test.substring",
            InputPredicate::NoSubstring(token("secret")),
        ),
        rule(
            "name",
            "test.match",
            InputPredicate::Matches("^\\w+$".into()),
        ),
        rule(
            "name",
            "test.not_match",
            InputPredicate::NotMatches("forbidden".into()),
        ),
    ]);
    for rules in [vec![], rules] {
        let original = schema(rules).validate().unwrap();
        let protocol = resource_schema_to_protocol(&original).unwrap();
        let json = serde_json::to_value(&protocol).unwrap();
        assert!(json["input_rules"].is_array());
        let decoded_protocol = serde_json::from_value(json).unwrap();
        let decoded = protocol_to_resource_schema(&decoded_protocol).unwrap();
        assert_eq!(
            original.as_schema().input_rules(),
            decoded.as_schema().input_rules()
        );
    }
}

#[test]
fn creation_violation_diagnostics_do_not_contain_input_values() {
    let rule = compiled(InputPredicate::AllowedChars(CharSet::ascii_digits()));
    let violation = rule.evaluate("very-secret-value").unwrap_err();
    assert!(!violation.to_string().contains("very-secret-value"));
    assert!(!format!("{violation:?}").contains("very-secret-value"));
}

#[test]
fn creation_protocol_admission_preserves_typed_declaration_errors() {
    use std::error::Error;
    let original = schema(vec![]).validate().unwrap();
    let base = serde_json::to_value(resource_schema_to_protocol(&original).unwrap()).unwrap();
    for (rule, expected) in [
        (
            serde_json::json!({"scope":"create_only", "target":{"kind":"value","path":"name"}, "code":"INVALID", "predicate":{"kind":"not_ipv4"}}),
            InputRuleError::InvalidCode,
        ),
        (
            serde_json::json!({"scope":"create_only", "target":{"kind":"value","path":"name[0]"}, "code":"test.rule", "predicate":{"kind":"not_ipv4"}}),
            InputRuleError::InvalidPath,
        ),
        (
            serde_json::json!({"scope":"create_only", "target":{"kind":"value","path":"name"}, "code":"test.rule", "predicate":{"kind":"no_prefix", "value":""}}),
            InputRuleError::EmptyToken,
        ),
        (
            serde_json::json!({"scope":"create_only", "target":{"kind":"value","path":"name"}, "code":"test.rule", "predicate":{"kind":"allowed_chars", "value":{"ranges":[]}}}),
            InputRuleError::EmptyCharSet,
        ),
        (
            serde_json::json!({"scope":"create_only", "target":{"kind":"value","path":"name"}, "code":"test.rule", "predicate":{"kind":"first_char", "value":{"ranges":[{"start":122,"end":97}]}}}),
            InputRuleError::InvertedCharRange,
        ),
        (
            serde_json::json!({"scope":"create_only", "target":{"kind":"value","path":"name"}, "code":"test.rule", "predicate":{"kind":"last_char", "value":{"ranges":[{"start":55296,"end":57343}]}}}),
            InputRuleError::InvalidUnicodeScalar,
        ),
        (
            serde_json::json!({"scope":"create_only", "target":{"kind":"value","path":"name"}, "code":"test.rule", "predicate":{"kind":"last_char", "value":{"ranges":[{"start":1114112,"end":1114112}]}}}),
            InputRuleError::InvalidUnicodeScalar,
        ),
    ] {
        let mut json = base.clone();
        json["input_rules"] = serde_json::json!([rule]);
        let wire = serde_json::from_value(json).unwrap();
        let error = protocol_to_resource_schema(&wire).unwrap_err();
        assert_eq!(
            error.source().unwrap().downcast_ref::<InputRuleError>(),
            Some(&expected)
        );
    }
}

#[test]
fn creation_admission_accepts_string_refinements_enums_and_reference_leaves() {
    use carina_core::schema::TypeIdentity;
    let identity = TypeIdentity::new(Some("test"), ["Bucket"], "Name");
    for ty in [
        AttributeType::refined_string(
            Some(identity.clone()),
            Some("^.+$".into()),
            Some((Some(1), None)),
            None,
        ),
        AttributeType::reference(identity.clone(), None, None),
        AttributeType::enum_(
            identity,
            Some(vec!["one".into(), "two".into()]),
            vec![],
            None,
            None,
        ),
        AttributeType::union(vec![
            AttributeType::string(),
            AttributeType::refined_string(None, None, Some((Some(2), None)), None),
        ]),
    ] {
        ResourceSchema::new("test.Bucket")
            .attribute(AttributeSchema::optional("name", ty).build())
            .input_rule(rule("name", "test.rule", InputPredicate::NotIpv4))
            .validate()
            .unwrap();
    }
}

#[test]
fn creation_charsets_canonicalize_at_unicode_scalar_boundaries() {
    let set = CharSet::new([
        ('\u{e000}', '\u{e000}'),
        ('\u{d7ff}', '\u{d7ff}'),
        ('\u{10ffff}', '\u{10ffff}'),
    ])
    .unwrap();
    assert_eq!(
        set.ranges(),
        &[('\u{d7ff}', '\u{e000}'), ('\u{10ffff}', '\u{10ffff}')]
    );
    for ch in ['\u{d7ff}', '\u{e000}', '\u{10ffff}'] {
        assert!(set.contains(ch));
    }
    assert!(!set.contains('\u{d7fe}'));
    assert!(!set.contains('\u{e001}'));
    let rule = compiled(InputPredicate::AllowedChars(set));
    assert!(rule.evaluate("\u{d7ff}\u{e000}\u{10ffff}").is_ok());
}
