//! Built-in functions for the Carina DSL
//!
//! Provides a registry of built-in functions that can be called from DSL expressions.
//! Functions take `&[Value]` arguments and return `Result<Value, String>`.

mod cidr_subnet;
mod concat;
mod flatten;
mod join;
mod keys_values;
mod length;
mod lookup;
mod min_max;
mod replace;
mod split;
mod trim;
mod upper_lower;

use crate::resource::Value;

/// Metadata for a built-in function, used by the LSP for completion, hover, and validation.
pub struct BuiltinFunctionInfo {
    pub name: &'static str,
    pub signature: &'static str,
    pub description: &'static str,
}

/// Return metadata for all built-in functions.
pub fn builtin_functions() -> &'static [BuiltinFunctionInfo] {
    static FUNCTIONS: &[BuiltinFunctionInfo] = &[
        BuiltinFunctionInfo {
            name: "cidr_subnet",
            signature: "cidr_subnet(prefix: string, newbits: int, netnum: int) -> string",
            description: "Calculates a subnet CIDR block within a given IP network address prefix.",
        },
        BuiltinFunctionInfo {
            name: "concat",
            signature: "concat(list1: list, list2: list) -> list",
            description: "Concatenates two lists into a single list.",
        },
        BuiltinFunctionInfo {
            name: "flatten",
            signature: "flatten(list: list) -> list",
            description: "Flattens nested lists by one level.",
        },
        BuiltinFunctionInfo {
            name: "join",
            signature: "join(separator: string, list: list) -> string",
            description: "Joins list elements into a string using the separator.",
        },
        BuiltinFunctionInfo {
            name: "keys",
            signature: "keys(map: map) -> list",
            description: "Returns the keys of a map as a sorted list.",
        },
        BuiltinFunctionInfo {
            name: "length",
            signature: "length(value: list | map | string) -> int",
            description: "Returns the number of elements in a list or map, or characters in a string.",
        },
        BuiltinFunctionInfo {
            name: "lookup",
            signature: "lookup(map: map, key: string, default: any) -> any",
            description: "Looks up a key in a map, returning the default value if the key is not found.",
        },
        BuiltinFunctionInfo {
            name: "lower",
            signature: "lower(string: string) -> string",
            description: "Converts a string to lowercase.",
        },
        BuiltinFunctionInfo {
            name: "max",
            signature: "max(a: number, b: number) -> number",
            description: "Returns the maximum of two numbers.",
        },
        BuiltinFunctionInfo {
            name: "min",
            signature: "min(a: number, b: number) -> number",
            description: "Returns the minimum of two numbers.",
        },
        BuiltinFunctionInfo {
            name: "replace",
            signature: "replace(string: string, search: string, replacement: string) -> string",
            description: "Replaces all occurrences of a search string with a replacement string.",
        },
        BuiltinFunctionInfo {
            name: "split",
            signature: "split(separator: string, string: string) -> list",
            description: "Splits a string into a list using the separator.",
        },
        BuiltinFunctionInfo {
            name: "trim",
            signature: "trim(string: string) -> string",
            description: "Removes leading and trailing whitespace from a string.",
        },
        BuiltinFunctionInfo {
            name: "upper",
            signature: "upper(string: string) -> string",
            description: "Converts a string to uppercase.",
        },
        BuiltinFunctionInfo {
            name: "values",
            signature: "values(map: map) -> list",
            description: "Returns the values of a map as a list, sorted by key.",
        },
    ];
    FUNCTIONS
}

/// Check if a function name is a known built-in function.
pub fn is_known_builtin(name: &str) -> bool {
    builtin_functions().iter().any(|f| f.name == name)
}

/// Evaluate a built-in function by name with the given arguments.
///
/// Returns `Err` if the function is unknown or if the arguments are invalid.
pub fn evaluate_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "cidr_subnet" => cidr_subnet::builtin_cidr_subnet(args),
        "concat" => concat::builtin_concat(args),
        "flatten" => flatten::builtin_flatten(args),
        "join" => join::builtin_join(args),
        "keys" => keys_values::builtin_keys(args),
        "length" => length::builtin_length(args),
        "lookup" => lookup::builtin_lookup(args),
        "lower" => upper_lower::builtin_lower(args),
        "max" => min_max::builtin_max(args),
        "min" => min_max::builtin_min(args),
        "replace" => replace::builtin_replace(args),
        "split" => split::builtin_split(args),
        "trim" => trim::builtin_trim(args),
        "upper" => upper_lower::builtin_upper(args),
        "values" => keys_values::builtin_values(args),
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
