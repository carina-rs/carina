//! `map(accessor, collection)` built-in function

use indexmap::IndexMap;

use crate::resource::{ConcreteValue, Value};

use super::value_type_name;

/// `map(accessor, collection)` - Extract a field from each element of a collection.
///
/// - First argument: a field accessor string starting with `.` (e.g., `".subnet_id"`)
/// - Second argument: a List of Maps, or a Map of Maps
/// - Returns: List or Map with each element replaced by the extracted field value
///
/// The argument order matches pipe convention: the collection (pipe target) is the
/// last argument. `subnets |> map(".subnet_id")` desugars to `map(".subnet_id", subnets)`.
///
/// Examples:
/// ```text
/// map(".id", [{name: "a", id: "1"}, {name: "b", id: "2"}])  // => ["1", "2"]
/// subnets |> map(".subnet_id")  // pipe syntax
/// ```
pub(crate) fn builtin_map(args: &[Value]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(format!("map() requires 2 arguments, got {}", args.len()));
    }

    let accessor = match &args[0] {
        Value::Concrete(ConcreteValue::String(s)) if s.starts_with('.') => &s[1..],
        _ => {
            return Err(
                "map() first argument must be a field accessor string starting with '.' (e.g., \".field_name\")".to_string(),
            );
        }
    };

    match &args[1] {
        Value::Concrete(ConcreteValue::List(items)) => {
            let mapped: Result<Vec<Value>, String> = items
                .iter()
                .map(|item| match item {
                    Value::Concrete(ConcreteValue::Map(map)) => {
                        map.get(accessor).cloned().ok_or_else(|| {
                            format!("map(): field '{}' not found in map element", accessor)
                        })
                    }
                    other => Err(format!(
                        "map() expects list of maps, got list of {}",
                        value_type_name(other)
                    )),
                })
                .collect();
            Ok(Value::Concrete(ConcreteValue::List(mapped?)))
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mapped: Result<IndexMap<String, Value>, String> = map
                .iter()
                .map(|(k, v)| match v {
                    Value::Concrete(ConcreteValue::Map(inner)) => inner
                        .get(accessor)
                        .cloned()
                        .map(|val| (k.clone(), val))
                        .ok_or_else(|| {
                            format!(
                                "map(): field '{}' not found in map value for key '{}'",
                                accessor, k
                            )
                        }),
                    other => Err(format!(
                        "map() expects map of maps, got map with {} value",
                        value_type_name(other)
                    )),
                })
                .collect();
            Ok(Value::Concrete(ConcreteValue::Map(mapped?)))
        }
        other => Err(format!(
            "map() second argument must be a list or map, got {}",
            value_type_name(other)
        )),
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::{ConcreteValue, Value};

    fn make_map(pairs: Vec<(&str, Value)>) -> Value {
        Value::Concrete(ConcreteValue::Map(
            pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        ))
    }

    #[test]
    fn map_list_of_maps_extracts_field() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(".subnet_id".to_string())),
            Value::Concrete(ConcreteValue::List(vec![
                make_map(vec![
                    (
                        "name",
                        Value::Concrete(ConcreteValue::String("subnet-a".to_string())),
                    ),
                    (
                        "subnet_id",
                        Value::Concrete(ConcreteValue::String("id-1".to_string())),
                    ),
                ]),
                make_map(vec![
                    (
                        "name",
                        Value::Concrete(ConcreteValue::String("subnet-b".to_string())),
                    ),
                    (
                        "subnet_id",
                        Value::Concrete(ConcreteValue::String("id-2".to_string())),
                    ),
                ]),
            ])),
        ];
        let result = evaluate_builtin("map", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("id-1".to_string())),
                Value::Concrete(ConcreteValue::String("id-2".to_string())),
            ]))
        );
    }

    #[test]
    fn map_map_of_maps_extracts_field() {
        let mut outer: IndexMap<String, Value> = IndexMap::new();
        outer.insert(
            "a".to_string(),
            make_map(vec![
                (
                    "name",
                    Value::Concrete(ConcreteValue::String("foo".to_string())),
                ),
                (
                    "id",
                    Value::Concrete(ConcreteValue::String("1".to_string())),
                ),
            ]),
        );
        outer.insert(
            "b".to_string(),
            make_map(vec![
                (
                    "name",
                    Value::Concrete(ConcreteValue::String("bar".to_string())),
                ),
                (
                    "id",
                    Value::Concrete(ConcreteValue::String("2".to_string())),
                ),
            ]),
        );
        let args = vec![
            Value::Concrete(ConcreteValue::String(".id".to_string())),
            Value::Concrete(ConcreteValue::Map(outer)),
        ];
        let result = evaluate_builtin("map", &args).unwrap();
        match result {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("a"),
                    Some(&Value::Concrete(ConcreteValue::String("1".to_string())))
                );
                assert_eq!(
                    m.get("b"),
                    Some(&Value::Concrete(ConcreteValue::String("2".to_string())))
                );
            }
            other => panic!("Expected Map, got {:?}", other),
        }
    }

    #[test]
    fn map_empty_list() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(".field".to_string())),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("map", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::List(vec![])));
    }

    #[test]
    fn map_error_wrong_arg_count_zero() {
        let result = evaluate_builtin("map", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires 2 arguments"));
    }

    #[test]
    fn map_partial_application_one_arg() {
        use crate::builtins::evaluate_builtin_for_tests;
        let args = vec![Value::Concrete(ConcreteValue::String(".field".to_string()))];
        let result = evaluate_builtin_for_tests("map", &args).unwrap();
        assert!(result.is_closure());
    }

    #[test]
    fn map_error_wrong_arg_count_three() {
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::String(".field".to_string())),
            Value::Concrete(ConcreteValue::String("extra".to_string())),
        ];
        let result = evaluate_builtin("map", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires 2 arguments"));
    }

    #[test]
    fn map_error_accessor_without_dot() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("field".to_string())),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("map", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("field accessor"));
    }

    #[test]
    fn map_error_accessor_not_string() {
        let args = vec![
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("map", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("field accessor"));
    }

    #[test]
    fn map_error_non_map_elements_in_list() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(".field".to_string())),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("not a map".to_string()),
            )])),
        ];
        let result = evaluate_builtin("map", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects list of maps"));
    }

    #[test]
    fn map_error_missing_field() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(".missing".to_string())),
            Value::Concrete(ConcreteValue::List(vec![make_map(vec![(
                "name",
                Value::Concrete(ConcreteValue::String("foo".to_string())),
            )])])),
        ];
        let result = evaluate_builtin("map", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("field 'missing' not found"));
    }

    #[test]
    fn map_error_second_arg_not_collection() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(".field".to_string())),
            Value::Concrete(ConcreteValue::String("not a collection".to_string())),
        ];
        let result = evaluate_builtin("map", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a list or map"));
    }
}
