//! String-literal parsers and unescape helpers.
//!
//! Handles both single-quoted (literal-only) and double-quoted
//! (interpolated) string forms, plus the related unescape routines.

use crate::parser::{ParseContext, ParseError, Rule, first_inner, parse_expression};
use crate::resource::{InterpolationPart, UnknownReason, Value};

pub(crate) fn parse_string_literal(
    pair: pest::iterators::Pair<Rule>,
) -> Result<String, ParseError> {
    // string = single_quoted_string | double_quoted_string
    let inner_pair = pair.into_inner().next().unwrap();

    if inner_pair.as_rule() == Rule::single_quoted_string {
        return Ok(inner_pair
            .into_inner()
            .next()
            .map(|p| unescape_single_quoted(p.as_str()))
            .unwrap_or_default());
    }

    // Double-quoted string
    let mut result = String::new();
    for part in inner_pair.into_inner() {
        if part.as_rule() == Rule::string_part {
            for inner in part.into_inner() {
                if inner.as_rule() == Rule::string_literal {
                    result.push_str(inner.as_str());
                }
            }
        }
    }
    Ok(result)
}

pub(crate) fn parse_string_value(
    pair: pest::iterators::Pair<Rule>,
    ctx: &ParseContext,
) -> Result<Value, ParseError> {
    // string = single_quoted_string | double_quoted_string
    let inner_pair = first_inner(pair, "string content", "string")?;

    if inner_pair.as_rule() == Rule::single_quoted_string {
        // Single-quoted: literal only, no interpolation
        let content = inner_pair
            .into_inner()
            .next()
            .map(|p| unescape_single_quoted(p.as_str()))
            .unwrap_or_default();
        return Ok(Value::String(content));
    }

    // Double-quoted string (original behavior)
    let mut parts: Vec<InterpolationPart> = Vec::new();
    let mut has_interpolation = false;

    for part in inner_pair.into_inner() {
        if part.as_rule() == Rule::string_part {
            let inner = first_inner(part, "string content", "string_part")?;
            match inner.as_rule() {
                Rule::string_literal => {
                    let s = unescape_string(inner.as_str());
                    parts.push(InterpolationPart::Literal(s));
                }
                Rule::interpolation => {
                    has_interpolation = true;
                    // Grammar makes the inner expression optional so
                    // mid-edit `${}` doesn't poison the AST. An empty
                    // interpolation lands as `Value::Unknown(EmptyInterpolation)`
                    // — the LSP surfaces it as a diagnostic; downstream
                    // resolvers carry it as an unresolved value. See #2480.
                    let value = match inner.into_inner().next() {
                        Some(expr_pair) => parse_expression(expr_pair, ctx)?,
                        None => Value::Unknown(UnknownReason::EmptyInterpolation),
                    };
                    parts.push(InterpolationPart::Expr(value));
                }
                _ => {}
            }
        }
    }

    if has_interpolation {
        // Deliberately do *not* call `Value::canonicalize` here. The
        // deferred-for placeholder substitution in
        // `parser/ast.rs::substitute_placeholder` walks the `Expr`
        // parts to replace `Value::Unknown(For*)` placeholders with the
        // resolved iterable element, so the parts must stay intact
        // through parse → for-expansion. Canonicalization runs later,
        // after resolution (see #2227).
        Ok(Value::Interpolation(parts))
    } else {
        // No interpolation — collapse to a plain String
        let s = parts
            .into_iter()
            .map(|p| match p {
                InterpolationPart::Literal(s) => s,
                _ => unreachable!(),
            })
            .collect::<String>();
        Ok(Value::String(s))
    }
}

/// Handle escape sequences in string literals
pub(crate) fn unescape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('"') => result.push('"'),
                Some('\\') => result.push('\\'),
                Some('$') => result.push('$'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}

/// Handle escape sequences in single-quoted string literals.
/// Only `\'` and `\\` are recognized as escape sequences.
pub(crate) fn unescape_single_quoted(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('\'') => result.push('\''),
                Some('\\') => result.push('\\'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(c);
        }
    }
    result
}
