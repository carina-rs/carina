//! `min(a, b)` and `max(a, b)` built-in functions

use crate::resource::{ConcreteValue, Value};

use super::value_type_name;

/// `min(a, b)` - Return the smaller of two numeric values.
///
/// - Two arguments: both must be Int or Float
/// - If both are Int, returns Int
/// - If either is Float, converts to Float for comparison and returns Float
///
/// Examples:
/// ```text
/// min(3, 5)      // => 3
/// min(2.5, 1.0)  // => 1.0
/// min(1, 2.5)    // => 1.0
/// ```
pub(crate) fn builtin_min(args: &[Value]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(format!("min() expects 2 arguments, got {}", args.len()));
    }
    compare_values(&args[0], &args[1], "min", |a, b| a < b)
}

/// `max(a, b)` - Return the larger of two numeric values.
///
/// - Two arguments: both must be Int or Float
/// - If both are Int, returns Int
/// - If either is Float, converts to Float for comparison and returns Float
///
/// Examples:
/// ```text
/// max(3, 5)      // => 5
/// max(2.5, 1.0)  // => 2.5
/// max(1, 2.5)    // => 2.5
/// ```
pub(crate) fn builtin_max(args: &[Value]) -> Result<Value, String> {
    if args.len() != 2 {
        return Err(format!("max() expects 2 arguments, got {}", args.len()));
    }
    compare_values(&args[0], &args[1], "max", |a, b| a > b)
}

/// Compare two numeric values using the given predicate.
/// Returns the first value if `prefer_first(a, b)` is true, otherwise the second.
fn compare_values(
    a: &Value,
    b: &Value,
    func_name: &str,
    prefer_first: fn(f64, f64) -> bool,
) -> Result<Value, String> {
    match (a, b) {
        (Value::Concrete(ConcreteValue::Int(x)), Value::Concrete(ConcreteValue::Int(y))) => {
            let (fx, fy) = (*x as f64, *y as f64);
            if prefer_first(fx, fy) {
                Ok(Value::Concrete(ConcreteValue::Int(*x)))
            } else {
                Ok(Value::Concrete(ConcreteValue::Int(*y)))
            }
        }
        (Value::Concrete(ConcreteValue::Float(x)), Value::Concrete(ConcreteValue::Float(y))) => {
            if prefer_first(*x, *y) {
                Ok(Value::Concrete(ConcreteValue::Float(*x)))
            } else {
                Ok(Value::Concrete(ConcreteValue::Float(*y)))
            }
        }
        (Value::Concrete(ConcreteValue::Int(x)), Value::Concrete(ConcreteValue::Float(y))) => {
            let fx = *x as f64;
            if prefer_first(fx, *y) {
                Ok(Value::Concrete(ConcreteValue::Float(fx)))
            } else {
                Ok(Value::Concrete(ConcreteValue::Float(*y)))
            }
        }
        (Value::Concrete(ConcreteValue::Float(x)), Value::Concrete(ConcreteValue::Int(y))) => {
            let fy = *y as f64;
            if prefer_first(*x, fy) {
                Ok(Value::Concrete(ConcreteValue::Float(*x)))
            } else {
                Ok(Value::Concrete(ConcreteValue::Float(fy)))
            }
        }
        _ => Err(format!(
            "{}() arguments must be Int or Float, got {} and {}",
            func_name,
            value_type_name(a),
            value_type_name(b)
        )),
    }
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value as evaluate_builtin;
    use crate::resource::{ConcreteValue, Value};

    // ── min() tests ──

    #[test]
    fn min_two_ints() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::Int(3)),
                Value::Concrete(ConcreteValue::Int(5)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(3)));
    }

    #[test]
    fn min_two_ints_reversed() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::Int(5)),
                Value::Concrete(ConcreteValue::Int(3)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(3)));
    }

    #[test]
    fn min_two_ints_equal() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::Int(4)),
                Value::Concrete(ConcreteValue::Int(4)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(4)));
    }

    #[test]
    fn min_two_floats() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::Float(2.5)),
                Value::Concrete(ConcreteValue::Float(1.0)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Float(1.0)));
    }

    #[test]
    fn min_int_and_float() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Float(2.5)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Float(1.0)));
    }

    #[test]
    fn min_float_and_int() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::Float(3.5)),
                Value::Concrete(ConcreteValue::Int(2)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Float(2.0)));
    }

    #[test]
    fn min_negative_ints() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::Int(-3)),
                Value::Concrete(ConcreteValue::Int(5)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(-3)));
    }

    #[test]
    fn min_wrong_arg_count_zero() {
        let result = evaluate_builtin("min", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn min_partial_application_one_arg() {
        use crate::builtins::evaluate_builtin_for_tests;
        let result =
            evaluate_builtin_for_tests("min", &[Value::Concrete(ConcreteValue::Int(1))]).unwrap();
        assert!(result.is_closure());
    }

    #[test]
    fn min_wrong_arg_count_three() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2)),
                Value::Concrete(ConcreteValue::Int(3)),
            ],
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn min_invalid_type_string() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::String("a".to_string())),
                Value::Concrete(ConcreteValue::Int(1)),
            ],
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be Int or Float"));
    }

    // ── max() tests ──

    #[test]
    fn max_two_ints() {
        let result = evaluate_builtin(
            "max",
            &[
                Value::Concrete(ConcreteValue::Int(3)),
                Value::Concrete(ConcreteValue::Int(5)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(5)));
    }

    #[test]
    fn max_two_ints_reversed() {
        let result = evaluate_builtin(
            "max",
            &[
                Value::Concrete(ConcreteValue::Int(5)),
                Value::Concrete(ConcreteValue::Int(3)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(5)));
    }

    #[test]
    fn max_two_ints_equal() {
        let result = evaluate_builtin(
            "max",
            &[
                Value::Concrete(ConcreteValue::Int(4)),
                Value::Concrete(ConcreteValue::Int(4)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(4)));
    }

    #[test]
    fn max_two_floats() {
        let result = evaluate_builtin(
            "max",
            &[
                Value::Concrete(ConcreteValue::Float(2.5)),
                Value::Concrete(ConcreteValue::Float(1.0)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Float(2.5)));
    }

    #[test]
    fn max_int_and_float() {
        let result = evaluate_builtin(
            "max",
            &[
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Float(2.5)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Float(2.5)));
    }

    #[test]
    fn max_float_and_int() {
        let result = evaluate_builtin(
            "max",
            &[
                Value::Concrete(ConcreteValue::Float(3.5)),
                Value::Concrete(ConcreteValue::Int(2)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Float(3.5)));
    }

    #[test]
    fn max_negative_ints() {
        let result = evaluate_builtin(
            "max",
            &[
                Value::Concrete(ConcreteValue::Int(-3)),
                Value::Concrete(ConcreteValue::Int(5)),
            ],
        )
        .unwrap();
        assert_eq!(result, Value::Concrete(ConcreteValue::Int(5)));
    }

    #[test]
    fn max_wrong_arg_count_zero() {
        let result = evaluate_builtin("max", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn max_invalid_type_bool() {
        let result = evaluate_builtin(
            "max",
            &[
                Value::Concrete(ConcreteValue::Bool(true)),
                Value::Concrete(ConcreteValue::Int(1)),
            ],
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be Int or Float"));
    }
}
