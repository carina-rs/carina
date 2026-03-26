//! Built-in functions for the Carina DSL
//!
//! Provides a registry of built-in functions that can be called from DSL expressions.
//! Functions take `&[Value]` arguments and return `Result<Value, String>`.

use crate::resource::Value;

/// Evaluate a built-in function by name with the given arguments.
///
/// Returns `Err` if the function is unknown or if the arguments are invalid.
pub fn evaluate_builtin(name: &str, args: &[Value]) -> Result<Value, String> {
    match name {
        "join" => builtin_join(args),
        _ => Err(format!("Unknown built-in function: {name}")),
    }
}

/// `join(separator, list)` - Join list elements into a string with a separator.
///
/// - First argument: separator (String)
/// - Second argument: list of values (each converted to string)
/// - Returns: String
///
/// Examples:
/// ```text
/// join("-", ["a", "b", "c"])  // => "a-b-c"
/// ["a", "b"] |> join("-")     // => "a-b" (pipe form)
/// ```
fn builtin_join(args: &[Value]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(format!(
            "join() expects 2 arguments (separator, list), got {}",
            args.len()
        ));
    }

    let separator = match &args[0] {
        Value::String(s) => s.clone(),
        other => {
            return Err(format!(
                "join() first argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    let items = match &args[1] {
        Value::List(items) => items,
        other => {
            return Err(format!(
                "join() second argument must be a list, got {}",
                value_type_name(other)
            ));
        }
    };

    let joined: String = items
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            other => format!("{:?}", other),
        })
        .collect::<Vec<_>>()
        .join(&separator);

    Ok(Value::String(joined))
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
    fn join_basic() {
        let args = vec![
            Value::String("-".to_string()),
            Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
                Value::String("c".to_string()),
            ]),
        ];
        let result = evaluate_builtin("join", &args).unwrap();
        assert_eq!(result, Value::String("a-b-c".to_string()));
    }

    #[test]
    fn join_empty_separator() {
        let args = vec![
            Value::String("".to_string()),
            Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
            ]),
        ];
        let result = evaluate_builtin("join", &args).unwrap();
        assert_eq!(result, Value::String("ab".to_string()));
    }

    #[test]
    fn join_empty_list() {
        let args = vec![Value::String("-".to_string()), Value::List(vec![])];
        let result = evaluate_builtin("join", &args).unwrap();
        assert_eq!(result, Value::String("".to_string()));
    }

    #[test]
    fn join_single_element() {
        let args = vec![
            Value::String("-".to_string()),
            Value::List(vec![Value::String("only".to_string())]),
        ];
        let result = evaluate_builtin("join", &args).unwrap();
        assert_eq!(result, Value::String("only".to_string()));
    }

    #[test]
    fn join_mixed_types() {
        let args = vec![
            Value::String(", ".to_string()),
            Value::List(vec![
                Value::String("hello".to_string()),
                Value::Int(42),
                Value::Bool(true),
            ]),
        ];
        let result = evaluate_builtin("join", &args).unwrap();
        assert_eq!(result, Value::String("hello, 42, true".to_string()));
    }

    #[test]
    fn join_wrong_arg_count() {
        let args = vec![Value::String("-".to_string())];
        let result = evaluate_builtin("join", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn join_non_string_separator() {
        let args = vec![
            Value::Int(1),
            Value::List(vec![Value::String("a".to_string())]),
        ];
        let result = evaluate_builtin("join", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("first argument must be a string")
        );
    }

    #[test]
    fn join_non_list_second_arg() {
        let args = vec![
            Value::String("-".to_string()),
            Value::String("not a list".to_string()),
        ];
        let result = evaluate_builtin("join", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("second argument must be a list")
        );
    }

    #[test]
    fn unknown_function() {
        let result = evaluate_builtin("unknown_func", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown built-in function"));
    }
}
