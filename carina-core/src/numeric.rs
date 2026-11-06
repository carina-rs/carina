//! Exact comparison and checked conversion between integer and float values.

use std::cmp::Ordering;

use crate::FiniteF64;
use crate::resource::{BindingAddress, ConcreteValue, ValueKind};

const I64_MIN_F64: f64 = -9_223_372_036_854_775_808.0;
const I64_UPPER_BOUND_F64: f64 = 9_223_372_036_854_775_808.0;

/// Return the integer only when the float represents an in-range integer exactly.
pub(crate) fn float_as_exact_i64(f: FiniteF64) -> Option<i64> {
    let f = f.get();
    if f.fract() != 0.0 || !(I64_MIN_F64..I64_UPPER_BOUND_F64).contains(&f) {
        return None;
    }

    // A finite integral float in [-2^63, 2^63) converts to i64 exactly.
    let integer = f as i64;
    Some(integer)
}

/// Compare without rounding the integer, including at the i64 boundaries.
pub(crate) fn compare_int_float(i: i64, f: FiniteF64) -> Ordering {
    let f = f.get();
    if f >= I64_UPPER_BOUND_F64 {
        return Ordering::Less;
    }
    if f < I64_MIN_F64 {
        return Ordering::Greater;
    }

    // Truncation is an integral float in [-2^63, 2^63), so the cast is exact.
    let truncated = f.trunc() as i64;
    match i.cmp(&truncated) {
        Ordering::Equal if f.fract() > 0.0 => Ordering::Less,
        Ordering::Equal if f.fract() < 0.0 => Ordering::Greater,
        ordering => ordering,
    }
}

/// Convert only integers that have an exact float representation.
pub(crate) fn int_to_exact_f64(i: i64) -> Option<FiniteF64> {
    // The exact comparison below permits returning this candidate only if no rounding occurred.
    #[allow(clippy::cast_precision_loss)]
    let float = i as f64;
    let float = FiniteF64::new(float).expect("an i64 cast is always finite");
    (compare_int_float(i, float) == Ordering::Equal).then_some(float)
}

/// Classify numeric operands while retaining their original representations.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NumericPair {
    Ints(i64, i64),
    Floats(FiniteF64, FiniteF64),
    IntFloat(i64, FiniteF64),
    FloatInt(FiniteF64, i64),
}

impl NumericPair {
    pub(crate) fn new<P: ValueKind, B: BindingAddress>(
        left: &ConcreteValue<P, B>,
        right: &ConcreteValue<P, B>,
    ) -> Option<Self> {
        match (left, right) {
            (ConcreteValue::Int(left), ConcreteValue::Int(right)) => {
                Some(Self::Ints(*left, *right))
            }
            (ConcreteValue::Float(left), ConcreteValue::Float(right)) => {
                Some(Self::Floats(*left, *right))
            }
            (ConcreteValue::Int(left), ConcreteValue::Float(right)) => {
                Some(Self::IntFloat(*left, *right))
            }
            (ConcreteValue::Float(left), ConcreteValue::Int(right)) => {
                Some(Self::FloatInt(*left, *right))
            }
            _ => None,
        }
    }

    pub(crate) fn cmp(&self) -> Ordering {
        match *self {
            Self::Ints(left, right) => left.cmp(&right),
            Self::Floats(left, right) => left.cmp(&right),
            Self::IntFloat(left, right) => compare_int_float(left, right),
            Self::FloatInt(left, right) => compare_int_float(right, left).reverse(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering::{Equal, Greater, Less};

    use super::*;
    use crate::resource::ExpandedConcreteValue as ConcreteValue;

    #[test]
    fn compare_int_float_exact_numeric_table() {
        for (integer, float, expected) in [
            (9_007_199_254_740_991, 9_007_199_254_740_992.0, Less),
            (9_007_199_254_740_992, 9_007_199_254_740_992.0, Equal),
            (9_007_199_254_740_993, 9_007_199_254_740_992.0, Greater),
            (-9_007_199_254_740_993, -9_007_199_254_740_992.0, Less),
            (i64::MAX, 9_223_372_036_854_775_808.0, Less),
            (i64::MAX, 9_223_372_036_854_774_784.0, Greater),
            (i64::MIN, -9_223_372_036_854_775_808.0, Equal),
            (i64::MIN + 1, -9_223_372_036_854_775_808.0, Greater),
            (i64::MIN, -9_223_372_036_854_777_856.0, Greater),
            (3, 3.5, Less),
            (4, 3.5, Greater),
            (-3, -3.5, Greater),
            (-4, -3.5, Less),
            (0, 0.5, Less),
            (0, -0.5, Greater),
            (0, f64::from_bits(1), Less),
            (0, -f64::from_bits(1), Greater),
            (0, -0.0, Equal),
        ] {
            assert_eq!(
                compare_int_float(integer, crate::finite(float)),
                expected,
                "{integer} vs {float:?}"
            );
        }
    }

    #[test]
    fn float_as_exact_i64_exact_numeric_boundaries() {
        for (float, expected) in [
            (3.0, Some(3)),
            (-3.0, Some(-3)),
            (-0.0, Some(0)),
            (9_007_199_254_740_992.0, Some(9_007_199_254_740_992)),
            (9_223_372_036_854_774_784.0, Some(9_223_372_036_854_774_784)),
            (9_223_372_036_854_775_808.0, None),
            (-9_223_372_036_854_775_808.0, Some(i64::MIN)),
            (-9_223_372_036_854_777_856.0, None),
            (3.5, None),
            (-3.5, None),
            (f64::from_bits(1), None),
        ] {
            assert_eq!(
                float_as_exact_i64(crate::finite(float)),
                expected,
                "{float:?}"
            );
        }
    }

    #[test]
    fn int_to_exact_f64_exact_numeric_round_trip() {
        for (integer, expected) in [
            (0, Some(0.0)),
            (3, Some(3.0)),
            (9_007_199_254_740_991, Some(9_007_199_254_740_991.0)),
            (9_007_199_254_740_992, Some(9_007_199_254_740_992.0)),
            (9_007_199_254_740_993, None),
            (-9_007_199_254_740_993, None),
            (9_007_199_254_740_994, Some(9_007_199_254_740_994.0)),
            (9_223_372_036_854_774_784, Some(9_223_372_036_854_774_784.0)),
            (i64::MAX, None),
            (i64::MAX - 1, None),
            (i64::MIN, Some(-9_223_372_036_854_775_808.0)),
            (i64::MIN + 1, None),
        ] {
            assert_eq!(
                int_to_exact_f64(integer),
                expected.map(crate::finite),
                "{integer}"
            );
        }
    }

    #[test]
    fn numeric_pair_exact_numeric_classification_and_ordering() {
        for (left, right, expected) in [
            (
                ConcreteValue::Int(i64::MAX),
                ConcreteValue::Int(i64::MAX - 1),
                Greater,
            ),
            (
                ConcreteValue::Float(crate::finite(3.0)),
                ConcreteValue::Float(crate::finite(3.5)),
                Less,
            ),
            (
                ConcreteValue::Int(9_007_199_254_740_993),
                ConcreteValue::Float(crate::finite(9_007_199_254_740_992.0)),
                Greater,
            ),
            (
                ConcreteValue::Float(crate::finite(9_223_372_036_854_775_808.0)),
                ConcreteValue::Int(i64::MAX),
                Greater,
            ),
        ] {
            assert_eq!(
                NumericPair::new(&left, &right).unwrap().cmp(),
                expected,
                "{left:?} vs {right:?}"
            );
            assert_eq!(
                NumericPair::new(&right, &left).unwrap().cmp(),
                expected.reverse()
            );
        }
        for other in [ConcreteValue::Bool(true), ConcreteValue::String("3".into())] {
            assert!(NumericPair::new(&ConcreteValue::Int(3), &other).is_none());
            assert!(NumericPair::new(&other, &ConcreteValue::Float(crate::finite(3.0))).is_none());
        }
    }
}
