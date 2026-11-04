//! Exact comparison and checked conversion between integer and float values.

use std::cmp::Ordering;

use crate::resource::{BindingAddress, ConcreteValue, ValueKind};

const I64_MIN_F64: f64 = -9_223_372_036_854_775_808.0;
const I64_UPPER_BOUND_F64: f64 = 9_223_372_036_854_775_808.0;

/// Return the integer only when the float represents an in-range integer exactly.
pub(crate) fn float_as_exact_i64(f: f64) -> Option<i64> {
    if !f.is_finite() || f.fract() != 0.0 || !(I64_MIN_F64..I64_UPPER_BOUND_F64).contains(&f) {
        return None;
    }

    // A finite integral float in [-2^63, 2^63) converts to i64 exactly.
    let integer = f as i64;
    Some(integer)
}

/// Compare without rounding the integer, including at the i64 boundaries.
pub(crate) fn compare_int_float(i: i64, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if f >= I64_UPPER_BOUND_F64 {
        return Some(Ordering::Less);
    }
    if f < I64_MIN_F64 {
        return Some(Ordering::Greater);
    }

    // Truncation is an integral float in [-2^63, 2^63), so the cast is exact.
    let truncated = f.trunc() as i64;
    Some(match i.cmp(&truncated) {
        Ordering::Equal if f.fract() > 0.0 => Ordering::Less,
        Ordering::Equal if f.fract() < 0.0 => Ordering::Greater,
        ordering => ordering,
    })
}

/// Convert only integers that have an exact float representation.
pub(crate) fn int_to_exact_f64(i: i64) -> Option<f64> {
    // The exact comparison below permits returning this candidate only if no rounding occurred.
    #[allow(clippy::cast_precision_loss)]
    let float = i as f64;
    (compare_int_float(i, float) == Some(Ordering::Equal)).then_some(float)
}

/// Classify numeric operands while retaining their original representations.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NumericPair {
    Ints(i64, i64),
    Floats(f64, f64),
    IntFloat(i64, f64),
    FloatInt(f64, i64),
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

    pub(crate) fn partial_cmp(&self) -> Option<Ordering> {
        match *self {
            Self::Ints(left, right) => Some(left.cmp(&right)),
            Self::Floats(left, right) => left.partial_cmp(&right),
            Self::IntFloat(left, right) => compare_int_float(left, right),
            Self::FloatInt(left, right) => compare_int_float(right, left).map(Ordering::reverse),
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
            (9_007_199_254_740_991, 9_007_199_254_740_992.0, Some(Less)),
            (9_007_199_254_740_992, 9_007_199_254_740_992.0, Some(Equal)),
            (
                9_007_199_254_740_993,
                9_007_199_254_740_992.0,
                Some(Greater),
            ),
            (-9_007_199_254_740_993, -9_007_199_254_740_992.0, Some(Less)),
            (i64::MAX, 9_223_372_036_854_775_808.0, Some(Less)),
            (i64::MAX, 9_223_372_036_854_774_784.0, Some(Greater)),
            (i64::MIN, -9_223_372_036_854_775_808.0, Some(Equal)),
            (i64::MIN + 1, -9_223_372_036_854_775_808.0, Some(Greater)),
            (i64::MIN, -9_223_372_036_854_777_856.0, Some(Greater)),
            (3, 3.5, Some(Less)),
            (4, 3.5, Some(Greater)),
            (-3, -3.5, Some(Greater)),
            (-4, -3.5, Some(Less)),
            (0, 0.5, Some(Less)),
            (0, -0.5, Some(Greater)),
            (0, f64::from_bits(1), Some(Less)),
            (0, -f64::from_bits(1), Some(Greater)),
            (0, -0.0, Some(Equal)),
            (0, f64::NAN, None),
            (i64::MAX, f64::INFINITY, Some(Less)),
            (i64::MIN, f64::NEG_INFINITY, Some(Greater)),
        ] {
            assert_eq!(
                compare_int_float(integer, float),
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
            (f64::NAN, None),
            (f64::INFINITY, None),
            (f64::NEG_INFINITY, None),
        ] {
            assert_eq!(float_as_exact_i64(float), expected, "{float:?}");
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
            assert_eq!(int_to_exact_f64(integer), expected, "{integer}");
        }
    }

    #[test]
    fn numeric_pair_exact_numeric_classification_and_ordering() {
        for (left, right, expected) in [
            (
                ConcreteValue::Int(i64::MAX),
                ConcreteValue::Int(i64::MAX - 1),
                Some(Greater),
            ),
            (
                ConcreteValue::Float(3.0),
                ConcreteValue::Float(3.5),
                Some(Less),
            ),
            (
                ConcreteValue::Int(9_007_199_254_740_993),
                ConcreteValue::Float(9_007_199_254_740_992.0),
                Some(Greater),
            ),
            (
                ConcreteValue::Float(9_223_372_036_854_775_808.0),
                ConcreteValue::Int(i64::MAX),
                Some(Greater),
            ),
            (
                ConcreteValue::Float(f64::NAN),
                ConcreteValue::Float(0.0),
                None,
            ),
            (ConcreteValue::Int(0), ConcreteValue::Float(f64::NAN), None),
            (ConcreteValue::Float(f64::NAN), ConcreteValue::Int(0), None),
        ] {
            assert_eq!(
                NumericPair::new(&left, &right).unwrap().partial_cmp(),
                expected,
                "{left:?} vs {right:?}"
            );
            assert_eq!(
                NumericPair::new(&right, &left).unwrap().partial_cmp(),
                expected.map(std::cmp::Ordering::reverse)
            );
        }
        for other in [ConcreteValue::Bool(true), ConcreteValue::String("3".into())] {
            assert!(NumericPair::new(&ConcreteValue::Int(3), &other).is_none());
            assert!(NumericPair::new(&other, &ConcreteValue::Float(3.0)).is_none());
        }
    }
}
