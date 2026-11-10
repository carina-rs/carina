//! Scalar declaration evidence retained by resolved schema annotations.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{NonEmptySchemaTypeOwners, SchemaTypeOwner};
use crate::schema::{
    AttrTypeKind, CompiledAttributeType, CompiledPattern, PatternSpec, RefinedScalarKind,
};

/// Unlike an Option field, an endpoint cannot silently default to unbounded
/// when a wire record omits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// A sorted pattern set keyed by immutable specifications. Compiled regex
/// caches are values, so cache mutation cannot affect ordering or deduplication.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DeclarationPatterns(BTreeMap<PatternSpec, CompiledPattern>);

impl DeclarationPatterns {
    fn from_pattern(pattern: Option<&CompiledPattern>) -> Self {
        Self(
            pattern
                .into_iter()
                .map(|pattern| (pattern.as_ref().clone(), pattern.clone()))
                .collect(),
        )
    }

    fn intersect(&mut self, other: &Self) {
        self.0.extend(
            other
                .0
                .iter()
                .map(|(spec, pattern)| (spec.clone(), pattern.clone())),
        );
    }

    fn first(&self) -> Option<&CompiledPattern> {
        self.0.values().next()
    }

    fn iter(&self) -> impl Iterator<Item = &CompiledPattern> {
        self.0.values()
    }
}

impl Serialize for DeclarationPatterns {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.keys())
    }
}
impl<'de> Deserialize<'de> for DeclarationPatterns {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let specs = BTreeSet::<PatternSpec>::deserialize(deserializer)?;
        specs
            .into_iter()
            .map(|spec| {
                let compiled = CompiledPattern::try_new(&spec).map_err(serde::de::Error::custom)?;
                Ok((spec, compiled))
            })
            .collect::<Result<_, D::Error>>()
            .map(Self)
    }
}

/// Value-side scalar evidence has identity constraints only, by construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ValueConstraints {
    scalar: IdentityConstraintKind,
    empty_intersection: bool,
}

/// An authored annotation has either one owner or a checked map of at least two.
/// No form has member slots, and no multi-owner form can have an empty map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum IdentityConstraints {
    Single(SingleOwnerConstraints),
    Multi(MultiOwnerConstraints),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SingleOwnerConstraints {
    owner: SchemaTypeOwner,
    scalar: IdentityConstraintKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MultiOwnerConstraints {
    scalar: IdentityConstraintKind,
    layers: MultipleOwnerLayers,
}

// This type has no Default or unchecked conversion from a map. Construction
// either promotes distinct single owners or validates a complete wire owner set.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MultipleOwnerLayers {
    layers: BTreeMap<SchemaTypeOwner, IdentityConstraintKind>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
struct OwnerLayersWire(
    #[serde(with = "owner_layers_wire")] BTreeMap<SchemaTypeOwner, IdentityConstraintKind>,
);

impl OwnerLayersWire {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IdentityConstraintsWire {
    scalar: IdentityConstraintKind,
    empty_intersection: bool,
    #[serde(default, skip_serializing_if = "OwnerLayersWire::is_empty")]
    owner_layers: OwnerLayersWire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StringIdentityConstraints {
    length: Bounds<u64>,
    patterns: DeclarationPatterns,
}

impl StringIdentityConstraints {
    fn intersect(&mut self, other: &Self) {
        self.patterns.intersect(&other.patterns);
        self.length.intersect(&other.length);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

mod owner_layers_wire {
    use super::*;
    use serde::ser::SerializeMap;

    pub(super) fn serialize<S: serde::Serializer>(
        layers: &BTreeMap<SchemaTypeOwner, IdentityConstraintKind>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(layers.len()))?;
        for (owner, layer) in layers {
            map.serialize_entry(owner.provider_name(), layer)?;
        }
        map.end()
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<SchemaTypeOwner, IdentityConstraintKind>, D::Error> {
        struct OwnerLayersVisitor;
        impl<'de> serde::de::Visitor<'de> for OwnerLayersVisitor {
            type Value = BTreeMap<SchemaTypeOwner, IdentityConstraintKind>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("one identity layer per schema owner")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut layers = BTreeMap::new();
                while let Some((name, layer)) =
                    map.next_entry::<String, IdentityConstraintKind>()?
                {
                    let owner = SchemaTypeOwner::Provider(name);
                    if layers.insert(owner, layer).is_some() {
                        return Err(serde::de::Error::custom("duplicate owner layer"));
                    }
                }
                Ok(layers)
            }
        }
        deserializer.deserialize_map(OwnerLayersVisitor)
    }
}

impl IdentityConstraintKind {
    fn materialize(&self, owned: &super::ResolvedSchemaType) -> CompiledAttributeType {
        let binding = crate::schema::RefinementBinding::Annotation(owned.clone());
        let kind = match self {
            IdentityConstraintKind::String { identity_layer } => AttrTypeKind::String {
                identity_layer: crate::schema::StringIdentityLayer {
                    binding,
                    length: identity_layer.length.pair(),
                    pattern: identity_layer.patterns.first().cloned(),
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

    fn from_declaration(declaration: &CompiledAttributeType) -> Option<Self> {
        Some(match declaration.kind() {
            AttrTypeKind::String {
                identity_layer,
                member_layer: _,
            } => IdentityConstraintKind::String {
                identity_layer: StringIdentityConstraints {
                    length: Bounds::new(identity_layer.length),
                    patterns: DeclarationPatterns::from_pattern(identity_layer.pattern.as_ref()),
                },
            },
            AttrTypeKind::Reference {
                pattern, length, ..
            } => IdentityConstraintKind::String {
                identity_layer: StringIdentityConstraints {
                    length: Bounds::new(*length),
                    patterns: DeclarationPatterns::from_pattern(pattern.as_ref()),
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
        })
    }

    fn patterns(&self) -> impl Iterator<Item = &CompiledPattern> {
        let patterns = match self {
            Self::String { identity_layer } => Some(&identity_layer.patterns),
            Self::Int { identity_range: _ } | Self::Float { range: _ } => None,
        };
        patterns.into_iter().flat_map(DeclarationPatterns::iter)
    }

    fn kind(&self) -> RefinedScalarKind {
        match self {
            Self::String { identity_layer: _ } => RefinedScalarKind::String,
            Self::Int { identity_range: _ } => RefinedScalarKind::Int,
            Self::Float { range: _ } => RefinedScalarKind::Float,
        }
    }

    fn bounds_are_empty(&self) -> bool {
        match self {
            Self::String { identity_layer } => identity_layer.length.is_empty(),
            Self::Int { identity_range } => identity_range.is_empty(),
            Self::Float { range } => range.is_empty(),
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

impl ValueConstraints {
    fn new(scalar: IdentityConstraintKind) -> Self {
        let empty_intersection = scalar.bounds_are_empty();
        Self {
            scalar,
            empty_intersection,
        }
    }

    pub(super) fn from_declaration(declaration: &CompiledAttributeType) -> Option<Self> {
        Some(Self::new(IdentityConstraintKind::from_declaration(
            declaration,
        )?))
    }

    pub(super) fn kind(&self) -> RefinedScalarKind {
        self.scalar.kind()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.scalar.bounds_are_empty()
    }

    pub(super) fn validate_record(&mut self) -> Result<(), &'static str> {
        self.scalar.normalize_float_bounds();
        if self.empty_intersection != self.scalar.bounds_are_empty() {
            return Err("scalar bounds and empty-intersection marker disagree");
        }
        Ok(())
    }

    pub(super) fn identity_constraints(&self, owner: SchemaTypeOwner) -> IdentityConstraints {
        IdentityConstraints::single(owner, self.scalar.clone())
    }

    pub(super) fn patterns(&self) -> impl Iterator<Item = &CompiledPattern> {
        self.scalar.patterns()
    }

    pub(super) fn materialize(&self, owned: &super::ResolvedSchemaType) -> CompiledAttributeType {
        self.scalar.materialize(owned)
    }
}

impl IdentityConstraints {
    fn single(owner: SchemaTypeOwner, scalar: IdentityConstraintKind) -> Self {
        Self::Single(SingleOwnerConstraints { owner, scalar })
    }

    pub(super) fn from_declaration(
        owner: SchemaTypeOwner,
        declaration: &CompiledAttributeType,
    ) -> Option<Self> {
        Some(Self::single(
            owner,
            IdentityConstraintKind::from_declaration(declaration)?,
        ))
    }

    fn scalar(&self) -> &IdentityConstraintKind {
        match self {
            Self::Single(single) => &single.scalar,
            Self::Multi(multi) => &multi.scalar,
        }
    }

    pub(super) fn kind(&self) -> RefinedScalarKind {
        self.scalar().kind()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.scalar().bounds_are_empty()
    }

    pub(super) fn owners(&self) -> NonEmptySchemaTypeOwners {
        match self {
            Self::Single(single) => NonEmptySchemaTypeOwners::singleton(single.owner.clone()),
            Self::Multi(multi) => {
                NonEmptySchemaTypeOwners::try_from_iter(multi.layers.layers.keys().cloned())
                    .expect("multi-owner layers have at least two keys by construction")
            }
        }
    }

    pub(super) fn to_wire(&self) -> IdentityConstraintsWire {
        IdentityConstraintsWire {
            scalar: self.scalar().clone(),
            empty_intersection: self.is_empty(),
            owner_layers: OwnerLayersWire(match self {
                Self::Single(_) => BTreeMap::new(),
                Self::Multi(multi) => multi.layers.layers.clone(),
            }),
        }
    }

    pub(super) fn conjoin(&self, other: &Self) -> Self {
        let mut joined = self.clone();
        match other {
            Self::Single(single) => joined.insert(single.owner.clone(), single.scalar.clone()),
            Self::Multi(multi) => {
                for (owner, layer) in &multi.layers.layers {
                    joined.insert(owner.clone(), layer.clone());
                }
            }
        }
        joined
    }

    fn insert(&mut self, owner: SchemaTypeOwner, incoming: IdentityConstraintKind) {
        match self {
            Self::Single(single) if single.owner == owner => single.scalar.intersect(&incoming),
            Self::Single(single) => {
                let mut scalar = single.scalar.clone();
                scalar.intersect(&incoming);
                let layers = BTreeMap::from([
                    (single.owner.clone(), single.scalar.clone()),
                    (owner, incoming),
                ]);
                // The unequal-owner arm supplies two distinct keys. Later
                // conjoins can insert or intersect entries, never remove them.
                *self = Self::Multi(MultiOwnerConstraints {
                    scalar,
                    layers: MultipleOwnerLayers { layers },
                });
            }
            Self::Multi(multi) => {
                multi.scalar.intersect(&incoming);
                multi
                    .layers
                    .layers
                    .entry(owner)
                    .and_modify(|layer| layer.intersect(&incoming))
                    .or_insert(incoming);
            }
        }
    }

    pub(super) fn owner_patterns(&self) -> impl Iterator<Item = &CompiledPattern> {
        self.scalar().patterns()
    }

    pub(super) fn materialize(&self, owned: &super::ResolvedSchemaType) -> CompiledAttributeType {
        self.scalar().materialize(owned)
    }
}

impl IdentityConstraintsWire {
    pub(super) fn bind(
        mut self,
        owners: &NonEmptySchemaTypeOwners,
    ) -> Result<IdentityConstraints, &'static str> {
        self.scalar.normalize_float_bounds();
        let constraints = if let Some(owner) = owners.single_owner() {
            if !self.owner_layers.is_empty() {
                return Err("single-owner annotations must omit owner layers");
            }
            IdentityConstraints::single(owner.clone(), self.scalar)
        } else {
            let mut layers = self.owner_layers.0;
            if !layers.keys().eq(owners.iter()) {
                return Err("owner layers must match the annotation owner set exactly");
            }
            let kind = self.scalar.kind();
            let mut expected: Option<IdentityConstraintKind> = None;
            for layer in layers.values_mut() {
                layer.normalize_float_bounds();
                if layer.kind() != kind {
                    return Err("owner layer scalar kind does not match annotation");
                }
                match &mut expected {
                    Some(expected) => expected.intersect(layer),
                    None => expected = Some(layer.clone()),
                }
            }
            if expected.as_ref() != Some(&self.scalar) {
                return Err("owner layer conjunction does not match scalar constraints");
            }
            // The nonempty owner set was not a singleton, and the keys agree
            // exactly. The private map therefore contains at least two owners.
            IdentityConstraints::Multi(MultiOwnerConstraints {
                scalar: self.scalar,
                layers: MultipleOwnerLayers { layers },
            })
        };
        if self.empty_intersection != constraints.is_empty() {
            return Err("scalar bounds and empty-intersection marker disagree");
        }
        Ok(constraints)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_owner_conjunctions_round_trip() {
        use crate::parser::ResolvedSchemaType;
        use crate::schema::{AttributeType, TypeIdentity, ValidatedConfigSchema};

        let id = TypeIdentity::from_dotted("test.Generated");
        let intervals: [(Option<u32>, Option<u32>); 5] = [
            (None, None),
            (None, Some(0)),
            (Some(1), Some(4)),
            (Some(8), Some(12)),
            (Some(16), None),
        ];
        for kind in [
            RefinedScalarKind::String,
            RefinedScalarKind::Int,
            RefinedScalarKind::Float,
        ] {
            let mut drafts: Vec<_> = intervals
                .into_iter()
                .map(|(min, max)| match kind {
                    RefinedScalarKind::String => AttributeType::refined_string(
                        Some(id.clone()),
                        None,
                        Some((min.map(u64::from), max.map(u64::from))),
                        None,
                    ),
                    RefinedScalarKind::Int => AttributeType::refined_int(
                        Some(id.clone()),
                        Some((min.map(i64::from), max.map(i64::from))),
                    ),
                    RefinedScalarKind::Float => AttributeType::refined_float(
                        Some(id.clone()),
                        Some((
                            min.map(|n| crate::finite(f64::from(n))),
                            max.map(|n| crate::finite(f64::from(n))),
                        )),
                    ),
                })
                .collect();
            if kind == RefinedScalarKind::Float {
                for (min, max) in [(-0.0, 0.0), (0.0, -0.0), (-4.0, -1.0)] {
                    drafts.push(AttributeType::refined_float(
                        Some(id.clone()),
                        Some((Some(crate::finite(min)), Some(crate::finite(max)))),
                    ));
                }
            }
            // Compile separately: differing String/Int identity definitions
            // are intentionally not admissible in one provider schema.
            let declarations: Vec<_> = drafts
                .into_iter()
                .map(|draft| {
                    let schema =
                        ValidatedConfigSchema::validate(BTreeMap::from([("value".into(), draft)]))
                            .unwrap();
                    schema.parts()["value"].clone()
                })
                .collect();
            for first in &declarations {
                for second in &declarations {
                    for second_owner in ["a", "b"] {
                        let a =
                            ResolvedSchemaType::from_annotation_declaration("a", first).unwrap();
                        let b =
                            ResolvedSchemaType::from_annotation_declaration(second_owner, second)
                                .unwrap();
                        let c =
                            ResolvedSchemaType::from_annotation_declaration("c", first).unwrap();
                        let ab = a.conjoin(&b);
                        for joined in [
                            a.clone(),
                            ab.clone(),
                            ab.conjoin(&a),
                            ab.conjoin(&c),
                            a.conjoin(&c).conjoin(&b),
                        ] {
                            let wire = serde_json::to_value(&joined).unwrap();
                            let restored: ResolvedSchemaType = serde_json::from_value(wire.clone())
                                .unwrap_or_else(|error| panic!("{kind:?}: {error}: {wire}"));
                            assert_eq!(restored, joined);
                            assert_eq!(serde_json::to_value(restored).unwrap(), wire);
                            if kind == RefinedScalarKind::Float {
                                let canonical = serde_json::to_string(&joined).unwrap();
                                let signed =
                                    canonical.replace("\"Inclusive\":0.0", "\"Inclusive\":-0.0");
                                let normalized: ResolvedSchemaType =
                                    serde_json::from_str(&signed).unwrap();
                                assert_eq!(serde_json::to_string(&normalized).unwrap(), canonical);
                            }
                            let mut tampered = wire;
                            let marker = tampered["constraints"]["empty_intersection"]
                                .as_bool()
                                .unwrap();
                            tampered["constraints"]["empty_intersection"] =
                                serde_json::json!(!marker);
                            assert!(
                                serde_json::from_value::<ResolvedSchemaType>(tampered).is_err()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn deserialized_declaration_bounds_normalize_both_zero_signs() {
        let json = r#"{"scalar":{"Float":{"range":{"min":{"Inclusive":-0.0},"max":{"Inclusive":0.0}}}},"empty_intersection":false}"#;
        let mut constraints: ValueConstraints = serde_json::from_str(json).unwrap();
        constraints.validate_record().unwrap();
        let IdentityConstraintKind::Float { range } = constraints.scalar else {
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
            assert!(serde_json::from_str::<ValueConstraints>(&json.replace("-0.0", raw)).is_err());
        }
    }
}

#[cfg(test)]
mod pattern_mode_tests {
    use super::*;
    use crate::schema::{PatternMatch, PatternReason, PatternSpec};

    #[test]
    fn value_identity_patterns_preserve_explicit_match_modes() {
        for mode in [
            PatternMatch::Full,
            PatternMatch::Substring {
                reason: PatternReason::new("an intentional substring constraint").unwrap(),
            },
        ] {
            let pattern = CompiledPattern::try_new(&PatternSpec::new("abc", mode.clone())).unwrap();
            let retained = ValueConstraints::new(IdentityConstraintKind::String {
                identity_layer: StringIdentityConstraints {
                    length: Bounds::new(None),
                    patterns: DeclarationPatterns::from_pattern(Some(&pattern)),
                },
            });
            let json = serde_json::to_string(&retained).unwrap();
            let decoded: ValueConstraints = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, retained);
            let IdentityConstraintKind::String { identity_layer } = decoded.scalar else {
                panic!("String identity")
            };
            assert_eq!(
                identity_layer.patterns.first().unwrap().is_match("xabcx"),
                matches!(mode, PatternMatch::Substring { .. })
            );
        }
    }

    fn round11_pattern_annotation(owner: &str, regex: &str) -> super::super::ResolvedSchemaType {
        use crate::schema::{
            AttributeType, PatternMatch, PatternSpec, TypeIdentity, ValidatedConfigSchema,
        };
        let schema = ValidatedConfigSchema::validate(BTreeMap::from([(
            "x".into(),
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("test.X")),
                Some(PatternSpec::new(regex, PatternMatch::Full)),
                None,
                None,
            ),
        )]))
        .unwrap();
        super::super::ResolvedSchemaType::from_annotation_declaration(owner, &schema.parts()["x"])
            .unwrap()
    }

    #[test]
    fn round11_repeated_single_owner_patterns_remain_a_conjunction() {
        use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};
        let a = round11_pattern_annotation("same", "^a.*$");
        let b = round11_pattern_annotation("same", "^.*z$");
        let joined = a.conjoin(&b).conjoin(&a);
        let patterns: Vec<_> = joined.owner_patterns().collect();
        assert_eq!(
            patterns.len(),
            2,
            "retain and deduplicate every owner pattern"
        );
        assert!(patterns.windows(2).all(|pair| pair[0] < pair[1]));
        let declaration = joined.materialize();
        assert!(
            declaration
                .validate(&Value::Concrete(ConcreteValue::String("az".into())))
                .is_ok()
        );
        for bad in ["aa", "zz"] {
            assert!(
                declaration
                    .validate(&Value::Concrete(ConcreteValue::String(bad.into())))
                    .is_err(),
                "{bad}"
            );
        }
        let wire = serde_json::to_value(&joined).unwrap();
        let decoded: super::super::ResolvedSchemaType = serde_json::from_value(wire).unwrap();
        assert_eq!(joined, decoded);
        assert_eq!(joined, b.conjoin(&a));
    }

    #[test]
    fn round11_annotation_names_render_every_owner_pattern() {
        use crate::schema::TypeInSchema;
        let a = round11_pattern_annotation("a", "^a.*$");
        let b = round11_pattern_annotation("b", "^z.*$");
        let c = round11_pattern_annotation("b", "^y.*$");
        let ab = a.conjoin(&b).materialize();
        let ac = a.conjoin(&c).materialize();
        let (expected, actual) =
            TypeInSchema::schemaless(&ab).mismatch_type_names(TypeInSchema::schemaless(&ac));
        assert_ne!(expected, actual, "different owner patterns must be visible");
        assert!(
            actual.contains("^a.*$") && actual.contains("^z.*$"),
            "{actual}"
        );
        assert!(
            expected.contains("^a.*$") && expected.contains("^y.*$"),
            "{expected}"
        );
    }
}
