//! `env(name)` built-in function

use crate::resource::Value;

use super::value_type_name;

/// `env(name)` - Read an environment variable.
///
/// - First argument: name of the environment variable (string)
/// - Returns: The value of the environment variable as a string
/// - Errors if the variable is not set or the argument is not a string
///
/// Examples:
/// ```text
/// env("HOME")          // => "/home/user"
/// env("DB_PASSWORD")   // => "s3cret"
/// ```
pub(crate) fn builtin_env(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!(
            "env() expects 1 argument (variable name), got {}",
            args.len()
        ));
    }

    let name = match &args[0] {
        Value::String(s) => s,
        other => {
            return Err(format!(
                "env() argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    std::env::var(name)
        .map(Value::String)
        .map_err(|_| format!("environment variable '{}' is not set", name))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin;
    use crate::resource::Value;

    #[test]
    fn env_reads_set_variable() {
        let var_name = "CARINA_TEST_ENV_READS_SET";
        unsafe {
            std::env::set_var(var_name, "hello_world");
        }
        let args = vec![Value::String(var_name.to_string())];
        let result = evaluate_builtin("env", &args).unwrap();
        assert_eq!(result, Value::String("hello_world".to_string()));
        unsafe {
            std::env::remove_var(var_name);
        }
    }

    #[test]
    fn env_error_on_unset_variable() {
        let var_name = "CARINA_TEST_ENV_UNSET_12345";
        // Ensure it's not set
        unsafe {
            std::env::remove_var(var_name);
        }
        let args = vec![Value::String(var_name.to_string())];
        let result = evaluate_builtin("env", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("is not set"));
    }

    #[test]
    fn env_error_on_non_string_arg() {
        let args = vec![Value::Int(42)];
        let result = evaluate_builtin("env", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("argument must be a string"));
    }

    #[test]
    fn env_error_on_no_args() {
        let args = vec![];
        let result = evaluate_builtin("env", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn env_error_on_too_many_args() {
        let args = vec![
            Value::String("A".to_string()),
            Value::String("B".to_string()),
        ];
        let result = evaluate_builtin("env", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn env_reads_empty_value() {
        let var_name = "CARINA_TEST_ENV_EMPTY_VAL";
        unsafe {
            std::env::set_var(var_name, "");
        }
        let args = vec![Value::String(var_name.to_string())];
        let result = evaluate_builtin("env", &args).unwrap();
        assert_eq!(result, Value::String("".to_string()));
        unsafe {
            std::env::remove_var(var_name);
        }
    }
}
