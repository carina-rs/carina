//! Serializable protocol types for host-guest communication.
//!
//! These mirror carina-core types but are JSON-serializable across the process boundary.

use serde::de::Error as DeError;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use crate::PERSISTED_SECRET_KEY;
pub use crate::{PendingPhase, PollFinality, PollRequest};

fn default_true() -> bool {
    true
}

/// Mirrors `carina_core::resource::ResourceId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceId {
    pub provider: String,
    pub resource_type: String,
    #[serde(alias = "name")]
    pub identity: String,
}

/// Structured identity of a provider-defined custom type.
///
/// Mirrors `carina_core::schema::TypeIdentity` and the WIT
/// `type-identity` record. Keyed on discrete `provider + segments +
/// kind` axes so two providers' same-named custom types stay distinct.
/// An empty `provider` string denotes a provider-agnostic type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TypeIdentity {
    pub provider: String,
    pub segments: Vec<String>,
    pub kind: String,
}

/// A validated long-form reference template.
///
/// The only placeholders accepted by protocol v17 are `{partition}`,
/// `{region}`, `{account}`, and `{name}`. The `{name}` placeholder appears
/// exactly once and denotes the short form of the reference.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReferenceTemplate(ValidatedReferenceTemplate);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct ValidatedReferenceTemplate {
    source: String,
    segments: Vec<ReferenceTemplateSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ReferenceTemplateSegment {
    Literal(String),
    Placeholder(ReferenceTemplatePlaceholder),
}

/// Why a reference template could not be admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceTemplateError {
    NamePlaceholderCount { actual: usize },
    UnknownPlaceholder { placeholder: String },
    StrayOpeningBrace,
    StrayClosingBrace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum ReferenceTemplatePlaceholder {
    Partition,
    Region,
    Account,
    Name,
}

fn reference_template_placeholder(
    value: &str,
) -> Result<ReferenceTemplatePlaceholder, ReferenceTemplateError> {
    if value == "partition" {
        Ok(ReferenceTemplatePlaceholder::Partition)
    } else if value == "region" {
        Ok(ReferenceTemplatePlaceholder::Region)
    } else if value == "account" {
        Ok(ReferenceTemplatePlaceholder::Account)
    } else if value == "name" {
        Ok(ReferenceTemplatePlaceholder::Name)
    } else {
        Err(ReferenceTemplateError::UnknownPlaceholder {
            placeholder: value.to_string(),
        })
    }
}

impl std::fmt::Display for ReferenceTemplateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NamePlaceholderCount { actual } => write!(
                formatter,
                "reference template must contain exactly one `{{name}}` placeholder; found {actual}"
            ),
            Self::UnknownPlaceholder { placeholder } => write!(
                formatter,
                "reference template contains unknown placeholder `{{{placeholder}}}`"
            ),
            Self::StrayOpeningBrace => {
                formatter.write_str("reference template contains a stray opening brace")
            }
            Self::StrayClosingBrace => {
                formatter.write_str("reference template contains a stray closing brace")
            }
        }
    }
}

impl std::error::Error for ReferenceTemplateError {}

impl ReferenceTemplate {
    pub fn new(value: impl Into<String>) -> Result<Self, ReferenceTemplateError> {
        let value = value.into();
        let bytes = value.as_bytes();
        let mut index = 0;
        let mut literal_start = 0;
        let mut name_count = 0;
        let mut segments = Vec::new();

        while index < bytes.len() {
            if bytes[index] == b'{' {
                if literal_start < index {
                    segments.push(ReferenceTemplateSegment::Literal(
                        value[literal_start..index].to_string(),
                    ));
                }
                let start = index + 1;
                let mut end = start;
                while end < bytes.len() && bytes[end] != b'}' {
                    if bytes[end] == b'{' {
                        return Err(ReferenceTemplateError::StrayOpeningBrace);
                    }
                    end += 1;
                }
                if end == bytes.len() {
                    return Err(ReferenceTemplateError::StrayOpeningBrace);
                }
                let placeholder = &value[start..end];
                let placeholder = reference_template_placeholder(placeholder)?;
                match placeholder {
                    ReferenceTemplatePlaceholder::Partition
                    | ReferenceTemplatePlaceholder::Region
                    | ReferenceTemplatePlaceholder::Account => {}
                    ReferenceTemplatePlaceholder::Name => name_count += 1,
                }
                segments.push(ReferenceTemplateSegment::Placeholder(placeholder));
                index = end + 1;
                literal_start = index;
            } else if bytes[index] == b'}' {
                return Err(ReferenceTemplateError::StrayClosingBrace);
            } else {
                index += 1;
            }
        }

        if name_count != 1 {
            return Err(ReferenceTemplateError::NamePlaceholderCount { actual: name_count });
        }
        if literal_start < value.len() {
            segments.push(ReferenceTemplateSegment::Literal(
                value[literal_start..].to_string(),
            ));
        }
        Ok(Self(ValidatedReferenceTemplate {
            source: value,
            segments,
        }))
    }

    pub fn as_str(&self) -> &str {
        &self.0.source
    }

    /// Expand this validated template once using one initialized provider
    /// scope. Inserted values are appended as literal text and are never
    /// parsed again as template placeholders.
    pub fn render(&self, scope: &ReferenceScope, name: &str) -> Option<String> {
        let values = scope.available_values()?;
        let mut rendered = String::with_capacity(self.as_str().len() + name.len());
        for segment in &self.0.segments {
            match segment {
                ReferenceTemplateSegment::Literal(literal) => rendered.push_str(literal),
                ReferenceTemplateSegment::Placeholder(ReferenceTemplatePlaceholder::Partition) => {
                    rendered.push_str(values.partition())
                }
                ReferenceTemplateSegment::Placeholder(ReferenceTemplatePlaceholder::Region) => {
                    rendered.push_str(values.region())
                }
                ReferenceTemplateSegment::Placeholder(ReferenceTemplatePlaceholder::Account) => {
                    rendered.push_str(values.account())
                }
                ReferenceTemplateSegment::Placeholder(ReferenceTemplatePlaceholder::Name) => {
                    rendered.push_str(name)
                }
            }
        }
        Some(rendered)
    }
}

impl std::fmt::Display for ReferenceTemplate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for ReferenceTemplate {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for ReferenceTemplate {
    type Error = ReferenceTemplateError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl Serialize for ReferenceTemplate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ReferenceTemplate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

/// Declared equivalence form for one refined String identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ReferenceForm {
    Template { long: ReferenceTemplate },
}

/// One dotted identity/form pair in the provider schema envelope.
///
/// The string takes the same wire path as identities embedded in attribute
/// types. Hosts therefore apply one decoding rule to both declarations before
/// validating that a form belongs to an identified refined string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceFormDeclaration {
    pub identity: String,
    pub form: ReferenceForm,
}

/// Resource schemas and provider-wide reference-form declarations.
///
/// `reference_forms` is a list so core decoding can reject duplicate decoded
/// identities explicitly. Core encoding sorts the list by its typed identity
/// before projecting each key onto the dotted wire form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSchemaEnvelope {
    pub resources: Vec<ResourceSchema>,
    pub reference_forms: Vec<ReferenceFormDeclaration>,
}

/// Values an initialized provider instance exposes for expanding short
/// provider references into their long form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceScopeField {
    Partition,
    Region,
    Account,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceScopeError {
    Empty { field: ReferenceScopeField },
    ContainsBrace { field: ReferenceScopeField },
}

impl std::fmt::Display for ReferenceScopeField {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Partition => formatter.write_str("partition"),
            Self::Region => formatter.write_str("region"),
            Self::Account => formatter.write_str("account"),
        }
    }
}

impl std::fmt::Display for ReferenceScopeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty { field } => write!(formatter, "reference scope {field} must not be empty"),
            Self::ContainsBrace { field } => write!(
                formatter,
                "reference scope {field} must not contain `{{` or `}}`"
            ),
        }
    }
}

impl std::error::Error for ReferenceScopeError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceScopeValues {
    partition: String,
    region: String,
    account: String,
}

impl ReferenceScopeValues {
    pub fn new(
        partition: impl Into<String>,
        region: impl Into<String>,
        account: impl Into<String>,
    ) -> Result<Self, ReferenceScopeError> {
        let partition = partition.into();
        let region = region.into();
        let account = account.into();
        validate_reference_scope_value(ReferenceScopeField::Partition, &partition)?;
        validate_reference_scope_value(ReferenceScopeField::Region, &region)?;
        validate_reference_scope_value(ReferenceScopeField::Account, &account)?;
        Ok(Self {
            partition,
            region,
            account,
        })
    }

    pub fn partition(&self) -> &str {
        &self.partition
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn account(&self) -> &str {
        &self.account
    }
}

fn validate_reference_scope_value(
    field: ReferenceScopeField,
    value: &str,
) -> Result<(), ReferenceScopeError> {
    if value.is_empty() {
        return Err(ReferenceScopeError::Empty { field });
    }
    if value.contains(['{', '}']) {
        return Err(ReferenceScopeError::ContainsBrace { field });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReferenceScope {
    Available(ReferenceScopeValues),
    Unavailable,
}

impl ReferenceScope {
    pub fn available(
        partition: impl Into<String>,
        region: impl Into<String>,
        account: impl Into<String>,
    ) -> Result<Self, ReferenceScopeError> {
        ReferenceScopeValues::new(partition, region, account).map(Self::Available)
    }

    pub fn available_values(&self) -> Option<&ReferenceScopeValues> {
        match self {
            Self::Available(values) => Some(values),
            Self::Unavailable => None,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum SerializedReferenceScope<'a> {
    Available {
        partition: &'a str,
        region: &'a str,
        account: &'a str,
    },
    Unavailable,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
enum DeserializedReferenceScope {
    Available {
        partition: String,
        region: String,
        account: String,
    },
    Unavailable,
}

impl Serialize for ReferenceScope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Available(values) => SerializedReferenceScope::Available {
                partition: values.partition(),
                region: values.region(),
                account: values.account(),
            }
            .serialize(serializer),
            Self::Unavailable => SerializedReferenceScope::Unavailable.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ReferenceScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match DeserializedReferenceScope::deserialize(deserializer)? {
            DeserializedReferenceScope::Available {
                partition,
                region,
                account,
            } => Self::available(partition, region, account).map_err(D::Error::custom),
            DeserializedReferenceScope::Unavailable => Ok(Self::Unavailable),
        }
    }
}

/// Mirrors `carina_core::resource::Value`.
///
/// Only includes variants that can cross the process boundary.
/// `ResourceRef`, `Interpolation`, `FunctionCall`, `Closure` are resolved
/// before reaching the provider, so they are excluded.
///
/// Custom Serialize/Deserialize to produce untagged JSON (strings as `"hello"`,
/// ints as `42`, etc.) without hitting serde's recursive monomorphization limit.
///
/// Redacted secrets are not representable at this ordinary boundary:
///
/// ```compile_fail
/// use carina_provider_protocol::types::Value;
///
/// let _ = Value::RedactedSecret("digest".to_string());
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    StringList(Vec<String>),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Int(i) => serializer.serialize_i64(*i),
            Value::Float(f) => serializer.serialize_f64(*f),
            Value::String(s) => serializer.serialize_str(s),
            Value::StringList(l) => {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(l.len()))?;
                for v in l {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Value::List(l) => {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(l.len()))?;
                for v in l {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Value::Map(m) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(m.len()))?;
                for (k, v) in m {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let json = serde_json::Value::deserialize(deserializer)?;
        Ok(Value::from_json(json))
    }
}

impl Value {
    fn from_json(json: serde_json::Value) -> Self {
        match json {
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else {
                    Value::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => Value::String(s),
            serde_json::Value::Array(a) => {
                Value::List(a.into_iter().map(Value::from_json).collect())
            }
            serde_json::Value::Object(values) => Value::Map(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Value::from_json(value)))
                    .collect(),
            ),
            serde_json::Value::Null => Value::String(String::new()),
        }
    }
}

/// A value that crosses a persisted-state boundary.
///
/// This is deliberately distinct from [`Value`]: only persisted positions can
/// carry an opaque redacted-secret digest or interpret the reserved
/// [`PERSISTED_SECRET_KEY`] JSON object.
#[derive(Debug, Clone, PartialEq)]
pub enum PersistedValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    RedactedSecret(String),
    StringList(Vec<String>),
    List(Vec<PersistedValue>),
    Map(BTreeMap<String, PersistedValue>),
}

impl Serialize for PersistedValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Int(value) => serializer.serialize_i64(*value),
            Self::Float(value) => serializer.serialize_f64(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::RedactedSecret(digest) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(PERSISTED_SECRET_KEY, digest)?;
                map.end()
            }
            Self::StringList(values) => {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(value)?;
                }
                seq.end()
            }
            Self::List(values) => {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(value)?;
                }
                seq.end()
            }
            Self::Map(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    let key = if key.starts_with('$') {
                        Cow::Owned(format!("${key}"))
                    } else {
                        Cow::Borrowed(key.as_str())
                    };
                    map.serialize_entry(key.as_ref(), value)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for PersistedValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let json = serde_json::Value::deserialize(deserializer)?;
        Self::from_json(json).map_err(D::Error::custom)
    }
}

impl PersistedValue {
    fn from_json(json: serde_json::Value) -> Result<Self, String> {
        match json {
            serde_json::Value::Bool(value) => Ok(Self::Bool(value)),
            serde_json::Value::Number(value) => {
                if let Some(value) = value.as_i64() {
                    Ok(Self::Int(value))
                } else {
                    Ok(Self::Float(value.as_f64().unwrap_or(0.0)))
                }
            }
            serde_json::Value::String(value) => Ok(Self::String(value)),
            serde_json::Value::Array(values) => values
                .into_iter()
                .map(Self::from_json)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::List),
            serde_json::Value::Object(mut values) => {
                if values.len() == 1 && values.contains_key(PERSISTED_SECRET_KEY) {
                    return match values.remove(PERSISTED_SECRET_KEY) {
                        Some(serde_json::Value::String(digest)) => Ok(Self::RedactedSecret(digest)),
                        _ => Err(format!("malformed {PERSISTED_SECRET_KEY} protocol value")),
                    };
                }
                if values
                    .keys()
                    .any(|key| key.starts_with('$') && !key.starts_with("$$"))
                {
                    return Err("invalid host-reserved protocol value".to_string());
                }
                values
                    .into_iter()
                    .map(|(key, value)| {
                        let key = key.strip_prefix('$').unwrap_or(&key).to_string();
                        Self::from_json(value).map(|value| (key, value))
                    })
                    .collect::<Result<BTreeMap<_, _>, _>>()
                    .map(Self::Map)
            }
            serde_json::Value::Null => Ok(Self::String(String::new())),
        }
    }

    /// Convert a persisted value into an ordinary provider value.
    ///
    /// This is fallible because a redacted secret is intentionally unavailable
    /// at ordinary/cloud-value positions.
    pub fn into_value(self) -> Result<Value, RedactedSecretValueError> {
        self.try_into()
    }

    /// Clone this persisted value into an ordinary provider value when it does
    /// not contain a redacted secret.
    pub fn to_value(&self) -> Result<Value, RedactedSecretValueError> {
        self.try_into()
    }
}

/// Returned when a persisted value cannot be exposed as an ordinary value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedactedSecretValueError;

impl std::fmt::Display for RedactedSecretValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a redacted secret cannot be converted to an ordinary provider value")
    }
}

impl std::error::Error for RedactedSecretValueError {}

impl From<Value> for PersistedValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Bool(value) => Self::Bool(value),
            Value::Int(value) => Self::Int(value),
            Value::Float(value) => Self::Float(value),
            Value::String(value) => Self::String(value),
            Value::StringList(values) => Self::StringList(values),
            Value::List(values) => Self::List(values.into_iter().map(Self::from).collect()),
            Value::Map(values) => Self::Map(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from(value)))
                    .collect(),
            ),
        }
    }
}

impl TryFrom<PersistedValue> for Value {
    type Error = RedactedSecretValueError;

    fn try_from(value: PersistedValue) -> Result<Self, Self::Error> {
        match value {
            PersistedValue::Bool(value) => Ok(Self::Bool(value)),
            PersistedValue::Int(value) => Ok(Self::Int(value)),
            PersistedValue::Float(value) => Ok(Self::Float(value)),
            PersistedValue::String(value) => Ok(Self::String(value)),
            PersistedValue::RedactedSecret(_) => Err(RedactedSecretValueError),
            PersistedValue::StringList(values) => Ok(Self::StringList(values)),
            PersistedValue::List(values) => values
                .into_iter()
                .map(Self::try_from)
                .collect::<Result<Vec<_>, _>>()
                .map(Self::List),
            PersistedValue::Map(values) => values
                .into_iter()
                .map(|(key, value)| Self::try_from(value).map(|value| (key, value)))
                .collect::<Result<BTreeMap<_, _>, _>>()
                .map(Self::Map),
        }
    }
}

impl TryFrom<&PersistedValue> for Value {
    type Error = RedactedSecretValueError;

    fn try_from(value: &PersistedValue) -> Result<Self, Self::Error> {
        value.clone().try_into()
    }
}

/// Mirrors `carina_core::resource::State`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub id: ResourceId,
    pub identifier: Option<String>,
    pub attributes: BTreeMap<String, Value>,
    pub exists: bool,
}

/// Mirrors a WIT `persisted-state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    pub id: ResourceId,
    pub identifier: Option<String>,
    pub attributes: BTreeMap<String, PersistedValue>,
    pub exists: bool,
}

/// Error returned when an applied provider identifier is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyAppliedIdentifier;

impl std::fmt::Display for EmptyAppliedIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an applied resource identifier must not be empty")
    }
}

impl std::error::Error for EmptyAppliedIdentifier {}

/// Non-empty provider identifier used by managed reads and mutations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct AppliedIdentifier(String);

impl AppliedIdentifier {
    pub fn new(identifier: impl Into<String>) -> Result<Self, EmptyAppliedIdentifier> {
        let identifier = identifier.into();
        if identifier.is_empty() {
            Err(EmptyAppliedIdentifier)
        } else {
            Ok(Self(identifier))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl std::fmt::Display for AppliedIdentifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for AppliedIdentifier {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<'de> Deserialize<'de> for AppliedIdentifier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let identifier = String::deserialize(deserializer)?;
        Self::new(identifier).map_err(serde::de::Error::custom)
    }
}

/// Ordinary-value state known to identify an existing provider-side resource.
///
/// Successful reads and create outcomes use this form. It always carries a
/// required provider identifier, so an existing resource cannot be written
/// back as absent. Unlike generic [`State`], applied state has no `exists`
/// flag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedState {
    /// Used by the JSON-RPC/process transport. The guest SDK's WIT encoding
    /// drops this field because the WIT outcome is already bound to the
    /// request resource id.
    pub id: ResourceId,
    pub identifier: AppliedIdentifier,
    pub attributes: BTreeMap<String, Value>,
}

impl AppliedState {
    pub fn into_parts(self) -> (ResourceId, AppliedIdentifier, BTreeMap<String, Value>) {
        (self.id, self.identifier, self.attributes)
    }
}

/// Persisted-value state observed after a successful update mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedPersistedState {
    /// Used by the JSON-RPC/process transport. The guest SDK's WIT encoding
    /// drops this field because the WIT outcome is already bound to the
    /// request resource id.
    pub id: ResourceId,
    pub identifier: AppliedIdentifier,
    pub attributes: BTreeMap<String, PersistedValue>,
}

/// Result of reading a managed resource by its provider identifier.
///
/// The resource ID is retained in both variants for the JSON-RPC/process
/// transport. The guest SDK drops it while encoding WIT because the request
/// already supplies the ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ReadOutcome {
    NotFound { id: ResourceId },
    Found { state: AppliedState },
}

impl ReadOutcome {
    pub fn into_found(self) -> Option<AppliedState> {
        match self {
            Self::Found { state } => Some(state),
            Self::NotFound { id: _id } => None,
        }
    }
}

/// Opaque provider continuation persisted for an unfinished create.
///
/// Its payload is deliberately absent from `Debug`, and this type has no
/// `Display` implementation. Serialization exists only because create tokens
/// cross the WIT boundary and are persisted in state.
///
/// ```compile_fail
/// use carina_provider_protocol::PendingToken;
///
/// let token = PendingToken::new("opaque");
/// let _ = format!("{token}");
/// ```
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PendingToken(String);

impl PendingToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// Expose the token only when handing it back to the provider that owns it.
    pub fn expose_for_provider_call(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for PendingToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingToken([REDACTED])")
    }
}

/// Top-level request attributes carried by the create request accepted by the
/// cloud API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "attributes")]
pub enum Sent {
    AllRequestAttributes,
    Only(BTreeSet<String>),
}

impl Sent {
    pub fn only<I, S>(attributes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Only(attributes.into_iter().map(Into::into).collect())
    }

    pub fn names<'a>(&'a self, request_attributes: &'a BTreeSet<String>) -> &'a BTreeSet<String> {
        match self {
            Self::AllRequestAttributes => request_attributes,
            Self::Only(attributes) => attributes,
        }
    }

    pub fn is_valid_for(&self, request_attributes: &BTreeSet<String>) -> bool {
        match self {
            Self::AllRequestAttributes => true,
            Self::Only(attributes) => attributes.is_subset(request_attributes),
        }
    }

    /// Restrict an explicit sent set to request attribute names. Returns
    /// whether the original set was already valid.
    pub fn clip_to(&mut self, request_attributes: &BTreeSet<String>) -> bool {
        if self.is_valid_for(request_attributes) {
            return true;
        }
        let Self::Only(attributes) = self else {
            return true;
        };
        attributes.retain(|attribute| request_attributes.contains(attribute));
        false
    }
}

/// Provider observation made while completing a create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Observation<R, E> {
    Read(R),
    ReadFailed(E),
}

/// Progress of an initial or resumed create. Read-back always has an identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum CreateProgress {
    Operating {
        identifier: Option<AppliedIdentifier>,
    },
    ReadingBack {
        identifier: AppliedIdentifier,
    },
}

impl CreateProgress {
    pub fn identifier(&self) -> Option<&AppliedIdentifier> {
        match self {
            Self::Operating { identifier } => identifier.as_ref(),
            Self::ReadingBack { identifier } => Some(identifier),
        }
    }

    pub fn phase(&self) -> crate::PendingPhase {
        match self {
            Self::Operating { .. } => crate::PendingPhase::Operation,
            Self::ReadingBack { .. } => crate::PendingPhase::ReadBack,
        }
    }

    pub fn into_identifier(self) -> Option<AppliedIdentifier> {
        match self {
            Self::Operating { identifier } => identifier,
            Self::ReadingBack { identifier } => Some(identifier),
        }
    }
}

/// An unfinished initial create, including the immutable `sent` declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCreate<T> {
    pub token: T,
    pub progress: CreateProgress,
    pub sent: Sent,
    pub poll_after_secs: u32,
}

impl<T> PendingCreate<T> {
    pub fn token(&self) -> &T {
        &self.token
    }
    pub fn identifier(&self) -> Option<&AppliedIdentifier> {
        self.progress.identifier()
    }
    pub fn sent(&self) -> &Sent {
        &self.sent
    }
    pub fn poll_after_secs(&self) -> u32 {
        self.poll_after_secs
    }
    pub fn phase(&self) -> crate::PendingPhase {
        self.progress.phase()
    }
    pub fn into_parts(self) -> (T, CreateProgress, Sent, u32) {
        (self.token, self.progress, self.sent, self.poll_after_secs)
    }
}

/// A create continuation retained after an error stopped the current step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateInFlight<T, E> {
    pub pending: PendingCreate<T>,
    pub error: E,
}

/// An unfinished resumed create. Sent values stay in the persisted envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumePending<T> {
    pub token: T,
    pub progress: CreateProgress,
    pub poll_after_secs: u32,
}

impl<T> ResumePending<T> {
    pub fn token(&self) -> &T {
        &self.token
    }
    pub fn identifier(&self) -> Option<&AppliedIdentifier> {
        self.progress.identifier()
    }
    pub fn poll_after_secs(&self) -> u32 {
        self.poll_after_secs
    }
    pub fn phase(&self) -> crate::PendingPhase {
        self.progress.phase()
    }
    pub fn into_parts(self) -> (T, CreateProgress, u32) {
        (self.token, self.progress, self.poll_after_secs)
    }
}

/// An unfinished delete operation. Delete has no post-apply read-back phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDelete<T> {
    pub token: T,
    pub poll_after_secs: u32,
}

impl<T> PendingDelete<T> {
    pub fn new(token: T, poll_after_secs: u32) -> Self {
        Self {
            token,
            poll_after_secs,
        }
    }
}

/// A resume continuation retained after an error stopped the current step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeInFlight<T, E> {
    pub pending: ResumePending<T>,
    pub error: E,
}

/// One provider step for an ordinary create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateStep<A, D, T> {
    Success {
        identifier: AppliedIdentifier,
        attributes: A,
    },
    PartialSuccess {
        identifier: AppliedIdentifier,
        attributes: A,
        diagnostic: D,
    },
    Pending(PendingCreate<T>),
}

/// Failure payload for a create for which the provider reported an identifier.
/// The observation says whether the resource was found during read-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedFailure<R, E> {
    pub identifier: AppliedIdentifier,
    pub observation: Observation<R, E>,
    pub sent: Sent,
    pub error: E,
}

impl<R, E> CreatedFailure<R, E> {
    pub fn into_parts(self) -> (AppliedIdentifier, Observation<R, E>, Sent, E) {
        (self.identifier, self.observation, self.sent, self.error)
    }
}

/// Plain wire verdict from an initial create.
///
/// This transport enum carries no typestate or provenance guarantee. The SDK
/// and native core adapters enforce create classification before converting
/// their opaque verdict types into this wire representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateVerdict<R, T, E> {
    Untracked(E),
    Created(CreatedFailure<R, E>),
    Discarded(E),
    InFlight(CreateInFlight<T, E>),
}

/// Plain wire verdict from polling a create. Classification is enforced by
/// the SDK or native core adapter before this value is encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollVerdict<R, T, E> {
    Untracked(E),
    Created(CreatedFailure<R, E>),
    Discarded(E),
    Undetermined(E),
    InFlight(CreateInFlight<T, E>),
}

impl<R, T, E> From<CreateVerdict<R, T, E>> for PollVerdict<R, T, E> {
    fn from(verdict: CreateVerdict<R, T, E>) -> Self {
        match verdict {
            CreateVerdict::Untracked(error) => Self::Untracked(error),
            CreateVerdict::Created(created) => Self::Created(created),
            CreateVerdict::Discarded(error) => Self::Discarded(error),
            CreateVerdict::InFlight(in_flight) => Self::InFlight(in_flight),
        }
    }
}

/// Failure payload for a resumed create for which the provider reported an
/// identifier. The observation says whether the resource was found during
/// read-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumedFailure<R, E> {
    pub identifier: AppliedIdentifier,
    pub observation: Observation<R, E>,
    pub error: E,
}

impl<R, E> ResumedFailure<R, E> {
    pub fn into_parts(self) -> (AppliedIdentifier, Observation<R, E>, E) {
        (self.identifier, self.observation, self.error)
    }
}

/// One provider step while resuming a persisted create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeStep<A, D, T> {
    Completed {
        identifier: AppliedIdentifier,
        attributes: A,
    },
    PartialSuccess {
        identifier: AppliedIdentifier,
        attributes: A,
        diagnostic: D,
    },
    Pending(ResumePending<T>),
    Resent(CreateStep<A, D, T>),
}

/// Plain wire verdict while resuming a persisted create. Classification is
/// enforced by the SDK or native core adapter before this value is encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeVerdict<R, T, E> {
    Untracked(E),
    Created(ResumedFailure<R, E>),
    Discarded(E),
    Undetermined(E),
    InFlight(ResumeInFlight<T, E>),
    Resent(CreateVerdict<R, T, E>),
}

impl<A, D, T> From<CreateStep<A, D, T>> for ResumeStep<A, D, T> {
    fn from(step: CreateStep<A, D, T>) -> Self {
        Self::Resent(step)
    }
}

impl<R, T, E> From<CreateVerdict<R, T, E>> for ResumeVerdict<R, T, E> {
    fn from(verdict: CreateVerdict<R, T, E>) -> Self {
        Self::Resent(verdict)
    }
}

impl From<State> for PersistedState {
    fn from(state: State) -> Self {
        Self {
            id: state.id,
            identifier: state.identifier,
            attributes: state
                .attributes
                .into_iter()
                .map(|(key, value)| (key, value.into()))
                .collect(),
            exists: state.exists,
        }
    }
}

impl TryFrom<PersistedState> for State {
    type Error = RedactedSecretValueError;

    fn try_from(state: PersistedState) -> Result<Self, Self::Error> {
        Ok(Self {
            id: state.id,
            identifier: state.identifier,
            attributes: state
                .attributes
                .into_iter()
                .map(|(key, value)| Value::try_from(value).map(|value| (key, value)))
                .collect::<Result<_, _>>()?,
            exists: state.exists,
        })
    }
}

/// Result of a create operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum CreateOutcome {
    Success {
        state: AppliedState,
    },
    PartialSuccess {
        state: AppliedState,
        diagnostic: PartialReadDiagnostic,
    },
}

/// Result of an update operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum UpdateOutcome {
    Success {
        state: AppliedPersistedState,
    },
    PartialSuccess {
        state: AppliedPersistedState,
        diagnostic: PartialReadDiagnostic,
    },
}

/// Diagnostic details for a partial post-mutation read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartialReadDiagnostic {
    pub reason: String,
    #[serde(default)]
    pub missing_attributes: Vec<String>,
}

/// One operation inside an [`UpdatePatch`]. Mirrors `patch-op` in
/// `wit/types.wit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PatchOp {
    Add { key: String, value: Value },
    Replace { key: String, value: Value },
    Reassert { key: String, value: Value },
    Remove { key: String },
}

impl PatchOp {
    /// Top-level attribute name. Nested-field patches are a future extension.
    pub fn key(&self) -> &str {
        match self {
            Self::Add { key, .. }
            | Self::Replace { key, .. }
            | Self::Reassert { key, .. }
            | Self::Remove { key } => key,
        }
    }

    /// Return the value sent by `Add`, `Replace`, or `Reassert`.
    pub fn sent_value(&self) -> Option<&Value> {
        match self {
            Self::Add { value, .. }
            | Self::Replace { value, .. }
            | Self::Reassert { value, .. } => Some(value),
            Self::Remove { .. } => None,
        }
    }
}

/// Structured description of the user's intended change to a resource plus
/// desired write-only values reasserted alongside that change. Mirrors
/// `update-patch` in `wit/types.wit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdatePatch {
    #[serde(default)]
    pub ops: Vec<PatchOp>,
}

/// Per-operation request record for `read`. Mirrors `read-request`
/// in `wit/types.wit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReadRequest;

/// Per-operation request record for `create`. Mirrors
/// `create-request` in `wit/types.wit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRequest {
    pub resource: Resource,
}

/// Per-operation request record for `update`. The patch contains changed keys
/// and any desired write-only values that must be reasserted with them. Mirrors
/// `update-request` in `wit/types.wit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRequest {
    pub from: PersistedState,
    pub patch: UpdatePatch,
}

impl PersistedState {
    /// Apply an ordinary update patch while retaining every unmentioned
    /// persisted attribute, including redacted secrets.
    pub fn apply_patch(&mut self, patch: &UpdatePatch) {
        for op in &patch.ops {
            match op {
                PatchOp::Add { key, value }
                | PatchOp::Replace { key, value }
                | PatchOp::Reassert { key, value } => {
                    self.attributes.insert(key.clone(), value.clone().into());
                }
                PatchOp::Remove { key } => {
                    self.attributes.remove(key);
                }
            }
        }
    }

    /// Return this persisted state with an ordinary update patch applied.
    pub fn with_patch(mut self, patch: &UpdatePatch) -> Self {
        self.apply_patch(patch);
        self
    }
}

impl UpdateRequest {
    /// Build the provider's post-update persisted state by applying the patch
    /// to a clone of `from`. Unchanged redacted secrets remain typed.
    pub fn state_after_patch(&self) -> PersistedState {
        self.from.clone().with_patch(&self.patch)
    }
}

/// Per-operation request record for `delete`. Mirrors
/// `delete-request` in `wit/types.wit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeleteRequest {
    #[serde(default)]
    pub directives: Directives,
}

/// Carina-side directives for a resource. Mirrors `directives` in
/// `wit/types.wit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Directives {
    #[serde(default)]
    pub force_delete: bool,
    #[serde(default)]
    pub create_before_destroy: bool,
    #[serde(default)]
    pub prevent_destroy: bool,
}

/// Simplified resource for the process boundary.
/// Attributes are pre-resolved `Value`s, not `Expr`s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    pub attributes: BTreeMap<String, Value>,
    #[serde(default)]
    pub directives: Directives,
}

/// Provider metadata returned by `provider_info`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub version: String,
}

/// Wire envelope for info(): carries the provider's metadata plus the
/// PROTOCOL_VERSION the SDK was compiled against, so the host can
/// reject a provider built against an older protocol (carina#3365).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfoEnvelope {
    #[serde(flatten)]
    pub info: ProviderInfo,
    /// Defaults to 0 for providers predating the envelope, so the host
    /// detects them as below any minimum.
    #[serde(default)]
    pub protocol_version: u32,
}

/// Completion value for LSP completions, serializable for WIT transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionValue {
    pub value: String,
    pub description: String,
}

/// Variant tag of [`ProviderError`].
///
/// Mirrors `provider-error` in `wit/types.wit`. Carried as a separate
/// `kind` field so the wire format stays a flat record (easier to
/// inspect than a serde-tagged enum, and matches the WIT variant
/// directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    InvalidInput,
    ApiError,
    NotFound,
    Timeout,
    #[default]
    Internal,
}

/// Provider error returned from operations.
///
/// Mirrors `(provider-error, error-detail)` in `wit/types.wit`. The
/// variant lives in [`ProviderErrorKind`]; the metadata fields mirror
/// `error-detail`.
///
/// The `operation` / `status` / `code` / `request_id` quartet carries
/// cloud-API metadata in a cloud-agnostic shape so the host can render
/// a multi-line, labeled error display without providers doing ad-hoc
/// string formatting. All four are optional — providers leave them
/// empty when the error doesn't come from an HTTP cloud API call, and
/// the host falls back to the legacy chain-walking render in that
/// case. See carina-rs/carina#3242 for the full design.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderError {
    #[serde(default)]
    pub kind: ProviderErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<ResourceId>,
    /// Flattened representation of the underlying cause chain, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// Provider name (e.g. `"aws"`, `"awscc"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    /// Service-qualified cloud-API operation that failed
    /// (e.g. `"iam.ListRoles"`, `"s3.HeadBucket"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// HTTP status code from the cloud-API response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Application-level error code (e.g. `"AccessDenied"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Correlation id from the cloud-API response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Serializable validator types that can cross the WASM boundary.
///
/// Function-pointer validators cannot cross WASM serialization. This enum
/// allows providers to declare validators as data, which the host reconstructs
/// into actual validator functions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorType {
    /// Check that a `tags` map does not use Key/Value pair list structure.
    TagsKeyValueCheck,
}

/// Classification of a schema in the provider protocol: managed (full CRUD
/// lifecycle) vs data source (read-only lookup of existing infrastructure).
///
/// Mirror of `carina_core::schema::SchemaKind`. Defined here independently to
/// keep `carina-provider-protocol` free of a dependency on `carina-core`
/// (the protocol crate is the lightweight contract layer that providers
/// link against).
///
/// `Default` is derived to back `#[serde(default)]` on `ResourceSchema::kind`,
/// so JSON payloads from older or minimal producers omit the field and
/// fall back to `Managed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaKind {
    #[default]
    Managed,
    DataSource,
}

/// Create-before-destroy uniqueness declaration for a resource schema.
///
/// Wire compatibility matrix:
/// - `Attribute(name)` serializes as the legacy string form and deserializes
///   from either that string or `{"type":"attribute","value":name}`.
/// - `Conflicting` serializes as legacy `null`; absent/null legacy payloads
///   also deserialize to `Conflicting`.
/// - `Coexisting` has no legacy representation, so it serializes as
///   `{"type":"coexisting"}`. Hosts that do not understand this version
///   cannot honor that behavior and must fail through protocol/version checks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UniqueNameSpec {
    /// User-chosen unique name attribute; CBD renames the replacement temporarily during the overlap window.
    Attribute(String),
    /// No unique name; a replacement coexists with the original, so CBD needs no temporary name.
    Coexisting,
    /// No unique name and the replacement conflicts with the original (server-side uniqueness/scoping); create_before_destroy is a plan-time error.
    #[default]
    Conflicting,
}

impl Serialize for UniqueNameSpec {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Attribute(attribute) => serializer.serialize_str(attribute),
            Self::Coexisting => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("type", "coexisting")?;
                map.end()
            }
            Self::Conflicting => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for UniqueNameSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Null => Ok(Self::Conflicting),
            serde_json::Value::String(attribute) => Ok(Self::Attribute(attribute)),
            serde_json::Value::Object(map) => {
                let tag = map
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        D::Error::custom(
                            "unique_name_attribute tagged form missing string field `type`",
                        )
                    })?;
                match tag {
                    "attribute" => {
                        let value = map
                            .get("value")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| {
                                D::Error::custom(
                                    "unique_name_attribute type `attribute` missing field `value`",
                                )
                            })?;
                        Ok(Self::Attribute(value.to_string()))
                    }
                    "coexisting" => Ok(Self::Coexisting),
                    "conflicting" => Ok(Self::Conflicting),
                    other => Err(D::Error::custom(format!(
                        "unknown unique_name_attribute type `{other}`; expected `attribute`, `coexisting`, or `conflicting`"
                    ))),
                }
            }
            other => Err(D::Error::custom(format!(
                "unique_name_attribute must be a string, null, or tagged object; got {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldGroupKind {
    ExactlyOne,
    AtLeastOne,
    AtMostOne,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FieldGroup {
    pub kind: FieldGroupKind,
    pub fields: Vec<String>,
}

/// Schema types for resource validation and completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSchema {
    pub resource_type: String,
    pub attributes: BTreeMap<String, AttributeSchema>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub kind: SchemaKind,
    #[serde(default, rename = "unique_name_attribute", alias = "name_attribute")]
    pub unique_name: UniqueNameSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_config: Option<OperationConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validators: Vec<ValidatorType>,
    /// Protocol-v16 cardinality constraints over sibling attributes.
    /// Required even when empty so version skew fails during JSON loading.
    pub field_groups: Vec<FieldGroup>,
    /// Whether the resource type is protected from destruction unless the
    /// user explicitly opts out. Required so version skew fails closed.
    pub default_prevent_destroy: bool,
    /// Named definitions reachable via [`AttributeType::Ref`] from
    /// this resource's attribute types. Empty for resources whose
    /// attribute graph contains no cycles (the common case). Mirror
    /// of [`carina_core::schema::ResourceSchema::defs`] — carries the
    /// cyclic CFN struct map across the JSON wire form so the host
    /// can resolve `Ref` targets at the walk-sites that need them
    /// (carina#3340).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub defs: std::collections::BTreeMap<String, AttributeType>,
}

/// Per-resource operational configuration for timeouts and retries.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct OperationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_back: Option<ReadBackBudget>,
}

/// Non-zero host-owned budget for a post-create or post-update read-back.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(deny_unknown_fields)]
pub struct ReadBackBudget {
    timeout_secs: NonZeroU64,
}

impl ReadBackBudget {
    pub const fn new(timeout_secs: NonZeroU64) -> Self {
        Self { timeout_secs }
    }

    pub fn from_secs(timeout_secs: u64) -> Option<Self> {
        NonZeroU64::new(timeout_secs).map(Self::new)
    }

    pub const fn timeout_secs(self) -> NonZeroU64 {
        self.timeout_secs
    }

    pub fn duration(self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.get())
    }
}

impl OperationConfig {
    /// Return the configured budget for `kind`, or the protocol default.
    pub fn budget(&self, kind: crate::OperationKind) -> std::time::Duration {
        let timeout_secs = match kind {
            crate::OperationKind::Create => self.create_timeout_secs,
            crate::OperationKind::Update => self.update_timeout_secs,
            crate::OperationKind::Delete => self.delete_timeout_secs,
        };
        timeout_secs
            .map(std::time::Duration::from_secs)
            .unwrap_or(crate::DEFAULT_OPERATION_BUDGET)
    }

    /// Return the configured read-back budget, or the host fallback.
    pub fn read_back_budget(&self) -> std::time::Duration {
        self.read_back
            .map(ReadBackBudget::duration)
            .unwrap_or(crate::DEFAULT_READ_BACK_BUDGET)
    }
}

/// How a create-only schema value contributes to anonymous identity.
///
/// This is a closed protocol-v16 enum. The field carrying it is required on
/// every attribute and struct-field record so host/provider skew fails before
/// schema semantics can be misinterpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityMode {
    /// The value does not participate in anonymous identity.
    None,
    /// The value must be available at plan time, even when its input mode is
    /// optional.
    Required,
    /// An omitted optional value contributes the dedicated absent marker; a
    /// written value must still be available at plan time.
    WhenPresent,
}

/// Whether a list permits repeated canonical element values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListElements {
    AllowDuplicates,
    Unique,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributeSchema {
    pub name: String,
    pub attr_type: AttributeType,
    pub required: bool,
    /// Key field for list elements whose provider-side values are updated only
    /// when that key is sent. Required metadata: `None` is encoded as JSON
    /// `null` so protocol-version mismatches fail at schema load.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub keyed_partial_update: Option<String>,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub create_only: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub write_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_name: Option<String>,
    /// Provider-side property name (e.g., "VpcId" for AWS Cloud Control)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    /// Override for removability detection.
    /// `None` = auto-detect, `Some(false)` = explicitly non-removable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removable: Option<bool>,
    /// How this attribute contributes to anonymous resource identity hashing.
    /// Required in protocol v16 so older providers fail the version check
    /// before this changed meaning can be decoded.
    pub identity: IdentityMode,
    /// Whether this attribute is populated asynchronously after Create returns.
    #[serde(default)]
    pub deferred_populate: bool,
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AttributeType {
    String {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        length: Option<(Option<u64>, Option<u64>)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validate: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_dsl: Option<DslTransform>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<String>,
    },
    Int {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range: Option<(Option<i64>, Option<i64>)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<String>,
    },
    Float {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range: Option<(Option<f64>, Option<f64>)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<String>,
    },
    Bool,
    /// Time duration. Authored in the DSL as a literal like `30min`,
    /// `1h`, or `15s` (`duration_literal` in the grammar); reaches the
    /// provider across the WIT boundary as integer seconds. Lets
    /// providers declare Duration-typed schema attributes so the
    /// host-side type checker accepts the literal form (carina#3166).
    Duration,
    #[serde(rename = "string_enum")]
    StringEnum {
        values: Vec<String>,
        #[serde(default)]
        name: String,
        /// DSL namespace prefix for enum validation (e.g., `"awscc"`).
        /// When present, values may be written as
        /// `{namespace}.{name}.{value}` in the DSL.
        ///
        /// Carried as a flat dotted string over the JSON wire form;
        /// the host reconstructs the structured
        /// [`carina_core::schema::TypeIdentity`] via
        /// `string_enum_identity(name, namespace.as_deref())` when
        /// projecting back into the core schema. Pre-#3222 the core
        /// `StringEnum` carried the same flat string; with #3222 the
        /// core form carries a structured identity but the wire form
        /// stays flat — splitting the boundary cost in one place.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        /// API-canonical -> DSL spelling pairs, for every value where the
        /// two differ. The host-side validator accepts both spellings.
        /// Empty means the API spelling IS the DSL spelling for every
        /// value, which is also the value used by older provider
        /// components that predate this field (carina#2831).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dsl_aliases: Vec<(String, String)>,
    },
    #[serde(rename = "list")]
    List {
        #[serde(rename = "element_type", alias = "inner")]
        element_type: Box<AttributeType>,
        /// Whether list elements are ordered (positional comparison).
        /// When false, elements are compared as multisets (order-insensitive).
        #[serde(default = "default_true")]
        ordered: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        length: Option<(Option<u64>, Option<u64>)>,
        /// Required in protocol v17, including when duplicates are allowed.
        elements: ListElements,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validate: Option<serde_json::Value>,
    },
    #[serde(rename = "map")]
    Map {
        inner: Box<AttributeType>,
        /// Key type constraint for map keys.
        key: Box<AttributeType>,
    },
    #[serde(rename = "struct")]
    Struct {
        name: String,
        fields: Vec<StructField>,
        /// Required in protocol v16, including when empty.
        field_groups: Vec<FieldGroup>,
    },
    #[serde(rename = "union")]
    Union {
        members: Vec<AttributeType>,
    },
    /// Structurally-validated custom type: values carry their own
    /// format (`arn:aws:s3:::bucket-name`, `vpc-12345678`) and reach
    /// the host-side validator verbatim. Sibling of
    /// [`AttributeType::CustomEnum`].
    ///
    /// The pre-#3222 wire shape carried a single `Custom` variant
    /// with a runtime `namespace: Option<String>` flag distinguishing
    /// enum-shaped vs structural; the variant split makes that a
    /// type-level fact (see the corresponding split in
    /// `carina_core::schema::AttributeType`).
    #[serde(rename = "custom")]
    Custom {
        name: String,
        base: Box<AttributeType>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        length: Option<(Option<u64>, Option<u64>)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validate: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_dsl: Option<DslTransform>,
    },
    /// Enum-shaped custom type: values are written as namespaced
    /// shorthand and expanded host-side via `expand_enum_shorthand`
    /// before the validator runs. `namespace` is the dotted prefix
    /// (`"aws"`, `"awscc.s3.Bucket"`) — the host reconstructs the
    /// structured [`carina_core::schema::TypeIdentity`] from
    /// `(name, namespace)`.
    #[serde(rename = "custom_enum")]
    CustomEnum {
        name: String,
        base: Box<AttributeType>,
        namespace: String,
        /// Data-driven API-to-DSL transform. The host applies the
        /// carried operation directly, so provider plugins do not need
        /// to register host-process callbacks across the WASM boundary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dsl_transform: Option<DslTransform>,
    },
    /// Named reference into the enclosing [`ResourceSchema::defs`]
    /// map. Used to express cyclic CFN definition graphs (WAFv2
    /// `WebACL.Statement`, AppSync `GraphQLApi`, ...) so the codegen
    /// emission step does not stack-overflow on recursive struct
    /// definitions (`Statement -> AndStatement -> List<Statement>`).
    ///
    /// Introduced in carina#3340. The host-side
    /// [`carina_core::schema::AttributeType::Ref`] is the structural
    /// counterpart; conversion in `carina-plugin-host::wasm_convert`
    /// passes the variant through unchanged.
    #[serde(rename = "ref")]
    Ref {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum DslTransform {
    Identity,
    HyphenToUnderscore,
    StripSuffix(String),
    ReplaceTable(Vec<(String, String)>),
    Unknown(serde_json::Value),
}

impl DslTransform {
    pub fn apply<'a>(&self, s: &'a str) -> Cow<'a, str> {
        match self {
            Self::Identity | Self::Unknown(_) => Cow::Borrowed(s),
            Self::HyphenToUnderscore => Cow::Owned(s.replace('-', "_")),
            Self::StripSuffix(suffix) => match s.strip_suffix(suffix.as_str()) {
                Some(stripped) => Cow::Borrowed(stripped),
                None => Cow::Borrowed(s),
            },
            Self::ReplaceTable(table) => match table.iter().find(|(k, _)| k == s) {
                Some((_, v)) => Cow::Owned(v.clone()),
                None => Cow::Borrowed(s),
            },
        }
    }
}

impl Serialize for DslTransform {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = match self {
            Self::Identity => serde_json::json!({ "type": "Identity" }),
            Self::HyphenToUnderscore => serde_json::json!({ "type": "HyphenToUnderscore" }),
            Self::StripSuffix(suffix) => {
                serde_json::json!({ "type": "StripSuffix", "value": suffix })
            }
            Self::ReplaceTable(table) => {
                serde_json::json!({ "type": "ReplaceTable", "value": table })
            }
            Self::Unknown(raw) => raw.clone(),
        };
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DslTransform {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        let tag = raw
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("DslTransform missing string `type` tag"))?;

        match tag {
            "Identity" => Ok(Self::Identity),
            "HyphenToUnderscore" => Ok(Self::HyphenToUnderscore),
            "StripSuffix" => {
                let value = raw
                    .get("value")
                    .cloned()
                    .ok_or_else(|| serde::de::Error::custom("StripSuffix missing `value`"))?;
                serde_json::from_value(value)
                    .map(Self::StripSuffix)
                    .map_err(serde::de::Error::custom)
            }
            "ReplaceTable" => {
                let value = raw
                    .get("value")
                    .cloned()
                    .ok_or_else(|| serde::de::Error::custom("ReplaceTable missing `value`"))?;
                serde_json::from_value(value)
                    .map(Self::ReplaceTable)
                    .map_err(serde::de::Error::custom)
            }
            _ => Ok(Self::Unknown(raw)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructField {
    pub name: String,
    pub field_type: AttributeType,
    pub required: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_name: Option<String>,
    /// Provider-side property name (e.g., "IpProtocol" for AWS Cloud Control)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    /// Whether this nested field is provider-populated and cannot be assigned.
    #[serde(default)]
    pub read_only: bool,
    /// Whether this nested field is accepted on write but omitted from reads.
    /// Required in protocol v16 so host/provider skew fails loudly.
    pub write_only: bool,
    /// Whether changing this nested user-input field requires replacement.
    /// Required in protocol v16 so host/provider skew fails loudly.
    pub create_only: bool,
    /// How this immutable nested field contributes to anonymous identity.
    /// Required in protocol v16 so host/provider skew fails loudly.
    pub identity: IdentityMode,
    /// Whether this nested field is populated asynchronously after Create returns.
    #[serde(default)]
    pub deferred_populate: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_v16_requires_field_groups_on_resources_and_structs() {
        let resource_error = serde_json::from_value::<ResourceSchema>(serde_json::json!({
            "resource_type": "test.Widget",
            "attributes": {},
            "default_prevent_destroy": false
        }))
        .expect_err("protocol v16 resource field_groups must not default")
        .to_string();
        assert!(resource_error.contains("field_groups"), "{resource_error}");

        let struct_error = serde_json::from_value::<AttributeType>(serde_json::json!({
            "type": "struct",
            "name": "Choice",
            "fields": []
        }))
        .expect_err("protocol v16 struct field_groups must not default")
        .to_string();
        assert!(struct_error.contains("field_groups"), "{struct_error}");
    }

    #[test]
    fn protocol_v16_requires_default_prevent_destroy_on_resources() {
        let error = serde_json::from_value::<ResourceSchema>(serde_json::json!({
            "resource_type": "test.Resource",
            "attributes": {},
            "field_groups": []
        }))
        .expect_err("protocol v16 default_prevent_destroy must not default")
        .to_string();

        assert!(error.contains("default_prevent_destroy"), "{error}");
    }

    #[test]
    fn field_group_wire_shape_carries_kind_and_named_fields() {
        let group = FieldGroup {
            kind: FieldGroupKind::AtLeastOne,
            fields: vec!["primary".to_string(), "fallback".to_string()],
        };
        assert_eq!(
            serde_json::to_value(&group).unwrap(),
            serde_json::json!({
                "kind": "at_least_one",
                "fields": ["primary", "fallback"]
            })
        );
    }

    fn string_attr() -> AttributeType {
        AttributeType::String {
            pattern: None,
            length: None,
            validate: None,
            to_dsl: None,
            identity: None,
        }
    }

    #[test]
    fn reference_scope_roundtrips_without_losing_the_closed_variant() {
        let scopes = [
            ReferenceScope::available("aws", "ap-northeast-1", "123456789012")
                .expect("fixture scope is valid"),
            ReferenceScope::Unavailable,
        ];

        for scope in scopes {
            let encoded = serde_json::to_string(&scope).expect("scope must serialize");
            let decoded: ReferenceScope =
                serde_json::from_str(&encoded).expect("scope must deserialize");
            assert_eq!(decoded, scope);
        }
    }

    #[test]
    fn reference_scope_constructor_and_serde_reject_invalid_values() {
        assert_eq!(
            ReferenceScope::available("aws", "", "123456789012").unwrap_err(),
            ReferenceScopeError::Empty {
                field: ReferenceScopeField::Region,
            }
        );
        assert_eq!(
            ReferenceScope::available("aws", "us-{name}-1", "123456789012").unwrap_err(),
            ReferenceScopeError::ContainsBrace {
                field: ReferenceScopeField::Region,
            }
        );
        assert!(
            serde_json::from_value::<ReferenceScope>(serde_json::json!({
                "kind": "available",
                "partition": "aws",
                "region": "",
                "account": "123456789012"
            }))
            .is_err(),
            "deserialization must enter through ReferenceScope::available",
        );
    }

    #[test]
    fn reference_template_accepts_the_four_declared_placeholders() {
        let raw = "arn:{partition}:lambda:{region}:{account}:function:{name}";
        let template = ReferenceTemplate::new(raw).expect("declared template must validate");

        assert_eq!(template.as_str(), raw);
    }

    #[test]
    fn reference_template_requires_exactly_one_name_placeholder() {
        for (raw, actual) in [
            ("arn:{partition}:lambda:{region}:{account}:function", 0),
            ("arn:{partition}:lambda:{name}:function:{name}", 2),
        ] {
            assert_eq!(
                ReferenceTemplate::new(raw).expect_err("invalid name count must be rejected"),
                ReferenceTemplateError::NamePlaceholderCount { actual },
            );
        }
    }

    #[test]
    fn reference_template_rejects_unknown_placeholders() {
        assert_eq!(
            ReferenceTemplate::new("arn:{partition}:lambda:{zone}:{account}:function:{name}")
                .expect_err("unknown placeholder must be rejected"),
            ReferenceTemplateError::UnknownPlaceholder {
                placeholder: "zone".to_string(),
            },
        );
    }

    #[test]
    fn reference_template_rejects_stray_braces() {
        for (raw, expected) in [
            (
                "arn:{partition:lambda:{region}:{account}:function:{name}",
                ReferenceTemplateError::StrayOpeningBrace,
            ),
            (
                "arn:{partition}:lambda:{region}}:{account}:function:{name}",
                ReferenceTemplateError::StrayClosingBrace,
            ),
        ] {
            assert_eq!(
                ReferenceTemplate::new(raw).expect_err("stray brace must be rejected"),
                expected,
            );
        }
    }

    #[test]
    fn reference_template_serde_cannot_bypass_validation() {
        let raw = "arn:{partition}:lambda:{region}:{account}:function:{name}";
        let template = ReferenceTemplate::new(raw).expect("declared template must validate");
        assert_eq!(
            serde_json::to_string(&template).expect("template must serialize as a string"),
            serde_json::to_string(raw).unwrap(),
        );
        assert_eq!(
            serde_json::from_str::<ReferenceTemplate>(&serde_json::to_string(raw).unwrap())
                .expect("valid template must deserialize"),
            template,
        );

        let invalid = serde_json::to_string("arn:{partition}:lambda:{name}:{name}").unwrap();
        assert!(
            serde_json::from_str::<ReferenceTemplate>(&invalid).is_err(),
            "Deserialize must enter through ReferenceTemplate::new",
        );
    }

    #[test]
    fn provider_schema_envelope_roundtrips_reference_form_records() {
        let identity = "aws.lambda.Function.Name".to_string();
        let form = ReferenceForm::Template {
            long: ReferenceTemplate::new(
                "arn:{partition}:lambda:{region}:{account}:function:{name}",
            )
            .unwrap(),
        };
        let envelope = ProviderSchemaEnvelope {
            resources: Vec::new(),
            reference_forms: vec![ReferenceFormDeclaration {
                identity: identity.clone(),
                form: form.clone(),
            }],
        };

        let json = serde_json::to_string(&envelope).expect("envelope must serialize");
        let decoded: ProviderSchemaEnvelope =
            serde_json::from_str(&json).expect("envelope must deserialize");
        assert!(decoded.resources.is_empty());
        assert_eq!(decoded.reference_forms.len(), 1);
        assert_eq!(decoded.reference_forms[0].identity, identity);
        assert_eq!(decoded.reference_forms[0].form, form);
    }

    #[test]
    fn in_flight_verdict_conversions_preserve_the_continuation() {
        let pending = PendingCreate {
            token: "next-token".to_string(),
            progress: CreateProgress::ReadingBack {
                identifier: AppliedIdentifier::new("resource-123").unwrap(),
            },
            sent: Sent::only(["name"]),
            poll_after_secs: 7,
        };
        let create: CreateVerdict<String, String, String> =
            CreateVerdict::InFlight(CreateInFlight {
                pending: pending.clone(),
                error: "interrupted".to_string(),
            });

        let poll: PollVerdict<String, String, String> = create.clone().into();
        assert_eq!(
            poll,
            PollVerdict::InFlight(CreateInFlight {
                pending: pending.clone(),
                error: "interrupted".to_string(),
            })
        );

        let resume: ResumeVerdict<String, String, String> = create.into();
        assert_eq!(
            resume,
            ResumeVerdict::Resent(CreateVerdict::InFlight(CreateInFlight {
                pending,
                error: "interrupted".to_string(),
            }))
        );
    }

    #[test]
    fn resume_in_flight_payload_roundtrips() {
        let in_flight = ResumeInFlight {
            pending: ResumePending {
                token: "resume-token".to_string(),
                progress: CreateProgress::Operating {
                    identifier: Some(AppliedIdentifier::new("resource-456").unwrap()),
                },
                poll_after_secs: 11,
            },
            error: "resume interrupted".to_string(),
        };

        let encoded = serde_json::to_string(&in_flight).expect("in-flight resume must serialize");
        let decoded: ResumeInFlight<String, String> =
            serde_json::from_str(&encoded).expect("in-flight resume must deserialize");
        assert_eq!(decoded, in_flight);
    }

    #[test]
    fn test_value_roundtrip() {
        let values = vec![
            Value::String("hello".into()),
            Value::Int(42),
            Value::Float(2.71),
            Value::Bool(true),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::Map(BTreeMap::from([(
                "key".into(),
                Value::String("val".into()),
            )])),
        ];

        for value in values {
            let json = serde_json::to_string(&value).unwrap();
            let back: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value, back);
        }
    }

    #[test]
    fn ordered_maps_preserve_v17_state_json_bytes() {
        assert_eq!(crate::PROTOCOL_VERSION, 17);
        let state = State {
            id: ResourceId {
                provider: "mock".into(),
                resource_type: "test.Widget".into(),
                identity: "example".into(),
            },
            identifier: Some("widget-123".into()),
            attributes: BTreeMap::from([
                ("z_name".into(), Value::String("example".into())),
                (
                    "a_settings".into(),
                    Value::Map(BTreeMap::from([
                        ("z_enabled".into(), Value::Bool(true)),
                        ("a_count".into(), Value::Int(2)),
                    ])),
                ),
            ]),
            exists: true,
        };

        assert_eq!(
            serde_json::to_vec(&state).expect("state must serialize"),
            br#"{"id":{"provider":"mock","resource_type":"test.Widget","identity":"example"},"identifier":"widget-123","attributes":{"a_settings":{"a_count":2,"z_enabled":true},"z_name":"example"},"exists":true}"#
        );
    }

    #[test]
    fn persisted_value_redacted_secret_uses_a_structural_json_tag() {
        let digest = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0MTIzNA$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let value = PersistedValue::RedactedSecret(digest.to_string());

        let json = serde_json::to_value(&value).expect("serialize redacted secret");
        assert_eq!(json, serde_json::json!({"$secret": digest}));
        assert_eq!(
            serde_json::from_value::<PersistedValue>(json).expect("deserialize redacted secret"),
            value
        );
    }

    #[test]
    fn value_marker_shaped_strings_never_gain_secret_semantics() {
        let marker = "_secret:$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0MTIzNA$aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        assert_eq!(
            serde_json::from_value::<Value>(serde_json::json!(marker))
                .expect("deserialize ordinary string"),
            Value::String(marker.to_string())
        );
    }

    #[test]
    fn value_deserialization_preserves_legacy_null_and_wide_number_fallbacks() {
        assert_eq!(
            serde_json::from_value::<Value>(serde_json::Value::Null)
                .expect("null retains its legacy empty-string decoding"),
            Value::String(String::new())
        );

        let number = serde_json::Number::from(u64::MAX);
        let expected = number.as_f64().unwrap_or(0.0);
        assert_eq!(
            Value::from_json(serde_json::Value::Number(number)),
            Value::Float(expected)
        );
    }

    #[test]
    fn ordinary_value_treats_persisted_shapes_as_ordinary_maps() {
        for json in [
            serde_json::json!({"$secret": 42}),
            serde_json::json!({"$secret": "digest", "extra": true}),
            serde_json::json!({"$reserved": "value"}),
        ] {
            let value = serde_json::from_value::<Value>(json).expect("ordinary map");
            assert!(matches!(value, Value::Map(_)));
        }
    }

    #[test]
    fn persisted_value_rejects_malformed_tags_and_unescaped_reserved_keys() {
        for json in [
            serde_json::json!({"$secret": 42}),
            serde_json::json!({"$secret": "digest", "extra": true}),
            serde_json::json!({"$reserved": "value"}),
        ] {
            assert!(
                serde_json::from_value::<PersistedValue>(json).is_err(),
                "malformed or unescaped persisted objects must be rejected"
            );
        }
    }

    #[test]
    fn persisted_value_maps_escape_dollar_keys_around_structural_secret_tags() {
        let value = PersistedValue::Map(BTreeMap::from([
            (
                "$secret".to_string(),
                PersistedValue::String("user".to_string()),
            ),
            (
                "typed".to_string(),
                PersistedValue::RedactedSecret("argon2-legacy:73616c74:aa".to_string()),
            ),
        ]));

        let json = serde_json::to_value(&value).expect("serialize value map");
        assert_eq!(json["$$secret"], "user");
        assert_eq!(
            json["typed"],
            serde_json::json!({"$secret": "argon2-legacy:73616c74:aa"})
        );
        assert_eq!(
            serde_json::from_value::<PersistedValue>(json).expect("deserialize value map"),
            value
        );
    }

    #[test]
    fn persisted_to_ordinary_conversion_is_recursive_and_rejects_redacted_secrets() {
        let ordinary = Value::Map(BTreeMap::from([(
            "$secret".to_string(),
            Value::List(vec![Value::String("user".to_string())]),
        )]));
        let persisted = PersistedValue::from(ordinary.clone());
        assert_eq!(persisted.to_value().unwrap(), ordinary);

        let secret =
            PersistedValue::List(vec![PersistedValue::RedactedSecret("digest".to_string())]);
        assert_eq!(secret.into_value(), Err(RedactedSecretValueError));
    }

    #[test]
    fn applied_outcomes_use_required_identifier_without_exists_flag() {
        let id = ResourceId {
            provider: "mock".into(),
            resource_type: "test.resource".into(),
            identity: "example".into(),
        };
        let create = CreateOutcome::Success {
            state: AppliedState {
                id: id.clone(),
                identifier: AppliedIdentifier::new("created-id")
                    .expect("fixture identifier is non-empty"),
                attributes: BTreeMap::new(),
            },
        };
        let read = ReadOutcome::Found {
            state: AppliedState {
                id: id.clone(),
                identifier: AppliedIdentifier::new("read-id")
                    .expect("fixture identifier is non-empty"),
                attributes: BTreeMap::new(),
            },
        };
        let update = UpdateOutcome::Success {
            state: AppliedPersistedState {
                id,
                identifier: AppliedIdentifier::new("updated-id")
                    .expect("fixture identifier is non-empty"),
                attributes: BTreeMap::new(),
            },
        };

        let create_json = serde_json::to_value(create).expect("serialize create outcome");
        let read_json = serde_json::to_value(read).expect("serialize read outcome");
        let update_json = serde_json::to_value(update).expect("serialize update outcome");
        assert_eq!(create_json["state"]["identifier"], "created-id");
        assert_eq!(read_json["state"]["identifier"], "read-id");
        assert_eq!(update_json["state"]["identifier"], "updated-id");
        assert!(create_json["state"].get("exists").is_none());
        assert!(read_json["state"].get("exists").is_none());
        assert!(update_json["state"].get("exists").is_none());
    }

    #[test]
    fn applied_identifier_deserialization_rejects_empty_string() {
        let error = serde_json::from_str::<AppliedIdentifier>("\"\"")
            .expect_err("protocol deserialization must preserve the non-empty invariant");
        assert!(error.to_string().contains("must not be empty"), "{error}");
    }

    #[test]
    fn update_request_builds_carried_forward_persisted_state() {
        let id = ResourceId {
            provider: "mock".into(),
            resource_type: "test.resource".into(),
            identity: "example".into(),
        };
        let request = UpdateRequest {
            from: PersistedState {
                id,
                identifier: Some("mock-id".into()),
                attributes: BTreeMap::from([
                    (
                        "token".into(),
                        PersistedValue::RedactedSecret("digest".into()),
                    ),
                    ("color".into(), PersistedValue::String("red".into())),
                ]),
                exists: true,
            },
            patch: UpdatePatch {
                ops: vec![PatchOp::Replace {
                    key: "color".into(),
                    value: Value::String("blue".into()),
                }],
            },
        };

        let state = request.state_after_patch();
        assert_eq!(
            state.attributes.get("token"),
            Some(&PersistedValue::RedactedSecret("digest".into()))
        );
        assert_eq!(
            state.attributes.get("color"),
            Some(&PersistedValue::String("blue".into()))
        );
    }

    #[test]
    fn update_patch_applies_all_operation_variants() {
        let mut state = PersistedState {
            id: ResourceId {
                provider: "mock".into(),
                resource_type: "test.resource".into(),
                identity: "example".into(),
            },
            identifier: Some("mock-id".into()),
            attributes: BTreeMap::from([
                (
                    "replace-target".into(),
                    PersistedValue::String("old".into()),
                ),
                (
                    "reassert-target".into(),
                    PersistedValue::String("old".into()),
                ),
                ("remove-target".into(), PersistedValue::String("old".into())),
            ]),
            exists: true,
        };
        state.apply_patch(&UpdatePatch {
            ops: vec![
                PatchOp::Add {
                    key: "add-target".into(),
                    value: Value::String("added".into()),
                },
                PatchOp::Replace {
                    key: "replace-target".into(),
                    value: Value::String("replaced".into()),
                },
                PatchOp::Reassert {
                    key: "reassert-target".into(),
                    value: Value::String("reasserted".into()),
                },
                PatchOp::Remove {
                    key: "remove-target".into(),
                },
            ],
        });

        assert_eq!(
            state.attributes.get("add-target"),
            Some(&PersistedValue::String("added".into()))
        );
        assert_eq!(
            state.attributes.get("replace-target"),
            Some(&PersistedValue::String("replaced".into()))
        );
        assert_eq!(
            state.attributes.get("reassert-target"),
            Some(&PersistedValue::String("reasserted".into()))
        );
        assert!(!state.attributes.contains_key("remove-target"));
    }

    #[test]
    fn patch_op_serde_round_trips_all_variants() {
        let operations = [
            PatchOp::Add {
                key: "added".into(),
                value: Value::String("a".into()),
            },
            PatchOp::Replace {
                key: "replaced".into(),
                value: Value::Int(2),
            },
            PatchOp::Reassert {
                key: "reasserted".into(),
                value: Value::Bool(true),
            },
            PatchOp::Remove {
                key: "removed".into(),
            },
        ];

        for operation in operations {
            let json = serde_json::to_value(&operation).expect("serialize patch operation");
            let back: PatchOp = serde_json::from_value(json).expect("deserialize patch operation");
            assert_eq!(back, operation);
        }
    }

    #[test]
    fn test_state_roundtrip() {
        let state = State {
            id: ResourceId {
                provider: "mock".into(),
                resource_type: "test.resource".into(),
                identity: "my-resource".into(),
            },
            identifier: Some("mock-id".into()),
            attributes: BTreeMap::from([("name".into(), Value::String("test".into()))]),
            exists: true,
        };

        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(state.id, back.id);
        assert_eq!(state.identifier, back.identifier);
        assert_eq!(state.exists, back.exists);
    }

    #[test]
    fn test_resource_id_deserializes_legacy_name() {
        let json = r#"{"provider":"mock","resource_type":"test.resource","name":"old-key"}"#;

        let id: ResourceId = serde_json::from_str(json).unwrap();

        assert_eq!(id.identity, "old-key");
    }

    #[test]
    fn test_attribute_type_roundtrip() {
        let attr = AttributeType::Struct {
            name: "Config".into(),
            fields: vec![StructField {
                name: "enabled".into(),
                field_type: AttributeType::Bool,
                required: true,
                description: None,
                block_name: None,
                provider_name: None,
                read_only: false,
                write_only: true,
                create_only: true,
                identity: IdentityMode::Required,
                deferred_populate: false,
            }],
            field_groups: vec![],
        };

        let json = serde_json::to_string(&attr).unwrap();
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        let AttributeType::Struct { fields, .. } = &back else {
            panic!("round-tripped attribute must remain a struct");
        };
        assert!(fields[0].write_only);
        assert!(fields[0].create_only);
        assert_eq!(fields[0].identity, IdentityMode::Required);
        assert_eq!(json, serde_json::to_string(&back).unwrap());
    }

    #[test]
    fn struct_field_missing_write_only_is_rejected() {
        let error = serde_json::from_str::<StructField>(
            r#"{
                "name":"provider_id",
                "field_type":{"type":"String"},
                "required":false
            }"#,
        )
        .expect_err("protocol v16 requires write_only on every struct field");

        assert!(error.to_string().contains("missing field `write_only`"));
    }

    #[test]
    fn protocol_v16_requires_nested_create_only_and_identity_mode() {
        let missing_create_only = serde_json::from_str::<StructField>(
            r#"{
                "name":"budget_name",
                "field_type":{"type":"String"},
                "required":false,
                "write_only":false,
                "identity":"required"
            }"#,
        )
        .expect_err("protocol v16 requires create_only on every struct field");
        assert!(
            missing_create_only
                .to_string()
                .contains("missing field `create_only`")
        );

        let missing_identity = serde_json::from_str::<StructField>(
            r#"{
                "name":"budget_name",
                "field_type":{"type":"String"},
                "required":false,
                "write_only":false,
                "create_only":true
            }"#,
        )
        .expect_err("protocol v16 requires identity on every struct field");
        assert!(
            missing_identity
                .to_string()
                .contains("missing field `identity`")
        );
    }

    #[test]
    fn attribute_schema_transport_flags_default_for_legacy_payloads() {
        let attr: AttributeSchema = serde_json::from_str(
            r#"{
                "name":"status",
                "attr_type":{"type":"String"},
                "required":false,
                "keyed_partial_update":null,
                "identity":"none"
            }"#,
        )
        .unwrap();
        assert!(!attr.deferred_populate);
    }

    #[test]
    fn protocol_v16_identity_mode_is_closed_and_required_on_every_schema_record() {
        for mode in [
            IdentityMode::None,
            IdentityMode::Required,
            IdentityMode::WhenPresent,
        ] {
            let json = serde_json::to_value(mode).expect("identity mode serializes");
            let decoded: IdentityMode =
                serde_json::from_value(json).expect("identity mode deserializes");
            assert_eq!(decoded, mode);
        }
        assert!(serde_json::from_value::<IdentityMode>(serde_json::json!("future")).is_err());

        let missing_attribute_identity =
            serde_json::from_value::<AttributeSchema>(serde_json::json!({
                "name": "name",
                "attr_type": {"type": "String"},
                "required": false,
                "keyed_partial_update": null
            }))
            .expect_err("protocol v16 requires identity on every attribute");
        assert!(
            missing_attribute_identity
                .to_string()
                .contains("missing field `identity`")
        );

        let missing_field_identity = serde_json::from_value::<StructField>(serde_json::json!({
            "name": "name",
            "field_type": {"type": "String"},
            "required": false,
            "write_only": false,
            "create_only": false
        }))
        .expect_err("protocol v16 requires identity on every struct field");
        assert!(
            missing_field_identity
                .to_string()
                .contains("missing field `identity`")
        );
    }

    #[test]
    fn attribute_schema_missing_keyed_partial_update_is_rejected() {
        let error = serde_json::from_str::<AttributeSchema>(
            r#"{
                "name":"settings",
                "attr_type":{"type":"list", "elements": "allow_duplicates","element_type":{"type":"String"}},
                "required":false
            }"#,
        )
        .expect_err("keyed_partial_update is required protocol metadata");

        assert!(
            error
                .to_string()
                .contains("missing field `keyed_partial_update`")
        );
    }

    #[test]
    fn attribute_schema_keyed_partial_update_round_trips_and_serializes_none() {
        for keyed_partial_update in [serde_json::Value::Null, serde_json::json!("key")] {
            let mut json = serde_json::json!({
                "name": "settings",
                "attr_type": {"type": "list", "elements": "allow_duplicates", "element_type": {"type": "String"}},
                "required": false,
                "keyed_partial_update": keyed_partial_update,
                "identity": "none",
            });
            let attr: AttributeSchema = serde_json::from_value(json.clone()).unwrap();
            json = serde_json::to_value(attr).unwrap();
            assert!(
                json.as_object()
                    .expect("attribute schema object")
                    .contains_key("keyed_partial_update"),
                "None must be serialized explicitly: {json}"
            );
            assert_eq!(json["keyed_partial_update"], keyed_partial_update);
        }
    }

    #[test]
    fn ref_attribute_type_roundtrip() {
        // carina#3340: cyclic CFN schemas emit `Ref { name }` at the
        // recursion point; the wire form must round-trip through
        // serde so the host can rebuild the structural Ref variant.
        let attr = AttributeType::Ref {
            name: "Statement".into(),
        };
        let json = serde_json::to_string(&attr).unwrap();
        assert_eq!(json, r#"{"type":"ref","name":"Statement"}"#);
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        match back {
            AttributeType::Ref { name } => assert_eq!(name, "Statement"),
            other => panic!("expected Ref, got {:?}", other),
        }
    }

    #[test]
    fn custom_pattern_and_length_round_trip() {
        let attr = AttributeType::Custom {
            name: "awscc.wafv2.WebACL.EntityDescription".to_string(),
            base: Box::new(string_attr()),
            pattern: Some("^[a-z]+$".to_string()),
            length: Some((Some(1), Some(9))),
            validate: None,
            to_dsl: None,
        };

        let json = serde_json::to_string(&attr).unwrap();
        assert!(json.contains("pattern"));
        assert!(json.contains("length"));
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        match back {
            AttributeType::Custom {
                pattern, length, ..
            } => {
                assert_eq!(pattern.as_deref(), Some("^[a-z]+$"));
                assert_eq!(length, Some((Some(1), Some(9))));
            }
            other => panic!("expected Custom, got {:?}", other),
        }
    }

    #[test]
    fn refined_primitive_attribute_types_round_trip() {
        let attrs = [
            AttributeType::String {
                pattern: Some("^[a-z]+$".to_string()),
                length: Some((Some(1), Some(63))),
                validate: Some(serde_json::json!(true)),
                to_dsl: Some(DslTransform::StripSuffix(".".to_string())),
                identity: Some("aws.route53.HostedZone.Name".to_string()),
            },
            AttributeType::Int {
                range: Some((Some(-1), Some(65535))),
                identity: Some("aws.ec2.Port".to_string()),
            },
            AttributeType::Float {
                range: Some((Some(0.0), Some(1.0))),
                identity: Some("awscc.wafv2.Rate".to_string()),
            },
            AttributeType::List {
                elements: ListElements::AllowDuplicates,
                element_type: Box::new(string_attr()),
                ordered: false,
                length: Some((Some(1), Some(5))),
                validate: Some(serde_json::json!(true)),
            },
        ];

        for attr in attrs {
            let json = serde_json::to_string(&attr).unwrap();
            let back: AttributeType = serde_json::from_str(&json).unwrap();
            assert_eq!(json, serde_json::to_string(&back).unwrap());
        }
    }

    #[test]
    fn primitive_attribute_types_default_missing_refinements() {
        for json in [
            r#"{"type":"String"}"#,
            r#"{"type":"Int"}"#,
            r#"{"type":"Float"}"#,
        ] {
            let attr: AttributeType = serde_json::from_str(json).unwrap();
            match attr {
                AttributeType::String {
                    pattern,
                    length,
                    validate,
                    to_dsl,
                    identity,
                } => {
                    assert!(pattern.is_none());
                    assert!(length.is_none());
                    assert!(validate.is_none());
                    assert!(to_dsl.is_none());
                    assert!(identity.is_none());
                }
                AttributeType::Int { range, identity } => {
                    assert!(range.is_none());
                    assert!(identity.is_none());
                }
                AttributeType::Float { range, identity } => {
                    assert!(range.is_none());
                    assert!(identity.is_none());
                }
                other => panic!("unexpected primitive default shape: {other:?}"),
            }
        }
    }

    #[test]
    fn list_inner_alias_deserializes_and_emits_element_type() {
        let json = r#"{
            "type":"list", "elements": "allow_duplicates",
            "inner":{"type":"String"},
            "ordered":false,
            "length":[1,3],
            "validate":true
        }"#;
        let attr: AttributeType = serde_json::from_str(json).unwrap();
        match &attr {
            AttributeType::List {
                elements,
                element_type,
                ordered,
                length,
                validate,
            } => {
                assert!(matches!(**element_type, AttributeType::String { .. }));
                assert!(!ordered);
                assert_eq!(*elements, ListElements::AllowDuplicates);
                assert_eq!(*length, Some((Some(1), Some(3))));
                assert_eq!(*validate, Some(serde_json::json!(true)));
            }
            other => panic!("expected List, got {other:?}"),
        }

        let emitted = serde_json::to_string(&attr).unwrap();
        assert!(emitted.contains("element_type"), "emitted: {emitted}");
        assert!(!emitted.contains("inner"), "emitted: {emitted}");
    }

    #[test]
    fn list_elements_refinement_is_required() {
        let error = serde_json::from_value::<AttributeType>(serde_json::json!({
            "type": "list",
            "element_type": { "type": "String" },
            "ordered": true
        }))
        .expect_err("a list declaration must explicitly carry its elements refinement");
        assert!(error.to_string().contains("elements"), "{error}");
    }

    #[test]
    fn resource_schema_unique_name_spec_deserializes_legacy_shapes() {
        let string_schema: ResourceSchema = serde_json::from_str(
            r#"{
                "resource_type":"ec2.SecurityGroup",
                "attributes":{},
                "field_groups":[],
                "default_prevent_destroy":false,
                "unique_name_attribute":"group_name"
            }"#,
        )
        .unwrap();
        assert_eq!(
            string_schema.unique_name,
            UniqueNameSpec::Attribute("group_name".to_string())
        );

        let null_schema: ResourceSchema = serde_json::from_str(
            r#"{
                "resource_type":"ec2.Vpc",
                "attributes":{},
                "field_groups":[],
                "default_prevent_destroy":false,
                "unique_name_attribute":null
            }"#,
        )
        .unwrap();
        assert_eq!(null_schema.unique_name, UniqueNameSpec::Conflicting);

        let absent_schema: ResourceSchema = serde_json::from_str(
            r#"{
                "resource_type":"ec2.Vpc",
                "attributes":{},
                "field_groups":[],
                "default_prevent_destroy":false
            }"#,
        )
        .unwrap();
        assert_eq!(absent_schema.unique_name, UniqueNameSpec::Conflicting);

        let alias_schema: ResourceSchema = serde_json::from_str(
            r#"{
                "resource_type":"logs.LogGroup",
                "attributes":{},
                "field_groups":[],
                "default_prevent_destroy":false,
                "name_attribute":"log_group_name"
            }"#,
        )
        .unwrap();
        assert_eq!(
            alias_schema.unique_name,
            UniqueNameSpec::Attribute("log_group_name".to_string())
        );
    }

    #[test]
    fn resource_schema_unique_name_spec_deserializes_new_tagged_shapes() {
        let attribute_schema: ResourceSchema = serde_json::from_str(
            r#"{
                "resource_type":"s3.Bucket",
                "attributes":{},
                "field_groups":[],
                "default_prevent_destroy":false,
                "unique_name_attribute":{"type":"attribute","value":"bucket_name"}
            }"#,
        )
        .unwrap();
        assert_eq!(
            attribute_schema.unique_name,
            UniqueNameSpec::Attribute("bucket_name".to_string())
        );

        let coexists_schema: ResourceSchema = serde_json::from_str(
            r#"{
                "resource_type":"ec2.Vpc",
                "attributes":{},
                "field_groups":[],
                "default_prevent_destroy":false,
                "unique_name_attribute":{"type":"coexisting"}
            }"#,
        )
        .unwrap();
        assert_eq!(coexists_schema.unique_name, UniqueNameSpec::Coexisting);

        let conflicting_schema: ResourceSchema = serde_json::from_str(
            r#"{
                "resource_type":"organizations.Organization",
                "attributes":{},
                "field_groups":[],
                "default_prevent_destroy":false,
                "unique_name_attribute":{"type":"conflicting"}
            }"#,
        )
        .unwrap();
        assert_eq!(conflicting_schema.unique_name, UniqueNameSpec::Conflicting);
    }

    #[test]
    fn resource_schema_unique_name_spec_serializes_legacy_compatible_shapes() {
        let cases = [
            (
                UniqueNameSpec::Attribute("bucket_name".to_string()),
                serde_json::json!("bucket_name"),
            ),
            (
                UniqueNameSpec::Coexisting,
                serde_json::json!({"type":"coexisting"}),
            ),
            (UniqueNameSpec::Conflicting, serde_json::Value::Null),
        ];

        for (unique_name, expected) in cases {
            let schema = ResourceSchema {
                resource_type: "s3.Bucket".into(),
                attributes: BTreeMap::new(),
                description: None,
                kind: SchemaKind::Managed,
                unique_name,
                operation_config: None,
                validators: vec![],
                field_groups: vec![],
                default_prevent_destroy: false,
                defs: std::collections::BTreeMap::new(),
            };

            let json = serde_json::to_value(&schema).unwrap();
            assert_eq!(json["unique_name_attribute"], expected);
        }
    }

    #[test]
    fn resource_schema_unique_name_spec_missing_attribute_value_reports_field() {
        let err = serde_json::from_str::<ResourceSchema>(
            r#"{
                "resource_type":"s3.Bucket",
                "attributes":{},
                "field_groups":[],
                "default_prevent_destroy":false,
                "unique_name_attribute":{"type":"attribute"}
            }"#,
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("unique_name_attribute"),
            "error should name the field: {err}"
        );
        assert!(
            err.contains("missing field `value`"),
            "error should name the missing value field: {err}"
        );
    }

    #[test]
    fn resource_schema_unique_name_spec_unknown_tag_reports_tag() {
        let err = serde_json::from_str::<ResourceSchema>(
            r#"{
                "resource_type":"ec2.Vpc",
                "attributes":{},
                "field_groups":[],
                "default_prevent_destroy":false,
                "unique_name_attribute":{"type":"coexists"}
            }"#,
        )
        .unwrap_err()
        .to_string();

        assert!(
            err.contains("unknown unique_name_attribute type `coexists`"),
            "error should name the unknown tag: {err}"
        );
    }

    #[test]
    fn resource_schema_defs_roundtrip() {
        // carina#3340: the `defs` map carries cyclic struct definitions
        // (`Statement` -> `AndStatement` -> `List<Statement>` ...).
        // Pin the wire shape so a host-side regression cannot silently
        // lose recursion-target schema information.
        let schema = ResourceSchema {
            resource_type: "awscc.wafv2.WebACL".into(),
            attributes: BTreeMap::new(),
            description: None,
            kind: SchemaKind::Managed,
            unique_name: UniqueNameSpec::Conflicting,
            operation_config: None,
            validators: vec![],
            field_groups: vec![],
            default_prevent_destroy: false,
            defs: std::collections::BTreeMap::from([(
                "Statement".to_string(),
                AttributeType::Struct {
                    name: "Statement".into(),
                    fields: vec![StructField {
                        name: "and_statement".into(),
                        field_type: AttributeType::List {
                            elements: ListElements::AllowDuplicates,
                            element_type: Box::new(AttributeType::Ref {
                                name: "Statement".into(),
                            }),
                            ordered: true,
                            length: None,
                            validate: None,
                        },
                        required: false,
                        description: None,
                        block_name: None,
                        provider_name: None,
                        read_only: false,
                        write_only: false,
                        create_only: false,
                        identity: IdentityMode::None,
                        deferred_populate: false,
                    }],
                    field_groups: vec![],
                },
            )]),
        };
        let json = serde_json::to_string(&schema).unwrap();
        let back: ResourceSchema = serde_json::from_str(&json).unwrap();
        assert_eq!(back.defs.len(), 1, "defs map lost during round-trip");
        assert!(back.defs.contains_key("Statement"));
    }

    #[test]
    fn duration_attribute_type_serializes_as_duration_tag() {
        // Providers serialize this via serde_json across the WIT
        // boundary; the host must reconstruct it from the
        // `{"type": "Duration"}` form. Pinning the wire shape is the
        // only thing that keeps the cross-boundary contract stable
        // (carina#3166).
        let attr = AttributeType::Duration;
        let json = serde_json::to_string(&attr).unwrap();
        assert_eq!(json, r#"{"type":"Duration"}"#);
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AttributeType::Duration));
    }

    #[test]
    fn test_provider_info_with_capabilities() {
        let info = ProviderInfo {
            name: "test".into(),
            display_name: "Test Provider".into(),
            capabilities: vec!["normalize_desired".into(), "normalize_state".into()],
            version: "1.2.3".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: ProviderInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.capabilities,
            vec!["normalize_desired", "normalize_state"]
        );
    }

    #[test]
    fn test_provider_info_without_capabilities_defaults_to_empty() {
        // Simulates deserializing a response from a plugin that doesn't send capabilities
        let json = r#"{"name":"old","display_name":"Old Provider","version":"1.0.0"}"#;
        let info: ProviderInfo = serde_json::from_str(json).unwrap();
        assert!(info.capabilities.is_empty());
    }

    #[test]
    fn test_provider_info_envelope_flat_roundtrip_and_default_protocol_version() {
        let envelope = ProviderInfoEnvelope {
            info: ProviderInfo {
                name: "test".into(),
                display_name: "Test Provider".into(),
                capabilities: vec!["normalize_desired".into()],
                version: "1.2.3".into(),
            },
            protocol_version: crate::PROTOCOL_VERSION,
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["name"], "test");
        assert_eq!(value["display_name"], "Test Provider");
        assert_eq!(value["capabilities"][0], "normalize_desired");
        assert_eq!(value["version"], "1.2.3");
        assert_eq!(value["protocol_version"], crate::PROTOCOL_VERSION);

        let back: ProviderInfoEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.info.name, "test");
        assert_eq!(back.info.display_name, "Test Provider");
        assert_eq!(back.info.capabilities, vec!["normalize_desired"]);
        assert_eq!(back.info.version, "1.2.3");
        assert_eq!(back.protocol_version, crate::PROTOCOL_VERSION);

        let old_json = r#"{"name":"old","display_name":"Old Provider","version":"1.0.0"}"#;
        let old: ProviderInfoEnvelope = serde_json::from_str(old_json).unwrap();
        assert_eq!(old.info.name, "old");
        assert_eq!(old.protocol_version, 0);
    }

    #[test]
    fn test_union_type_roundtrip() {
        let attr = AttributeType::Union {
            members: vec![
                AttributeType::Struct {
                    name: "IamPolicyPrincipal".into(),
                    fields: vec![StructField {
                        name: "service".into(),
                        field_type: string_attr(),
                        required: false,
                        description: None,
                        block_name: None,
                        provider_name: None,
                        read_only: false,
                        write_only: false,
                        create_only: false,
                        identity: IdentityMode::None,
                        deferred_populate: false,
                    }],
                    field_groups: vec![],
                },
                string_attr(),
            ],
        };

        let json = serde_json::to_string(&attr).unwrap();
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        assert_eq!(json, serde_json::to_string(&back).unwrap());
    }

    #[test]
    fn test_operation_config_serialization() {
        let config = OperationConfig {
            delete_timeout_secs: Some(1800),
            delete_max_retries: Some(24),
            create_timeout_secs: None,
            create_max_retries: None,
            update_timeout_secs: Some(900),
            read_back: ReadBackBudget::from_secs(120),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"delete_timeout_secs\":1800"));
        assert!(json.contains("\"update_timeout_secs\":900"));
        assert!(json.contains("\"read_back\":{\"timeout_secs\":120}"));
        assert!(!json.contains("create_timeout_secs"));

        let back: OperationConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.delete_timeout_secs, Some(1800));
        assert_eq!(back.create_timeout_secs, None);
        assert_eq!(back.update_timeout_secs, Some(900));
        assert_eq!(
            back.read_back
                .expect("read-back budget")
                .timeout_secs()
                .get(),
            120
        );
    }

    #[test]
    fn operation_config_budget_uses_kind_specific_timeout_or_default() {
        let config = OperationConfig {
            create_timeout_secs: Some(10),
            update_timeout_secs: Some(20),
            delete_timeout_secs: Some(30),
            ..OperationConfig::default()
        };

        assert_eq!(
            config.budget(crate::OperationKind::Create),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(
            config.budget(crate::OperationKind::Update),
            std::time::Duration::from_secs(20)
        );
        assert_eq!(
            config.budget(crate::OperationKind::Delete),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            OperationConfig::default().budget(crate::OperationKind::Create),
            crate::DEFAULT_OPERATION_BUDGET
        );
        assert_eq!(
            OperationConfig::default().read_back_budget(),
            crate::DEFAULT_READ_BACK_BUDGET
        );
    }

    #[test]
    fn read_back_budget_rejects_zero_seconds() {
        assert!(ReadBackBudget::from_secs(0).is_none());
        assert!(
            serde_json::from_str::<ReadBackBudget>(r#"{"timeout_secs":0}"#).is_err(),
            "deserialization must not bypass the non-zero budget type",
        );
    }

    #[test]
    fn test_resource_schema_with_operation_config() {
        let json = r#"{"resource_type":"ec2.tgw","attributes":{},"field_groups":[],"default_prevent_destroy":false,"operation_config":{"delete_timeout_secs":1800}}"#;
        let schema: ResourceSchema = serde_json::from_str(json).unwrap();
        assert_eq!(
            schema.operation_config.unwrap().delete_timeout_secs,
            Some(1800)
        );
    }

    #[test]
    fn test_resource_schema_without_operation_config() {
        let json = r#"{"resource_type":"ec2.Vpc","attributes":{},"field_groups":[],"default_prevent_destroy":false}"#;
        let schema: ResourceSchema = serde_json::from_str(json).unwrap();
        assert!(schema.operation_config.is_none());
    }

    /// carina#2831: `StringEnum.dsl_aliases` carries the API->DSL spelling
    /// map that the host-side validator consults. Round-trips through
    /// JSON so a WASM provider's serialized schema reaches the host with
    /// the field intact.
    #[test]
    fn string_enum_dsl_aliases_round_trip() {
        let attr = AttributeType::StringEnum {
            name: "ObjectOwnership".to_string(),
            values: vec![
                "ObjectWriter".to_string(),
                "BucketOwnerEnforced".to_string(),
            ],
            namespace: Some("awscc.s3.Bucket".to_string()),
            dsl_aliases: vec![
                ("ObjectWriter".to_string(), "object_writer".to_string()),
                (
                    "BucketOwnerEnforced".to_string(),
                    "bucket_owner_enforced".to_string(),
                ),
            ],
        };
        let json = serde_json::to_string(&attr).unwrap();
        assert!(json.contains("dsl_aliases"));
        assert!(json.contains("bucket_owner_enforced"));
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        match back {
            AttributeType::StringEnum {
                dsl_aliases: aliases,
                ..
            } => {
                assert_eq!(aliases.len(), 2);
                assert!(
                    aliases
                        .iter()
                        .any(|(a, d)| a == "BucketOwnerEnforced" && d == "bucket_owner_enforced"),
                    "alias pair lost in round trip: {aliases:?}"
                );
            }
            other => panic!("expected StringEnum, got {other:?}"),
        }
    }

    /// `dsl_aliases` defaults to empty so older WASM providers that
    /// predate this field (carina#2831 stage 1) still parse — they
    /// emit no `dsl_aliases` key and the host treats every value as
    /// already in DSL form.
    #[test]
    fn string_enum_without_dsl_aliases_defaults_empty() {
        let json = r#"{"type":"string_enum","values":["A","B"],"name":"X"}"#;
        let attr: AttributeType = serde_json::from_str(json).unwrap();
        match attr {
            AttributeType::StringEnum {
                dsl_aliases: aliases,
                ..
            } => {
                assert!(
                    aliases.is_empty(),
                    "missing dsl_aliases must deserialize as empty vec"
                );
            }
            other => panic!("expected StringEnum, got {other:?}"),
        }
    }

    /// Empty `dsl_aliases` skips serialization so the wire format
    /// stays compact for the common case (no aliases) and older host
    /// builds (which do not know the field) round-trip unchanged.
    #[test]
    fn string_enum_empty_dsl_aliases_skipped_on_serialize() {
        let attr = AttributeType::StringEnum {
            name: "X".to_string(),
            values: vec!["A".to_string()],
            namespace: None,
            dsl_aliases: vec![],
        };
        let json = serde_json::to_string(&attr).unwrap();
        assert!(
            !json.contains("dsl_aliases"),
            "empty dsl_aliases must be omitted, got: {json}"
        );
    }

    #[test]
    fn custom_enum_dsl_transform_round_trip() {
        let transforms = [
            DslTransform::Identity,
            DslTransform::HyphenToUnderscore,
            DslTransform::StripSuffix(".".to_string()),
            DslTransform::ReplaceTable(vec![("all".to_string(), "-1".to_string())]),
        ];

        for transform in transforms {
            let attr = AttributeType::CustomEnum {
                name: "ZoneName".to_string(),
                base: Box::new(string_attr()),
                namespace: "aws.AvailabilityZone".to_string(),
                dsl_transform: Some(transform.clone()),
            };
            let json = serde_json::to_string(&attr).unwrap();
            let back: AttributeType = serde_json::from_str(&json).unwrap();
            match back {
                AttributeType::CustomEnum { dsl_transform, .. } => {
                    assert_eq!(dsl_transform, Some(transform));
                }
                other => panic!("expected CustomEnum, got {other:?}"),
            }
        }
    }

    #[test]
    fn dsl_transform_replace_table_applies_first_match() {
        let transform = DslTransform::ReplaceTable(vec![
            ("-1".to_string(), "all".to_string()),
            ("tcp".to_string(), "tcp".to_string()),
        ]);
        assert_eq!(transform.apply("-1"), "all");
        assert_eq!(transform.apply("udp"), "udp");
    }

    #[test]
    fn dsl_transform_no_change_paths_borrow_input() {
        let identity = DslTransform::Identity;
        let result = identity.apply("unchanged");
        assert!(matches!(result, Cow::Borrowed("unchanged")));

        let unknown = DslTransform::Unknown(serde_json::json!({"type":"FutureTransform"}));
        let result = unknown.apply("foo.bar.");
        assert!(matches!(result, Cow::Borrowed("foo.bar.")));

        let strip_suffix = DslTransform::StripSuffix(".".to_string());
        let result = strip_suffix.apply("foo");
        assert!(matches!(result, Cow::Borrowed("foo")));

        let replace_table = DslTransform::ReplaceTable(vec![("-1".to_string(), "all".to_string())]);
        let result = replace_table.apply("udp");
        assert!(matches!(result, Cow::Borrowed("udp")));
    }

    #[test]
    fn dsl_transform_unknown_unit_deserializes_as_identity() {
        let json = r#"{"type":"FutureTransform"}"#;
        let transform: DslTransform = serde_json::from_str(json).unwrap();
        assert_eq!(
            transform,
            DslTransform::Unknown(serde_json::json!({"type":"FutureTransform"}))
        );
        assert_eq!(transform.apply("foo.bar."), "foo.bar.");
    }

    #[test]
    fn dsl_transform_unknown_sequence_payload_deserializes_as_identity() {
        let json = r#"{"type":"FutureTransform","value":[["a","b"]]}"#;
        let transform: DslTransform = serde_json::from_str(json).unwrap();
        assert_eq!(
            transform,
            DslTransform::Unknown(serde_json::json!({
                "type": "FutureTransform",
                "value": [["a", "b"]]
            }))
        );
        assert_eq!(transform.apply("foo.bar."), "foo.bar.");
    }

    #[test]
    fn custom_enum_unknown_sequence_payload_deserializes_without_schema_failure() {
        let json = r#"{
            "type":"custom_enum",
            "name":"ZoneName",
            "base":{"type":"String"},
            "namespace":"aws.AvailabilityZone",
            "dsl_transform":{"type":"FutureTransform","value":[["a","b"]]}
        }"#;
        let attr: AttributeType = serde_json::from_str(json).unwrap();
        match attr {
            AttributeType::CustomEnum { dsl_transform, .. } => {
                assert!(matches!(
                    dsl_transform,
                    Some(DslTransform::Unknown(raw))
                        if raw == serde_json::json!({
                            "type": "FutureTransform",
                            "value": [["a", "b"]]
                        })
                ));
            }
            other => panic!("expected CustomEnum, got {other:?}"),
        }
    }

    #[test]
    fn custom_enum_unknown_struct_payload_deserializes_without_schema_failure() {
        let json = r#"{
            "type":"custom_enum",
            "name":"ZoneName",
            "base":{"type":"String"},
            "namespace":"aws.AvailabilityZone",
            "dsl_transform":{"type":"Other","value":{"x":1}}
        }"#;
        let attr: AttributeType = serde_json::from_str(json).unwrap();
        match attr {
            AttributeType::CustomEnum { dsl_transform, .. } => {
                assert!(matches!(
                    dsl_transform,
                    Some(DslTransform::Unknown(raw))
                        if raw == serde_json::json!({
                            "type": "Other",
                            "value": {"x": 1}
                        })
                ));
            }
            other => panic!("expected CustomEnum, got {other:?}"),
        }
    }

    #[test]
    fn dsl_transform_unknown_serializes_losslessly() {
        let raw = serde_json::json!({
            "type": "FutureTransform",
            "value": [["a", "b"]]
        });
        let transform = DslTransform::Unknown(raw.clone());
        let json = serde_json::to_string(&transform).unwrap();
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back, raw);
    }

    #[test]
    fn pending_token_debug_is_redacted() {
        let token = PendingToken::new("do-not-log-this");
        let debug = format!("{token:?}");

        assert_eq!(debug, "PendingToken([REDACTED])");
        assert!(!debug.contains("do-not-log-this"));
        assert_eq!(
            token.expose_for_provider_call(),
            "do-not-log-this",
            "the raw value is available only at the explicit provider-call seam"
        );
    }

    #[test]
    fn delete_request_retains_default_for_provider_compatibility() {
        let request = DeleteRequest::default();

        assert!(!request.directives.force_delete);
        assert!(!request.directives.create_before_destroy);
        assert!(!request.directives.prevent_destroy);
    }
}
