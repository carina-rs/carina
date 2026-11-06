use super::*;
use crate::{
    resource::{InterpolationPart, Resource},
    schema::{AttributeSchema, AttributeType, CharSet, ResourceSchema},
};

fn literal(s: &str) -> Value {
    Value::Concrete(ConcreteValue::String(s.into()))
}

fn template(parts: &[&str]) -> Value {
    Value::Deferred(DeferredValue::Interpolation(
        parts
            .iter()
            .map(|s| InterpolationPart::Expr(literal(s)))
            .collect(),
    ))
}

fn estimate(predicate: InputPredicate<String>, value: Value) -> InputVerdict {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Target")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .input_rule(InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("test.rule").unwrap(),
                predicate,
            ))
            .validate()
            .unwrap(),
    );
    let resource =
        Resource::with_provider("mock", "Target", "target", None).with_attribute("name", value);
    let site = classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
        .unwrap();
    site.applicable_rules().next().unwrap().evaluate()
}

#[test]
fn estimator_literal_templates_decide_primitives() {
    use InputPredicate::*;
    let cases = [
        (
            Length {
                min: Some(3),
                max: Some(3),
            },
            vec!["é", "日", "😀"],
            true,
        ),
        (
            Length {
                min: None,
                max: Some(2),
            },
            vec!["é", "日", "😀"],
            false,
        ),
        (
            AllowedChars(CharSet::ascii_lowercase()),
            vec!["a", "bc"],
            true,
        ),
        (
            AllowedChars(CharSet::ascii_lowercase()),
            vec!["ab", "C"],
            false,
        ),
        (FirstChar(CharSet::ascii_lowercase()), vec!["", "abc"], true),
        (
            FirstChar(CharSet::ascii_lowercase()),
            vec!["", "Abc"],
            false,
        ),
        (LastChar(CharSet::ascii_lowercase()), vec!["abc", ""], true),
        (LastChar(CharSet::ascii_lowercase()), vec!["abC", ""], false),
        (
            NoPrefix(crate::schema::NonEmptyString::new("abc").unwrap()),
            vec!["ab", "d"],
            true,
        ),
        (
            NoPrefix(crate::schema::NonEmptyString::new("abc").unwrap()),
            vec!["ab", "c"],
            false,
        ),
        (
            ForbiddenPrefix {
                prefix: crate::schema::NonEmptyString::new("aws:").unwrap(),
                ignore_ascii_case: true,
            },
            vec!["AW", "S:"],
            false,
        ),
        (
            NoSuffix(crate::schema::NonEmptyString::new("abc").unwrap()),
            vec!["d", "bc"],
            true,
        ),
        (
            NoSuffix(crate::schema::NonEmptyString::new("abc").unwrap()),
            vec!["a", "bc"],
            false,
        ),
        (
            NoSubstring(crate::schema::NonEmptyString::new("abc").unwrap()),
            vec!["x", "ab", "", "c", "y"],
            false,
        ),
        (
            NoSubstring(crate::schema::NonEmptyString::new("abc").unwrap()),
            vec!["x", "abd", "y"],
            true,
        ),
        (
            NoAdjacent {
                left: CharSet::ascii_lowercase(),
                right: CharSet::ascii_digits(),
            },
            vec!["a", "1"],
            false,
        ),
        (
            NoAdjacent {
                left: CharSet::ascii_lowercase(),
                right: CharSet::ascii_digits(),
            },
            vec!["1", "a"],
            true,
        ),
        (NotIpv4, vec!["192.", "168.1.1"], false),
        (NotIpv4, vec!["192.", "168.01.1"], true),
    ];
    for (predicate, parts, passes) in cases {
        let verdict = estimate(predicate.clone(), template(&parts));
        assert_eq!(
            matches!(verdict, InputVerdict::Proven),
            passes,
            "{predicate:?}: {verdict:?}"
        );
        assert_eq!(
            matches!(verdict, InputVerdict::Violation(_)),
            !passes,
            "{predicate:?}: {verdict:?}"
        );
    }
}

#[test]
fn estimator_unsupported_regex_stays_undecided() {
    assert_eq!(
        estimate(
            InputPredicate::Matches("^abc$".into()),
            template(&["ab", "c"])
        ),
        InputVerdict::Undecided
    );
}

use crate::{
    effect::deps::UnresolvedResource,
    resource::{AccessPath, DataSource, PathSegment, Subscript},
    schema::{NonEmptyString, StructField, TypeIdentity},
};

fn token(s: &str) -> NonEmptyString {
    NonEmptyString::new(s).unwrap()
}
fn reference() -> Value {
    Value::root_resource_ref("source", "value", vec![])
}
fn interpolation(parts: Vec<Value>) -> Value {
    Value::Deferred(DeferredValue::Interpolation(
        parts.into_iter().map(InterpolationPart::Expr).collect(),
    ))
}
fn refined(pattern: &str, min: u64, max: Option<u64>) -> AttributeType {
    AttributeType::refined_string(None, Some(pattern.into()), Some((Some(min), max)), None)
}

fn typed_estimate(
    predicate: InputPredicate<String>,
    value: Value,
    ty: AttributeType,
) -> InputVerdict {
    typed_target_estimate(predicate, value, ty, false, false)
}

fn typed_target_estimate(
    predicate: InputPredicate<String>,
    value: Value,
    ty: AttributeType,
    keys: bool,
    optional: bool,
) -> InputVerdict {
    let mut schemas = SchemaRegistry::new();
    let path = InputPath::new("name").unwrap();
    schemas.insert(
        "mock",
        ResourceSchema::new("Target")
            .attribute(
                AttributeSchema::optional(
                    "name",
                    if keys {
                        AttributeType::map(AttributeType::string())
                    } else {
                        AttributeType::string()
                    },
                )
                .build(),
            )
            .input_rule(InputRule::new(
                InputRuleScope::EveryWrite,
                if keys {
                    InputRuleTarget::MapKeys(path)
                } else {
                    InputRuleTarget::Value(path)
                },
                InputRuleCode::new("test.rule").unwrap(),
                predicate,
            ))
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(if optional {
                AttributeSchema::optional("value", ty).build()
            } else {
                AttributeSchema::provider_populated("value", ty).build()
            })
            .validate()
            .unwrap(),
    );
    let mut source = Resource::with_provider("mock", "Source", "source", None);
    source.binding = Some(crate::resource::absolute_binding_address_for_test("source"));
    let types = InputTypeContext::from_sources(
        &[UnresolvedResource::from_pre_resolve(source)],
        &[],
        std::iter::empty(),
        &[],
        &schemas,
    );
    let resource =
        Resource::with_provider("mock", "Target", "target", None).with_attribute("name", value);
    let mut site =
        classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
            .unwrap();
    site.types = Some(&types.facts);
    // Exercise the estimator for map literals too (a template effect has no
    // evaluated attributes). The classifier still owns the authored projection.
    if keys {
        site.attributes = None;
    }
    site.applicable_rules().next().unwrap().evaluate()
}

fn outcome(verdict: InputVerdict) -> &'static str {
    match verdict {
        InputVerdict::Proven => "proven",
        InputVerdict::Violation(_) => "violation",
        InputVerdict::Undecided => "undecided",
    }
}

#[test]
fn estimator_declared_types_cover_three_outcomes_per_primitive() {
    use InputPredicate::*;
    let cases = [
        (
            Length {
                min: Some(3),
                max: Some(5),
            },
            refined("^[a-z]+$", 3, Some(5)),
            "proven",
        ),
        (
            Length {
                min: Some(3),
                max: Some(5),
            },
            refined("^[a-z]+$", 6, Some(9)),
            "violation",
        ),
        (
            Length {
                min: Some(3),
                max: Some(5),
            },
            refined("^[a-z]+$", 1, Some(9)),
            "undecided",
        ),
        (
            AllowedChars(CharSet::ascii_lowercase()),
            refined("^[a-z]+$", 1, None),
            "proven",
        ),
        (
            AllowedChars(CharSet::ascii_lowercase()),
            refined("^[0-9]+$", 1, None),
            "violation",
        ),
        (
            AllowedChars(CharSet::ascii_lowercase()),
            refined("^[a-z0-9]+$", 1, None),
            "undecided",
        ),
        (
            FirstChar(CharSet::ascii_lowercase()),
            refined("^[a-z]+$", 1, None),
            "proven",
        ),
        (
            FirstChar(CharSet::ascii_lowercase()),
            refined("^[0-9]+$", 1, None),
            "violation",
        ),
        (
            FirstChar(CharSet::ascii_lowercase()),
            refined("^[a-z0-9]+$", 1, None),
            "undecided",
        ),
        (
            LastChar(CharSet::ascii_lowercase()),
            refined("^[a-z]+$", 1, None),
            "proven",
        ),
        (
            LastChar(CharSet::ascii_lowercase()),
            refined("^[0-9]+$", 1, None),
            "violation",
        ),
        (
            LastChar(CharSet::ascii_lowercase()),
            refined("^[a-z0-9]+$", 1, None),
            "undecided",
        ),
        (NoPrefix(token("aa")), refined("^[b]+$", 1, None), "proven"),
        (
            NoPrefix(token("aa")),
            refined("^[a]+$", 2, None),
            "violation",
        ),
        (
            NoPrefix(token("aa")),
            refined("^[ab]+$", 1, None),
            "undecided",
        ),
        (
            ForbiddenPrefix {
                prefix: token("AA"),
                ignore_ascii_case: true,
            },
            refined("^[b]+$", 1, None),
            "proven",
        ),
        (
            ForbiddenPrefix {
                prefix: token("AA"),
                ignore_ascii_case: true,
            },
            refined("^[aA]+$", 2, None),
            "violation",
        ),
        (
            ForbiddenPrefix {
                prefix: token("AA"),
                ignore_ascii_case: true,
            },
            refined("^[ab]+$", 1, None),
            "undecided",
        ),
        (NoSuffix(token("aa")), refined("^[b]+$", 1, None), "proven"),
        (
            NoSuffix(token("aa")),
            refined("^[a]+$", 2, None),
            "violation",
        ),
        (
            NoSuffix(token("aa")),
            refined("^[ab]+$", 1, None),
            "undecided",
        ),
        (
            NoSubstring(token("aa")),
            refined("^[b]+$", 1, None),
            "proven",
        ),
        (
            NoSubstring(token("aa")),
            refined("^[a]+$", 2, None),
            "violation",
        ),
        (
            NoSubstring(token("aa")),
            refined("^[ab]+$", 1, None),
            "undecided",
        ),
        (
            NoAdjacent {
                left: CharSet::ascii_lowercase(),
                right: CharSet::ascii_digits(),
            },
            refined("^[a-z]+$", 1, None),
            "proven",
        ),
        (
            NoAdjacent {
                left: CharSet::ascii_lowercase(),
                right: CharSet::ascii_lowercase(),
            },
            refined("^[a-z]+$", 2, None),
            "violation",
        ),
        (
            NoAdjacent {
                left: CharSet::ascii_lowercase(),
                right: CharSet::ascii_digits(),
            },
            refined("^[a-z0-9]+$", 2, None),
            "undecided",
        ),
        (NotIpv4, refined("^[a-z]+$", 1, None), "proven"),
        (
            NotIpv4,
            AttributeType::string_literal_set(vec!["1.2.3.4".into(), "255.0.0.0".into()]),
            "violation",
        ),
        (NotIpv4, refined("^[0-9.]+$", 7, Some(15)), "undecided"),
        (
            Matches(".*".into()),
            refined("^[a-z]+$", 1, None),
            "undecided",
        ),
        (
            NotMatches(".*".into()),
            refined("^[a-z]+$", 1, None),
            "undecided",
        ),
    ];
    for (predicate, ty, expected) in cases {
        assert_eq!(
            outcome(typed_estimate(predicate.clone(), reference(), ty)),
            expected,
            "{predicate:?}"
        );
    }
}

#[test]
fn estimator_nullable_endpoints_and_empty_endpoint_semantics() {
    let nullable = refined("^[a-z]*$", 0, None);
    assert_eq!(
        outcome(typed_estimate(
            InputPredicate::FirstChar(CharSet::ascii_lowercase()),
            reference(),
            nullable.clone()
        )),
        "undecided"
    );
    for predicate in [
        InputPredicate::FirstChar(CharSet::ascii_lowercase()),
        InputPredicate::LastChar(CharSet::ascii_lowercase()),
    ] {
        assert_eq!(
            outcome(estimate(predicate.clone(), template(&["", ""]))),
            "violation"
        );
        assert_eq!(
            outcome(typed_estimate(
                predicate.clone(),
                interpolation(vec![reference(), literal("x"), reference()]),
                nullable.clone()
            )),
            "proven"
        );
        assert_eq!(
            outcome(typed_estimate(
                predicate,
                interpolation(vec![reference(), literal("0"), reference()]),
                nullable.clone()
            )),
            "undecided"
        );
    }
}

#[test]
fn estimator_crossings_skip_multiple_nullable_parts_and_keep_mandatory_distinct() {
    for predicate in [
        InputPredicate::NoSubstring(token("abc")),
        InputPredicate::NoPrefix(token("abc")),
        InputPredicate::NoSuffix(token("abc")),
    ] {
        let expression = interpolation(vec![literal("a"), reference(), reference(), literal("bc")]);
        assert_eq!(
            outcome(typed_estimate(
                predicate.clone(),
                expression.clone(),
                refined("^[x]*$", 0, Some(2))
            )),
            "undecided",
            "{predicate:?}"
        );
        assert_eq!(
            outcome(typed_estimate(
                predicate.clone(),
                expression.clone(),
                refined("^[x]{0}$", 0, Some(0))
            )),
            "violation",
            "{predicate:?}"
        );
        assert_eq!(
            outcome(typed_estimate(
                predicate.clone(),
                expression,
                refined("^[x]+$", 1, Some(2))
            )),
            "proven",
            "{predicate:?}"
        );
    }
    let adjacency = InputPredicate::NoAdjacent {
        left: CharSet::ascii_lowercase(),
        right: CharSet::ascii_digits(),
    };
    let expression = interpolation(vec![literal("a"), reference(), reference(), literal("1")]);
    assert_eq!(
        outcome(typed_estimate(
            adjacency.clone(),
            expression.clone(),
            refined("^[X]*$", 0, Some(2))
        )),
        "undecided"
    );
    assert_eq!(
        outcome(typed_estimate(
            adjacency,
            expression,
            refined("^[X]+$", 1, Some(2))
        )),
        "proven"
    );
    assert_eq!(
        outcome(typed_estimate(
            InputPredicate::NoSubstring(token("xx")),
            interpolation(vec![reference(), reference()]),
            refined("^[x]$", 1, Some(1))
        )),
        "violation"
    );
    assert_eq!(
        outcome(typed_estimate(
            InputPredicate::AllowedChars(CharSet::ascii_lowercase()),
            interpolation(vec![reference(), literal("BAD"), reference()]),
            AttributeType::string()
        )),
        "violation"
    );
    assert_eq!(
        outcome(typed_estimate(
            InputPredicate::NoSubstring(token("bad")),
            interpolation(vec![reference(), literal("bad"), reference()]),
            AttributeType::string()
        )),
        "violation"
    );
}

#[test]
fn estimator_enums_use_api_values_and_unions_preserve_alternatives() {
    let enum_ty = |values: &[&str]| {
        AttributeType::enum_(
            TypeIdentity::bare("TestMode"),
            Some(values.iter().map(|v| (*v).into()).collect()),
            vec![("alpha".into(), "INVALID_ALIAS".into())],
            None,
            None,
        )
    };
    for (values, expected) in [
        (vec!["alpha", "beta"], "proven"),
        (vec!["123", "456"], "violation"),
        (vec!["alpha", "123"], "undecided"),
    ] {
        assert_eq!(
            outcome(typed_estimate(
                InputPredicate::AllowedChars(CharSet::ascii_lowercase()),
                reference(),
                enum_ty(&values)
            )),
            expected
        );
    }
    for (a, b, expected) in [
        ("^[a-z]+$", "^[a-z]+$", "proven"),
        ("^[0-9]+$", "^[0-9]+$", "violation"),
        ("^[a-z]+$", "^[0-9]+$", "undecided"),
    ] {
        assert_eq!(
            outcome(typed_estimate(
                InputPredicate::AllowedChars(CharSet::ascii_lowercase()),
                reference(),
                AttributeType::union(vec![refined(a, 1, None), refined(b, 1, None)])
            )),
            expected
        );
    }
    assert_eq!(
        outcome(typed_estimate(
            InputPredicate::NoSubstring(token("aa")),
            interpolation(vec![reference(), reference()]),
            enum_ty(&["a", "b"])
        )),
        "undecided"
    );
}

#[test]
fn estimator_patterns_are_whole_string_and_follow_compiler_ascii_semantics() {
    for pattern in ["[a-z]+", "^[a-z]+", "[a-z]+$", "^(ab|bc)+$", "(?m)^[a-z]+$"] {
        assert_eq!(
            outcome(typed_estimate(
                InputPredicate::AllowedChars(CharSet::ascii_lowercase()),
                reference(),
                refined(pattern, 1, None)
            )),
            "undecided",
            "{pattern}"
        );
    }
    for (pattern, chars) in [
        (r"^\d+$", CharSet::ascii_digits()),
        (
            r"^[\w]+$",
            CharSet::new([('a', 'z'), ('A', 'Z'), ('0', '9'), ('_', '_')]).unwrap(),
        ),
    ] {
        assert_eq!(
            outcome(typed_estimate(
                InputPredicate::AllowedChars(chars),
                reference(),
                refined(pattern, 1, None)
            )),
            "proven"
        );
    }
    assert_eq!(
        outcome(typed_estimate(
            InputPredicate::Length {
                min: Some(3),
                max: Some(3)
            },
            reference(),
            refined("^[é日😀]{3}$", 0, None)
        )),
        "proven"
    );
}

#[test]
fn estimator_ipv4_vectors_and_impossible_octet_structure() {
    for (value, ipv4) in [
        ("0.0.0.0", true),
        ("255.255.255.255", true),
        ("01.2.3.4", false),
        ("256.2.3.4", false),
        ("+1.2.3.4", false),
        ("1.2.3.4 ", false),
        ("١.2.3.4", false),
        ("1.2.3", false),
        ("1.2.3.4.5", false),
    ] {
        assert_eq!(is_ipv4(value), ipv4);
        assert_eq!(
            outcome(estimate(InputPredicate::NotIpv4, template(&[value]))),
            if ipv4 { "violation" } else { "proven" }
        );
    }
    for (pattern, min, max, tail) in [
        ("^[3-9]{3}$", 3, Some(3), ".0.0.0"),
        ("^[0-9]+$", 1, Some(3), ".01.0.0"),
        ("^[0-9]+$", 1, Some(3), ".0.0"),
        ("^[a-z]*$", 0, None, "-suffix"),
    ] {
        assert_eq!(
            outcome(typed_estimate(
                InputPredicate::NotIpv4,
                interpolation(vec![reference(), literal(tail)]),
                refined(pattern, min, max)
            )),
            "proven"
        );
    }
}

#[test]
fn estimator_absence_erased_unknown_functions_and_secrets_are_conservative() {
    let predicate = InputPredicate::FirstChar(CharSet::ascii_lowercase());
    assert_eq!(
        outcome(typed_target_estimate(
            predicate.clone(),
            reference(),
            refined("^[0-9]+$", 1, None),
            false,
            true
        )),
        "undecided"
    );
    let unknown = Value::Deferred(DeferredValue::Unknown(
        crate::resource::ApplyTimeUnknown::PendingReRead(
            crate::resource::PendingReRead::not_found(),
        ),
    ));
    assert_eq!(
        estimate(predicate.clone(), unknown),
        InputVerdict::Undecided
    );
    let call = Value::Deferred(DeferredValue::FunctionCall {
        name: crate::builtins::BuiltinFunction::join,
        args: vec![],
        source_line: Default::default(),
    });
    assert_eq!(estimate(predicate.clone(), call), InputVerdict::Undecided);
    let value = interpolation(vec![reference(), literal("secret-DO-NOT-PRINT")]).into_secret();
    let verdict = typed_estimate(
        InputPredicate::Length {
            min: None,
            max: Some(2),
        },
        value,
        refined("^[a-z]+$", 1, None),
    );
    assert_eq!(outcome(verdict.clone()), "violation");
    assert!(!format!("{verdict:?}").contains("DO-NOT-PRINT"));
    assert_eq!(
        outcome(estimate(predicate, template(&["abc"]).into_secret())),
        "proven"
    );
}

#[test]
fn estimator_map_keys_literals_and_refined_key_declarations() {
    let predicate = InputPredicate::ForbiddenPrefix {
        prefix: token("aws:"),
        ignore_ascii_case: true,
    };
    for (keys, expected) in [
        (vec!["good", "safe"], "proven"),
        (vec!["safe", "AWS:bad"], "violation"),
        (vec![], "proven"),
    ] {
        let map = Value::Concrete(ConcreteValue::Map(
            keys.iter()
                .map(|key| ((*key).into(), literal("ignored")))
                .collect(),
        ));
        assert_eq!(
            outcome(typed_target_estimate(
                predicate.clone(),
                map,
                AttributeType::string(),
                true,
                false
            )),
            expected
        );
    }
    for (key_ty, expected) in [
        (refined("^[a-z]+$", 1, None), "proven"),
        (AttributeType::string(), "undecided"),
        (refined("^[aAwWsS:]+$", 4, None), "undecided"),
    ] {
        assert_eq!(
            outcome(typed_target_estimate(
                predicate.clone(),
                reference(),
                AttributeType::map_with_key(key_ty, AttributeType::string()),
                true,
                false
            )),
            expected
        );
    }
    // A map might be empty even if every possible key violates the rule.
    assert_eq!(
        outcome(typed_target_estimate(
            InputPredicate::AllowedChars(CharSet::ascii_lowercase()),
            reference(),
            AttributeType::map_with_key(refined("^[0-9]+$", 1, None), AttributeType::string()),
            true,
            false
        )),
        "undecided"
    );
}

#[test]
fn estimator_seeded_literal_concatenations_are_sound() {
    // Generated-input property test with a fixed seed. No proptest/quickcheck
    // dependency exists in this workspace. Include Unicode and empty parts.
    let predicates = vec![
        InputPredicate::Length {
            min: Some(2),
            max: Some(8),
        },
        InputPredicate::AllowedChars(CharSet::ascii_lowercase()),
        InputPredicate::FirstChar(CharSet::ascii_letters()),
        InputPredicate::LastChar(CharSet::ascii_digits()),
        InputPredicate::NoSubstring(token("ab")),
        InputPredicate::NoPrefix(token("ab")),
        InputPredicate::NoSuffix(token("ab")),
        InputPredicate::ForbiddenPrefix {
            prefix: token("ab"),
            ignore_ascii_case: true,
        },
        InputPredicate::NoAdjacent {
            left: CharSet::ascii_lowercase(),
            right: CharSet::ascii_digits(),
        },
        InputPredicate::NotIpv4,
    ];
    let alphabet = ['a', 'b', 'A', '0', '1', '.', 'é', '日', '😀'];
    let mut seed = 0x4267_4295_u64;
    for _ in 0..300 {
        let mut parts = Vec::new();
        for _ in 0..5 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let value = (0..(seed >> 32) % 4)
                .map(|i| alphabet[((seed >> (i * 8)) as usize) % alphabet.len()])
                .collect::<String>();
            parts.push(value);
        }
        let whole = parts.concat();
        let refs = parts.iter().map(String::as_str).collect::<Vec<_>>();
        for predicate in &predicates {
            let concrete = outcome(estimate(predicate.clone(), literal(&whole)));
            let estimated = outcome(estimate(predicate.clone(), template(&refs)));
            assert!(
                estimated == "undecided" || estimated == concrete,
                "{predicate:?}: {estimated}, concrete {concrete}"
            );
        }
    }
}

fn binding(name: &str) -> crate::resource::AbsoluteBindingAddress {
    crate::resource::absolute_binding_address_for_test(name)
}

fn path_reference(root: &str, attr: &str, segments: Vec<PathSegment>) -> Value {
    Value::Deferred(DeferredValue::ResourceRef {
        path: AccessPath::from_binding(binding(root), attr, segments),
    })
}

fn target_registry() -> SchemaRegistry {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "mock",
        ResourceSchema::new("Target")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .input_rule(InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("test.rule").unwrap(),
                InputPredicate::Length {
                    min: Some(3),
                    max: Some(5),
                },
            ))
            .validate()
            .unwrap(),
    );
    registry
}

fn estimate_in_context<S>(
    value: Value,
    schemas: &SchemaRegistry,
    types: &InputTypeContext<S>,
) -> InputVerdict {
    let resource =
        Resource::with_provider("mock", "Target", "target", None).with_attribute("name", value);
    let mut site =
        classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), schemas).unwrap();
    site.types = Some(&types.facts);
    site.applicable_rules().next().unwrap().evaluate()
}

#[test]
fn estimator_resolves_definitions_nested_paths_and_reference_metadata() {
    let mut registry = target_registry();
    registry.insert(
        "mock",
        ResourceSchema::new("Source")
            .with_def(
                "Node",
                AttributeType::struct_(
                    "Node",
                    vec![
                        StructField::required(
                            "label",
                            AttributeType::reference(
                                TypeIdentity::bare("Label"),
                                Some("^[a-z]+$".into()),
                                Some((Some(3), Some(5))),
                            ),
                        )
                        .build(),
                        StructField::optional("child", AttributeType::ref_("Node")).build(),
                    ],
                ),
            )
            .attribute(
                AttributeSchema::provider_populated("value", AttributeType::ref_("Node")).build(),
            )
            .attribute(
                AttributeSchema::provider_populated(
                    "list",
                    AttributeType::list(AttributeType::ref_("Node")),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::provider_populated(
                    "map",
                    AttributeType::map(AttributeType::ref_("Node")),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let mut source = Resource::with_provider("mock", "Source", "source", None);
    source.binding = Some(binding("source"));
    let types = InputTypeContext::from_sources(
        &[UnresolvedResource::from_pre_resolve(source)],
        &[],
        std::iter::empty(),
        &[],
        &registry,
    );
    for (attr, steps) in [
        (
            "value",
            vec![PathSegment::Field {
                name: "label".into(),
            }],
        ),
        (
            "list",
            vec![
                PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                },
                PathSegment::Field {
                    name: "label".into(),
                },
            ],
        ),
        (
            "map",
            vec![
                PathSegment::Subscript {
                    index: Subscript::Str { key: "key".into() },
                },
                PathSegment::Field {
                    name: "label".into(),
                },
            ],
        ),
    ] {
        assert_eq!(
            outcome(estimate_in_context(
                path_reference("source", attr, steps),
                &registry,
                &types
            )),
            "proven"
        );
    }
    assert_eq!(
        outcome(estimate_in_context(
            path_reference(
                "source",
                "value",
                vec![PathSegment::Field {
                    name: "missing".into()
                }]
            ),
            &registry,
            &types
        )),
        "undecided"
    );
}

#[test]
fn estimator_follows_composition_expressions_wait_aliases_and_data_sources_for_live_and_saved_sources()
 {
    use crate::binding_index::WaitAliasSpec;
    let mut registry = target_registry();
    registry.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(
                AttributeSchema::provider_populated("value", refined("^[a-z]+$", 3, Some(5)))
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    registry.insert(
        "mock",
        ResourceSchema::new("Lookup")
            .as_data_source()
            .attribute(
                AttributeSchema::provider_populated("value", refined("^[a-z]+$", 3, Some(5)))
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let mut source = Resource::with_provider("mock", "Source", "source", None);
    source.binding = Some(binding("source"));
    let mut lookup = DataSource::with_provider("mock", "Lookup", "lookup", None);
    lookup.binding = Some(binding("lookup"));
    let outputs = || {
        vec![(
            binding("module"),
            IndexMap::from([
                ("forwarded".into(), reference()),
                (
                    "derived".into(),
                    interpolation(vec![literal("abc"), literal("d")]),
                ),
            ]),
        )]
    };
    let aliases = [WaitAliasSpec {
        binding: binding("ready"),
        target: binding("source"),
    }];
    let types = InputTypeContext::from_sources(
        &[UnresolvedResource::from_pre_resolve(source.clone())],
        &[lookup],
        outputs(),
        &aliases,
        &registry,
    );
    for (root, attr) in [
        ("module", "forwarded"),
        ("module", "derived"),
        ("ready", "value"),
        ("lookup", "value"),
    ] {
        assert_eq!(
            outcome(estimate_in_context(
                path_reference(root, attr, vec![]),
                &registry,
                &types
            )),
            "proven"
        );
    }
    let saved: crate::resource::Resource<crate::resource::SavedModuleSource> = source.into();
    let saved_types = InputTypeContext::from_sources(
        &[UnresolvedResource::from_pre_resolve(saved)],
        &[],
        outputs(),
        &aliases,
        &registry,
    );
    assert_eq!(
        outcome(estimate_in_context(
            path_reference("module", "forwarded", vec![]),
            &registry,
            &saved_types
        )),
        "proven"
    );
    let cycle = [WaitAliasSpec {
        binding: binding("source"),
        target: binding("source"),
    }];
    let mut source = Resource::with_provider("mock", "Source", "source", None);
    source.binding = Some(binding("source"));
    let cyclic_types = InputTypeContext::from_sources(
        &[UnresolvedResource::from_pre_resolve(source)],
        &[],
        std::iter::empty(),
        &cycle,
        &registry,
    );
    assert_eq!(
        outcome(estimate_in_context(reference(), &registry, &cyclic_types)),
        "undecided"
    );
}

#[test]
fn estimator_default_tags_are_checked_through_the_host_merge_for_unknown_maps() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Tagged")
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .input_rule(InputRule::new(
                InputRuleScope::EveryWrite,
                InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
                InputRuleCode::new("tags.prefix").unwrap(),
                InputPredicate::ForbiddenPrefix {
                    prefix: token("aws:"),
                    ignore_ascii_case: true,
                },
            ))
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(
                AttributeSchema::provider_populated(
                    "value",
                    AttributeType::map_with_key(
                        refined("^[a-z]+$", 1, None),
                        AttributeType::string(),
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let mut source = Resource::with_provider("mock", "Source", "source", None);
    source.binding = Some(binding("source"));
    let target = Resource::with_provider("mock", "Tagged", "target", None)
        .with_attribute("tags", reference());
    let unresolved = [
        UnresolvedResource::from_pre_resolve(source),
        UnresolvedResource::from_pre_resolve(target.clone()),
    ];
    for (key, value, expected) in [
        ("safe", literal("value"), "proven"),
        ("AWS:private", literal("value"), "violation"),
        ("AWS:private", reference(), "undecided"),
    ] {
        let config = crate::parser::ProviderConfig {
            name: "mock".into(),
            attributes: Default::default(),
            default_tags: IndexMap::from([(key.into(), value)]),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: Default::default(),
            binding: None,
        };
        let types =
            InputTypeContext::from_sources(&unresolved, &[], std::iter::empty(), &[], &schemas)
                .with_default_tags(&[config], &schemas, [&target.id])
                .unwrap();
        let mut site =
            classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&target), &schemas)
                .unwrap();
        site.types = Some(&types.facts);
        assert_eq!(
            outcome(site.applicable_rules().next().unwrap().evaluate()),
            expected
        );
    }
}

#[test]
fn estimator_deferred_template_uses_retained_literals_and_defers_loop_slots() {
    use crate::{
        effect::DeferredCreatePayload,
        parser::{DeferredForExpression, DeferredForTarget, ForBinding},
        plan::Plan,
    };
    let registry = target_registry();
    for (expression, expected) in [
        (template(&["ab", "cd"]).into_placeholder_type(), "proven"),
        (
            template(&["too-", "long"]).into_placeholder_type(),
            "violation",
        ),
        (
            Value::Deferred(DeferredValue::Placeholder(crate::resource::LoopSlot::Value)),
            "undecided",
        ),
    ] {
        let template: DeferredForExpression = DeferredForExpression::new(
            None,
            1,
            "for item in parent.items".into(),
            DeferredForTarget::bound(binding("children")).unwrap(),
            Value::root_resource_ref("parent", "items", vec![]),
            ForBinding::Simple("item".into()),
            Resource::with_provider("mock", "Target", "children", None)
                .with_attribute("name", expression),
        );
        let payload: DeferredCreatePayload = crate::test_support::compiled_loop(Box::new(template));
        let mut plan = Plan::new();
        plan.add(Effect::DeferredCreate(payload));
        let finalized = FinalizedPlan::new(plan);
        let current = HashMap::new();
        let site = classify_write_site(
            WriteSiteSource::Finalized {
                plan: &finalized,
                index: 0,
                current: &current,
                types: None,
            },
            &registry,
        )
        .unwrap();
        assert_eq!(
            outcome(site.applicable_rules().next().unwrap().evaluate()),
            expected
        );
    }
}

#[test]
fn estimator_depth_budget_and_overflow_widen_to_pending() {
    let mut expression = reference();
    for _ in 0..80 {
        expression = interpolation(vec![expression]);
    }
    assert_eq!(
        outcome(typed_estimate(
            InputPredicate::Length {
                min: Some(1),
                max: Some(10)
            },
            expression,
            refined("^[a-z]+$", 2, Some(3))
        )),
        "undecided"
    );
    let enormous =
        AttributeType::refined_string(None, None, Some((Some(u64::MAX), Some(u64::MAX))), None);
    assert_eq!(
        outcome(typed_estimate(
            InputPredicate::Length {
                min: Some(1),
                max: Some(10)
            },
            interpolation(vec![reference(), literal("x")]),
            enormous
        )),
        "undecided"
    );
}

#[test]
fn estimator_map_cardinality_is_required_to_prove_a_key_violation() {
    let predicate = InputPredicate::AllowedChars(CharSet::ascii_lowercase());
    let map = |min| {
        AttributeType::refined_map(
            refined("^[0-9]+$", 1, None),
            AttributeType::string(),
            Some((Some(min), None)),
        )
    };
    assert_eq!(
        outcome(typed_target_estimate(
            predicate.clone(),
            reference(),
            map(1),
            true,
            false
        )),
        "violation"
    );
    assert_eq!(
        outcome(typed_target_estimate(
            predicate,
            reference(),
            map(0),
            true,
            false
        )),
        "undecided"
    );
}

#[test]
fn estimator_literal_map_keys_use_the_provider_api_spelling() {
    let mut schemas = SchemaRegistry::new();
    let key_type = AttributeType::enum_(
        TypeIdentity::bare("Status"),
        Some(vec!["Enabled".into()]),
        vec![("Enabled".into(), "enabled".into())],
        None,
        None,
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Tagged")
            .attribute(
                AttributeSchema::optional(
                    "tags",
                    AttributeType::map_with_key(key_type.clone(), AttributeType::string()),
                )
                .build(),
            )
            .input_rule(InputRule::new(
                InputRuleScope::EveryWrite,
                InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
                InputRuleCode::new("tags.characters").unwrap(),
                InputPredicate::AllowedChars(CharSet::ascii_lowercase()),
            ))
            .validate()
            .unwrap(),
    );
    let resource = Resource::with_provider("mock", "Tagged", "target", None).with_attribute(
        "tags",
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "enabled".into(),
            literal("ignored"),
        )]))),
    );
    let mut normalized = [resource.clone()];
    crate::value::canonicalize_resources_with_schemas(&mut normalized, &schemas).unwrap();
    let concrete =
        classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&normalized[0]), &schemas)
            .unwrap();
    assert_eq!(
        outcome(concrete.applicable_rules().next().unwrap().evaluate()),
        "violation"
    );
    let mut site =
        classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&resource), &schemas)
            .unwrap();
    site.attributes = None;
    assert_eq!(
        outcome(site.applicable_rules().next().unwrap().evaluate()),
        "violation"
    );
    for (source_key_type, expected) in [
        (key_type, "violation"),
        (refined("^[a-z]+$", 1, None), "undecided"),
    ] {
        schemas.insert(
            "mock",
            ResourceSchema::new("Source")
                .attribute(
                    AttributeSchema::provider_populated(
                        "value",
                        AttributeType::refined_map(
                            source_key_type,
                            AttributeType::string(),
                            Some((Some(1), None)),
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let mut source = Resource::with_provider("mock", "Source", "source", None);
        source.binding = Some(binding("source"));
        let types = InputTypeContext::from_sources(
            &[UnresolvedResource::from_pre_resolve(source)],
            &[],
            std::iter::empty(),
            &[],
            &schemas,
        );
        let target = Resource::with_provider("mock", "Tagged", "target", None)
            .with_attribute("tags", reference());
        let mut site =
            classify_write_site::<ModuleSource>(WriteSiteSource::Authored(&target), &schemas)
                .unwrap();
        site.types = Some(&types.facts);
        assert_eq!(
            outcome(site.applicable_rules().next().unwrap().evaluate()),
            expected
        );
    }
}

#[test]
fn estimator_forbidden_prefix_case_folding_is_ascii_only() {
    let predicate = |prefix, ignore_ascii_case| InputPredicate::ForbiddenPrefix {
        prefix: token(prefix),
        ignore_ascii_case,
    };
    assert_eq!(
        outcome(typed_estimate(
            predicate("AA", false),
            reference(),
            refined("^[a]+$", 2, None)
        )),
        "proven"
    );
    assert_eq!(
        outcome(typed_estimate(
            predicate("AA", true),
            reference(),
            refined("^[aA]+$", 2, None)
        )),
        "violation"
    );
    assert_eq!(
        outcome(typed_estimate(
            predicate("É", true),
            reference(),
            refined("^[é]+$", 2, None)
        )),
        "proven"
    );
}

#[test]
fn estimator_cannot_derive_proof_from_an_erased_final_value_without_source() {
    use crate::{plan::Plan, resource::ResolvedResource};
    let registry = target_registry();
    let resource = Resource::with_provider("mock", "Target", "target", None).with_attribute(
        "name",
        Value::Deferred(DeferredValue::Unknown(
            crate::resource::ApplyTimeUnknown::PendingReRead(
                crate::resource::PendingReRead::not_found(),
            ),
        )),
    );
    let mut plan = Plan::new();
    plan.add(Effect::Create(
        ResolvedResource::resolve_raw_fixture(resource, &registry).unwrap(),
    ));
    assert_eq!(
        FinalizedPlan::new(plan)
            .check_inputs_with_types(&registry, &HashMap::new(), &InputTypeContext::empty())
            .unwrap()
            .pending_inputs()
            .len(),
        1
    );
}

#[test]
fn estimator_homogeneous_summaries_never_contradict_generated_witnesses() {
    let mut seed = 0x4295u64;
    for _ in 0..100 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let min = (seed >> 32) % 3;
        let max = min + (seed >> 40) % 4;
        let pattern = if seed & 1 == 0 { "^[ab]*$" } else { "^[a0]*$" };
        let ty = refined(pattern, min, Some(max));
        let left = if seed & 2 == 0 { "a" } else { "" };
        let right = if seed & 4 == 0 { "b" } else { "0" };
        let predicates = [
            InputPredicate::NoSubstring(token("ab")),
            InputPredicate::NoAdjacent {
                left: CharSet::ascii_lowercase(),
                right: CharSet::ascii_digits(),
            },
            InputPredicate::FirstChar(CharSet::ascii_lowercase()),
            InputPredicate::Length {
                min: Some(2),
                max: Some(5),
            },
        ];
        for predicate in predicates {
            let verdict = outcome(typed_estimate(
                predicate.clone(),
                interpolation(vec![
                    literal(left),
                    reference(),
                    reference(),
                    literal(right),
                ]),
                ty.clone(),
            ));
            for n in min..=max {
                for bits in 0..(1u64 << n) {
                    let middle = (0..n)
                        .map(|i| {
                            if bits & (1 << i) == 0 {
                                'a'
                            } else if seed & 1 == 0 {
                                'b'
                            } else {
                                '0'
                            }
                        })
                        .collect::<String>();
                    let whole = format!("{left}{middle}{middle}{right}");
                    let concrete = outcome(estimate(predicate.clone(), literal(&whole)));
                    assert!(
                        verdict == "undecided" || verdict == concrete,
                        "{predicate:?}: {verdict} versus {concrete}"
                    );
                }
            }
        }
    }
}
