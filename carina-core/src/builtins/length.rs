//! `length(value)` built-in function

use crate::resource::{ConcreteValue, Value};

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
        Value::Concrete(ConcreteValue::List(items)) => {
            Ok(Value::Concrete(ConcreteValue::Int(items.len() as i64)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            Ok(Value::Concrete(ConcreteValue::Int(map.len() as i64)))
        }
        Value::Concrete(ConcreteValue::String(s)) => {
            Ok(Value::Concrete(ConcreteValue::Int(s.len() as i64)))
        }
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
    use crate::resource::{ConcreteValue, Value};

    #[test]
    fn length_list() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::Int(2)),
            Value::Concrete(ConcreteValue::Int(3)),
        ]))];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(3)));
    }

    #[test]
    fn length_empty_list() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![]))];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(0)));
    }

    #[test]
    fn length_map() {
        let args = vec![Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            ("b".to_string(), Value::Concrete(ConcreteValue::Int(2))),
        ])))];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(2)));
    }

    #[test]
    fn length_empty_map() {
        let args = vec![Value::Concrete(ConcreteValue::Map(IndexMap::new()))];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(0)));
    }

    #[test]
    fn length_string() {
        let args = vec![Value::Concrete(ConcreteValue::String("hello".to_string()))];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(5)));
    }

    #[test]
    fn length_empty_string() {
        let args = vec![Value::Concrete(ConcreteValue::String("".to_string()))];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(0)));
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
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(1),
            )])),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(2),
            )])),
        ];
        let result = evaluate_builtin("length", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn length_invalid_type() {
        let args = vec![Value::Concrete(ConcreteValue::Int(42))];
        let result = evaluate_builtin("length", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("must be a List, Map, or String")
        );
    }
}
