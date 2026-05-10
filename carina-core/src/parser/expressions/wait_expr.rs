//! Parser for `wait <target> { until = ..., depends_on = [...], timeout = ... }`.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md` for the surface
//! design. The grammar piece lives in `carina.pest::wait_expr`.

use super::primary::parse_duration_secs;
use super::validate_expr::parse_validate_expr;
use crate::parser::ast::WaitBinding;
use crate::parser::error::ParseError;
use crate::parser::{Rule, first_inner};
use pest::iterators::Pair;

/// Parse a `wait` expression into a [`WaitBinding`].
///
/// The pest grammar guarantees:
/// - Exactly one `identifier` follows `wait` (the target name).
/// - Each `wait_attr` is one of `wait_until_attr`, `wait_timeout_attr`,
///   `wait_depends_on_attr`.
///
/// Semantic rules enforced here:
/// - `until` is required; absence is an [`ParseError::InvalidExpression`].
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
    let mut until_ast = None;
    let mut timeout_secs: Option<u64> = None;
    let mut timeout_seen = false;
    let mut depends_on: Vec<String> = Vec::new();
    let mut depends_on_seen = false;

    for attr_pair in inner {
        debug_assert_eq!(attr_pair.as_rule(), Rule::wait_attr);
        let child = first_inner(attr_pair, "wait_attr inner", "wait_expr")?;
        match child.as_rule() {
            Rule::wait_until_attr => {
                if until_ast.is_some() {
                    return Err(ParseError::InvalidExpression {
                        line,
                        message: format!("duplicate `until` in `wait {}` block", target),
                    });
                }
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
                let ast = parse_validate_expr(expr_pair)?;
                until_raw = Some(predicate_text);
                until_ast = Some(ast);
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
                        depends_on.push(ident.as_str().to_string());
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

    let (Some(until_raw), Some(until_ast)) = (until_raw, until_ast) else {
        return Err(ParseError::InvalidExpression {
            line,
            message: format!("`wait {}` block requires `until = <predicate>`", target),
        });
    };

    Ok(WaitBinding {
        binding: binding_name.to_string(),
        target,
        until_raw,
        until_ast,
        timeout_secs,
        depends_on,
        line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::CarinaParser;
    use pest::Parser;

    fn parse_one(src: &str) -> WaitBinding {
        let pair = CarinaParser::parse(Rule::wait_expr, src)
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
        assert!(we.timeout_secs.is_none());
        assert!(we.depends_on.is_empty());
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
    fn rejects_missing_until() {
        let pair = CarinaParser::parse(Rule::wait_expr, "wait cert {\n    timeout = 30s\n}")
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
        let pair = CarinaParser::parse(
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
    fn pest_grammar_rejects_wait_without_target() {
        let result = CarinaParser::parse(Rule::file, "let foo = wait { until = x.y == z }");
        assert!(
            result.is_err(),
            "pest should reject `wait` without an identifier"
        );
    }

    #[test]
    fn pest_grammar_accepts_wait_inside_let_binding() {
        let result = CarinaParser::parse(
            Rule::file,
            "let cert_issued = wait cert {\n    until = cert.status == ISSUED\n    timeout = 75min\n    depends_on = [validation_record]\n}",
        );
        assert!(result.is_ok(), "expected parse success, got {:?}", result);
    }
}
