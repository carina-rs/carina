//! String-literal parsers and unescape helpers.
//!
//! Handles both single-quoted (literal-only) and double-quoted
//! (interpolated) string forms, plus the related unescape routines.

use crate::parser::{ParseContext, ParseError, Rule, first_inner, parse_expression};
use crate::resource::{
    AuthoredConcreteValue as ConcreteValue, AuthoredDeferredValue as DeferredValue,
    AuthoredInterpolationPart as InterpolationPart, AuthoredUnknownReason as UnknownReason,
    AuthoredValue as Value, ValueString,
};

/// Encode a string value as canonical, parseable Carina source.
///
/// Returns `None` when `value` contains a control character for which Carina
/// has no escape sequence.
pub fn encode_string_literal(value: &str) -> Option<String> {
    let has_unrepresentable_control = value
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'));
    if has_unrepresentable_control {
        return None;
    }

    let use_single_quotes = !value.contains(['\'', '\n', '\r', '\t']) && !value.contains("${");

    if use_single_quotes {
        return Some(format!("'{}'", value.replace('\\', "\\\\")));
    }

    let escaped =
        escape_double_quoted_literal_part(value, true, UnrepresentableControl::PreserveAuthored);
    Some(format!("\"{escaped}\""))
}

/// Render a string value safely for diagnostics and other human-readable output.
///
/// The result uses canonical Carina source when possible. Unrepresentable
/// controls are rendered as `\u{...}` escapes, so the fallback is display-only
/// and is not guaranteed to parse as Carina source.
pub fn display_string_literal(value: &str) -> String {
    encode_string_literal(value)
        .unwrap_or_else(|| format!("\"{}\"", display_double_quoted_literal_part(value)))
}

#[derive(Clone, Copy)]
enum UnrepresentableControl {
    EscapeForDisplay,
    PreserveAuthored,
}

fn escape_double_quoted_literal_part(
    value: &str,
    escape_interpolation: bool,
    unrepresentable: UnrepresentableControl,
) -> String {
    let mut rendered = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(escaped) =
            escape_double_quoted_character(ch, chars.peek().copied(), escape_interpolation)
        {
            rendered.push_str(escaped);
        } else if ch.is_control() {
            match unrepresentable {
                UnrepresentableControl::EscapeForDisplay => {
                    rendered.push_str(&format!("\\u{{{:x}}}", ch as u32));
                }
                UnrepresentableControl::PreserveAuthored => rendered.push(ch),
            }
        } else {
            rendered.push(ch);
        }
    }
    rendered
}

pub(crate) fn escape_double_quoted_character(
    ch: char,
    next: Option<char>,
    escape_interpolation: bool,
) -> Option<&'static str> {
    match ch {
        '\\' => Some("\\\\"),
        '"' => Some("\\\""),
        '\n' => Some("\\n"),
        '\r' => Some("\\r"),
        '\t' => Some("\\t"),
        '$' if escape_interpolation && next == Some('{') => Some("\\$"),
        _ => None,
    }
}

pub(crate) fn display_double_quoted_literal_part(value: &str) -> String {
    escape_double_quoted_literal_part(value, true, UnrepresentableControl::EscapeForDisplay)
}

pub(crate) fn parse_string_literal(
    pair: pest::iterators::Pair<Rule>,
) -> Result<String, ParseError> {
    // string = single_quoted_string | double_quoted_string
    let inner_pair = first_inner(pair, "quoted string", "string literal")?;

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
            let inner = first_inner(part, "string content", "string literal")?;
            match inner.as_rule() {
                Rule::string_literal => result.push_str(&unescape_string(inner.as_str())),
                Rule::interpolation => {
                    let line = inner.as_span().start_pos().line_col().0;
                    return Err(ParseError::InterpolationNotAllowedInLiteralString { line });
                }
                other => {
                    let line = inner.as_span().start_pos().line_col().0;
                    return Err(ParseError::InvalidExpression {
                        line,
                        message: format!("Unexpected rule in string literal: {other:?}"),
                    });
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
        return Ok(Value::Concrete(ConcreteValue::String(content.into())));
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
                    parts.push(InterpolationPart::Literal(s.into()));
                }
                Rule::interpolation => {
                    has_interpolation = true;
                    // Grammar makes the inner expression optional so
                    // mid-edit `${}` doesn't poison the AST. An empty
                    // interpolation lands as `Value::Deferred(DeferredValue::Unknown(EmptyInterpolation))`
                    // — the LSP surfaces it as a diagnostic; downstream
                    // resolvers carry it as an unresolved value. See #2480.
                    let value = match inner.into_inner().next() {
                        Some(expr_wrapper) => {
                            let expr_pair = first_inner(
                                expr_wrapper,
                                "expression",
                                "interpolation expression",
                            )?;
                            parse_expression(expr_pair, ctx)?
                        }
                        None => Value::Deferred(DeferredValue::Unknown(
                            UnknownReason::EmptyInterpolation,
                        )),
                    };
                    parts.push(InterpolationPart::Expr(value));
                }
                _ => {}
            }
        }
    }

    if has_interpolation {
        // Canonicalization only folds concrete scalar interpolation
        // parts and preserves deferred refs/placeholders as Expr parts,
        // so for-expansion substitution still finds them intact
        // (covered by expand_deferred_for_substitutes_placeholder_inside_interpolation).
        // Folding fully concrete interpolations here preserves #2227's
        // canonical shape without the removed #3464 forward-ref pass.
        Ok(Value::Deferred(DeferredValue::Interpolation(parts)).canonicalize())
    } else {
        // No interpolation — collapse to a plain String
        let mut s: ValueString = "".into();
        for part in parts {
            match part {
                InterpolationPart::Literal(part) => s.push_derived(&part),
                _ => unreachable!(),
            }
        }
        Ok(Value::Concrete(ConcreteValue::String(s)))
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

#[cfg(test)]
mod tests {
    use super::{
        UnrepresentableControl, display_string_literal, encode_string_literal,
        escape_double_quoted_literal_part,
    };
    use crate::parser::{ProviderContext, parse};
    use crate::resource::{ConcreteValue, Value};

    #[test]
    fn source_encoder_renders_canonical_string_literals() {
        assert_eq!(
            encode_string_literal("production").as_deref(),
            Some("'production'")
        );
        assert_eq!(encode_string_literal("it's").as_deref(), Some("\"it's\""));
        assert_eq!(encode_string_literal(r"a\b").as_deref(), Some(r"'a\\b'"));
        assert_eq!(
            encode_string_literal(r#"say "hi""#).as_deref(),
            Some(r#"'say "hi"'"#)
        );
        assert_eq!(
            encode_string_literal("line\nnext").as_deref(),
            Some(r#""line\nnext""#)
        );
        assert_eq!(
            encode_string_literal("${value}").as_deref(),
            Some(r#""\${value}""#)
        );
    }

    #[test]
    fn source_encoder_rejects_unrepresentable_control_characters() {
        assert_eq!(encode_string_literal("before\x1bafter"), None);
    }

    #[test]
    fn display_renderer_escapes_control_characters_for_diagnostics() {
        assert_eq!(
            display_string_literal("before\x1bafter"),
            r#""before\u{1b}after""#
        );
    }

    #[test]
    fn shared_double_quote_escaper_returns_rendered_text() {
        let escaped: String = escape_double_quoted_literal_part(
            "literal-${value}\x1b",
            true,
            UnrepresentableControl::EscapeForDisplay,
        );
        assert_eq!(escaped, r"literal-\${value}\u{1b}");
    }

    #[test]
    fn rendered_string_literals_round_trip_through_the_parser() {
        // Other control characters are diagnostic-only because Carina has no
        // escape syntax that can represent them.
        let values = [
            "",
            "production",
            "it's",
            r"a\b",
            r#"say "hi""#,
            "line\nnext",
            "carriage\rreturn",
            "tab\tseparated",
            "${value}",
            r#"'\"\n\r\t${value}"#,
            "こんにちは",
        ];

        for value in values {
            let literal = encode_string_literal(value).expect("value should be source-encodable");
            let source = format!("let rendered = {literal}\n");
            let parsed = parse(&source, &ProviderContext::default())
                .unwrap_or_else(|error| panic!("failed to parse {literal}: {error}"));

            assert_eq!(
                parsed.variables.get("rendered"),
                Some(&Value::Concrete(ConcreteValue::String(
                    value.to_string().into()
                ))),
                "literal {literal} did not round-trip"
            );
        }
    }
}
