//! Flatten one known level; an element whose shape is pending retains the call.
use super::value_type_name;
use crate::resource::knowledge::{Classification, KnownView, PlanIterable, ShapeView};
use crate::resource::{BindingAddress, ConcreteValue, DeferredValue, NoPlaceholder, Value};
pub fn builtin_flatten<B: BindingAddress>(
    collection: &PlanIterable<'_, NoPlaceholder, B>,
    source_line: crate::resource::DiagnosticSourceLine,
) -> Result<Value<NoPlaceholder, B>, crate::resolver::ResolutionError> {
    let flatten = |items: &[Classification<'_, NoPlaceholder, B>]| {
        let mut result = Vec::new();
        for item in items {
            match item.flatten_item()? {
                Some(item) => item.append_to(&mut result),
                None => return Ok(retained(collection, source_line)),
            }
        }
        Ok(Value::Concrete(ConcreteValue::List(result)))
    };
    collection.visit(
        |view| match view {
            KnownView::List(items) => flatten(
                &items
                    .iter()
                    .map(|value| Classification::Known(value.clone()))
                    .collect::<Vec<_>>(),
            ),
            _ => Err(format!(
                "flatten() argument must be a List, got {}",
                value_type_name(&collection.source_value())
            )
            .into()),
        },
        |view| match view {
            ShapeView::List(items) => flatten(items),
            ShapeView::Map(_) => Err("flatten() argument must be a List, got Map".into()),
        },
        |_| Err("flatten() argument must be a List, got Secret".into()),
    )
}
fn retained<B: BindingAddress>(
    collection: &PlanIterable<'_, NoPlaceholder, B>,
    source_line: crate::resource::DiagnosticSourceLine,
) -> Value<NoPlaceholder, B> {
    Value::Deferred(DeferredValue::FunctionCall {
        name: super::BuiltinFunction::flatten,
        args: vec![collection.source_value().into()],
        source_line,
    })
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value_for_tests as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

    #[test]
    fn flatten_retains_a_pending_secret_item() {
        use crate::resource::knowledge::{Classification, KnowledgeContext, PlanIterable};
        let source = Value::Concrete(ConcreteValue::List(vec![
            Value::root_resource_ref("producer", "identifier", vec![]).into_secret(),
        ]));
        let Classification::ShapeKnown(shape) = source
            .knowledge(&KnowledgeContext::validation())
            .unwrap()
            .into_classification()
        else {
            panic!("outer membership is known");
        };
        let iterable = PlanIterable::from_shape(shape).unwrap();
        let result = super::builtin_flatten(
            &iterable,
            crate::resource::DiagnosticSourceLine::synthetic(),
        )
        .unwrap();
        assert!(
            matches!(
                result,
                Value::Deferred(crate::resource::DeferredValue::FunctionCall {
                    name: crate::builtins::BuiltinFunction::flatten,
                    ..
                })
            ),
            "pending protected inner shape must retain flatten"
        );
    }

    #[test]
    fn retained_flatten_keeps_the_authored_line() {
        use crate::eval_value::EvalValue;
        use crate::parser::ProviderContext;
        let argument = Value::Concrete(ConcreteValue::List(vec![Value::root_resource_ref(
            "producer",
            "identifier",
            vec![],
        )]));
        let result = super::super::evaluate_builtin_with_config(
            "flatten",
            &[EvalValue::from_expanded_value(argument)],
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            crate::resource::DiagnosticSourceLine::authored(42),
        )
        .unwrap()
        .into_expanded_value()
        .unwrap();
        assert!(
            matches!(result, Value::Deferred(crate::resource::DeferredValue::FunctionCall {
            source_line, ..
        }) if source_line.get() == Some(42)),
            "retention must preserve the call's authored line"
        );
    }

    #[test]
    fn flatten_nested_lists() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(3)),
                Value::Concrete(ConcreteValue::Int(4)),
            ])),
        ]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
                Value::Concrete(ConcreteValue::Int(3)),
                Value::Concrete(ConcreteValue::Int(4)),
            ]))
        );
    }

    #[test]
    fn flatten_string_lists() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".into())),
                Value::Concrete(ConcreteValue::String("b".into())),
            ])),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("c".into()),
            )])),
        ]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
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
    fn flatten_mixed_list_and_non_list() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
            Value::Concrete(ConcreteValue::Int(3)),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(4),
            )])),
        ]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
                Value::Concrete(ConcreteValue::Int(3)),
                Value::Concrete(ConcreteValue::Int(4)),
            ]))
        );
    }

    #[test]
    fn flatten_empty_list() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::List(vec![])));
    }

    #[test]
    fn flatten_no_nested_lists() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::Int(2)),
            Value::Concrete(ConcreteValue::Int(3)),
        ]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
                Value::Concrete(ConcreteValue::Int(3)),
            ]))
        );
    }

    #[test]
    fn flatten_single_level_only() {
        // Nested [[1, [2, 3]]] should flatten to [1, [2, 3]], not [1, 2, 3]
        let args = vec![Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::Int(2)),
                    Value::Concrete(ConcreteValue::Int(3)),
                ])),
            ]),
        )]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::Int(2)),
                    Value::Concrete(ConcreteValue::Int(3))
                ])),
            ]))
        );
    }

    #[test]
    fn flatten_empty_inner_lists() {
        let args = vec![Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(1),
            )])),
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(2),
            )])),
        ]))];
        let result = evaluate_builtin("flatten", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ]))
        );
    }

    #[test]
    fn flatten_wrong_arg_count_zero() {
        let result: Result<Value, String> = evaluate_builtin("flatten", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn flatten_wrong_arg_count_two() {
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(1),
            )])),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(2),
            )])),
        ];
        let result = evaluate_builtin("flatten", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 1 argument"));
    }

    #[test]
    fn flatten_invalid_type() {
        let args = vec![Value::Concrete(ConcreteValue::String("not a list".into()))];
        let result = evaluate_builtin("flatten", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be a List"));
    }
}
