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

/// Register built-in functions in a single place.
///
/// Generates both `evaluate_builtin()` (dispatch) and `builtin_functions()` (metadata)
/// from one definition, ensuring they never get out of sync.
macro_rules! register_builtins {
    (
        $(
            $name:ident ( $handler:expr ) {
                signature: $sig:expr,
                description: $desc:expr,
            }
        ),* $(,)?
    ) => {
        /// Return metadata for all built-in functions.
        pub fn builtin_functions() -> &'static [BuiltinFunctionInfo] {
            static FUNCTIONS: &[BuiltinFunctionInfo] = &[
                $(
                    BuiltinFunctionInfo {
                        name: stringify!($name),
                        signature: $sig,
                        description: $desc,
                    },
                )*
            ];
            FUNCTIONS
        }

        /// Evaluate a built-in function by name with the given arguments.
        ///
        /// Returns `Err` if the function is unknown or if the arguments are invalid.
        pub fn evaluate_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
            match name {
                $( stringify!($name) => $handler(args), )*
                _ => Err(format!("Unknown built-in function: {name}")),
            }
        }
    };
}

register_builtins! {
    cidr_subnet(cidr_subnet::builtin_cidr_subnet) {
        signature: "cidr_subnet(prefix: string, newbits: int, netnum: int) -> string",
        description: "Calculates a subnet CIDR block within a given IP network address prefix.",
    },
    concat(concat::builtin_concat) {
        signature: "concat(list1: list, list2: list) -> list",
        description: "Concatenates two lists into a single list.",
    },
    flatten(flatten::builtin_flatten) {
        signature: "flatten(list: list) -> list",
        description: "Flattens nested lists by one level.",
    },
    join(join::builtin_join) {
        signature: "join(separator: string, list: list) -> string",
        description: "Joins list elements into a string using the separator.",
    },
    keys(keys_values::builtin_keys) {
        signature: "keys(map: map) -> list",
        description: "Returns the keys of a map as a sorted list.",
    },
    length(length::builtin_length) {
        signature: "length(value: list | map | string) -> int",
        description: "Returns the number of elements in a list or map, or characters in a string.",
    },
    lookup(lookup::builtin_lookup) {
        signature: "lookup(map: map, key: string, default: any) -> any",
        description: "Looks up a key in a map, returning the default value if the key is not found.",
    },
    lower(upper_lower::builtin_lower) {
        signature: "lower(string: string) -> string",
        description: "Converts a string to lowercase.",
    },
    max(min_max::builtin_max) {
        signature: "max(a: number, b: number) -> number",
        description: "Returns the maximum of two numbers.",
    },
    min(min_max::builtin_min) {
        signature: "min(a: number, b: number) -> number",
        description: "Returns the minimum of two numbers.",
    },
    replace(replace::builtin_replace) {
        signature: "replace(string: string, search: string, replacement: string) -> string",
        description: "Replaces all occurrences of a search string with a replacement string.",
    },
    split(split::builtin_split) {
        signature: "split(separator: string, string: string) -> list",
        description: "Splits a string into a list using the separator.",
    },
    trim(trim::builtin_trim) {
        signature: "trim(string: string) -> string",
        description: "Removes leading and trailing whitespace from a string.",
    },
    upper(upper_lower::builtin_upper) {
        signature: "upper(string: string) -> string",
        description: "Converts a string to uppercase.",
    },
    values(keys_values::builtin_values) {
        signature: "values(map: map) -> list",
        description: "Returns the values of a map as a list, sorted by key.",
    },
}

/// Check if a function name is a known built-in function.
pub fn is_known_builtin(name: &str) -> bool {
    builtin_functions().iter().any(|f| f.name == name)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_and_dispatch_are_in_sync() {
        // Every function listed in builtin_functions() must be accepted by
        // evaluate_builtin() (i.e. not return "Unknown built-in function").
        for func in builtin_functions() {
            let result = evaluate_builtin(func.name, &[]);
            // The call may fail due to wrong arguments, but it must NOT fail
            // with "Unknown built-in function".
            if let Err(ref msg) = result {
                assert!(
                    !msg.contains("Unknown built-in function"),
                    "builtin_functions() lists '{}' but evaluate_builtin() does not handle it",
                    func.name,
                );
            }
        }
    }

    #[test]
    fn test_unknown_function_is_rejected() {
        let result = evaluate_builtin("nonexistent_function", &[]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Unknown built-in function: nonexistent_function")
        );
    }
}
