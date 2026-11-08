//! `min(a, b)` and `max(a, b)` built-in functions

use std::cmp::Ordering;

use crate::numeric::{NumericPair, int_to_exact_f64};
use crate::resource::{BindingAddress, ConcreteValue, NoPlaceholder, Value};

use super::value_type_name;

/// `min(a, b)` - Return the smaller of two numeric values.
///
/// - Two arguments: both must be Int or Float
/// - If both are Int, returns Int
/// - Compares all numeric values exactly, without rounding Int operands
/// - If either is Float, returns Float; errors if the selected Int cannot be represented exactly
///
/// Examples:
/// ```text
/// min(3, 5)      // => 3
/// min(2.5, 1.0)  // => 1.0
/// min(1, 2.5)    // => 1.0
/// ```
pub(crate) fn builtin_min<B: BindingAddress>(
    args: &[crate::resource::knowledge::KnownValue<'_, NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, String> {
    if args.len() != 2 {
        return Err(format!("min() expects 2 arguments, got {}", args.len()));
    }
    compare_values(&args[0], &args[1], "min", Ordering::Less)
}

/// `max(a, b)` - Return the larger of two numeric values.
///
/// - Two arguments: both must be Int or Float
/// - If both are Int, returns Int
/// - Compares all numeric values exactly, without rounding Int operands
/// - If either is Float, returns Float; errors if the selected Int cannot be represented exactly
///
/// Examples:
/// ```text
/// max(3, 5)      // => 5
/// max(2.5, 1.0)  // => 2.5
/// max(1, 2.5)    // => 2.5
/// ```
pub(crate) fn builtin_max<B: BindingAddress>(
    args: &[crate::resource::knowledge::KnownValue<'_, NoPlaceholder, B>],
) -> Result<Value<NoPlaceholder, B>, String> {
    if args.len() != 2 {
        return Err(format!("max() expects 2 arguments, got {}", args.len()));
    }
    compare_values(&args[0], &args[1], "max", Ordering::Greater)
}

/// Select the first value if the exact comparison yields `prefer_first`.
/// On equal comparisons, treat Float(-0.0) as smaller than +0 (including Int(0)),
/// selecting the second for remaining ties.
fn compare_values<B: BindingAddress>(
    a: &crate::resource::knowledge::KnownValue<'_, NoPlaceholder, B>,
    b: &crate::resource::knowledge::KnownValue<'_, NoPlaceholder, B>,
    func_name: &str,
    prefer_first: Ordering,
) -> Result<Value<NoPlaceholder, B>, String> {
    let a_value = a.to_value();
    let b_value = b.to_value();
    let (a, b) = (&a_value, &b_value);
    let pair = match (a, b) {
        (Value::Concrete(a), Value::Concrete(b)) => NumericPair::new(a, b),
        _ => None,
    }
    .ok_or_else(|| {
        format!(
            "{}() arguments must be Int or Float, got {} and {}",
            func_name,
            value_type_name(a),
            value_type_name(b)
        )
    })?;
    let ordering = pair.cmp();
    let choose_first = if ordering == Ordering::Equal {
        let is_negative_zero = |value: &Value<NoPlaceholder, B>| matches!(value, Value::Concrete(ConcreteValue::Float(f)) if f.get() == 0.0 && f.get().is_sign_negative());
        match (is_negative_zero(a), is_negative_zero(b)) {
            (true, false) => prefer_first == Ordering::Less,
            (false, true) => prefer_first == Ordering::Greater,
            _ => false,
        }
    } else {
        ordering == prefer_first
    };
    let exact_float = |integer| {
        int_to_exact_f64(integer).ok_or_else(|| {
            format!("{func_name}() integer {integer} cannot be represented exactly as Float")
        })
    };
    let result = match pair {
        NumericPair::Ints(x, y) => ConcreteValue::Int(if choose_first { x } else { y }),
        NumericPair::Floats(x, y) => ConcreteValue::Float(if choose_first { x } else { y }),
        NumericPair::IntFloat(i, f) => {
            ConcreteValue::Float(if choose_first { exact_float(i)? } else { f })
        }
        NumericPair::FloatInt(f, i) => {
            ConcreteValue::Float(if choose_first { f } else { exact_float(i)? })
        }
    };
    Ok(Value::Concrete(result))
}

#[cfg(test)]
mod tests {
    use crate::builtins::evaluate_builtin_to_value_for_tests as evaluate_builtin;
    use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};

    fn assert_signed_zero_result(name: &str, a: ConcreteValue, b: ConcreteValue, expected: f64) {
        let result = evaluate_builtin(name, &[Value::Concrete(a), Value::Concrete(b)]).unwrap();
        let Value::Concrete(ConcreteValue::Float(actual)) = result else {
            panic!("expected Float, got {result:?}");
        };
        assert_eq!(actual.get().to_bits(), expected.to_bits());
    }

    #[test]
    fn min_positive_zero_then_negative_zero() {
        assert_signed_zero_result(
            "min",
            ConcreteValue::Float(crate::finite(0.0)),
            ConcreteValue::Float(crate::finite(-0.0)),
            -0.0,
        );
    }

    #[test]
    fn min_negative_zero_then_positive_zero() {
        assert_signed_zero_result(
            "min",
            ConcreteValue::Float(crate::finite(-0.0)),
            ConcreteValue::Float(crate::finite(0.0)),
            -0.0,
        );
    }

    #[test]
    fn max_positive_zero_then_negative_zero() {
        assert_signed_zero_result(
            "max",
            ConcreteValue::Float(crate::finite(0.0)),
            ConcreteValue::Float(crate::finite(-0.0)),
            0.0,
        );
    }

    #[test]
    fn max_negative_zero_then_positive_zero() {
        assert_signed_zero_result(
            "max",
            ConcreteValue::Float(crate::finite(-0.0)),
            ConcreteValue::Float(crate::finite(0.0)),
            0.0,
        );
    }

    #[test]
    fn min_int_zero_then_negative_zero() {
        assert_signed_zero_result(
            "min",
            ConcreteValue::Int(0),
            ConcreteValue::Float(crate::finite(-0.0)),
            -0.0,
        );
    }

    #[test]
    fn min_negative_zero_then_int_zero() {
        assert_signed_zero_result(
            "min",
            ConcreteValue::Float(crate::finite(-0.0)),
            ConcreteValue::Int(0),
            -0.0,
        );
    }

    #[test]
    fn max_int_zero_then_negative_zero() {
        assert_signed_zero_result(
            "max",
            ConcreteValue::Int(0),
            ConcreteValue::Float(crate::finite(-0.0)),
            0.0,
        );
    }

    #[test]
    fn max_negative_zero_then_int_zero() {
        assert_signed_zero_result(
            "max",
            ConcreteValue::Float(crate::finite(-0.0)),
            ConcreteValue::Int(0),
            0.0,
        );
    }

    #[test]
    fn min_exact_numeric_ints() {
        for (smaller, larger) in [
            (9_007_199_254_740_992, 9_007_199_254_740_993),
            (i64::MAX - 1, i64::MAX),
        ] {
            for (a, b) in [(smaller, larger), (larger, smaller)] {
                assert_eq!(
                    evaluate_builtin(
                        "min",
                        &[
                            Value::Concrete(ConcreteValue::Int(a)),
                            Value::Concrete(ConcreteValue::Int(b)),
                        ],
                    )
                    .unwrap(),
                    Value::Concrete(ConcreteValue::Int(smaller)),
                    "min({a}, {b})",
                );
            }
        }
    }

    #[test]
    fn max_exact_numeric_ints() {
        for (smaller, larger) in [
            (9_007_199_254_740_992, 9_007_199_254_740_993),
            (i64::MAX - 1, i64::MAX),
        ] {
            for (a, b) in [(larger, smaller), (smaller, larger)] {
                assert_eq!(
                    evaluate_builtin(
                        "max",
                        &[
                            Value::Concrete(ConcreteValue::Int(a)),
                            Value::Concrete(ConcreteValue::Int(b)),
                        ],
                    )
                    .unwrap(),
                    Value::Concrete(ConcreteValue::Int(larger)),
                    "max({a}, {b})",
                );
            }
        }
    }

    #[test]
    fn min_exact_numeric_mixed() {
        let args = [
            Value::Concrete(ConcreteValue::Int(9_007_199_254_740_993)),
            Value::Concrete(ConcreteValue::Float(crate::finite(9_007_199_254_740_992.0))),
        ];
        for args in [args.clone(), [args[1].clone(), args[0].clone()]] {
            assert_eq!(
                evaluate_builtin("min", &args).unwrap(),
                Value::Concrete(ConcreteValue::Float(crate::finite(9_007_199_254_740_992.0))),
            );
        }
    }

    #[test]
    fn min_max_exact_numeric_unrepresentable_winner() {
        for (name, integer, float) in [
            ("max", 9_007_199_254_740_993, 9_007_199_254_740_992.0),
            ("min", -9_007_199_254_740_993, -9_007_199_254_740_992.0),
            ("min", i64::MAX, 9_223_372_036_854_775_808.0),
        ] {
            let args = [
                Value::Concrete(ConcreteValue::Int(integer)),
                Value::Concrete(ConcreteValue::Float(crate::finite(float))),
            ];
            for args in [args.clone(), [args[1].clone(), args[0].clone()]] {
                let error = evaluate_builtin(name, &args).unwrap_err();
                assert!(error.contains(&integer.to_string()), "{error}");
                assert!(
                    error.contains("cannot be represented exactly as Float"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn max_exact_numeric_representable_winner() {
        let args = [
            Value::Concrete(ConcreteValue::Int(9_007_199_254_740_992)),
            Value::Concrete(ConcreteValue::Float(crate::finite(1.5))),
        ];
        for args in [args.clone(), [args[1].clone(), args[0].clone()]] {
            assert_eq!(
                evaluate_builtin("max", &args).unwrap(),
                Value::Concrete(ConcreteValue::Float(crate::finite(9_007_199_254_740_992.0))),
            );
        }
    }

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
                Value::Concrete(ConcreteValue::Float(crate::finite(2.5))),
                Value::Concrete(ConcreteValue::Float(crate::finite(1.0))),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::Float(crate::finite(1.0)))
        );
    }

    #[test]
    fn min_int_and_float() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Float(crate::finite(2.5))),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::Float(crate::finite(1.0)))
        );
    }

    #[test]
    fn min_float_and_int() {
        let result = evaluate_builtin(
            "min",
            &[
                Value::Concrete(ConcreteValue::Float(crate::finite(3.5))),
                Value::Concrete(ConcreteValue::Int(2)),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::Float(crate::finite(2.0)))
        );
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
        let result: Result<Value, String> = evaluate_builtin("min", &[]);
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
                Value::Concrete(ConcreteValue::String("a".into())),
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
                Value::Concrete(ConcreteValue::Float(crate::finite(2.5))),
                Value::Concrete(ConcreteValue::Float(crate::finite(1.0))),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::Float(crate::finite(2.5)))
        );
    }

    #[test]
    fn max_int_and_float() {
        let result = evaluate_builtin(
            "max",
            &[
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Float(crate::finite(2.5))),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::Float(crate::finite(2.5)))
        );
    }

    #[test]
    fn max_float_and_int() {
        let result = evaluate_builtin(
            "max",
            &[
                Value::Concrete(ConcreteValue::Float(crate::finite(3.5))),
                Value::Concrete(ConcreteValue::Int(2)),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            Value::Concrete(ConcreteValue::Float(crate::finite(3.5)))
        );
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
        let result: Result<Value, String> = evaluate_builtin("max", &[]);
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
