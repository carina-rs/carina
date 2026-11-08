//! `join(separator, list)` built-in function
use crate::resolver::ResolutionError;
use crate::resource::knowledge::KnownValue;
use crate::resource::{BindingAddress, NoPlaceholder, Value};
pub(crate) fn builtin_join<B: BindingAddress>(
    args: &[KnownValue<'_, NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, ResolutionError> {
    crate::resource::knowledge::join_known_values(args)
}

#[cfg(test)]
mod tests {
    use crate::ValueDescription;
    use crate::builtins::evaluate_builtin_to_value_for_tests as evaluate_builtin;
    use crate::resource::{
        ExpandedConcreteValue as ConcreteValue, ExpandedDeferredValue as DeferredValue,
        ExpandedValue as Value,
    };
    use crate::value::{PERSISTED_SECRET_KEY, format_value, value_to_json};

    #[test]
    fn join_basic() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("-".into())),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".into())),
                Value::Concrete(ConcreteValue::String("b".into())),
                Value::Concrete(ConcreteValue::String("c".into())),
            ])),
        ];
        let result = evaluate_builtin("join", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("a-b-c".into()))
        );
    }

    #[test]
    fn join_empty_separator() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("".into())),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".into())),
                Value::Concrete(ConcreteValue::String("b".into())),
            ])),
        ];
        let result = evaluate_builtin("join", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::String("ab".into())));
    }

    #[test]
    fn join_empty_list() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("-".into())),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("join", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::String("".into())));
    }

    #[test]
    fn join_single_element() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("-".into())),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("only".into()),
            )])),
        ];
        let result = evaluate_builtin("join", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("only".into()))
        );
    }

    #[test]
    fn join_mixed_types() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(", ".into())),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("hello".into())),
                Value::Concrete(ConcreteValue::Int(42)),
                Value::Concrete(ConcreteValue::Bool(true)),
            ])),
        ];
        let result = evaluate_builtin("join", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("hello, 42, true".into()))
        );
    }

    #[test]
    fn join_over_secret_element_returns_secret_without_debug_text() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("-".into())),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("prefix".into())),
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::String(
                        "hunter2SECRET".into(),
                    )))
                    .into(),
                )),
            ])),
        ];

        let result = evaluate_builtin("join", &args).unwrap();

        assert_eq!(
            result,
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String(
                    "prefix-hunter2SECRET".into()
                ),))
                .into()
            ))
        );
        assert_eq!(format_value(&result), "(secret)");
        assert_eq!(ValueDescription::of(&result).to_string(), "secret");
        let state_value = value_to_json(&result).unwrap();
        let digest = state_value[PERSISTED_SECRET_KEY]
            .as_str()
            .expect("structural secret digest");
        assert!(crate::resource::RedactedSecretHash::from_encoded_digest(digest).is_some());
        assert!(!state_value.to_string().contains("hunter2SECRET"));
    }

    #[test]
    fn join_partial_application() {
        use crate::builtins::evaluate_builtin_for_tests;
        let args = vec![Value::Concrete(ConcreteValue::String("-".into()))];
        let result = evaluate_builtin_for_tests("join", &args).unwrap();
        assert!(result.is_closure());
    }

    #[test]
    fn join_non_string_separator() {
        let args = vec![
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("a".into()),
            )])),
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
            Value::Concrete(ConcreteValue::String("-".into())),
            Value::Concrete(ConcreteValue::String("not a list".into())),
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
        let result: Result<Value, String> = evaluate_builtin("unknown_func", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown built-in function"));
    }
}
