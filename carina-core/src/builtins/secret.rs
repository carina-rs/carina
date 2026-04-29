//! `secret(value)` built-in function

use crate::resource::Value;

/// `secret(value)` - Mark a value as secret.
///
/// The inner value is sent to the provider but stored only as a SHA256 hash
/// in state. Plan output displays `(secret)` instead of the actual value.
///
/// Examples:
/// ```text
/// secret("my-password")       // => Value::Secret(Value::String("my-password"))
/// secret(env("DB_PASSWORD"))  // => Value::Secret(Value::String(<env value>))
/// ```
pub(crate) fn builtin_secret(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!("secret() expects 1 argument, got {}", args.len()));
    }

    Ok(Value::Secret(Box::new(args[0].clone())))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::Value;

    #[test]
    fn secret_wraps_string_value() {
        let args = vec![Value::String("my-password".to_string())];
        let result = evaluate_builtin("secret", &args).unwrap();
        assert_eq!(
            result,
            Value::Secret(Box::new(Value::String("my-password".to_string())))
        );
    }

    #[test]
    fn secret_wraps_any_value() {
        let args = vec![Value::Int(42)];
        let result = evaluate_builtin("secret", &args).unwrap();
        assert_eq!(result, Value::Secret(Box::new(Value::Int(42))));
    }

    #[test]
    fn secret_error_on_no_args() {
        let args = vec![];
        let result = evaluate_builtin("secret", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn secret_error_on_too_many_args() {
        let args = vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ];
        let result = evaluate_builtin("secret", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }
}
