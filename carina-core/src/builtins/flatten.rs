//! `flatten(list)` built-in function

use crate::resource::{BindingAddress, ConcreteValue, Value};

use super::value_type_name;

/// `flatten(list)` - Flatten a list of lists by one level.
///
/// - Single argument: a List
/// - Returns: List with one level of nesting removed
///
/// Elements that are lists are expanded into the result;
/// non-list elements are kept as-is.
///
/// Examples:
/// ```text
/// flatten([[1, 2], [3, 4]])      // => [1, 2, 3, 4]
/// flatten([["a", "b"], ["c"]])   // => ["a", "b", "c"]
/// flatten([[1, 2], 3, [4]])      // => [1, 2, 3, 4]
/// ```
pub(crate) fn builtin_flatten<B: BindingAddress>(args: &[Value<B>]) -> Result<Value<B>, String> {
    if args.len() != 1 {
        return Err(format!("flatten() expects 1 argument, got {}", args.len()));
    }

    let items = match &args[0] {
        Value::Concrete(ConcreteValue::List(items)) => items,
        other => {
            return Err(format!(
                "flatten() argument must be a List, got {}",
                value_type_name(other)
            ));
        }
    };

    let mut result = Vec::new();
    for item in items {
        match item {
            Value::Concrete(ConcreteValue::List(inner)) => result.extend(inner.iter().cloned()),
            other => result.push(other.clone()),
        }
    }

    Ok(Value::Concrete(ConcreteValue::List(result)))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

    #[test]
    fn flatten_nested_lists() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(3)),
                Value::Concrete(ConcreteValue::Int(4)),
            ])),
        ]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
                Value::Concrete(ConcreteValue::Int(3)),
                Value::Concrete(ConcreteValue::Int(4)),
            ]))
        );
    }

    #[test]
    fn flatten_string_lists() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".into())),
                Value::Concrete(ConcreteValue::String("b".into())),
            ])),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("c".into()),
            )])),
        ]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
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
    fn flatten_mixed_list_and_non_list() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
            Value::Concrete(ConcreteValue::Int(3)),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(4),
            )])),
        ]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
                Value::Concrete(ConcreteValue::Int(3)),
                Value::Concrete(ConcreteValue::Int(4)),
            ]))
        );
    }

    #[test]
    fn flatten_empty_list() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::List(vec![])));
    }

    #[test]
    fn flatten_no_nested_lists() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::Int(2)),
            Value::Concrete(ConcreteValue::Int(3)),
        ]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
                Value::Concrete(ConcreteValue::Int(3)),
            ]))
        );
    }

    #[test]
    fn flatten_single_level_only() {
        // Nested [[1, [2, 3]]] should flatten to [1, [2, 3]], not [1, 2, 3]
        let args = vec![Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::Int(2)),
                    Value::Concrete(ConcreteValue::Int(3)),
                ])),
            ]),
        )]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::Int(2)),
                    Value::Concrete(ConcreteValue::Int(3))
                ])),
            ]))
        );
    }

    #[test]
    fn flatten_empty_inner_lists() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(1),
            )])),
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(2),
            )])),
        ]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ]))
        );
    }

    #[test]
    fn flatten_wrong_arg_count_zero() {
        let result: Result<Value, String> = evaluate_builtin("flatten", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn flatten_wrong_arg_count_two() {
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(1),
            )])),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(2),
            )])),
        ];
        let result = evaluate_builtin("flatten", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn flatten_invalid_type() {
        let args = vec![Value::Concrete(ConcreteValue::String("not a list".into()))];
        let result = evaluate_builtin("flatten", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a List"));
    }
}
