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
        self.0.cmp(&other.0)
    }
}
impl Serialize for DeclarationPattern {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.as_ref().serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for DeclarationPattern {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        CompiledPattern::try_new(&crate::schema::PatternSpec::deserialize(deserializer)?)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

fn required_pattern<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DeclarationPattern>, D::Error> {
    Option::deserialize(deserializer)
}

fn intersect_pattern(
    target: &mut Option<DeclarationPattern>,
    other: &Option<DeclarationPattern>,
) -> Result<(), &'static str> {
    match (target.as_ref(), other.as_ref()) {
        (Some(first), Some(second)) if first != second => {
            return Err("conflicting identity-layer patterns");
        }
        (None, Some(pattern)) => *target = Some(pattern.clone()),
        _ => {}
    }
    Ok(())
}

/// Scalar evidence retains the two layers even for exact schema projections.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) struct ScalarConstraints {
    scalar: ScalarConstraintKind,
    empty_intersection: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct StringIdentityConstraints {
    length: Bounds<u64>,
    #[serde(deserialize_with = "required_pattern")]
    pattern: Option<DeclarationPattern>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct StringMemberConstraints {
    length: Bounds<u64>,
    #[serde(deserialize_with = "required_pattern")]
    pattern: Option<DeclarationPattern>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum ScalarConstraintKind {
    String {
        identity_layer: StringIdentityConstraints,
        member_layer: StringMemberConstraints,
    },
    Int {
        identity_range: Bounds<i64>,
        member_range: Bounds<i64>,
    },
    Float {
        range: Bounds<crate::FiniteF64>,
    },
}

fn normalize_float_bounds(bounds: Bounds<crate::FiniteF64>) -> Bounds<crate::FiniteF64> {
    let endpoint = |value: Endpoint<crate::FiniteF64>| {
        value
            .value()
            .map(|value| {
                if value.get() == 0.0 {
                    crate::FiniteF64::new(0.0).expect("zero is finite")
                } else {
                    value
                }
            })
            .into()
    };
    Bounds {
        min: endpoint(bounds.min),
        max: endpoint(bounds.max),
    }
}

impl ScalarConstraints {
    fn new(scalar: ScalarConstraintKind) -> Self {
        let mut constraints = Self {
            scalar,
            empty_intersection: false,
        };
        constraints.empty_intersection = constraints.bounds_are_empty();
        constraints
    }

    pub(super) fn from_identity(parts: crate::schema::ScalarIdentityParts<'_>) -> Self {
        use crate::schema::{IntIdentityParts, ScalarIdentityParts, StringIdentityParts};
        Self::new(match parts {
            ScalarIdentityParts::String(StringIdentityParts {
                pattern,
                length,
                identity: _identity,
                validate: _validate,
                to_dsl: _to_dsl,
            }) => ScalarConstraintKind::String {
                identity_layer: StringIdentityConstraints {
                    length: Bounds::new(length),
                    pattern: pattern.map(|spec| {
                        DeclarationPattern(
                            CompiledPattern::try_new(spec).expect("admitted identity pattern"),
                        )
                    }),
                },
                member_layer: StringMemberConstraints {
                    length: Bounds::new(None),
                    pattern: None,
                },
            },
            ScalarIdentityParts::Int(IntIdentityParts {
                range,
                identity: _identity,
                validate: _validate,
            }) => ScalarConstraintKind::Int {
                identity_range: Bounds::new(range),
                member_range: Bounds::new(None),
            },
            ScalarIdentityParts::Float {
                range,
                identity: _identity,
                validate: _validate,
            } => ScalarConstraintKind::Float {
                range: normalize_float_bounds(Bounds::new(range)),
            },
        })
    }

    pub(super) fn from_declaration(declaration: &CompiledAttributeType) -> Option<Self> {
        Some(Self::new(match declaration.kind() {
            AttrTypeKind::String {
                identity_layer,
                member_layer,
            } => ScalarConstraintKind::String {
                identity_layer: StringIdentityConstraints {
                    length: Bounds::new(identity_layer.length),
                    pattern: identity_layer.pattern.clone().map(DeclarationPattern),
                },
                member_layer: StringMemberConstraints {
                    length: Bounds::new(member_layer.length),
                    pattern: member_layer.pattern.clone().map(DeclarationPattern),
                },
            },
            AttrTypeKind::Reference {
                pattern, length, ..
            } => ScalarConstraintKind::String {
                identity_layer: StringIdentityConstraints {
                    length: Bounds::new(*length),
                    pattern: pattern.clone().map(DeclarationPattern),
                },
                member_layer: StringMemberConstraints {
                    length: Bounds::new(None),
                    pattern: None,
                },
            },
            AttrTypeKind::Int {
                identity_layer,
                member_layer,
            } => ScalarConstraintKind::Int {
                identity_range: Bounds::new(identity_layer.range),
                member_range: Bounds::new(member_layer.range),
            },
            AttrTypeKind::Float { range, .. } => ScalarConstraintKind::Float {
                range: normalize_float_bounds(Bounds::new(*range)),
            },
            AttrTypeKind::Enum { base, .. } => return Self::from_declaration(base),
            _ => return None,
        }))
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
            ScalarConstraintKind::String {
                identity_layer,
                member_layer,
            } => {
                let mut length = identity_layer.length;
                length.intersect(&member_layer.length);
                length.is_empty()
            }
            ScalarConstraintKind::Int {
                identity_range,
                member_range,
            } => {
                let mut range = *identity_range;
                range.intersect(member_range);
                range.is_empty()
            }
            ScalarConstraintKind::Float { range } => range.is_empty(),
        }
    }

    pub(super) fn validate_record(&mut self) -> Result<(), &'static str> {
        if self.empty_intersection != self.bounds_are_empty() {
            return Err("scalar bounds and empty-intersection marker disagree");
        }
        if let ScalarConstraintKind::Float { range } = &mut self.scalar {
            *range = normalize_float_bounds(*range);
        }
        Ok(())
    }

    pub(super) fn intersect(&mut self, other: &Self) -> Result<(), &'static str> {
        match (&mut self.scalar, &other.scalar) {
            (
                ScalarConstraintKind::String {
                    identity_layer,
                    member_layer,
                },
                ScalarConstraintKind::String {
                    identity_layer: other_identity,
                    member_layer: other_member,
                },
            ) => {
                // Catalog admission has already proved identity agreement.
                // Never collapse distinct patterns into an overwritten slot.
                intersect_pattern(&mut identity_layer.pattern, &other_identity.pattern)?;
                intersect_pattern(&mut member_layer.pattern, &other_member.pattern)?;
                identity_layer.length.intersect(&other_identity.length);
                member_layer.length.intersect(&other_member.length);
            }
            (
                ScalarConstraintKind::Int {
                    identity_range,
                    member_range,
                },
                ScalarConstraintKind::Int {
                    identity_range: other_identity,
                    member_range: other_member,
                },
            ) => {
                identity_range.intersect(other_identity);
                member_range.intersect(other_member);
            }
            (
                ScalarConstraintKind::Float { range },
                ScalarConstraintKind::Float { range: other },
            ) => range.intersect(other),
            _ => unreachable!("annotation resolution checks scalar kinds before intersecting"),
        }
        self.empty_intersection = self.bounds_are_empty();
        Ok(())
    }

    pub(super) fn materialize(&self, owned: &super::ResolvedSchemaType) -> CompiledAttributeType {
        let binding = crate::schema::RefinementBinding::Annotation(owned.clone());
        let kind = match &self.scalar {
            ScalarConstraintKind::String {
                identity_layer,
                member_layer,
            } => AttrTypeKind::String {
                identity_layer: crate::schema::StringIdentityLayer {
                    binding,
                    length: identity_layer.length.pair(),
                    pattern: identity_layer
                        .pattern
                        .as_ref()
                        .map(|pattern| pattern.0.clone()),
                    to_dsl: None,
                },
                member_layer: crate::schema::StringMemberConstraints::new(
                    member_layer
                        .pattern
                        .as_ref()
                        .map(|pattern| pattern.0.clone()),
                    member_layer.length.pair(),
                ),
            },
            ScalarConstraintKind::Int {
                identity_range,
                member_range,
            } => AttrTypeKind::Int {
                identity_layer: crate::schema::IntIdentityLayer {
                    binding,
                    range: identity_range.pair(),
                },
                member_layer: crate::schema::IntMemberConstraints::new(member_range.pair()),
            },
            ScalarConstraintKind::Float { range } => AttrTypeKind::Float {
                binding,
                range: range.pair(),
            },
        };
        CompiledAttributeType { kind }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialized_declaration_bounds_normalize_both_zero_signs() {
        let json = r#"{"scalar":{"Float":{"range":{"min":{"Inclusive":-0.0},"max":{"Inclusive":0.0}}}},"empty_intersection":false}"#;
        let mut constraints: ScalarConstraints = serde_json::from_str(json).unwrap();
        constraints.validate_record().unwrap();
        let ScalarConstraintKind::Float { range } = constraints.scalar else {
            panic!("expected float bounds")
        };
        assert_eq!(
            range.min.value().unwrap().get().to_bits(),
            0.0_f64.to_bits()
        );
        assert_eq!(
            range.max.value().unwrap().get().to_bits(),
            0.0_f64.to_bits()
        );
        for raw in ["1e999", "-1e999"] {
            assert!(serde_json::from_str::<ScalarConstraints>(&json.replace("-0.0", raw)).is_err());
        }
    }
}

#[cfg(test)]
mod pattern_mode_tests {
    use super::*;
    use crate::schema::{PatternMatch, PatternReason, PatternSpec};

    #[test]
    fn retained_layers_deserialize_complete_pattern_specs_only() {
        for reason in ["first exception", "second exception"] {
            let identity_pattern = DeclarationPattern(
                CompiledPattern::try_new(&PatternSpec::new("abc", PatternMatch::Full)).unwrap(),
            );
            let member_pattern = DeclarationPattern(
                CompiledPattern::try_new(&PatternSpec::new(
                    "abc",
                    PatternMatch::Substring {
                        reason: PatternReason::new(reason).unwrap(),
                    },
                ))
                .unwrap(),
            );
            let mut retained = ScalarConstraints::new(ScalarConstraintKind::String {
                identity_layer: StringIdentityConstraints {
                    length: Bounds::new(None),
                    pattern: Some(identity_pattern),
                },
                member_layer: StringMemberConstraints {
                    length: Bounds::new(None),
                    pattern: Some(member_pattern),
                },
            });
            retained.intersect(&retained.clone()).unwrap();
            let json = serde_json::to_string(&retained).unwrap();
            let decoded: ScalarConstraints = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, retained);
            let ScalarConstraintKind::String {
                identity_layer,
                member_layer,
            } = decoded.scalar
            else {
                panic!("String layers")
            };
            assert!(!identity_layer.pattern.unwrap().0.is_match("xabcx"));
            assert!(member_layer.pattern.unwrap().0.is_match("xabcx"));
        }
        for bad in [
            serde_json::json!("abc"),
            serde_json::json!({"regex":"abc"}),
            serde_json::json!({"regex":"abc","mode":{"kind":"substring","reason":""}}),
        ] {
            assert!(serde_json::from_value::<DeclarationPattern>(bad).is_err());
        }
    }
}
