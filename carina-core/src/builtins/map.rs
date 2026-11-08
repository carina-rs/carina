//! Field projection preserves collection membership and reference selectors.
use super::value_type_name;
use crate::resolver::ResolutionError;
use crate::resource::knowledge::{Classification, KnownValue, KnownView, PlanIterable, ShapeView};
use crate::resource::{
    AccessMissPolicy, BindingAddress, ConcreteValue, NoPlaceholder, PathSegment, Value,
    ValuePathSegmentResult, ValueString,
};
use indexmap::IndexMap;
pub fn builtin_map<B: BindingAddress>(
    accessor: &KnownValue<'_, NoPlaceholder, B>,
    collection: &PlanIterable<'_, NoPlaceholder, B>,
) -> Result<Value<NoPlaceholder, B>, ResolutionError> {
    let accessor = accessor.to_value();
    let Value::Concrete(ConcreteValue::String(accessor)) = accessor else {
        return Err("map() first argument must be a field accessor string starting with '.' (e.g., \".field_name\")".into());
    };
    if !accessor.expose_plaintext().starts_with('.') {
        return Err("map() first argument must be a field accessor string starting with '.' (e.g., \".field_name\")".into());
    }
    collection.visit(
        |view| match view {
            KnownView::List(items) => items
                .iter()
                .map(|item| {
                    extract(
                        &Classification::Known(item.clone()),
                        &accessor,
                        "in map element",
                        "list of",
                    )
                })
                .collect::<Result<Vec<_>, _>>()
                .map(|items| Value::Concrete(ConcreteValue::List(items))),
            KnownView::Map(items) => items
                .iter()
                .map(|(key, item)| {
                    extract(
                        &Classification::Known(item.clone()),
                        &accessor,
                        "in map value",
                        "map with",
                    )
                    .map(|value| (key.to_owned(), value))
                })
                .collect::<Result<IndexMap<_, _>, _>>()
                .map(|items| Value::Concrete(ConcreteValue::Map(items))),
            _ => Err(format!(
                "map() second argument must be a list or map, got {}",
                value_type_name(&collection.source_value())
            )
            .into()),
        },
        |view| match view {
            ShapeView::List(items) => items
                .iter()
                .map(|item| extract(item, &accessor, "in map element", "list of"))
                .collect::<Result<Vec<_>, _>>()
                .map(|items| Value::Concrete(ConcreteValue::List(items))),
            ShapeView::Map(items) => items
                .iter()
                .map(|(key, item)| {
                    extract(item, &accessor, "in map value", "map with")
                        .map(|value| ((*key).to_owned(), value))
                })
                .collect::<Result<IndexMap<_, _>, _>>()
                .map(|items| Value::Concrete(ConcreteValue::Map(items))),
        },
        |_| Err("map() second argument must be a list or map, got Secret".into()),
    )
}
fn extract<B: BindingAddress>(
    value: &Classification<'_, NoPlaceholder, B>,
    accessor: &ValueString,
    location: &str,
    kind: &str,
) -> Result<Value<NoPlaceholder, B>, ResolutionError> {
    let field = &accessor.expose_plaintext()[1..];
    let wrong = || {
        format!(
            "map() expects {} maps, got {kind} {}{}",
            if kind == "map with" { "map of" } else { kind },
            value_type_name(&value.to_value()),
            if kind == "map with" { " value" } else { "" }
        )
    };
    match value {
        Classification::Known(known) => known.visit(
            |view| match view {
                KnownView::Map(map) => map
                    .iter()
                    .find(|(key, _)| *key == field)
                    .map(|(_, value)| value.to_value())
                    .ok_or_else(|| ResolutionError::from(missing_field(accessor, location))),
                _ => Err(wrong().into()),
            },
            |_| Err(wrong().into()),
        ),
        Classification::ShapeKnown(shape) => shape.visit(
            |view| match view {
                ShapeView::Map(map) => map
                    .get(field)
                    .map(Classification::to_value)
                    .ok_or_else(|| ResolutionError::from(missing_field(accessor, location))),
                ShapeView::List(_) => Err(wrong().into()),
            },
            |_| Err(wrong().into()),
        ),
        Classification::Failed(value) => Err(value.error().into()),
        Classification::Pending(_) => crate::resource::apply_value_path_segment(
            value.to_value(),
            PathSegment::Field {
                name: field.to_owned(),
            },
            0,
            AccessMissPolicy::Error,
        )
        .map_err(ResolutionError::from)
        .and_then(|value| match value {
            ValuePathSegmentResult::Resolved(value) | ValuePathSegmentResult::Deferred(value) => {
                Ok(value)
            }
            ValuePathSegmentResult::DeferredMiss(error) => Err(error.into()),
        }),
    }
}
fn missing_field(accessor: &ValueString, location: &str) -> String {
    match accessor.diagnostic_literal() {
        Some(accessor) => format!("map(): field '{}' not found {location}", &accessor[1..]),
        None => format!("map(): field named by a sensitive value not found {location}"),
    }
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use crate::builtins::evaluate_builtin_to_value_for_tests as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

    fn make_map(pairs: Vec<(&str, Value)>) -> Value {
        Value::Concrete(ConcreteValue::Map(
            pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        ))
    }

    #[test]
    fn map_list_of_maps_extracts_field() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(".subnet_id".into())),
            Value::Concrete(ConcreteValue::List(vec![
                make_map(vec![
                    (
                        "name",
                        Value::Concrete(ConcreteValue::String("subnet-a".into())),
                    ),
                    (
                        "subnet_id",
                        Value::Concrete(ConcreteValue::String("id-1".into())),
                    ),
                ]),
                make_map(vec![
                    (
                        "name",
                        Value::Concrete(ConcreteValue::String("subnet-b".into())),
                    ),
                    (
                        "subnet_id",
                        Value::Concrete(ConcreteValue::String("id-2".into())),
                    ),
                ]),
            ])),
        ];
        let result = evaluate_builtin("map", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("id-1".into())),
                Value::Concrete(ConcreteValue::String("id-2".into())),
            ]))
        );
    }

    #[test]
    fn map_map_of_maps_extracts_field() {
        let mut outer: IndexMap<String, Value> = IndexMap::new();
        outer.insert(
            "a".to_string(),
            make_map(vec![
                ("name", Value::Concrete(ConcreteValue::String("foo".into()))),
                ("id", Value::Concrete(ConcreteValue::String("1".into()))),
            ]),
        );
        outer.insert(
            "b".to_string(),
            make_map(vec![
                ("name", Value::Concrete(ConcreteValue::String("bar".into()))),
                ("id", Value::Concrete(ConcreteValue::String("2".into()))),
            ]),
        );
        let args = vec![
            Value::Concrete(ConcreteValue::String(".id".into())),
            Value::Concrete(ConcreteValue::Map(outer)),
        ];
        let result = evaluate_builtin("map", &args).unwrap();
        match result {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("a"),
                    Some(&Value::Concrete(ConcreteValue::String("1".into())))
                );
                assert_eq!(
                    m.get("b"),
                    Some(&Value::Concrete(ConcreteValue::String("2".into())))
                );
            }
            other => panic!("Expected Map, got {:?}", other),
        }
    }

    #[test]
    fn map_empty_list() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(".field".into())),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("map", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::List(vec![])));
    }

    #[test]
    fn map_error_wrong_arg_count_zero() {
        let result: Result<Value, String> = evaluate_builtin("map", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires 2 arguments"));
    }

    #[test]
    fn map_partial_application_one_arg() {
        use crate::builtins::evaluate_builtin_for_tests;
        let args = vec![Value::Concrete(ConcreteValue::String(".field".into()))];
        let result = evaluate_builtin_for_tests("map", &args).unwrap();
        assert!(result.is_closure());
    }

    #[test]
    fn map_error_wrong_arg_count_three() {
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::String(".field".into())),
            Value::Concrete(ConcreteValue::String("extra".into())),
        ];
        let result = evaluate_builtin("map", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires 2 arguments"));
    }

    #[test]
    fn map_error_accessor_without_dot() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("field".into())),
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
            Value::Concrete(ConcreteValue::String(".field".into())),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("not a map".into()),
            )])),
        ];
        let result = evaluate_builtin("map", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects list of maps"));
    }

    #[test]
    fn map_error_missing_field() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(".missing".into())),
            Value::Concrete(ConcreteValue::List(vec![make_map(vec![(
                "name",
                Value::Concrete(ConcreteValue::String("foo".into())),
            )])])),
        ];
        let result = evaluate_builtin("map", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("field 'missing' not found"));
    }

    #[test]
    fn map_error_second_arg_not_collection() {
        let args = vec![
            Value::Concrete(ConcreteValue::String(".field".into())),
            Value::Concrete(ConcreteValue::String("not a collection".into())),
        ];
        let result = evaluate_builtin("map", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a list or map"));
    }
}
