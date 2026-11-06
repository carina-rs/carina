use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A finite floating-point number that preserves the sign of zero.
///
/// Construction must go through [`Self::new`] or [`TryFrom<f64>`]. The field
/// is private so callers cannot bypass the finiteness check:
///
/// ```compile_fail,E0423
/// use carina_provider_protocol::FiniteF64;
///
/// let _ = FiniteF64(1.0);
/// ```
#[derive(Clone, Copy)]
pub struct FiniteF64(f64);

/// The non-finite category rejected by [`FiniteF64::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonFiniteFloatError {
    NaN,
    PositiveInfinity,
    NegativeInfinity,
}

impl fmt::Display for NonFiniteFloatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let category = match self {
            Self::NaN => "NaN",
            Self::PositiveInfinity => "+inf",
            Self::NegativeInfinity => "-inf",
        };
        write!(formatter, "non-finite float {category}")
    }
}

impl std::error::Error for NonFiniteFloatError {}

impl FiniteF64 {
    /// Admit a number only if it is finite, preserving its exact bits.
    pub fn new(value: f64) -> Result<Self, NonFiniteFloatError> {
        if value.is_finite() {
            Ok(Self(value))
        } else if value.is_nan() {
            Err(NonFiniteFloatError::NaN)
        } else if value.is_sign_positive() {
            Err(NonFiniteFloatError::PositiveInfinity)
        } else {
            Err(NonFiniteFloatError::NegativeInfinity)
        }
    }

    pub fn get(self) -> f64 {
        self.0
    }

    /// Convert to an ordinary JSON number using the finiteness invariant.
    pub fn to_json_number(self) -> serde_json::Number {
        serde_json::Number::from_f64(self.0).expect("FiniteF64 always contains a finite number")
    }
}

impl TryFrom<f64> for FiniteF64 {
    type Error = NonFiniteFloatError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl fmt::Display for FiniteF64 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl fmt::Debug for FiniteF64 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, formatter)
    }
}

impl PartialEq for FiniteF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for FiniteF64 {}

impl Ord for FiniteF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        if self == other {
            Ordering::Equal
        } else {
            self.0.total_cmp(&other.0)
        }
    }
}

impl PartialOrd for FiniteF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for FiniteF64 {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let bits = if self.0 == 0.0 { 0 } else { self.0.to_bits() };
        bits.hash(state);
    }
}

impl Serialize for FiniteF64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.0)
    }
}

impl<'de> Deserialize<'de> for FiniteF64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(f64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};
    use std::hash::DefaultHasher;

    use serde::de::value::{Error, F64Deserializer};

    use super::*;

    #[test]
    fn constructors_accept_finite_values_and_preserve_bits() {
        for value in [
            f64::MIN,
            -1.0,
            -f64::MIN_POSITIVE,
            -f64::from_bits(1),
            -0.0,
            0.0,
            f64::from_bits(1),
            f64::MIN_POSITIVE,
            1.0,
            42.25,
            f64::MAX,
        ] {
            assert_eq!(
                FiniteF64::new(value).unwrap().get().to_bits(),
                value.to_bits()
            );
            assert_eq!(
                FiniteF64::try_from(value).unwrap().get().to_bits(),
                value.to_bits()
            );
        }
    }

    #[test]
    fn constructors_reject_each_non_finite_category() {
        for (value, category, message) in [
            (f64::NAN, NonFiniteFloatError::NaN, "non-finite float NaN"),
            (-f64::NAN, NonFiniteFloatError::NaN, "non-finite float NaN"),
            (
                f64::from_bits(0x7ff0_0000_0000_0001),
                NonFiniteFloatError::NaN,
                "non-finite float NaN",
            ),
            (
                f64::INFINITY,
                NonFiniteFloatError::PositiveInfinity,
                "non-finite float +inf",
            ),
            (
                f64::NEG_INFINITY,
                NonFiniteFloatError::NegativeInfinity,
                "non-finite float -inf",
            ),
        ] {
            assert_eq!(FiniteF64::new(value), Err(category));
            assert_eq!(FiniteF64::try_from(value), Err(category));
            assert_eq!(category.to_string(), message);
            let error: &dyn std::error::Error = &category;
            assert!(error.source().is_none());
        }
    }

    #[test]
    fn signed_zeros_have_consistent_equality_order_and_hash() {
        let negative = FiniteF64::new(-0.0).unwrap();
        let positive = FiniteF64::new(0.0).unwrap();
        assert_eq!(negative, positive);
        assert_eq!(negative.cmp(&positive), Ordering::Equal);
        assert_eq!(positive.cmp(&negative), Ordering::Equal);
        assert_eq!(negative.partial_cmp(&positive), Some(Ordering::Equal));
        assert_eq!(positive.partial_cmp(&negative), Some(Ordering::Equal));

        let hash = |value: FiniteF64| {
            let mut hasher = DefaultHasher::new();
            value.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(negative), hash(positive));
        assert_eq!(HashSet::from([negative, positive]).len(), 1);
        assert_eq!(BTreeSet::from([negative, positive]).len(), 1);
    }

    #[test]
    fn get_preserves_negative_zero_sign_bit() {
        assert_eq!(
            FiniteF64::new(-0.0).unwrap().get().to_bits(),
            (-0.0_f64).to_bits()
        );
    }

    #[test]
    fn ordering_matches_numeric_order_including_subnormals() {
        let ordered = [
            f64::MIN,
            -1.0,
            -f64::MIN_POSITIVE,
            -f64::from_bits(1),
            0.0,
            f64::from_bits(1),
            f64::MIN_POSITIVE,
            1.0,
            f64::MAX,
        ];
        for pair in ordered.windows(2) {
            let left = FiniteF64::new(pair[0]).unwrap();
            let right = FiniteF64::new(pair[1]).unwrap();
            assert!(left < right);
            assert_eq!(left.partial_cmp(&right), Some(Ordering::Less));
            assert_eq!(right.cmp(&left), Ordering::Greater);
        }
    }

    #[test]
    fn display_and_debug_delegate_to_the_number() {
        for value in [-0.0, 0.0, 1.0, -42.25, f64::from_bits(1), f64::MAX] {
            let finite = FiniteF64::new(value).unwrap();
            assert_eq!(format!("{finite}"), format!("{value}"));
            assert_eq!(format!("{finite:?}"), format!("{value:?}"));
            assert_eq!(format!("{finite:.3}"), format!("{value:.3}"));
        }
    }

    #[test]
    fn json_round_trip_uses_plain_numbers_and_preserves_bits() {
        for value in [
            f64::MIN,
            -42.25,
            -f64::from_bits(1),
            -0.0,
            0.0,
            f64::from_bits(1),
            f64::MIN_POSITIVE,
            1.0,
            f64::MAX,
        ] {
            let finite = FiniteF64::new(value).unwrap();
            let json = serde_json::to_string(&finite).unwrap();
            assert_eq!(json, serde_json::to_string(&value).unwrap());
            assert!(
                serde_json::from_str::<serde_json::Value>(&json)
                    .unwrap()
                    .is_number()
            );
            let decoded: FiniteF64 = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded.get().to_bits(), value.to_bits());
            assert_eq!(
                finite.to_json_number().as_f64().unwrap().to_bits(),
                value.to_bits()
            );
            assert_eq!(
                serde_json::to_value(finite).unwrap(),
                serde_json::Value::Number(finite.to_json_number())
            );
        }
    }

    #[test]
    fn deserialization_rejects_non_finite_raw_floats() {
        for (value, category) in [
            (f64::NAN, NonFiniteFloatError::NaN),
            (f64::INFINITY, NonFiniteFloatError::PositiveInfinity),
            (f64::NEG_INFINITY, NonFiniteFloatError::NegativeInfinity),
        ] {
            let error = FiniteF64::deserialize(F64Deserializer::<Error>::new(value)).unwrap_err();
            assert_eq!(error.to_string(), category.to_string());
        }
    }

    #[test]
    fn json_overflow_is_an_error_instead_of_null_or_zero() {
        for json in ["1e999", "-1e999"] {
            let error = serde_json::from_str::<FiniteF64>(json).unwrap_err();
            assert!(error.is_syntax(), "{error}");
            assert!(error.to_string().contains("number out of range"), "{error}");
            assert!(serde_json::from_str::<Option<FiniteF64>>(json).is_err());
        }
    }
}
