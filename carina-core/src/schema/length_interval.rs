//! Character-count intervals shared by assignment and input estimation.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LengthInterval {
    pub min: u64,
    pub max: Option<u64>,
}

impl LengthInterval {
    pub const ANY: Self = Self { min: 0, max: None };

    pub fn from_bounds(bounds: Option<&(Option<u64>, Option<u64>)>) -> Self {
        bounds.map_or(Self::ANY, |(min, max)| Self {
            min: min.unwrap_or(0),
            max: *max,
        })
    }

    pub fn exact(value: u64) -> Self {
        Self {
            min: value,
            max: Some(value),
        }
    }

    pub fn contains(self, source: Self) -> bool {
        self.min <= source.min && source.max.unwrap_or(u64::MAX) <= self.max.unwrap_or(u64::MAX)
    }

    pub fn disjoint(self, other: Self) -> bool {
        self.max.is_some_and(|max| max < other.min) || other.max.is_some_and(|max| max < self.min)
    }

    /// On overflow discard the affected bound. A wrapped upper bound would
    /// incorrectly prove impossibility; a wrapped lower bound could prove fit.
    pub fn add(self, other: Self) -> Self {
        Self {
            min: self.min.checked_add(other.min).unwrap_or(0),
            max: self.max.zip(other.max).and_then(|(a, b)| a.checked_add(b)),
        }
    }

    pub fn hull(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.zip(other.max).map(|(a, b)| a.max(b)),
        }
    }

    pub fn intersect(self, other: Self) -> Option<Self> {
        let result = Self {
            min: self.min.max(other.min),
            max: match (self.max, other.max) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        };
        result
            .max
            .is_none_or(|max| max >= result.min)
            .then_some(result)
    }
}

pub(super) fn length_contains(
    source: Option<&(Option<u64>, Option<u64>)>,
    sink: Option<&(Option<u64>, Option<u64>)>,
) -> bool {
    // Preserve assignment's distinction between absent refinement metadata and
    // explicitly authored zero bounds.
    if source.is_none() {
        return matches!(sink, None | Some((None, None)));
    }
    LengthInterval::from_bounds(sink).contains(LengthInterval::from_bounds(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overflow_widens_each_bound_and_never_wraps() {
        assert_eq!(
            LengthInterval::exact(u64::MAX).add(LengthInterval::exact(1)),
            LengthInterval::ANY
        );
        assert_eq!(
            LengthInterval {
                min: 1,
                max: Some(u64::MAX)
            }
            .add(LengthInterval::exact(1)),
            LengthInterval { min: 2, max: None }
        );
    }

    #[test]
    fn missing_bounds_keep_assignment_semantics() {
        assert!(length_contains(None, Some(&(None, None))));
        assert!(!length_contains(None, Some(&(Some(0), Some(10)))));
        assert!(length_contains(
            Some(&(Some(2), Some(4))),
            Some(&(Some(1), Some(5)))
        ));
    }
}
