//! Map membership is sufficient for keys and values.
use super::value_type_name;
use crate::resource::knowledge::PlanIterable;
use crate::resource::{BindingAddress, ConcreteValue, NoPlaceholder, Value};
pub fn builtin_keys<B: BindingAddress>(
    value: &PlanIterable<'_, NoPlaceholder, B>,
) -> Result<Value<NoPlaceholder, B>, String> {
    let data = value.for_expansion();
    let Value::Concrete(ConcreteValue::Map(map)) = data else {
        return Err(format!(
            "keys() argument must be a Map, got {}",
            value_type_name(&data)
        ));
    };
    let mut keys = map.into_keys().collect::<Vec<_>>();
    keys.sort();
    Ok(Value::Concrete(ConcreteValue::List(
        keys.into_iter()
            .map(|key| Value::Concrete(ConcreteValue::String(key.into())))
            .collect(),
    )))
}
pub fn builtin_values<B: BindingAddress>(
    value: &PlanIterable<'_, NoPlaceholder, B>,
) -> Result<Value<NoPlaceholder, B>, String> {
    let data = value.for_expansion();
    let Value::Concrete(ConcreteValue::Map(mut map)) = data else {
        return Err(format!(
            "values() argument must be a Map, got {}",
            value_type_name(&data)
        ));
    };
    let mut keys = map.keys().cloned().collect::<Vec<_>>();
    keys.sort();
    Ok(Value::Concrete(ConcreteValue::List(
        keys.into_iter()
            .map(|key| map.swap_remove(&key).expect("proven map member"))
            .collect(),
    )))
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use crate::builtins::evaluate_builtin_to_value_for_tests as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

    #[test]
    fn keys_basic() {
        let args = vec![Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("b".to_string(), Value::Concrete(ConcreteValue::Int(2))),
            ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            ("c".to_string(), Value::Concrete(ConcreteValue::Int(3))),
        ])))];
        let result = evaluate_builtin("keys", &args).unwrap();
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
    fn keys_empty_map() {
        let args = vec![Value::Concrete(ConcreteValue::Map(IndexMap::new()))];
        let result = evaluate_builtin("keys", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::List(vec![])));
    }

    #[test]
    fn keys_single_entry() {
        let args = vec![Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "only".to_string(),
            Value::Concrete(ConcreteValue::String("value".into())),
        )])))];
        let result = evaluate_builtin("keys", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("only".into())
            )]))
        );
    }

    #[test]
    fn keys_wrong_arg_count_zero() {
        let result: Result<Value, String> = evaluate_builtin("keys", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn keys_wrong_arg_count_two() {
        let args = vec![
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        ];
        let result = evaluate_builtin("keys", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn keys_invalid_type() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Int(1),
        )]))];
        let result = evaluate_builtin("keys", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a Map"));
    }

    #[test]
    fn values_basic() {
        let args = vec![Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("b".to_string(), Value::Concrete(ConcreteValue::Int(2))),
            ("a".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            ("c".to_string(), Value::Concrete(ConcreteValue::Int(3))),
        ])))];
        let result = evaluate_builtin("values", &args).unwrap();
        // Values should be ordered by sorted keys: a=1, b=2, c=3
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
                Value::Concrete(ConcreteValue::Int(3))
            ]))
        );
    }

    #[test]
    fn values_empty_map() {
        let args = vec![Value::Concrete(ConcreteValue::Map(IndexMap::new()))];
        let result = evaluate_builtin("values", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::List(vec![])));
    }

    #[test]
    fn values_mixed_types() {
        let args = vec![Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("test".into())),
            ),
            ("count".to_string(), Value::Concrete(ConcreteValue::Int(42))),
        ])))];
        let result = evaluate_builtin("values", &args).unwrap();
        // Sorted by key: count=42, name="test"
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(42)),
                Value::Concrete(ConcreteValue::String("test".into())),
            ]))
        );
    }

    #[test]
    fn values_wrong_arg_count_zero() {
        let result: Result<Value, String> = evaluate_builtin("values", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn values_wrong_arg_count_two() {
        let args = vec![
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        ];
        let result = evaluate_builtin("values", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn values_invalid_type() {
        let args = vec![Value::Concrete(ConcreteValue::String("not a map".into()))];
        let result = evaluate_builtin("values", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a Map"));
    }
}
