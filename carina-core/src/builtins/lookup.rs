//! `lookup(map, key, default)` built-in function

use crate::resource::{BindingAddress, ConcreteValue, Value};

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
pub(crate) fn builtin_lookup<B: BindingAddress>(args: &[Value<B>]) -> Result<Value<B>, String> {
    if args.len() != 3 {
        return Err(format!(
            "lookup() expects 3 arguments (map, key, default), got {}",
            args.len()
        ));
    }

    let map = match &args[0] {
        Value::Concrete(ConcreteValue::Map(m)) => m,
        other => {
            return Err(format!(
                "lookup() first argument must be a map, got {}",
                value_type_name(other)
            ));
        }
    };

    let key = match &args[1] {
        Value::Concrete(ConcreteValue::String(s)) => s,
        other => {
            return Err(format!(
                "lookup() second argument must be a string, got {}",
                value_type_name(other)
            ));
        }
    };

    Ok(map
        .get(key.expose_plaintext())
        .cloned()
        .unwrap_or_else(|| args[2].clone()))
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

    #[test]
    fn lookup_key_found() {
        let map = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "a".to_string(),
                Value::Concrete(ConcreteValue::String("one".into())),
            ),
            (
                "b".to_string(),
                Value::Concrete(ConcreteValue::String("two".into())),
            ),
        ])));
        let args = vec![
            map,
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::String("default".into())),
        ];
        let result = evaluate_builtin("lookup", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::String("one".into())));
    }

    #[test]
    fn lookup_key_not_found() {
        let map = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "a".to_string(),
                Value::Concrete(ConcreteValue::String("one".into())),
            ),
            (
                "b".to_string(),
                Value::Concrete(ConcreteValue::String("two".into())),
            ),
        ])));
        let args = vec![
            map,
            Value::Concrete(ConcreteValue::String("c".into())),
            Value::Concrete(ConcreteValue::String("default".into())),
        ];
        let result = evaluate_builtin("lookup", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("default".into()))
        );
    }

    #[test]
    fn lookup_empty_map() {
        let map = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
        let args = vec![
            map,
            Value::Concrete(ConcreteValue::String("key".into())),
            Value::Concrete(ConcreteValue::String("fallback".into())),
        ];
        let result = evaluate_builtin("lookup", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::String("fallback".into()))
        );
    }

    #[test]
    fn lookup_int_default() {
        let map = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "x".to_string(),
            Value::Concrete(ConcreteValue::Int(42)),
        )])));
        let args = vec![
            map,
            Value::Concrete(ConcreteValue::String("missing".into())),
            Value::Concrete(ConcreteValue::Int(0)),
        ];
        let result = evaluate_builtin("lookup", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(0)));
    }

    #[test]
    fn lookup_returns_non_string_value() {
        let map = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "count".to_string(),
            Value::Concrete(ConcreteValue::Int(99)),
        )])));
        let args = vec![
            map,
            Value::Concrete(ConcreteValue::String("count".into())),
            Value::Concrete(ConcreteValue::Int(0)),
        ];
        let result = evaluate_builtin("lookup", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(99)));
    }

    #[test]
    fn lookup_partial_application() {
        use crate::builtins::evaluate_builtin_for_tests;
        let args = vec![
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            Value::Concrete(ConcreteValue::String("key".into())),
        ];
        let result = evaluate_builtin_for_tests("lookup", &args).unwrap();
        assert!(result.is_closure());
    }

    #[test]
    fn lookup_non_map_first_arg() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("not a map".into())),
            Value::Concrete(ConcreteValue::String("key".into())),
            Value::Concrete(ConcreteValue::String("default".into())),
        ];
        let result = evaluate_builtin("lookup", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("first argument must be a map"));
    }

    #[test]
    fn lookup_non_string_key() {
        let args = vec![
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::String("default".into())),
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
