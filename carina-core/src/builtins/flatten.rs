//! `flatten(list)` built-in function

use crate::resource::Value;

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
pub(crate) fn builtin_flatten(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!("flatten() expects 1 argument, got {}", args.len()));
    }

    let items = match &args[0] {
        Value::List(items) => items,
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
            Value::List(inner) => result.extend(inner.iter().cloned()),
            other => result.push(other.clone()),
        }
    }

    Ok(Value::List(result))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin;
    use crate::resource::Value;

    #[test]
    fn flatten_nested_lists() {
        let args = vec![Value::List(vec![
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::List(vec![Value::Int(3), Value::Int(4)]),
        ])];
        let result = evaluate_builtin("flatten", &args).unwrap();
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
    fn flatten_string_lists() {
        let args = vec![Value::List(vec![
            Value::List(vec![
                Value::String("a".to_string()),
                Value::String("b".to_string()),
            ]),
            Value::List(vec![Value::String("c".to_string())]),
        ])];
        let result = evaluate_builtin("flatten", &args).unwrap();
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
    fn flatten_mixed_list_and_non_list() {
        let args = vec![Value::List(vec![
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::Int(3),
            Value::List(vec![Value::Int(4)]),
        ])];
        let result = evaluate_builtin("flatten", &args).unwrap();
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
    fn flatten_empty_list() {
        let args = vec![Value::List(vec![])];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn flatten_no_nested_lists() {
        let args = vec![Value::List(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
        ])];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3),])
        );
    }

    #[test]
    fn flatten_single_level_only() {
        // Nested [[1, [2, 3]]] should flatten to [1, [2, 3]], not [1, 2, 3]
        let args = vec![Value::List(vec![Value::List(vec![
            Value::Int(1),
            Value::List(vec![Value::Int(2), Value::Int(3)]),
        ])])];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::List(vec![
                Value::Int(1),
                Value::List(vec![Value::Int(2), Value::Int(3)]),
            ])
        );
    }

    #[test]
    fn flatten_empty_inner_lists() {
        let args = vec![Value::List(vec![
            Value::List(vec![Value::Int(1)]),
            Value::List(vec![]),
            Value::List(vec![Value::Int(2)]),
        ])];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(result, Value::List(vec![Value::Int(1), Value::Int(2),]));
    }

    #[test]
    fn flatten_wrong_arg_count_zero() {
        let result = evaluate_builtin("flatten", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn flatten_wrong_arg_count_two() {
        let args = vec![
            Value::List(vec![Value::Int(1)]),
            Value::List(vec![Value::Int(2)]),
        ];
        let result = evaluate_builtin("flatten", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn flatten_invalid_type() {
        let args = vec![Value::String("not a list".to_string())];
        let result = evaluate_builtin("flatten", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a List"));
    }
}
