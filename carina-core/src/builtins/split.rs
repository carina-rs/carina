//! `split(separator, string)` built-in function

use crate::resource::Value;

use super::value_type_name;

/// `split(separator, string)` - Split a string into a list by a separator.
///
/// - First argument: separator (String)
/// - Second argument: string to split (String)
/// - Returns: List of Strings
///
/// Examples:
/// ```text
/// split("-", "a-b-c")  // => ["a", "b", "c"]
/// "a-b-c" |> split("-") // => ["a", "b", "c"] (pipe form)
/// ```
pub(crate) fn builtin_split(args: &[Value]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(format!(
            "split() expects 2 arguments (separator, string), got {}",
            args.len()
        ));
    }

    let separator = match &args[0] {
        Value::String(s) => s.clone(),
        other => {
            return Err(format!(
                "split() first argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    let input = match &args[1] {
        Value::String(s) => s.clone(),
        other => {
            return Err(format!(
                "split() second argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    let parts: Vec<Value> = input
        .split(&separator)
        .map(|s| Value::String(s.to_string()))
        .collect();

    Ok(Value::List(parts))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin;
    use crate::resource::Value;

    #[test]
    fn split_basic() {
        let args = vec![
            Value::String("-".to_string()),
            Value::String("a-b-c".to_string()),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        assert_eq!(
            result,
            Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
                Value::String("c".to_string()),
            ])
        );
    }

    #[test]
    fn split_empty_separator() {
        let args = vec![
            Value::String("".to_string()),
            Value::String("abc".to_string()),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        // Rust's split("") yields ["", "a", "b", "c", ""]
        assert_eq!(
            result,
            Value::List(vec![
                Value::String("".to_string()),
                Value::String("a".to_string()),
                Value::String("b".to_string()),
                Value::String("c".to_string()),
                Value::String("".to_string()),
            ])
        );
    }

    #[test]
    fn split_no_match() {
        let args = vec![
            Value::String(",".to_string()),
            Value::String("no-commas-here".to_string()),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        assert_eq!(
            result,
            Value::List(vec![Value::String("no-commas-here".to_string())])
        );
    }

    #[test]
    fn split_empty_string() {
        let args = vec![
            Value::String("-".to_string()),
            Value::String("".to_string()),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        assert_eq!(result, Value::List(vec![Value::String("".to_string())]));
    }

    #[test]
    fn split_multi_char_separator() {
        let args = vec![
            Value::String("::".to_string()),
            Value::String("a::b::c".to_string()),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        assert_eq!(
            result,
            Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
                Value::String("c".to_string()),
            ])
        );
    }

    #[test]
    fn split_single_element() {
        let args = vec![
            Value::String("-".to_string()),
            Value::String("only".to_string()),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        assert_eq!(result, Value::List(vec![Value::String("only".to_string())]));
    }

    #[test]
    fn split_partial_application() {
        let args = vec![Value::String("-".to_string())];
        let result = evaluate_builtin("split", &args).unwrap();
        assert!(result.is_closure());
    }

    #[test]
    fn split_non_string_separator() {
        let args = vec![Value::Int(1), Value::String("a-b".to_string())];
        let result = evaluate_builtin("split", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("first argument must be a string")
        );
    }

    #[test]
    fn split_non_string_second_arg() {
        let args = vec![
            Value::String("-".to_string()),
            Value::List(vec![Value::String("a".to_string())]),
        ];
        let result = evaluate_builtin("split", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("second argument must be a string")
        );
    }
}
