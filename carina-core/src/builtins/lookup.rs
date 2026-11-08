//! Select a key with exact map membership; the selected leaf may remain pending.
use super::value_type_name;
use crate::resource::knowledge::{Classification, KnownValue, PlanIterable};
use crate::resource::{BindingAddress, ConcreteValue, NoPlaceholder, Value};
pub fn builtin_lookup<B: BindingAddress>(
    map: &PlanIterable<'_, NoPlaceholder, B>,
    key: &KnownValue<'_, NoPlaceholder, B>,
    default: &Classification<'_, NoPlaceholder, B>,
) -> Result<Value<NoPlaceholder, B>, String> {
    let map = map.for_expansion();
    let Value::Concrete(ConcreteValue::Map(map)) = map else {
        return Err(format!(
            "lookup() first argument must be a map, got {}",
            value_type_name(&map)
        ));
    };
    let key = key.to_value();
    let Value::Concrete(ConcreteValue::String(key)) = key else {
        return Err(format!(
            "lookup() second argument must be a string, got {}",
            value_type_name(&key)
        ));
    };
    Ok(map
        .get(key.expose_plaintext())
        .cloned()
        .unwrap_or_else(|| default.to_value()))
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use crate::builtins::evaluate_builtin_to_value_for_tests as evaluate_builtin;
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
