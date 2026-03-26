//! `upper(string)` and `lower(string)` built-in functions

use crate::resource::Value;

use super::value_type_name;

/// `upper(string)` - Convert a string to uppercase.
///
/// - Single argument: a string value
/// - Returns: String with all characters converted to uppercase
///
/// Examples:
/// ```text
/// upper("hello")  // => "HELLO"
/// ```
pub(crate) fn builtin_upper(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!(
            "upper() expects 1 argument (string), got {}",
            args.len()
        ));
    }

    match &args[0] {
        Value::String(s) => Ok(Value::String(s.to_uppercase())),
        other => Err(format!(
            "upper() argument must be a string, got {}",
            value_type_name(other)
        )),
    }
}

/// `lower(string)` - Convert a string to lowercase.
///
/// - Single argument: a string value
/// - Returns: String with all characters converted to lowercase
///
/// Examples:
/// ```text
/// lower("HELLO")  // => "hello"
/// ```
pub(crate) fn builtin_lower(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!(
            "lower() expects 1 argument (string), got {}",
            args.len()
        ));
    }

    match &args[0] {
        Value::String(s) => Ok(Value::String(s.to_lowercase())),
        other => Err(format!(
            "lower() argument must be a string, got {}",
            value_type_name(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin;
    use crate::resource::Value;

    #[test]
    fn upper_basic() {
        let args = vec![Value::String("hello".to_string())];
        let result = evaluate_builtin("upper", &args).unwrap();
        assert_eq!(result, Value::String("HELLO".to_string()));
    }

    #[test]
    fn upper_already_uppercase() {
        let args = vec![Value::String("HELLO".to_string())];
        let result = evaluate_builtin("upper", &args).unwrap();
        assert_eq!(result, Value::String("HELLO".to_string()));
    }

    #[test]
    fn upper_mixed_case() {
        let args = vec![Value::String("Hello World".to_string())];
        let result = evaluate_builtin("upper", &args).unwrap();
        assert_eq!(result, Value::String("HELLO WORLD".to_string()));
    }

    #[test]
    fn upper_empty_string() {
        let args = vec![Value::String("".to_string())];
        let result = evaluate_builtin("upper", &args).unwrap();
        assert_eq!(result, Value::String("".to_string()));
    }

    #[test]
    fn upper_with_numbers_and_symbols() {
        let args = vec![Value::String("abc-123_def".to_string())];
        let result = evaluate_builtin("upper", &args).unwrap();
        assert_eq!(result, Value::String("ABC-123_DEF".to_string()));
    }

    #[test]
    fn upper_wrong_arg_count() {
        let args = vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ];
        let result = evaluate_builtin("upper", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn upper_non_string_arg() {
        let args = vec![Value::Int(42)];
        let result = evaluate_builtin("upper", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("argument must be a string"));
    }

    #[test]
    fn lower_basic() {
        let args = vec![Value::String("HELLO".to_string())];
        let result = evaluate_builtin("lower", &args).unwrap();
        assert_eq!(result, Value::String("hello".to_string()));
    }

    #[test]
    fn lower_already_lowercase() {
        let args = vec![Value::String("hello".to_string())];
        let result = evaluate_builtin("lower", &args).unwrap();
        assert_eq!(result, Value::String("hello".to_string()));
    }

    #[test]
    fn lower_mixed_case() {
        let args = vec![Value::String("Hello World".to_string())];
        let result = evaluate_builtin("lower", &args).unwrap();
        assert_eq!(result, Value::String("hello world".to_string()));
    }

    #[test]
    fn lower_empty_string() {
        let args = vec![Value::String("".to_string())];
        let result = evaluate_builtin("lower", &args).unwrap();
        assert_eq!(result, Value::String("".to_string()));
    }

    #[test]
    fn lower_with_numbers_and_symbols() {
        let args = vec![Value::String("ABC-123_DEF".to_string())];
        let result = evaluate_builtin("lower", &args).unwrap();
        assert_eq!(result, Value::String("abc-123_def".to_string()));
    }

    #[test]
    fn lower_wrong_arg_count() {
        let result = evaluate_builtin("lower", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn lower_non_string_arg() {
        let args = vec![Value::Bool(true)];
        let result = evaluate_builtin("lower", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("argument must be a string"));
    }
}
