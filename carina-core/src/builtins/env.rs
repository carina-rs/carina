//! `env(name)` built-in function

use crate::EnvironmentSnapshot;
use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, NoPlaceholder, Value, ValueString,
};

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
    environment: &EnvironmentSnapshot,
) -> Result<Value<NoPlaceholder, B>, String> {
    let (name, secret) = parse_env_args(args)?;
    let value = environment
        .get(name.expose_plaintext())
        .and_then(|value| value.to_str())
        .map(|s| Value::Concrete(ConcreteValue::String(ValueString::diagnostic_sensitive(s))))
        .ok_or_else(|| {
            let literal = name.diagnostic_literal().filter(|_name| !secret);
            match literal {
                Some(name) => format!("environment variable '{name}' is not set"),
                None => "environment variable named by a sensitive value is not set".to_string(),
            }
        })?;
    Ok(if secret {
        Value::Deferred(DeferredValue::Secret(Box::new(value)))
    } else {
        value
    })
}

fn parse_env_args<B: BindingAddress>(
    args: &[Value<NoPlaceholder, B>],
) -> Result<(&ValueString, bool), String> {
    if args.len() != 1 {
        return Err(format!(
            "env() expects 1 argument (variable name), got {}",
            args.len()
        ));
    }

    // Preserve the historical env(secret(name)) result and diagnostic masking
    // without routing environment lookups through the transforming builtins.
    let mut argument = &args[0];
    let mut secret = false;
    while let Value::Deferred(DeferredValue::Secret(inner)) = argument {
        argument = inner;
        secret = true;
    }
    match argument {
        Value::Concrete(ConcreteValue::String(name)) => Ok((name, secret)),
        other => Err(format!(
            "env() argument must be a string, got {}",
            value_type_name(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::ValueDescription;
    use crate::builtins::evaluate_builtin_with_config_to_value as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};
    use crate::value::{format_value, value_to_json};
    use crate::{EnvironmentSnapshot, parser::ProviderContext};

    #[test]
    fn env_dispatch_uses_the_supplied_snapshot() {
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            "VARIABLE".to_string(),
            "captured".into(),
        )]));
        let args = [Value::Concrete(ConcreteValue::String("VARIABLE".into()))];
        assert_eq!(
            crate::builtins::evaluate_builtin_to_value("env", &args, &config).unwrap(),
            Value::Concrete(ConcreteValue::String("captured".into())),
        );
        let invalid = crate::builtins::evaluate_builtin_to_value::<
            crate::resource::RelativeBindingName,
        >("env", &[], &config)
        .unwrap_err();
        assert!(invalid.contains("expects 1 argument"));
    }

    #[test]
    fn env_snapshots_are_isolated_across_threads() {
        std::thread::scope(|threads| {
            for expected in ["first", "second"] {
                threads.spawn(move || {
                    let config = ProviderContext::new(EnvironmentSnapshot::new([(
                        "SHARED_NAME".to_string(),
                        expected.into(),
                    )]));
                    let args = [Value::Concrete(ConcreteValue::String("SHARED_NAME".into()))];
                    let value = evaluate_builtin("env", &args, &config).unwrap();
                    assert_eq!(
                        value,
                        Value::Concrete(ConcreteValue::String(expected.into()))
                    );
                });
            }
        });
    }

    #[test]
    #[cfg(unix)]
    fn env_non_utf8_value_uses_the_unset_error_and_masks_sensitive_names() {
        use std::os::unix::ffi::OsStringExt;
        let name = "NON_UTF8";
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            name.to_string(),
            std::ffi::OsString::from_vec(vec![0xff]),
        )]));
        let args = [Value::Concrete(ConcreteValue::String(name.into()))];
        assert_eq!(
            evaluate_builtin("env", &args, &config).unwrap_err(),
            "environment variable 'NON_UTF8' is not set"
        );
        let sensitive_args = [Value::Concrete(ConcreteValue::String(
            crate::resource::ValueString::diagnostic_sensitive(name),
        ))];
        assert_eq!(
            evaluate_builtin("env", &sensitive_args, &config).unwrap_err(),
            "environment variable named by a sensitive value is not set"
        );
    }

    #[test]
    fn env_reads_set_variable() {
        let var_name = "CARINA_TEST_ENV_READS_SET";
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            var_name.to_string(),
            "hello_world".into(),
        )]));
        let args = vec![Value::Concrete(ConcreteValue::String(
            var_name.to_string().into(),
        ))];
        let result = evaluate_builtin("env", &args, &config).unwrap();
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
    }

    #[test]
    fn env_error_on_unset_variable() {
        let var_name = "CARINA_TEST_ENV_UNSET_12345";
        let config = ProviderContext::new(EnvironmentSnapshot::empty());
        let args = vec![Value::Concrete(ConcreteValue::String(
            var_name.to_string().into(),
        ))];
        let result = evaluate_builtin("env", &args, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("is not set"));
    }

    #[test]
    fn env_error_on_non_string_arg() {
        let config = ProviderContext::new(EnvironmentSnapshot::empty());
        let args = vec![Value::Concrete(ConcreteValue::Int(42))];
        let result = evaluate_builtin("env", &args, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("argument must be a string"));
    }

    #[test]
    fn env_preserves_origin_main_secret_argument_behavior() {
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            "SECRET_NAME".to_string(),
            "captured".into(),
        )]));
        let secret = |value| {
            Value::Deferred(crate::resource::ExpandedDeferredValue::Secret(Box::new(
                value,
            )))
        };
        let name = Value::Concrete(ConcreteValue::String("SECRET_NAME".into()));
        let expected = secret(Value::Concrete(ConcreteValue::String("captured".into())));
        for argument in [secret(name.clone()), secret(secret(name))] {
            assert_eq!(
                evaluate_builtin("env", &[argument], &config).unwrap(),
                expected
            );
        }
        let missing = secret(Value::Concrete(ConcreteValue::String(
            "MISSING_SECRET_NAME".into(),
        )));
        assert_eq!(
            evaluate_builtin("env", &[missing], &config).unwrap_err(),
            "environment variable named by a sensitive value is not set",
        );
        for (value, kind) in [
            (Value::Concrete(ConcreteValue::Int(42)), "Int"),
            (Value::Concrete(ConcreteValue::Bool(true)), "Bool"),
            (Value::Concrete(ConcreteValue::List(Vec::new())), "List"),
        ] {
            for argument in [value.clone(), secret(value)] {
                assert_eq!(
                    evaluate_builtin("env", &[argument], &config).unwrap_err(),
                    format!("env() argument must be a string, got {kind}"),
                );
            }
        }
    }

    #[test]
    fn env_error_on_no_args() {
        let config = ProviderContext::new(EnvironmentSnapshot::empty());
        let args: Vec<Value> = vec![];
        let result = evaluate_builtin("env", &args, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn env_error_on_too_many_args() {
        let config = ProviderContext::new(EnvironmentSnapshot::empty());
        let args = vec![
            Value::Concrete(ConcreteValue::String("A".into())),
            Value::Concrete(ConcreteValue::String("B".into())),
        ];
        let result = evaluate_builtin("env", &args, &config);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn env_reads_empty_value() {
        let var_name = "CARINA_TEST_ENV_EMPTY_VAL";
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            var_name.to_string(),
            "".into(),
        )]));
        let args = vec![Value::Concrete(ConcreteValue::String(
            var_name.to_string().into(),
        ))];
        let result = evaluate_builtin("env", &args, &config).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::String("".into())));
    }

    #[test]
    fn env_sensitivity_propagates_through_string_builtins() {
        let var_name = "CARINA_TEST_ENV_SENSITIVITY_PROPAGATES";
        let config = ProviderContext::new(EnvironmentSnapshot::new([(
            var_name.to_string(),
            "hunter2SECRET".into(),
        )]));
        let env_value = evaluate_builtin(
            "env",
            &[Value::Concrete(ConcreteValue::String(
                var_name.to_string().into(),
            ))],
            &config,
        )
        .unwrap();

        let upper = evaluate_builtin("upper", std::slice::from_ref(&env_value), &config).unwrap();
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
            &config,
        )
        .unwrap();
        assert_eq!(ValueDescription::of(&joined).to_string(), "string");

        let concatenated = evaluate_builtin(
            "concat",
            &[
                Value::Concrete(ConcreteValue::List(vec![env_value])),
                Value::Concrete(ConcreteValue::List(vec![])),
            ],
            &config,
        )
        .unwrap();
        let Value::Concrete(ConcreteValue::List(items)) = concatenated else {
            panic!("concat() must return a list");
        };
        assert_eq!(ValueDescription::of(&items[0]).to_string(), "string");
    }
}
