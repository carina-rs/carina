//! Scalar declaration evidence retained by resolved schema annotations.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

use crate::schema::{AttrTypeKind, CompiledAttributeType, CompiledPattern, RefinedScalarKind};

/// Unlike an Option field, an endpoint cannot silently default to unbounded
/// when a wire record omits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum Endpoint<T> {
    Unbounded,
    Inclusive(T),
}

impl<T: Copy> Endpoint<T> {
    fn value(self) -> Option<T> {
        match self {
            Self::Unbounded => None,
            Self::Inclusive(value) => Some(value),
        }
    }
}

impl<T> From<Option<T>> for Endpoint<T> {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Unbounded, Self::Inclusive)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct Bounds<T> {
    min: Endpoint<T>,
    max: Endpoint<T>,
}

impl<T: Copy + Ord> Bounds<T> {
    fn new(bounds: Option<(Option<T>, Option<T>)>) -> Self {
        let (min, max) = bounds.unwrap_or((None, None));
        Self {
            min: min.into(),
            max: max.into(),
        }
    }

    fn pair(self) -> Option<(Option<T>, Option<T>)> {
        let pair = (self.min.value(), self.max.value());
        (pair != (None, None)).then_some(pair)
    }

    fn intersect(&mut self, other: &Self) {
        self.min = match (self.min.value(), other.min.value()) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
        .into();
        self.max = match (self.max.value(), other.max.value()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
        .into();
    }

    fn is_empty(self) -> bool {
        matches!((self.min.value(), self.max.value()), (Some(min), Some(max)) if min > max)
    }
}

/// A finite float with stable equality and ordering for scalar bounds.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(transparent)]
struct FiniteFloat(f64);

impl FiniteFloat {
    fn new(value: f64) -> Option<Self> {
        value
            .is_finite()
            .then_some(Self(if value == 0.0 { 0.0 } else { value }))
    }
}

impl<'de> Deserialize<'de> for FiniteFloat {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(f64::deserialize(deserializer)?)
            .ok_or_else(|| serde::de::Error::custom("scalar range endpoints must be finite"))
    }
}

impl PartialEq for FiniteFloat {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for FiniteFloat {}
impl PartialOrd for FiniteFloat {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for FiniteFloat {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

#[derive(Debug, Clone)]
struct DeclarationPattern(CompiledPattern);

impl PartialEq for DeclarationPattern {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for DeclarationPattern {}
impl PartialOrd for DeclarationPattern {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for DeclarationPattern {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_str().cmp(other.0.as_str())
    }
}
impl Serialize for DeclarationPattern {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.as_str())
    }
}
impl<'de> Deserialize<'de> for DeclarationPattern {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        CompiledPattern::try_new(&String::deserialize(deserializer)?)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// The effective conjunction. Every field is mandatory on the wire, including
/// unbounded endpoints and the marker distinguishing an empty intersection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) struct ScalarConstraints {
    scalar: ScalarConstraintKind,
    empty_intersection: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum ScalarConstraintKind {
    String {
        length: Bounds<u64>,
        patterns: Vec<DeclarationPattern>,
    },
    Int {
        range: Bounds<i64>,
    },
    Float {
        range: Bounds<FiniteFloat>,
    },
}

impl ScalarConstraints {
    pub(super) fn from_declaration(declaration: &CompiledAttributeType) -> Option<Self> {
        let scalar = match declaration.kind() {
            AttrTypeKind::String {
                pattern, length, ..
            }
            | AttrTypeKind::Reference {
                pattern, length, ..
            } => ScalarConstraintKind::String {
                length: Bounds::new(*length),
                patterns: pattern.iter().cloned().map(DeclarationPattern).collect(),
            },
            AttrTypeKind::Int { range, .. } => ScalarConstraintKind::Int {
                range: Bounds::new(*range),
            },
            AttrTypeKind::Float { range, .. } => {
                let (min, max) = range.unwrap_or((None, None));
                let endpoint = |value: Option<f64>| match value {
                    Some(value) => FiniteFloat::new(value).map(Some),
                    None => Some(None),
                };
                ScalarConstraintKind::Float {
                    range: Bounds::new(Some((endpoint(min)?, endpoint(max)?))),
                }
            }
            AttrTypeKind::Enum { base, .. } => return Self::from_declaration(base),
            _ => return None,
        };
        let mut constraints = Self {
            scalar,
            empty_intersection: false,
        };
        constraints.empty_intersection = constraints.bounds_are_empty();
        Some(constraints)
    }

    pub(super) fn kind(&self) -> RefinedScalarKind {
        match self.scalar {
            ScalarConstraintKind::String { .. } => RefinedScalarKind::String,
            ScalarConstraintKind::Int { .. } => RefinedScalarKind::Int,
            ScalarConstraintKind::Float { .. } => RefinedScalarKind::Float,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.empty_intersection
    }

    fn bounds_are_empty(&self) -> bool {
        match &self.scalar {
            ScalarConstraintKind::String { length, .. } => length.is_empty(),
            ScalarConstraintKind::Int { range } => range.is_empty(),
            ScalarConstraintKind::Float { range } => range.is_empty(),
        }
    }

    pub(super) fn validate_record(&mut self) -> Result<(), &'static str> {
        if self.empty_intersection != self.bounds_are_empty() {
            return Err("scalar bounds and empty-intersection marker disagree");
        }
        if let ScalarConstraintKind::String { patterns, .. } = &mut self.scalar {
            patterns.sort();
            patterns.dedup();
        }
        Ok(())
    }

    pub(super) fn intersect(&mut self, other: &Self) {
        match (&mut self.scalar, &other.scalar) {
            (
                ScalarConstraintKind::String { length, patterns },
                ScalarConstraintKind::String {
                    length: other_length,
                    patterns: other_patterns,
                },
            ) => {
                length.intersect(other_length);
                patterns.extend(other_patterns.iter().cloned());
                patterns.sort();
                patterns.dedup();
            }
            (ScalarConstraintKind::Int { range }, ScalarConstraintKind::Int { range: other }) => {
                range.intersect(other)
            }
            (
                ScalarConstraintKind::Float { range },
                ScalarConstraintKind::Float { range: other },
            ) => range.intersect(other),
            _ => unreachable!("annotation resolution checks scalar kinds before intersecting"),
        }
        self.empty_intersection = self.bounds_are_empty();
    }

    pub(super) fn materialize(&self, owned: &super::ResolvedSchemaType) -> CompiledAttributeType {
        let binding = crate::schema::RefinementBinding::Annotation(owned.clone());
        self.materialize_with_binding(binding)
    }

    fn materialize_with_binding(
        &self,
        binding: crate::schema::RefinementBinding,
    ) -> CompiledAttributeType {
        let kind = match &self.scalar {
            ScalarConstraintKind::String { length, patterns } => AttrTypeKind::String {
                binding,
                length: length.pair(),
                pattern: patterns.first().map(|pattern| pattern.0.clone()),
                to_dsl: None,
            },
            ScalarConstraintKind::Int { range } => AttrTypeKind::Int {
                binding,
                range: range.pair(),
            },
            ScalarConstraintKind::Float { range } => AttrTypeKind::Float {
                binding,
                range: range
                    .pair()
                    .map(|(min, max)| (min.map(|v| v.0), max.map(|v| v.0))),
            },
        };
        CompiledAttributeType { kind }
    }

    pub(super) fn patterns(&self) -> impl Iterator<Item = &CompiledPattern> {
        let patterns = match &self.scalar {
            ScalarConstraintKind::String { patterns, .. } => Some(patterns),
            _ => None,
        };
        patterns.into_iter().flatten().map(|pattern| &pattern.0)
    }
}
