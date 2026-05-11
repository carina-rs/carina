//! `concat(items, base_list)` built-in function

use crate::resource::{ConcreteValue, Value};

use super::value_type_name;

/// `concat(items, base_list)` - Concatenate two lists into one.
///
/// Follows F#/Haskell convention: data argument (base_list) is last,
/// so pipe form works naturally: `base_list |> concat(items)`.
///
/// The result is `base_list ++ items` (base first, then items appended).
///
/// - First argument: items to append (List)
/// - Second argument: base list (List) — the data argument
/// - Returns: a new List with base_list elements followed by items
///
/// Examples:
/// ```text
/// concat([3, 4], [1, 2])         // => [1, 2, 3, 4]
/// [1, 2] |> concat([3, 4])       // => [1, 2, 3, 4] (pipe form)
/// ```
pub(crate) fn builtin_concat(args: &[Value]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(format!(
            "concat() expects 2 arguments (items, base_list), got {}",
            args.len()
        ));
    }

    let items = match &args[0] {
        Value::Concrete(ConcreteValue::List(items)) => items,
        other => {
            return Err(format!(
                "concat() first argument must be a list, got {}",
                value_type_name(other)
            ));
        }
    };

    let base = match &args[1] {
        Value::Concrete(ConcreteValue::List(items)) => items,
        other => {
            return Err(format!(
                "concat() second argument must be a list, got {}",
                value_type_name(other)
            ));
        }
    };

    // base_list first, then items appended
    let mut result = base.clone();
    result.extend(items.iter().cloned());
    Ok(Value::Concrete(ConcreteValue::List(result)))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::{ConcreteValue, Value};

    #[test]
    fn concat_basic() {
        // concat(items, base_list) => base_list ++ items
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(3)),
                Value::Concrete(ConcreteValue::Int(4)),
            ])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
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
    fn concat_strings() {
        // concat(items=["c"], base=["a", "b"]) => ["a", "b", "c"]
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("c".to_string()),
            )])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".to_string())),
                Value::Concrete(ConcreteValue::String("b".to_string())),
            ])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".to_string())),
                Value::Concrete(ConcreteValue::String("b".to_string())),
                Value::Concrete(ConcreteValue::String("c".to_string())),
            ]))
        );
    }

    #[test]
    fn concat_mixed_types() {
        // concat(items=[true], base=[1, "two"]) => [1, "two", true]
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Bool(true),
            )])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::String("two".to_string())),
            ])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::String("two".to_string())),
                Value::Concrete(ConcreteValue::Bool(true)),
            ]))
        );
    }

    #[test]
    fn concat_empty_first() {
        // concat(items=[], base=[1, 2]) => [1, 2]
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2))
            ]))
        );
    }

    #[test]
    fn concat_empty_second() {
        // concat(items=[1, 2], base=[]) => [1, 2]
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2))
            ]))
        );
    }

    #[test]
    fn concat_both_empty() {
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::List(vec![])));
    }

    #[test]
    fn concat_partial_application_one_arg() {
        use crate::builtins::evaluate_builtin_for_tests;
        let args = vec![Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Int(1),
        )]))];
        let result = evaluate_builtin_for_tests("concat", &args).unwrap();
        assert!(result.is_closure());
    }

    #[test]
    fn concat_wrong_arg_count_three() {
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("concat", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn concat_first_arg_not_list() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("not a list".to_string())),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(1),
            )])),
        ];
        let result = evaluate_builtin("concat", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("first argument must be a list")
        );
    }

    #[test]
    fn concat_second_arg_not_list() {
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(1),
            )])),
            Value::Concrete(ConcreteValue::String("not a list".to_string())),
        ];
        let result = evaluate_builtin("concat", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("second argument must be a list")
        );
    }
}
