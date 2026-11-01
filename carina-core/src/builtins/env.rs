//! `env(name)` built-in function

use crate::resource::{BindingAddress, ConcreteValue, NoPlaceholder, Value, ValueString};

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
pub(crate) fn builtin_env<B: BindingAddress>(
    args: &[Value<NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, String> {
    if args.len() != 1 {
        return Err(format!(
            "env() expects 1 argument (variable name), got {}",
            args.len()
        ));
    }

    let name = match &args[0] {
        Value::Concrete(ConcreteValue::String(s)) => s,
        other => {
            return Err(format!(
                "env() argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    std::env::var(name.expose_plaintext())
        .map(|s| Value::Concrete(ConcreteValue::String(ValueString::diagnostic_sensitive(s))))
        .map_err(|_| match name.diagnostic_literal() {
            Some(name) => format!("environment variable '{name}' is not set"),
            None => "environment variable named by a sensitive value is not set".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use crate::ValueDescription;
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};
    use crate::value::{format_value, value_to_json};

    #[test]
    fn env_reads_set_variable() {
        let var_name = "CARINA_TEST_ENV_READS_SET";
        unsafe {
            std::env::set_var(var_name, "hello_world");
        }
        let args = vec![Value::Concrete(ConcreteValue::String(
            var_name.to_string().into(),
        ))];
        let result = evaluate_builtin("env", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("hello_world".into()))
        );
        assert_eq!(ValueDescription::of(&result).to_string(), "string");
        assert_eq!(format_value(&result), "'hello_world'");
        assert_eq!(
            value_to_json(&result).unwrap(),
            serde_json::json!("hello_world")
        );
        assert!(
            matches!(&result, Value::Concrete(ConcreteValue::String(value)) if value.is_diagnostic_sensitive())
        );
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
        let args = vec![Value::Concrete(ConcreteValue::String(
            var_name.to_string().into(),
        ))];
        let result = evaluate_builtin("env", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("is not set"));
    }

    #[test]
    fn env_error_on_non_string_arg() {
        let args = vec![Value::Concrete(ConcreteValue::Int(42))];
        let result = evaluate_builtin("env", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("argument must be a string"));
    }

    #[test]
    fn env_error_on_no_args() {
        let args: Vec<Value> = vec![];
        let result = evaluate_builtin("env", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn env_error_on_too_many_args() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("A".into())),
            Value::Concrete(ConcreteValue::String("B".into())),
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
        let args = vec![Value::Concrete(ConcreteValue::String(
            var_name.to_string().into(),
        ))];
        let result = evaluate_builtin("env", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::String("".into())));
        unsafe {
            std::env::remove_var(var_name);
        }
    }

    #[test]
    fn env_sensitivity_propagates_through_string_builtins() {
        let var_name = "CARINA_TEST_ENV_SENSITIVITY_PROPAGATES";
        unsafe {
            std::env::set_var(var_name, "hunter2SECRET");
        }
        let env_value = evaluate_builtin(
            "env",
            &[Value::Concrete(ConcreteValue::String(
                var_name.to_string().into(),
            ))],
        )
        .unwrap();

        let upper = evaluate_builtin("upper", std::slice::from_ref(&env_value)).unwrap();
        assert_eq!(ValueDescription::of(&upper).to_string(), "string");

        let joined = evaluate_builtin(
            "join",
            &[
                Value::Concrete(ConcreteValue::String("-".into())),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("prefix".into())),
                    env_value.clone(),
                ])),
            ],
        )
        .unwrap();
        assert_eq!(ValueDescription::of(&joined).to_string(), "string");

        let concatenated = evaluate_builtin(
            "concat",
            &[
                Value::Concrete(ConcreteValue::List(vec![env_value])),
                Value::Concrete(ConcreteValue::List(vec![])),
            ],
        )
        .unwrap();
        let Value::Concrete(ConcreteValue::List(items)) = concatenated else {
            panic!("concat() must return a list");
        };
        assert_eq!(ValueDescription::of(&items[0]).to_string(), "string");

        unsafe {
            std::env::remove_var(var_name);
        }
    }
}
