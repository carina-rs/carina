//! Type-checking of module call arguments against declared `TypeExpr`s.

use crate::parser::{ProviderContext, TypeExpr, validate_custom_type};
use crate::resource::Value;

use super::error::ModuleError;

/// Check that a module argument value matches the declared type.
///
/// Similar to parser's `check_fn_arg_type` for user-defined functions,
/// this validates module call arguments against their declared `TypeExpr`.
pub(super) fn check_module_arg_type(
    module_name: &str,
    arg_name: &str,
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> Result<(), ModuleError> {
    match check_type_match(type_expr, value, config) {
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

pub(super) fn check_type_match(
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> TypeCheckResult {
    match type_expr {
        // Deferred-resolution values: type unknown at parse time.
        _ if matches!(value, Value::FunctionCall { .. } | Value::Unknown(_)) => TypeCheckResult::Ok,
        TypeExpr::String => {
            if matches!(
                value,
                Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
            ) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Int => {
            if matches!(value, Value::Int(_)) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Float => {
            if matches!(value, Value::Float(_)) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Bool => {
            if matches!(value, Value::Bool(_)) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::List(inner) => {
            if let Value::List(items) = value {
                for item in items {
                    match check_type_match(inner, item, config) {
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
            if let Value::Map(entries) = value {
                for v in entries.values() {
                    match check_type_match(inner, v, config) {
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
                Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
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
                Value::String(_) | Value::Interpolation(_) | Value::ResourceRef { .. }
            ) {
                TypeCheckResult::Ok
            } else {
                TypeCheckResult::Mismatch
            }
        }
        TypeExpr::Struct { fields } => {
            let Value::Map(entries) = value else {
                return TypeCheckResult::Mismatch;
            };
            if crate::validation::struct_field_shape_errors(fields, entries).is_some() {
                return TypeCheckResult::Mismatch;
            }
            for (name, ty) in fields {
                if let Some(v) = entries.get(name) {
                    match check_type_match(ty, v, config) {
                        TypeCheckResult::Ok => {}
                        other => return other,
                    }
                }
            }
            TypeCheckResult::Ok
        }
        // Sentinel for failed inference (#2360 stage 2). Module signatures
        // are user-declared and should never carry Unknown — reaching this
        // arm is a defensive fallthrough, treated as a mismatch.
        TypeExpr::Unknown => TypeCheckResult::Mismatch,
    }
}
