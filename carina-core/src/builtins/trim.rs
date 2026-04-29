//! `trim(string)` built-in function

use crate::resource::Value;

use super::value_type_name;

/// `trim(string)` - Remove leading and trailing whitespace from a string.
///
/// - First argument: string to trim
/// - Returns: String with whitespace removed from both ends
///
/// Examples:
/// ```text
/// trim("  hello  ")  // => "hello"
/// trim("\n hello \t") // => "hello"
/// ```
pub(crate) fn builtin_trim(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!(
            "trim() expects 1 argument (string), got {}",
            args.len()
        ));
    }

    let s = match &args[0] {
        Value::String(s) => s,
        other => {
            return Err(format!(
                "trim() argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    Ok(Value::String(s.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::Value;

    #[test]
    fn trim_both_sides() {
        let args = vec![Value::String("  hello  ".to_string())];
        let result = evaluate_builtin("trim", &args).unwrap();
        assert_eq!(result, Value::String("hello".to_string()));
    }

    #[test]
    fn trim_leading_only() {
        let args = vec![Value::String("  hello".to_string())];
        let result = evaluate_builtin("trim", &args).unwrap();
        assert_eq!(result, Value::String("hello".to_string()));
    }

    #[test]
    fn trim_trailing_only() {
        let args = vec![Value::String("hello  ".to_string())];
        let result = evaluate_builtin("trim", &args).unwrap();
        assert_eq!(result, Value::String("hello".to_string()));
    }

    #[test]
    fn trim_no_whitespace() {
        let args = vec![Value::String("hello".to_string())];
        let result = evaluate_builtin("trim", &args).unwrap();
        assert_eq!(result, Value::String("hello".to_string()));
    }

    #[test]
    fn trim_empty_string() {
        let args = vec![Value::String("".to_string())];
        let result = evaluate_builtin("trim", &args).unwrap();
        assert_eq!(result, Value::String("".to_string()));
    }

    #[test]
    fn trim_whitespace_only() {
        let args = vec![Value::String("   ".to_string())];
        let result = evaluate_builtin("trim", &args).unwrap();
        assert_eq!(result, Value::String("".to_string()));
    }

    #[test]
    fn trim_tabs_and_newlines() {
        let args = vec![Value::String("\n\t hello \t\n".to_string())];
        let result = evaluate_builtin("trim", &args).unwrap();
        assert_eq!(result, Value::String("hello".to_string()));
    }

    #[test]
    fn trim_preserves_inner_whitespace() {
        let args = vec![Value::String("  hello world  ".to_string())];
        let result = evaluate_builtin("trim", &args).unwrap();
        assert_eq!(result, Value::String("hello world".to_string()));
    }

    #[test]
    fn trim_wrong_arg_count() {
        let args = vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ];
        let result = evaluate_builtin("trim", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn trim_no_args() {
        let args = vec![];
        let result = evaluate_builtin("trim", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn trim_non_string_arg() {
        let args = vec![Value::Int(42)];
        let result = evaluate_builtin("trim", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("argument must be a string"));
    }
}
