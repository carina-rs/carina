//! Parser for `wait <target> { until = ..., depends_on = [...], timeout = ... }`.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md` for the surface
//! design. The grammar piece lives in `carina.pest::wait_expr`.

use super::primary::parse_duration_secs;
use crate::parser::error::ParseError;
use crate::parser::{ParserUntilPredicate as UntilPredicateAst, ParserWaitBinding as WaitBinding};
use crate::parser::{Rule, first_inner};
use crate::resource::{
    ConcreteValue as GenericConcreteValue, NoPlaceholder, RelativeBindingName,
    Value as GenericValue,
};
use pest::iterators::Pair;

type Value = GenericValue<NoPlaceholder, RelativeBindingName>;
type ConcreteValue = GenericConcreteValue<NoPlaceholder, RelativeBindingName>;

/// Parse a `wait` expression into a [`WaitBinding`].
///
/// The pest grammar guarantees:
/// - Exactly one `identifier` follows `wait` (the target name).
/// - Each `wait_attr` is one of `wait_until_attr`, `wait_timeout_attr`,
///   `wait_depends_on_attr`.
///
/// Semantic rules enforced here:
/// - `until` is required; absence is an [`ParseError::InvalidExpression`].
/// - `until` must be `<target-binding>.<attr-path> == <value>` (MVP).
/// - Each `wait_attr` may appear at most once; duplicates are rejected.
pub(crate) fn parse_wait_expr(
    pair: Pair<'_, Rule>,
    binding_name: &str,
) -> Result<WaitBinding, ParseError> {
    debug_assert_eq!(pair.as_rule(), Rule::wait_expr);
    let (line, _) = pair.as_span().start_pos().line_col();
    let mut inner = pair.into_inner();

    let target_pair = inner.next().ok_or_else(|| ParseError::InternalError {
        expected: "wait target identifier".to_string(),
        context: "wait_expr".to_string(),
    })?;
    let target = target_pair.as_str().to_string();

    let mut until_raw: Option<String> = None;
    let mut until_predicate: Option<UntilPredicateAst> = None;
    let mut timeout_secs: Option<u64> = None;
    let mut timeout_seen = false;
    let mut depends_on: Vec<RelativeBindingName> = Vec::new();
    let mut depends_on_seen = false;

    for attr_pair in inner {
        debug_assert_eq!(attr_pair.as_rule(), Rule::wait_attr);
        let child = first_inner(attr_pair, "wait_attr inner", "wait_expr")?;
        match child.as_rule() {
            Rule::wait_until_attr => {
                if until_predicate.is_some() {
                    return Err(ParseError::InvalidExpression {
                        line,
                        message: format!("duplicate `until` in `wait {}` block", target),
                    });
                }
                let attr_line = child.as_span().start_pos().line_col().0;
                let raw = child.as_str();
                // Strip leading `until` and `=` so the surface form starts
                // with the predicate itself (e.g.
                // `cert.status == aws.acm.Certificate.Status.Issued`).
                let predicate_text = raw
                    .split_once('=')
                    .map(|(_, rest)| rest)
                    .unwrap_or(raw)
                    .trim()
                    .to_string();
                let expr_pair = first_inner(child, "validate_expr", "wait_until_attr")?;
                let predicate = lower_validate_expr_to_until(expr_pair, &target, attr_line)?;
                until_raw = Some(predicate_text);
                until_predicate = Some(predicate);
            }
            Rule::wait_timeout_attr => {
                if timeout_seen {
                    return Err(ParseError::InvalidExpression {
                        line,
                        message: format!("duplicate `timeout` in `wait {}` block", target),
                    });
                }
                timeout_seen = true;
                let dur_pair = first_inner(child, "duration_literal", "wait_timeout_attr")?;
                let secs = parse_duration_secs(dur_pair.as_str(), line)?;
                timeout_secs = Some(secs);
            }
            Rule::wait_depends_on_attr => {
                if depends_on_seen {
                    return Err(ParseError::InvalidExpression {
                        line,
                        message: format!("duplicate `depends_on` in `wait {}` block", target),
                    });
                }
                depends_on_seen = true;
                for ident in child.into_inner() {
                    if ident.as_rule() == Rule::identifier {
                        depends_on.push(RelativeBindingName::new(ident.as_str()));
                    }
                }
            }
            other => {
                return Err(ParseError::InternalError {
                    expected: "wait_until_attr | wait_timeout_attr | wait_depends_on_attr"
                        .to_string(),
                    context: format!("wait_attr child rule {:?}", other),
                });
            }
        }
    }

    let (Some(until_raw), Some(until_predicate)) = (until_raw, until_predicate) else {
        return Err(ParseError::InvalidExpression {
            line,
            message: format!("`wait {}` block requires `until = <predicate>`", target),
        });
    };

    Ok(WaitBinding {
        binding: RelativeBindingName::from(binding_name),
        target: RelativeBindingName::from(target),
        until_raw,
        until_predicate,
        timeout_secs,
        depends_on,
        line,
    })
}

/// Walk a `validate_expr` pest tree and lower it into an
/// [`UntilPredicateAst`].
///
/// MVP narrowing: the only accepted shape is
/// `<target-binding>.<attr-path> == <value>`. Boolean combinators,
/// `!=`, `>=`, `<=`, comparisons, `in`, function calls, and bare
/// variable references all produce a descriptive parse error tied to
/// the `until` span.
fn lower_validate_expr_to_until(
    pair: Pair<'_, Rule>,
    target_binding: &str,
    line: usize,
) -> Result<UntilPredicateAst, ParseError> {
    let cmp = descend_to_compare(pair, line)?;
    // `validate_comparison` has shape:
    // validate_additive_expr (compare_op validate_additive_expr)?
    let mut iter = cmp.into_inner();
    let lhs_pair = iter.next().ok_or_else(|| ParseError::InvalidExpression {
        line,
        message: "`until`: missing LHS of comparison".to_string(),
    })?;
    let op_pair = iter.next().ok_or_else(|| ParseError::InvalidExpression {
        line,
        message: "`until`: comparison operator required (only `==` is supported)".to_string(),
    })?;
    let rhs_pair = iter.next().ok_or_else(|| ParseError::InvalidExpression {
        line,
        message: "`until`: missing RHS of comparison".to_string(),
    })?;

    if op_pair.as_rule() != Rule::compare_op || op_pair.as_str() != "==" {
        return Err(ParseError::InvalidExpression {
            line,
            message: format!(
                "`until`: only `==` is supported in this version (got `{}`)",
                op_pair.as_str()
            ),
        });
    }

    let lhs_pair = unwrap_wait_arithmetic(lhs_pair, line)?;
    let rhs_pair = unwrap_wait_arithmetic(rhs_pair, line)?;
    let mut lhs_segments = lower_until_lhs(lhs_pair, target_binding, line)?;
    let root = RelativeBindingName::new(lhs_segments.remove(0));
    let rhs = lower_until_rhs(rhs_pair, line)?;

    Ok(UntilPredicateAst {
        root,
        attribute_segments: lhs_segments,
        rhs,
    })
}

/// Remove the validation-only arithmetic precedence wrappers when they contain
/// no operator. An authored arithmetic operation is deliberately rejected:
/// wait polling remains a narrow equality predicate in this release.
fn unwrap_wait_arithmetic<'i>(
    pair: Pair<'i, Rule>,
    line: usize,
) -> Result<Pair<'i, Rule>, ParseError> {
    fn contains_integer_arithmetic(pair: &Pair<'_, Rule>) -> bool {
        if matches!(
            pair.as_rule(),
            Rule::validate_additive_expr | Rule::validate_multiplicative_expr
        ) && pair.clone().into_inner().count() > 1
        {
            return true;
        }
        pair.clone()
            .into_inner()
            .any(|inner| contains_integer_arithmetic(&inner))
    }

    // Parentheses introduce another complete validation-expression wrapper.
    // Scan the whole operand first so they cannot hide an arithmetic node from
    // the targeted wait-predicate diagnostic.
    if contains_integer_arithmetic(&pair) {
        return Err(ParseError::InvalidExpression {
            line,
            message: "`until`: integer arithmetic is not supported in wait predicates".to_string(),
        });
    }

    match pair.as_rule() {
        Rule::validate_additive_expr | Rule::validate_multiplicative_expr => {
            let mut inner = pair.into_inner();
            let first = inner.next().ok_or_else(|| ParseError::InvalidExpression {
                line,
                message: "`until`: empty predicate operand".to_string(),
            })?;
            unwrap_wait_arithmetic(first, line)
        }
        Rule::validate_primary => Ok(pair),
        other => Err(ParseError::InvalidExpression {
            line,
            message: format!("`until`: unexpected predicate operand ({other:?})"),
        }),
    }
}

/// Walk through `validate_expr` → `validate_or_expr` → `validate_and_expr`
/// → `validate_not_expr` → `validate_comparison`, rejecting any
/// boolean combinator or negation along the way.
fn descend_to_compare<'i>(pair: Pair<'i, Rule>, line: usize) -> Result<Pair<'i, Rule>, ParseError> {
    fn step<'i>(pair: Pair<'i, Rule>, line: usize) -> Result<Pair<'i, Rule>, ParseError> {
        match pair.as_rule() {
            Rule::validate_expr
            | Rule::validate_or_expr
            | Rule::validate_and_expr
            | Rule::validate_not_expr => {
                let mut inner = pair.into_inner();
                let first = inner.next().ok_or_else(|| ParseError::InvalidExpression {
                    line,
                    message: "`until`: empty predicate".to_string(),
                })?;
                if inner.next().is_some() {
                    return Err(ParseError::InvalidExpression {
                        line,
                        message: "`until`: boolean combinators (`&&`/`||`) and `!` are not supported in this version"
                            .to_string(),
                    });
                }
                step(first, line)
            }
            Rule::validate_comparison => Ok(pair),
            _ => Err(ParseError::InvalidExpression {
                line,
                message: format!(
                    "`until`: unexpected predicate shape (rule {:?})",
                    pair.as_rule()
                ),
            }),
        }
    }
    step(pair, line)
}

/// Extract the dotted segments from the LHS `validate_primary`. The
/// first segment must equal `target_binding`.
fn lower_until_lhs(
    pair: Pair<'_, Rule>,
    target_binding: &str,
    line: usize,
) -> Result<Vec<String>, ParseError> {
    // validate_primary wraps a single child; descend to it.
    let primary = first_inner(pair, "validate_primary inner", "until LHS")?;
    if primary.as_rule() != Rule::variable_ref {
        return Err(ParseError::InvalidExpression {
            line,
            message: format!(
                "`until`: LHS must be `<target>.<attribute>` (got {:?})",
                primary.as_rule()
            ),
        });
    }
    let mut segments: Vec<String> = Vec::new();
    for child in primary.into_inner() {
        match child.as_rule() {
            Rule::identifier => segments.push(child.as_str().to_string()),
            Rule::field_access => {
                let id = first_inner(child, "identifier", "field_access")?;
                segments.push(id.as_str().to_string());
            }
            Rule::index_access => {
                return Err(ParseError::InvalidExpression {
                    line,
                    message: "`until`: index access in LHS is not supported".to_string(),
                });
            }
            other => {
                return Err(ParseError::InternalError {
                    expected: "identifier | field_access | index_access".to_string(),
                    context: format!("variable_ref child {:?}", other),
                });
            }
        }
    }
    if segments.is_empty() {
        return Err(ParseError::InvalidExpression {
            line,
            message: "`until`: LHS is empty".to_string(),
        });
    }
    if segments.len() < 2 {
        return Err(ParseError::InvalidExpression {
            line,
            message: format!(
                "`until`: LHS must reference an attribute of `{}` (e.g. `{0}.status`), got bare binding `{}`",
                target_binding, segments[0]
            ),
        });
    }
    if segments[0] != target_binding {
        return Err(ParseError::InvalidExpression {
            line,
            message: format!(
                "`until`: LHS must reference target `{}` (got `{}`); cross-target predicates are not supported",
                target_binding, segments[0]
            ),
        });
    }
    Ok(segments)
}

/// Extract the literal RHS as a `Value`. Supports string, int, float,
/// bool, duration, and namespaced-id enums (e.g.
/// `aws.acm.Certificate.Status.Issued`).
fn lower_until_rhs(pair: Pair<'_, Rule>, line: usize) -> Result<Value, ParseError> {
    let primary = first_inner(pair, "validate_primary inner", "until RHS")?;
    match primary.as_rule() {
        Rule::string => {
            // Strip surrounding quotes — same convention as
            // validate_expr's string handling.
            let raw = primary.as_str();
            if raw.len() < 2 {
                return Err(ParseError::InvalidExpression {
                    line,
                    message: "`until`: malformed string literal".to_string(),
                });
            }
            Ok(Value::Concrete(ConcreteValue::String(
                raw[1..raw.len() - 1].to_string().into(),
            )))
        }
        Rule::number => {
            let n: i64 = primary
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line,
                    message: format!("`until`: invalid integer: {}", e),
                })?;
            Ok(Value::Concrete(ConcreteValue::Int(n)))
        }
        Rule::float => {
            let f: f64 = primary
                .as_str()
                .parse()
                .map_err(|e| ParseError::InvalidExpression {
                    line,
                    message: format!("`until`: invalid float: {}", e),
                })?;
            Ok(Value::Concrete(ConcreteValue::Float(f)))
        }
        Rule::boolean => Ok(Value::Concrete(ConcreteValue::Bool(
            primary.as_str() == "true",
        ))),
        Rule::duration_literal => {
            let secs = parse_duration_secs(primary.as_str(), line)?;
            Ok(Value::Concrete(ConcreteValue::Duration(
                std::time::Duration::from_secs(secs),
            )))
        }
        Rule::variable_ref => {
            // Treat every unquoted variable-ref shape as an enum identifier,
            // from a bare `ready` through a fully-qualified
            // `aws.acm.Certificate.Status.Issued`. The plan pipeline resolves
            // it through schema metadata or rejects it with quoting guidance.
            let mut segments: Vec<String> = Vec::new();
            for child in primary.into_inner() {
                match child.as_rule() {
                    Rule::identifier => segments.push(child.as_str().to_string()),
                    Rule::field_access => {
                        let id = first_inner(child, "identifier", "field_access")?;
                        segments.push(id.as_str().to_string());
                    }
                    Rule::index_access => {
                        return Err(ParseError::InvalidExpression {
                            line,
                            message: "`until`: index access in RHS is not supported".to_string(),
                        });
                    }
                    other => {
                        return Err(ParseError::InternalError {
                            expected: "identifier | field_access | index_access".to_string(),
                            context: format!("variable_ref child {:?}", other),
                        });
                    }
                }
            }
            // Preserve the source distinction between an unquoted enum
            // identifier and a quoted string. Schema-driven enum resolution
            // downstream must dispatch on this variant, never on text shape.
            Ok(Value::Concrete(ConcreteValue::enum_identifier(
                segments.join("."),
            )))
        }
        Rule::null_literal => Err(ParseError::InvalidExpression {
            line,
            message: "`until`: `null` is not a valid comparison value".to_string(),
        }),
        Rule::validate_function_call => Err(ParseError::InvalidExpression {
            line,
            message: "`until`: function calls in the RHS are not supported".to_string(),
        }),
        other => Err(ParseError::InvalidExpression {
            line,
            message: format!("`until`: unsupported RHS shape ({:?})", other),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::grammar::parse_rule_for_test;
    use crate::parser::preprocess_file;

    fn parse_one(src: &str) -> WaitBinding {
        let pair = parse_rule_for_test(Rule::wait_expr, src)
            .expect("pest should parse wait_expr")
            .next()
            .expect("at least one pair");
        parse_wait_expr(pair, "cert_issued").expect("parse_wait_expr should succeed")
    }

    #[test]
    fn parses_target_and_until() {
        let we = parse_one(
            "wait cert {\n    until = cert.status == aws.acm.Certificate.Status.Issued\n}",
        );
        assert_eq!(we.binding, "cert_issued");
        assert_eq!(we.target, "cert");
        assert_eq!(
            we.until_raw,
            "cert.status == aws.acm.Certificate.Status.Issued"
        );
        assert_eq!(we.until_predicate.root, "cert");
        assert_eq!(we.until_predicate.attribute_segments, vec!["status"]);
        // The RHS is kept as the typed RAW dotted identifier here; enum-alias
        // resolution to the canonical AWS value happens downstream when
        // `create_plan` resolves the predicate against the target schema.
        assert!(matches!(
            &we.until_predicate.rhs,
            Value::Concrete(ConcreteValue::EnumIdentifier(raw))
                if raw.as_str() == "aws.acm.Certificate.Status.Issued"
        ));
        assert!(we.timeout_secs.is_none());
        assert!(we.depends_on.is_empty());
    }

    #[test]
    fn parses_string_rhs() {
        let we = parse_one("wait cert {\n    until = cert.status == \"ISSUED\"\n}");
        assert_eq!(
            we.until_predicate.rhs,
            Value::Concrete(ConcreteValue::String("ISSUED".into()))
        );
    }

    #[test]
    fn parses_quoted_dotted_string_rhs_as_string() {
        let we = parse_one("wait cert {\n    until = cert.status == 'A.json'\n}");
        assert!(matches!(
            &we.until_predicate.rhs,
            Value::Concrete(ConcreteValue::String(value))
                if value.expose_plaintext() == "A.json"
        ));
    }

    #[test]
    fn parses_bare_identifier_rhs_as_enum_identifier() {
        let we = parse_one("wait job {\n    until = job.state == ready\n}");
        assert!(matches!(
            &we.until_predicate.rhs,
            Value::Concrete(ConcreteValue::EnumIdentifier(raw)) if raw.as_str() == "ready"
        ));
    }

    #[test]
    fn parses_int_rhs() {
        let we = parse_one("wait job {\n    until = job.completed_steps == 10\n}");
        assert_eq!(
            we.until_predicate.rhs,
            Value::Concrete(ConcreteValue::Int(10))
        );
    }

    #[test]
    fn parses_bool_rhs() {
        let we = parse_one("wait flag {\n    until = flag.enabled == true\n}");
        assert_eq!(
            we.until_predicate.rhs,
            Value::Concrete(ConcreteValue::Bool(true))
        );
    }

    #[test]
    fn parses_timeout_in_seconds() {
        let we =
            parse_one("wait cert {\n    until = cert.status == ISSUED\n    timeout = 75min\n}");
        assert_eq!(we.timeout_secs, Some(75 * 60));
    }

    #[test]
    fn parses_depends_on_list() {
        let we = parse_one(
            "wait cert {\n    until = cert.status == ISSUED\n    depends_on = [a, b, c]\n}",
        );
        assert_eq!(we.depends_on, vec!["a", "b", "c"]);
    }

    #[test]
    fn parses_nested_attr_path() {
        let we =
            parse_one("wait cert {\n    until = cert.renewal_summary.renewal_status == SUCCESS\n}");
        assert_eq!(
            we.until_predicate.attribute_segments,
            vec!["renewal_summary", "renewal_status"]
        );
        assert_eq!(we.until_predicate.root, "cert");
    }

    #[test]
    fn rejects_missing_until() {
        let pair = parse_rule_for_test(Rule::wait_expr, "wait cert {\n    timeout = 30s\n}")
            .unwrap()
            .next()
            .unwrap();
        let err = parse_wait_expr(pair, "cert_issued").expect_err("missing until");
        assert!(
            err.to_string().contains("until"),
            "error should mention `until`, got: {}",
            err
        );
    }

    #[test]
    fn rejects_duplicate_until() {
        let pair = parse_rule_for_test(
            Rule::wait_expr,
            "wait cert {\n    until = cert.status == A\n    until = cert.status == B\n}",
        )
        .unwrap()
        .next()
        .unwrap();
        let err = parse_wait_expr(pair, "cert_issued").expect_err("duplicate until");
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn rejects_lhs_unrelated_to_target() {
        let pair = parse_rule_for_test(
            Rule::wait_expr,
            "wait cert {\n    until = other.status == ISSUED\n}",
        )
        .unwrap()
        .next()
        .unwrap();
        let err = parse_wait_expr(pair, "cert_issued").expect_err("cross-target");
        assert!(err.to_string().contains("must reference target"));
    }

    #[test]
    fn rejects_bare_binding_lhs() {
        let pair = parse_rule_for_test(
            Rule::wait_expr,
            "wait cert {\n    until = cert == ISSUED\n}",
        )
        .unwrap()
        .next()
        .unwrap();
        let err = parse_wait_expr(pair, "cert_issued").expect_err("bare binding");
        assert!(err.to_string().contains("attribute"));
    }

    #[test]
    fn rejects_non_eq_operator() {
        let pair = parse_rule_for_test(
            Rule::wait_expr,
            "wait cert {\n    until = cert.status != FAILED\n}",
        )
        .unwrap()
        .next()
        .unwrap();
        let err = parse_wait_expr(pair, "cert_issued").expect_err("non-eq");
        assert!(err.to_string().contains("only `==` is supported"));
    }

    #[test]
    fn rejects_boolean_combinator() {
        let pair = parse_rule_for_test(
            Rule::wait_expr,
            "wait cert {\n    until = cert.status == A && cert.status == B\n}",
        )
        .unwrap()
        .next()
        .unwrap();
        let err = parse_wait_expr(pair, "cert_issued").expect_err("&&");
        assert!(err.to_string().contains("boolean combinators"));
    }

    #[test]
    fn pest_grammar_rejects_wait_without_target() {
        let preprocessed_file =
            preprocess_file("let foo = wait { until = x.y == z }").expect("preprocessing succeeds");
        let result = preprocessed_file.parse();
        assert!(
            result.is_err(),
            "pest should reject `wait` without an identifier"
        );
    }

    #[test]
    fn pest_grammar_accepts_wait_inside_let_binding() {
        let preprocessed_file = preprocess_file(
            "let cert_issued = wait cert {\n    until = cert.status == ISSUED\n    timeout = 75min\n    depends_on = [validation_record]\n}",
        )
        .expect("preprocessing succeeds");
        let result = preprocessed_file.parse();
        assert!(result.is_ok(), "expected parse success, got {:?}", result);
    }
}
