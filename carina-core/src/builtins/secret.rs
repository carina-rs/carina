#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value_for_tests as evaluate_builtin;
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
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String("my-password".into()))).into()
            ))
        );
    }

    #[test]
    fn secret_wraps_any_value() {
        let args = vec![Value::Concrete(ConcreteValue::Int(42))];
        let result = evaluate_builtin("secret", &args).unwrap();
        assert_eq!(
            result,
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::Int(42))).into()
            ))
        );
    }

    #[test]
    fn secret_is_idempotent() {
        let secret = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String("my-password".into()))).into(),
        ));

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
