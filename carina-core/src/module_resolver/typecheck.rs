//! Type-checking of module call arguments against declared `TypeExpr`s.

use crate::parser::{ArgumentParameter, ProviderContext, TypeExpr, validate_custom_type};
use crate::resource::{ConcreteValue, DeferredValue, Value};

use super::error::ModuleError;

/// Check that a module argument value matches the declared type.
///
/// `enclosing_args` is the argument signature of the module the call site
/// lives inside (`None` for a top-level call). When the value is a bare
/// reference to one of those arguments, the inner check recurses against
/// the enclosing arg's declared type so the inner mismatch surfaces here
/// rather than after the parent's substitution erases the type tag.
pub(super) fn check_module_arg_type(
    module_name: &str,
    arg_name: &str,
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
    enclosing_args: Option<&[ArgumentParameter]>,
) -> Result<(), ModuleError> {
    match check_type_match(type_expr, value, config, enclosing_args) {
        TypeCheckResult::Ok => Ok(()),
        TypeCheckResult::Mismatch => Err(ModuleError::InvalidArgumentType {
            module: module_name.to_string(),
            argument: arg_name.to_string(),
            expected: type_expr.to_string(),
        }),
        TypeCheckResult::ValidationError(e) => Err(ModuleError::InvalidArgumentType {
            module: module_name.to_string(),
            argument: arg_name.to_string(),
            expected: format!("{} ({})", type_expr, e),
        }),
    }
}

pub(super) enum TypeCheckResult {
    Ok,
    Mismatch,
    ValidationError(String),
}

/// If `value` is a bare reference to a binding in `enclosing_args`,
/// return that arg's declared type. Bare-binding refs are how the
/// parser represents arguments-block names that propagate into nested
/// module calls (#2549).
///
/// `Value::Deferred(DeferredValue::BindingRef)` is the canonical representation since #2847;
/// the type system guarantees no attribute/field/subscript can hide
/// inside it.
fn enclosing_arg_type<'a>(
    value: &Value,
    enclosing_args: Option<&'a [ArgumentParameter]>,
) -> Option<&'a TypeExpr> {
    let Value::Deferred(DeferredValue::BindingRef { binding }) = value else {
        return None;
    };
    enclosing_args?
        .iter()
        .find(|a| a.name == binding.as_str())
        .map(|a| &a.type_expr)
}

pub(super) fn check_type_match(
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
    enclosing_args: Option<&[ArgumentParameter]>,
) -> TypeCheckResult {
    // Bare ref to an enclosing-module argument: typecheck against the
    // declared type of that argument. The substituted value isn't
    // available here — it lands later when the parent expands — so we
    // compare type tags instead.
    if let Some(declared) = enclosing_arg_type(value, enclosing_args) {
        return type_expr_compatible(type_expr, declared);
    }

    match type_expr {
        // Deferred-resolution values: type unknown at this checkpoint.
        // `BindingRef` falls through here only when it is not an
        // enclosing-arg ref (the early return above handles that case);
        // those leftover bare refs are unresolved sibling/forward refs
        // and behave like `Unknown` for typecheck purposes.
        _ if matches!(
            value,
            Value::Deferred(DeferredValue::FunctionCall { .. })
                | Value::Deferred(DeferredValue::Unknown(_))
                | Value::Deferred(DeferredValue::BindingRef { .. })
        ) =>
        {
            TypeCheckResult::Ok
        }
        TypeExpr::String => {
            if matches!(
                value,
                Value::Concrete(ConcreteValue::String(_))
                    | Value::Deferred(DeferredValue::Interpolation(_))
                    | Value::Deferred(DeferredValue::ResourceRef { .. })
            ) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Int => {
            if matches!(value, Value::Concrete(ConcreteValue::Int(_))) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Float => {
            if matches!(value, Value::Concrete(ConcreteValue::Float(_))) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Bool => {
            if matches!(value, Value::Concrete(ConcreteValue::Bool(_))) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Duration => {
            if matches!(value, Value::Concrete(ConcreteValue::Duration(_))) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::List(inner) => {
            if let Value::Concrete(ConcreteValue::List(items)) = value {
                for item in items {
                    match check_type_match(inner, item, config, enclosing_args) {
                        TypeCheckResult::Ok => {}
                        other => return other,
                    }
                }
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Map(inner) => {
            if let Value::Concrete(ConcreteValue::Map(entries)) = value {
                for v in entries.values() {
                    match check_type_match(inner, v, config, enclosing_args) {
                        TypeCheckResult::Ok => {}
                        other => return other,
                    }
                }
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        // Simple types (cidr, arn, iam_policy_arn, etc.) are string subtypes
        TypeExpr::Simple(name) => {
            if !matches!(
                value,
                Value::Concrete(ConcreteValue::String(_))
                    | Value::Deferred(DeferredValue::Interpolation(_))
                    | Value::Deferred(DeferredValue::ResourceRef { .. })
            ) {
                TypeCheckResult::Mismatch
            } else if let Err(e) = validate_custom_type(name, value, config) {
                TypeCheckResult::ValidationError(e)
            } else {
                TypeCheckResult::Ok
            }
        }
        // Resource type refs and schema types: accept strings (validated elsewhere)
        TypeExpr::Ref(_) | TypeExpr::SchemaType { .. } => {
            if matches!(
                value,
                Value::Concrete(ConcreteValue::String(_))
                    | Value::Deferred(DeferredValue::Interpolation(_))
                    | Value::Deferred(DeferredValue::ResourceRef { .. })
            ) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Struct { fields } => {
            let Value::Concrete(ConcreteValue::Map(entries)) = value else {
                return TypeCheckResult::Mismatch;
            };
            if crate::validation::struct_field_shape_errors(fields, entries).is_some() {
                return TypeCheckResult::Mismatch;
            }
            for (name, ty) in fields {
                if let Some(v) = entries.get(name) {
                    match check_type_match(ty, v, config, enclosing_args) {
                        TypeCheckResult::Ok => {}
                        other => return other,
                    }
                }
            }
            TypeCheckResult::Ok
        }
        // Closed-set string literal type (`'dev'` etc., carina-rs/carina#2611).
        // Only an exact-string `Value::Concrete(ConcreteValue::String)` match is accepted.
        TypeExpr::StringLiteral(expected) => {
            if matches!(value, Value::Concrete(ConcreteValue::String(s)) if s == expected) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        // Union: `T1 | T2 | ...` accepts the value if any member does.
        TypeExpr::Union(members) => {
            for m in members {
                if matches!(
                    check_type_match(m, value, config, enclosing_args),
                    TypeCheckResult::Ok
                ) {
                    return TypeCheckResult::Ok;
                }
            }
            TypeCheckResult::Mismatch
        }
        // Sentinel for failed inference (#2360 stage 2). Module signatures
        // are user-declared and should never carry Unknown — reaching this
        // arm is a defensive fallthrough, treated as a mismatch.
        TypeExpr::Unknown => TypeCheckResult::Mismatch,
    }
}

/// Structural compatibility between two declared `TypeExpr`s. Used when
/// both sides are known by their type tags rather than by a concrete
/// `Value`. `String`-shaped types (`String`, `Simple`, `Ref`,
/// `SchemaType`) are mutually compatible because the parser does not
/// distinguish their value shape — they all accept string-shaped values.
fn type_expr_compatible(expected: &TypeExpr, actual: &TypeExpr) -> TypeCheckResult {
    fn is_string_shaped(t: &TypeExpr) -> bool {
        matches!(
            t,
            TypeExpr::String
                | TypeExpr::Simple(_)
                | TypeExpr::Ref(_)
                | TypeExpr::SchemaType { .. }
                | TypeExpr::StringLiteral(_)
        )
    }

    match (expected, actual) {
        (a, b) if is_string_shaped(a) && is_string_shaped(b) => TypeCheckResult::Ok,
        (TypeExpr::Int, TypeExpr::Int)
        | (TypeExpr::Float, TypeExpr::Float)
        | (TypeExpr::Bool, TypeExpr::Bool)
        | (TypeExpr::Duration, TypeExpr::Duration) => TypeCheckResult::Ok,
        (TypeExpr::List(e), TypeExpr::List(a)) => type_expr_compatible(e, a),
        (TypeExpr::Map(e), TypeExpr::Map(a)) => type_expr_compatible(e, a),
        (TypeExpr::Struct { fields: ef }, TypeExpr::Struct { fields: af }) => {
            if ef.len() != af.len() {
                return TypeCheckResult::Mismatch;
            }
            for ((en, et), (an, at)) in ef.iter().zip(af.iter()) {
                if en != an {
                    return TypeCheckResult::Mismatch;
                }
                match type_expr_compatible(et, at) {
                    TypeCheckResult::Ok => {}
                    other => return other,
                }
            }
            TypeCheckResult::Ok
        }
        // Union compatibility: actual must satisfy *some* member of
        // expected when expected is a union; expected actual union
        // must satisfy expected on at least one member when actual is
        // a union. See carina-rs/carina#2611.
        (TypeExpr::Union(members), other) => {
            if members
                .iter()
                .any(|m| matches!(type_expr_compatible(m, other), TypeCheckResult::Ok))
            {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        (other, TypeExpr::Union(members)) => {
            if members
                .iter()
                .any(|m| matches!(type_expr_compatible(other, m), TypeCheckResult::Ok))
            {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        // Unknown is the failed-inference sentinel; anything paired with
        // it is a defensive mismatch.
        _ => TypeCheckResult::Mismatch,
    }
}
