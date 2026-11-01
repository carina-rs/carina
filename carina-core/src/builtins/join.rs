//! `join(separator, list)` built-in function

use crate::resource::{
    BindingAddress, ConcreteValue, DeferredValue, NoPlaceholder, Value, ValueString,
};

use super::value_type_name;

/// `join(separator, list)` - Join list elements into a string with a separator.
///
/// - First argument: separator (String)
/// - Second argument: list of values (each converted to string)
/// - Returns: String
///
/// Examples:
/// ```text
/// join("-", ["a", "b", "c"])  // => "a-b-c"
/// ["a", "b"] |> join("-")     // => "a-b" (pipe form)
/// ```
pub(crate) fn builtin_join<B: BindingAddress>(
    args: &[Value<NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, String> {
    if args.len() != 2 {
        return Err(format!(
            "join() expects 2 arguments (separator, list), got {}",
            args.len()
        ));
    }

    let separator = match &args[0] {
        Value::Concrete(ConcreteValue::String(s)) => s,
        other => {
            return Err(format!(
                "join() first argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    let items = match &args[1] {
        Value::Concrete(ConcreteValue::List(items)) => items,
        other => {
            return Err(format!(
                "join() second argument must be a list, got {}",
                value_type_name(other)
            ));
        }
    };

    let mut contains_secret = false;
    let rendered = items
        .iter()
        .map(|value| {
            let (text, item_secret) = join_scalar(value)?;
            contains_secret |= item_secret;
            Ok(text)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let joined = rendered
        .iter()
        .map(ValueString::expose_plaintext)
        .collect::<Vec<_>>()
        .join(separator.expose_plaintext());
    let joined =
        ValueString::derived_from(joined, std::iter::once(separator).chain(rendered.iter()));
    let value = Value::Concrete(ConcreteValue::String(joined));
    if contains_secret {
        Ok(Value::Deferred(DeferredValue::Secret(Box::new(value))))
    } else {
        Ok(value)
    }
}

fn join_scalar<B: BindingAddress>(
    value: &Value<NoPlaceholder, B>,
) -> Result<(ValueString, bool), String> {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => Ok((value.clone(), false)),
        Value::Concrete(ConcreteValue::EnumIdentifier(value)) => Ok((value.as_str().into(), false)),
        Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
            Ok((value.api_value().into(), false))
        }
        Value::Concrete(ConcreteValue::Int(value)) => Ok((value.to_string().into(), false)),
        Value::Concrete(ConcreteValue::Float(value)) => Ok((value.to_string().into(), false)),
        Value::Concrete(ConcreteValue::Bool(value)) => Ok((value.to_string().into(), false)),
        Value::Concrete(ConcreteValue::Duration(value)) => {
            Ok((crate::value::render_duration(*value).into(), false))
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            let (text, _) = join_scalar(inner)?;
            Ok((
                ValueString::diagnostic_sensitive(text.into_plaintext()),
                true,
            ))
        }
        other => Err(format!(
            "join() list elements must be scalar, got {}",
            value_type_name(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::ValueDescription;
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
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
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("hunter2SECRET".into()),
                )))),
            ])),
        ];

        let result = evaluate_builtin("join", &args).unwrap();

        assert_eq!(
            result,
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("prefix-hunter2SECRET".into()),
            ))))
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
