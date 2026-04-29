//! `length(value)` built-in function

use crate::resource::Value;

use super::value_type_name;

/// `length(value)` - Return the length of a list, map, or string.
///
/// - Single argument: a List, Map, or String
/// - Returns: Int (the number of elements or characters)
///
/// Examples:
/// ```text
/// length([1, 2, 3])     // => 3
/// length({a: 1, b: 2})  // => 2
/// length("hello")       // => 5
/// ```
pub(crate) fn builtin_length(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!("length() expects 1 argument, got {}", args.len()));
    }

    match &args[0] {
        Value::List(items) => Ok(Value::Int(items.len() as i64)),
        Value::Map(map) => Ok(Value::Int(map.len() as i64)),
        Value::String(s) => Ok(Value::Int(s.len() as i64)),
        other => Err(format!(
            "length() argument must be a List, Map, or String, got {}",
            value_type_name(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::Value;

    #[test]
    fn length_list() {
        let args = vec![Value::List(vec![
            Value::Int(1),
            Value::Int(2),
            Value::Int(3),
        ])];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn length_empty_list() {
        let args = vec![Value::List(vec![])];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn length_map() {
        let args = vec![Value::Map(IndexMap::from([
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(2)),
        ]))];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn length_empty_map() {
        let args = vec![Value::Map(IndexMap::new())];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn length_string() {
        let args = vec![Value::String("hello".to_string())];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn length_empty_string() {
        let args = vec![Value::String("".to_string())];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn length_wrong_arg_count_zero() {
        let result = evaluate_builtin("length", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn length_wrong_arg_count_two() {
        let args = vec![
            Value::List(vec![Value::Int(1)]),
            Value::List(vec![Value::Int(2)]),
        ];
        let result = evaluate_builtin("length", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn length_invalid_type() {
        let args = vec![Value::Int(42)];
        let result = evaluate_builtin("length", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("must be a List, Map, or String")
        );
    }
}
