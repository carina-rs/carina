//! Built-in functions for the Carina DSL
//!
//! Provides a registry of built-in functions that can be called from DSL expressions.
//! Functions take `&[Value]` arguments and return `Result<Value, String>`.

mod cidr_subnet;
mod concat;
pub mod decrypt;
mod env;
mod flatten;
mod join;
mod keys_values;
mod length;
mod lookup;
mod map;
mod min_max;
mod replace;
mod secret;
mod split;
mod trim;
mod upper_lower;

use crate::eval_value::EvalValue;
use crate::parser::ProviderContext;
use crate::resource::Value;

/// Coarse-grained return type for a built-in function. Used by the LSP to
/// filter value-position completion candidates to those whose return type
/// fits the attribute's declared type.
///
/// Deliberately coarse: the enum carries only the base type (String, Int,
/// List, Map, Secret) — not semantic subtypes (Cidr, AwsAccountId, Arn,
/// etc.). A String-returning built-in like `join` is therefore not
/// considered compatible with a `Custom { semantic_name: Some(..) }`
/// attribute, because no built-in can guarantee the semantic invariant
/// the Custom type requires.
///
/// `Bool` and `Float` variants are intentionally omitted because no
/// built-in currently returns those types; add them when one does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinReturnType {
    /// Returns a plain `Value::String` (no semantic subtype).
    String,
    /// Returns a `Value::Int`.
    Int,
    /// Returns a `Value::List` of some element type.
    List,
    /// Returns a `Value::Map`.
    Map,
    /// Return shape depends on the arguments (e.g. `lookup`, `min`, `max`,
    /// `map`). Matches any base type the attribute accepts, but still fails
    /// to match `Custom` / `StringEnum` attributes — the caller has no
    /// evidence the returned value satisfies those semantic constraints.
    Any,
    /// Returns a `Value::Secret` wrapper.
    Secret,
}

/// Metadata for a built-in function, used by the LSP for completion, hover, and validation.
pub struct BuiltinFunctionInfo {
    pub name: &'static str,
    pub signature: &'static str,
    pub description: &'static str,
    /// Coarse return-type classification — see `BuiltinReturnType`.
    pub return_type: BuiltinReturnType,
}

/// Register built-in functions in a single place.
///
/// Generates both `evaluate_builtin()` (dispatch) and `builtin_functions()` (metadata)
/// from one definition, ensuring they never get out of sync.
macro_rules! register_builtins {
    (
        $(
            $name:ident ( $handler:expr, arity: $arity:expr ) {
                signature: $sig:expr,
                description: $desc:expr,
                return_type: $ret:expr,
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
                        return_type: $ret,
                    },
                )*
            ];
            FUNCTIONS
        }

        /// Return the expected arity (number of arguments) for a built-in function.
        ///
        /// Returns `None` if the function is unknown.
        pub fn builtin_arity(name: &str) -> Option<usize> {
            match name {
                $( stringify!($name) => Some($arity), )*
                _ => None,
            }
        }

        /// Evaluate a built-in function by name with the given arguments.
        ///
        /// If fewer arguments than the arity are provided, returns an
        /// `EvalValue::Closure` capturing the partial arguments.
        /// Returns `Err` if the function is unknown, the arguments are
        /// invalid, or one of the supplied arguments is itself a closure.
        pub(crate) fn evaluate_builtin(
            name: &str,
            args: &[EvalValue],
        ) -> Result<EvalValue, String> {
            match name {
                $(
                    stringify!($name) => {
                        let arity: usize = $arity;
                        if !args.is_empty() && args.len() < arity {
                            return Ok(EvalValue::closure(
                                name,
                                args.to_vec(),
                                arity - args.len(),
                            ));
                        }
                        // Lower each argument to a `Value` for the
                        // handler. Handlers expect fully-reduced inputs,
                        // so a closure here is a usage error: the caller
                        // tried to pass a partially-applied function as a
                        // data argument.
                        let lowered: Result<Vec<Value>, String> = args
                            .iter()
                            .cloned()
                            .map(|arg| {
                                arg.into_value().map_err(|leak| {
                                    format!(
                                        "{}: closure '{}' (still needs {} arg(s)) cannot be \
                                         used as a data argument; finish the partial \
                                         application first",
                                        name, leak.name, leak.remaining_arity
                                    )
                                })
                            })
                            .collect();
                        $handler(&lowered?).map(EvalValue::from_value)
                    }
                )*
                _ => Err(format!("Unknown built-in function: {name}")),
            }
        }

        /// Public alias that lowers an `EvalValue` result to a `Value`
        /// for callers that don't care about the closure case (e.g.
        /// LSP-style hover preview that just wants "what does this call
        /// produce"). Returns `Err` if the call would produce a closure.
        pub fn evaluate_builtin_to_value(
            name: &str,
            args: &[Value],
        ) -> Result<Value, String> {
            let eval_args: Vec<EvalValue> = args.iter().cloned().map(EvalValue::from_value).collect();
            let result = evaluate_builtin(name, &eval_args)?;
            result.into_value().map_err(|leak| {
                format!(
                    "{}: would return a closure ({} arg(s) still needed)",
                    leak.name, leak.remaining_arity
                )
            })
        }
    };
}

register_builtins! {
    cidr_subnet(cidr_subnet::builtin_cidr_subnet, arity: 3) {
        signature: "cidr_subnet(prefix: String, newbits: Int, netnum: Int) -> String",
        description: "Calculates a subnet CIDR block within a given IP network address prefix.",
        return_type: BuiltinReturnType::String,
    },
    concat(concat::builtin_concat, arity: 2) {
        signature: "concat(items: list, base_list: list) -> list",
        description: "Appends items to a list. Data-last: base_list |> concat(items).",
        return_type: BuiltinReturnType::List,
    },
    decrypt(decrypt::builtin_decrypt, arity: 1) {
        signature: "decrypt(ciphertext: String, key?: String) -> String",
        description: "Decrypts ciphertext using the configured provider's encryption service (e.g., AWS KMS). Key is optional when embedded in ciphertext.",
        return_type: BuiltinReturnType::String,
    },
    env(env::builtin_env, arity: 1) {
        signature: "env(name: String) -> String",
        description: "Reads an environment variable. Errors if the variable is not set.",
        return_type: BuiltinReturnType::String,
    },
    flatten(flatten::builtin_flatten, arity: 1) {
        signature: "flatten(list: list) -> list",
        description: "Flattens nested lists by one level.",
        return_type: BuiltinReturnType::List,
    },
    join(join::builtin_join, arity: 2) {
        signature: "join(separator: String, list: list) -> String",
        description: "Joins list elements into a string using the separator.",
        return_type: BuiltinReturnType::String,
    },
    keys(keys_values::builtin_keys, arity: 1) {
        signature: "keys(map: map) -> list",
        description: "Returns the keys of a map as a sorted list.",
        return_type: BuiltinReturnType::List,
    },
    length(length::builtin_length, arity: 1) {
        signature: "length(value: list | map | String) -> Int",
        description: "Returns the number of elements in a list or map, or characters in a string.",
        return_type: BuiltinReturnType::Int,
    },
    lookup(lookup::builtin_lookup, arity: 3) {
        signature: "lookup(map: map, key: String, default: Any) -> Any",
        description: "Looks up a key in a map, returning the default value if the key is not found.",
        return_type: BuiltinReturnType::Any,
    },
    lower(upper_lower::builtin_lower, arity: 1) {
        signature: "lower(string: String) -> String",
        description: "Converts a string to lowercase.",
        return_type: BuiltinReturnType::String,
    },
    map(map::builtin_map, arity: 2) {
        signature: "map(accessor: String, collection: list | map) -> list | map",
        description: "Extracts a field from each element. Use a dot-prefixed accessor (e.g., \".field_name\"). Pipe form: collection |> map(\".field\").",
        return_type: BuiltinReturnType::Any,
    },
    max(min_max::builtin_max, arity: 2) {
        signature: "max(a: Number, b: Number) -> Number",
        description: "Returns the maximum of two numbers.",
        return_type: BuiltinReturnType::Any,
    },
    min(min_max::builtin_min, arity: 2) {
        signature: "min(a: Number, b: Number) -> Number",
        description: "Returns the minimum of two numbers.",
        return_type: BuiltinReturnType::Any,
    },
    replace(replace::builtin_replace, arity: 3) {
        signature: "replace(search: String, replacement: String, string: String) -> String",
        description: "Replaces all occurrences of a search string. Data-last: String |> replace(search, replacement).",
        return_type: BuiltinReturnType::String,
    },
    secret(secret::builtin_secret, arity: 1) {
        signature: "secret(value: Any) -> Secret",
        description: "Marks a value as secret. The value is sent to the provider but stored only as a SHA256 hash in state.",
        return_type: BuiltinReturnType::Secret,
    },
    split(split::builtin_split, arity: 2) {
        signature: "split(separator: String, string: String) -> list",
        description: "Splits a string into a list using the separator.",
        return_type: BuiltinReturnType::List,
    },
    trim(trim::builtin_trim, arity: 1) {
        signature: "trim(string: String) -> String",
        description: "Removes leading and trailing whitespace from a string.",
        return_type: BuiltinReturnType::String,
    },
    upper(upper_lower::builtin_upper, arity: 1) {
        signature: "upper(string: String) -> String",
        description: "Converts a string to uppercase.",
        return_type: BuiltinReturnType::String,
    },
    values(keys_values::builtin_values, arity: 1) {
        signature: "values(map: map) -> list",
        description: "Returns the values of a map as a list, sorted by key.",
        return_type: BuiltinReturnType::List,
    },
}

/// Apply additional arguments to a Closure using parser configuration.
///
/// Merges `new_args` into the closure's captured args. If enough arguments
/// are now present, evaluates the underlying built-in function (routing
/// `decrypt` through the config-aware path). Otherwise returns a new
/// `EvalValue::Closure` with updated captured args and remaining arity.
pub(crate) fn apply_closure_with_config(
    name: &str,
    captured_args: &[EvalValue],
    remaining_arity: usize,
    new_args: &[EvalValue],
    config: &ProviderContext,
) -> Result<EvalValue, String> {
    // Handle composed closures: pipe the argument through each function in sequence
    if name == "__compose__" {
        if new_args.len() != 1 {
            return Err(format!(
                "composed function expects exactly 1 argument, got {}",
                new_args.len(),
            ));
        }
        let mut result = new_args[0].clone();
        for func in captured_args {
            if let EvalValue::Closure {
                name: fn_name,
                captured_args: fn_captured,
                remaining_arity: fn_remaining,
            } = func
            {
                result = apply_closure_with_config(
                    fn_name,
                    fn_captured,
                    *fn_remaining,
                    &[result],
                    config,
                )?;
            } else {
                return Err(format!(
                    "composed function chain contains a non-Closure value: {:?}",
                    func
                ));
            }
        }
        return Ok(result);
    }

    if new_args.len() > remaining_arity {
        return Err(format!(
            "{}() closure expects {} more argument{}, got {}",
            name,
            remaining_arity,
            if remaining_arity == 1 { "" } else { "s" },
            new_args.len(),
        ));
    }
    let mut all_args = captured_args.to_vec();
    all_args.extend_from_slice(new_args);
    let new_remaining = remaining_arity - new_args.len();
    if new_remaining == 0 {
        evaluate_builtin_with_config(name, &all_args, config)
    } else {
        Ok(EvalValue::closure(name, all_args, new_remaining))
    }
}

/// Evaluate a built-in function with parser configuration.
///
/// This dispatches `decrypt` to use the decryptor from the config instead of
/// the global Mutex. All other builtins are delegated to [`evaluate_builtin`].
pub(crate) fn evaluate_builtin_with_config(
    name: &str,
    args: &[EvalValue],
    config: &ProviderContext,
) -> Result<EvalValue, String> {
    // Check for partial application before dispatching
    if let Some(arity) = builtin_arity(name)
        && !args.is_empty()
        && args.len() < arity
    {
        return Ok(EvalValue::closure(name, args.to_vec(), arity - args.len()));
    }
    match name {
        "decrypt" => {
            // `decrypt` is the only handler that needs the parser config.
            // It still operates on `&[Value]` so we lower the arguments.
            let lowered: Result<Vec<Value>, String> = args
                .iter()
                .cloned()
                .map(|arg| {
                    arg.into_value().map_err(|leak| {
                        format!(
                            "decrypt: closure '{}' (still needs {} arg(s)) cannot be \
                             used as a data argument; finish the partial application first",
                            leak.name, leak.remaining_arity
                        )
                    })
                })
                .collect();
            decrypt::builtin_decrypt_with_config(&lowered?, config).map(EvalValue::from_value)
        }
        _ => evaluate_builtin(name, args),
    }
}

/// Check if a function name is a known built-in function.
pub fn is_known_builtin(name: &str) -> bool {
    builtin_functions().iter().any(|f| f.name == name)
}

/// Test-only helper: dispatch a built-in by lifting `&[Value]` to
/// `&[EvalValue]` and returning the raw `EvalValue` result. Used by the
/// per-builtin test modules so existing assertions like
/// `result.is_closure()` keep working without forcing each test to
/// allocate `EvalValue` arguments by hand.
#[cfg(test)]
pub(crate) fn evaluate_builtin_for_tests(name: &str, args: &[Value]) -> Result<EvalValue, String> {
    let eval_args: Vec<EvalValue> = args.iter().cloned().map(EvalValue::from_value).collect();
    evaluate_builtin(name, &eval_args)
}

/// `evaluate_builtin_with_config`'s `Value`-friendly counterpart for
/// tests. Lifts arguments to `EvalValue`, dispatches, and lowers the
/// result back to `Value` — failing on closure leaks the same way
/// production code does.
#[cfg(test)]
pub(crate) fn evaluate_builtin_with_config_to_value(
    name: &str,
    args: &[Value],
    config: &ProviderContext,
) -> Result<Value, String> {
    let eval_args: Vec<EvalValue> = args.iter().cloned().map(EvalValue::from_value).collect();
    let result = evaluate_builtin_with_config(name, &eval_args, config)?;
    result.into_value().map_err(|leak| {
        format!(
            "{}: would return a closure ({} arg(s) still needed)",
            leak.name, leak.remaining_arity
        )
    })
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
        Value::Interpolation(_) => "Interpolation",
        Value::FunctionCall { .. } => "FunctionCall",
        Value::Secret(_) => "Secret",
        Value::Unknown(_) => {
            unimplemented!("Value::Unknown handling lands in RFC #2371 stage 2/3")
        }
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
    fn test_all_builtin_modules_are_registered() {
        let builtins_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/builtins");

        // Count .rs files in builtins/ directory (excluding mod.rs)
        let file_count = std::fs::read_dir(&builtins_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.ends_with(".rs") && name != "mod.rs"
            })
            .count();

        // Count `mod` declarations in mod.rs
        let mod_rs = std::fs::read_to_string(builtins_dir.join("mod.rs")).unwrap();
        let mod_count = mod_rs
            .lines()
            .filter(|line| {
                let trimmed = line.trim();
                (trimmed.starts_with("mod ") || trimmed.starts_with("pub mod "))
                    && trimmed.ends_with(';')
            })
            .count();

        assert_eq!(
            file_count, mod_count,
            "Number of .rs files in builtins/ ({file_count}) does not match \
             the number of `mod` declarations in mod.rs ({mod_count}). \
             Did you forget to add a `mod` declaration for a new builtin file?"
        );
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
