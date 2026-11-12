//! Typed predicate AST for the `wait` construct.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md` for the surface
//! design. The MVP supports `<target>.<attr> == <value>` only; the enum
//! is shaped to grow (`NotEquals`, `And`, `Or`, comparisons, `In`)
//! without breaking call sites.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::resource::{
    AbsoluteBindingAddress, BindingAddress, ConcreteValue, NoPlaceholder, Value,
};
use crate::schema::{AttributePathTypeError, ValidatedResourceSchema};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AttrPathError {
    #[error("AttrPath segments cannot be empty")]
    Empty,
}

/// Dotted path into a target's attribute tree.
///
/// Resolution walks the path one segment at a time, descending into
/// nested `ConcreteValue::Map` values. For example, an `AttrPath`
/// with `segments = ["renewal_summary", "renewal_status"]` resolves
/// to `attrs["renewal_summary"]["renewal_status"]`.
///
/// The `segments` field is private to make the empty-path state
/// unrepresentable: construct via `AttrPath::single` for a single
/// top-level attribute or `AttrPath::try_new` for a multi-segment
/// path. Serde deserialization also rejects empty segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "AttrPathSerde", try_from = "AttrPathSerde")]
pub struct AttrPath {
    segments: Vec<String>,
}

impl AttrPath {
    pub fn single(name: impl Into<String>) -> Self {
        Self {
            segments: vec![name.into()],
        }
    }

    pub fn try_new(segments: Vec<String>) -> Result<Self, AttrPathError> {
        if segments.is_empty() {
            return Err(AttrPathError::Empty);
        }
        Ok(Self { segments })
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// Walk this path into `attrs`, descending through nested
    /// `ConcreteValue::Map` values as needed. Returns the leaf `Value`
    /// or `None` if any segment is missing or not a map at a non-leaf.
    pub(crate) fn resolve<'a, P: crate::resource::ValueKind, B: BindingAddress>(
        &self,
        attrs: &'a HashMap<String, Value<P, B>>,
    ) -> Option<&'a Value<P, B>> {
        let (first, rest) = self.segments().split_first()?;
        let mut current = attrs.get(first)?;
        for segment in rest {
            match current {
                Value::Concrete(ConcreteValue::Map(map)) => {
                    current = map.get(segment)?;
                }
                _ => return None,
            }
        }
        Some(current)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttrPathSerde {
    segments: Vec<String>,
}

impl From<AttrPath> for AttrPathSerde {
    fn from(p: AttrPath) -> Self {
        Self {
            segments: p.segments,
        }
    }
}

impl TryFrom<AttrPathSerde> for AttrPath {
    type Error = AttrPathError;

    fn try_from(p: AttrPathSerde) -> Result<Self, Self::Error> {
        AttrPath::try_new(p.segments)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WaitPredicateError {
    #[error("invalid wait operand")]
    UnsupportedOperand,
    #[error("wait string requires a checked literal")]
    UncheckedString,
    #[error("unresolved enum identifier `{identifier}`")]
    UnresolvedEnumIdentifier {
        identifier: crate::resource::SecretDiagnosticError<String>,
    },
}

impl From<std::convert::Infallible> for WaitPredicateError {
    fn from(value: std::convert::Infallible) -> Self {
        match value {}
    }
}

/// Context-rich failure produced while resolving and sealing a parsed wait
/// predicate. Every plan/validate/LSP caller renders this same value, keeping
/// both path spelling and remediation text identical.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WaitResolutionError {
    #[error("invalid wait operand")]
    UnsupportedOperand,
    #[error("wait string requires a checked literal")]
    UncheckedString,
    #[error("invalid predicate attribute path: {0}")]
    InvalidPath(AttrPathError),
    #[error("`until` references unknown attribute `{target}.{attribute}` on `{resource_type}`")]
    UnknownAttribute {
        target: String,
        attribute: String,
        resource_type: String,
    },
    #[error("`until` references unknown field `{target}.{full_path}` on struct `{struct_name}`")]
    UnknownStructField {
        target: String,
        full_path: String,
        struct_name: String,
    },
    #[error(
        "cannot resolve wait attribute path `{target}.{full_path}`: `{target}.{traversed}` has type {type_name}; only struct fields and map values can be traversed"
    )]
    CannotTraverse {
        target: String,
        full_path: String,
        traversed: String,
        type_name: String,
    },
    #[error(
        "cannot resolve `{identifier}` as an enum value of `{target}.{attribute}`; quote it to compare as a string"
    )]
    UnresolvedEnumIdentifier {
        target: String,
        attribute: String,
        identifier: crate::resource::SecretDiagnosticError<String>,
    },
}

impl WaitResolutionError {
    pub fn rhs_anchor(&self) -> Option<&str> {
        match self {
            Self::UnresolvedEnumIdentifier {
                identifier: crate::resource::SecretDiagnosticError::Public(identifier),
                ..
            } => Some(identifier),
            Self::UnresolvedEnumIdentifier {
                identifier: crate::resource::SecretDiagnosticError::InsideSecret,
                ..
            } => Some(crate::schema::SECRET_VALUE_MASK),
            Self::UncheckedString
            | Self::UnsupportedOperand
            | Self::InvalidPath(_)
            | Self::UnknownAttribute { .. }
            | Self::UnknownStructField { .. }
            | Self::CannotTraverse { .. } => None,
        }
    }
}

/// Resolve a parsed wait predicate against the type at its complete LHS path,
/// then seal it so no unresolved enum identifier can reach polling.
///
/// Only an explicitly typed [`ConcreteValue::EnumIdentifier`] is eligible for
/// enum canonicalization. A quoted [`ConcreteValue::String`] remains literal,
/// even when the target attribute has an enum schema.
pub fn resolve_wait_predicate<B: BindingAddress>(
    root: &B,
    attribute_segments: &[String],
    value: WaitRhs<B>,
    schema: Option<&ValidatedResourceSchema>,
) -> Result<WaitPredicate<B>, WaitResolutionError> {
    let target = root.as_str();
    let attr =
        AttrPath::try_new(attribute_segments.to_vec()).map_err(WaitResolutionError::InvalidPath)?;
    let attribute = attr.segments().join(".");
    let mut resolved = value;

    if let Some(schema) = schema {
        let effective_type = schema
            .effective_type_at_attribute_path(attr.segments())
            .map_err(|error| match error {
                AttributePathTypeError::Empty => {
                    WaitResolutionError::InvalidPath(AttrPathError::Empty)
                }
                AttributePathTypeError::UnknownAttribute { attribute } => {
                    WaitResolutionError::UnknownAttribute {
                        target: target.to_string(),
                        attribute,
                        resource_type: schema.as_schema().resource_type().to_string(),
                    }
                }
                AttributePathTypeError::UnknownStructField {
                    parent_path,
                    field,
                    struct_name,
                } => WaitResolutionError::UnknownStructField {
                    target: target.to_string(),
                    full_path: format!("{parent_path}.{field}"),
                    struct_name,
                },
                AttributePathTypeError::CannotTraverse { path, type_name } => {
                    WaitResolutionError::CannotTraverse {
                        target: target.to_string(),
                        full_path: attribute.clone(),
                        traversed: path,
                        type_name,
                    }
                }
            })?;
        if matches!(resolved.operand, WaitOperand::Identifier(_)) {
            resolved = WaitRhs::try_from(crate::value::canonicalize_wait_rhs_with_type(
                resolved.to_value(),
                &effective_type,
            ))
            .map_err(|_| WaitResolutionError::UnsupportedOperand)?;
        }
    }

    WaitPredicate::equals(attr, resolved).map_err(|error| match error {
        WaitPredicateError::UncheckedString => WaitResolutionError::UncheckedString,
        WaitPredicateError::UnsupportedOperand => WaitResolutionError::UnsupportedOperand,
        WaitPredicateError::UnresolvedEnumIdentifier { identifier } => {
            WaitResolutionError::UnresolvedEnumIdentifier {
                target: target.to_string(),
                attribute,
                identifier,
            }
        }
    })
}

/// Validate a parser-stage wait RHS without converting the surrounding value
/// tree to the post-expansion stage.
pub(crate) fn validate_wait_predicate<B: BindingAddress>(
    root: &B,
    attribute_segments: &[String],
    value: &WaitRhs<B>,
    schema: Option<&ValidatedResourceSchema>,
) -> Result<(), WaitResolutionError> {
    resolve_wait_predicate(root, attribute_segments, value.clone(), schema).map(|_| ())
}

/// A resolved wait operand. Its variants are the scalar results of
/// `lower_until_rhs` and schema enum resolution, never a general Value tree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum WaitScalar {
    String(crate::parser::ValidateStringLiteral),
    Int(i64),
    Float(carina_provider_protocol::FiniteF64),
    Bool(bool),
    #[serde(with = "crate::resource::duration_secs")]
    Duration(std::time::Duration),
    CanonicalEnum(crate::resource::CanonicalEnumValue),
}

impl WaitScalar {
    fn to_value<B: BindingAddress>(&self) -> Value<NoPlaceholder, B> {
        Value::Concrete(match self {
            Self::String(literal) => ConcreteValue::String(literal.as_str().into()),
            Self::Int(value) => ConcreteValue::Int(*value),
            Self::Float(value) => ConcreteValue::Float(*value),
            Self::Bool(value) => ConcreteValue::Bool(*value),
            Self::Duration(value) => ConcreteValue::Duration(*value),
            Self::CanonicalEnum(value) => ConcreteValue::CanonicalEnum(value.clone()),
        })
    }
}

// RawEnumIdentifier is shared with other expression paths and accepts arbitrary
// text. Wait identifiers must instead be dotted field names from the grammar.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
struct WaitEnumIdentifier(crate::resource::RawEnumIdentifier);

impl TryFrom<crate::resource::RawEnumIdentifier> for WaitEnumIdentifier {
    type Error = WaitPredicateError;

    fn try_from(identifier: crate::resource::RawEnumIdentifier) -> Result<Self, Self::Error> {
        if crate::parser::is_wait_enum_identifier(identifier.as_str()) {
            Ok(Self(identifier))
        } else {
            Err(WaitPredicateError::UnsupportedOperand)
        }
    }
}

impl<'de> Deserialize<'de> for WaitEnumIdentifier {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let identifier = crate::resource::RawEnumIdentifier::deserialize(deserializer)?;
        Self::try_from(identifier).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum WaitIdentifier {
    EnumIdentifier(WaitEnumIdentifier),
}

// Both alternatives are closed enums. There is no general Value serde fallback.
// Untagged serialization preserves the existing externally tagged scalar bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum WaitOperand {
    Scalar(WaitScalar),
    Identifier(WaitIdentifier),
}

// The core has no project path. Saved-plan callers add scoped recovery guidance.
const INVALID_WAIT_OPERAND: &str = "invalid wait operand";

/// A resolved scalar wait value, sealed against unresolved enum identifiers,
/// interpolation, references, secrets, unknowns, and collection values.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WaitPredicateValue<B: BindingAddress = AbsoluteBindingAddress> {
    scalar: WaitScalar,
    #[serde(skip)]
    binding: std::marker::PhantomData<fn() -> B>,
}

/// The closed set of wait RHS shapes admitted by the parser and enum resolver.
/// Binding phases are tracked without storing any binding-bearing Value.
#[derive(Debug, Clone, PartialEq)]
pub struct WaitRhs<B: BindingAddress = AbsoluteBindingAddress> {
    operand: WaitOperand,
    binding: std::marker::PhantomData<fn() -> B>,
}

impl<B: BindingAddress> WaitRhs<B> {
    pub fn literal(literal: crate::parser::ValidateStringLiteral) -> Self {
        Self::from_operand(WaitOperand::Scalar(WaitScalar::String(literal)))
    }

    fn from_operand(operand: WaitOperand) -> Self {
        Self {
            operand,
            binding: std::marker::PhantomData,
        }
    }

    pub fn to_value(&self) -> Value<NoPlaceholder, B> {
        match &self.operand {
            WaitOperand::Scalar(scalar) => scalar.to_value(),
            WaitOperand::Identifier(WaitIdentifier::EnumIdentifier(identifier)) => {
                Value::Concrete(ConcreteValue::EnumIdentifier(identifier.0.clone()))
            }
        }
    }

    pub(crate) fn rebind<C: BindingAddress>(self) -> WaitRhs<C> {
        WaitRhs::from_operand(self.operand)
    }
}

impl<B: BindingAddress> PartialEq<Value<NoPlaceholder, B>> for WaitRhs<B> {
    fn eq(&self, other: &Value<NoPlaceholder, B>) -> bool {
        self.to_value() == *other
    }
}

impl<B: BindingAddress> TryFrom<Value<NoPlaceholder, B>> for WaitRhs<B> {
    type Error = WaitPredicateError;

    fn try_from(value: Value<NoPlaceholder, B>) -> Result<Self, Self::Error> {
        let scalar = match value {
            Value::Concrete(ConcreteValue::String(_)) => {
                return Err(WaitPredicateError::UncheckedString);
            }
            Value::Concrete(ConcreteValue::EnumIdentifier(identifier)) => {
                return Ok(Self::from_operand(WaitOperand::Identifier(
                    WaitIdentifier::EnumIdentifier(identifier.try_into()?),
                )));
            }
            Value::Concrete(ConcreteValue::Int(value)) => WaitScalar::Int(value),
            Value::Concrete(ConcreteValue::Float(value)) => WaitScalar::Float(value),
            Value::Concrete(ConcreteValue::Bool(value)) => WaitScalar::Bool(value),
            Value::Concrete(ConcreteValue::Duration(value)) => WaitScalar::Duration(value),
            Value::Concrete(ConcreteValue::CanonicalEnum(value)) => {
                WaitScalar::CanonicalEnum(value)
            }
            _ => return Err(WaitPredicateError::UnsupportedOperand),
        };
        Ok(Self::from_operand(WaitOperand::Scalar(scalar)))
    }
}

impl<B: BindingAddress> Serialize for WaitRhs<B> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.operand.serialize(serializer)
    }
}

impl<'de, B: BindingAddress> Deserialize<'de> for WaitRhs<B> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        WaitOperand::deserialize(deserializer)
            .map(Self::from_operand)
            .map_err(|_| serde::de::Error::custom(INVALID_WAIT_OPERAND))
    }
}

impl<B: BindingAddress> WaitPredicateValue<B> {
    pub fn to_value(&self) -> Value<NoPlaceholder, B> {
        self.scalar.to_value()
    }
}

impl<B: BindingAddress> TryFrom<Value<NoPlaceholder, B>> for WaitPredicateValue<B> {
    type Error = WaitPredicateError;

    fn try_from(value: Value<NoPlaceholder, B>) -> Result<Self, Self::Error> {
        Self::try_from(WaitRhs::try_from(value)?)
    }
}

impl<B: BindingAddress> TryFrom<WaitRhs<B>> for WaitPredicateValue<B> {
    type Error = WaitPredicateError;

    fn try_from(value: WaitRhs<B>) -> Result<Self, Self::Error> {
        match value.operand {
            WaitOperand::Scalar(scalar) => Ok(Self {
                scalar,
                binding: std::marker::PhantomData,
            }),
            WaitOperand::Identifier(WaitIdentifier::EnumIdentifier(identifier)) => {
                Err(WaitPredicateError::UnresolvedEnumIdentifier {
                    identifier: crate::resource::SecretDiagnosticError::Public(
                        identifier.0.as_str().into(),
                    ),
                })
            }
        }
    }
}

impl<'de, B: BindingAddress> Deserialize<'de> for WaitPredicateValue<B> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = WaitRhs::<B>::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

/// Typed predicate AST evaluated against a target's attribute snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitPredicate<B: BindingAddress = AbsoluteBindingAddress> {
    Equals {
        attr: AttrPath,
        value: WaitPredicateValue<B>,
    },
}

impl<B: BindingAddress> WaitPredicate<B> {
    pub fn equals<V>(attr: AttrPath, value: V) -> Result<Self, WaitPredicateError>
    where
        V: TryInto<WaitRhs<B>>,
        WaitPredicateError: From<V::Error>,
    {
        Ok(Self::Equals {
            attr,
            value: WaitPredicateValue::try_from(
                value.try_into().map_err(WaitPredicateError::from)?,
            )?,
        })
    }

    pub fn value(&self) -> Value<NoPlaceholder, B> {
        match self {
            Self::Equals { value, .. } => value.to_value(),
        }
    }

    /// Attribute paths referenced by this predicate, in evaluation
    /// order. Heartbeat consumers use this to show the operator the
    /// value the wait is actually polling. Future predicate variants
    /// (And, Or, NotEquals, comparisons) extend this list.
    pub fn watched_attrs(&self) -> Vec<&AttrPath> {
        match self {
            WaitPredicate::Equals { attr, .. } => vec![attr],
        }
    }

    /// Evaluate against the target's `read()` attribute map. Returns
    /// `true` when the predicate is satisfied; missing attributes and
    /// type mismatches both yield `false` (the wait keeps polling).
    pub fn evaluate(&self, attrs: &HashMap<String, Value<NoPlaceholder, B>>) -> bool {
        match self {
            WaitPredicate::Equals { attr, value } => attr
                .resolve(attrs)
                .is_some_and(|observed| wait_values_equal(observed, &value.to_value())),
        }
    }
}

/// Compare a schema-canonical wait value with a provider observation.
///
/// Canonical enums retain their identity in plans and saved plans, while the
/// provider ABI returns their API spelling as an ordinary string. Only a typed
/// expected value authorizes case-insensitive enum comparison. An expected
/// string is a quoted literal and therefore compares exactly, regardless of
/// whether the observed API value has already been lifted to `CanonicalEnum`.
fn wait_values_equal<B: BindingAddress>(
    observed: &Value<NoPlaceholder, B>,
    expected: &Value<NoPlaceholder, B>,
) -> bool {
    match (observed, expected) {
        (
            Value::Concrete(ConcreteValue::String(observed)),
            Value::Concrete(ConcreteValue::CanonicalEnum(expected)),
        ) => crate::resource::enum_values_equal(observed.expose_plaintext(), expected.api_value()),
        (
            Value::Concrete(ConcreteValue::CanonicalEnum(observed)),
            Value::Concrete(ConcreteValue::String(expected)),
        ) => observed.api_value() == expected.expose_plaintext(),
        _ => observed == expected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::ExpandedConcreteValue as ConcreteValue;
    use crate::resource::{DeferredValue, InterpolationPart};
    type Value = crate::resource::ExpandedValue;
    use std::collections::HashMap;

    #[test]
    fn round2_wait_decode_accepts_only_authored_scalar_shapes() {
        let config = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        for (source, expected_wire) in [
            ("'ready'", serde_json::json!({"String": "'ready'"})),
            (
                r#""line\n\${literal}""#,
                serde_json::json!({"String": r#""line\n\${literal}""#}),
            ),
            ("42", serde_json::json!({"Int": 42})),
            ("-7", serde_json::json!({"Int": -7})),
            ("1.25", serde_json::json!({"Float": 1.25})),
            ("true", serde_json::json!({"Bool": true})),
            ("false", serde_json::json!({"Bool": false})),
            ("2min", serde_json::json!({"Duration": 120})),
            ("ready", serde_json::json!({"EnumIdentifier": "ready"})),
            (
                "aws.acm.Certificate.Status.Issued",
                serde_json::json!({"EnumIdentifier": "aws.acm.Certificate.Status.Issued"}),
            ),
        ] {
            let parsed = crate::parser::parse_file(
                &format!("let w = wait r {{ until = r.status == {source} }}"),
                std::path::Path::new("main.crn"),
                &config,
            )
            .unwrap();
            let rhs = &parsed.wait_bindings[0].until_predicate.rhs;
            let wire = serde_json::to_string(rhs).unwrap();
            assert_eq!(wire, serde_json::to_string(&expected_wire).unwrap());
            let decoded: WaitRhs<crate::resource::RelativeBindingName> =
                serde_json::from_str(&wire).unwrap();
            assert_eq!(&decoded, rhs);
            assert_eq!(serde_json::to_string(&decoded).unwrap(), wire);
            if !matches!(
                rhs.to_value(),
                crate::resource::Value::Concrete(crate::resource::ConcreteValue::EnumIdentifier(_))
            ) {
                let predicate = WaitPredicate::equals(AttrPath::single("status"), decoded).unwrap();
                let observed = rhs.to_value();
                assert!(predicate.evaluate(&HashMap::from([("status".to_string(), observed)])));
                let wire = serde_json::to_string(&predicate).unwrap();
                let decoded: WaitPredicate<crate::resource::RelativeBindingName> =
                    serde_json::from_str(&wire).unwrap();
                assert_eq!(serde_json::to_string(&decoded).unwrap(), wire);
            }
        }
        let interpolation =
            serde_json::json!({"Interpolation": [{"Literal":"${secret('CANARY_4386')}"}]});
        for wire in [
            interpolation.clone(),
            serde_json::json!({"List": [interpolation.clone()]}),
            serde_json::json!({"Map": {"CANARY_4386": interpolation.clone()}}),
            serde_json::json!({"StringList": ["${secret('CANARY_4386')}"]}),
            serde_json::json!({"Secret": {"String": "CANARY_4386"}}),
            serde_json::json!({"BindingRef": {"binding": "r"}}),
            serde_json::json!({"Unknown": {"UpstreamBareRef": {"binding": "r"}}}),
            serde_json::json!({"ResourceRef": {"path": {"binding": "r", "segments": []}}}),
            serde_json::json!({"CANARY_4386": null}),
            serde_json::json!({"EnumIdentifier": "${secret('CANARY_4386')}"}),
        ] {
            for result in [
                serde_json::from_value::<WaitRhs>(wire.clone()).map(|_| ()),
                serde_json::from_value::<WaitPredicateValue>(wire.clone()).map(|_| ()),
                serde_json::from_value::<WaitPredicate>(serde_json::json!({"kind":"equals","attr":{"segments":["status"]},"value":wire})).map(|_| ()),
            ] {
                assert!(result.is_err(), "only closed scalar wait operands may decode");
                let error = result.unwrap_err();
                assert_eq!(error.to_string(), "invalid wait operand");
                assert!(!format!("{error} {error:?}").contains("CANARY_4386"));
            }
        }
        let value = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                crate::schema::enum_identity("Status", Some("aws.acm.Certificate")),
                "ISSUED",
            ),
        ));
        let expected_value = serde_json::to_value(&value).unwrap();
        let predicate = WaitPredicate::equals(AttrPath::single("status"), value).unwrap();
        let wire = serde_json::to_string(&predicate).unwrap();
        assert_eq!(
            serde_json::to_value(&predicate).unwrap()["value"],
            expected_value
        );
        let decoded: WaitPredicate = serde_json::from_str(&wire).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), wire);
        assert!(predicate.evaluate(&HashMap::from([(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String("Issued".into()))
        )])));
    }

    #[test]
    fn round2_wait_constructors_reject_non_parser_values() {
        let interpolation = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("${secret('CANARY_4386')}".into()),
        ]));
        for value in [
            interpolation.clone(),
            Value::Concrete(ConcreteValue::List(vec![interpolation.clone()])),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "CANARY_4386".into(),
                interpolation.clone(),
            )]))),
            Value::Concrete(ConcreteValue::StringList(vec![
                "${secret('CANARY_4386')}".into(),
            ])),
            Value::Deferred(DeferredValue::Secret(Box::new(interpolation).into())),
            Value::Deferred(DeferredValue::BindingRef {
                binding: crate::resource::RootScope::new()
                    .resolve(crate::resource::RelativeBindingName::new("r")),
            }),
            Value::Deferred(DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::UpstreamAllExports {
                    binding: crate::resource::RootScope::new()
                        .resolve(crate::resource::RelativeBindingName::new("r")),
                },
            )),
        ] {
            for result in [
                WaitRhs::try_from(value.clone()).map(|_| ()),
                WaitPredicateValue::try_from(value.clone()).map(|_| ()),
                WaitPredicate::equals(AttrPath::single("status"), value).map(|_| ()),
            ] {
                assert!(
                    result.is_err(),
                    "wait constructors must reject non-parser shapes"
                );
                let error = result.unwrap_err();
                assert_eq!(error.to_string(), "invalid wait operand");
                assert!(!format!("{error} {error:?}").contains("CANARY_4386"));
            }
        }
    }

    #[test]
    fn round1_wait_decode_rejects_unchecked_or_interpolated_string_sources() {
        let error = WaitRhs::<AbsoluteBindingAddress>::try_from(Value::Concrete(
            ConcreteValue::String("${secret('CANARY_4386')}".into()),
        ))
        .unwrap_err();
        assert_eq!(error.to_string(), "wait string requires a checked literal");
        for source in [
            "ready",
            "${secret('CANARY_4386')}",
            r#""${secret('CANARY_4386')}""#,
        ] {
            let wire = serde_json::json!({"kind":"equals","attr":{"segments":["identifier"]},"value":{"String":source}});
            let result = serde_json::from_value::<WaitPredicate>(wire);
            assert!(
                result.is_err(),
                "saved wait strings must carry checked literal source"
            );
            let error = result.unwrap_err();
            assert_eq!(error.to_string(), "invalid wait operand");
            assert!(!format!("{error} {error:?}").contains("CANARY_4386"));
        }
        for source in [
            r#""ready""#,
            r#"'${secret("literal text")}'"#,
            r#""\${secret('literal text')}""#,
            r#""line\nquote\"slash\\tab\t""#,
            r#"'quote\'slash\\'"#,
        ] {
            let wire = serde_json::json!({"kind":"equals","attr":{"segments":["identifier"]},"value":{"String":source}});
            let predicate: WaitPredicate = serde_json::from_value(wire.clone()).unwrap();
            let expected = &source[1..source.len() - 1];
            assert_eq!(
                &predicate.value(),
                &Value::Concrete(ConcreteValue::String(expected.into()))
            );
            assert!(predicate.evaluate(&HashMap::from([(
                "identifier".to_string(),
                Value::Concrete(ConcreteValue::String(expected.into())),
            )])));
            assert_eq!(serde_json::to_value(predicate).unwrap(), wire);
        }
    }

    #[test]
    fn secret_origin_4275_wait_enum_identifier_keeps_its_scope() {
        let value: Value = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::enum_identifier(
                "private-enum",
            )))
            .into(),
        ));
        let error = WaitPredicateValue::try_from(value).unwrap_err();
        assert!(!format!("{error} {error:?}").contains("private-enum"));
    }

    #[test]
    fn equals_watched_attrs_returns_the_predicate_attr() {
        let attr = AttrPath::single("status");
        let predicate = WaitPredicate::equals(
            attr.clone(),
            serde_json::from_value::<crate::wait::WaitRhs>(
                serde_json::json!({"String": "'ready'"}),
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(predicate.watched_attrs(), vec![&attr]);
    }

    #[test]
    fn attr_path_try_new_rejects_empty_segments() {
        assert!(matches!(
            AttrPath::try_new(vec![]),
            Err(AttrPathError::Empty)
        ));
    }

    #[test]
    fn attr_path_try_new_accepts_non_empty_segments() {
        let path =
            AttrPath::try_new(vec!["renewal_summary".into(), "renewal_status".into()]).unwrap();

        assert_eq!(path.segments(), &["renewal_summary", "renewal_status"]);
    }

    #[test]
    fn attr_path_segments_field_is_not_publicly_accessible() {
        let path = AttrPath::single("status");
        let segments: &[String] = path.segments();

        assert_eq!(segments, &["status"]);
    }

    #[test]
    fn attr_path_serde_roundtrip_preserves_wire_format() {
        let path =
            AttrPath::try_new(vec!["renewal_summary".into(), "renewal_status".into()]).unwrap();

        let json = serde_json::to_string(&path).unwrap();
        assert_eq!(json, r#"{"segments":["renewal_summary","renewal_status"]}"#);
        let parsed: AttrPath = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.segments(), &["renewal_summary", "renewal_status"]);
    }

    #[test]
    fn attr_path_serde_rejects_empty_segments() {
        let json = r#"{"segments":[]}"#;
        let result: Result<AttrPath, _> = serde_json::from_str(json);

        assert!(result.is_err());
    }

    #[test]
    fn resolve_returns_none_for_missing_attr() {
        let attrs = HashMap::from([(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String("pending".into())),
        )]);
        let path = AttrPath::single("missing");

        assert!(path.resolve(&attrs).is_none());
    }

    #[test]
    fn canonical_enum_equals_provider_api_string_case_insensitively() {
        let predicate = WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::CanonicalEnum(
                crate::resource::CanonicalEnumValue::new_for_test(
                    crate::schema::enum_identity("Status", Some("aws.acm.Certificate")),
                    "ISSUED",
                ),
            )),
        )
        .unwrap();
        let attrs = HashMap::from([(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String("Issued".into())),
        )]);

        assert!(predicate.evaluate(&attrs));
    }

    #[test]
    fn plan_and_apply_enum_observations_agree_for_quoted_and_typed_rhs() {
        fn canonical_status() -> Value {
            Value::Concrete(ConcreteValue::CanonicalEnum(
                crate::resource::CanonicalEnumValue::new_for_test(
                    crate::schema::enum_identity("Status", Some("aws.acm.Certificate")),
                    "ISSUED",
                ),
            ))
        }

        let plan_state = HashMap::from([("status".to_string(), canonical_status())]);
        let apply_state = HashMap::from([(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String("ISSUED".into())),
        )]);
        let cases = [
            (
                "quoted lowercase API spelling",
                serde_json::from_value::<WaitRhs>(serde_json::json!({"String": "'issued'"}))
                    .unwrap(),
                false,
            ),
            (
                "quoted exact API spelling",
                serde_json::from_value::<WaitRhs>(serde_json::json!({"String": "'ISSUED'"}))
                    .unwrap(),
                true,
            ),
            (
                "resolved enum identifier",
                canonical_status().try_into().unwrap(),
                true,
            ),
        ];

        for (case, rhs, expected) in cases {
            let predicate = WaitPredicate::equals(AttrPath::single("status"), rhs).unwrap();
            assert_eq!(
                predicate.evaluate(&plan_state),
                expected,
                "plan-state result for {case}"
            );
            assert_eq!(
                predicate.evaluate(&apply_state),
                expected,
                "apply-state result for {case}"
            );
        }
    }

    #[test]
    fn serde_rejects_unresolved_enum_identifier() {
        let json = serde_json::json!({
            "kind": "equals",
            "attr": AttrPath::single("state"),
            "value": Value::Concrete(ConcreteValue::enum_identifier("ready")),
        })
        .to_string();

        let decoded: Result<WaitPredicate, _> = serde_json::from_str(&json);

        assert!(
            decoded
                .unwrap_err()
                .to_string()
                .contains("unresolved enum identifier `ready`")
        );
    }
}
