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
            actual: describe_value_shape(value).to_string(),
        }),
        TypeCheckResult::ValidationError(e) => Err(ModuleError::InvalidArgumentType {
            module: module_name.to_string(),
            argument: arg_name.to_string(),
            expected: format!("{} ({})", type_expr, e),
            actual: describe_value_shape(value).to_string(),
        }),
    }
}

/// One-word description of a value's shape for error messages.
///
/// The previous `expected list(T)`-only message was misleading because
/// it didn't say what the user actually passed — a string, a map, a
/// reference to another resource's attribute, etc. Naming the actual
/// shape lets the reader see at a glance whether the mismatch is
/// element-type or value-shape (carina#3238).
fn describe_value_shape(value: &Value) -> &'static str {
    match value {
        Value::Concrete(ConcreteValue::String(_)) => "string",
        Value::Concrete(ConcreteValue::Int(_)) => "int",
        Value::Concrete(ConcreteValue::Float(_)) => "float",
        Value::Concrete(ConcreteValue::Bool(_)) => "bool",
        Value::Concrete(ConcreteValue::Duration(_)) => "duration",
        Value::Concrete(ConcreteValue::List(_)) | Value::Concrete(ConcreteValue::StringList(_)) => {
            "list"
        }
        Value::Concrete(ConcreteValue::Map(_)) => "map",
        Value::Concrete(ConcreteValue::EnumIdentifier(_)) => "enum identifier",
        Value::Deferred(DeferredValue::ResourceRef { .. }) => "resource reference",
        Value::Deferred(DeferredValue::BindingRef { .. }) => "binding reference",
        Value::Deferred(DeferredValue::Interpolation(_)) => "interpolation",
        Value::Deferred(DeferredValue::FunctionCall { .. }) => "function call",
        Value::Deferred(DeferredValue::Secret(_)) => "secret",
        Value::Deferred(DeferredValue::Unknown(_)) => "unknown",
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
        TypeExpr::List(inner) => match value {
            Value::Concrete(ConcreteValue::List(items)) => {
                for item in items {
                    match check_type_match(inner, item, config, enclosing_args) {
                        TypeCheckResult::Ok => {}
                        other => return other,
                    }
                }
                TypeCheckResult::Ok
            }
            // `StringList` is the canonical form for the
            // `string_or_list_of_strings` union shape (#2481, #2510);
            // structurally it is a list of strings, so accept it where
            // `list(T)` is declared and recurse so the inner type
            // arm applies the same check it would for a regular list
            // of string-shaped values.
            Value::Concrete(ConcreteValue::StringList(items)) => {
                for s in items {
                    let item = Value::Concrete(ConcreteValue::String(s.clone()));
                    match check_type_match(inner, &item, config, enclosing_args) {
                        TypeCheckResult::Ok => {}
                        other => return other,
                    }
                }
                TypeCheckResult::Ok
            }
            // A reference to another resource's attribute (e.g.
            // `roles.arns` from `read aws.iam.Roles`) is a deferred
            // value whose element type cannot be checked here. The
            // scalar arms (`String`, `Simple`, `Ref`, `SchemaType`)
            // already accept `ResourceRef` for the same reason;
            // collection arms must do the same so a `list(T)` argument
            // can receive a list-typed attribute (carina#3238).
            Value::Deferred(DeferredValue::ResourceRef { .. }) => TypeCheckResult::Ok,
            _ => TypeCheckResult::Mismatch,
        },
        TypeExpr::Map(inner) => match value {
            Value::Concrete(ConcreteValue::Map(entries)) => {
                for v in entries.values() {
                    match check_type_match(inner, v, config, enclosing_args) {
                        TypeCheckResult::Ok => {}
                        other => return other,
                    }
                }
                TypeCheckResult::Ok
            }
            // Sibling case to the `List` arm above (carina#3238).
            Value::Deferred(DeferredValue::ResourceRef { .. }) => TypeCheckResult::Ok,
            _ => TypeCheckResult::Mismatch,
        },
        // Simple types (cidr, arn, iam_policy_arn, etc.) are string subtypes
        TypeExpr::Simple(name) => {
            if !matches!(
                value,
                Value::Concrete(ConcreteValue::String(_))
                    | Value::Deferred(DeferredValue::Interpolation(_))
                    | Value::Deferred(DeferredValue::ResourceRef { .. })
            ) {
                TypeCheckResult::Mismatch
            } else {
                let identity =
                    crate::schema::TypeIdentity::bare(crate::parser::snake_to_pascal(name));
                if let Err(e) = validate_custom_type(&identity, value, config) {
                    TypeCheckResult::ValidationError(e)
                } else {
                    TypeCheckResult::Ok
                }
            }
        }
        // Resource refs, unresolved dotted refs, and schema types are string-shaped.
        TypeExpr::Ref(_) | TypeExpr::DottedUnresolved(_) | TypeExpr::SchemaType { .. } => {
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
            // Sibling case to the `List` / `Map` arms above (carina#3238):
            // a struct-typed argument fed from another resource's
            // attribute can't have its fields checked here. Defer to
            // expansion time, same as List/Map.
            if matches!(value, Value::Deferred(DeferredValue::ResourceRef { .. })) {
                return TypeCheckResult::Ok;
            }
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
/// unresolved dotted refs, `SchemaType`) are mutually compatible because the parser does not
/// distinguish their value shape — they all accept string-shaped values.
fn type_expr_compatible(expected: &TypeExpr, actual: &TypeExpr) -> TypeCheckResult {
    fn is_string_shaped(t: &TypeExpr) -> bool {
        matches!(
            t,
            TypeExpr::String
                | TypeExpr::Simple(_)
                | TypeExpr::Ref(_)
                | TypeExpr::DottedUnresolved(_)
                | TypeExpr::SchemaType { .. }
                | TypeExpr::StringLiteral(_)
        )
    }

    if is_string_shaped(expected) && is_string_shaped(actual) {
        return TypeCheckResult::Ok;
    }
    if matches!(
        (expected, actual),
        (TypeExpr::Int, TypeExpr::Int)
            | (TypeExpr::Float, TypeExpr::Float)
            | (TypeExpr::Bool, TypeExpr::Bool)
            | (TypeExpr::Duration, TypeExpr::Duration)
    ) {
        return TypeCheckResult::Ok;
    }
    if let (TypeExpr::List(e), TypeExpr::List(a)) = (expected, actual) {
        return type_expr_compatible(e, a);
    }
    if let (TypeExpr::Map(e), TypeExpr::Map(a)) = (expected, actual) {
        return type_expr_compatible(e, a);
    }
    if let (TypeExpr::Struct { fields: ef }, TypeExpr::Struct { fields: af }) = (expected, actual) {
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
        return TypeCheckResult::Ok;
    }
    // Union compatibility: actual must satisfy *some* member of expected when
    // expected is a union; an actual union must satisfy expected on at least
    // one member. See carina-rs/carina#2611.
    if let TypeExpr::Union(members) = expected
        && members
            .iter()
            .any(|m| matches!(type_expr_compatible(m, actual), TypeCheckResult::Ok))
    {
        return TypeCheckResult::Ok;
    }
    if matches!(expected, TypeExpr::Union(_)) {
        return TypeCheckResult::Mismatch;
    }
    if let TypeExpr::Union(members) = actual
        && members
            .iter()
            .any(|m| matches!(type_expr_compatible(expected, m), TypeCheckResult::Ok))
    {
        return TypeCheckResult::Ok;
    }

    // Unknown is the failed-inference sentinel; all remaining concrete
    // shape mismatches are rejected.
    TypeCheckResult::Mismatch
}
