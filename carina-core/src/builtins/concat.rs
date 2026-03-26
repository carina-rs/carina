//! `concat(list, list)` built-in function

use crate::resource::Value;

use super::value_type_name;

/// `concat(list, list)` - Concatenate two lists into one.
///
/// - First argument: a List
/// - Second argument: a List
/// - Returns: a new List containing all elements from both lists
///
/// Examples:
/// ```text
/// concat([1, 2], [3, 4])       // => [1, 2, 3, 4]
/// concat(["a"], ["b", "c"])    // => ["a", "b", "c"]
/// ```
pub(crate) fn builtin_concat(args: &[Value]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(format!(
            "concat() expects 2 arguments (list, list), got {}",
            args.len()
        ));
    }

    let first = match &args[0] {
        Value::List(items) => items,
        other => {
            return Err(format!(
                "concat() first argument must be a list, got {}",
                value_type_name(other)
            ));
        }
    };

    let second = match &args[1] {
        Value::List(items) => items,
        other => {
            return Err(format!(
                "concat() second argument must be a list, got {}",
                value_type_name(other)
            ));
        }
    };

    let mut result = first.clone();
    result.extend(second.iter().cloned());
    Ok(Value::List(result))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin;
    use crate::resource::Value;

    #[test]
    fn concat_basic() {
        let args = vec![
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::List(vec![Value::Int(3), Value::Int(4)]),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(
            result,
            Value::List(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
                Value::Int(4),
            ])
        );
    }

    #[test]
    fn concat_strings() {
        let args = vec![
            Value::List(vec![Value::String("a".to_string())]),
            Value::List(vec![
                Value::String("b".to_string()),
                Value::String("c".to_string()),
            ]),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(
            result,
            Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
                Value::String("c".to_string()),
            ])
        );
    }

    #[test]
    fn concat_mixed_types() {
        let args = vec![
            Value::List(vec![Value::Int(1), Value::String("two".to_string())]),
            Value::List(vec![Value::Bool(true)]),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(
            result,
            Value::List(vec![
                Value::Int(1),
                Value::String("two".to_string()),
                Value::Bool(true),
            ])
        );
    }

    #[test]
    fn concat_empty_first() {
        let args = vec![
            Value::List(vec![]),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(result, Value::List(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn concat_empty_second() {
        let args = vec![
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::List(vec![]),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(result, Value::List(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn concat_both_empty() {
        let args = vec![Value::List(vec![]), Value::List(vec![])];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn concat_wrong_arg_count_one() {
        let args = vec![Value::List(vec![Value::Int(1)])];
        let result = evaluate_builtin("concat", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn concat_wrong_arg_count_three() {
        let args = vec![
            Value::List(vec![]),
            Value::List(vec![]),
            Value::List(vec![]),
        ];
        let result = evaluate_builtin("concat", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn concat_first_arg_not_list() {
        let args = vec![
            Value::String("not a list".to_string()),
            Value::List(vec![Value::Int(1)]),
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
            Value::List(vec![Value::Int(1)]),
            Value::String("not a list".to_string()),
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
