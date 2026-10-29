//! Typed predicate AST for the `wait` construct.
//!
//! See `notes/specs/2026-05-09-wait-construct-design.md` for the surface
//! design. The MVP supports `<target>.<attr> == <value>` only; the enum
//! is shaped to grow (`NotEquals`, `And`, `Or`, comparisons, `In`)
//! without breaking call sites.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};
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
    pub(crate) fn resolve<'a>(&self, attrs: &'a HashMap<String, Value>) -> Option<&'a Value> {
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
    #[error("unresolved enum identifier `{identifier}`")]
    UnresolvedEnumIdentifier { identifier: String },
}

/// Context-rich failure produced while resolving and sealing a parsed wait
/// predicate. Every plan/validate/LSP caller renders this same value, keeping
/// both path spelling and remediation text identical.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WaitResolutionError {
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
        identifier: String,
    },
}

impl WaitResolutionError {
    pub fn rhs_anchor(&self) -> Option<&str> {
        match self {
            Self::UnresolvedEnumIdentifier { identifier, .. } => Some(identifier),
            Self::InvalidPath(_)
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
pub fn resolve_wait_predicate(
    lhs_segments: &[String],
    value: Value,
    schema: Option<&ValidatedResourceSchema>,
) -> Result<WaitPredicate, WaitResolutionError> {
    let (target, attribute_segments) = lhs_segments
        .split_first()
        .ok_or(WaitResolutionError::InvalidPath(AttrPathError::Empty))?;
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
                        target: target.clone(),
                        attribute,
                        resource_type: schema.as_schema().resource_type().to_string(),
                    }
                }
                AttributePathTypeError::UnknownStructField {
                    parent_path,
                    field,
                    struct_name,
                } => WaitResolutionError::UnknownStructField {
                    target: target.clone(),
                    full_path: format!("{parent_path}.{field}"),
                    struct_name,
                },
                AttributePathTypeError::CannotTraverse { path, type_name } => {
                    WaitResolutionError::CannotTraverse {
                        target: target.clone(),
                        full_path: attribute.clone(),
                        traversed: path,
                        type_name,
                    }
                }
            })?;
        if matches!(resolved, Value::Concrete(ConcreteValue::EnumIdentifier(_))) {
            resolved = crate::value::canonicalize_wait_rhs_with_type(resolved, &effective_type);
        }
    }

    WaitPredicate::equals(attr, resolved).map_err(|error| match error {
        WaitPredicateError::UnresolvedEnumIdentifier { identifier } => {
            WaitResolutionError::UnresolvedEnumIdentifier {
                target: target.clone(),
                attribute,
                identifier,
            }
        }
    })
}

/// A wait RHS that is safe to persist and evaluate.
///
/// The inner value is private so an unresolved [`ConcreteValue::EnumIdentifier`]
/// cannot reach polling through direct construction or saved-plan
/// deserialization. Build predicates with [`WaitPredicate::equals`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WaitPredicateValue(Value);

impl WaitPredicateValue {
    pub fn as_value(&self) -> &Value {
        &self.0
    }
}

impl TryFrom<Value> for WaitPredicateValue {
    type Error = WaitPredicateError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        if let Some(identifier) = unresolved_enum_identifier(&value) {
            return Err(WaitPredicateError::UnresolvedEnumIdentifier {
                identifier: identifier.to_string(),
            });
        }
        Ok(Self(value))
    }
}

impl<'de> Deserialize<'de> for WaitPredicateValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

fn unresolved_enum_identifier(value: &Value) -> Option<&str> {
    match value {
        Value::Concrete(ConcreteValue::EnumIdentifier(identifier)) => Some(identifier.as_str()),
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().find_map(unresolved_enum_identifier)
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            map.values().find_map(unresolved_enum_identifier)
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().find_map(|part| {
            if let InterpolationPart::Expr(value) = part {
                unresolved_enum_identifier(value)
            } else {
                None
            }
        }),
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args
            .iter()
            .find_map(|argument| unresolved_enum_identifier(argument.walker_value())),
        Value::Deferred(DeferredValue::Access(access)) => unresolved_enum_identifier(access.base()),
        Value::Deferred(DeferredValue::Secret(inner)) => unresolved_enum_identifier(inner),
        Value::Deferred(DeferredValue::BindingCandidate { value, .. }) => {
            unresolved_enum_identifier(value)
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            unresolved_enum_identifier(value).or_else(|| unresolved_enum_identifier(default))
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
        ) => None,
    }
}

/// Typed predicate AST evaluated against a target's attribute snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitPredicate {
    Equals {
        attr: AttrPath,
        value: WaitPredicateValue,
    },
}

impl WaitPredicate {
    pub fn equals(attr: AttrPath, value: Value) -> Result<Self, WaitPredicateError> {
        Ok(Self::Equals {
            attr,
            value: WaitPredicateValue::try_from(value)?,
        })
    }

    pub fn value(&self) -> &Value {
        match self {
            Self::Equals { value, .. } => value.as_value(),
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
    pub fn evaluate(&self, attrs: &HashMap<String, Value>) -> bool {
        match self {
            WaitPredicate::Equals { attr, value } => attr
                .resolve(attrs)
                .is_some_and(|observed| wait_values_equal(observed, value.as_value())),
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
fn wait_values_equal(observed: &Value, expected: &Value) -> bool {
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
    use crate::resource::ConcreteValue;
    use std::collections::HashMap;

    #[test]
    fn equals_watched_attrs_returns_the_predicate_attr() {
        let attr = AttrPath::single("status");
        let predicate = WaitPredicate::equals(
            attr.clone(),
            Value::Concrete(ConcreteValue::String("ready".into())),
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
                Value::Concrete(ConcreteValue::String("issued".into())),
                false,
            ),
            (
                "quoted exact API spelling",
                Value::Concrete(ConcreteValue::String("ISSUED".into())),
                true,
            ),
            ("resolved enum identifier", canonical_status(), true),
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
