//! `min(a, b)` and `max(a, b)` built-in functions

use crate::resource::Value;

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
        (Value::Int(x), Value::Int(y)) => {
            let (fx, fy) = (*x as f64, *y as f64);
            if prefer_first(fx, fy) {
                Ok(Value::Int(*x))
            } else {
                Ok(Value::Int(*y))
            }
        }
        (Value::Float(x), Value::Float(y)) => {
            if prefer_first(*x, *y) {
                Ok(Value::Float(*x))
            } else {
                Ok(Value::Float(*y))
            }
        }
        (Value::Int(x), Value::Float(y)) => {
            let fx = *x as f64;
            if prefer_first(fx, *y) {
                Ok(Value::Float(fx))
            } else {
                Ok(Value::Float(*y))
            }
        }
        (Value::Float(x), Value::Int(y)) => {
            let fy = *y as f64;
            if prefer_first(*x, fy) {
                Ok(Value::Float(*x))
            } else {
                Ok(Value::Float(fy))
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
    use crate::builtins::evaluate_builtin;
    use crate::resource::Value;

    // ── min() tests ──

    #[test]
    fn min_two_ints() {
        let result = evaluate_builtin("min", &[Value::Int(3), Value::Int(5)]).unwrap();
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn min_two_ints_reversed() {
        let result = evaluate_builtin("min", &[Value::Int(5), Value::Int(3)]).unwrap();
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn min_two_ints_equal() {
        let result = evaluate_builtin("min", &[Value::Int(4), Value::Int(4)]).unwrap();
        assert_eq!(result, Value::Int(4));
    }

    #[test]
    fn min_two_floats() {
        let result = evaluate_builtin("min", &[Value::Float(2.5), Value::Float(1.0)]).unwrap();
        assert_eq!(result, Value::Float(1.0));
    }

    #[test]
    fn min_int_and_float() {
        let result = evaluate_builtin("min", &[Value::Int(1), Value::Float(2.5)]).unwrap();
        assert_eq!(result, Value::Float(1.0));
    }

    #[test]
    fn min_float_and_int() {
        let result = evaluate_builtin("min", &[Value::Float(3.5), Value::Int(2)]).unwrap();
        assert_eq!(result, Value::Float(2.0));
    }

    #[test]
    fn min_negative_ints() {
        let result = evaluate_builtin("min", &[Value::Int(-3), Value::Int(5)]).unwrap();
        assert_eq!(result, Value::Int(-3));
    }

    #[test]
    fn min_wrong_arg_count_zero() {
        let result = evaluate_builtin("min", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn min_wrong_arg_count_one() {
        let result = evaluate_builtin("min", &[Value::Int(1)]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn min_wrong_arg_count_three() {
        let result = evaluate_builtin("min", &[Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn min_invalid_type_string() {
        let result = evaluate_builtin("min", &[Value::String("a".to_string()), Value::Int(1)]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be Int or Float"));
    }

    // ── max() tests ──

    #[test]
    fn max_two_ints() {
        let result = evaluate_builtin("max", &[Value::Int(3), Value::Int(5)]).unwrap();
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn max_two_ints_reversed() {
        let result = evaluate_builtin("max", &[Value::Int(5), Value::Int(3)]).unwrap();
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn max_two_ints_equal() {
        let result = evaluate_builtin("max", &[Value::Int(4), Value::Int(4)]).unwrap();
        assert_eq!(result, Value::Int(4));
    }

    #[test]
    fn max_two_floats() {
        let result = evaluate_builtin("max", &[Value::Float(2.5), Value::Float(1.0)]).unwrap();
        assert_eq!(result, Value::Float(2.5));
    }

    #[test]
    fn max_int_and_float() {
        let result = evaluate_builtin("max", &[Value::Int(1), Value::Float(2.5)]).unwrap();
        assert_eq!(result, Value::Float(2.5));
    }

    #[test]
    fn max_float_and_int() {
        let result = evaluate_builtin("max", &[Value::Float(3.5), Value::Int(2)]).unwrap();
        assert_eq!(result, Value::Float(3.5));
    }

    #[test]
    fn max_negative_ints() {
        let result = evaluate_builtin("max", &[Value::Int(-3), Value::Int(5)]).unwrap();
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn max_wrong_arg_count_zero() {
        let result = evaluate_builtin("max", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("expects 2 arguments"));
    }

    #[test]
    fn max_invalid_type_bool() {
        let result = evaluate_builtin("max", &[Value::Bool(true), Value::Int(1)]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("must be Int or Float"));
    }
}
