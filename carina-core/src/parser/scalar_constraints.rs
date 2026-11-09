//! Scalar declaration evidence retained by resolved schema annotations.

use std::{cmp::Ordering, collections::BTreeMap};

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
#[serde(deny_unknown_fields)]
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
        *self = Self::new(crate::schema::intersect_bounds(self.pair(), other.pair()));
    }

    fn is_empty(self) -> bool {
        crate::schema::bounds_are_empty(self.pair())
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

/// Exact schema-declaration evidence is the only payload with member constraints.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ScalarConstraints {
    scalar: ScalarConstraintKind,
    empty_intersection: bool,
}

/// An authored annotation names an identity. No member slots exist here.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IdentityConstraints {
    scalar: IdentityConstraintKind,
    empty_intersection: bool,
    // Preserve each owner's identity pattern, not a selected member pattern.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "unique_owner_layers"
    )]
    owner_layers: BTreeMap<String, IdentityConstraintKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StringIdentityConstraints {
    length: Bounds<u64>,
    #[serde(deserialize_with = "required_pattern")]
    pattern: Option<DeclarationPattern>,
}

impl StringIdentityConstraints {
    fn intersect(&mut self, other: &Self) {
        self.pattern = self
            .pattern
            .iter()
            .chain(other.pattern.iter())
            .min()
            .cloned();
        self.length.intersect(&other.length);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StringMemberConstraints {
    length: Bounds<u64>,
    #[serde(deserialize_with = "required_pattern")]
    pattern: Option<DeclarationPattern>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum IdentityConstraintKind {
    String {
        identity_layer: StringIdentityConstraints,
    },
    Int {
        identity_range: Bounds<i64>,
    },
    Float {
        range: Bounds<crate::FiniteF64>,
    },
}

fn unique_owner_layers<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, IdentityConstraintKind>, D::Error> {
    struct OwnerLayersVisitor;
    impl<'de> serde::de::Visitor<'de> for OwnerLayersVisitor {
        type Value = BTreeMap<String, IdentityConstraintKind>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("one identity layer per schema owner")
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut layers = BTreeMap::new();
            while let Some((owner, layer)) = map.next_entry::<String, IdentityConstraintKind>()? {
                if layers.insert(owner, layer).is_some() {
                    return Err(serde::de::Error::custom("duplicate owner layer"));
                }
            }
            Ok(layers)
        }
    }
    deserializer.deserialize_map(OwnerLayersVisitor)
}

impl IdentityConstraintKind {
    fn kind(&self) -> RefinedScalarKind {
        match self {
            Self::String { identity_layer: _ } => RefinedScalarKind::String,
            Self::Int { identity_range: _ } => RefinedScalarKind::Int,
            Self::Float { range: _ } => RefinedScalarKind::Float,
        }
    }

    fn normalize_float_bounds(&mut self) {
        if let Self::Float { range } = self {
            *range = normalize_float_bounds(*range);
        }
    }

    fn intersect(&mut self, other: &Self) {
        match (self, other) {
            (
                Self::String { identity_layer },
                Self::String {
                    identity_layer: other,
                },
            ) => identity_layer.intersect(other),
            (
                Self::Int { identity_range },
                Self::Int {
                    identity_range: other,
                },
            ) => identity_range.intersect(other),
            (Self::Float { range }, Self::Float { range: other }) => range.intersect(other),
            _ => unreachable!("annotation resolution checks scalar kinds before intersecting"),
        }
    }
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

    pub(super) fn identity_constraints(&self) -> IdentityConstraints {
        IdentityConstraints::new(match &self.scalar {
            ScalarConstraintKind::String {
                identity_layer,
                member_layer: _,
            } => IdentityConstraintKind::String {
                identity_layer: identity_layer.clone(),
            },
            ScalarConstraintKind::Int {
                identity_range,
                member_range: _,
            } => IdentityConstraintKind::Int {
                identity_range: *identity_range,
            },
            ScalarConstraintKind::Float { range } => {
                IdentityConstraintKind::Float { range: *range }
            }
        })
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

impl IdentityConstraints {
    fn new(scalar: IdentityConstraintKind) -> Self {
        let mut constraints = Self {
            scalar,
            empty_intersection: false,
            owner_layers: BTreeMap::new(),
        };
        constraints.empty_intersection = constraints.bounds_are_empty();
        constraints
    }

    pub(super) fn from_declaration(declaration: &CompiledAttributeType) -> Option<Self> {
        // Clone the compiled identity pattern directly; never carry member data
        // into annotation evidence, including enum/reference fallback lookups.
        Some(Self::new(match declaration.kind() {
            AttrTypeKind::String {
                identity_layer,
                member_layer: _,
            } => IdentityConstraintKind::String {
                identity_layer: StringIdentityConstraints {
                    length: Bounds::new(identity_layer.length),
                    pattern: identity_layer.pattern.clone().map(DeclarationPattern),
                },
            },
            AttrTypeKind::Reference {
                pattern, length, ..
            } => IdentityConstraintKind::String {
                identity_layer: StringIdentityConstraints {
                    length: Bounds::new(*length),
                    pattern: pattern.clone().map(DeclarationPattern),
                },
            },
            AttrTypeKind::Int {
                identity_layer,
                member_layer: _,
            } => IdentityConstraintKind::Int {
                identity_range: Bounds::new(identity_layer.range),
            },
            AttrTypeKind::Float { range, .. } => IdentityConstraintKind::Float {
                range: normalize_float_bounds(Bounds::new(*range)),
            },
            AttrTypeKind::Enum { base, .. } => return Self::from_declaration(base),
            _ => return None,
        }))
    }

    pub(super) fn kind(&self) -> RefinedScalarKind {
        match self.scalar {
            IdentityConstraintKind::String { .. } => RefinedScalarKind::String,
            IdentityConstraintKind::Int { .. } => RefinedScalarKind::Int,
            IdentityConstraintKind::Float { .. } => RefinedScalarKind::Float,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.empty_intersection
    }

    fn bounds_are_empty(&self) -> bool {
        match &self.scalar {
            IdentityConstraintKind::String { identity_layer } => identity_layer.length.is_empty(),
            IdentityConstraintKind::Int { identity_range } => identity_range.is_empty(),
            IdentityConstraintKind::Float { range } => range.is_empty(),
        }
    }

    pub(super) fn validate_record(
        &mut self,
        owners: &super::NonEmptySchemaTypeOwners,
    ) -> Result<(), &'static str> {
        if self.empty_intersection != self.bounds_are_empty() {
            return Err("scalar bounds and empty-intersection marker disagree");
        }
        self.scalar.normalize_float_bounds();
        if owners.is_singleton() {
            if !self.owner_layers.is_empty() {
                return Err("single-owner annotations must omit owner layers");
            }
        } else if !self
            .owner_layers
            .keys()
            .map(String::as_str)
            .eq(owners.iter().map(|owner| {
                let super::SchemaTypeOwner::Provider(owner) = owner;
                owner.as_str()
            }))
        {
            return Err("owner layers must match the annotation owner set exactly");
        }
        let mut expected: Option<IdentityConstraintKind> = None;
        let kind = self.kind();
        for layer in self.owner_layers.values_mut() {
            layer.normalize_float_bounds();
            if layer.kind() != kind {
                return Err("owner layer scalar kind does not match annotation");
            }
            match &mut expected {
                Some(expected) => expected.intersect(layer),
                None => expected = Some(layer.clone()),
            }
        }
        if expected.is_some_and(|expected| expected != self.scalar) {
            return Err("owner layer conjunction does not match scalar constraints");
        }
        Ok(())
    }

    fn layers_for(
        &self,
        owners: &super::NonEmptySchemaTypeOwners,
    ) -> BTreeMap<String, IdentityConstraintKind> {
        if !self.owner_layers.is_empty() {
            return self.owner_layers.clone();
        }
        assert!(
            owners.is_singleton(),
            "multi-owner annotation retains every owner layer"
        );
        let super::SchemaTypeOwner::Provider(owner) =
            owners.iter().next().expect("nonempty owners");
        BTreeMap::from([(owner.clone(), self.scalar.clone())])
    }

    pub(super) fn intersect(
        &mut self,
        owners: &super::NonEmptySchemaTypeOwners,
        other: &Self,
        other_owners: &super::NonEmptySchemaTypeOwners,
    ) {
        let all_owners = owners.union(other_owners);
        if !all_owners.is_singleton() {
            let mut layers = self.layers_for(owners);
            for (owner, incoming) in other.layers_for(other_owners) {
                layers
                    .entry(owner)
                    .and_modify(|layer| layer.intersect(&incoming))
                    .or_insert(incoming);
            }
            self.owner_layers = layers;
        }
        // Only identity constraints participate. Each owner's pattern remains
        // in the keyed layers; no annotation payload contains member slots.
        self.scalar.intersect(&other.scalar);
        self.empty_intersection = self.bounds_are_empty();
    }

    pub(super) fn owner_patterns(&self) -> impl Iterator<Item = &CompiledPattern> {
        self.owner_layers.values().filter_map(|layer| match layer {
            IdentityConstraintKind::String { identity_layer } => {
                identity_layer.pattern.as_ref().map(|pattern| &pattern.0)
            }
            IdentityConstraintKind::Int { identity_range: _ }
            | IdentityConstraintKind::Float { range: _ } => None,
        })
    }

    pub(super) fn materialize(&self, owned: &super::ResolvedSchemaType) -> CompiledAttributeType {
        let binding = crate::schema::RefinementBinding::Annotation(owned.clone());
        let kind = match &self.scalar {
            IdentityConstraintKind::String { identity_layer } => AttrTypeKind::String {
                identity_layer: crate::schema::StringIdentityLayer {
                    binding,
                    length: identity_layer.length.pair(),
                    pattern: identity_layer
                        .pattern
                        .as_ref()
                        .map(|pattern| pattern.0.clone()),
                    to_dsl: None,
                },
                member_layer: Default::default(),
            },
            IdentityConstraintKind::Int { identity_range } => AttrTypeKind::Int {
                identity_layer: crate::schema::IntIdentityLayer {
                    binding,
                    range: identity_range.pair(),
                },
                member_layer: Default::default(),
            },
            IdentityConstraintKind::Float { range } => AttrTypeKind::Float {
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
            let retained = ScalarConstraints::new(ScalarConstraintKind::String {
                identity_layer: StringIdentityConstraints {
                    length: Bounds::new(None),
                    pattern: Some(identity_pattern),
                },
                member_layer: StringMemberConstraints {
                    length: Bounds::new(None),
                    pattern: Some(member_pattern),
                },
            });
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
