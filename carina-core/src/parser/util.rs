//! Small parser-internal utilities.
//!
//! Extracted from `parser/mod.rs` per #2263 (part 2/2).

use super::Rule;
use super::context::next_pair;
use super::error::ParseError;
use super::expressions::string_literal::parse_string_literal;
use crate::eval_value::EvalValue;
use crate::parser::ParsedValue as Value;
use crate::resource::{ConcreteValue, DeferredValue};

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
        Value::Concrete(ConcreteValue::String(_)) => "string",
        Value::Concrete(ConcreteValue::EnumIdentifier(_)) => "enum identifier",
        Value::Concrete(ConcreteValue::CanonicalEnum(_)) => "canonical enum",
        Value::Concrete(ConcreteValue::Int(_)) => "int",
        Value::Concrete(ConcreteValue::Float(_)) => "float",
        Value::Concrete(ConcreteValue::Bool(_)) => "bool",
        Value::Concrete(ConcreteValue::Duration(_)) => "duration",
        Value::Concrete(ConcreteValue::List(_)) => "list",
        Value::Concrete(ConcreteValue::StringList(_)) => "list",
        Value::Concrete(ConcreteValue::Map(_)) => "map",
        Value::Deferred(DeferredValue::ModuleIf(_)) => "module conditional",
        Value::Deferred(DeferredValue::ResourceRef { .. }) => "resource reference",
        Value::Deferred(DeferredValue::BindingRef { .. }) => "binding reference",
        Value::Deferred(DeferredValue::Coalesce { .. }) => "coalesce expression",
        Value::Deferred(DeferredValue::IntegerArithmetic(_)) => "int",
        Value::Deferred(DeferredValue::Interpolation(_)) => "string",
        Value::Deferred(
            DeferredValue::FunctionCall { .. } | DeferredValue::ResolvedFunctionCall { .. },
        ) => "function call",
        Value::Deferred(DeferredValue::Unavailable { .. }) => "unavailable function result",
        Value::Deferred(DeferredValue::Access(_)) => "access expression",
        Value::Deferred(DeferredValue::Secret(_)) => "secret",


        Value::Deferred(DeferredValue::Unknown(_)) => "unknown",
        Value::Deferred(DeferredValue::Placeholder(_)) => "placeholder",
    }
}

/// Return a human-readable type name for an `EvalValue`. Closures only
/// exist on the evaluator-internal type, so this is the version used by
/// pipe/compose error messages where a closure can legitimately show up.
pub(crate) fn eval_type_name(value: &EvalValue) -> &'static str {
    match value {
        EvalValue::User(v) => value_type_name(v),
        EvalValue::UnexpandedStructuralHandle(_) => "resource handle",
        EvalValue::PendingValue(_) => "pending value",
        EvalValue::ProvisionalReference { .. } => "unresolved reference",
        EvalValue::ComputedWithUnexpandedStructuralHandles { value, .. } => value_type_name(value),
        EvalValue::DeferredForValue { .. } => "unknown",
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

/// Parse a resource address into a [`StateBlockAddress`] — the
/// routing-agnostic newtype used by `import`/`removed`/`moved` blocks.
///
/// The DSL `resource_address` rule (`provider.service.type "name"`)
/// has no syntax for `provider_instance`, so the returned type
/// deliberately has no routing field at all. A downstream consumer
/// cannot accidentally compare the address against a routed
/// [`ResourceId`] — the carina#3324 bug class is unrepresentable at
/// the type level.
pub(crate) fn parse_state_block_address(
    pair: pest::iterators::Pair<Rule>,
) -> Result<crate::parser::ast::StateBlockAddress, ParseError> {
    let line = pair.as_span().start_pos().line_col().0;
    let mut inner = pair.into_inner();
    let namespaced = next_pair(&mut inner, "namespaced id", "resource address")?
        .as_str()
        .to_string();
    let name_pair = next_pair(&mut inner, "resource name", "resource address")?;
    let raw_name = parse_string_literal(name_pair)?;
    // Name canonicalization lives in `StateBlockAddress::new` so the
    // invariant holds for every constructor, not just this parser
    // path. A future programmatic caller can't bypass it.
    let (provider, resource_type) = split_namespaced_id(&namespaced);
    let name = crate::resource::ResourceIdentity::try_from(raw_name)
        .map_err(|error| ParseError::InvalidResourceIdentity { line, error })?;
    Ok(crate::parser::ast::StateBlockAddress::new(
        provider,
        resource_type,
        name,
    ))
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

/// The first-operand spine of the expression precedence chain.
///
/// Keeping the wrappers as pairs lets the structural-let parser apply every
/// suffix and operator after parsing its special primary, while literal-only
/// callers can require that every wrapper contains exactly one child.
pub(crate) struct ExpressionDescent<'i> {
    wrappers: Vec<pest::iterators::Pair<'i, Rule>>,
    primary: pest::iterators::Pair<'i, Rule>,
}

impl<'i> ExpressionDescent<'i> {
    pub(crate) fn primary(&self) -> pest::iterators::Pair<'i, Rule> {
        self.primary.clone()
    }

    pub(crate) fn wrapper(&self, rule: Rule) -> Option<pest::iterators::Pair<'i, Rule>> {
        self.wrappers
            .iter()
            .find(|pair| pair.as_rule() == rule)
            .cloned()
    }
}

fn is_expression_wrapper(rule: Rule) -> bool {
    matches!(
        rule,
        Rule::expression
            | Rule::coalesce_expr
            | Rule::pipe_expr
            | Rule::compose_expr
            | Rule::additive_expr
            | Rule::multiplicative_expr
            | Rule::postfix_expr
    )
}

/// Walk the first operand through all expression wrappers. When
/// `require_single_child` is true, any operator or postfix suffix makes the
/// descent fail; otherwise every wrapper is retained for later evaluation.
pub(crate) fn descend_expression<'i>(
    mut pair: pest::iterators::Pair<'i, Rule>,
    require_single_child: bool,
) -> Option<ExpressionDescent<'i>> {
    let mut wrappers = Vec::new();
    while is_expression_wrapper(pair.as_rule()) {
        wrappers.push(pair.clone());
        let mut inner = pair.into_inner();
        let first = inner.next()?;
        if require_single_child && inner.next().is_some() {
            return None;
        }
        pair = first;
    }
    (pair.as_rule() == Rule::primary).then_some(ExpressionDescent {
        wrappers,
        primary: pair,
    })
}

/// Return a primary only when no wrapper contains an operator or postfix
/// suffix.
pub(crate) fn unwrap_to_primary(
    pair: pest::iterators::Pair<Rule>,
) -> Option<pest::iterators::Pair<Rule>> {
    descend_expression(pair, true).map(|descent| descent.primary())
}
