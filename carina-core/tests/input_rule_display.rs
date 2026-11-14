//! Declaration text is also the requirement reported for a rejected input.
use std::collections::HashMap;

use carina_core::provider_text::{DiagnosticText, MAX_PROVIDER_TEXT_BYTES};
use carina_core::resource::{ConcreteValue, ResolvedResource, Resource, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, CharSet, InputCondition, InputLiteral, InputPath,
    InputPredicate, InputRule, InputRuleCode, InputRuleDisplay, InputRuleScope, InputRuleTarget,
    NonEmptyString, PatternMatch, PatternReason, PatternSpec, ResourceSchema, SchemaRegistry,
    ValidatedResourceSchema,
};

fn path(value: &str) -> InputPath {
    InputPath::new(value).unwrap()
}

fn token(value: &str) -> NonEmptyString {
    NonEmptyString::new(value).unwrap()
}

#[test]
fn review_4327_quoted_literals_and_identities_are_unambiguous_and_inline() {
    let value = "a\"b\\c\n\t\r\u{1b}\u{202e}\u{2028}\u{2066}\u{200b}";
    let quoted = r#""a\"b\\c\n\t\r\u{1b}\u{202e}\u{2028}\u{2066}\u{200b}""#;
    let literal = InputLiteral::String(value.into());
    assert_eq!(literal.to_string(), quoted);
    let declaration = rule(InputPredicate::Equals(literal.clone())).with_condition(
        InputCondition::Not(Box::new(InputCondition::Equals {
            path: path("settings.guard"),
            literal,
        })),
    );
    assert_eq!(
        InputRuleDisplay::new(&declaration).to_string(),
        format!(
            "value at name: must equal {quoted} when not (settings.guard equals {quoted}) (on create)"
        )
    );
    let enumeration = InputLiteral::Enum {
        identity: carina_core::schema::TypeIdentity::new(
            Some("pro\"vider\\\n"),
            ["s3", "Bucket"],
            "Namespace\u{202e}",
        ),
        api_value: value.into(),
    };
    assert_eq!(
        enumeration.to_string(),
        format!(r#""pro\"vider\\\n.s3.Bucket.Namespace\u{{202e}}"::{quoted}"#)
    );
    assert_eq!(path("settings.guard").to_string(), "settings.guard");
}

#[test]
fn review_4327_all_quoted_tokens_and_character_endpoints_share_escaping() {
    let value = "a\"b\\c\n\u{202e}";
    let quoted = r#""a\"b\\c\n\u{202e}""#;
    for (predicate, verb) in [
        (InputPredicate::NoSubstring(token(value)), "contain"),
        (InputPredicate::NoPrefix(token(value)), "start with"),
        (
            InputPredicate::ForbiddenPrefix {
                prefix: token(value),
                ignore_ascii_case: false,
            },
            "start with",
        ),
        (InputPredicate::NoSuffix(token(value)), "end with"),
    ] {
        assert_eq!(
            InputRuleDisplay::new(&rule(predicate)).to_string(),
            format!("value at name: must not {verb} {quoted} (on create)")
        );
    }
    let declaration = rule(InputPredicate::AllowedChars(
        CharSet::new([
            ('"', '"'),
            ('\'', '\''),
            ('\\', '\\'),
            ('\u{202e}', '\u{202e}'),
        ])
        .unwrap(),
    ));
    assert_eq!(
        InputRuleDisplay::new(&declaration).to_string(),
        r#"value at name: must contain only characters in {'\"', '\'', '\\', '\u{202e}'} (on create)"#
    );
}

#[test]
fn review_4327_long_quoted_literals_are_budgeted_once_with_the_requirement() {
    let value = "é\"\\\n".repeat(MAX_PROVIDER_TEXT_BYTES);
    let quoted = format!("{value:?}");
    let requirement =
        DiagnosticText::new(format!("must equal {quoted} when guard equals {quoted}"));
    let literal = InputLiteral::String(value);
    let declaration =
        rule(InputPredicate::Equals(literal.clone())).with_condition(InputCondition::Equals {
            path: path("guard"),
            literal,
        });
    assert_eq!(
        InputRuleDisplay::new(&declaration).to_string(),
        format!("value at name: {requirement} (on create)")
    );
}

#[test]
fn review_4327_plan_prefix_budget_preserves_the_requirement_and_separator() {
    let token_text = "é".repeat(MAX_PROVIDER_TEXT_BYTES);
    let requirement = DiagnosticText::new(format!("must not end with {token_text:?}"));
    let path = "field".repeat(MAX_PROVIDER_TEXT_BYTES);
    let prefix = DiagnosticText::new(format!("cannot create: {path} [test.long_suffix]"));
    let resource: Resource =
        Resource::with_provider("test", "test.InputRuleDisplay", "example", None);
    let error = carina_core::plan::PlanError::new(
        resource.id,
        carina_core::plan::PlanErrorKind::InputRule {
            path,
            code: "test.long_suffix".into(),
            reason: requirement.clone(),
            context: carina_core::input_rules::InputWriteContext::Create,
        },
    );
    assert_eq!(
        error
            .display_with_project_dir(std::path::Path::new("."))
            .to_string(),
        format!("test.test.InputRuleDisplay.example: {prefix}: {requirement}")
    );
}

#[test]
fn review_4327_input_patterns_are_inline_in_declarations_and_violations() {
    let source = "a\u{202e}\nb";
    for (predicate, value, verb) in [
        (
            InputPredicate::Matches(PatternSpec::new(source, PatternMatch::Full)),
            "DO-NOT-PRINT",
            "must match",
        ),
        (
            InputPredicate::NotMatches(PatternSpec::new(source, PatternMatch::Full)),
            source,
            "must not match",
        ),
    ] {
        let declaration = rule(predicate);
        let requirement = format!("{verb} \"a\\u{{202e}}\\nb\" (Full)");
        assert_eq!(
            InputRuleDisplay::new(&declaration).to_string(),
            format!("value at name: {requirement} (on create)")
        );
        let schema = schema(declaration);
        assert_eq!(
            InputRuleDisplay::new(&schema.as_schema().input_rules()[0]).to_string(),
            format!("value at name: {requirement} (on create)")
        );
        assert_eq!(
            reject(&schema, Some(string(value))).message().to_string(),
            requirement
        );
    }
}

fn rule(predicate: InputPredicate<PatternSpec>) -> InputRule {
    InputRule::value(
        InputRuleScope::CreateOnly,
        path("name"),
        InputRuleCode::new("test.display").unwrap(),
        predicate,
    )
}

fn string(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.into()))
}

#[test]
fn secret_predicate_display_shares_the_pinned_requirement_with_violations() {
    let requirement = "must be a secret value (for example `secret(...)` or `decrypt(...)`)";
    for (scope, scope_text) in [
        (InputRuleScope::CreateOnly, "on create"),
        (InputRuleScope::EveryWrite, "on every write"),
    ] {
        for (condition, suffix) in [
            (InputCondition::Always, ""),
            (
                InputCondition::Present(path("guard")),
                " when guard is present",
            ),
        ] {
            let declaration = InputRule::value(
                scope,
                path("name"),
                InputRuleCode::new("test.secret").unwrap(),
                InputPredicate::Secret,
            )
            .with_condition(condition);
            let expected = format!("value at name: {requirement}{suffix} ({scope_text})");
            assert_eq!(InputRuleDisplay::new(&declaration).to_string(), expected);
            let admitted = schema(declaration);
            assert_eq!(
                InputRuleDisplay::new(&admitted.as_schema().input_rules()[0]).to_string(),
                expected
            );
            let violation = reject(&admitted, Some(string("DO-NOT-PRINT")));
            assert_eq!(
                violation.message().to_string(),
                format!("{requirement}{suffix}")
            );
            assert!(!format!("{violation:?} {violation}").contains("DO-NOT-PRINT"));
        }
    }
}

struct Case {
    predicate: InputPredicate<PatternSpec>,
    requirement: &'static str,
    rejected: Option<Value>,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            predicate: InputPredicate::Present,
            requirement: "must be present",
            rejected: None,
        },
        Case {
            predicate: InputPredicate::Absent,
            requirement: "must be absent",
            rejected: Some(string("DO-NOT-PRINT")),
        },
        Case {
            predicate: InputPredicate::Equals(InputLiteral::Bool(true)),
            requirement: "must equal true",
            rejected: Some(Value::Concrete(ConcreteValue::Bool(false))),
        },
        Case {
            predicate: InputPredicate::Length {
                min: Some(3),
                max: Some(63),
            },
            requirement: "must have length 3..=63 Unicode scalar values",
            rejected: Some(string(&"DO-NOT-PRINT".repeat(8))),
        },
        Case {
            predicate: InputPredicate::AllowedChars(CharSet::ascii_lowercase()),
            requirement: "must contain only characters in {'a'..='z'}",
            rejected: Some(string("DO-NOT-PRINT")),
        },
        Case {
            predicate: InputPredicate::FirstChar(CharSet::ascii_digits()),
            requirement: "must start with a character in {'0'..='9'}",
            rejected: Some(string("DO-NOT-PRINT")),
        },
        Case {
            predicate: InputPredicate::LastChar(CharSet::ascii_digits()),
            requirement: "must end with a character in {'0'..='9'}",
            rejected: Some(string("DO-NOT-PRINT")),
        },
        Case {
            predicate: InputPredicate::NoSubstring(token("..")),
            requirement: "must not contain \"..\"",
            rejected: Some(string("DO-NOT-PRINT..")),
        },
        Case {
            predicate: InputPredicate::NoPrefix(token("xn--")),
            requirement: "must not start with \"xn--\"",
            rejected: Some(string("xn--DO-NOT-PRINT")),
        },
        Case {
            predicate: InputPredicate::ForbiddenPrefix {
                prefix: token("aws:"),
                ignore_ascii_case: true,
            },
            requirement: "must not start with \"aws:\" (ignoring ASCII case)",
            rejected: Some(string("AWS:DO-NOT-PRINT")),
        },
        Case {
            predicate: InputPredicate::NoSuffix(token("-an")),
            requirement: "must not end with \"-an\"",
            rejected: Some(string("DO-NOT-PRINT-an")),
        },
        Case {
            predicate: InputPredicate::NoAdjacent {
                left: CharSet::new([('.', '.')]).unwrap(),
                right: CharSet::new([('.', '.')]).unwrap(),
            },
            requirement: "must not contain an adjacent pair from {'.'} then {'.'}",
            rejected: Some(string("DO-NOT-PRINT..")),
        },
        Case {
            predicate: InputPredicate::NotIpv4,
            requirement: "must not have IPv4 dotted-quad form",
            rejected: Some(string("192.168.1.1")),
        },
        Case {
            predicate: InputPredicate::Matches(PatternSpec::new("[a-z]+", PatternMatch::Full)),
            requirement: "must match /[a-z]+/ (Full)",
            rejected: Some(string("DO-NOT-PRINT")),
        },
        Case {
            predicate: InputPredicate::NotMatches(PatternSpec::new(
                "tmp",
                PatternMatch::Substring {
                    reason: PatternReason::new("temporary names").unwrap(),
                },
            )),
            requirement: "must not match /tmp/ (Substring: temporary names)",
            rejected: Some(string("DO-NOT-PRINT-tmp")),
        },
    ]
}

fn schema(declaration: InputRule) -> ValidatedResourceSchema {
    let ty = if matches!(
        declaration.predicate(),
        InputPredicate::Equals(InputLiteral::Bool(_))
    ) {
        AttributeType::bool()
    } else {
        AttributeType::string()
    };
    ResourceSchema::new("test.InputRuleDisplay")
        .attribute(AttributeSchema::optional("name", ty).build())
        .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
        .input_rule(declaration)
        .validate()
        .unwrap()
}

fn reject(
    schema: &ValidatedResourceSchema,
    value: Option<Value>,
) -> carina_core::schema::InputViolation {
    let mut registry = SchemaRegistry::new();
    registry.insert("mock", schema.clone());
    let mut resource = Resource::with_provider("mock", "test.InputRuleDisplay", "example", None)
        .with_attribute("guard", Value::Concrete(ConcreteValue::Bool(true)));
    if let Some(value) = value {
        resource = resource.with_attribute("name", value);
    }
    let mut plan = carina_core::plan::Plan::new();
    plan.add(carina_core::effect::Effect::Create(
        ResolvedResource::resolve_raw_fixture(resource, &registry).unwrap(),
    ));
    let plan = carina_core::input_rules::finalize_plan_for_test(plan);
    let current = HashMap::new();
    for site in plan.write_sites(&registry, &current) {
        let evaluation = site.unwrap().evaluate();
        assert!(evaluation.pending().is_empty());
        if let Some(finding) = evaluation.findings().first() {
            return finding.violation().clone();
        }
    }
    panic!("fixture must violate its declaration");
}

#[test]
fn input_rule_display_pins_every_predicate_and_shares_violation_requirements() {
    for case in cases() {
        for (condition, suffix) in [
            (InputCondition::Always, ""),
            (
                InputCondition::Present(path("guard")),
                " when guard is present",
            ),
        ] {
            let declaration = rule(case.predicate.clone()).with_condition(condition);
            let expected = format!("value at name: {}{suffix} (on create)", case.requirement);
            assert_eq!(InputRuleDisplay::new(&declaration).to_string(), expected);
            let schema = schema(declaration);
            let compiled = &schema.as_schema().input_rules()[0];
            assert_eq!(InputRuleDisplay::new(compiled).to_string(), expected);
            for protected in [false, true] {
                let value = case.rejected.clone().map(|value| {
                    if protected {
                        Value::Deferred(carina_core::resource::DeferredValue::Secret(
                            Box::new(value).into(),
                        ))
                    } else {
                        value
                    }
                });
                let violation = reject(&schema, value);
                let requirement = expected
                    .strip_prefix("value at name: ")
                    .unwrap()
                    .strip_suffix(" (on create)")
                    .unwrap();
                assert_eq!(violation.message().to_string(), requirement);
                assert_eq!(violation.code(), compiled.code());
                let rendered = format!("{violation} {violation:?}");
                assert!(!rendered.contains("DO-NOT-PRINT"));
                assert!(!rendered.contains("192.168.1.1"));
            }
        }
    }
}

#[test]
fn input_rule_display_covers_targets_scopes_case_flags_and_one_sided_lengths() {
    for (scope, scope_text) in [
        (InputRuleScope::CreateOnly, "on create"),
        (InputRuleScope::EveryWrite, "on every write"),
    ] {
        for (target, target_text) in [
            (
                InputRuleTarget::Value(path("settings.name")),
                "value at settings.name",
            ),
            (InputRuleTarget::MapKeys(path("tags")), "map keys at tags"),
        ] {
            for (ignore_ascii_case, qualifier) in [(false, ""), (true, " (ignoring ASCII case)")] {
                let declaration: InputRule = InputRule::new(
                    scope,
                    target.clone(),
                    InputRuleCode::new("test.prefix").unwrap(),
                    InputPredicate::ForbiddenPrefix {
                        prefix: token("aws:"),
                        ignore_ascii_case,
                    },
                );
                assert_eq!(
                    InputRuleDisplay::new(&declaration).to_string(),
                    format!(
                        "{target_text}: must not start with \"aws:\"{qualifier} ({scope_text})"
                    )
                );
            }
        }
    }
    for (min, max, bounds) in [(Some(3), None, "3.."), (None, Some(63), "..=63")] {
        assert_eq!(
            InputRuleDisplay::new(&rule(InputPredicate::Length { min, max })).to_string(),
            format!("value at name: must have length {bounds} Unicode scalar values (on create)")
        );
    }
    let asymmetric = rule(InputPredicate::NoAdjacent {
        left: CharSet::ascii_lowercase(),
        right: CharSet::ascii_digits(),
    });
    assert_eq!(
        InputRuleDisplay::new(&asymmetric).to_string(),
        "value at name: must not contain an adjacent pair from {'a'..='z'} then {'0'..='9'} (on create)"
    );
    let canonical = rule(InputPredicate::AllowedChars(
        CharSet::new([('b', 'z'), ('0', '9'), ('a', 'b'), ('-', '.')]).unwrap(),
    ));
    assert_eq!(
        InputRuleDisplay::new(&canonical).to_string(),
        "value at name: must contain only characters in {'-'..='.', '0'..='9', 'a'..='z'} (on create)"
    );
}

#[test]
fn input_rule_display_preserves_every_condition_form_and_struct_relative_paths() {
    let equals = InputCondition::Equals {
        path: path("guard"),
        literal: InputLiteral::Bool(true),
    };
    for (condition, suffix) in [
        (InputCondition::Always, ""),
        (
            InputCondition::Present(path("period")),
            " when period is present",
        ),
        (equals.clone(), " when guard equals true"),
        (
            InputCondition::Not(Box::new(equals.clone())),
            " when not (guard equals true)",
        ),
        (
            InputCondition::Not(Box::new(InputCondition::Present(path("period")))),
            " when not (period is present)",
        ),
        (
            InputCondition::Not(Box::new(InputCondition::Not(Box::new(equals)))),
            " when not (not (guard equals true))",
        ),
        (
            InputCondition::Not(Box::new(InputCondition::Always)),
            " when not (always)",
        ),
    ] {
        let declaration: InputRule = InputRule::value(
            InputRuleScope::EveryWrite,
            path("enabled"),
            InputRuleCode::new("test.pitr").unwrap(),
            InputPredicate::Present,
        )
        .with_condition(condition);
        assert_eq!(
            InputRuleDisplay::new(&declaration).to_string(),
            format!("value at enabled: must be present{suffix} (on every write)")
        );
    }
}

#[test]
fn input_rule_display_covers_every_literal_kind_in_predicates_and_conditions() {
    for (literal, spelling) in [
        (InputLiteral::Bool(false), "false"),
        (InputLiteral::Int(-7), "-7"),
        (
            InputLiteral::Float(carina_core::FiniteF64::new(1.5).unwrap()),
            "1.5",
        ),
        (InputLiteral::String("fifo".into()), "\"fifo\""),
        (
            InputLiteral::Enum {
                identity: carina_core::schema::TypeIdentity::new(
                    Some("aws"),
                    ["s3", "Bucket"],
                    "Namespace",
                ),
                api_value: "account-regional".into(),
            },
            "aws.s3.Bucket.Namespace::\"account-regional\"",
        ),
    ] {
        let declaration =
            rule(InputPredicate::Equals(literal.clone())).with_condition(InputCondition::Equals {
                path: path("guard"),
                literal,
            });
        assert_eq!(
            InputRuleDisplay::new(&declaration).to_string(),
            format!(
                "value at name: must equal {spelling} when guard equals {spelling} (on create)"
            )
        );
    }
}

#[test]
fn input_rule_display_escapes_provider_text_without_markdown_escaping() {
    let declaration = rule(InputPredicate::NoSuffix(token("*_[x]|`\"\n\t\u{1b}")));
    assert_eq!(
        InputRuleDisplay::new(&declaration).to_string(),
        "value at name: must not end with \"*_[x]|`\\\"\\n\\t\\u{1b}\" (on create)"
    );
    let controls = rule(InputPredicate::AllowedChars(
        CharSet::new([('\n', '\n'), ('\u{1b}', '\u{1b}')]).unwrap(),
    ));
    assert_eq!(
        InputRuleDisplay::new(&controls).to_string(),
        "value at name: must contain only characters in {'\\n', '\\u{1b}'} (on create)"
    );
    let pattern = rule(InputPredicate::Matches(PatternSpec::new(
        "a\u{1b}",
        PatternMatch::Substring {
            reason: PatternReason::new("why\n\u{1b}[31m\u{202e}").unwrap(),
        },
    )));
    assert_eq!(
        InputRuleDisplay::new(&pattern).to_string(),
        "value at name: must match \"a\\u{1b}\" (Substring: \"why\\n\\u{1b}[31m\\u{202e}\") (on create)"
    );
    let literal = InputLiteral::String("provider\u{1b}".into());
    let declaration =
        rule(InputPredicate::Equals(literal.clone())).with_condition(InputCondition::Equals {
            path: path("guard"),
            literal,
        });
    assert_eq!(
        InputRuleDisplay::new(&declaration).to_string(),
        "value at name: must equal \"provider\\u{1b}\" when guard equals \"provider\\u{1b}\" (on create)"
    );
}

#[test]
fn input_rule_display_truncates_the_shared_requirement_once() {
    let token_text = "é".repeat(MAX_PROVIDER_TEXT_BYTES);
    let declaration = rule(InputPredicate::NoSuffix(token(&token_text)));
    let requirement = DiagnosticText::new(format!("must not end with {token_text:?}"));
    assert!(requirement.as_str().contains("… (truncated, "));
    assert!(requirement.as_str().len() <= MAX_PROVIDER_TEXT_BYTES);
    let expected = format!("value at name: {requirement} (on create)");
    assert_eq!(InputRuleDisplay::new(&declaration).to_string(), expected);
    let schema = schema(declaration);
    assert_eq!(
        InputRuleDisplay::new(&schema.as_schema().input_rules()[0]).to_string(),
        expected
    );
    assert_eq!(
        reject(&schema, Some(string(&token_text)))
            .message()
            .to_string(),
        requirement.as_str()
    );
}
