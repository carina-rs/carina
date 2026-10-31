//! `secret(value)` built-in function

use crate::resource::{BindingAddress, DeferredValue, Value};

/// `secret(value)` - Mark a value as secret.
///
/// The inner value is sent to the provider but stored only as an Argon2id hash
/// in state. Plan output displays `(secret)` instead of the actual value.
///
/// Examples:
/// ```text
/// secret("my-password")       // => Value::Deferred(DeferredValue::Secret(Value::String("my-password")))
/// secret(env("DB_PASSWORD"))  // => Value::Deferred(DeferredValue::Secret(Value::String(<env value>)))
/// ```
pub(crate) fn builtin_secret<B: BindingAddress>(args: &[Value<B>]) -> Result<Value<B>, String> {
    if args.len() != 1 {
        return Err(format!("secret() expects 1 argument, got {}", args.len()));
    }

    match &args[0] {
        already_secret @ Value::Deferred(DeferredValue::Secret(_)) => Ok(already_secret.clone()),
        value => Ok(Value::Deferred(DeferredValue::Secret(Box::new(
            value.clone(),
        )))),
    }
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::{
        ExpandedConcreteValue as ConcreteValue, ExpandedDeferredValue as DeferredValue,
        ExpandedValue as Value,
    };

    #[test]
    fn secret_wraps_string_value() {
        let args = vec![Value::Concrete(ConcreteValue::String("my-password".into()))];
        let result = evaluate_builtin("secret", &args).unwrap();
        assert_eq!(
            result,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("my-password".into())
            ))))
        );
    }

    #[test]
    fn secret_wraps_any_value() {
        let args = vec![Value::Concrete(ConcreteValue::Int(42))];
        let result = evaluate_builtin("secret", &args).unwrap();
        assert_eq!(
            result,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::Int(42)
            ))))
        );
    }

    #[test]
    fn secret_is_idempotent() {
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        ))));

        let result = evaluate_builtin("secret", std::slice::from_ref(&secret)).unwrap();

        assert_eq!(result, secret);
    }

    #[test]
    fn secret_error_on_no_args() {
        let args: Vec<Value> = vec![];
        let result = evaluate_builtin("secret", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn secret_error_on_too_many_args() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::String("b".into())),
        ];
        let result = evaluate_builtin("secret", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }
}
