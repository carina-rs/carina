//! `lookup(map, key, default)` built-in function

use crate::resource::Value;

use super::value_type_name;

/// `lookup(map, key, default)` - Look up a key in a map, returning a default if not found.
///
/// - First argument: map (Map)
/// - Second argument: key (String)
/// - Third argument: default value (any Value)
/// - Returns: the value at key if it exists, otherwise the default
///
/// Examples:
/// ```text
/// lookup({a: "one", b: "two"}, "a", "default")  // => "one"
/// lookup({a: "one", b: "two"}, "c", "default")  // => "default"
/// ```
pub(crate) fn builtin_lookup(args: &[Value]) -> Result<Value, String> {
    if args.len() != 3 {
        return Err(format!(
            "lookup() expects 3 arguments (map, key, default), got {}",
            args.len()
        ));
    }

    let map = match &args[0] {
        Value::Map(m) => m,
        other => {
            return Err(format!(
                "lookup() first argument must be a map, got {}",
                value_type_name(other)
            ));
        }
    };

    let key = match &args[1] {
        Value::String(s) => s,
        other => {
            return Err(format!(
                "lookup() second argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    Ok(map.get(key).cloned().unwrap_or_else(|| args[2].clone()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::builtins::evaluate_builtin;
    use crate::resource::Value;

    #[test]
    fn lookup_key_found() {
        let map = Value::Map(HashMap::from([
            ("a".to_string(), Value::String("one".to_string())),
            ("b".to_string(), Value::String("two".to_string())),
        ]));
        let args = vec![
            map,
            Value::String("a".to_string()),
            Value::String("default".to_string()),
        ];
        let result = evaluate_builtin("lookup", &args).unwrap();
        assert_eq!(result, Value::String("one".to_string()));
    }

    #[test]
    fn lookup_key_not_found() {
        let map = Value::Map(HashMap::from([
            ("a".to_string(), Value::String("one".to_string())),
            ("b".to_string(), Value::String("two".to_string())),
        ]));
        let args = vec![
            map,
            Value::String("c".to_string()),
            Value::String("default".to_string()),
        ];
        let result = evaluate_builtin("lookup", &args).unwrap();
        assert_eq!(result, Value::String("default".to_string()));
    }

    #[test]
    fn lookup_empty_map() {
        let map = Value::Map(HashMap::new());
        let args = vec![
            map,
            Value::String("key".to_string()),
            Value::String("fallback".to_string()),
        ];
        let result = evaluate_builtin("lookup", &args).unwrap();
        assert_eq!(result, Value::String("fallback".to_string()));
    }

    #[test]
    fn lookup_int_default() {
        let map = Value::Map(HashMap::from([("x".to_string(), Value::Int(42))]));
        let args = vec![map, Value::String("missing".to_string()), Value::Int(0)];
        let result = evaluate_builtin("lookup", &args).unwrap();
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn lookup_returns_non_string_value() {
        let map = Value::Map(HashMap::from([("count".to_string(), Value::Int(99))]));
        let args = vec![map, Value::String("count".to_string()), Value::Int(0)];
        let result = evaluate_builtin("lookup", &args).unwrap();
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn lookup_wrong_arg_count() {
        let args = vec![Value::Map(HashMap::new()), Value::String("key".to_string())];
        let result = evaluate_builtin("lookup", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 3 arguments"));
    }

    #[test]
    fn lookup_non_map_first_arg() {
        let args = vec![
            Value::String("not a map".to_string()),
            Value::String("key".to_string()),
            Value::String("default".to_string()),
        ];
        let result = evaluate_builtin("lookup", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("first argument must be a map"));
    }

    #[test]
    fn lookup_non_string_key() {
        let args = vec![
            Value::Map(HashMap::new()),
            Value::Int(1),
            Value::String("default".to_string()),
        ];
        let result = evaluate_builtin("lookup", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("second argument must be a string")
        );
    }
}
