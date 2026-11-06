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
use crate::resource::{BindingAddress, ConcreteValue, DeferredValue, NoPlaceholder, Value};

/// Authored builtin syntax. Secret is an intrinsic value constructor and
/// cannot inhabit the ordinary deferred-call identity (`BuiltinFunction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    Secret,
    Function(BuiltinFunction),
}

impl Builtin {
    /// Dispatch both intrinsic construction and ordinary builtin handling here.
    fn dispatch<
        P: crate::resource::ValueKind,
        B: BindingAddress,
        R,
        E: crate::resource::InsideSecret,
    >(
        self,
        args: Vec<crate::resource::FunctionArgument<P, B>>,
        prepare_secret: impl FnOnce(Value<P, B>) -> Result<Value<P, B>, E>,
        wrap: impl FnOnce(Value<P, B>) -> R,
        function: impl FnOnce(
            BuiltinFunction,
            Vec<crate::resource::FunctionArgument<P, B>>,
        ) -> Result<R, E>,
        invalid: impl Fn(String) -> E,
    ) -> Result<R, E> {
        match self {
            Self::Secret => {
                if args.len() != 1 {
                    return Err(invalid(format!(
                        "secret() expects 1 argument, got {}",
                        args.len()
                    )));
                }
                let argument = args.into_iter().next().expect("one argument");
                let value = argument
                    .into_value()
                    .ok_or_else(|| invalid("secret() expects a value argument".to_string()))?;
                let value =
                    prepare_secret(value).map_err(crate::resource::InsideSecret::inside_secret)?;
                Ok(wrap(value.into_secret()))
            }
            Self::Function(name) => function(name, args),
        }
    }

    pub(crate) fn call<P: crate::resource::ValueKind, B: BindingAddress>(
        self,
        args: Vec<crate::resource::FunctionArgument<P, B>>,
        source_line: crate::resource::DiagnosticSourceLine,
        prepare_secret: impl FnOnce(Value<P, B>) -> Result<Value<P, B>, crate::parser::ParseError>,
    ) -> Result<Value<P, B>, crate::parser::ParseError> {
        self.dispatch(
            args,
            prepare_secret,
            |value| value,
            |name, args| {
                Ok(Value::Deferred(DeferredValue::FunctionCall {
                    name,
                    args,
                    source_line,
                }))
            },
            |message| crate::parser::ParseError::InvalidExpression {
                line: source_line.get().unwrap_or(0),
                message,
            },
        )
    }

    pub(crate) fn protect_parse_error(
        self,
        error: crate::parser::ParseError,
        line: usize,
    ) -> crate::parser::ParseError {
        match self {
            Self::Secret => error.inside_secret_at(line),
            Self::Function(_) => error,
        }
    }
}

/// The value boundary peels, protects errors and rewraps the result together.
fn evaluate_secret_arguments<B: BindingAddress>(
    args: Vec<Value<NoPlaceholder, B>>,
    handler: impl FnOnce(&[Value<NoPlaceholder, B>]) -> Result<Value<NoPlaceholder, B>, String>,
) -> Result<EvalValue<B>, String> {
    crate::resource::with_secret_values(args, |args| handler(&args))
        .map(EvalValue::from_expanded_value)
}

/// A builtin's result type and the evidence it supplies at an authored sink.
/// Every registry entry must choose a declaration. Judgement, projected type
/// inference, and completion consume it exhaustively.
///
/// Constructed strings use mixed interpolation's admission and value checks:
/// a named string sink checks the result's value, once it is known. Ordinary
/// strings retain their declared-assignment obligation at named sinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinResultTyping {
    /// Preserves the authored type and construction kind of this argument.
    Argument(usize),
    /// Returns an existing string, without construction evidence.
    String,
    /// Returns a secret external string, without construction evidence.
    SecretString,
    /// Constructs a string, with the same evidence as mixed interpolation.
    ConstructedString,
    /// Returns a `Value::Concrete(ConcreteValue::Int)`.
    Int,
    /// Returns a `Value::Concrete(ConcreteValue::List)` of some element type.
    List,
    /// Returns a list whose elements are statically known to be strings.
    StringList,
    /// Returns a list whose elements each carry string-construction evidence.
    ConstructedStringList,
    /// Returns a `Value::Concrete(ConcreteValue::Map)`.
    Map,
    /// Return shape depends on the arguments (e.g. `lookup`, `min`, `max`,
    /// `map`). Matches any base type the attribute accepts, but still fails
    /// to match `Custom` / `Enum` attributes — the caller has no
    /// evidence the returned value satisfies those semantic constraints.
    Any,
}

/// Metadata for a built-in function, used by the LSP for completion, hover, and validation.
pub struct BuiltinFunctionInfo {
    pub name: &'static str,
    pub signature: &'static str,
    pub description: &'static str,
    /// The sole result-typing declaration — see `BuiltinResultTyping`.
    pub result_typing: BuiltinResultTyping,
}

/// Register built-in functions in a single place.
///
/// Generates both `evaluate_builtin_with_config()` (dispatch) and `builtin_functions()` (metadata)
/// from one definition, ensuring they never get out of sync.
macro_rules! register_builtins {
    (@evaluate env, $handler:expr, $lowered:expr, $config:expr) => {
        evaluate_secret_arguments($lowered, |args| $handler(args, &$config.environment))
    };
    (@evaluate decrypt, $handler:expr, $lowered:expr, $config:expr) => {
        evaluate_secret_arguments($lowered, |args| $handler(args, $config))
    };
    (@evaluate $name:ident, $handler:expr, $lowered:expr, $config:expr) => {
        evaluate_secret_arguments($lowered, $handler)
    };
    (@collect [$($info:tt)*] [$($functions:tt)*]) => {
        register_builtins!(@generate [$($info)*] [$($functions)*]);
    };
    (@collect [$($info:tt)*] [$($functions:tt)*]
        intrinsic $name:ident($variant:ident, arity: $arity:expr) {
            signature: $sig:expr, description: $desc:expr, result_typing: $ret:expr,
        }, $($rest:tt)*
    ) => {
        register_builtins!(@collect [ $($info)* $name { kind: (Builtin::$variant), arity: $arity, signature: $sig, description: $desc, result_typing: $ret, } ] [$($functions)*] $($rest)*);
    };
    (@collect [$($info:tt)*] [$($functions:tt)*]
        $name:ident($handler:expr, arity: $arity:expr) {
            signature: $sig:expr, description: $desc:expr, result_typing: $ret:expr,
        }, $($rest:tt)*
    ) => {
        register_builtins!(@collect [ $($info)* $name { kind: (Builtin::Function(BuiltinFunction::$name)), arity: $arity, signature: $sig, description: $desc, result_typing: $ret, } ] [$($functions)* $name($handler, arity: $arity),] $($rest)*);
    };
    (@generate [$( $info_name:ident { kind: ($($kind:tt)*), arity: $info_arity:expr, signature: $sig:expr, description: $desc:expr, result_typing: $ret:expr, } )*]
        [$( $name:ident($handler:expr, arity: $arity:expr), )*]
    ) => {
        impl Builtin {
            pub fn from_name(name: &str) -> Option<Self> {
                match name { $( stringify!($info_name) => Some($($kind)*), )* _ => None }
            }

            pub const fn result_typing(self) -> BuiltinResultTyping {
                match self { $( $($kind)* => $ret, )* }
            }
        }
        /// Stable, typed identity for a registered built-in function.
        ///
        /// Deferred built-in calls store this value rather than a free-form
        /// string, so a user-function or unknown name cannot enter the
        /// built-in-only runtime path. Intrinsic constructors such as
        /// `secret(...)` have their own value nodes and cannot be named here.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[allow(non_camel_case_types)]
        pub enum BuiltinFunction {
            $( $name, )*
        }

        impl BuiltinFunction {
            pub fn from_name(name: &str) -> Option<Self> {
                match name {
                    $( stringify!($name) => Some(Self::$name), )*
                    _ => None,
                }
            }

            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$name => stringify!($name), )*
                }
            }

            pub const fn arity(self) -> usize {
                match self {
                    $( Self::$name => $arity, )*
                }
            }
        }

        impl std::fmt::Display for BuiltinFunction {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl std::ops::Deref for BuiltinFunction {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                self.as_str()
            }
        }

        impl AsRef<str> for BuiltinFunction {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl PartialEq<str> for BuiltinFunction {
            fn eq(&self, other: &str) -> bool {
                self.as_str() == other
            }
        }

        impl PartialEq<&str> for BuiltinFunction {
            fn eq(&self, other: &&str) -> bool {
                self.as_str() == *other
            }
        }

        /// Return metadata for all built-in functions.
        pub fn builtin_functions() -> &'static [BuiltinFunctionInfo] {
            static FUNCTIONS: &[BuiltinFunctionInfo] = &[
                $(
                    BuiltinFunctionInfo {
                        name: stringify!($info_name),
                        signature: $sig,
                        description: $desc,
                        result_typing: $ret,
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
                $( stringify!($info_name) => Some($info_arity), )*
                _ => None,
            }
        }

        /// Evaluate a built-in function using the caller's runtime inputs.
        ///
        /// If fewer arguments than the arity are provided, returns an
        /// `EvalValue::Closure` capturing the partial arguments.
        /// Returns `Err` if the function is unknown, the arguments are
        /// invalid, or one of the supplied arguments is itself a closure.
        pub(crate) fn evaluate_builtin_with_config<B: BindingAddress>(
            name: &str,
            args: &[EvalValue<B>],
            config: &ProviderContext,
        ) -> Result<EvalValue<B>, String> {
            let builtin = Builtin::from_name(name).ok_or_else(|| format!("Unknown built-in function: {name}"))?;
            let arity = builtin_arity(name).expect("registered builtin");
            if !args.is_empty() && args.len() < arity {
                return Ok(EvalValue::closure(name, args.to_vec(), arity - args.len()));
            }
            let lowered = args.iter().cloned().map(|arg| arg.into_expanded_value()
                .map(crate::resource::FunctionArgument::from_value)
                .map_err(|error| format!("{name}: {error}"))).collect::<Result<Vec<_>, _>>()?;
            builtin.dispatch(lowered, Ok::<_, String>, EvalValue::from_expanded_value, |function, args| {
                let lowered = args.into_iter().map(|arg| arg.into_value().ok_or_else(|| format!("{name}: expected a value argument"))).collect::<Result<Vec<_>, _>>()?;
                match function {
                    $( BuiltinFunction::$name => register_builtins!(@evaluate $name, $handler, lowered, config), )*
                }
            }, |message| message)
        }

        /// Public alias that lowers an `EvalValue` result to a `Value`
        /// for callers that don't care about the closure case (e.g.
        /// LSP-style hover preview that just wants "what does this call
        /// produce"). Returns `Err` if the call would produce a closure.
        pub fn evaluate_builtin_to_value<B: BindingAddress>(
            name: &str,
            args: &[Value<NoPlaceholder, B>],
            config: &ProviderContext,
        ) -> Result<Value<NoPlaceholder, B>, String> {
            let eval_args: Vec<EvalValue<B>> = args
                .iter()
                .cloned()
                .map(EvalValue::from_expanded_value)
                .collect();
            let result = evaluate_builtin_with_config(name, &eval_args, config)?;
            result.into_expanded_value()
        }
    };
    ($($entry:tt)*) => { register_builtins!(@collect [] [] $($entry)*); };
}

register_builtins! {
    cidr_subnet(cidr_subnet::builtin_cidr_subnet, arity: 3) {
        signature: "cidr_subnet(prefix: String, newbits: Int, netnum: Int) -> String",
        description: "Calculates a subnet CIDR block within a given IP network address prefix.",
        result_typing: BuiltinResultTyping::ConstructedString,
    },
    concat(concat::builtin_concat, arity: 2) {
        signature: "concat(items: list, base_list: list) -> list",
        description: "Appends items to a list. Data-last: base_list |> concat(items).",
        result_typing: BuiltinResultTyping::List,
    },
    decrypt(decrypt::builtin_decrypt_with_config, arity: 1) {
        signature: "decrypt(ciphertext: String, key?: String) -> Secret",
        description: "Decrypts ciphertext using the configured provider's encryption service (e.g., AWS KMS) and marks the result as secret. Key is optional when embedded in ciphertext.",
        result_typing: BuiltinResultTyping::SecretString,
    },
    env(env::builtin_env, arity: 1) {
        signature: "env(name: String) -> String",
        description: "Reads an environment variable. Errors if the variable is not set.",
        result_typing: BuiltinResultTyping::String,
    },
    flatten(flatten::builtin_flatten, arity: 1) {
        signature: "flatten(list: list) -> list",
        description: "Flattens nested lists by one level.",
        result_typing: BuiltinResultTyping::List,
    },
    join(join::builtin_join, arity: 2) {
        signature: "join(separator: String, list: list) -> String",
        description: "Joins list elements into a string using the separator.",
        result_typing: BuiltinResultTyping::ConstructedString,
    },
    keys(keys_values::builtin_keys, arity: 1) {
        signature: "keys(map: map) -> list",
        description: "Returns the keys of a map as a sorted list.",
        result_typing: BuiltinResultTyping::StringList,
    },
    length(length::builtin_length, arity: 1) {
        signature: "length(value: list | map | String) -> Int",
        description: "Returns the number of elements in a list or map, or characters in a string.",
        result_typing: BuiltinResultTyping::Int,
    },
    lookup(lookup::builtin_lookup, arity: 3) {
        signature: "lookup(map: map, key: String, default: Any) -> Any",
        description: "Looks up a key in a map, returning the default value if the key is not found.",
        result_typing: BuiltinResultTyping::Any,
    },
    lower(upper_lower::builtin_lower, arity: 1) {
        signature: "lower(string: String) -> String",
        description: "Converts a string to lowercase.",
        result_typing: BuiltinResultTyping::ConstructedString,
    },
    map(map::builtin_map, arity: 2) {
        signature: "map(accessor: String, collection: list | map) -> list | map",
        description: "Extracts a field from each element. Use a dot-prefixed accessor (e.g., \".field_name\"). Pipe form: collection |> map(\".field\").",
        result_typing: BuiltinResultTyping::Any,
    },
    max(min_max::builtin_max, arity: 2) {
        signature: "max(a: Number, b: Number) -> Number",
        description: "Returns the maximum of two numbers.",
        result_typing: BuiltinResultTyping::Any,
    },
    min(min_max::builtin_min, arity: 2) {
        signature: "min(a: Number, b: Number) -> Number",
        description: "Returns the minimum of two numbers.",
        result_typing: BuiltinResultTyping::Any,
    },
    replace(replace::builtin_replace, arity: 3) {
        signature: "replace(search: String, replacement: String, string: String) -> String",
        description: "Replaces all occurrences of a search string. Data-last: String |> replace(search, replacement).",
        result_typing: BuiltinResultTyping::ConstructedString,
    },
    intrinsic secret(Secret, arity: 1) {
        signature: "secret(value: Any) -> Secret",
        description: "Marks a value as secret. The value is sent to the provider but stored only as an Argon2id hash in state.",
        result_typing: BuiltinResultTyping::Argument(0),
    },
    split(split::builtin_split, arity: 2) {
        signature: "split(separator: String, string: String) -> list",
        description: "Splits a string into a list using the separator.",
        result_typing: BuiltinResultTyping::ConstructedStringList,
    },
    trim(trim::builtin_trim, arity: 1) {
        signature: "trim(string: String) -> String",
        description: "Removes leading and trailing whitespace from a string.",
        result_typing: BuiltinResultTyping::ConstructedString,
    },
    upper(upper_lower::builtin_upper, arity: 1) {
        signature: "upper(string: String) -> String",
        description: "Converts a string to uppercase.",
        result_typing: BuiltinResultTyping::ConstructedString,
    },
    values(keys_values::builtin_values, arity: 1) {
        signature: "values(map: map) -> list",
        description: "Returns the values of a map as a list, sorted by key.",
        result_typing: BuiltinResultTyping::List,
    },
}

/// Apply additional arguments to a Closure using parser configuration.
///
/// Merges `new_args` into the closure's captured args. If enough arguments
/// are now present, evaluates the underlying built-in function (routing
/// `decrypt` and `env` through the config-aware path). Otherwise returns a new
/// `EvalValue::Closure` with updated captured args and remaining arity.
pub(crate) fn apply_closure_with_config<B: BindingAddress>(
    name: &str,
    captured_args: &[EvalValue<B>],
    remaining_arity: usize,
    new_args: &[EvalValue<B>],
    config: &ProviderContext,
) -> Result<EvalValue<B>, String> {
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
                return Err("composed function chain contains a non-function value".to_string());
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

/// Check if a function name is a known built-in function.
pub fn is_known_builtin(name: &str) -> bool {
    Builtin::from_name(name).is_some()
}

/// Test-only helper: dispatch a built-in by lifting `&[Value]` to
/// `&[EvalValue]` and returning the raw `EvalValue` result. Used by the
/// per-builtin test modules so existing assertions like
/// `result.is_closure()` keep working without forcing each test to
/// allocate `EvalValue` arguments by hand.
#[cfg(test)]
pub(crate) fn evaluate_builtin_for_tests<B: BindingAddress>(
    name: &str,
    args: &[Value<NoPlaceholder, B>],
) -> Result<EvalValue<B>, String> {
    let eval_args: Vec<EvalValue<B>> = args
        .iter()
        .cloned()
        .map(EvalValue::from_expanded_value)
        .collect();
    evaluate_builtin_with_config(
        name,
        &eval_args,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
}

/// Supply an explicit empty test context for pure-builtin assertions.
#[cfg(test)]
pub(crate) fn evaluate_builtin_to_value_for_tests<B: BindingAddress>(
    name: &str,
    args: &[Value<NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, String> {
    evaluate_builtin_to_value(
        name,
        args,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
}

/// `evaluate_builtin_with_config`'s `Value`-friendly counterpart for
/// tests. Lifts arguments to `EvalValue`, dispatches, and lowers the
/// result back to `Value` — failing on closure leaks the same way
/// production code does.
#[cfg(test)]
pub(crate) fn evaluate_builtin_with_config_to_value<B: BindingAddress>(
    name: &str,
    args: &[Value<NoPlaceholder, B>],
    config: &ProviderContext,
) -> Result<Value<NoPlaceholder, B>, String> {
    evaluate_builtin_to_value(name, args, config)
}

/// Return a human-readable type name for a Value
fn value_type_name<B: BindingAddress>(value: &Value<NoPlaceholder, B>) -> &'static str {
    match value {
        Value::Concrete(ConcreteValue::String(_)) => "String",
        Value::Concrete(ConcreteValue::EnumIdentifier(_)) => "EnumIdentifier",
        Value::Concrete(ConcreteValue::CanonicalEnum(_)) => "CanonicalEnum",
        Value::Concrete(ConcreteValue::Int(_)) => "Int",
        Value::Concrete(ConcreteValue::Float(_)) => "Float",
        Value::Concrete(ConcreteValue::Bool(_)) => "Bool",
        Value::Concrete(ConcreteValue::Duration(_)) => "Duration",
        Value::Concrete(ConcreteValue::List(_)) => "List",
        Value::Concrete(ConcreteValue::StringList(_)) => "StringList",
        Value::Concrete(ConcreteValue::Map(_)) => "Map",
        Value::Deferred(DeferredValue::ModuleIf(_)) => "ModuleIf",
        Value::Deferred(DeferredValue::ResourceRef { .. }) => "ResourceRef",
        Value::Deferred(DeferredValue::BindingRef { .. }) => "BindingRef",
        Value::Deferred(DeferredValue::Coalesce { .. }) => "Coalesce",
        Value::Deferred(DeferredValue::IntegerArithmetic(_)) => "Int",
        Value::Deferred(DeferredValue::Interpolation(_)) => "Interpolation",
        Value::Deferred(
            DeferredValue::FunctionCall { .. } | DeferredValue::ResolvedFunctionCall { .. },
        ) => "FunctionCall",
        Value::Deferred(DeferredValue::Unavailable { .. }) => "Unavailable",
        Value::Deferred(DeferredValue::Access(_)) => "Access",
        Value::Deferred(DeferredValue::Secret(_)) => "Secret",

        Value::Deferred(DeferredValue::Unknown(_)) => "Unknown",
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
    }
}

#[cfg(test)]
mod tests {

    use indexmap::IndexMap;

    use super::evaluate_builtin_to_value_for_tests as evaluate_builtin_to_value;
    use super::*;

    type Value = crate::resource::ExpandedValue;
    type ConcreteValue = crate::resource::ExpandedConcreteValue;
    type DeferredValue = crate::resource::ExpandedDeferredValue;

    fn string(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.into()))
    }

    fn secret(value: Value) -> Value {
        Value::Deferred(DeferredValue::Secret(Box::new(value).into()))
    }

    #[test]
    fn review3_registry_secret_argument_errors_never_expose_marker() {
        let marker = "PRIVATE-REGISTRY-MARKER-4275";
        let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        for builtin in builtin_functions() {
            let arity = builtin_arity(builtin.name).unwrap();
            // Include optional parameters declared by the same registry entry.
            let parameters = builtin
                .signature
                .split_once('(')
                .unwrap()
                .1
                .split_once(") ->")
                .unwrap()
                .0;
            let maximum_arity = if parameters.is_empty() {
                0
            } else {
                parameters.split(',').count()
            };
            assert!(maximum_arity >= arity, "{} registry arity", builtin.name);
            for argument_count in arity..=maximum_arity {
                for position in 0..argument_count {
                    for shape in [
                        secret(string(marker)),
                        Value::Concrete(ConcreteValue::List(vec![secret(string(marker))])),
                        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                            "key".into(),
                            secret(string(marker)),
                        )]))),
                    ] {
                        let mut args = vec![string("public"); argument_count];
                        args[position] = shape;
                        if let Err(error) =
                            super::evaluate_builtin_to_value(builtin.name, &args, &config)
                        {
                            for text in [error.to_string(), format!("{error:?}")] {
                                assert!(
                                    !text.contains(marker),
                                    "{} argument {position}: {text}",
                                    builtin.name
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    fn assert_secret_inner(actual: Value, expected: Value) {
        assert_eq!(actual, secret(expected));
    }

    #[test]
    fn review2_f1_nested_secrets_preserve_main_values_and_leaf_hashes() {
        let list = Value::Concrete(ConcreteValue::List(vec![
            string("public"),
            secret(string("x")),
        ]));
        assert_eq!(
            evaluate_builtin_to_value("length", std::slice::from_ref(&list)).unwrap(),
            Value::Concrete(ConcreteValue::Int(2))
        );
        let map = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("a".into(), secret(string("x"))),
            ("b".into(), string("y")),
        ])));
        assert_eq!(
            evaluate_builtin_to_value("lookup", &[map, string("b"), string("")]).unwrap(),
            string("y")
        );
        let result = evaluate_builtin_to_value(
            "concat",
            &[
                Value::Concrete(ConcreteValue::List(vec![string("tail")])),
                list.clone(),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                string("public"),
                secret(string("x")),
                string("tail")
            ]))
        );
        let json = crate::value::value_to_json(&result).unwrap();
        assert_eq!(json[0], "public");
        assert_eq!(json[2], "tail");
        let hash = crate::resource::RedactedSecretHash::from_encoded_digest(
            json[1]["$secret"].as_str().unwrap(),
        )
        .unwrap();
        // origin/main canonical_secret_hash_material(String("x")) is JSON string bytes.
        assert!(hash.verify(br#""x""#));
    }

    #[test]
    fn review2_f6_nested_errors_preserve_public_reasons_and_hide_derived_strings() {
        let nested = Value::Concrete(ConcreteValue::List(vec![secret(string("private-prefix"))]));
        assert_eq!(
            evaluate_builtin_to_value("join", &[string("-"), nested.clone(), string("extra")])
                .unwrap_err(),
            "join() expects 2 arguments (separator, list), got 3"
        );
        assert_eq!(
            evaluate_builtin_to_value("join", &[Value::Concrete(ConcreteValue::Int(1)), nested])
                .unwrap_err(),
            "join() first argument must be a string, got Int"
        );
        let maps = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(IndexMap::from([("a".into(), secret(string("x")))])),
        )]));
        assert_eq!(
            evaluate_builtin_to_value("map", &[string(".missing"), maps]).unwrap_err(),
            "map(): field 'missing' not found in map element"
        );
        assert_eq!(
            evaluate_builtin_to_value(
                "cidr_subnet",
                &[
                    string("bad"),
                    Value::Concrete(ConcreteValue::Int(1)),
                    Value::Concrete(ConcreteValue::Int(0))
                ]
            )
            .unwrap_err(),
            "cidr_subnet() invalid CIDR format: 'bad' (expected 'address/prefix')"
        );
        // Payload construction marks strings; keep that protection
        // even when a later handler sees only the derived (unwrapped) string.
        let joined = evaluate_builtin_to_value(
            "join",
            &[
                string(""),
                Value::Concrete(ConcreteValue::List(vec![secret(string("private-prefix"))])),
            ],
        )
        .unwrap();
        let Value::Deferred(DeferredValue::Secret(payload)) = joined else {
            panic!("join must be secret")
        };
        let error = evaluate_builtin_to_value(
            "cidr_subnet",
            &[
                payload.expose_plaintext().clone(),
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(0)),
            ],
        )
        .unwrap_err();
        assert_eq!(
            error,
            "cidr_subnet() sensitive prefix is not a valid CIDR string"
        );
        assert!(!error.contains("private-prefix"));
    }

    #[test]
    fn secret_origin_4275_round2_builtin_handler_errors_are_masked() {
        let integer = 9_007_199_254_740_993;
        let args = [
            secret(Value::Concrete(ConcreteValue::Int(integer))),
            Value::Concrete(ConcreteValue::Float(crate::finite(0.5))),
        ];
        let error = evaluate_builtin_to_value("max", &args).unwrap_err();
        assert!(!error.contains(&integer.to_string()), "{error}");
        assert_eq!(error, crate::resource::INSIDE_SECRET_MESSAGE);

        let public_args = [
            Value::Concrete(ConcreteValue::Int(integer)),
            args[1].clone(),
        ];
        let public_error = evaluate_builtin_to_value("max", &public_args).unwrap_err();
        assert!(
            public_error.contains(&integer.to_string()),
            "{public_error}"
        );
    }

    #[test]
    fn test_registry_and_dispatch_are_in_sync() {
        // Every function listed in builtin_functions() must be accepted by
        // evaluate_builtin_with_config() (i.e. not return "Unknown built-in function").
        let args: Vec<EvalValue> = Vec::new();
        for func in builtin_functions() {
            let result = evaluate_builtin_with_config(
                func.name,
                &args,
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            );
            // The call may fail due to wrong arguments, but it must NOT fail
            // with "Unknown built-in function".
            if let Err(ref msg) = result {
                assert!(
                    !msg.contains("Unknown built-in function"),
                    "builtin_functions() lists '{}' but evaluate_builtin_with_config() does not handle it",
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
        let args: Vec<EvalValue> = Vec::new();
        let result = evaluate_builtin_with_config(
            "nonexistent_function",
            &args,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Unknown built-in function: nonexistent_function")
        );
    }

    #[test]
    fn builtins_evaluate_secret_inputs_and_propagate_the_secret_wrapper() {
        let cases = [
            ("upper", vec![secret(string("hello"))], string("HELLO")),
            ("lower", vec![secret(string("HELLO"))], string("hello")),
            ("trim", vec![secret(string("  hello  "))], string("hello")),
            (
                "replace",
                vec![string("old"), string("new"), secret(string("old-value"))],
                string("new-value"),
            ),
            (
                "split",
                vec![string("-"), secret(string("a-b"))],
                Value::Concrete(ConcreteValue::List(vec![string("a"), string("b")])),
            ),
            (
                "length",
                vec![secret(string("hello"))],
                Value::Concrete(ConcreteValue::Int(5)),
            ),
            (
                "lookup",
                vec![
                    Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                        "key".to_string(),
                        string("value"),
                    )]))),
                    secret(string("key")),
                    string("default"),
                ],
                string("value"),
            ),
            (
                "cidr_subnet",
                vec![
                    secret(string("10.0.0.0/16")),
                    Value::Concrete(ConcreteValue::Int(8)),
                    Value::Concrete(ConcreteValue::Int(1)),
                ],
                string("10.0.1.0/24"),
            ),
        ];

        for (name, args, expected) in cases {
            let actual = evaluate_builtin_to_value(name, &args)
                .unwrap_or_else(|error| panic!("{name} failed: {error}"));
            assert_secret_inner(actual, expected);
        }
    }

    #[test]
    fn decrypt_accepts_a_secret_from_another_decrypt_without_double_wrapping() {
        let config = ProviderContext {
            decryptor: Some(std::sync::Arc::new(|ciphertext, _| {
                Ok(format!("plain:{ciphertext}"))
            })),
            resource_types: Default::default(),
            ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
        };
        let first =
            evaluate_builtin_with_config_to_value("decrypt", &[string("ciphertext")], &config)
                .expect("first decrypt");

        let second = evaluate_builtin_with_config_to_value("decrypt", &[first], &config)
            .expect("second decrypt");

        assert_secret_inner(second, string("plain:plain:ciphertext"));
    }

    #[test]
    fn secret_input_errors_are_masked_without_exposing_values() {
        let marker = "hunter2SECRET";
        let cases = [
            (
                "cidr_subnet newbits range",
                "cidr_subnet",
                vec![
                    string("10.0.0.0/16"),
                    secret(Value::Concrete(ConcreteValue::Int(424_242))),
                    Value::Concrete(ConcreteValue::Int(0)),
                ],
                crate::resource::INSIDE_SECRET_MESSAGE,
                "424242",
            ),
            (
                "join arity",
                "join",
                vec![
                    secret(string(marker)),
                    Value::Concrete(ConcreteValue::List(vec![])),
                    string("extra"),
                ],
                crate::resource::INSIDE_SECRET_MESSAGE,
                marker,
            ),
            (
                "map accessor",
                "map",
                vec![
                    secret(string(marker)),
                    Value::Concrete(ConcreteValue::List(vec![])),
                ],
                crate::resource::INSIDE_SECRET_MESSAGE,
                marker,
            ),
        ];

        for (case, name, args, reason, forbidden) in cases {
            let error = match evaluate_builtin_to_value(name, &args) {
                Ok(value) => panic!("{case} unexpectedly returned {value:?}"),
                Err(error) => error,
            };
            assert!(error.contains(reason), "{case}: {error}");
            assert!(!error.contains(forbidden), "{case} leaked input: {error}");
            assert!(
                !error.contains("failed for a secret input"),
                "{case} hid the actual reason: {error}",
            );
        }
    }

    #[test]
    fn secret_collection_errors_do_not_expose_nested_plaintext() {
        let marker = "nested-hunter2SECRET";
        let secret_map = secret(Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            marker.to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        )]))));

        let error = evaluate_builtin_to_value("map", &[string(".missing"), secret_map])
            .expect_err("missing field must fail");

        assert_eq!(error, crate::resource::INSIDE_SECRET_MESSAGE);
        assert!(!error.contains(marker), "nested secret leaked: {error}");
    }
}
