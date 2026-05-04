//! Small parser-internal utilities.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::Rule;
use super::context::next_pair;
use super::error::ParseError;
use super::expressions::string_literal::{parse_string_literal, unescape_single_quoted};
use crate::eval_value::EvalValue;
use crate::resource::{ResourceId, Value};

/// Convert PascalCase to snake_case (e.g., "VpcId" → "vpc_id", "AwsAccountId" → "aws_account_id").
pub fn pascal_to_snake(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_lowercase().next().unwrap());
        } else {
            result.push(c);
        }
    }
    result
}

/// Convert snake_case to PascalCase (e.g., "vpc_id" → "VpcId", "aws_account_id" → "AwsAccountId").
///
/// Acronyms are treated as regular words (`iam_policy_arn` → `IamPolicyArn`,
/// `ipv4_cidr` → `Ipv4Cidr`) so that the result matches `semantic_name` values
/// already produced by `pascal_to_snake` and is a round-trip inverse for them.
pub fn snake_to_pascal(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut capitalize_next = true;
    for c in s.chars() {
        if c == '_' {
            capitalize_next = true;
        } else if capitalize_next {
            result.push(c.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(c);
        }
    }
    result
}

/// Return a human-readable type name for a Value
pub(crate) fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::List(_) => "list",
        Value::Map(_) => "map",
        Value::ResourceRef { .. } => "resource reference",
        Value::Interpolation(_) => "string",
        Value::FunctionCall { .. } => "function call",
        Value::Secret(_) => "secret",
        Value::Unknown(_) => {
            unimplemented!("Value::Unknown handling lands in RFC #2371 stage 2/3")
        }
    }
}

/// Return a human-readable type name for an `EvalValue`. Closures only
/// exist on the evaluator-internal type, so this is the version used by
/// pipe/compose error messages where a closure can legitimately show up.
pub(crate) fn eval_type_name(value: &EvalValue) -> &'static str {
    match value {
        EvalValue::User(v) => value_type_name(v),
        EvalValue::Closure { .. } => "closure",
    }
}

/// Split a namespaced identifier (e.g., "awscc.ec2.Vpc") into (provider, resource_type)
pub(crate) fn split_namespaced_id(namespaced: &str) -> (String, String) {
    let parts: Vec<&str> = namespaced.split('.').collect();
    if parts.len() >= 2 {
        (parts[0].to_string(), parts[1..].join("."))
    } else {
        (String::new(), namespaced.to_string())
    }
}

/// Parse a resource address: `provider.service.type "name"`
pub(crate) fn parse_resource_address(
    pair: pest::iterators::Pair<Rule>,
) -> Result<ResourceId, ParseError> {
    let mut inner = pair.into_inner();
    let namespaced = next_pair(&mut inner, "namespaced id", "resource address")?
        .as_str()
        .to_string();
    let name_pair = next_pair(&mut inner, "resource name", "resource address")?;
    // The name is a string literal - extract value from quotes
    let raw_name = parse_string_literal(name_pair)?;
    // Normalize map-key trailing segment so all three input shapes
    // (`binding.key`, `binding['key']`, `binding["key"]`) collapse
    // to the canonical form before state lookup. See #1903.
    let name = crate::utils::canonicalize_map_key_address(&raw_name);

    // Split namespaced id into provider and resource_type
    let (provider, resource_type) = split_namespaced_id(&namespaced);

    Ok(ResourceId::with_provider(provider, resource_type, name))
}

pub(crate) fn extract_string_from_pair(
    pair: pest::iterators::Pair<Rule>,
) -> Result<String, ParseError> {
    // Walk through expression -> pipe_expr -> primary -> string -> string_part -> string_literal
    fn find_string(pair: pest::iterators::Pair<Rule>) -> Option<String> {
        if pair.as_rule() == Rule::string_literal {
            return Some(pair.as_str().to_string());
        }
        if pair.as_rule() == Rule::single_quoted_content {
            return Some(unescape_single_quoted(pair.as_str()));
        }
        if pair.as_rule() == Rule::single_quoted_string {
            return pair
                .into_inner()
                .next()
                .map(|p| unescape_single_quoted(p.as_str()));
        }
        if pair.as_rule() == Rule::string {
            let mut result = String::new();
            for inner in pair.into_inner() {
                if let Some(s) = find_string(inner) {
                    result.push_str(&s);
                }
            }
            return Some(result);
        }
        if pair.as_rule() == Rule::string_part {
            for inner in pair.into_inner() {
                if let Some(s) = find_string(inner) {
                    return Some(s);
                }
            }
            return None;
        }
        for inner in pair.into_inner() {
            if let Some(s) = find_string(inner) {
                return Some(s);
            }
        }
        None
    }

    find_string(pair).ok_or_else(|| ParseError::InvalidExpression {
        line: 0,
        message: "expected a string literal".to_string(),
    })
}

/// True if a parsed expression is exactly a plain (uninterpolated)
/// quoted string literal — no operators, no interpolation, no
/// list / map wrapping. Used to populate `Resource.quoted_string_attrs`
/// so enum-attribute diagnostics can distinguish a shape mismatch
/// (`attr = "AWS_ACCOUNT"`) from a variant mismatch
/// (`attr = AWS_ACCOUNT`); see #2094 / #2229.
pub(crate) fn expression_is_plain_string_literal(pair: pest::iterators::Pair<Rule>) -> bool {
    let Some(primary) = unwrap_to_primary(pair) else {
        return false;
    };
    let Some(inner) = primary.into_inner().next() else {
        return false;
    };
    if inner.as_rule() != Rule::string {
        return false;
    }
    let Some(string_inner) = inner.into_inner().next() else {
        return false;
    };
    match string_inner.as_rule() {
        Rule::single_quoted_string => true,
        Rule::double_quoted_string => string_inner
            .into_inner()
            .filter(|p| p.as_rule() == Rule::string_part)
            .flat_map(|p| p.into_inner())
            .all(|leaf| leaf.as_rule() != Rule::interpolation),
        _ => false,
    }
}

/// Walk an `expression` (or any intermediate operator rule) down to
/// the first `primary`, returning `None` if any operator (`|>`, `>>`,
/// `??`) has more than one operand — such chains are expressions, not
/// pure literals.
pub(crate) fn unwrap_to_primary(
    pair: pest::iterators::Pair<Rule>,
) -> Option<pest::iterators::Pair<Rule>> {
    match pair.as_rule() {
        Rule::primary => Some(pair),
        Rule::expression | Rule::coalesce_expr | Rule::pipe_expr | Rule::compose_expr => {
            let mut inner = pair.into_inner();
            let first = inner.next()?;
            if inner.next().is_some() {
                return None;
            }
            unwrap_to_primary(first)
        }
        _ => None,
    }
}
