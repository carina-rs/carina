//! `keys(map)` and `values(map)` built-in functions

use crate::resource::Value;

use super::value_type_name;

/// `keys(map)` - Return the keys of a map as a sorted list of strings.
///
/// - Single argument: a Map
/// - Returns: List of String (sorted alphabetically)
///
/// Examples:
/// ```text
/// keys({b: 2, a: 1})  // => ["a", "b"]
/// keys({})             // => []
/// ```
pub(crate) fn builtin_keys(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!("keys() expects 1 argument, got {}", args.len()));
    }

    match &args[0] {
        Value::Map(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            Ok(Value::List(keys.into_iter().map(Value::String).collect()))
        }
        other => Err(format!(
            "keys() argument must be a Map, got {}",
            value_type_name(other)
        )),
    }
}

/// `values(map)` - Return the values of a map as a list, ordered by sorted keys.
///
/// - Single argument: a Map
/// - Returns: List of values (ordered by alphabetically sorted keys)
///
/// Examples:
/// ```text
/// values({b: 2, a: 1})  // => [1, 2]
/// values({})             // => []
/// ```
pub(crate) fn builtin_values(args: &[Value]) -> Result<Value, String> {
    if args.len() != 1 {
        return Err(format!("values() expects 1 argument, got {}", args.len()));
    }

    match &args[0] {
        Value::Map(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            Ok(Value::List(
                keys.into_iter().map(|k| map[&k].clone()).collect(),
            ))
        }
        other => Err(format!(
            "values() argument must be a Map, got {}",
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
    fn keys_basic() {
        let args = vec![Value::Map(IndexMap::from([
            ("b".to_string(), Value::Int(2)),
            ("a".to_string(), Value::Int(1)),
            ("c".to_string(), Value::Int(3)),
        ]))];
        let result = evaluate_builtin("keys", &args).unwrap();
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
    fn keys_empty_map() {
        let args = vec![Value::Map(IndexMap::new())];
        let result = evaluate_builtin("keys", &args).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn keys_single_entry() {
        let args = vec![Value::Map(IndexMap::from([(
            "only".to_string(),
            Value::String("value".to_string()),
        )]))];
        let result = evaluate_builtin("keys", &args).unwrap();
        assert_eq!(result, Value::List(vec![Value::String("only".to_string())]));
    }

    #[test]
    fn keys_wrong_arg_count_zero() {
        let result = evaluate_builtin("keys", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn keys_wrong_arg_count_two() {
        let args = vec![Value::Map(IndexMap::new()), Value::Map(IndexMap::new())];
        let result = evaluate_builtin("keys", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn keys_invalid_type() {
        let args = vec![Value::List(vec![Value::Int(1)])];
        let result = evaluate_builtin("keys", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a Map"));
    }

    #[test]
    fn values_basic() {
        let args = vec![Value::Map(IndexMap::from([
            ("b".to_string(), Value::Int(2)),
            ("a".to_string(), Value::Int(1)),
            ("c".to_string(), Value::Int(3)),
        ]))];
        let result = evaluate_builtin("values", &args).unwrap();
        // Values should be ordered by sorted keys: a=1, b=2, c=3
        assert_eq!(
            result,
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        );
    }

    #[test]
    fn values_empty_map() {
        let args = vec![Value::Map(IndexMap::new())];
        let result = evaluate_builtin("values", &args).unwrap();
        assert_eq!(result, Value::List(vec![]));
    }

    #[test]
    fn values_mixed_types() {
        let args = vec![Value::Map(IndexMap::from([
            ("name".to_string(), Value::String("test".to_string())),
            ("count".to_string(), Value::Int(42)),
        ]))];
        let result = evaluate_builtin("values", &args).unwrap();
        // Sorted by key: count=42, name="test"
        assert_eq!(
            result,
            Value::List(vec![Value::Int(42), Value::String("test".to_string()),])
        );
    }

    #[test]
    fn values_wrong_arg_count_zero() {
        let result = evaluate_builtin("values", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn values_wrong_arg_count_two() {
        let args = vec![Value::Map(IndexMap::new()), Value::Map(IndexMap::new())];
        let result = evaluate_builtin("values", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn values_invalid_type() {
        let args = vec![Value::String("not a map".to_string())];
        let result = evaluate_builtin("values", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a Map"));
    }
}
