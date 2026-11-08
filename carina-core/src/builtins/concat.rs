//! Concatenate exact list shapes, preserving pending leaves and data-last order.
use super::value_type_name;
use crate::resource::knowledge::PlanIterable;
use crate::resource::{BindingAddress, ConcreteValue, NoPlaceholder, Value};
pub fn builtin_concat<B: BindingAddress>(
    items: &PlanIterable<'_, NoPlaceholder, B>,
    base: &PlanIterable<'_, NoPlaceholder, B>,
) -> Result<Value<NoPlaceholder, B>, String> {
    let items = items.for_expansion();
    let Value::Concrete(ConcreteValue::List(items)) = items else {
        return Err(format!(
            "concat() first argument must be a list, got {}",
            value_type_name(&items)
        ));
    };
    let base = base.for_expansion();
    let Value::Concrete(ConcreteValue::List(mut base)) = base else {
        return Err(format!(
            "concat() second argument must be a list, got {}",
            value_type_name(&base)
        ));
    };
    base.extend(items);
    Ok(Value::Concrete(ConcreteValue::List(base)))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value_for_tests as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

    #[test]
    fn concat_basic() {
        // concat(items, base_list) => base_list ++ items
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(3)),
                Value::Concrete(ConcreteValue::Int(4)),
            ])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
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
    fn concat_strings() {
        // concat(items=["c"], base=["a", "b"]) => ["a", "b", "c"]
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("c".into()),
            )])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("a".into())),
                Value::Concrete(ConcreteValue::String("b".into())),
            ])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
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
    fn concat_mixed_types() {
        // concat(items=[true], base=[1, "two"]) => [1, "two", true]
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Bool(true),
            )])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::String("two".into())),
            ])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::String("two".into())),
                Value::Concrete(ConcreteValue::Bool(true)),
            ]))
        );
    }

    #[test]
    fn concat_empty_first() {
        // concat(items=[], base=[1, 2]) => [1, 2]
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2))
            ]))
        );
    }

    #[test]
    fn concat_empty_second() {
        // concat(items=[1, 2], base=[]) => [1, 2]
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
            ])),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2))
            ]))
        );
    }

    #[test]
    fn concat_both_empty() {
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("concat", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::List(vec![])));
    }

    #[test]
    fn concat_partial_application_one_arg() {
        use crate::builtins::evaluate_builtin_for_tests;
        let args = vec![Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Int(1),
        )]))];
        let result = evaluate_builtin_for_tests("concat", &args).unwrap();
        assert!(result.is_closure());
    }

    #[test]
    fn concat_wrong_arg_count_three() {
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::List(vec![])),
            Value::Concrete(ConcreteValue::List(vec![])),
        ];
        let result = evaluate_builtin("concat", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn concat_first_arg_not_list() {
        let args = vec![
            Value::Concrete(ConcreteValue::String("not a list".into())),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(1),
            )])),
        ];
        let result = evaluate_builtin("concat", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("first argument must be a list")
        );
    }

    #[test]
    fn concat_second_arg_not_list() {
        let args = vec![
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Int(1),
            )])),
            Value::Concrete(ConcreteValue::String("not a list".into())),
        ];
        let result = evaluate_builtin("concat", &args);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("second argument must be a list")
        );
    }
}
