//! Collection length uses shape without requiring its leaves.
use super::value_type_name;
use crate::resolver::ResolutionError;
use crate::resource::knowledge::{Classification, KnownView};
use crate::resource::{BindingAddress, ConcreteValue, NoPlaceholder, Value};
pub fn builtin_length<B: BindingAddress>(
    value: &Classification<'_, NoPlaceholder, B>,
) -> Result<Value<NoPlaceholder, B>, ResolutionError> {
    let length = match value {
        Classification::Known(known) => known.visit(
            |view| -> Result<usize, ResolutionError> {
                match view {
                    KnownView::List(v) => Ok(v.len()),
                    KnownView::Map(v) => Ok(v.len()),
                    KnownView::String(v) => Ok(v.expose_plaintext().len()),
                    _ => Err(format!(
                        "length() argument must be a List, Map, or String, got {}",
                        value_type_name(&known.to_value())
                    )
                    .into()),
                }
            },
            |_| Err("length() argument must be a List, Map, or String, got Secret".into()),
        ),
        Classification::ShapeKnown(shape) => Ok(shape.len()),
        Classification::Failed(value) => Err(value.error().into()),
        Classification::Pending(_) => {
            return Err(format!(
                "length() argument must be a List, Map, or String, got {}",
                value_type_name(&value.to_value())
            )
            .into());
        }
    }?;
    Ok(Value::Concrete(ConcreteValue::Int(length as i64)))
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap;

    use crate::builtins::evaluate_builtin_to_value_for_tests as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

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
        let args = vec![Value::Concrete(ConcreteValue::String("hello".into()))];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(5)));
    }

    #[test]
    fn length_empty_string() {
        let args = vec![Value::Concrete(ConcreteValue::String("".into()))];
        let result = evaluate_builtin("length", &args).unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(0)));
    }

    #[test]
    fn length_wrong_arg_count_zero() {
        let result: Result<Value, String> = evaluate_builtin("length", &[]);
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
