//! `split(separator, string)` built-in function

use crate::resource::{BindingAddress, ConcreteValue, NoPlaceholder, Value, ValueString};

use super::value_type_name;

/// `split(separator, string)` - Split a string into a list by a separator.
///
/// - First argument: separator (String)
/// - Second argument: string to split (String)
/// - Returns: List of Strings
///
/// Examples:
/// ```text
/// split("-", "a-b-c")  // => ["a", "b", "c"]
/// "a-b-c" |> split("-") // => ["a", "b", "c"] (pipe form)
/// ```
pub(crate) fn builtin_split<B: BindingAddress>(
    args: &[Value<NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, String> {
    if args.len() != 2 {
        return Err(format!(
            "split() expects 2 arguments (separator, string), got {}",
            args.len()
        ));
    }

    let separator = match &args[0] {
        Value::Concrete(ConcreteValue::String(s)) => s,
        other => {
            return Err(format!(
                "split() first argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    let input = match &args[1] {
        Value::Concrete(ConcreteValue::String(s)) => s,
        other => {
            return Err(format!(
                "split() second argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    let parts: Vec<Value<NoPlaceholder, B>> = input
        .expose_plaintext()
        .split(separator.expose_plaintext())
        .map(|part| {
            Value::Concrete(ConcreteValue::String(ValueString::derived_from(
                part,
                [separator, input],
            )))
        })
        .collect();

    Ok(Value::Concrete(ConcreteValue::List(parts)))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

    #[test]
    fn split_basic() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("-".into())),
            Value::Concrete(ConcreteValue::String("a-b-c".into())),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".into())),
                Value::Concrete(ConcreteValue::String("b".into())),
                Value::Concrete(ConcreteValue::String("c".into())),
            ]))
        );
    }

    #[test]
    fn split_empty_separator() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("".into())),
            Value::Concrete(ConcreteValue::String("abc".into())),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        // Rust's split("") yields ["", "a", "b", "c", ""]
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("".into())),
                Value::Concrete(ConcreteValue::String("a".into())),
                Value::Concrete(ConcreteValue::String("b".into())),
                Value::Concrete(ConcreteValue::String("c".into())),
                Value::Concrete(ConcreteValue::String("".into())),
            ]))
        );
    }

    #[test]
    fn split_no_match() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(",".into())),
            Value::Concrete(ConcreteValue::String("no-commas-here".into())),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("no-commas-here".into())
            )]))
        );
    }

    #[test]
    fn split_empty_string() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("-".into())),
            Value::Concrete(ConcreteValue::String("".into())),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("".into())
            )]))
        );
    }

    #[test]
    fn split_multi_char_separator() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("::".into())),
            Value::Concrete(ConcreteValue::String("a::b::c".into())),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".into())),
                Value::Concrete(ConcreteValue::String("b".into())),
                Value::Concrete(ConcreteValue::String("c".into())),
            ]))
        );
    }

    #[test]
    fn split_single_element() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("-".into())),
            Value::Concrete(ConcreteValue::String("only".into())),
        ];
        let result = evaluate_builtin("split", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("only".into())
            )]))
        );
    }

    #[test]
    fn split_partial_application() {
        use crate::builtins::evaluate_builtin_for_tests;
        let args = vec![Value::Concrete(ConcreteValue::String("-".into()))];
        let result = evaluate_builtin_for_tests("split", &args).unwrap();
        assert!(result.is_closure());
    }

    #[test]
    fn split_non_string_separator() {
        let args = vec![
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::String("a-b".into())),
        ];
        let result = evaluate_builtin("split", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("first argument must be a string")
        );
    }

    #[test]
    fn split_non_string_second_arg() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("-".into())),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("a".into()),
            )])),
        ];
        let result = evaluate_builtin("split", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("second argument must be a string")
        );
    }
}
