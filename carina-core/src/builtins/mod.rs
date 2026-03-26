//! Built-in functions for the Carina DSL
//!
//! Provides a registry of built-in functions that can be called from DSL expressions.
//! Functions take `&[Value]` arguments and return `Result<Value, String>`.

mod cidr_subnet;
mod flatten;
mod join;
mod length;
mod split;
mod trim;
mod upper_lower;

use crate::resource::Value;

/// Evaluate a built-in function by name with the given arguments.
///
/// Returns `Err` if the function is unknown or if the arguments are invalid.
pub fn evaluate_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "cidr_subnet" => cidr_subnet::builtin_cidr_subnet(args),
        "flatten" => flatten::builtin_flatten(args),
        "join" => join::builtin_join(args),
        "length" => length::builtin_length(args),
        "split" => split::builtin_split(args),
        "trim" => trim::builtin_trim(args),
        "upper" => upper_lower::builtin_upper(args),
        "lower" => upper_lower::builtin_lower(args),
        _ => Err(format!("Unknown built-in function: {name}")),
    }
}

/// Return a human-readable type name for a Value
fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "String",
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::Bool(_) => "Bool",
        Value::List(_) => "List",
        Value::Map(_) => "Map",
        Value::ResourceRef { .. } => "ResourceRef",
        Value::UnresolvedIdent(_, _) => "UnresolvedIdent",
        Value::Interpolation(_) => "Interpolation",
        Value::FunctionCall { .. } => "FunctionCall",
    }
}
