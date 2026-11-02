//! Value conversion and formatting utilities

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::convert::Infallible;

use indexmap::IndexMap;
use thiserror::Error;

use crate::parser::{display_double_quoted_literal_part, display_string_literal};
use crate::resource::{
    ApplyTimeUnknown, BindingAddress, CanonicalEnumValue, ConcreteValue, DeferredValue,
    InterpolationPart, LoopSlot, PlaceholderKind, RedactedSecretHash, UnavailableCoalescePolicy,
    UnavailableSecretPolicy, Value, find_unavailable_evaluation,
};
use crate::schema::{AttrTypeKind, CompiledAttributeType as AttributeType, TypeIdentity};
use crate::utils::is_identifier_safe;

/// Where in the pipeline a `Value` is being serialized. Used so the
/// caller of a failing serialization (e.g. `--out plan.json`) can
/// tell the user *which* boundary refused the value, not just that
/// some boundary did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SerializationContext {
    /// JSON conversion of a `Value` (the shared helper used by both
    /// the plan-file write path and arbitrary callers).
    ValueToJson,
    /// Recursive secret-redaction walk over a `Value` tree.
    SecretRedaction,
    /// State backend write path (after apply).
    StateWriteback,
    /// WASM provider boundary (`core_to_wit_value` and the JSON
    /// fallback used to inspect provider input/output).
    WasmBoundary,
}

impl std::fmt::Display for SerializationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ValueToJson => write!(f, "JSON conversion"),
            Self::SecretRedaction => write!(f, "secret redaction"),
            Self::StateWriteback => write!(f, "state writeback"),
            Self::WasmBoundary => write!(f, "WASM provider boundary"),
        }
    }
}

/// Error produced when a `Value` cannot be serialized for transport
/// out of the planner (provider/state/plan-file).
///
/// The `UnknownNotAllowed` variant carries the structured
/// [`ApplyTimeUnknown`] (rather than a stringified rendition) so the
/// top-level CLI handler can build an actionable diagnostic — e.g.
/// it can mention the specific upstream path or the for-binding kind
/// that produced the placeholder, without re-parsing a flattened
/// message string.
#[derive(Debug, Error)]
pub enum SerializationError {
    /// A parser-only module conditional reached a runtime or persistence
    /// boundary. Module expansion must select every branch first.
    #[error("cannot serialize at {context}: unresolved module-template conditional")]
    UnexpandedModuleIf { context: SerializationContext },
    /// A `Value::Deferred(DeferredValue::Unknown)` reached a serialization boundary. Producers
    /// must strip / resolve it before this point — see
    /// [`crate::resource::value_contains_unknown`] for recursive
    /// detection before crossing a serialization boundary.
    #[error("cannot serialize at {context}: value is not yet known ({reason})")]
    UnknownNotAllowed {
        reason: Box<ApplyTimeUnknown>,
        context: SerializationContext,
    },
    /// A non-finite float (`NaN`, `±∞`) reached JSON serialization.
    /// JSON has no representation for these.
    #[error("cannot serialize at {context}: non-finite float {value}")]
    NonFiniteFloat {
        value: f64,
        context: SerializationContext,
    },
    /// A `Value::Deferred(DeferredValue::ResourceRef)` reached a serialization boundary that
    /// expected a concrete value. Resolvers must substitute the
    /// reference before this point. Reaching this arm at apply-time
    /// state writeback or plan-file write is a resolver bug.
    ///
    /// `path` is stored as a pre-formatted `String` rather than the
    /// structured `AccessPath` (cf. `ApplyTimeUnknown::UpstreamExport`)
    /// because `SerializationError` is terminating diagnostic data
    /// consumed only via `Display`; programmatic path inspection has
    /// no callers today. Lift to `AccessPath` if a future caller needs
    /// it.
    #[error("cannot serialize at {context}: unresolved reference {path}")]
    UnresolvedResourceRef {
        path: String,
        context: SerializationContext,
    },
    /// A `Value::Deferred(DeferredValue::Interpolation)` reached a serialization boundary. The
    /// canonicalize pass should collapse interpolations to a `String`
    /// once all parts resolve; reaching this arm means a part stayed
    /// unresolved through apply-time export resolution.
    #[error("cannot serialize at {context}: unresolved interpolation")]
    UnresolvedInterpolation { context: SerializationContext },
    /// A `Value::Deferred(DeferredValue::FunctionCall)` reached a serialization boundary. The
    /// resolver should evaluate the function (built-in or user-defined)
    /// before this point; reaching this arm is a resolver bug.
    #[error("cannot serialize at {context}: unresolved function call {name}(...)")]
    UnresolvedFunctionCall {
        name: String,
        context: SerializationContext,
    },
    /// A deferred postfix field/index access reached a concrete-only
    /// serialization boundary before its base resolved.
    #[error("cannot serialize at {context}: unresolved access {expression}")]
    UnresolvedAccess {
        expression: String,
        context: SerializationContext,
    },
    /// A coalesce expression reached a concrete-only serialization boundary
    /// before its left operand became present or definitively absent.
    #[error("cannot serialize at {context}: unresolved coalesce expression")]
    UnresolvedCoalesce { context: SerializationContext },
    /// A typed integer expression reached a concrete-only serialization
    /// boundary before both operands resolved.
    #[error("cannot serialize at {context}: unresolved integer arithmetic expression")]
    UnresolvedIntegerArithmetic { context: SerializationContext },
    /// A parser-authored enum identifier reached a serialization boundary
    /// without schema resolution. Provider transports accept only
    /// [`CanonicalEnumValue`] API spellings; plain strings remain literals.
    #[error("cannot serialize at {context}: unresolved enum identifier {value}")]
    UnresolvedEnumIdentifier {
        value: String,
        context: SerializationContext,
    },
    /// A secret reached a boundary where no safe representation exists.
    /// Resource state and provider transport have dedicated hash/unseal
    /// semantics; export state does not, because upstream consumers require
    /// the original value.
    #[error("cannot serialize at {context}: secret values are not allowed")]
    SecretNotAllowed { context: SerializationContext },
    /// A secret was found in a named saved-plan field that cannot safely
    /// persist plaintext or a placeholder.
    #[error("cannot serialize saved-plan field {location}: secret values are not allowed")]
    SavedPlanFieldSecretNotAllowed { location: String },
    /// A typed redacted-secret placeholder reached a serialization boundary
    /// that accepts only ordinary or plaintext values.
    #[error("cannot serialize at {context}: redacted secret is not allowed at this boundary")]
    RedactedSecretNotAllowed { context: SerializationContext },
}

impl SerializationError {
    /// Mask user-derived payload text when an error originated below a
    /// [`DeferredValue::Secret`] wrapper.
    #[must_use]
    pub fn masked_for_secret(self) -> Self {
        const MASKED: &str = "(secret)";

        match self {
            Self::UnexpandedModuleIf { context } => Self::UnexpandedModuleIf { context },
            Self::UnresolvedEnumIdentifier { context, .. } => Self::UnresolvedEnumIdentifier {
                value: MASKED.to_string(),
                context,
            },
            Self::UnresolvedResourceRef { context, .. } => Self::UnresolvedResourceRef {
                path: MASKED.to_string(),
                context,
            },
            Self::UnresolvedFunctionCall { context, .. } => Self::UnresolvedFunctionCall {
                name: MASKED.to_string(),
                context,
            },
            Self::UnresolvedAccess { context, .. } => Self::UnresolvedAccess {
                expression: MASKED.to_string(),
                context,
            },
            Self::UnresolvedCoalesce { context } => Self::UnresolvedCoalesce { context },
            Self::UnresolvedIntegerArithmetic { context } => {
                Self::UnresolvedIntegerArithmetic { context }
            }
            other => other,
        }
    }
}

/// Why a saved-plan secret placeholder could not be restored.
#[derive(Debug, Error)]
pub enum SecretRehydrationError {
    /// The current configuration no longer evaluates to a secret at the
    /// position recorded by the saved plan.
    #[error("the current value is no longer a secret")]
    ExpectedSecret,
    /// The current secret hashes differently from the value used to create
    /// the saved plan.
    #[error("the secret changed since the plan was created")]
    HashMismatch,
    /// A container holding a placeholder changed shape, so there is no value
    /// at the recorded path that can be verified.
    #[error("the value containing the secret changed shape")]
    ShapeMismatch,
    /// The expression still depends on an unavailable runtime input (for
    /// example an unset environment variable or missing decryptor key).
    #[error("the current secret expression could not be evaluated")]
    EvaluationUnavailable,
    /// A state-only resource carrying a historical placeholder disappeared
    /// before its planned delete could verify the secret.
    #[error("the orphan resource no longer exists")]
    OrphanVanished,
    /// Hashing the candidate failed because it was not serializable.
    #[error(transparent)]
    Serialization(#[from] SerializationError),
}

/// A rehydration failure annotated with its saved-plan location.
#[derive(Debug)]
pub struct SecretRehydrationSiteError {
    pub location: String,
    pub source: SecretRehydrationError,
}

impl SecretRehydrationSiteError {
    pub fn new(location: impl Into<String>, source: SecretRehydrationError) -> Self {
        Self {
            location: location.into(),
            source,
        }
    }
}

impl std::fmt::Display for SecretRehydrationSiteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.source)
    }
}

impl std::error::Error for SecretRehydrationSiteError {}

impl<B: BindingAddress> std::fmt::Display for ApplyTimeUnknown<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyTimeUnknown::UpstreamExport { path } => {
                write!(f, "upstream value {}", path.to_dot_string())
            }
            ApplyTimeUnknown::UpstreamAllExports { binding } => {
                write!(f, "upstream value {}", binding)
            }
            ApplyTimeUnknown::PendingReRead(pending) => pending.fmt(f),
        }
    }
}

impl std::fmt::Display for crate::resource::PendingReRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.reason() {
            crate::resource::ReReadReason::Incomplete { detail } => {
                write!(f, "read after apply was incomplete: {detail}")
            }
            crate::resource::ReReadReason::ReadFailed { detail } => {
                write!(f, "read after apply failed: {detail}")
            }
            crate::resource::ReReadReason::NotFound => {
                f.write_str("resource was not found during read after apply")
            }
        }
    }
}

/// Render an `ApplyTimeUnknown` to its plan-display string.
pub fn render_unknown<B: BindingAddress>(reason: &ApplyTimeUnknown<B>) -> String {
    match reason {
        ApplyTimeUnknown::UpstreamExport { path } => {
            format!("(known after upstream apply: {})", path.to_dot_string())
        }
        ApplyTimeUnknown::UpstreamAllExports { binding } => {
            format!("(known after upstream apply: {})", binding)
        }
        ApplyTimeUnknown::PendingReRead(pending) => match pending.reason() {
            crate::resource::ReReadReason::Incomplete { detail } => {
                format!("(unknown until next refresh: read after apply was incomplete — {detail})")
            }
            crate::resource::ReReadReason::ReadFailed { detail } => {
                format!("(unknown until next refresh: read after apply failed — {detail})")
            }
            crate::resource::ReReadReason::NotFound => {
                "(unknown until next refresh: resource was not found during read after apply)"
                    .to_string()
            }
        },
    }
}

pub use carina_provider_protocol::PERSISTED_SECRET_KEY;
const PERSISTED_ENUM_KEY: &str = "$enum";
const LEGACY_ENUM_KEY: &str = "Enum";

#[derive(Clone, Copy)]
enum JsonEncoding {
    /// Provider/function JSON: preserve user keys and send enum API values.
    Ordinary,
    /// State/export JSON: reserve `$` keys and retain canonical enum identity.
    Persisted,
    /// Pre-v13 representation retained only for stable secret hash material.
    LegacySecretMaterial,
}

impl JsonEncoding {
    fn context(self) -> SerializationContext {
        match self {
            Self::Ordinary => SerializationContext::ValueToJson,
            Self::Persisted => SerializationContext::StateWriteback,
            Self::LegacySecretMaterial => SerializationContext::SecretRedaction,
        }
    }
}

/// A persisted JSON object used the host-reserved `$` namespace incorrectly.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PersistedValueDecodeError {
    #[error("invalid host-reserved persisted JSON object with keys {keys:?}")]
    InvalidHostObject { keys: Vec<String> },
    #[error("malformed {PERSISTED_ENUM_KEY} persisted enum payload")]
    MalformedEnum,
    #[error("malformed {PERSISTED_SECRET_KEY} persisted secret payload")]
    MalformedSecret,
}

/// A complete resource-state value tree in Carina's persisted JSON encoding.
///
/// Construction validates the reserved `$` namespace, so state/export
/// containers cannot hold an ambiguous or malformed host payload. Ordinary
/// JSON remains a separate type and is decoded with the ordinary JSON codec.
/// Raw JSON construction is intentionally limited to serde state loading:
///
/// ```compile_fail
/// use carina_core::value::PersistedValue;
///
/// let ordinary_json = serde_json::json!({ "$$cost": 10 });
/// let _persisted = PersistedValue::try_from(ordinary_json);
/// ```
#[cfg_attr(
    not(feature = "test-utils"),
    doc = r#"
The raw persisted JSON is not part of the default public API:

```compile_fail
use carina_core::value::PersistedValue;

fn raw_json(value: &PersistedValue) -> &serde_json::Value {
    value.as_json()
}
```
"#
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedValue {
    encoded: serde_json::Value,
}

impl PersistedValue {
    /// Encode provider/history state, preserving typed redacted secret hashes
    /// and reusing a previous hash at the same value path when supplied.
    pub fn encode_provider_value(
        value: &Value,
        previous: Option<&Self>,
    ) -> Result<Self, SerializationError> {
        value_to_json_inner(
            value,
            previous.map(Self::view),
            true,
            JsonEncoding::Persisted,
            true,
        )
        .map(|encoded| Self { encoded })
    }

    /// Encode a desired state value, rejecting historical redacted-secret
    /// placeholders while still hashing live `Secret` values.
    pub fn encode_desired_value(
        value: &Value,
        previous: Option<&Self>,
    ) -> Result<Self, SerializationError> {
        Self::encode_desired_value_from_view(value, previous.map(Self::view))
    }

    fn encode_desired_value_from_view(
        value: &Value,
        previous: Option<PersistedView<'_>>,
    ) -> Result<Self, SerializationError> {
        value_to_json_inner(value, previous, true, JsonEncoding::Persisted, false)
            .map(|encoded| Self { encoded })
    }

    /// Decode through the persisted codec. Validation at construction makes
    /// this infallible; JSON null continues to mean an absent DSL value.
    pub fn decode(&self) -> Option<Value> {
        persisted_json_to_dsl_value(&self.encoded, PersistedPurpose::State)
            .expect("PersistedValue validates the reserved namespace at construction")
    }

    /// Borrow the encoded bytes for backend serialization and secret-hash
    /// reconciliation. Value-layer consumers should use [`Self::decode`].
    #[cfg(not(any(test, feature = "test-utils")))]
    pub(crate) fn as_json(&self) -> &serde_json::Value {
        &self.encoded
    }

    /// Borrow the encoded JSON in tests and test-support code.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn as_json(&self) -> &serde_json::Value {
        &self.encoded
    }

    /// Decode host tags and key escaping back into ordinary JSON.
    ///
    /// This is intended for JSON/raw display surfaces. Persisted secrets
    /// become the fixed string `(secret)`, canonical enums become their
    /// provider API value, and user `$` keys regain their original spelling.
    pub fn decoded_json(&self) -> serde_json::Value {
        decode_persisted_json(&self.encoded, PersistedPurpose::State)
            .expect("PersistedValue validates the reserved namespace at construction")
            .into_ordinary_json()
    }

    pub(crate) fn view(&self) -> PersistedView<'_> {
        PersistedView {
            encoded: &self.encoded,
        }
    }

    pub(crate) fn from_serialized_json(
        encoded: serde_json::Value,
    ) -> Result<Self, PersistedValueDecodeError> {
        decode_persisted_json(&encoded, PersistedPurpose::State)?;
        Ok(Self { encoded })
    }

    pub(crate) fn saved_plan_secret_inventory(value: &Value) -> Option<Self> {
        saved_plan_secret_hashes_json(value).map(|encoded| Self { encoded })
    }
}

impl serde::Serialize for PersistedValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(self.as_json(), serializer)
    }
}

impl<'de> serde::Deserialize<'de> for PersistedValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded: serde_json::Value = serde::Deserialize::deserialize(deserializer)?;
        Self::from_serialized_json(encoded).map_err(serde::de::Error::custom)
    }
}

/// A complete export value tree in Carina's persisted JSON encoding.
///
/// Export strings never acquire secret semantics from their contents. This
/// distinct container type also rejects structural secret tags for remote-state
/// reads and export display.
#[cfg_attr(
    not(feature = "test-utils"),
    doc = r#"
The raw persisted JSON is not part of the default public API:

```compile_fail
use carina_core::value::PersistedExportValue;

fn raw_json(value: &PersistedExportValue) -> &serde_json::Value {
    value.as_json()
}
```
"#
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedExportValue {
    encoded: serde_json::Value,
}

impl PersistedExportValue {
    pub fn encode(value: &Value) -> Result<Self, SerializationError> {
        if contains_unredacted_secret(value) {
            return Err(SerializationError::SecretNotAllowed {
                context: SerializationContext::StateWriteback,
            });
        }
        value_to_json_inner(value, None, false, JsonEncoding::Persisted, false)
            .map(|encoded| Self { encoded })
    }

    pub fn decode(&self) -> Option<Value> {
        persisted_json_to_dsl_value(&self.encoded, PersistedPurpose::Export)
            .expect("PersistedExportValue validates the reserved namespace at construction")
    }

    #[cfg(not(any(test, feature = "test-utils")))]
    pub(crate) fn as_json(&self) -> &serde_json::Value {
        &self.encoded
    }

    /// Borrow the encoded JSON in tests and test-support code.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn as_json(&self) -> &serde_json::Value {
        &self.encoded
    }

    pub fn decoded_json(&self) -> serde_json::Value {
        decode_persisted_json(&self.encoded, PersistedPurpose::Export)
            .expect("PersistedExportValue validates the reserved namespace at construction")
            .into_ordinary_json()
    }

    pub(crate) fn view(&self) -> PersistedView<'_> {
        PersistedView {
            encoded: &self.encoded,
        }
    }

    fn from_serialized_json(encoded: serde_json::Value) -> Result<Self, PersistedValueDecodeError> {
        decode_persisted_json(&encoded, PersistedPurpose::Export)?;
        Ok(Self { encoded })
    }
}

impl serde::Serialize for PersistedExportValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(self.as_json(), serializer)
    }
}

impl<'de> serde::Deserialize<'de> for PersistedExportValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded: serde_json::Value = serde::Deserialize::deserialize(deserializer)?;
        Self::from_serialized_json(encoded).map_err(serde::de::Error::custom)
    }
}

/// A borrowed, validated subtree of the persisted encoding.
///
/// Traversal accepts DSL keys and applies persisted-key escaping internally,
/// preventing state-guided walkers from accidentally addressing encoded JSON
/// with an unescaped user key.
#[derive(Clone, Copy)]
pub(crate) struct PersistedView<'a> {
    encoded: &'a serde_json::Value,
}

#[derive(Clone, Copy)]
pub(crate) struct PersistedMapView<'a> {
    values: &'a serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone, Copy)]
pub(crate) struct PersistedListView<'a> {
    values: &'a [serde_json::Value],
}

impl<'a> PersistedView<'a> {
    pub(crate) fn list_child(self, index: usize) -> Option<Self> {
        self.as_list()?.get(index)
    }

    pub(crate) fn as_map(self) -> Option<PersistedMapView<'a>> {
        let values = self.encoded.as_object()?;
        if values
            .keys()
            .any(|key| key.starts_with('$') && !key.starts_with("$$"))
        {
            return None;
        }
        Some(PersistedMapView { values })
    }

    pub(crate) fn as_list(self) -> Option<PersistedListView<'a>> {
        self.encoded
            .as_array()
            .map(|values| PersistedListView { values })
    }

    pub(crate) fn redacted_secret_hash(self) -> Option<RedactedSecretHash> {
        let values = self.encoded.as_object()?;
        if values.len() != 1 {
            return None;
        }
        values
            .get(PERSISTED_SECRET_KEY)?
            .as_str()
            .and_then(RedactedSecretHash::from_encoded_digest)
    }

    pub(crate) fn is_redacted_secret(self) -> bool {
        self.redacted_secret_hash().is_some()
    }

    pub(crate) fn is_null(self) -> bool {
        self.encoded.is_null()
    }

    pub(crate) fn decode_value(self) -> Option<Value> {
        persisted_json_to_dsl_value(self.encoded, PersistedPurpose::State)
            .expect("PersistedView is a subtree of a validated persisted value")
    }

    pub(crate) fn to_owned_json(self) -> serde_json::Value {
        self.encoded.clone()
    }

    pub(crate) fn encoded_eq(self, other: Self) -> bool {
        self.encoded == other.encoded
    }
}

impl<'a> PersistedMapView<'a> {
    pub(crate) fn get(self, dsl_key: &str) -> Option<PersistedView<'a>> {
        let encoded_key = escape_persisted_map_key(dsl_key);
        self.values
            .get(encoded_key.as_ref())
            .map(|encoded| PersistedView { encoded })
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = (&'a str, PersistedView<'a>)> + Clone + 'a {
        self.values.iter().map(|(key, encoded)| {
            (
                key.strip_prefix('$').unwrap_or(key),
                PersistedView { encoded },
            )
        })
    }

    pub(crate) fn replacing_children(
        self,
        replacements: impl IntoIterator<Item = (String, serde_json::Value)>,
    ) -> serde_json::Value {
        let mut values = self.values.clone();
        for (dsl_key, value) in replacements {
            values.insert(escape_persisted_map_key(&dsl_key).into_owned(), value);
        }
        serde_json::Value::Object(values)
    }
}

impl<'a> PersistedListView<'a> {
    pub(crate) fn get(self, index: usize) -> Option<PersistedView<'a>> {
        self.values
            .get(index)
            .map(|encoded| PersistedView { encoded })
    }

    pub(crate) fn len(self) -> usize {
        self.values.len()
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = PersistedView<'a>> + Clone + 'a {
        self.values.iter().map(|encoded| PersistedView { encoded })
    }

    pub(crate) fn replacing_children(
        self,
        replacements: impl IntoIterator<Item = (usize, serde_json::Value)>,
    ) -> serde_json::Value {
        let mut values = self.values.to_vec();
        for (index, value) in replacements {
            if let Some(slot) = values.get_mut(index) {
                *slot = value;
            }
        }
        serde_json::Value::Array(values)
    }
}

/// Convert `Value` to ordinary JSON.
///
/// This does not apply the persisted `$` namespace. Canonical enums become
/// their provider API string and user map keys are left unchanged. State
/// writers use [`PersistedValue`]; export writers use [`PersistedExportValue`].
///
/// Returns an error if `value` contains a non-finite float (NaN or infinity)
/// because JSON cannot represent these values.
///
#[cfg(any(test, feature = "test-utils"))]
pub fn value_to_json(value: &Value) -> Result<serde_json::Value, SerializationError> {
    value_to_json_inner(value, None, true, JsonEncoding::Ordinary, false)
}

fn value_to_json_inner(
    value: &Value,
    previous: Option<PersistedView<'_>>,
    persist_secrets: bool,
    encoding: JsonEncoding,
    allow_redacted_secrets: bool,
) -> Result<serde_json::Value, SerializationError> {
    let ctx = encoding.context();
    match value {
        Value::Deferred(DeferredValue::ModuleIf(_)) => {
            Err(SerializationError::UnexpandedModuleIf { context: ctx })
        }
        Value::Concrete(ConcreteValue::String(s)) => {
            // Both serialize as a flat JSON string. The schema-aware
            // state loader re-classifies `EnumIdentifier` from the
            // attribute's declared type, so the on-disk JSON stays
            // unchanged. See carina#2986 design doc §5.
            Ok(serde_json::Value::String(s.expose_plaintext().to_owned()))
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => {
            Ok(serde_json::Value::String(s.to_string()))
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(c)) => match encoding {
            JsonEncoding::Ordinary => Ok(serde_json::Value::String(c.api_value().to_string())),
            JsonEncoding::Persisted => Ok(canonical_enum_to_json(c, PERSISTED_ENUM_KEY)),
            JsonEncoding::LegacySecretMaterial => Ok(canonical_enum_to_json(c, LEGACY_ENUM_KEY)),
        },
        Value::Concrete(ConcreteValue::Int(n)) => Ok(serde_json::Value::Number((*n).into())),
        Value::Concrete(ConcreteValue::Duration(d)) => {
            Ok(serde_json::Value::Number((d.as_secs() as i64).into()))
        }
        Value::Concrete(ConcreteValue::Float(f)) => {
            let num =
                serde_json::Number::from_f64(*f).ok_or(SerializationError::NonFiniteFloat {
                    value: *f,
                    context: ctx,
                })?;
            Ok(serde_json::Value::Number(num))
        }
        Value::Concrete(ConcreteValue::Bool(b)) => Ok(serde_json::Value::Bool(*b)),
        Value::Concrete(ConcreteValue::List(items)) => {
            let previous = previous.and_then(PersistedView::as_list);
            let arr: Result<Vec<_>, _> = items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    value_to_json_inner(
                        item,
                        previous.and_then(|previous| previous.get(index)),
                        persist_secrets,
                        encoding,
                        allow_redacted_secrets,
                    )
                })
                .collect();
            Ok(serde_json::Value::Array(arr?))
        }
        Value::Concrete(ConcreteValue::StringList(items)) => Ok(serde_json::Value::Array(
            items
                .iter()
                .map(|s| serde_json::Value::String(s.expose_plaintext().to_owned()))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let previous = previous.and_then(PersistedView::as_map);
            let obj: Result<serde_json::Map<_, _>, _> = map
                .iter()
                .map(|(key, value)| {
                    let encoded_key = if matches!(encoding, JsonEncoding::Persisted) {
                        escape_persisted_map_key(key).into_owned()
                    } else {
                        key.clone()
                    };
                    value_to_json_inner(
                        value,
                        previous.and_then(|previous| previous.get(key)),
                        persist_secrets,
                        encoding,
                        allow_redacted_secrets,
                    )
                    .map(|json| (encoded_key, json))
                })
                .collect();
            Ok(serde_json::Value::Object(obj?))
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            Err(SerializationError::UnresolvedResourceRef {
                path: path.to_dot_string(),
                context: ctx,
            })
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            Err(SerializationError::UnresolvedResourceRef {
                // A bare-binding reference is by construction never a
                // resolved value — it must be substituted by the resolver
                // pass before reaching any serialization boundary. Report
                // through the same channel as `ResourceRef` so the same
                // diagnostic path covers both producer kinds.
                path: binding.to_string(),
                context: ctx,
            })
        }
        Value::Deferred(DeferredValue::Coalesce { .. }) => {
            Err(SerializationError::UnresolvedCoalesce { context: ctx })
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(_)) => {
            Err(SerializationError::UnresolvedIntegerArithmetic { context: ctx })
        }
        Value::Deferred(DeferredValue::Interpolation(_)) => {
            Err(SerializationError::UnresolvedInterpolation { context: ctx })
        }
        Value::Deferred(
            DeferredValue::FunctionCall { name, .. } | DeferredValue::Unavailable { name, .. },
        ) => Err(SerializationError::UnresolvedFunctionCall {
            name: name.to_string(),
            context: ctx,
        }),
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) => {
            Err(SerializationError::UnresolvedFunctionCall {
                name: function.name.clone(),
                context: ctx,
            })
        }
        Value::Deferred(DeferredValue::Access(_)) => Err(SerializationError::UnresolvedAccess {
            expression: format_value(value),
            context: ctx,
        }),
        Value::Deferred(DeferredValue::Secret(inner)) => {
            if persist_secrets {
                let material = canonical_secret_hash_material(inner)?;
                let previous = previous.and_then(PersistedView::redacted_secret_hash);
                Ok(persisted_secret_json(
                    &RedactedSecretHash::reuse_or_generate(&material, previous.as_ref()),
                ))
            } else {
                value_to_json_inner(inner, None, false, encoding, allow_redacted_secrets)
            }
        }
        Value::Deferred(DeferredValue::RedactedSecret { hash }) if allow_redacted_secrets => {
            Ok(persisted_secret_json(hash))
        }
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => {
            Err(SerializationError::RedactedSecretNotAllowed { context: ctx })
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            Err(SerializationError::UnknownNotAllowed {
                reason: Box::new(reason.clone()),
                context: ctx,
            })
        }
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
    }
}

/// Encode one materialized secret, reusing a matching v2 digest when present.
///
/// This is the single digest-selection point for state writeback. Callers walk
/// containers and pass the previous value at the same path.
pub(crate) fn persist_secret_json(
    plaintext: &Value,
    previous: Option<PersistedView<'_>>,
) -> Result<serde_json::Value, SerializationError> {
    let material = canonical_secret_hash_material(plaintext)?;
    let previous = previous.and_then(PersistedView::redacted_secret_hash);
    Ok(persisted_secret_json(
        &RedactedSecretHash::reuse_or_generate(&material, previous.as_ref()),
    ))
}

fn persisted_secret_json(hash: &RedactedSecretHash) -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::from_iter([(
        PERSISTED_SECRET_KEY.to_string(),
        serde_json::Value::String(hash.encoded_digest()),
    )]))
}

/// Extract only redacted-secret leaves from a saved-plan value in persisted
/// encoding. Maps omit unrelated siblings; lists retain their indexes with
/// null placeholders so later hash lookup addresses the same path.
fn saved_plan_secret_hashes_json(value: &Value) -> Option<serde_json::Value> {
    match value {
        Value::Deferred(DeferredValue::RedactedSecret { hash }) => {
            Some(persisted_secret_json(hash))
        }
        Value::Concrete(ConcreteValue::Map(values)) => {
            let values: serde_json::Map<_, _> = values
                .iter()
                .filter_map(|(key, value)| {
                    saved_plan_secret_hashes_json(value)
                        .map(|value| (escape_persisted_map_key(key).into_owned(), value))
                })
                .collect();
            (!values.is_empty()).then_some(serde_json::Value::Object(values))
        }
        Value::Concrete(ConcreteValue::List(values)) => {
            let mut found = false;
            let values = values
                .iter()
                .map(|value| {
                    saved_plan_secret_hashes_json(value).map_or(serde_json::Value::Null, |value| {
                        found = true;
                        value
                    })
                })
                .collect();
            found.then_some(serde_json::Value::Array(values))
        }
        // The operand placeholder hashes the secret input, not the eventual
        // arithmetic result. It is therefore not valid previous-state hash
        // authority for that result; rehydration handles it structurally.
        Value::Deferred(DeferredValue::IntegerArithmetic(_)) => None,
        _ => None,
    }
}

/// Produce the only byte representation that may be hashed as secret material.
///
/// Secret wrappers are sensitivity metadata, not plaintext: nested wrappers
/// are recursively unwrapped. Deferred values retain their stable template
/// representation for saved-plan verification.
pub(crate) fn canonical_secret_hash_material<P: PlaceholderKind>(
    value: &Value<P>,
) -> Result<Vec<u8>, SerializationError> {
    // Secret hash material predates the structural persisted encoding. Keep
    // these bytes stable even though state now carries digests in `$secret`
    // tags; identifiers and saved hashes depend on this representation.
    let canonical = match value.to_expanded() {
        None => canonical_secret_template(value)?,
        Some(runtime_value) => match value_to_json_inner(
            &runtime_value,
            None,
            false,
            JsonEncoding::LegacySecretMaterial,
            false,
        ) {
            Ok(json) => json,
            Err(
                SerializationError::UnknownNotAllowed { .. }
                | SerializationError::UnresolvedResourceRef { .. }
                | SerializationError::UnresolvedInterpolation { .. }
                | SerializationError::UnresolvedFunctionCall { .. }
                | SerializationError::UnresolvedAccess { .. }
                | SerializationError::UnresolvedCoalesce { .. }
                | SerializationError::SecretNotAllowed { .. },
            ) => canonical_secret_template(value)?,
            Err(error) => return Err(error),
        },
    };
    serde_json::to_vec(&canonical).map_err(|_| SerializationError::SecretNotAllowed {
        context: SerializationContext::SecretRedaction,
    })
}

fn canonical_enum_to_json(c: &CanonicalEnumValue, tag: &str) -> serde_json::Value {
    let identity = c.identity();
    let payload = serde_json::json!({
        "identity": {
            "provider": identity.provider.clone(),
            "segments": identity.segments.clone(),
            "kind": identity.kind.clone(),
        },
        "api_value": c.api_value(),
    });
    serde_json::Value::Object(serde_json::Map::from_iter([(tag.to_string(), payload)]))
}

fn escape_persisted_map_key(key: &str) -> Cow<'_, str> {
    if key.starts_with('$') {
        Cow::Owned(format!("${key}"))
    } else {
        Cow::Borrowed(key)
    }
}

/// Upgrade one value tree written by state format v12 or earlier.
///
/// The legacy enum recognition deliberately matches the old decoder: an
/// outer object containing only `Enum`, with all required typed identity and
/// `api_value` fields, is treated as the enum it used to decode into. Its
/// payload is re-emitted canonically. Every other object is user data, so `$`
/// keys are escaped recursively.
pub fn migrate_legacy_persisted_json(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                migrate_legacy_persisted_json(value);
            }
        }
        serde_json::Value::Object(values) => {
            if values.len() == 1
                && let Some(serde_json::Value::Object(payload)) = values.get(LEGACY_ENUM_KEY)
                && let Some(canonical) = canonical_enum_from_payload(payload, false)
            {
                *value = canonical_enum_to_json(&canonical, PERSISTED_ENUM_KEY);
                return;
            }

            let old = std::mem::take(values);
            *values = old
                .into_iter()
                .map(|(key, mut value)| {
                    migrate_legacy_persisted_json(&mut value);
                    (escape_persisted_map_key(&key).into_owned(), value)
                })
                .collect();
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

/// Convert `serde_json::Value` to DSL `Value`.
///
/// Returns `None` for JSON null, since null represents a missing/unset value
/// rather than a meaningful attribute value. Callers should filter out `None`
/// entries when building attribute maps. `$` keys are ordinary user data here;
/// only [`PersistedValue`] interprets the persisted host namespace.
#[cfg(any(test, feature = "test-utils"))]
pub fn json_to_dsl_value(json: &serde_json::Value) -> Option<Value> {
    json_to_dsl_value_inner(json)
}

/// Convert persisted state JSON to a DSL value, lifting structural `$secret`
/// tags into their opaque typed representation. Versioned state migrations
/// rewrite legacy string encodings before this conversion runs.
fn persisted_json_to_dsl_value(
    json: &serde_json::Value,
    purpose: PersistedPurpose,
) -> Result<Option<Value>, PersistedValueDecodeError> {
    decode_persisted_json(json, purpose).map(DecodedPersistedJson::into_dsl_value)
}

#[cfg(any(test, feature = "test-utils"))]
fn json_to_dsl_value_inner(json: &serde_json::Value) -> Option<Value> {
    match json {
        serde_json::Value::String(s) => {
            Some(Value::Concrete(ConcreteValue::String(s.clone().into())))
        }
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Value::Concrete(ConcreteValue::Int(i)))
            } else {
                Some(Value::Concrete(ConcreteValue::Float(
                    n.as_f64().unwrap_or(0.0),
                )))
            }
        }
        serde_json::Value::Bool(b) => Some(Value::Concrete(ConcreteValue::Bool(*b))),
        serde_json::Value::Array(items) => Some(Value::Concrete(ConcreteValue::List(
            items.iter().filter_map(json_to_dsl_value_inner).collect(),
        ))),
        serde_json::Value::Object(map) => {
            let m: IndexMap<_, _> = map
                .iter()
                .filter_map(|(k, v)| json_to_dsl_value_inner(v).map(|val| (k.clone(), val)))
                .collect();
            Some(Value::Concrete(ConcreteValue::Map(m)))
        }
        serde_json::Value::Null => None,
    }
}

#[derive(Clone, Copy)]
enum PersistedPurpose {
    State,
    Export,
}

enum DecodedPersistedJson {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Enum(CanonicalEnumValue),
    Secret(RedactedSecretHash),
    Array(Vec<Self>),
    Object(IndexMap<String, Self>),
}

impl DecodedPersistedJson {
    fn into_dsl_value(self) -> Option<Value> {
        match self {
            Self::Null => None,
            Self::Bool(value) => Some(Value::Concrete(ConcreteValue::Bool(value))),
            Self::Number(value) => {
                let value = if let Some(value) = value.as_i64() {
                    ConcreteValue::Int(value)
                } else {
                    ConcreteValue::Float(value.as_f64().unwrap_or(0.0))
                };
                Some(Value::Concrete(value))
            }
            Self::String(value) => Some(Value::Concrete(ConcreteValue::String(value.into()))),
            Self::Enum(value) => Some(Value::Concrete(ConcreteValue::CanonicalEnum(value))),
            Self::Secret(hash) => Some(Value::Deferred(DeferredValue::RedactedSecret { hash })),
            Self::Array(items) => Some(Value::Concrete(ConcreteValue::List(
                items.into_iter().filter_map(Self::into_dsl_value).collect(),
            ))),
            Self::Object(values) => Some(Value::Concrete(ConcreteValue::Map(
                values
                    .into_iter()
                    .filter_map(|(key, value)| value.into_dsl_value().map(|value| (key, value)))
                    .collect(),
            ))),
        }
    }

    fn into_ordinary_json(self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(value) => serde_json::Value::Bool(value),
            Self::Number(value) => serde_json::Value::Number(value),
            Self::String(value) => serde_json::Value::String(value),
            Self::Enum(value) => serde_json::Value::String(value.api_value().to_string()),
            Self::Secret(_) => serde_json::Value::String("(secret)".to_string()),
            Self::Array(values) => {
                serde_json::Value::Array(values.into_iter().map(Self::into_ordinary_json).collect())
            }
            Self::Object(values) => serde_json::Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, value.into_ordinary_json()))
                    .collect(),
            ),
        }
    }
}

fn decode_persisted_json(
    json: &serde_json::Value,
    purpose: PersistedPurpose,
) -> Result<DecodedPersistedJson, PersistedValueDecodeError> {
    match json {
        serde_json::Value::Null => Ok(DecodedPersistedJson::Null),
        serde_json::Value::Bool(value) => Ok(DecodedPersistedJson::Bool(*value)),
        serde_json::Value::Number(value) => Ok(DecodedPersistedJson::Number(value.clone())),
        serde_json::Value::String(value) => Ok(DecodedPersistedJson::String(value.clone())),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| decode_persisted_json(value, purpose))
            .collect::<Result<Vec<_>, _>>()
            .map(DecodedPersistedJson::Array),
        serde_json::Value::Object(values) => {
            let host_keys: Vec<_> = values
                .keys()
                .filter(|key| key.starts_with('$') && !key.starts_with("$$"))
                .cloned()
                .collect();
            if !host_keys.is_empty() {
                if values.len() == 1 && host_keys.as_slice() == [PERSISTED_SECRET_KEY] {
                    if matches!(purpose, PersistedPurpose::Export) {
                        return Err(PersistedValueDecodeError::InvalidHostObject {
                            keys: values.keys().cloned().collect(),
                        });
                    }
                    let digest = values
                        .get(PERSISTED_SECRET_KEY)
                        .and_then(serde_json::Value::as_str)
                        .ok_or(PersistedValueDecodeError::MalformedSecret)?;
                    let hash = RedactedSecretHash::from_encoded_digest(digest)
                        .ok_or(PersistedValueDecodeError::MalformedSecret)?;
                    return Ok(DecodedPersistedJson::Secret(hash));
                }
                if values.len() == 1 && host_keys.as_slice() == [PERSISTED_ENUM_KEY] {
                    let canonical = json_to_canonical_enum(values)
                        .ok_or(PersistedValueDecodeError::MalformedEnum)?;
                    return Ok(DecodedPersistedJson::Enum(canonical));
                }
                return Err(PersistedValueDecodeError::InvalidHostObject {
                    keys: values.keys().cloned().collect(),
                });
            }

            values
                .iter()
                .map(|(key, value)| {
                    let key = key.strip_prefix('$').unwrap_or(key).to_string();
                    decode_persisted_json(value, purpose).map(|value| (key, value))
                })
                .collect::<Result<IndexMap<_, _>, _>>()
                .map(DecodedPersistedJson::Object)
        }
    }
}

fn json_to_canonical_enum(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Option<CanonicalEnumValue> {
    let serde_json::Value::Object(payload) = map.get(PERSISTED_ENUM_KEY)? else {
        return None;
    };
    if map.len() != 1 {
        return None;
    }

    canonical_enum_from_payload(payload, true)
}

fn canonical_enum_from_payload(
    payload: &serde_json::Map<String, serde_json::Value>,
    require_canonical_shape: bool,
) -> Option<CanonicalEnumValue> {
    if require_canonical_shape && payload.len() != 2 {
        return None;
    }
    let serde_json::Value::Object(identity) = payload.get("identity")? else {
        return None;
    };
    if require_canonical_shape && identity.len() != 3 {
        return None;
    }
    let provider = match identity.get("provider")? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        _ => return None,
    };
    let serde_json::Value::Array(segments) = identity.get("segments")? else {
        return None;
    };
    let segments: Vec<String> = segments
        .iter()
        .map(|segment| match segment {
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    let kind = identity.get("kind")?.as_str()?.to_string();
    let api_value = payload.get("api_value")?.as_str()?.to_string();

    Some(CanonicalEnumValue::from_trusted_state(
        TypeIdentity::new(provider, segments, kind),
        api_value,
    ))
}

/// Format a `Value` for display
pub fn format_value<P: PlaceholderKind, B: BindingAddress>(value: &Value<P, B>) -> String {
    format_value_with_key(value, None)
}

/// Semantic category attached to a fragment of a rendered value.
///
/// Frontends use this metadata for styling; they must not infer value
/// structure or types by parsing the rendered Carina source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSpanKind {
    StringLiteral,
    Bool,
    Number,
    Enum,
    Reference,
    Structure,
    Key,
    Whitespace,
    Other,
}

/// One semantically typed fragment of a [`RenderedValue`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueSpan {
    kind: ValueSpanKind,
    text: String,
}

impl ValueSpan {
    pub fn kind(&self) -> ValueSpanKind {
        self.kind
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// A value rendered by Carina's canonical formatter, retaining enough
/// structure for frontends to style it without re-parsing the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedValue {
    plain: String,
    spans: Vec<ValueSpan>,
}

impl RenderedValue {
    pub fn plain(&self) -> &str {
        &self.plain
    }

    pub fn spans(&self) -> &[ValueSpan] {
        &self.spans
    }
}

impl std::fmt::Display for RenderedValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.plain())
    }
}

/// A user-provided map key rendered with Carina's canonical map-key rule.
///
/// The rendered text is private so display IR cannot carry an unencoded raw
/// key. Callers may construct this type from a raw key, but cannot bypass the
/// identifier-safe/string-literal decision made here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedKey {
    raw: String,
    text: String,
}

impl RenderedKey {
    pub fn from_raw(key: &str) -> Self {
        Self {
            raw: key.to_string(),
            text: if is_identifier_safe(key) {
                key.to_string()
            } else {
                display_string_literal(key)
            },
        }
    }

    pub fn plain(&self) -> &str {
        &self.text
    }

    pub(crate) fn raw(&self) -> &str {
        &self.raw
    }
}

impl std::fmt::Display for RenderedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.plain())
    }
}

/// A value retained for indentation-aware rendering without exposing the raw
/// [`Value`] to display frontends.
#[derive(Debug, Clone, PartialEq)]
pub struct PrettyValue {
    value: Value,
}

impl PrettyValue {
    pub fn new(value: Value) -> Self {
        Self { value }
    }

    pub fn render(&self, layout: PrettyLayout) -> RenderedValue {
        render_value_pretty(&self.value, layout)
    }

    pub fn needs_trailing_separator(&self) -> bool {
        needs_trailing_separator(&self.value)
    }

    #[cfg(test)]
    pub(crate) fn value(&self) -> &Value {
        &self.value
    }
}

/// Render a value together with semantic span metadata.
pub fn render_value(value: &Value) -> RenderedValue {
    let mut sink = SpanSink::default();
    format_value_into(value, &mut sink).expect("SpanSink writes are infallible");
    sink.rendered
}

/// Render a persisted export without assigning secret semantics to strings.
///
/// JSON null has no corresponding [`Value`] variant, so it is emitted as an
/// `Other` token. Every representable value uses the same token kinds and
/// canonical string/map-key encoding as [`render_value`].
pub fn render_export_json_value(value: &PersistedExportValue) -> RenderedValue {
    render_persisted_json(value.view())
}

fn render_persisted_json(value: PersistedView<'_>) -> RenderedValue {
    let mut sink = SpanSink::default();
    let value = decode_persisted_json(value.encoded, PersistedPurpose::Export)
        .expect("persisted value types validate the reserved namespace at construction");
    format_decoded_persisted_json_into(&value, &mut sink).expect("SpanSink writes are infallible");
    sink.rendered
}

/// Render a value for a deferred-for template, replacing typed upstream
/// placeholders while the value tree is still available. No rendered text is
/// inspected to identify the placeholder.
pub(crate) fn render_value_with_upstream_deferred_label<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    upstream_binding: &str,
    verb: &str,
) -> RenderedValue {
    let mut sink = SpanSink::default();
    let context = FormatContext {
        upstream_deferred_label: Some((upstream_binding, verb)),
    };
    format_value_into_context(value, &mut sink, context).expect("SpanSink writes are infallible");
    sink.rendered
}

fn render_single_span(kind: ValueSpanKind, text: impl Into<String>) -> RenderedValue {
    let text = text.into();
    let spans = (!text.is_empty())
        .then(|| ValueSpan {
            kind,
            text: text.clone(),
        })
        .into_iter()
        .collect();
    RenderedValue { plain: text, spans }
}

/// Wrap display-only text that is not a Carina value literal in the typed
/// rendering container. This is reserved for placeholders and identifiers
/// whose bespoke human form intentionally differs from `format_value`.
pub(crate) fn render_other(text: impl Into<String>) -> RenderedValue {
    render_single_span(ValueSpanKind::Other, text)
}

pub(crate) fn render_enum_text(text: impl Into<String>) -> RenderedValue {
    render_single_span(ValueSpanKind::Enum, text)
}

pub(crate) fn render_string_text(text: &str) -> RenderedValue {
    render_single_span(ValueSpanKind::StringLiteral, display_string_literal(text))
}

/// Format a `Value` for user-visible messages where DSL literal quotes
/// around bare strings get in the way.
pub fn format_value_user_facing<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
) -> String {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => value.expose_plaintext().to_string(),
        _ => format_value(value),
    }
}

/// Format a `Value` for display, with an optional key for context
pub fn format_value_with_key<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    _key: Option<&str>,
) -> String {
    let mut sink = StringSink::default();
    // `StringSink::write_token` never returns Err (it has no overflow
    // budget), so the `Result` here is structurally `Ok(())`. Treat the
    // unwrap as proof of that — never an actual fallible path.
    format_value_into(value, &mut sink).expect("StringSink writes are infallible");
    sink.buf
}

/// Marker returned by [`FormatSink::write_token`] when a budget-bounded
/// sink (e.g. [`WidthCounter`]) has exceeded its limit. [`StringSink`]
/// never returns this.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Overflow;

/// Output sink for [`format_value_into`]. The trait lets the same
/// formatting code path drive the public [`format_value`] text, the semantic
/// spans returned by [`render_value`], and the [`WidthCounter`] used by
/// [`render_value_pretty`]'s inline-vs-vertical decision. #2434.
///
/// Historically the byte-length path was its own arm-by-arm
/// `inline_width` mirror of `format_value`, kept consistent by
/// a parity test. That shape silently rotted when a `Value` variant was
/// added on one side but not the other; the trait fuses both into a
/// single source of truth so a new variant only needs one update.
pub(crate) trait FormatSink {
    /// Append `s` to the sink. `Err(Overflow)` short-circuits the
    /// whole render — used by the budget-bounded sink to bail out as
    /// soon as the running width exceeds the limit, without visiting
    /// the rest of the value tree.
    fn write_token(&mut self, kind: ValueSpanKind, s: &str) -> Result<(), Overflow>;
}

#[derive(Default)]
pub(crate) struct StringSink {
    pub(crate) buf: String,
}

impl FormatSink for StringSink {
    fn write_token(&mut self, _kind: ValueSpanKind, s: &str) -> Result<(), Overflow> {
        self.buf.push_str(s);
        Ok(())
    }
}

struct SpanSink {
    rendered: RenderedValue,
}

impl Default for SpanSink {
    fn default() -> Self {
        Self {
            rendered: RenderedValue {
                plain: String::new(),
                spans: Vec::new(),
            },
        }
    }
}

impl FormatSink for SpanSink {
    fn write_token(&mut self, kind: ValueSpanKind, s: &str) -> Result<(), Overflow> {
        if s.is_empty() {
            return Ok(());
        }
        self.rendered.plain.push_str(s);
        if let Some(last) = self.rendered.spans.last_mut()
            && last.kind == kind
        {
            last.text.push_str(s);
        } else {
            self.rendered.spans.push(ValueSpan {
                kind,
                text: s.to_string(),
            });
        }
        Ok(())
    }
}

/// Counts byte length of writes, returning `Err(Overflow)` once the
/// running total exceeds `budget`. Pairs with [`format_value_into`] to
/// answer "would this value's inline form fit in N bytes?" without
/// allocating the rendered string.
pub(crate) struct WidthCounter {
    running: usize,
    budget: usize,
}

impl WidthCounter {
    pub(crate) fn new(budget: usize) -> Self {
        Self { running: 0, budget }
    }
    pub(crate) fn width(&self) -> usize {
        self.running
    }
}

impl FormatSink for WidthCounter {
    fn write_token(&mut self, _kind: ValueSpanKind, s: &str) -> Result<(), Overflow> {
        let next = self.running.checked_add(s.len()).ok_or(Overflow)?;
        if next > self.budget {
            return Err(Overflow);
        }
        self.running = next;
        Ok(())
    }
}

fn write_map_key<S: FormatSink + ?Sized>(key: &str, sink: &mut S) -> Result<(), Overflow> {
    let rendered = RenderedKey::from_raw(key);
    sink.write_token(ValueSpanKind::Key, rendered.plain())
}

/// Render a `Duration` to its canonical surface form.
///
/// Picks the largest unit that divides the duration cleanly:
/// `3600s` → `1h`, `60s` → `1min`, anything else → `Ns`. The original
/// authoring unit is not preserved (`Value::Concrete(ConcreteValue::Duration)` carries only a
/// `std::time::Duration`), so this is a deterministic re-rendering
/// rule — not a faithful round-trip.
///
/// Used by every value-tree consumer: plan display, hover, deferred-
/// for / export display, builtin-error messages, and `Display for
/// Value`. The source-text formatter (`carina fmt`) currently passes
/// duration literals through verbatim — see #2966 for the planned
/// fmt-side normalisation that will make `2700s` rewrite to `45min`.
pub fn render_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return "0s".into();
    }
    if secs.is_multiple_of(3600) {
        return format!("{}h", secs / 3600);
    }
    if secs.is_multiple_of(60) {
        return format!("{}min", secs / 60);
    }
    format!("{secs}s")
}

/// Render `value` into `sink` using the same code path that produces
/// the public [`format_value`] output. The single source of
/// truth for plan-display value formatting; sinks downstream of this
/// function decide whether to materialise a `String` or just count
/// bytes (#2434).
pub(crate) fn format_value_into<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    sink: &mut dyn FormatSink,
) -> Result<(), Overflow> {
    format_value_into_context(value, sink, FormatContext::default())
}

#[derive(Clone, Copy, Default)]
struct FormatContext<'a> {
    upstream_deferred_label: Option<(&'a str, &'a str)>,
}

fn format_value_into_context<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    sink: &mut dyn FormatSink,
    context: FormatContext<'_>,
) -> Result<(), Overflow> {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            sink.write_token(ValueSpanKind::Other, "if ")?;
            format_value_into_context(deferred.condition().value(), sink, context)?;
            sink.write_token(ValueSpanKind::Structure, " { ")?;
            format_value_into_context(deferred.then_value(), sink, context)?;
            sink.write_token(ValueSpanKind::Structure, " } else { ")?;
            format_value_into_context(deferred.else_value(), sink, context)?;
            sink.write_token(ValueSpanKind::Structure, " }")
        }
        Value::Concrete(ConcreteValue::String(s)) => sink.write_token(
            ValueSpanKind::StringLiteral,
            &display_string_literal(s.expose_plaintext()),
        ),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => {
            // Preserve unresolved parser-surface enum syntax verbatim. The
            // typed variant, rather than its text shape, is what authorizes
            // unquoted rendering.
            sink.write_token(ValueSpanKind::Enum, s.as_str())
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(c)) => {
            sink.write_token(ValueSpanKind::Enum, c.api_value())
        }
        Value::Concrete(ConcreteValue::Int(n)) => {
            sink.write_token(ValueSpanKind::Number, &n.to_string())
        }
        Value::Concrete(ConcreteValue::Duration(d)) => {
            // Duration literals were not classified as numeric by either
            // frontend's previous color rules (`1h` is not a number token).
            // Keep that presentation while still rendering through the
            // canonical value formatter.
            sink.write_token(ValueSpanKind::Other, &render_duration(*d))
        }
        Value::Concrete(ConcreteValue::Float(f)) => write_float(*f, sink),
        Value::Concrete(ConcreteValue::Bool(b)) => {
            sink.write_token(ValueSpanKind::Bool, if *b { "true" } else { "false" })
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            sink.write_token(ValueSpanKind::Structure, "[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    sink.write_token(ValueSpanKind::Structure, ", ")?;
                }
                format_value_into_context(item, sink, context)?;
            }
            sink.write_token(ValueSpanKind::Structure, "]")
        }
        Value::Concrete(ConcreteValue::StringList(items)) => {
            // Canonicalised string-or-list-of-strings shape (#2510).
            // Renders with the same `['a', 'b']` form as `Value::Concrete(ConcreteValue::List)`
            // of `Value::Concrete(ConcreteValue::String)` so plan output stays uniform.
            sink.write_token(ValueSpanKind::Structure, "[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    sink.write_token(ValueSpanKind::Structure, ", ")?;
                }
                sink.write_token(
                    ValueSpanKind::StringLiteral,
                    &display_string_literal(item.expose_plaintext()),
                )?;
            }
            sink.write_token(ValueSpanKind::Structure, "]")
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            sink.write_token(ValueSpanKind::Structure, "{")?;
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    sink.write_token(ValueSpanKind::Structure, ", ")?;
                }
                write_map_key(k, sink)?;
                sink.write_token(ValueSpanKind::Structure, ": ")?;
                format_value_into_context(&map[*k], sink, context)?;
            }
            sink.write_token(ValueSpanKind::Structure, "}")
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            sink.write_token(ValueSpanKind::Reference, &path.to_dot_string())
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            sink.write_token(ValueSpanKind::Reference, binding.as_str())
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            let parenthesize_left = matches!(
                value.as_ref(),
                Value::Deferred(DeferredValue::Coalesce { .. })
            );
            if parenthesize_left {
                sink.write_token(ValueSpanKind::Structure, "(")?;
            }
            format_value_into_context(value, sink, context)?;
            if parenthesize_left {
                sink.write_token(ValueSpanKind::Structure, ")")?;
            }
            sink.write_token(ValueSpanKind::Structure, " ?? ")?;
            format_value_into_context(default, sink, context)
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            format_integer_arithmetic(expression, sink, context)
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            sink.write_token(ValueSpanKind::StringLiteral, "\"")?;
            for part in parts {
                match part {
                    InterpolationPart::Literal(s) => sink.write_token(
                        ValueSpanKind::StringLiteral,
                        &display_double_quoted_literal_part(s.expose_plaintext()),
                    )?,
                    InterpolationPart::Expr(v) => {
                        sink.write_token(ValueSpanKind::Structure, "${")?;
                        format_value_into_context(v, sink, context)?;
                        sink.write_token(ValueSpanKind::Structure, "}")?;
                    }
                }
            }
            sink.write_token(ValueSpanKind::StringLiteral, "\"")
        }
        Value::Deferred(
            DeferredValue::FunctionCall { name, args, .. }
            | DeferredValue::Unavailable { name, args, .. },
        ) => {
            sink.write_token(ValueSpanKind::Other, name.as_str())?;
            sink.write_token(ValueSpanKind::Structure, "(")?;
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    sink.write_token(ValueSpanKind::Structure, ", ")?;
                }
                format_value_into_context(arg.walker_value(), sink, context)?;
            }
            sink.write_token(ValueSpanKind::Structure, ")")
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, .. }) => {
            sink.write_token(ValueSpanKind::Other, &function.name)?;
            sink.write_token(ValueSpanKind::Structure, "(")?;
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    sink.write_token(ValueSpanKind::Structure, ", ")?;
                }
                format_value_into_context(arg.walker_value(), sink, context)?;
            }
            sink.write_token(ValueSpanKind::Structure, ")")
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            if let Some(path) = deferred_for_access_display_path(access) {
                if let Some((upstream_binding, verb)) = context.upstream_deferred_label {
                    write_upstream_deferred_label(sink, upstream_binding, verb)
                } else {
                    sink.write_token(ValueSpanKind::Other, "(known after upstream apply: ")?;
                    sink.write_token(ValueSpanKind::Other, &path)?;
                    sink.write_token(ValueSpanKind::Other, ")")
                }
            } else {
                match access_chain_root(access.base()) {
                    Some(AccessChainRoot::LoopSlot(slot)) => {
                        if let Some((upstream_binding, verb)) = context.upstream_deferred_label {
                            write_upstream_deferred_label(sink, upstream_binding, verb)
                        } else {
                            sink.write_token(ValueSpanKind::Other, &slot.render())
                        }
                    }
                    Some(AccessChainRoot::Unknown(reason)) => write_unknown(reason, sink, context),
                    None => {
                        format_value_into_context(access.base(), sink, context)?;
                        sink.write_token(ValueSpanKind::Reference, &access.step().to_dot_string())
                    }
                }
            }
        }
        Value::Deferred(DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. }) => {
            sink.write_token(ValueSpanKind::Other, "(secret)")
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => write_unknown(reason, sink, context),
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            if placeholder.loop_slot().is_some()
                && let Some((upstream_binding, verb)) = context.upstream_deferred_label
            {
                write_upstream_deferred_label(sink, upstream_binding, verb)
            } else {
                sink.write_token(ValueSpanKind::Other, &placeholder.render())
            }
        }
    }
}

fn format_integer_arithmetic<P: PlaceholderKind, B: BindingAddress>(
    expression: &crate::integer_arithmetic::ValidatedIntegerExpr<P, B>,
    sink: &mut dyn FormatSink,
    context: FormatContext<'_>,
) -> Result<(), Overflow> {
    format_integer_operand(expression.lhs(), expression.op(), false, sink, context)?;
    sink.write_token(
        ValueSpanKind::Structure,
        &format!(" {} ", expression.op().symbol()),
    )?;
    format_integer_operand(expression.rhs(), expression.op(), true, sink, context)
}

fn format_integer_operand<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
    parent_op: crate::integer_arithmetic::IntegerOp,
    right_operand: bool,
    sink: &mut dyn FormatSink,
    context: FormatContext<'_>,
) -> Result<(), Overflow> {
    let child_precedence = match value {
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            Some(expression.op().precedence())
        }
        _ => None,
    };
    let parenthesize = matches!(value, Value::Deferred(DeferredValue::Coalesce { .. }))
        || child_precedence.is_some_and(|precedence| {
            precedence < parent_op.precedence()
                || (right_operand && precedence == parent_op.precedence())
        });
    if parenthesize {
        sink.write_token(ValueSpanKind::Structure, "(")?;
    }
    format_value_into_context(value, sink, context)?;
    if parenthesize {
        sink.write_token(ValueSpanKind::Structure, ")")?;
    }
    Ok(())
}

fn write_float(value: f64, sink: &mut dyn FormatSink) -> Result<(), Overflow> {
    let text = value.to_string();
    sink.write_token(ValueSpanKind::Number, &text)?;
    if !text.contains('.') {
        sink.write_token(ValueSpanKind::Number, ".0")?;
    }
    Ok(())
}

fn format_decoded_persisted_json_into(
    value: &DecodedPersistedJson,
    sink: &mut dyn FormatSink,
) -> Result<(), Overflow> {
    match value {
        DecodedPersistedJson::Null => sink.write_token(ValueSpanKind::Other, "null"),
        DecodedPersistedJson::Bool(value) => {
            sink.write_token(ValueSpanKind::Bool, if *value { "true" } else { "false" })
        }
        DecodedPersistedJson::Number(value) => {
            if let Some(value) = value.as_i64() {
                sink.write_token(ValueSpanKind::Number, &value.to_string())
            } else if let Some(value) = value.as_u64() {
                sink.write_token(ValueSpanKind::Number, &value.to_string())
            } else {
                write_float(value.as_f64().unwrap_or(0.0), sink)
            }
        }
        DecodedPersistedJson::String(value) => {
            sink.write_token(ValueSpanKind::StringLiteral, &display_string_literal(value))
        }
        DecodedPersistedJson::Enum(value) => {
            sink.write_token(ValueSpanKind::Enum, value.api_value())
        }
        DecodedPersistedJson::Secret(_) => sink.write_token(ValueSpanKind::Other, "(secret)"),
        DecodedPersistedJson::Array(values) => {
            sink.write_token(ValueSpanKind::Structure, "[")?;
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    sink.write_token(ValueSpanKind::Structure, ", ")?;
                }
                format_decoded_persisted_json_into(value, sink)?;
            }
            sink.write_token(ValueSpanKind::Structure, "]")
        }
        DecodedPersistedJson::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort();
            sink.write_token(ValueSpanKind::Structure, "{")?;
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    sink.write_token(ValueSpanKind::Structure, ", ")?;
                }
                write_map_key(key, sink)?;
                sink.write_token(ValueSpanKind::Structure, ": ")?;
                format_decoded_persisted_json_into(&values[*key], sink)?;
            }
            sink.write_token(ValueSpanKind::Structure, "}")
        }
    }
}

fn write_unknown<B: BindingAddress>(
    reason: &ApplyTimeUnknown<B>,
    sink: &mut dyn FormatSink,
    context: FormatContext<'_>,
) -> Result<(), Overflow> {
    if is_upstream_deferred_reason(reason)
        && let Some((upstream_binding, verb)) = context.upstream_deferred_label
    {
        write_upstream_deferred_label(sink, upstream_binding, verb)
    } else {
        sink.write_token(ValueSpanKind::Other, &render_unknown(reason))
    }
}

fn write_upstream_deferred_label(
    sink: &mut dyn FormatSink,
    upstream_binding: &str,
    verb: &str,
) -> Result<(), Overflow> {
    sink.write_token(
        ValueSpanKind::Other,
        &format!("(known after {upstream_binding} {verb})"),
    )
}

fn is_upstream_deferred_reason<B: BindingAddress>(reason: &ApplyTimeUnknown<B>) -> bool {
    matches!(
        reason,
        ApplyTimeUnknown::UpstreamExport { .. } | ApplyTimeUnknown::UpstreamAllExports { .. }
    )
}

pub(crate) fn is_upstream_deferred_marker_value<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
) -> bool {
    match value {
        Value::Deferred(DeferredValue::Unknown(reason)) => is_upstream_deferred_reason(reason),
        Value::Deferred(DeferredValue::Access(access)) => {
            if access.has_authored_loop_path() {
                deferred_for_access_display_path(access).is_some()
            } else {
                match access_chain_root(access.base()) {
                    Some(AccessChainRoot::LoopSlot(_)) => true,
                    Some(AccessChainRoot::Unknown(reason)) => is_upstream_deferred_reason(reason),
                    None => false,
                }
            }
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(_)) => false,
        _ => false,
    }
}

/// Reconstruct an authored deferred-for value path from sealed access nodes.
/// The semantic placeholder remains `Placeholder(LoopSlot::Value)`; the binding name is
/// display-only parser metadata on `AccessExpr`, so no obsolete unknown reason
/// or independently navigable path representation is reintroduced.
fn deferred_for_access_display_path<P: PlaceholderKind, B: BindingAddress>(
    access: &crate::resource::AccessExpr<P, B>,
) -> Option<String> {
    fn append_all_steps<P: PlaceholderKind, B: BindingAddress>(
        access: &crate::resource::AccessExpr<P, B>,
        output: &mut String,
    ) {
        if let Value::Deferred(DeferredValue::Access(inner)) = access.base() {
            append_all_steps(inner, output);
        }
        output.push_str(&access.step().to_dot_string());
    }

    fn append_steps<P: PlaceholderKind, B: BindingAddress>(
        access: &crate::resource::AccessExpr<P, B>,
        output: &mut String,
    ) -> bool {
        let rooted_at_loop_slot = match access.base() {
            Value::Deferred(DeferredValue::Access(inner)) => append_steps(inner, output),
            Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
                placeholder.loop_slot().is_some()
            }
            _ => false,
        };
        if rooted_at_loop_slot {
            output.push_str(&access.step().to_dot_string());
        }
        rooted_at_loop_slot
    }

    if let Some(root) = access.deferred_miss_display_root() {
        let mut path = root.to_string();
        append_all_steps(access, &mut path);
        return Some(path);
    }

    let mut path = access.display_root()?.to_string();
    append_steps(access, &mut path).then_some(path)
}

pub(crate) enum AccessChainRoot<'a, B: BindingAddress = crate::resource::AbsoluteBindingAddress> {
    LoopSlot(LoopSlot),
    Unknown(&'a ApplyTimeUnknown<B>),
}

/// Return the typed root of a direct postfix-access chain.
/// Function calls and all other expression nodes intentionally stop the
/// search: `unknown.field` is still unknown, while `split(unknown)[0]`
/// remains a useful expression in diagnostics and plans.
pub(crate) fn access_chain_root<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
) -> Option<AccessChainRoot<'_, B>> {
    match value {
        Value::Deferred(DeferredValue::Access(access)) => access_chain_root(access.base()),
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => {
            placeholder.loop_slot().map(AccessChainRoot::LoopSlot)
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => Some(AccessChainRoot::Unknown(reason)),
        _ => None,
    }
}

/// Check if a Value contains any Secret values at any nesting depth.
pub fn contains_secret<P: PlaceholderKind, B: BindingAddress>(value: &Value<P, B>) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            deferred.values().into_iter().any(contains_secret)
        }
        Value::Deferred(DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. }) => true,
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(contains_secret),
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(contains_secret),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts
            .iter()
            .any(|part| matches!(part, InterpolationPart::Expr(value) if contains_secret(value))),
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args
            .iter()
            .any(|argument| contains_secret(argument.walker_value())),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            contains_secret(value) || contains_secret(default)
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            contains_secret(expression.lhs()) || contains_secret(expression.rhs())
        }
        Value::Deferred(DeferredValue::Access(access)) => contains_secret(access.base()),
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
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
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        ) => false,
    }
}

/// Check whether a value contains plaintext secret material rather than an
/// opaque hash loaded from persisted state or a saved plan.
///
/// This distinction matters at state writeback: a [`DeferredValue::Secret`]
/// authorizes hashing provider plaintext, while a
/// [`DeferredValue::RedactedSecret`] is only historical hash evidence and
/// must not be treated as a new desired secret.
pub fn contains_unredacted_secret(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => {
            deferred
                .values()
                .into_iter()
                .any(contains_unredacted_secret)
        }
        Value::Deferred(DeferredValue::Secret(_)) => true,
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => false,
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().any(contains_unredacted_secret)
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            map.values().any(contains_unredacted_secret)
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(part, InterpolationPart::Expr(value) if contains_unredacted_secret(value))
        }),
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::Unavailable { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => {
            args.iter()
                .any(|argument| contains_unredacted_secret(argument.walker_value()))
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            contains_unredacted_secret(access.base())
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            contains_unredacted_secret(value) || contains_unredacted_secret(default)
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            contains_unredacted_secret(expression.lhs())
                || contains_unredacted_secret(expression.rhs())
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
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
            | DeferredValue::Unknown(_),
        ) => false,
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
    }
}

/// Merge secret hashes from the desired value into the provider-returned JSON.
///
/// For attributes containing secrets nested inside Maps or Lists, we cannot simply
/// replace the entire provider value with the desired value's JSON, because the
/// provider may return extra keys (e.g., CloudControl auto-adds tags). This function
/// recursively walks both trees:
/// - If the desired value is `Secret(inner)`, return the hashed value
/// - If desired is a `Map` and provider is an object, merge: for each provider key,
///   if the desired map has a corresponding secret-containing value, use the hashed
///   version; otherwise keep the provider value
/// - If desired is a `List` and provider is an array, merge element-by-element
/// - Otherwise, return the provider value as-is
pub fn merge_secrets_into_provider_json(
    desired: &Value,
    provider: &PersistedValue,
    previous: Option<&PersistedValue>,
) -> Result<PersistedValue, SerializationError> {
    let encoded = merge_secrets_into_provider_json_inner(
        desired,
        provider.view(),
        previous.map(PersistedValue::view),
    )?;
    Ok(PersistedValue { encoded })
}

/// Mask an observed value using the desired value's secret shape without
/// replacing any observed material with desired material.
///
/// Writeback selects provider-observed, accepted-request, or previously
/// persisted material before calling this walker. It hashes a top-level
/// observed secret even when the desired secret is a literal, and never
/// inserts a desired map child that the selected value did not contain.
pub fn merge_secrets_into_observation_json(
    desired: &Value,
    observed: &PersistedValue,
    previous: Option<&PersistedValue>,
) -> Result<PersistedValue, SerializationError> {
    let encoded = merge_secrets_into_observation_json_inner(
        desired,
        observed.view(),
        previous.map(PersistedValue::view),
    )?;
    Ok(PersistedValue { encoded })
}

fn merge_secrets_into_observation_json_inner(
    desired: &Value,
    observed: PersistedView<'_>,
    previous: Option<PersistedView<'_>>,
) -> Result<serde_json::Value, SerializationError> {
    match desired {
        Value::Deferred(DeferredValue::Secret(_inner)) => {
            if observed.is_redacted_secret() {
                return Ok(observed.to_owned_json());
            }
            let observed_value =
                observed
                    .decode_value()
                    .ok_or(SerializationError::SecretNotAllowed {
                        context: SerializationContext::StateWriteback,
                    })?;
            persist_secret_json(&observed_value, previous)
        }
        Value::Concrete(ConcreteValue::Map(desired_map)) => {
            let Some(observed_map) = observed.as_map() else {
                return mask_mismatched_observation_shape(desired, observed, previous);
            };
            let previous_map = previous.and_then(PersistedView::as_map);
            let replacements = desired_map
                .iter()
                .filter(|(_key, desired_value)| contains_secret(desired_value))
                .filter_map(|(key, desired_value)| {
                    observed_map.get(key).map(|observed_value| {
                        merge_secrets_into_observation_json_inner(
                            desired_value,
                            observed_value,
                            previous_map.and_then(|previous| previous.get(key)),
                        )
                        .map(|masked| (key.clone(), masked))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(observed_map.replacing_children(replacements))
        }
        Value::Concrete(ConcreteValue::List(desired_items)) => {
            let Some(observed_items) = observed.as_list() else {
                return mask_mismatched_observation_shape(desired, observed, previous);
            };
            let previous_items = previous.and_then(PersistedView::as_list);
            let replacements = observed_items
                .iter()
                .enumerate()
                .filter_map(|(index, observed_value)| {
                    desired_items.get(index).and_then(|desired_value| {
                        contains_secret(desired_value).then(|| {
                            merge_secrets_into_observation_json_inner(
                                desired_value,
                                observed_value,
                                previous_items.and_then(|previous| previous.get(index)),
                            )
                            .map(|masked| (index, masked))
                        })
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(observed_items.replacing_children(replacements))
        }
        // Deferred/non-structural expressions can still contain a secret. The
        // normal executor resolves them before writeback, but state-boundary
        // callers must fail closed rather than persisting observed plaintext if
        // one reaches this walker unresolved.
        _ if contains_secret(desired) => {
            mask_mismatched_observation_shape(desired, observed, previous)
        }
        _ => Ok(observed.to_owned_json()),
    }
}

fn mask_mismatched_observation_shape(
    desired: &Value,
    observed: PersistedView<'_>,
    previous: Option<PersistedView<'_>>,
) -> Result<serde_json::Value, SerializationError> {
    if !contains_secret(desired) {
        return Ok(observed.to_owned_json());
    }
    let observed_value = observed
        .decode_value()
        .ok_or(SerializationError::SecretNotAllowed {
            context: SerializationContext::StateWriteback,
        })?;
    persist_secret_json(&observed_value, previous)
}

fn merge_secrets_into_provider_json_inner(
    desired: &Value,
    provider: PersistedView<'_>,
    previous: Option<PersistedView<'_>>,
) -> Result<serde_json::Value, SerializationError> {
    match desired {
        Value::Deferred(DeferredValue::Secret(inner)) => {
            if !secret_value_requires_provider_material(inner) {
                return persist_secret_json(inner, previous);
            }
            if provider.is_redacted_secret() {
                return Ok(provider.to_owned_json());
            }
            let provider_value =
                provider
                    .decode_value()
                    .ok_or(SerializationError::SecretNotAllowed {
                        context: SerializationContext::StateWriteback,
                    })?;
            persist_secret_json(&provider_value, previous)
        }
        Value::Concrete(ConcreteValue::Map(desired_map)) => {
            if let Some(provider_map) = provider.as_map() {
                let previous_map = previous.and_then(PersistedView::as_map);
                let mut replacements = Vec::new();
                for (k, desired_val) in desired_map {
                    if contains_secret(desired_val) {
                        if let Some(provider_value) = provider_map.get(k) {
                            replacements.push((
                                k.clone(),
                                merge_secrets_into_provider_json_inner(
                                    desired_val,
                                    provider_value,
                                    previous_map.and_then(|previous| previous.get(k)),
                                )?,
                            ));
                        } else {
                            // Key only in desired (not returned by provider); use desired hash
                            replacements.push((
                                k.clone(),
                                PersistedValue::encode_desired_value_from_view(
                                    desired_val,
                                    previous_map.and_then(|previous| previous.get(k)),
                                )?
                                .encoded,
                            ));
                        }
                    }
                }
                Ok(provider_map.replacing_children(replacements))
            } else {
                // Provider didn't return a map; fall back to desired
                PersistedValue::encode_desired_value_from_view(desired, previous)
                    .map(|value| value.encoded)
            }
        }
        Value::Concrete(ConcreteValue::List(desired_items)) => {
            if let Some(provider_items) = provider.as_list() {
                let previous_items = previous.and_then(PersistedView::as_list);
                let mut replacements = Vec::new();
                for (i, provider_item) in provider_items.iter().enumerate() {
                    if let Some(desired_elem) = desired_items.get(i)
                        && contains_secret(desired_elem)
                    {
                        replacements.push((
                            i,
                            merge_secrets_into_provider_json_inner(
                                desired_elem,
                                provider_item,
                                previous_items.and_then(|previous| previous.get(i)),
                            )?,
                        ));
                    }
                }
                Ok(provider_items.replacing_children(replacements))
            } else {
                PersistedValue::encode_desired_value_from_view(desired, previous)
                    .map(|value| value.encoded)
            }
        }
        // A coalesce, function call, or interpolation does not expose enough
        // structural information to mask only one child. If it contains any
        // secret, hash the whole provider value instead of leaking plaintext.
        _ if contains_secret(desired) => {
            mask_mismatched_observation_shape(desired, provider, previous)
        }
        _ => Ok(provider.to_owned_json()),
    }
}

fn secret_value_requires_provider_material(value: &Value) -> bool {
    match value {
        Value::Deferred(
            DeferredValue::ModuleIf(_)
            | DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::IntegerArithmetic(_)
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::Unknown(_),
        ) => true,
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            secret_value_requires_provider_material(value)
                || secret_value_requires_provider_material(default)
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            secret_value_requires_provider_material(inner)
        }
        Value::Concrete(ConcreteValue::List(values)) => {
            values.iter().any(secret_value_requires_provider_material)
        }
        Value::Concrete(ConcreteValue::Map(values)) => {
            values.values().any(secret_value_requires_provider_material)
        }
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::StringList(_),
        )
        | Value::Deferred(DeferredValue::RedactedSecret { .. }) => false,
        Value::Deferred(DeferredValue::Placeholder(never)) => match *never {},
    }
}

/// Secret-only redactor used inside deferred wrappers (`Interpolation`,
/// `FunctionCall`) where a `Value::Deferred(Unknown(UpstreamExport))` is a
/// load-bearing plan-time placeholder, not an error.
///
/// Hashes every `Secret(_)` leaf while passing `Unknown` and other deferred
/// shapes through verbatim.
fn redact_secrets_only<P: PlaceholderKind>(
    value: &Value<P>,
) -> Result<Value<P>, SerializationError> {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => redact_secret_leaf(inner),
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => Ok(value.clone()),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let redacted: Result<IndexMap<String, Value<P>>, _> = map
                .iter()
                .map(|(k, v)| redact_secrets_only(v).map(|rv| (k.clone(), rv)))
                .collect();
            Ok(Value::Concrete(ConcreteValue::Map(redacted?)))
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let redacted: Result<Vec<_>, _> = items.iter().map(redact_secrets_only).collect();
            Ok(Value::Concrete(ConcreteValue::List(redacted?)))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let redacted: Result<Vec<InterpolationPart<P>>, _> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Literal(s) => Ok(InterpolationPart::Literal(s.clone())),
                    InterpolationPart::Expr(v) => {
                        redact_secrets_only(v).map(InterpolationPart::Expr)
                    }
                })
                .collect();
            Ok(Value::Deferred(DeferredValue::Interpolation(redacted?)))
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => {
            let redacted = args
                .iter()
                .map(|argument| argument.try_map_value(redact_secrets_only))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Deferred(DeferredValue::FunctionCall {
                name: *name,
                args: redacted,
                source_line: *source_line,
            }))
        }
        Value::Deferred(DeferredValue::Unavailable {
            name,
            args,
            reason,
            source_line,
        }) => {
            let redacted = args
                .iter()
                .map(|argument| argument.try_map_value(redact_secrets_only))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Deferred(DeferredValue::Unavailable {
                name: *name,
                args: redacted,
                reason: reason.clone(),
                source_line: *source_line,
            }))
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => {
            let redacted = args
                .iter()
                .map(|argument| argument.try_map_value(redact_secrets_only))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function: function.clone(),
                functions: functions.clone(),
                args: redacted,
                source_line: *source_line,
            }))
        }
        Value::Deferred(DeferredValue::Access(access)) => access
            .rebuild_with_base(
                redact_secrets_only(access.base())?,
                crate::resource::AccessMissPolicy::Error,
            )
            .map_err(|_| SerializationError::UnresolvedAccess {
                expression: format_value(value),
                context: SerializationContext::SecretRedaction,
            }),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Ok(Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(redact_secrets_only(value)?),
                default: Box::new(redact_secrets_only(default)?),
            }))
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = redact_secrets_only(expression.lhs())?;
            let rhs = redact_secrets_only(expression.rhs())?;
            expression.evaluate_with_operands(lhs, rhs).map_err(|_| {
                SerializationError::UnresolvedIntegerArithmetic {
                    context: SerializationContext::SecretRedaction,
                }
            })
        }
        // `Unknown` and every other deferred / concrete shape pass through.
        other => Ok(other.clone()),
    }
}

/// Redact a typed secret leaf without requiring its material to be fully
/// resolved.
///
/// Every secret wrapper becomes exactly one placeholder. For a deferred inner
/// expression, the placeholder hashes a canonical template representation;
/// references and unknowns are represented structurally, so no individual
/// literal fragment is exposed to an independent brute-force attempt.
fn redact_secret_leaf<P: PlaceholderKind>(
    inner: &Value<P>,
) -> Result<Value<P>, SerializationError> {
    match inner {
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => Ok(inner.clone()),
        _ => {
            let material = canonical_secret_hash_material(inner)?;
            Ok(Value::Deferred(DeferredValue::RedactedSecret {
                hash: RedactedSecretHash::reuse_or_generate(&material, None),
            }))
        }
    }
}

/// Canonical hash material for a secret that is still deferred. Tags keep
/// expression nodes disjoint from user-authored maps while deliberately
/// retaining only stable resolver inputs (paths, bindings, literals, and
/// unknown reasons). This representation is hashed but never persisted.
fn canonical_secret_template<P: PlaceholderKind>(
    value: &Value<P>,
) -> Result<serde_json::Value, SerializationError> {
    use serde_json::{Map, Value as JsonValue};

    fn tagged(tag: &str, value: JsonValue) -> JsonValue {
        let mut object = Map::new();
        object.insert("kind".to_string(), JsonValue::String(tag.to_string()));
        object.insert("value".to_string(), value);
        JsonValue::Object(object)
    }

    Ok(match value {
        Value::Deferred(DeferredValue::ModuleIf(_)) => {
            return Err(SerializationError::UnexpandedModuleIf {
                context: SerializationContext::SecretRedaction,
            });
        }
        Value::Concrete(ConcreteValue::String(value)) => tagged(
            "string",
            JsonValue::String(value.expose_plaintext().to_string()),
        ),
        Value::Concrete(ConcreteValue::EnumIdentifier(value)) => {
            tagged("enum_identifier", JsonValue::String(value.to_string()))
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(value)) => tagged(
            "canonical_enum",
            serde_json::to_value(value).expect("CanonicalEnumValue is serializable"),
        ),
        Value::Concrete(ConcreteValue::Int(value)) => {
            tagged("int", JsonValue::Number((*value).into()))
        }
        Value::Concrete(ConcreteValue::Float(value)) => tagged(
            "float",
            JsonValue::Number(serde_json::Number::from_f64(*value).ok_or(
                SerializationError::NonFiniteFloat {
                    value: *value,
                    context: SerializationContext::SecretRedaction,
                },
            )?),
        ),
        Value::Concrete(ConcreteValue::Bool(value)) => tagged("bool", JsonValue::Bool(*value)),
        Value::Concrete(ConcreteValue::Duration(value)) => tagged(
            "duration_seconds",
            JsonValue::Number(value.as_secs().into()),
        ),
        Value::Concrete(ConcreteValue::List(values)) => tagged(
            "list",
            JsonValue::Array(
                values
                    .iter()
                    .map(canonical_secret_template)
                    .collect::<Result<_, _>>()?,
            ),
        ),
        Value::Concrete(ConcreteValue::StringList(values)) => tagged(
            "string_list",
            JsonValue::Array(
                values
                    .iter()
                    .map(|value| JsonValue::String(value.expose_plaintext().to_string()))
                    .collect(),
            ),
        ),
        Value::Concrete(ConcreteValue::Map(values)) => tagged(
            "map",
            JsonValue::Object(
                values
                    .iter()
                    .map(|(key, value)| {
                        canonical_secret_template(value).map(|value| (key.clone(), value))
                    })
                    .collect::<Result<_, _>>()?,
            ),
        ),
        Value::Deferred(DeferredValue::ResourceRef { path }) => tagged(
            "resource_ref",
            serde_json::to_value(path).expect("AccessPath is serializable"),
        ),
        Value::Deferred(DeferredValue::BindingRef { binding }) => {
            tagged("binding_ref", JsonValue::String(binding.to_string()))
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => tagged(
            "coalesce",
            JsonValue::Array(vec![
                canonical_secret_template(value)?,
                canonical_secret_template(default)?,
            ]),
        ),
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let mut encoded = Map::new();
            encoded.insert(
                "lhs".to_string(),
                canonical_secret_template(expression.lhs())?,
            );
            encoded.insert(
                "op".to_string(),
                JsonValue::String(expression.op().symbol().to_string()),
            );
            encoded.insert(
                "rhs".to_string(),
                canonical_secret_template(expression.rhs())?,
            );
            tagged("integer_arithmetic", JsonValue::Object(encoded))
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => tagged(
            "interpolation",
            JsonValue::Array(
                parts
                    .iter()
                    .map(|part| match part {
                        InterpolationPart::Literal(value) => Ok(tagged(
                            "literal",
                            JsonValue::String(value.expose_plaintext().to_string()),
                        )),
                        InterpolationPart::Expr(value) => {
                            canonical_secret_template(value).map(|value| tagged("expr", value))
                        }
                    })
                    .collect::<Result<_, SerializationError>>()?,
            ),
        ),
        Value::Deferred(
            DeferredValue::FunctionCall { name, args, .. }
            | DeferredValue::Unavailable { name, args, .. },
        ) => {
            let mut call = Map::new();
            call.insert("name".to_string(), JsonValue::String(name.to_string()));
            call.insert(
                "args".to_string(),
                JsonValue::Array(
                    args.iter()
                        .map(|argument| canonical_secret_template(argument.walker_value()))
                        .collect::<Result<_, _>>()?,
                ),
            );
            tagged("function_call", JsonValue::Object(call))
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall { function, args, .. }) => {
            let mut call = Map::new();
            call.insert("name".to_string(), JsonValue::String(function.name.clone()));
            call.insert(
                "args".to_string(),
                JsonValue::Array(
                    args.iter()
                        .map(|argument| canonical_secret_template(argument.walker_value()))
                        .collect::<Result<_, _>>()?,
                ),
            );
            tagged("function_call", JsonValue::Object(call))
        }
        Value::Deferred(DeferredValue::Access(access)) => {
            let mut encoded = Map::new();
            encoded.insert(
                "base".to_string(),
                canonical_secret_template(access.base())?,
            );
            encoded.insert(
                "step".to_string(),
                serde_json::to_value(access.step()).expect("PathSegment is serializable"),
            );
            tagged("access", JsonValue::Object(encoded))
        }
        Value::Deferred(DeferredValue::Secret(inner)) => canonical_secret_template(inner)?,
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => {
            return Err(SerializationError::RedactedSecretNotAllowed {
                context: SerializationContext::SecretRedaction,
            });
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => tagged(
            "unknown",
            serde_json::to_value(reason).expect("ApplyTimeUnknown is serializable"),
        ),
        Value::Deferred(DeferredValue::Placeholder(placeholder)) => tagged(
            "placeholder",
            serde_json::to_value(placeholder).map_err(|_| {
                SerializationError::SecretNotAllowed {
                    context: SerializationContext::SecretRedaction,
                }
            })?,
        ),
    })
}

/// Return whether a value tree contains a saved-plan secret placeholder.
pub fn contains_redacted_secret<P: PlaceholderKind, B: BindingAddress>(
    value: &Value<P, B>,
) -> bool {
    match value {
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => true,
        Value::Concrete(ConcreteValue::List(items)) => {
            items.iter().any(contains_redacted_secret)
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            map.values().any(contains_redacted_secret)
        }
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(part, InterpolationPart::Expr(value) if contains_redacted_secret(value))
        }),
        Value::Deferred(
            DeferredValue::FunctionCall { args, .. }
            | DeferredValue::ResolvedFunctionCall { args, .. },
        ) => args
            .iter()
            .any(|argument| contains_redacted_secret(argument.walker_value())),
        Value::Deferred(DeferredValue::Access(access)) => {
            contains_redacted_secret(access.base())
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            contains_redacted_secret(value) || contains_redacted_secret(default)
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            contains_redacted_secret(expression.lhs())
                || contains_redacted_secret(expression.rhs())
        }
        _ => false,
    }
}

fn attributes_contain_redacted_secret<'a, P: PlaceholderKind + 'a, B: BindingAddress + 'a>(
    values: impl Iterator<Item = &'a Value<P, B>>,
) -> bool {
    values.into_iter().any(contains_redacted_secret)
}

fn deferred_template_contains_redacted_secret<S, P, T, D>(
    template: &crate::parser::DeferredForExpression<
        S,
        P,
        T,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
) -> bool
where
    P: PlaceholderKind,
    T: PlaceholderKind,
{
    contains_redacted_secret(&template.iterable)
        || attributes_contain_redacted_secret(template.attributes().values())
}

pub(crate) fn unresolved_effect_contains_redacted_secret(
    effect: &crate::effect::UnresolvedSavedEffect,
) -> bool {
    use crate::effect::UnresolvedSavedEffect;

    match effect {
        UnresolvedSavedEffect::Read { resource } => {
            attributes_contain_redacted_secret(resource.attributes.values())
        }
        UnresolvedSavedEffect::Create(resource)
        | UnresolvedSavedEffect::ResumeCreate { resource, .. } => {
            attributes_contain_redacted_secret(resource.as_resource().attributes.values())
        }
        UnresolvedSavedEffect::Update { from, to, .. } => {
            attributes_contain_redacted_secret(from.attributes.values())
                || attributes_contain_redacted_secret(to.as_resource().attributes.values())
        }
        UnresolvedSavedEffect::Import { identifier, .. } => contains_redacted_secret(identifier),
        UnresolvedSavedEffect::ReleaseListKeys { keys, .. } => keys
            .iter()
            .any(|key| contains_redacted_secret(key.retained_value())),
        UnresolvedSavedEffect::Wait { until, .. } => contains_redacted_secret(until.value()),
        UnresolvedSavedEffect::DeferredCreate(payload) => {
            deferred_template_contains_redacted_secret(&payload.template)
        }
        UnresolvedSavedEffect::DeferredReplace(payload) => {
            deferred_template_contains_redacted_secret(&payload.template)
        }
        UnresolvedSavedEffect::Delete { .. }
        | UnresolvedSavedEffect::Remove { .. }
        | UnresolvedSavedEffect::Move { .. } => false,
    }
}

pub(crate) fn effect_contains_redacted_secret<S>(effect: &crate::effect::Effect<S>) -> bool {
    use crate::effect::Effect;

    match effect {
        Effect::Read { resource } => {
            attributes_contain_redacted_secret(resource.attributes.values())
        }
        Effect::Create(resource) => {
            attributes_contain_redacted_secret(resource.attributes.values())
        }
        Effect::ResumeCreate { resource, .. } => {
            attributes_contain_redacted_secret(resource.attributes.values())
        }
        Effect::Update {
            from,
            identifier: _identifier,
            to,
            changed_attributes: _changed_attributes,
        } => {
            attributes_contain_redacted_secret(from.attributes.values())
                || attributes_contain_redacted_secret(to.attributes.values())
        }
        Effect::Delete {
            id: _id,
            identifier: _identifier,
            generation: _generation,
            directives: _directives,
            binding: _binding,
            dependencies: _dependencies,
            explicit_dependencies: _explicit_dependencies,
            blocked_by_updates: _blocked_by_updates,
        } => false,
        Effect::Import {
            id: _id,
            identifier,
        } => contains_redacted_secret(identifier),
        Effect::Remove { id: _id } => false,
        Effect::Move {
            from: _from,
            to: _to,
            origin: _origin,
        } => false,
        Effect::ReleaseListKeys {
            id: _id,
            attribute: _attribute,
            key_field: _key_field,
            keys,
        } => keys
            .iter()
            .any(|key| contains_redacted_secret(key.retained_value())),
        Effect::Wait {
            id: _id,
            target_id: _target_id,
            until,
            until_surface: _until_surface,
            timeout: _timeout,
            interval: _interval,
            explicit_dependencies: _explicit_dependencies,
        } => contains_redacted_secret(until.value()),
        Effect::DeferredCreate(payload) => {
            deferred_template_contains_redacted_secret(&payload.template)
        }
        Effect::DeferredReplace(payload) => {
            let crate::effect::DeferredReplacePayload {
                deletes: _deletes,
                dependency_template: _dependency_template,
                template,
            } = payload.as_ref();
            deferred_template_contains_redacted_secret(template)
        }
    }
}

/// Reject a secret from a saved-plan field whose contract forbids secrets.
pub fn reject_secret_in_saved_plan_field(
    value: &Value,
    location: impl Into<String>,
) -> Result<(), SerializationError> {
    if contains_secret(value) {
        return Err(SerializationError::SavedPlanFieldSecretNotAllowed {
            location: location.into(),
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum SecretRedactionGuide<'a> {
    Evaluated(&'a Value),
    SecretShape(&'a Value),
    /// The redacted guide classifies provider/historical plaintext as secret.
    RedactedShape(&'a Value),
    /// The source value is authoritative for secret semantics; the redacted
    /// guide supplies only a same-path digest that may be reused.
    RedactedDesired(&'a Value),
    /// Persisted state is authoritative for classifying provider plaintext.
    Persisted(PersistedView<'a>),
    /// Desired `Secret` wrappers are authoritative; persisted state supplies
    /// only a same-path digest that may be reused.
    PersistedDesired(PersistedView<'a>),
}

#[derive(Clone, Copy)]
enum SecretRedactionMapGuide<'a> {
    Evaluated(&'a IndexMap<String, Value>),
    SecretShape(&'a IndexMap<String, Value>),
    RedactedShape(&'a IndexMap<String, Value>),
    RedactedDesired(&'a IndexMap<String, Value>),
    Persisted(PersistedMapView<'a>),
    PersistedDesired(PersistedMapView<'a>),
}

impl<'a> SecretRedactionMapGuide<'a> {
    fn child(self, key: &str) -> Option<SecretRedactionGuide<'a>> {
        match self {
            Self::Evaluated(values) => values.get(key).map(SecretRedactionGuide::Evaluated),
            Self::SecretShape(values) => values.get(key).map(SecretRedactionGuide::SecretShape),
            Self::RedactedShape(values) => values.get(key).map(SecretRedactionGuide::RedactedShape),
            Self::RedactedDesired(values) => {
                values.get(key).map(SecretRedactionGuide::RedactedDesired)
            }
            Self::Persisted(values) => values.get(key).map(SecretRedactionGuide::Persisted),
            Self::PersistedDesired(values) => {
                values.get(key).map(SecretRedactionGuide::PersistedDesired)
            }
        }
    }
}

impl<'a> SecretRedactionGuide<'a> {
    fn secret_value<'b>(&'b self, source: &'b Value) -> Option<&'b Value> {
        let plaintext = match source {
            Value::Deferred(DeferredValue::Secret(inner)) => inner.as_ref(),
            _ => source,
        };
        match self {
            Self::Evaluated(Value::Deferred(DeferredValue::Secret(inner))) => Some(inner),
            Self::SecretShape(Value::Deferred(DeferredValue::Secret(_))) => Some(plaintext),
            Self::RedactedShape(Value::Deferred(DeferredValue::RedactedSecret { .. })) => {
                Some(plaintext)
            }
            Self::RedactedDesired(Value::Deferred(DeferredValue::RedactedSecret { .. })) => {
                match source {
                    Value::Deferred(
                        DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. },
                    ) => Some(plaintext),
                    _ => None,
                }
            }
            Self::Persisted(value) if value.is_redacted_secret() => Some(plaintext),
            Self::PersistedDesired(value) if value.is_redacted_secret() => match source {
                Value::Deferred(
                    DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. },
                ) => Some(plaintext),
                _ => None,
            },
            _ => None,
        }
    }

    fn previous_hash(self) -> Option<RedactedSecretHash> {
        match self {
            Self::RedactedShape(Value::Deferred(DeferredValue::RedactedSecret { hash }))
            | Self::RedactedDesired(Value::Deferred(DeferredValue::RedactedSecret { hash })) => {
                Some(hash.clone())
            }
            Self::Persisted(value) | Self::PersistedDesired(value) => value.redacted_secret_hash(),
            _ => None,
        }
    }

    fn as_map(self) -> Option<SecretRedactionMapGuide<'a>> {
        match self {
            Self::Evaluated(Value::Concrete(ConcreteValue::Map(values))) => {
                Some(SecretRedactionMapGuide::Evaluated(values))
            }
            Self::SecretShape(Value::Concrete(ConcreteValue::Map(values))) => {
                Some(SecretRedactionMapGuide::SecretShape(values))
            }
            Self::RedactedShape(Value::Concrete(ConcreteValue::Map(values))) => {
                Some(SecretRedactionMapGuide::RedactedShape(values))
            }
            Self::RedactedDesired(Value::Concrete(ConcreteValue::Map(values))) => {
                Some(SecretRedactionMapGuide::RedactedDesired(values))
            }
            Self::Persisted(values) => values.as_map().map(SecretRedactionMapGuide::Persisted),
            Self::PersistedDesired(values) => values
                .as_map()
                .map(SecretRedactionMapGuide::PersistedDesired),
            _ => None,
        }
    }

    fn list_child(self, index: usize) -> Option<Self> {
        match self {
            Self::Evaluated(Value::Concrete(ConcreteValue::List(values))) => {
                values.get(index).map(Self::Evaluated)
            }
            Self::SecretShape(Value::Concrete(ConcreteValue::List(values))) => {
                values.get(index).map(Self::SecretShape)
            }
            Self::RedactedShape(Value::Concrete(ConcreteValue::List(values))) => {
                values.get(index).map(Self::RedactedShape)
            }
            Self::RedactedDesired(Value::Concrete(ConcreteValue::List(values))) => {
                values.get(index).map(Self::RedactedDesired)
            }
            Self::Persisted(values) => values.list_child(index).map(Self::Persisted),
            Self::PersistedDesired(values) => values.list_child(index).map(Self::PersistedDesired),
            _ => None,
        }
    }
}

fn redact_with_guide(
    source: &Value,
    guide: SecretRedactionGuide<'_>,
) -> Result<Value, SerializationError> {
    if let Some(secret_value) = guide.secret_value(source) {
        if matches!(
            source,
            Value::Deferred(DeferredValue::RedactedSecret { .. })
        ) {
            return Ok(source.clone());
        }
        let previous = guide.previous_hash();
        let material = canonical_secret_hash_material(secret_value)?;
        return Ok(Value::Deferred(DeferredValue::RedactedSecret {
            hash: RedactedSecretHash::reuse_or_generate(&material, previous.as_ref()),
        }));
    }

    match source {
        Value::Concrete(ConcreteValue::Map(values)) => {
            let guide = guide.as_map();
            let values = values
                .iter()
                .map(|(key, value)| {
                    let value = match guide.and_then(|guide| guide.child(key)) {
                        Some(guide) => redact_with_guide(value, guide)?,
                        None => redact_secrets_only(value)?,
                    };
                    Ok((key.clone(), value))
                })
                .collect::<Result<IndexMap<_, _>, SerializationError>>()?;
            Ok(Value::Concrete(ConcreteValue::Map(values)))
        }
        Value::Concrete(ConcreteValue::List(values)) => {
            let values = values
                .iter()
                .enumerate()
                .map(|(index, value)| match guide.list_child(index) {
                    Some(guide) => redact_with_guide(value, guide),
                    None => redact_secrets_only(value),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Concrete(ConcreteValue::List(values)))
        }
        _ => redact_secrets_only(source),
    }
}

/// Redact `source`, using its evaluated value as the authority for secret
/// semantics. This recognizes any expression whose result is secret without
/// duplicating evaluator policy at the saved-plan seam.
pub fn redact_secrets_using_evaluated(
    source: &Value,
    evaluated: &Value,
) -> Result<Value, SerializationError> {
    if contains_secret(evaluated) {
        // Once evaluation reveals a secret anywhere below this path, persist
        // the evaluated subtree rather than the authored expression. This
        // keeps unresolved refs/unknowns needed at apply time while ensuring
        // a FunctionCall or Interpolation cannot retain plaintext arguments
        // that merely *produce* a secret after evaluation.
        return redact_secrets_only(evaluated);
    }
    redact_with_guide(source, SecretRedactionGuide::Evaluated(evaluated))
}

fn redact_using_redacted_shape(
    value: &Value,
    redacted_shape: &Value,
) -> Result<Value, SerializationError> {
    redact_with_guide(value, SecretRedactionGuide::RedactedShape(redacted_shape))
}

fn redact_desired_reusing_redacted_shape(
    value: &Value,
    redacted_shape: &Value,
) -> Result<Value, SerializationError> {
    redact_with_guide(value, SecretRedactionGuide::RedactedDesired(redacted_shape))
}

/// Redact provider plaintext at paths marked secret by the desired value.
pub fn redact_plaintext_using_secret_shape(
    plaintext: &Value,
    secret_shape: &Value,
) -> Result<Value, SerializationError> {
    redact_with_guide(plaintext, SecretRedactionGuide::SecretShape(secret_shape))
}

/// Redact provider plaintext at paths marked by secret hashes in persisted
/// state. Fresh provider plaintext is hashed with a new embedded salt.
/// State values themselves are lifted into typed placeholders when decoded,
/// so this function never classifies source strings as secrets by content.
fn redact_plaintext_using_persisted_state(
    plaintext: &Value,
    persisted: PersistedView<'_>,
) -> Result<Value, SerializationError> {
    redact_with_guide(plaintext, SecretRedactionGuide::Persisted(persisted))
}

/// Apply secret sensitivity to provider plaintext for display only.
///
/// Unlike saved-plan redaction this does not hash or verify anything. It
/// preserves the plaintext for execution while ensuring formatters render
/// every path marked secret by the desired value as `(secret)`.
pub fn mask_plaintext_using_secret_shape(plaintext: &Value, shape: &Value) -> Value {
    project_secret_shape(plaintext, shape, SecretShapeProjectionMode::Display)
}

fn rebuild_projected_string_list(values: Vec<Value>) -> Value {
    let strings = values
        .iter()
        .map(|value| match value {
            Value::Concrete(ConcreteValue::String(value)) => Some(value.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    strings.map_or_else(
        || Value::Concrete(ConcreteValue::List(values)),
        |strings| Value::Concrete(ConcreteValue::StringList(strings)),
    )
}

#[derive(Clone, Copy)]
enum SecretShapeProjectionMode {
    Display,
    FailClosed,
    ResolvedUnion,
}

impl SecretShapeProjectionMode {
    fn preserves_secret_value(self) -> bool {
        matches!(self, Self::FailClosed | Self::ResolvedUnion)
    }

    fn deferred_shape(self, value: &Value, shape: &Value) -> Value {
        if matches!(
            shape,
            Value::Deferred(DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. })
        ) {
            return Value::Deferred(DeferredValue::Secret(Box::new(value.clone())));
        }

        match self {
            Self::Display => value.clone(),
            Self::FailClosed => mark_secret(value),
            Self::ResolvedUnion if matches!(value, Value::Concrete(_)) => value.clone(),
            Self::ResolvedUnion => mark_secret(value),
        }
    }

    fn unprojectable(self, value: &Value) -> Value {
        match self {
            Self::Display => value.clone(),
            Self::FailClosed | Self::ResolvedUnion => mark_secret(value),
        }
    }
}

fn mark_secret(value: &Value) -> Value {
    Value::Deferred(DeferredValue::Secret(Box::new(value.clone())))
}

fn project_secret_shape(value: &Value, shape: &Value, mode: SecretShapeProjectionMode) -> Value {
    if mode.preserves_secret_value()
        && matches!(
            value,
            Value::Deferred(DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. })
        )
    {
        return value.clone();
    }
    if !contains_secret(shape) {
        return value.clone();
    }
    if matches!(shape, Value::Deferred(_)) {
        return mode.deferred_shape(value, shape);
    }

    match (value, shape) {
        (
            Value::Concrete(ConcreteValue::Map(values)),
            Value::Concrete(ConcreteValue::Map(shapes)),
        ) => Value::Concrete(ConcreteValue::Map(
            values
                .iter()
                .map(|(key, value)| {
                    let projected = shapes.get(key).map_or_else(
                        || value.clone(),
                        |shape| project_secret_shape(value, shape, mode),
                    );
                    (key.clone(), projected)
                })
                .collect(),
        )),
        (
            Value::Concrete(ConcreteValue::List(values)),
            Value::Concrete(ConcreteValue::List(shapes)),
        ) => Value::Concrete(ConcreteValue::List(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    shapes.get(index).map_or_else(
                        || value.clone(),
                        |shape| project_secret_shape(value, shape, mode),
                    )
                })
                .collect(),
        )),
        (
            Value::Concrete(ConcreteValue::StringList(values)),
            Value::Concrete(ConcreteValue::List(shapes)),
        ) => rebuild_projected_string_list(
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let value = Value::Concrete(ConcreteValue::String(value.clone()));
                    shapes.get(index).map_or(value.clone(), |shape| {
                        project_secret_shape(&value, shape, mode)
                    })
                })
                .collect(),
        ),
        _ => mode.unprojectable(value),
    }
}

/// Apply a secret shape to provider plaintext, marking only the exact value
/// whose secret-bearing shape cannot be projected structurally. Containers
/// and their other children retain their independently projected shapes.
pub fn mask_plaintext_using_secret_shape_fail_closed(plaintext: &Value, shape: &Value) -> Value {
    project_secret_shape(plaintext, shape, SecretShapeProjectionMode::FailClosed)
}

/// Combine the secret shape selected during apply with secret markers from
/// the authored value used to build the plan.
///
/// The apply-time value is authoritative when a deferred authored expression
/// has resolved to a concrete branch. Structural authored values are merged
/// recursively so an unprojectable child fails closed at that child without
/// tainting its container or siblings.
pub fn union_secret_shapes_after_resolution(applied: &Value, authored: &Value) -> Value {
    project_secret_shape(applied, authored, SecretShapeProjectionMode::ResolvedUnion)
}

/// Apply persisted secret sensitivity to provider plaintext.
///
/// This masks values for display and for pre-apply validation without copying
/// a stored hash into the runtime value. The raw JSON walker stays private;
/// callers must supply the opaque persisted-state view and an attribute key.
fn mask_plaintext_using_persisted_json_shape(
    plaintext: &Value,
    persisted: PersistedView<'_>,
) -> Value {
    if persisted.is_redacted_secret() {
        return Value::Deferred(DeferredValue::Secret(Box::new(plaintext.clone())));
    }
    match plaintext {
        Value::Concrete(ConcreteValue::Map(values)) => persisted.as_map().map_or_else(
            || plaintext.clone(),
            |shapes| {
                Value::Concrete(ConcreteValue::Map(
                    values
                        .iter()
                        .map(|(key, value)| {
                            let masked = shapes.get(key).map_or_else(
                                || value.clone(),
                                |shape| mask_plaintext_using_persisted_json_shape(value, shape),
                            );
                            (key.clone(), masked)
                        })
                        .collect(),
                ))
            },
        ),
        Value::Concrete(ConcreteValue::List(values)) => persisted.as_list().map_or_else(
            || plaintext.clone(),
            |shapes| {
                Value::Concrete(ConcreteValue::List(
                    values
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            shapes.get(index).map_or_else(
                                || value.clone(),
                                |shape| mask_plaintext_using_persisted_json_shape(value, shape),
                            )
                        })
                        .collect(),
                ))
            },
        ),
        _ => plaintext.clone(),
    }
}

/// Mask one provider value according to the same attribute in persisted state.
pub fn mask_plaintext_using_persisted_attribute(
    plaintext: &Value,
    persisted: &crate::provider::PersistedAttributes,
    key: &str,
) -> Value {
    persisted.encoded().get(key).map_or_else(
        || plaintext.clone(),
        |shape| mask_plaintext_using_persisted_json_shape(plaintext, shape.view()),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SecretRehydrationInput {
    /// A freshly evaluated configuration value. A saved placeholder must align
    /// with a typed `Secret` leaf.
    Evaluated,
    /// Hash material below a typed `Secret` that was temporarily unwrapped so
    /// a saved structural template can be traversed.
    EvaluatedHashMaterial,
    /// Plaintext returned by a provider for historical-state verification.
    Plaintext,
}

fn rehydrate_redacted_secrets_impl<P: PlaceholderKind>(
    saved: &Value<P>,
    candidate: &Value<P>,
    input: SecretRehydrationInput,
) -> Result<Value<P>, SecretRehydrationError> {
    if !contains_redacted_secret(saved) {
        return Ok(saved.clone());
    }

    // A secret interpolation/function may have been saved as a redacted
    // structural template because its final value depended on an apply-time
    // reference. Verify every template leaf against the fresh secret's inner
    // expression, then return the fresh typed Secret so taint is preserved.
    if input == SecretRehydrationInput::Evaluated
        && !matches!(saved, Value::Deferred(DeferredValue::RedactedSecret { .. }))
        && let Value::Deferred(DeferredValue::Secret(inner)) = candidate
    {
        rehydrate_redacted_secrets_impl(
            saved,
            inner,
            SecretRehydrationInput::EvaluatedHashMaterial,
        )?;
        return Ok(candidate.clone());
    }

    match saved {
        Value::Deferred(DeferredValue::RedactedSecret { hash }) => {
            let hash_material = match input {
                SecretRehydrationInput::Evaluated => match candidate {
                    Value::Deferred(DeferredValue::Secret(inner)) => inner.as_ref(),
                    _ => return Err(SecretRehydrationError::ExpectedSecret),
                },
                SecretRehydrationInput::EvaluatedHashMaterial
                | SecretRehydrationInput::Plaintext => candidate,
            };
            if find_unavailable_evaluation(
                hash_material,
                UnavailableCoalescePolicy::IncludeDefault,
                UnavailableSecretPolicy::Include,
            )
            .is_some()
            {
                return Err(SecretRehydrationError::EvaluationUnavailable);
            }
            let material = canonical_secret_hash_material(hash_material)?;
            if !hash.verify(material) {
                return Err(SecretRehydrationError::HashMismatch);
            }
            Ok(candidate.clone())
        }
        Value::Concrete(ConcreteValue::Map(saved)) => {
            let Value::Concrete(ConcreteValue::Map(candidate)) = candidate else {
                return Err(SecretRehydrationError::ShapeMismatch);
            };
            let values = saved
                .iter()
                .map(|(key, value)| {
                    let value = if contains_redacted_secret(value) {
                        let candidate = candidate
                            .get(key)
                            .ok_or(SecretRehydrationError::ShapeMismatch)?;
                        rehydrate_redacted_secrets_impl(value, candidate, input)?
                    } else {
                        value.clone()
                    };
                    Ok((key.clone(), value))
                })
                .collect::<Result<IndexMap<_, _>, SecretRehydrationError>>()?;
            Ok(Value::Concrete(ConcreteValue::Map(values)))
        }
        Value::Concrete(ConcreteValue::List(saved)) => {
            let Value::Concrete(ConcreteValue::List(candidate)) = candidate else {
                return Err(SecretRehydrationError::ShapeMismatch);
            };
            let values = saved
                .iter()
                .enumerate()
                .map(|(index, saved)| {
                    if !contains_redacted_secret(saved) {
                        return Ok(saved.clone());
                    }
                    let candidate = candidate
                        .get(index)
                        .ok_or(SecretRehydrationError::ShapeMismatch)?;
                    rehydrate_redacted_secrets_impl(saved, candidate, input)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Concrete(ConcreteValue::List(values)))
        }
        Value::Deferred(DeferredValue::Interpolation(saved)) => {
            let Value::Deferred(DeferredValue::Interpolation(candidate)) = candidate else {
                return Err(SecretRehydrationError::ShapeMismatch);
            };
            let parts = saved
                .iter()
                .enumerate()
                .map(|(index, saved)| match saved {
                    InterpolationPart::Literal(saved) => {
                        Ok(InterpolationPart::Literal(saved.clone()))
                    }
                    InterpolationPart::Expr(saved) if !contains_redacted_secret(saved) => {
                        Ok(InterpolationPart::Expr(saved.clone()))
                    }
                    InterpolationPart::Expr(saved) => match candidate.get(index) {
                        Some(InterpolationPart::Expr(candidate)) => {
                            rehydrate_redacted_secrets_impl(saved, candidate, input)
                                .map(InterpolationPart::Expr)
                        }
                        Some(InterpolationPart::Literal(candidate)) => {
                            let candidate =
                                Value::Concrete(ConcreteValue::String(candidate.clone()));
                            let input = match input {
                                SecretRehydrationInput::Plaintext => {
                                    SecretRehydrationInput::Plaintext
                                }
                                SecretRehydrationInput::Evaluated
                                | SecretRehydrationInput::EvaluatedHashMaterial => {
                                    SecretRehydrationInput::EvaluatedHashMaterial
                                }
                            };
                            let value = rehydrate_redacted_secrets_impl(saved, &candidate, input)?;
                            match value {
                                Value::Concrete(ConcreteValue::String(value)) => {
                                    Ok(InterpolationPart::Literal(value))
                                }
                                value => Ok(InterpolationPart::Expr(value)),
                            }
                        }
                        None => Err(SecretRehydrationError::ShapeMismatch),
                    },
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Deferred(DeferredValue::Interpolation(parts)))
        }
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args: saved,
            source_line,
        }) => {
            let Value::Deferred(
                DeferredValue::FunctionCall {
                    name: candidate_name,
                    args: candidate,
                    ..
                }
                | DeferredValue::Unavailable {
                    name: candidate_name,
                    args: candidate,
                    ..
                },
            ) = candidate
            else {
                return Err(SecretRehydrationError::ShapeMismatch);
            };
            if name != candidate_name {
                return Err(SecretRehydrationError::ShapeMismatch);
            }
            let args = saved
                .iter()
                .enumerate()
                .map(|(index, saved)| {
                    if !contains_redacted_secret(saved.walker_value()) {
                        return Ok(saved.clone());
                    }
                    let candidate = candidate
                        .get(index)
                        .ok_or(SecretRehydrationError::ShapeMismatch)?;
                    let (Some(saved), Some(candidate)) = (saved.value(), candidate.value()) else {
                        return Err(SecretRehydrationError::ShapeMismatch);
                    };
                    rehydrate_redacted_secrets_impl(saved, candidate, input)
                        .map(crate::resource::FunctionArgument::from_value)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Deferred(DeferredValue::FunctionCall {
                name: *name,
                args,
                source_line: *source_line,
            }))
        }
        Value::Deferred(DeferredValue::Access(saved_access)) => {
            let Value::Deferred(DeferredValue::Access(candidate_access)) = candidate else {
                return Err(SecretRehydrationError::ShapeMismatch);
            };
            if saved_access.step() != candidate_access.step() {
                return Err(SecretRehydrationError::ShapeMismatch);
            }
            let base = rehydrate_redacted_secrets_impl(
                saved_access.base(),
                candidate_access.base(),
                input,
            )?;
            saved_access
                .rebuild_with_base(base, crate::resource::AccessMissPolicy::Error)
                .map_err(|_| SecretRehydrationError::ShapeMismatch)
        }
        Value::Deferred(DeferredValue::Coalesce {
            value: saved_value,
            default: saved_default,
        }) => {
            let Value::Deferred(DeferredValue::Coalesce {
                value: candidate_value,
                default: candidate_default,
            }) = candidate
            else {
                return Err(SecretRehydrationError::ShapeMismatch);
            };
            let value = rehydrate_redacted_secrets_impl(saved_value, candidate_value, input)?;
            let default = rehydrate_redacted_secrets_impl(saved_default, candidate_default, input)?;
            Ok(Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(value),
                default: Box::new(default),
            }))
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(saved_expression)) => {
            let Value::Deferred(DeferredValue::IntegerArithmetic(candidate_expression)) = candidate
            else {
                return Err(SecretRehydrationError::ShapeMismatch);
            };
            if saved_expression.op() != candidate_expression.op() {
                return Err(SecretRehydrationError::ShapeMismatch);
            }
            let lhs = if contains_redacted_secret(saved_expression.lhs()) {
                rehydrate_redacted_secrets_impl(
                    saved_expression.lhs(),
                    candidate_expression.lhs(),
                    input,
                )?
            } else {
                saved_expression.lhs().clone()
            };
            let rhs = if contains_redacted_secret(saved_expression.rhs()) {
                rehydrate_redacted_secrets_impl(
                    saved_expression.rhs(),
                    candidate_expression.rhs(),
                    input,
                )?
            } else {
                saved_expression.rhs().clone()
            };
            saved_expression
                .rebuild(lhs, rhs)
                .map(DeferredValue::IntegerArithmetic)
                .map(Value::Deferred)
                .map_err(|_| SecretRehydrationError::ShapeMismatch)
        }
        _ => Err(SecretRehydrationError::ShapeMismatch),
    }
}

/// Restore saved-plan placeholders from freshly evaluated configuration
/// values, verifying the plan-time hash at every placeholder.
pub fn rehydrate_redacted_secrets<P: PlaceholderKind>(
    saved: &Value<P>,
    evaluated: &Value<P>,
) -> Result<Value<P>, SecretRehydrationError> {
    if contains_redacted_secret(saved)
        && find_unavailable_evaluation(
            evaluated,
            UnavailableCoalescePolicy::IncludeDefault,
            UnavailableSecretPolicy::OnlyInside,
        )
        .is_some()
    {
        return Err(SecretRehydrationError::EvaluationUnavailable);
    }
    rehydrate_redacted_secrets_impl(saved, evaluated, SecretRehydrationInput::Evaluated)
}

/// Restore historical saved-plan placeholders from a fresh provider read,
/// verifying the plan-time hash before returning provider plaintext.
pub fn rehydrate_redacted_secrets_from_plaintext<P: PlaceholderKind>(
    saved: &Value<P>,
    plaintext: &Value<P>,
) -> Result<Value<P>, SecretRehydrationError> {
    rehydrate_redacted_secrets_impl(saved, plaintext, SecretRehydrationInput::Plaintext)
}

/// Whether a saved-plan placeholder candidate is still wrapped as a typed
/// secret or is plaintext returned by a provider read.
#[derive(Clone, Copy)]
pub enum SecretRehydrationCandidate {
    Evaluated,
    Plaintext,
}

/// Rehydrate one saved-plan value when it contains a typed secret placeholder.
///
/// Placeholder-free values return `Ok(None)` without requiring a candidate,
/// which keeps secret verification transparent to ordinary saved-plan values.
pub fn rehydrate_secret_entry<P: PlaceholderKind>(
    saved: &Value<P>,
    candidate: Option<&Value<P>>,
    location: impl Into<String>,
    candidate_kind: SecretRehydrationCandidate,
) -> Result<Option<Value<P>>, SecretRehydrationSiteError> {
    if !contains_redacted_secret(saved) {
        return Ok(None);
    }

    let location = location.into();
    let candidate = candidate.ok_or_else(|| {
        SecretRehydrationSiteError::new(location.clone(), SecretRehydrationError::ShapeMismatch)
    })?;
    let value = match candidate_kind {
        SecretRehydrationCandidate::Evaluated => rehydrate_redacted_secrets(saved, candidate),
        SecretRehydrationCandidate::Plaintext => {
            rehydrate_redacted_secrets_from_plaintext(saved, candidate)
        }
    }
    .map_err(|source| SecretRehydrationSiteError::new(location, source))?;
    Ok(Some(value))
}

pub fn redact_secrets_in_value(value: &Value) -> Result<Value, SerializationError> {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => redact_secret_leaf(inner),
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => Ok(value.clone()),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let redacted: Result<IndexMap<String, Value>, _> = map
                .iter()
                .map(|(k, v)| redact_secrets_in_value(v).map(|rv| (k.clone(), rv)))
                .collect();
            Ok(Value::Concrete(ConcreteValue::Map(redacted?)))
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let redacted: Result<Vec<_>, _> = items.iter().map(redact_secrets_in_value).collect();
            Ok(Value::Concrete(ConcreteValue::List(redacted?)))
        }
        // carina#3329: an `Interpolation` can carry a secret under an
        // `Expr` part (e.g. `id = "${secret_let.value}|tail"`). Walk
        // the parts via the secret-only redactor — a generic
        // `redact_secrets_in_value` recurse would reject the deferred
        // `Unknown(UpstreamExport)` produced by plan-time stamping (RFC
        // #2371 stage-4 invariant), even though the supported scenario
        // here is exactly "deferred upstream ref inside import id".
        // The dedicated `redact_secrets_only` walker only hashes
        // `Secret` leaves and passes every other shape through
        // unchanged, so `Unknown` survives saved-plan write to the
        // apply side where the version-5 contract is enforced.
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let redacted: Result<Vec<InterpolationPart>, _> = parts
                .iter()
                .map(|p| match p {
                    InterpolationPart::Literal(s) => Ok(InterpolationPart::Literal(s.clone())),
                    InterpolationPart::Expr(v) => {
                        redact_secrets_only(v).map(InterpolationPart::Expr)
                    }
                })
                .collect();
            Ok(Value::Deferred(DeferredValue::Interpolation(redacted?)))
        }
        // Sibling shape: a deferred function-call carries `Value` args
        // that could equally hide a `Secret`. Same Unknown-tolerant
        // recursion via `redact_secrets_only`.
        Value::Deferred(DeferredValue::FunctionCall {
            name,
            args,
            source_line,
        }) => {
            let redacted = args
                .iter()
                .map(|argument| argument.try_map_value(redact_secrets_only))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Deferred(DeferredValue::FunctionCall {
                name: *name,
                args: redacted,
                source_line: *source_line,
            }))
        }
        Value::Deferred(DeferredValue::Unavailable {
            name,
            args,
            reason,
            source_line,
        }) => {
            let redacted = args
                .iter()
                .map(|argument| argument.try_map_value(redact_secrets_only))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Deferred(DeferredValue::Unavailable {
                name: *name,
                args: redacted,
                reason: reason.clone(),
                source_line: *source_line,
            }))
        }
        Value::Deferred(DeferredValue::ResolvedFunctionCall {
            function,
            functions,
            args,
            source_line,
        }) => {
            let redacted = args
                .iter()
                .map(|argument| argument.try_map_value(redact_secrets_only))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function: function.clone(),
                functions: functions.clone(),
                args: redacted,
                source_line: *source_line,
            }))
        }
        Value::Deferred(DeferredValue::Access(access)) => access
            .rebuild_with_base(
                redact_secrets_only(access.base())?,
                crate::resource::AccessMissPolicy::Error,
            )
            .map_err(|_| SerializationError::UnresolvedAccess {
                expression: format_value(value),
                context: SerializationContext::SecretRedaction,
            }),
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            Ok(Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(redact_secrets_only(value)?),
                default: Box::new(redact_secrets_only(default)?),
            }))
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            let lhs = redact_secrets_only(expression.lhs())?;
            let rhs = redact_secrets_only(expression.rhs())?;
            expression.evaluate_with_operands(lhs, rhs).map_err(|_| {
                SerializationError::UnresolvedIntegerArithmetic {
                    context: SerializationContext::SecretRedaction,
                }
            })
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            Err(SerializationError::UnknownNotAllowed {
                reason: Box::new(reason.clone()),
                context: SerializationContext::SecretRedaction,
            })
        }
        other => Ok(other.clone()),
    }
}

fn redact_attrs_with_guide<'a, Attributes, Redact>(
    attrs: impl Iterator<Item = (&'a String, &'a Value)>,
    mut redact: Redact,
) -> Result<Attributes, SerializationError>
where
    Attributes: FromIterator<(String, Value)>,
    Redact: FnMut(&str, &Value) -> Result<Value, SerializationError>,
{
    attrs
        .map(|(key, value)| redact(key, value).map(|value| (key.clone(), value)))
        .collect()
}

/// Redact all secrets in an attributes map.
pub fn redact_secrets_in_attributes(
    attrs: &HashMap<String, Value>,
) -> Result<HashMap<String, Value>, SerializationError> {
    redact_attrs_with_guide(attrs.iter(), |_, value| redact_secrets_in_value(value))
}

/// Redact all secrets in a `Resource`, returning a new Resource with secrets replaced by hashes.
pub fn redact_secrets_in_resource<S: Clone, D: Clone>(
    resource: &crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
) -> Result<
    crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    SerializationError,
> {
    let attributes = redact_attrs_with_guide(resource.attributes.iter(), |_, value| {
        redact_secrets_in_value(value)
    })?;
    Ok(crate::resource::Resource {
        attributes,
        ..resource.clone()
    })
}

/// Redact desired secrets while reusing matching persisted digests at the
/// same nested paths.
pub fn redact_secrets_in_resource_reusing_persisted_hashes<S: Clone, D: Clone>(
    resource: &crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    persisted: &crate::provider::PersistedAttributes,
) -> Result<
    crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    SerializationError,
> {
    let attributes = redact_attrs_with_guide(resource.attributes.iter(), |key, value| {
        match persisted.encoded_value(key) {
            Some(previous) => redact_with_guide(
                value,
                SecretRedactionGuide::PersistedDesired(previous.view()),
            ),
            None => redact_secrets_only(value),
        }
    })?;
    Ok(crate::resource::Resource {
        attributes,
        ..resource.clone()
    })
}

/// Redact an effect resource in place, using a separately redacted planned
/// resource only as the source of reusable secret digests. Non-secret values
/// stay exactly as they appear in the effect: replacement creates may carry a
/// temporary name, and cascade updates may deliberately retain unresolved
/// references for apply-time resolution.
fn redact_resource_using_redacted_shape<S: Clone, D: Clone, T, E>(
    resource: &crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    guide: &crate::resource::Resource<
        T,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        E,
    >,
) -> Result<
    crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    SerializationError,
> {
    let attributes = redact_attrs_with_guide(resource.attributes.iter(), |key, value| match guide
        .attributes
        .get(key)
    {
        Some(shape) => redact_desired_reusing_redacted_shape(value, shape),
        None => redact_secrets_only(value),
    })?;
    Ok(crate::resource::Resource {
        attributes,
        ..resource.clone()
    })
}

/// Redact an authored resource by pairing it with the evaluator's resource at
/// the same identity. Attribute names form the first path segment; nested
/// paths are handled by [`redact_secrets_using_evaluated`].
pub fn redact_secrets_in_resource_using_evaluated<S: Clone, D: Clone, T, E>(
    source: &crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    evaluated: &crate::resource::Resource<
        T,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        E,
    >,
) -> Result<
    crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    SerializationError,
> {
    let attributes =
        redact_attrs_with_guide(source.attributes.iter(), |key, value| {
            match evaluated.attributes.get(key) {
                Some(evaluated) => redact_secrets_using_evaluated(value, evaluated),
                None => redact_secrets_only(value),
            }
        })?;
    Ok(crate::resource::Resource {
        attributes,
        ..source.clone()
    })
}

/// Restore every placeholder in a resource from a freshly evaluated resource.
pub fn rehydrate_resource_secrets<S: Clone, D: Clone, T, E>(
    saved: &crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    evaluated: &crate::resource::Resource<
        T,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        E,
    >,
) -> Result<
    crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    SecretRehydrationSiteError,
> {
    let mut resource = saved.clone();
    for (key, saved_value) in &saved.attributes {
        if let Some(value) = rehydrate_secret_entry(
            saved_value,
            evaluated.attributes.get(key),
            format!("{}.{}", saved.id, key),
            SecretRehydrationCandidate::Evaluated,
        )? {
            resource.attributes.insert(key.clone(), value);
        }
    }
    Ok(resource)
}

/// Redact all secrets in a [`DataSource`](crate::resource::DataSource).
pub fn redact_secrets_in_data_source<S: Clone, D: Clone>(
    resource: &crate::resource::DataSource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
) -> Result<
    crate::resource::DataSource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    SerializationError,
> {
    let attributes = redact_attrs_with_guide(resource.attributes.iter(), |_, value| {
        redact_secrets_in_value(value)
    })?;
    Ok(crate::resource::DataSource {
        attributes,
        ..resource.clone()
    })
}

pub fn redact_secrets_in_data_source_using_evaluated<S: Clone, D: Clone, T, E>(
    source: &crate::resource::DataSource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    evaluated: &crate::resource::DataSource<
        T,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        E,
    >,
) -> Result<
    crate::resource::DataSource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    SerializationError,
> {
    let attributes =
        redact_attrs_with_guide(source.attributes.iter(), |key, value| {
            match evaluated.attributes.get(key) {
                Some(evaluated) => redact_secrets_using_evaluated(value, evaluated),
                None => redact_secrets_only(value),
            }
        })?;
    Ok(crate::resource::DataSource {
        attributes,
        ..source.clone()
    })
}

pub fn rehydrate_data_source_secrets<S: Clone, D: Clone, T, E>(
    saved: &crate::resource::DataSource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    evaluated: &crate::resource::DataSource<
        T,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        E,
    >,
) -> Result<
    crate::resource::DataSource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    SecretRehydrationSiteError,
> {
    let mut resource = saved.clone();
    for (key, saved_value) in &saved.attributes {
        if let Some(value) = rehydrate_secret_entry(
            saved_value,
            evaluated.attributes.get(key),
            format!("{}.{}", saved.id, key),
            SecretRehydrationCandidate::Evaluated,
        )? {
            resource.attributes.insert(key.clone(), value);
        }
    }
    Ok(resource)
}

/// Redact all secrets in a
/// [`LiveComposition`](crate::resource::LiveComposition) (carina#3248).
///
/// compositions are now persisted in saved plan files (`PlanFile`
/// version `4`) so the saved-plan apply path can rebuild the same
/// `ResolvedBindings` view as the live-apply path (carina#3246). A
/// composition's attribute map can hold values copied through from an
/// inner module's `attributes { ... }` block — including literal
/// secrets — so it must pass through the same per-kind redaction as
/// managed resources / data sources / state before serialization.
/// The `IndexMap<String, Value>` shape (vs `HashMap` for managed
/// resources) is preserved so the user-authored attribute order
/// survives redaction.
pub fn redact_secrets_in_live_composition(
    resource: &crate::resource::LiveComposition,
) -> Result<crate::resource::LiveComposition, SerializationError> {
    let arguments: Result<indexmap::IndexMap<String, crate::resource::LiveCompositionArgument>, _> =
        resource
            .signature
            .arguments
            .iter()
            .map(|(k, argument)| {
                redact_secrets_in_value(argument.value())
                    .map(|rv| (k.clone(), argument.with_value(rv)))
            })
            .collect();
    let attributes: Result<
        indexmap::IndexMap<String, crate::resource::LiveCompositionAttribute>,
        _,
    > = resource
        .signature
        .attributes
        .iter()
        .map(|(k, attr)| match attr.value() {
            Some(value) => {
                redact_secrets_in_value(&value).map(|value| (k.clone(), attr.with_value(value)))
            }
            None => Ok((k.clone(), attr.clone())),
        })
        .collect();
    let mut out = resource.clone();
    out.signature.arguments = arguments?;
    out.signature.attributes = attributes?;
    Ok(out)
}

pub fn redact_secrets_in_saved_composition(
    resource: &crate::resource::SavedComposition,
) -> Result<crate::resource::SavedComposition, SerializationError> {
    let arguments: Result<
        indexmap::IndexMap<String, crate::resource::SavedCompositionArgument>,
        _,
    > = resource
        .signature
        .arguments
        .iter()
        .map(|(name, argument)| {
            redact_secrets_in_value(argument.value())
                .map(|value| (name.clone(), argument.with_value(value)))
        })
        .collect();
    let attributes: Result<
        indexmap::IndexMap<String, crate::resource::SavedCompositionAttribute>,
        _,
    > = resource
        .signature
        .attributes
        .iter()
        .map(|(name, attribute)| {
            redact_secrets_in_value(&attribute.to_value())
                .map(|value| (name.clone(), attribute.with_value(value)))
        })
        .collect();
    let mut redacted = resource.clone();
    redacted.signature.arguments = arguments?;
    redacted.signature.attributes = attributes?;
    Ok(redacted)
}

pub fn rehydrate_saved_composition_secrets(
    saved: &crate::resource::SavedComposition,
    evaluated: &crate::resource::LiveComposition,
) -> Result<crate::resource::SavedComposition, SecretRehydrationSiteError> {
    let mut out = saved.clone();
    for (key, saved_argument) in &saved.signature.arguments {
        let value = rehydrate_secret_entry(
            saved_argument.value(),
            evaluated
                .signature
                .arguments
                .get(key)
                .map(crate::resource::LiveCompositionArgument::value),
            format!("{}.arguments.{key}", saved.id),
            SecretRehydrationCandidate::Evaluated,
        )?;
        if let Some(value) = value {
            out.signature
                .arguments
                .insert(key.clone(), saved_argument.with_value(value));
        }
    }
    for (key, saved_attribute) in &saved.signature.attributes {
        let saved_value = saved_attribute.to_value();
        let evaluated_value = evaluated
            .signature
            .attributes
            .get(key)
            .and_then(crate::resource::LiveCompositionAttribute::value);
        let value = rehydrate_secret_entry(
            &saved_value,
            evaluated_value.as_ref(),
            format!("{}.attributes.{key}", saved.id),
            SecretRehydrationCandidate::Evaluated,
        )?;
        if let Some(value) = value {
            out.signature
                .attributes
                .insert(key.clone(), saved_attribute.with_value(value));
        }
    }
    Ok(out)
}

/// Redact all secrets in a `State`, returning a new State with secrets replaced by hashes.
pub fn redact_secrets_in_state(
    state: &crate::resource::State,
) -> Result<crate::resource::State, SerializationError> {
    Ok(crate::resource::State {
        id: state.id.clone(),
        identifier: state.identifier.clone(),
        attributes: redact_secrets_in_attributes(&state.attributes)?,
        exists: state.exists,
        dependency_bindings: state.dependency_bindings.clone(),
        partial_read: state.partial_read.clone(),
    })
}

/// Redact provider plaintext in a state using the corresponding desired
/// resource as the secret-shape authority.
pub fn redact_secrets_in_state_using_resource<D>(
    state: &crate::resource::State,
    desired: &crate::resource::Resource<
        crate::resource::ModuleSource,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
) -> Result<crate::resource::State, SerializationError> {
    let attributes = redact_attrs_with_guide(state.attributes.iter(), |key, value| match desired
        .attributes
        .get(key)
    {
        Some(shape) => redact_plaintext_using_secret_shape(value, shape),
        None => redact_secrets_only(value),
    })?;
    Ok(crate::resource::State {
        attributes,
        ..state.clone()
    })
}

pub fn redact_secrets_in_state_using_persisted(
    state: &crate::resource::State,
    persisted: &crate::provider::PersistedAttributes,
) -> Result<crate::resource::State, SerializationError> {
    let attributes =
        redact_attrs_with_guide(state.attributes.iter(), |key, value| {
            match persisted.encoded().get(key) {
                Some(marker) => redact_plaintext_using_persisted_state(value, marker.view()),
                None => redact_secrets_only(value),
            }
        })?;
    Ok(crate::resource::State {
        attributes,
        ..state.clone()
    })
}

/// Result of comparing every historical secret placeholder with a fresh
/// provider read.
///
/// Hash/shape mismatches are data-plane drift, not configuration-secret
/// failures. They remain attached to their top-level attributes so the drift
/// reporter can mask those values.
pub struct HistoricalSecretVerification {
    state: crate::resource::State,
    drifted_attributes: BTreeSet<String>,
    drift_errors: Vec<SecretRehydrationSiteError>,
}

impl HistoricalSecretVerification {
    pub fn state(&self) -> &crate::resource::State {
        &self.state
    }

    pub fn drifted_attributes(&self) -> &BTreeSet<String> {
        &self.drifted_attributes
    }

    pub fn into_verified_state(self) -> Result<crate::resource::State, SecretRehydrationSiteError> {
        match self.drift_errors.into_iter().next() {
            Some(error) => Err(error),
            None => Ok(self.state),
        }
    }
}

/// Compare historical state placeholders with a fresh provider read.
pub fn verify_historical_state_secrets(
    saved: &crate::resource::State,
    actual: &crate::resource::State,
) -> Result<HistoricalSecretVerification, SecretRehydrationSiteError> {
    let mut state = saved.clone();
    let mut drifted_attributes = BTreeSet::new();
    let mut drift_errors = Vec::new();
    for (key, saved_value) in &saved.attributes {
        // The managed-read seam may fill a provider-omitted write-only value
        // with the exact persisted placeholder from the current state row.
        // That is historical evidence, not provider plaintext to rehydrate.
        // Keep genuinely provider-returned redacted values strict: only an
        // exact match with the saved placeholder represents no drift.
        if contains_redacted_secret(saved_value) && actual.attributes.get(key) == Some(saved_value)
        {
            continue;
        }
        let result = rehydrate_secret_entry(
            saved_value,
            actual.attributes.get(key),
            format!("{}.{}", saved.id, key),
            SecretRehydrationCandidate::Plaintext,
        );
        match result {
            Ok(Some(value)) => {
                state.attributes.insert(key.clone(), value);
            }
            Ok(None) => {}
            Err(error)
                if matches!(
                    &error.source,
                    SecretRehydrationError::ExpectedSecret
                        | SecretRehydrationError::HashMismatch
                        | SecretRehydrationError::ShapeMismatch
                        | SecretRehydrationError::EvaluationUnavailable
                ) =>
            {
                drifted_attributes.insert(key.clone());
                drift_errors.push(error);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(HistoricalSecretVerification {
        state,
        drifted_attributes,
        drift_errors,
    })
}

/// Strict historical rehydration used after drift classification (for
/// example orphan deletes). Any mismatch is returned as a typed verification
/// error.
pub fn rehydrate_state_secrets_from_plaintext(
    saved: &crate::resource::State,
    actual: &crate::resource::State,
) -> Result<crate::resource::State, SecretRehydrationSiteError> {
    verify_historical_state_secrets(saved, actual)?.into_verified_state()
}

fn redact_deferred_template(
    template: &crate::parser::CompiledDeferredForExpression,
) -> Result<crate::parser::CompiledDeferredForExpression, SerializationError> {
    let mut redacted = template.clone();
    redacted.iterable = redact_secrets_only(&template.iterable)?;
    *redacted.template_attributes_mut() = template
        .attributes()
        .iter()
        .map(|(key, value)| redact_secrets_only(value).map(|value| (key.clone(), value)))
        .collect::<Result<_, _>>()?;
    Ok(redacted)
}

/// Redact all secrets in an `Effect`, returning a new Effect with secrets replaced by hashes.
pub fn redact_secrets_in_effect(
    effect: &crate::effect::Effect,
) -> Result<crate::effect::Effect, SerializationError> {
    use crate::effect::{DeferredCreatePayload, DeferredReplacePayload, Effect};
    Ok(match effect {
        Effect::Read { resource } => Effect::Read {
            resource: resource.replace_inner(redact_secrets_in_data_source(resource)?),
        },
        Effect::Create(resource) => {
            Effect::Create(resource.replace_inner(redact_secrets_in_resource(resource)?))
        }
        Effect::ResumeCreate {
            resource,
            started_at,
        } => Effect::ResumeCreate {
            resource: resource.replace_inner(redact_secrets_in_resource(resource)?),
            started_at: started_at.clone(),
        },
        Effect::Update {
            from,
            identifier,
            to,
            changed_attributes,
        } => Effect::Update {
            from: Box::new(redact_secrets_in_state(from)?),
            identifier: identifier.clone(),
            to: to.replace_inner(redact_secrets_in_resource(to)?),
            changed_attributes: changed_attributes.clone(),
        },
        Effect::Delete {
            id,
            identifier,
            generation,
            directives,
            binding,
            dependencies,
            explicit_dependencies,
            blocked_by_updates,
        } => Effect::Delete {
            id: id.clone(),
            identifier: identifier.clone(),
            generation: generation.clone(),
            directives: directives.clone(),
            binding: binding.clone(),
            dependencies: dependencies.clone(),
            explicit_dependencies: explicit_dependencies.clone(),
            blocked_by_updates: blocked_by_updates.clone(),
        },
        Effect::Import { id, identifier } => Effect::Import {
            id: id.clone(),
            identifier: redact_secrets_in_value(identifier)?,
        },
        Effect::Remove { id } => Effect::Remove { id: id.clone() },
        Effect::Move { from, to, origin } => Effect::Move {
            from: from.clone(),
            to: to.clone(),
            origin: origin.clone(),
        },
        Effect::ReleaseListKeys {
            id,
            attribute,
            key_field,
            keys,
        } => Effect::ReleaseListKeys {
            id: id.clone(),
            attribute: attribute.clone(),
            key_field: key_field.clone(),
            keys: crate::effect::ReleasedListKeys::try_new(
                keys.iter()
                    .map(|key| {
                        redact_secrets_in_value(key.retained_value()).map(|retained| {
                            crate::effect::ReleasedListKey::new(key.key(), retained)
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            )
            .expect("redaction preserves non-empty unique release keys"),
        },
        // Wait effects carry no secret-bearing fields — `until` is a
        // typed predicate over scalar values, surface form is the
        // user-authored source. Clone through unchanged.
        Effect::Wait { .. } => effect.clone(),
        Effect::DeferredCreate(payload) => Effect::DeferredCreate(DeferredCreatePayload::new(
            Box::new(redact_deferred_template(&payload.template)?),
            payload.dependency_template.clone(),
        )),
        Effect::DeferredReplace(payload) => {
            Effect::DeferredReplace(Box::new(DeferredReplacePayload {
                deletes: payload.deletes.clone(),
                dependency_template: payload.dependency_template.clone(),
                template: Box::new(redact_deferred_template(&payload.template)?),
            }))
        }
    })
}

/// Redact all secrets in a `Plan`, returning a new Plan with secrets replaced by hashes.
pub fn redact_secrets_in_plan(
    plan: &crate::plan::Plan,
) -> Result<crate::plan::Plan, SerializationError> {
    let planned_resources: HashMap<_, crate::resource::Resource> = HashMap::new();
    redact_secrets_in_plan_with_desired_and_historical(plan, &planned_resources, &HashMap::new())
}

fn create_effect_ids<S>(plan: &crate::plan::Plan<S>) -> Vec<Option<crate::resource::ResourceId>> {
    plan.effects()
        .iter()
        .map(|effect| match effect {
            crate::effect::Effect::Create(resource) => Some(resource.id.clone()),
            crate::effect::Effect::Read { .. }
            | crate::effect::Effect::ResumeCreate { .. }
            | crate::effect::Effect::Update { .. }
            | crate::effect::Effect::Delete { .. }
            | crate::effect::Effect::Import { .. }
            | crate::effect::Effect::Remove { .. }
            | crate::effect::Effect::Move { .. }
            | crate::effect::Effect::ReleaseListKeys { .. }
            | crate::effect::Effect::Wait { .. }
            | crate::effect::Effect::DeferredCreate(_)
            | crate::effect::Effect::DeferredReplace(_) => None,
        })
        .collect()
}

pub fn redact_secrets_in_plan_with_desired_and_historical<D>(
    plan: &crate::plan::Plan,
    planned_resources: &HashMap<
        crate::resource::ResourceId,
        crate::resource::Resource<
            crate::resource::ModuleSource,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
    >,
    historical: &HashMap<crate::resource::ResourceId, crate::resource::State>,
) -> Result<crate::plan::Plan, SerializationError> {
    let reusable_resource = |id: &crate::resource::ResourceId| {
        planned_resources
            .get(id)
            .filter(|resource| resource.attributes.values().any(contains_redacted_secret))
    };
    let mut redacted = plan.clone();
    let redacted_effects: Vec<_> = plan
        .effects()
        .iter()
        .map(|effect| {
            let mut effect = match effect {
                crate::effect::Effect::Create(resource) => {
                    match reusable_resource(&resource.id) {
                        Some(planned) => crate::effect::Effect::Create(resource.replace_inner(
                            redact_resource_using_redacted_shape(resource, planned)?,
                        )),
                        None => redact_secrets_in_effect(effect)?,
                    }
                }
                crate::effect::Effect::Update {
                    from,
                    identifier,
                    to,
                    changed_attributes,
                } => crate::effect::Effect::Update {
                    from: Box::new(redact_secrets_in_state(from)?),
                    identifier: identifier.clone(),
                    to: match reusable_resource(&to.id) {
                        Some(planned) => {
                            to.replace_inner(redact_resource_using_redacted_shape(to, planned)?)
                        }
                        None => to.replace_inner(redact_secrets_in_resource(to)?),
                    },
                    changed_attributes: changed_attributes.clone(),
                },
                crate::effect::Effect::Read { .. }
                | crate::effect::Effect::ResumeCreate { .. }
                | crate::effect::Effect::Delete { .. }
                | crate::effect::Effect::Import { .. }
                | crate::effect::Effect::Remove { .. }
                | crate::effect::Effect::Move { .. }
                | crate::effect::Effect::ReleaseListKeys { .. }
                | crate::effect::Effect::Wait { .. }
                | crate::effect::Effect::DeferredCreate(_)
                | crate::effect::Effect::DeferredReplace(_) => redact_secrets_in_effect(effect)?,
            };
            if let crate::effect::Effect::Update { from, .. } = &mut effect {
                if let Some(state) = historical.get(&from.id) {
                    for (key, value) in &mut from.attributes {
                        let Some(shape) = state.attributes.get(key) else {
                            continue;
                        };
                        *value = redact_using_redacted_shape(value, shape)?;
                    }
                } else if let Some(guide) = planned_resources.get(&from.id) {
                    **from = if reusable_resource(&from.id).is_some() {
                        let attributes = redact_attrs_with_guide(
                            from.attributes.iter(),
                            |key, value| match guide.attributes.get(key) {
                                Some(shape) => redact_using_redacted_shape(value, shape),
                                None => redact_secrets_only(value),
                            },
                        )?;
                        crate::resource::State {
                            attributes,
                            ..from.as_ref().clone()
                        }
                    } else {
                        redact_secrets_in_state_using_resource(from, guide)?
                    };
                }
            }
            Ok(effect)
        })
        .collect::<Result<_, _>>()?;
    *redacted.effects_mut() = redacted_effects;

    let create_ids = create_effect_ids(&redacted);
    for metadata in redacted.replace_display_mut() {
        let resource_id = create_ids.get(metadata.create_idx).and_then(Option::as_ref);
        let historical = resource_id.and_then(|id| historical.get(id));
        let guide = resource_id.and_then(|id| planned_resources.get(id));
        for (key, value) in &mut metadata.previous_attributes {
            *value = if let Some(shape) = historical.and_then(|state| state.attributes.get(key)) {
                redact_using_redacted_shape(value, shape)?
            } else {
                match guide.and_then(|resource| resource.attributes.get(key)) {
                    Some(shape) if contains_redacted_secret(shape) => {
                        redact_using_redacted_shape(value, shape)?
                    }
                    Some(shape) => redact_plaintext_using_secret_shape(value, shape)?,
                    None => redact_secrets_in_value(value)?,
                }
            };
        }
    }

    Ok(redacted)
}

/// Rehydrate desired-side placeholders in plan effects from freshly evaluated
/// resources/data sources. Historical state is restored separately from the
/// single verified provider-read pass by
/// [`rehydrate_plan_historical_secrets_from_verified_states`].
pub fn rehydrate_plan_desired_secrets<S: Clone>(
    plan: &crate::plan::Plan<S>,
    resources: &HashMap<crate::resource::ResourceId, crate::resource::Resource>,
    data_sources: &HashMap<crate::resource::ResourceId, crate::resource::DataSource>,
    import_identifiers: &HashMap<crate::resource::ResourceId, Value>,
    deferred_templates: &crate::parser::ResolvedDeferredForExpressions,
) -> Result<crate::plan::Plan<S>, SecretRehydrationSiteError> {
    use crate::effect::{DeferredCreatePayload, DeferredReplacePayload, Effect};

    let mut out = plan.clone();
    let effects = plan
        .effects()
        .iter()
        .map(|effect| match effect {
            Effect::Read { resource } => {
                if !resource.attributes.values().any(contains_redacted_secret) {
                    return Ok(effect.clone());
                }
                let fresh = data_sources.get(&resource.id).ok_or_else(|| {
                    SecretRehydrationSiteError::new(
                        resource.id.to_string(),
                        SecretRehydrationError::ShapeMismatch,
                    )
                })?;
                Ok(Effect::Read {
                    resource: resource
                        .replace_inner(rehydrate_data_source_secrets(resource, fresh)?),
                })
            }
            Effect::Create(resource) => {
                if !resource.attributes.values().any(contains_redacted_secret) {
                    return Ok(effect.clone());
                }
                let fresh = resources.get(&resource.id).ok_or_else(|| {
                    SecretRehydrationSiteError::new(
                        resource.id.to_string(),
                        SecretRehydrationError::ShapeMismatch,
                    )
                })?;
                Ok(Effect::Create(resource.replace_inner(
                    rehydrate_resource_secrets(resource, fresh)?,
                )))
            }
            Effect::Update {
                from,
                identifier,
                to,
                changed_attributes,
            } => {
                if !to.attributes.values().any(contains_redacted_secret) {
                    return Ok(effect.clone());
                }
                let fresh = resources.get(&to.id).ok_or_else(|| {
                    SecretRehydrationSiteError::new(
                        to.id.to_string(),
                        SecretRehydrationError::ShapeMismatch,
                    )
                })?;
                Ok(Effect::Update {
                    from: from.clone(),
                    identifier: identifier.clone(),
                    to: to.replace_inner(rehydrate_resource_secrets(to, fresh)?),
                    changed_attributes: changed_attributes.clone(),
                })
            }
            Effect::Import { id, identifier } if contains_redacted_secret(identifier) => {
                Ok(Effect::Import {
                    id: id.clone(),
                    identifier: rehydrate_secret_entry(
                        identifier,
                        import_identifiers.get(id.as_inner()),
                        format!("import {id}"),
                        SecretRehydrationCandidate::Evaluated,
                    )?
                    .expect("guarded by contains_redacted_secret"),
                })
            }
            Effect::DeferredCreate(payload) => {
                let template = rehydrate_deferred_template(&payload.template, deferred_templates)?;
                Ok(Effect::DeferredCreate(DeferredCreatePayload::new(
                    Box::new(template),
                    payload.dependency_template.clone(),
                )))
            }
            Effect::DeferredReplace(payload) => {
                let template = rehydrate_deferred_template(&payload.template, deferred_templates)?;
                Ok(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
                    deletes: payload.deletes.clone(),
                    dependency_template: payload.dependency_template.clone(),
                    template: Box::new(template),
                })))
            }
            Effect::ResumeCreate { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. } => Ok(effect.clone()),
        })
        .collect::<Result<Vec<_>, SecretRehydrationSiteError>>()?;
    *out.effects_mut() = effects;
    Ok(out)
}

fn rehydrate_deferred_template<S: Clone, T>(
    saved: &crate::parser::CompiledDeferredForExpression<S>,
    candidates: &[crate::parser::DeferredForExpression<T>],
) -> Result<crate::parser::CompiledDeferredForExpression<S>, SecretRehydrationSiteError> {
    let has_placeholder = deferred_template_contains_redacted_secret(saved);
    if !has_placeholder {
        return Ok(saved.clone());
    }

    let mut out = saved.clone();

    let candidate = candidates
        .iter()
        .find(|candidate| {
            candidate.target().same_system_target(saved.target())
                && candidate.resource_type() == saved.resource_type()
                && candidate.root_bindings() == saved.root_bindings()
        })
        .ok_or_else(|| {
            SecretRehydrationSiteError::new(
                format!("deferred for {}", saved.target().display_name()),
                SecretRehydrationError::ShapeMismatch,
            )
        })?;

    if let Some(value) = rehydrate_secret_entry(
        &saved.iterable,
        Some(&candidate.iterable),
        format!("deferred iterable {}", saved.target().display_name()),
        SecretRehydrationCandidate::Evaluated,
    )? {
        out.iterable = value;
    }

    for (key, saved_value) in out.template_attributes_mut() {
        if let Some(value) = rehydrate_secret_entry(
            saved_value,
            candidate.attributes().get(key),
            format!("deferred resource {}.{key}", saved.target().display_name()),
            SecretRehydrationCandidate::Evaluated,
        )? {
            *saved_value = value;
        }
    }
    Ok(out)
}

/// Restore plan-history placeholders from states whose hashes were already
/// verified against fresh provider reads.
///
/// The redacted and verified maps are a paired witness: the former proves that
/// each plan placeholder has the same hash as the corresponding saved-state
/// placeholder, while the latter supplies the plaintext shape produced by the
/// single Argon2 verification pass. This deliberately performs no hashing.
pub fn rehydrate_plan_historical_secrets_from_verified_states<S: Clone>(
    plan: &crate::plan::Plan<S>,
    redacted_states: &HashMap<crate::resource::ResourceId, crate::resource::State>,
    verified_states: &HashMap<crate::resource::ResourceId, crate::resource::State>,
) -> Result<crate::plan::Plan<S>, SecretRehydrationSiteError> {
    use crate::effect::Effect;

    fn restore_verified_value(
        saved: &Value,
        redacted_guide: &Value,
        verified: &Value,
    ) -> Result<Value, SecretRehydrationError> {
        if !contains_redacted_secret(saved) {
            return Ok(saved.clone());
        }
        match (saved, redacted_guide, verified) {
            (
                Value::Deferred(DeferredValue::RedactedSecret { hash: saved_hash }),
                Value::Deferred(DeferredValue::RedactedSecret { hash: guide_hash }),
                verified,
            ) => {
                if saved_hash != guide_hash {
                    return Err(SecretRehydrationError::HashMismatch);
                }
                if let Value::Deferred(DeferredValue::RedactedSecret {
                    hash: verified_hash,
                }) = verified
                {
                    if verified_hash != guide_hash {
                        return Err(SecretRehydrationError::HashMismatch);
                    }
                    // A provider-omitted write-only value is verified by its
                    // exact persisted placeholder. Keep it in Update.from;
                    // the desired patch is rehydrated independently and must
                    // still carry provider plaintext.
                    return Ok(verified.clone());
                }
                if contains_redacted_secret(verified) {
                    return Err(SecretRehydrationError::ShapeMismatch);
                }
                Ok(verified.clone())
            }
            (
                Value::Concrete(ConcreteValue::Map(saved)),
                Value::Concrete(ConcreteValue::Map(redacted_guide)),
                Value::Concrete(ConcreteValue::Map(verified)),
            ) => Ok(Value::Concrete(ConcreteValue::Map(
                saved
                    .iter()
                    .map(|(key, value)| {
                        if !contains_redacted_secret(value) {
                            return Ok((key.clone(), value.clone()));
                        }
                        let guide = redacted_guide
                            .get(key)
                            .ok_or(SecretRehydrationError::ShapeMismatch)?;
                        let verified = verified
                            .get(key)
                            .ok_or(SecretRehydrationError::ShapeMismatch)?;
                        restore_verified_value(value, guide, verified)
                            .map(|value| (key.clone(), value))
                    })
                    .collect::<Result<_, _>>()?,
            ))),
            (
                Value::Concrete(ConcreteValue::List(saved)),
                Value::Concrete(ConcreteValue::List(redacted_guide)),
                Value::Concrete(ConcreteValue::List(verified)),
            ) if saved.len() == redacted_guide.len() && saved.len() == verified.len() => {
                Ok(Value::Concrete(ConcreteValue::List(
                    saved
                        .iter()
                        .zip(redacted_guide)
                        .zip(verified)
                        .map(|((value, guide), verified)| {
                            restore_verified_value(value, guide, verified)
                        })
                        .collect::<Result<_, _>>()?,
                )))
            }
            _ => Err(SecretRehydrationError::ShapeMismatch),
        }
    }

    fn restore_state(
        saved: &crate::resource::State,
        redacted_guide: &crate::resource::State,
        verified: &crate::resource::State,
    ) -> Result<crate::resource::State, SecretRehydrationSiteError> {
        let mut out = saved.clone();
        for (key, value) in &mut out.attributes {
            if !contains_redacted_secret(value) {
                continue;
            }
            let guide = redacted_guide.attributes.get(key).ok_or_else(|| {
                SecretRehydrationSiteError::new(
                    format!("{}.{}", saved.id, key),
                    SecretRehydrationError::ShapeMismatch,
                )
            })?;
            let verified = verified.attributes.get(key).ok_or_else(|| {
                SecretRehydrationSiteError::new(
                    format!("{}.{}", saved.id, key),
                    SecretRehydrationError::ShapeMismatch,
                )
            })?;
            *value = restore_verified_value(value, guide, verified).map_err(|source| {
                SecretRehydrationSiteError::new(format!("{}.{}", saved.id, key), source)
            })?;
        }
        Ok(out)
    }

    let state_evidence = |id: &crate::resource::ResourceId| {
        redacted_states
            .get(id)
            .zip(verified_states.get(id))
            .ok_or_else(|| {
                SecretRehydrationSiteError::new(
                    id.to_string(),
                    SecretRehydrationError::ShapeMismatch,
                )
            })
    };

    let mut out = plan.clone();
    let effects = plan
        .effects()
        .iter()
        .map(|effect| match effect {
            Effect::Update {
                from,
                identifier,
                to,
                changed_attributes,
            } if from.attributes.values().any(contains_redacted_secret) => {
                let (redacted, verified) = state_evidence(&from.id)?;
                Ok(Effect::Update {
                    from: Box::new(restore_state(from, redacted, verified)?),
                    identifier: identifier.clone(),
                    to: to.clone(),
                    changed_attributes: changed_attributes.clone(),
                })
            }
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate(_)
            | Effect::DeferredReplace(_) => Ok(effect.clone()),
        })
        .collect::<Result<Vec<_>, SecretRehydrationSiteError>>()?;
    *out.effects_mut() = effects;

    let create_ids = create_effect_ids(&out);
    for metadata in out.replace_display_mut() {
        if !metadata
            .previous_attributes
            .values()
            .any(contains_redacted_secret)
        {
            continue;
        }
        let resource_id = create_ids
            .get(metadata.create_idx)
            .and_then(Option::as_ref)
            .ok_or_else(|| {
                SecretRehydrationSiteError::new(
                    "plan.replace_display",
                    SecretRehydrationError::ShapeMismatch,
                )
            })?;
        let (redacted, verified) = state_evidence(resource_id)?;
        for (key, value) in &mut metadata.previous_attributes {
            if !contains_redacted_secret(value) {
                continue;
            }
            let guide = redacted.attributes.get(key).ok_or_else(|| {
                SecretRehydrationSiteError::new(
                    format!("{}.{}", resource_id, key),
                    SecretRehydrationError::ShapeMismatch,
                )
            })?;
            let verified = verified.attributes.get(key).ok_or_else(|| {
                SecretRehydrationSiteError::new(
                    format!("{}.{}", resource_id, key),
                    SecretRehydrationError::ShapeMismatch,
                )
            })?;
            *value = restore_verified_value(value, guide, verified).map_err(|source| {
                SecretRehydrationSiteError::new(format!("{}.{}", resource_id, key), source)
            })?;
        }
    }
    Ok(out)
}

/// Maximum line width before list-of-string and Map values expand vertically
/// in [`render_value_pretty`]. Fixed (not terminal-derived) so snapshot tests are
/// deterministic and CI/PR-comment readers see identical output.
pub(crate) const PRETTY_LINE_LIMIT: usize = 80;

/// Layout context for [`render_value_pretty`].
///
/// Stores the already-rendered key width rather than accepting an arbitrary
/// key string. Callers must choose between a schema attribute and a typed
/// [`RenderedKey`], so user map keys cannot be encoded a second time.
#[derive(Debug, Clone, Copy)]
pub struct PrettyLayout {
    /// Column at which the parent attribute's key is rendered (i.e. how
    /// many leading spaces precede `<key>:` on its line).
    parent_indent_cols: usize,
    key_cols: usize,
}

impl PrettyLayout {
    pub fn for_attribute(parent_indent_cols: usize, key: &str) -> Self {
        Self {
            parent_indent_cols,
            key_cols: key.chars().count(),
        }
    }

    pub fn for_map_key(parent_indent_cols: usize, key: &RenderedKey) -> Self {
        Self {
            parent_indent_cols,
            key_cols: key.plain().chars().count(),
        }
    }

    fn for_raw_map_key(parent_indent_cols: usize, key: &str) -> Self {
        Self::for_map_key(parent_indent_cols, &RenderedKey::from_raw(key))
    }

    /// Width of the prefix (`<indent>key: `) that the caller has already
    /// emitted, in columns. Used as the budget consumed before any value
    /// content can fit on the same line. Map keys can carry non-ASCII
    /// characters (the `Value::Concrete(ConcreteValue::Map)` key type is `String` with no
    /// encoding constraint), so width is measured in Unicode scalar values.
    fn prefix_cols(&self) -> usize {
        self.parent_indent_cols + self.key_cols + 2
    }

    /// Column for child entries / keys when expanding vertically.
    fn child_indent_cols(&self) -> usize {
        self.parent_indent_cols + 2
    }
}

/// Format a `Value` for human-readable, multi-line plan output.
///
/// The return value is either a bare inline string, or begins with `\n`
/// followed by lines indented at `layout.parent_indent_cols + 2`. Either
/// way, the caller can append it verbatim after `<indent><key>: ` and the
/// indentation will line up correctly.
///
/// Behavior:
/// - Scalar variants are identical to [`format_value`].
/// - `Value::Concrete(ConcreteValue::List)` of all `Value::Concrete(ConcreteValue::Map)` always renders vertically. Each
///   element's first key is prefixed with `* ` at `parent_indent_cols + 2`;
///   continuation keys align at `parent_indent_cols + 4`. Map keys are
///   sorted alphabetically. Consecutive elements are also separated by
///   a blank line. The marker is `*` rather than `-` because `-`
///   collides with the destroy action marker at the resource-row level
///   (#2545 dropped the marker, #2552 brought it back as `*`).
/// - `Value::Concrete(ConcreteValue::List)` of scalars renders inline `[a, b, c]` if the entire line
///   (`<indent>key: <inline>`) fits within `PRETTY_LINE_LIMIT`; otherwise
///   expands to a bracketed multi-line form.
/// - `Value::Concrete(ConcreteValue::StringList)` (the canonicalized `Union[String, list(String)]`
///   form, #2511) follows the same inline-vs-vertical rule as
///   `Value::Concrete(ConcreteValue::List)` of `Value::Concrete(ConcreteValue::String)` (#2528).
/// - `Value::Concrete(ConcreteValue::Map)` renders inline if it fits, otherwise expands vertically
///   with each key at `parent_indent_cols + 2`.
///
/// When adding a new layout-bearing `Value` variant, add a new arm here
/// — the wildcard fallthrough collapses to the inline form and skips
/// the line-budget check, which is wrong for any container variant.
/// Render a human-readable value with semantic span metadata.
pub(crate) fn render_value_pretty(value: &Value, layout: PrettyLayout) -> RenderedValue {
    let mut sink = SpanSink::default();
    render_value_pretty_into(value, layout, &mut sink).expect("SpanSink writes are infallible");
    sink.rendered
}

fn render_value_pretty_into<S: FormatSink>(
    value: &Value,
    layout: PrettyLayout,
    sink: &mut S,
) -> Result<(), Overflow> {
    match value {
        Value::Concrete(ConcreteValue::List(items)) => {
            if items.is_empty() {
                return sink.write_token(ValueSpanKind::Structure, "[]");
            }
            if is_list_of_maps(value) {
                return format_list_of_maps_vertical(items, layout.child_indent_cols(), sink);
            }
            // #2434: measure first, build only when we know it fits.
            // Pre-fix this built the inline form unconditionally
            // and discarded the result on overflow — quadratic on deep
            // nested values because `format_*_vertical` recurses back
            // into `render_value_pretty`, which built-and-discarded again
            // at every level.
            let budget = PRETTY_LINE_LIMIT.saturating_sub(layout.prefix_cols());
            if inline_width(value, budget).is_some() {
                return format_value_into(value, sink);
            }
            format_list_of_scalars_vertical(items, layout.child_indent_cols(), sink)
        }
        Value::Concrete(ConcreteValue::StringList(items)) => {
            // #2528: `Value::Concrete(ConcreteValue::StringList)` is the canonicalized form the
            // `Union[String, list(String)]` shape collapses to (#2511).
            // It behaves like `Value::Concrete(ConcreteValue::List)` of `Value::Concrete(ConcreteValue::String)` for
            // layout purposes — apply the same inline-vs-vertical
            // decision so a long list under a dynamic-key Map (e.g. an
            // IAM `condition.string_like.<context-key>: [a, b]`) breaks
            // across lines instead of dumping inline. Lift items to
            // `Value::Concrete(ConcreteValue::String)` for the vertical fallback so the per-item
            // rendering (including DSL-enum resolution)
            // stays byte-identical to [`format_value`]'s arm.
            if items.is_empty() {
                return sink.write_token(ValueSpanKind::Structure, "[]");
            }
            let budget = PRETTY_LINE_LIMIT.saturating_sub(layout.prefix_cols());
            if inline_width(value, budget).is_some() {
                return format_value_into(value, sink);
            }
            let lifted: Vec<Value> = items
                .iter()
                .cloned()
                .map(|s| Value::Concrete(ConcreteValue::String(s)))
                .collect();
            format_list_of_scalars_vertical(&lifted, layout.child_indent_cols(), sink)
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            if map.is_empty() {
                return sink.write_token(ValueSpanKind::Structure, "{}");
            }
            let budget = PRETTY_LINE_LIMIT.saturating_sub(layout.prefix_cols());
            if inline_width(value, budget).is_some() {
                return format_value_into(value, sink);
            }
            format_map_vertical(map, layout.child_indent_cols(), sink)
        }
        _ => format_value_into(value, sink),
    }
}

/// Compute the byte length of [`format_value`] without
/// allocating the rendered string, short-circuiting to `None` as soon as
/// the running total exceeds `budget` (#2434).
///
/// Implemented as a thin wrapper around [`format_value_into`] with a
/// [`WidthCounter`] sink, so the byte count is byte-for-byte identical
/// to what [`format_value`] would emit by construction —
/// adding a new `Value` variant only needs one site update (the
/// `format_value_into` arm), not a measure/build pair that could rot
/// out of sync.
///
/// [`render_value_pretty`] is the only intended caller; `pub(crate)`
/// lets the value tests pin the boundary behaviour.
pub(crate) fn inline_width(value: &Value, budget: usize) -> Option<usize> {
    let mut counter = WidthCounter::new(budget);
    format_value_into(value, &mut counter)
        .ok()
        .map(|()| counter.width())
}

/// Render a list-of-maps vertically. The first key of each element is
/// prefixed with a `* ` marker at `entry_indent_cols`; remaining keys
/// align under it at `entry_indent_cols + 2`. Consecutive elements are
/// separated by a blank line. The marker uses `*` rather than the
/// YAML-style `-` because `-` collides with the destroy action marker
/// at the resource-row level (#2545 dropped the marker; #2552 brought
/// it back as `*`). Callers that iterate sibling keys (`format_map_vertical`
/// here, `DetailRow::MapExpanded` in `carina-cli/src/display/mod.rs` and
/// `carina-tui/src/ui/detail.rs`) consult `needs_trailing_separator` to
/// insert a blank line before any sibling key that follows a multi-element
/// list-of-maps so the boundary stays visible (#2555); this function only
/// handles the inter-element separators inside the list itself.
fn format_list_of_maps_vertical<S: FormatSink>(
    items: &[Value],
    entry_indent_cols: usize,
    sink: &mut S,
) -> Result<(), Overflow> {
    let entry_indent = " ".repeat(entry_indent_cols);
    let continuation_indent = " ".repeat(entry_indent_cols + 2);
    let mut first_element = true;
    for item in items {
        if let Value::Concrete(ConcreteValue::Map(map)) = item {
            if !first_element {
                sink.write_token(ValueSpanKind::Whitespace, "\n")?;
            }
            first_element = false;
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for (i, k) in keys.iter().enumerate() {
                let child_layout = PrettyLayout::for_raw_map_key(entry_indent_cols + 2, k);
                sink.write_token(ValueSpanKind::Whitespace, "\n")?;
                if i == 0 {
                    sink.write_token(ValueSpanKind::Whitespace, &entry_indent)?;
                    sink.write_token(ValueSpanKind::Structure, "* ")?;
                } else {
                    sink.write_token(ValueSpanKind::Whitespace, &continuation_indent)?;
                }
                write_map_key(k, sink)?;
                sink.write_token(ValueSpanKind::Structure, ": ")?;
                render_value_pretty_into(&map[*k], child_layout, sink)?;
            }
        }
    }
    Ok(())
}

/// Render a list-of-scalars vertically inside a bracketed block. Items
/// indent at `item_indent_cols`; closing `]` aligns with the parent line
/// (one level shallower).
fn format_list_of_scalars_vertical<S: FormatSink>(
    items: &[Value],
    item_indent_cols: usize,
    sink: &mut S,
) -> Result<(), Overflow> {
    // Invariant: `item_indent_cols` is always `child_indent_cols()` of some
    // `PrettyLayout`, which is `parent_indent_cols + 2`, so >= 2.
    debug_assert!(item_indent_cols >= 2);
    let item_indent = " ".repeat(item_indent_cols);
    let close_indent = " ".repeat(item_indent_cols - 2);
    sink.write_token(ValueSpanKind::Structure, "[")?;
    sink.write_token(ValueSpanKind::Whitespace, "\n")?;
    for item in items {
        sink.write_token(ValueSpanKind::Whitespace, &item_indent)?;
        format_value_into(item, sink)?;
        sink.write_token(ValueSpanKind::Structure, ",")?;
        sink.write_token(ValueSpanKind::Whitespace, "\n")?;
    }
    sink.write_token(ValueSpanKind::Whitespace, &close_indent)?;
    sink.write_token(ValueSpanKind::Structure, "]")
}

/// Render a map vertically. Each key is at `key_indent_cols` and its value
/// recurses with that key as the new parent key. A blank line is injected
/// before any key that follows a multi-element list-of-maps so the list
/// boundary stays visible — the `*` per-element marker disambiguates
/// element starts but not element *ends* (#2555). The blank is only
/// inserted when a sibling actually follows, avoiding orphan whitespace
/// at the end of a block.
fn format_map_vertical<S: FormatSink>(
    map: &IndexMap<String, Value>,
    key_indent_cols: usize,
    sink: &mut S,
) -> Result<(), Overflow> {
    let mut keys: Vec<_> = map.keys().collect();
    keys.sort();
    let key_indent = " ".repeat(key_indent_cols);
    let mut prev_needs_separator = false;
    for k in keys {
        let child_layout = PrettyLayout::for_raw_map_key(key_indent_cols, k);
        if prev_needs_separator {
            sink.write_token(ValueSpanKind::Whitespace, "\n")?;
        }
        prev_needs_separator = needs_trailing_separator(&map[k]);
        sink.write_token(ValueSpanKind::Whitespace, "\n")?;
        sink.write_token(ValueSpanKind::Whitespace, &key_indent)?;
        write_map_key(k, sink)?;
        sink.write_token(ValueSpanKind::Structure, ": ")?;
        render_value_pretty_into(&map[k], child_layout, sink)?;
    }
    Ok(())
}

/// Check if a value is a list of maps (list-of-struct)
pub fn is_list_of_maps(value: &Value) -> bool {
    if let Value::Concrete(ConcreteValue::List(items)) = value {
        !items.is_empty()
            && items
                .iter()
                .all(|item| matches!(item, Value::Concrete(ConcreteValue::Map(_))))
    } else {
        false
    }
}

/// Extract a `Vec<String>` from a `Value::Concrete(ConcreteValue::StringList)` or a `Value::Concrete(ConcreteValue::List)`
/// whose every element is a string-bearing scalar — `String` **or**
/// `EnumIdentifier`. Returns `None` for other shapes. Empty lists
/// return `Some(vec![])` so callers can distinguish "empty string
/// list" from "not a string list" — needed by #2943's diff path so a
/// list shrinking to empty still routes through per-element `-` lines
/// instead of the inline `[a, b] → []` form.
///
/// `EnumIdentifier` and `CanonicalEnum` are accepted so `List<Enum>` can use
/// the per-element display path. This function only projects text; callers
/// that apply schema semantics must still distinguish the original variants.
pub fn as_string_list(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Concrete(ConcreteValue::StringList(items)) => Some(
            items
                .iter()
                .map(|item| item.expose_plaintext().to_owned())
                .collect(),
        ),
        Value::Concrete(ConcreteValue::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::Concrete(ConcreteValue::String(s)) => Some(s.expose_plaintext().to_owned()),
                Value::Concrete(ConcreteValue::EnumIdentifier(s)) => Some(s.to_string()),
                Value::Concrete(ConcreteValue::CanonicalEnum(c)) => Some(c.api_value().to_string()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// Whether `value` renders to a vertical block whose final line sits at
/// the element's key column, leaving a sibling key visually attached
/// to the last element. A blank line should follow such a value before
/// the next sibling key. Currently only multi-element list-of-maps
/// matches — a lone element is visually no more ambiguous than a
/// regular map under the parent key, and skipping the blank for it
/// avoids noise around common single-statement policies. (#2555)
pub(crate) fn needs_trailing_separator(value: &Value) -> bool {
    is_list_of_maps(value)
        && matches!(value, Value::Concrete(ConcreteValue::List(items)) if items.len() >= 2)
}

/// Returns true when `attr_type` is exactly the IAM-style
/// `string_or_list_of_strings` shape — `Union(vec![String, list(String)])`
/// in either order — peeling top-level Union-member Refs through `defs`.
fn is_string_or_list_of_strings(
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> bool {
    let Some(members) = crate::schema::union_members_with_defs(attr_type, defs) else {
        return false;
    };
    if members.len() != 2 {
        return false;
    }
    let mut has_string = false;
    let mut has_list_of_string = false;
    for member in members.iter() {
        let Some(member) = member else {
            return false;
        };
        match &member.as_attr().kind {
            AttrTypeKind::UnloadedSchemaType(_) => return false,
            AttrTypeKind::String { .. } => has_string = true,
            AttrTypeKind::List {
                element_type: inner,
                ..
            } if matches!(&inner.kind, AttrTypeKind::String { .. }) => {
                has_list_of_string = true;
            }
            _ => return false,
        }
    }
    has_string && has_list_of_string
}

/// Where a strict desired value came from. Both variants must be fully
/// canonical before the value can cross a host-to-provider boundary, but
/// parser-authored identifiers and provider-returned text have different
/// alias precedence.
#[derive(Clone, Copy)]
pub(crate) enum DesiredValueSource {
    AuthoredDsl,
    ProviderOutput,
}

/// Result of canonicalizing a provider or persisted-state value.
///
/// `Absent` is distinct from an empty value so every state container walker
/// must decide whether to remove its key. Desired-value canonicalization never
/// produces `Absent`; authored empty objects remain available to providers.
#[derive(Debug, Clone, PartialEq)]
pub enum CanonicalState<H: PlaceholderKind = crate::resource::NoPlaceholder> {
    Present(Value<H>),
    Absent,
}

impl<H: PlaceholderKind> CanonicalState<H> {
    pub fn into_present(self) -> Option<Value<H>> {
        match self {
            Self::Present(value) => Some(value),
            Self::Absent => None,
        }
    }
}

/// Internal recursion policy. Public and crate-visible callers enter through
/// purpose-named functions below rather than selecting strictness flags.
#[derive(Clone, Copy)]
pub(crate) enum CanonicalizationPurpose<'a> {
    Desired(DesiredValueSource),
    ProviderConfig { provider_name: &'a str },
    State,
    WaitRhs,
}

impl<'a> CanonicalizationPurpose<'a> {
    pub(crate) fn canonicalizes_full_shape(self) -> bool {
        matches!(
            self,
            Self::Desired(_) | Self::ProviderConfig { .. } | Self::State
        )
    }

    pub(crate) fn resolves_identifiers_as_state_text(self) -> bool {
        matches!(
            self,
            Self::Desired(DesiredValueSource::ProviderOutput) | Self::State
        )
    }

    pub(crate) fn provider_config_name(self) -> Option<&'a str> {
        match self {
            Self::ProviderConfig { provider_name } => Some(provider_name),
            Self::Desired(_) | Self::State | Self::WaitRhs => None,
        }
    }

    pub(crate) fn accepts_enum_string(self, enumerated: bool) -> bool {
        match self {
            Self::Desired(DesiredValueSource::AuthoredDsl) | Self::WaitRhs => !enumerated,
            Self::Desired(DesiredValueSource::ProviderOutput)
            | Self::ProviderConfig { .. }
            | Self::State => true,
        }
    }

    pub(crate) fn validates_struct_assignments(self) -> bool {
        matches!(
            self,
            Self::Desired(DesiredValueSource::AuthoredDsl)
                | Self::ProviderConfig { .. }
                | Self::WaitRhs
        )
    }

    fn rejects_unrecognized_enum_text(self) -> bool {
        matches!(self, Self::Desired(DesiredValueSource::ProviderOutput))
    }
}

trait CanonicalizationErrorPolicy: Copy {
    type Error;

    fn canonicalization_failed<H: PlaceholderKind, B: BindingAddress>(
        self,
        original: Value<H, B>,
        source: crate::schema::TypeError,
    ) -> Result<Value<H, B>, Self::Error>;

    fn mask_secret(self, error: Self::Error) -> Self::Error;

    fn at_list_index(self, index: usize, error: Self::Error) -> Self::Error;

    fn at_map_key(self, key: String, error: Self::Error) -> Self::Error;

    fn at_map_value(self, key: String, error: Self::Error) -> Self::Error;

    fn at_struct_field(self, field: String, error: Self::Error) -> Self::Error;

    fn map_key_collision(self, key: String, canonical_key: String) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy)]
struct RejectEnumResolutionErrors;

impl CanonicalizationErrorPolicy for RejectEnumResolutionErrors {
    type Error = crate::schema::TypeError;

    fn canonicalization_failed<H: PlaceholderKind, B: BindingAddress>(
        self,
        _original: Value<H, B>,
        source: crate::schema::TypeError,
    ) -> Result<Value<H, B>, Self::Error> {
        Err(source)
    }

    fn mask_secret(self, error: Self::Error) -> Self::Error {
        error.masked_for_secret()
    }

    fn at_list_index(self, index: usize, error: Self::Error) -> Self::Error {
        crate::schema::TypeError::ListItemError {
            index,
            inner: Box::new(error),
        }
    }

    fn at_map_key(self, key: String, error: Self::Error) -> Self::Error {
        crate::schema::TypeError::MapKeyError {
            key,
            inner: Box::new(error),
        }
    }

    fn at_map_value(self, key: String, error: Self::Error) -> Self::Error {
        crate::schema::TypeError::MapValueError {
            key,
            inner: Box::new(error),
        }
    }

    fn at_struct_field(self, field: String, error: Self::Error) -> Self::Error {
        crate::schema::TypeError::StructFieldError {
            field,
            inner: Box::new(error),
        }
    }

    fn map_key_collision(self, key: String, canonical_key: String) -> Result<(), Self::Error> {
        Err(self.at_map_key(
            key,
            crate::schema::TypeError::ValidationFailed {
                message: format!(
                    "canonical map key `{canonical_key}` conflicts with an earlier map key"
                ),
            },
        ))
    }
}

#[derive(Clone, Copy)]
struct PreserveEnumResolutionErrors;

impl CanonicalizationErrorPolicy for PreserveEnumResolutionErrors {
    type Error = Infallible;

    fn canonicalization_failed<H: PlaceholderKind, B: BindingAddress>(
        self,
        original: Value<H, B>,
        _source: crate::schema::TypeError,
    ) -> Result<Value<H, B>, Self::Error> {
        Ok(original)
    }

    fn mask_secret(self, error: Self::Error) -> Self::Error {
        match error {}
    }

    fn at_list_index(self, _index: usize, error: Self::Error) -> Self::Error {
        match error {}
    }

    fn at_map_key(self, _key: String, error: Self::Error) -> Self::Error {
        match error {}
    }

    fn at_map_value(self, _key: String, error: Self::Error) -> Self::Error {
        match error {}
    }

    fn at_struct_field(self, _field: String, error: Self::Error) -> Self::Error {
        match error {}
    }

    fn map_key_collision(self, _key: String, _canonical_key: String) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn unwrap_infallible<T>(result: Result<T, Infallible>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => match error {},
    }
}

/// Canonicalize a desired value completely. Success guarantees that no
/// [`ConcreteValue::EnumIdentifier`] remains anywhere in the value tree.
#[cfg(test)]
fn canonicalize_desired_with_type<H: PlaceholderKind, B: BindingAddress>(
    value: Value<H, B>,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    source: DesiredValueSource,
) -> Result<Value<H, B>, crate::schema::TypeError> {
    canonicalize_desired_with_type_and_ancestry(
        value,
        attr_type,
        defs,
        crate::schema::FieldAncestry::WRITABLE,
        source,
    )
}

fn canonicalize_desired_with_type_and_ancestry<H: PlaceholderKind, B: BindingAddress>(
    value: Value<H, B>,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    ancestry: crate::schema::FieldAncestry,
    source: DesiredValueSource,
) -> Result<Value<H, B>, crate::schema::TypeError> {
    let canonical = canonicalize_with_type_for_purpose(
        value,
        attr_type,
        defs,
        ancestry,
        CanonicalizationPurpose::Desired(source),
        RejectEnumResolutionErrors,
    )?;
    if let Some(error) = unresolved_enum_identifier_type_error_in(&canonical) {
        return Err(error);
    }
    Ok(canonical)
}

/// Canonicalize one authored desired attribute through its resource schema.
///
/// Identity and reconciliation seams use this before reducing a value to a
/// comparison string, so parser-surface enum identifiers and already
/// canonical enum values share the same API-value representation.
pub(crate) fn canonicalize_desired_attribute_value_with_schema(
    mut value: Value,
    schema: &crate::schema::CompiledResourceSchema,
    attribute: &str,
) -> Result<Value, crate::schema::TypeError> {
    // Already-canonical enums may carry an equivalent historical provider
    // namespace. Their API spelling is the comparison identity at this seam,
    // so only parser-surface EnumIdentifier values need schema resolution.
    if first_enum_identifier(&value).is_none() {
        return Ok(value);
    }
    reparse_canonical_enum_leaves(&mut value);
    let Some(attribute_schema) = schema.attributes.get(attribute) else {
        return Ok(value);
    };
    canonicalize_desired_with_type_and_ancestry(
        value,
        &attribute_schema.attr_type,
        &schema.defs,
        crate::schema::FieldAncestry::from_input_mode(attribute_schema.input_mode()),
        DesiredValueSource::AuthoredDsl,
    )
}

/// Re-enter already-canonical enum leaves through their API spelling when a
/// container also contains parser-surface enum identifiers. This keeps the
/// identity/reconciliation seam's documented API-value identity rule stable
/// for mixed List/Map values, including historical provider namespaces.
fn reparse_canonical_enum_leaves(value: &mut Value) {
    match value {
        Value::Concrete(ConcreteValue::CanonicalEnum(canonical)) => {
            *value = Value::Concrete(ConcreteValue::enum_identifier(
                canonical.api_value().to_string(),
            ));
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items {
                reparse_canonical_enum_leaves(item);
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            for item in map.values_mut() {
                reparse_canonical_enum_leaves(item);
            }
        }
        // Validated arithmetic operands are Int-shaped and cannot contain an
        // enum leaf. Keep the arm explicit so this traversal cannot silently
        // become stale if the deferred enum grows another variant.
        Value::Deferred(DeferredValue::IntegerArithmetic(_)) => {}
        _ => {}
    }
}

/// Canonicalize one provider or persisted-state attribute through its resource
/// schema while preserving the attribute's effective input ancestry.
pub(crate) fn canonicalize_state_attribute_value_with_schema(
    value: Value,
    schema: &crate::schema::CompiledResourceSchema,
    attribute: &str,
) -> CanonicalState {
    let Some(attribute_schema) = schema.attributes.get(attribute) else {
        return CanonicalState::Present(value);
    };
    canonicalize_state_with_type_and_ancestry(
        value,
        &attribute_schema.attr_type,
        &schema.defs,
        crate::schema::FieldAncestry::from_input_mode(attribute_schema.input_mode()),
    )
}

/// Canonicalize provider or persisted state. Unrecognized enum-positioned
/// strings legitimately remain strings for validation and comparison.
#[cfg(test)]
fn canonicalize_state_with_type(
    value: Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Value {
    canonicalize_state_with_type_and_ancestry(
        value,
        attr_type,
        defs,
        crate::schema::FieldAncestry::WRITABLE,
    )
    .into_present()
    .expect("state fixture must remain present")
}

pub(crate) fn canonicalize_state_with_type_and_ancestry(
    value: Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    ancestry: crate::schema::FieldAncestry,
) -> CanonicalState {
    unwrap_infallible(canonicalize_state_entry_with_type(
        value,
        attr_type,
        defs,
        ancestry,
        PreserveEnumResolutionErrors,
    ))
}

/// Resolve typed wait-RHS enum syntax without reclassifying quoted strings or
/// applying unrelated structural rewrites.
pub(crate) fn canonicalize_wait_rhs_with_type<P: PlaceholderKind, B: BindingAddress>(
    value: Value<P, B>,
    effective_type: &crate::schema::EffectiveType<'_>,
) -> Value<P, B> {
    unwrap_infallible(canonicalize_with_type_for_purpose(
        value,
        effective_type.as_type(),
        effective_type.defs(),
        effective_type.ancestry(),
        CanonicalizationPurpose::WaitRhs,
        PreserveEnumResolutionErrors,
    ))
}

fn canonicalize_with_type_for_purpose<
    H: PlaceholderKind,
    B: BindingAddress,
    P: CanonicalizationErrorPolicy,
>(
    value: Value<H, B>,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    ancestry: crate::schema::FieldAncestry,
    purpose: CanonicalizationPurpose<'_>,
    error_policy: P,
) -> Result<Value<H, B>, P::Error> {
    let mut visited_refs = Vec::new();
    let resolved_type = attr_type.resolve_refs_with_defs_on_path(defs, &mut visited_refs);
    canonicalize_resolved_value_for_purpose(
        value,
        resolved_type,
        defs,
        ancestry,
        purpose,
        error_policy,
        &mut visited_refs,
    )
}

fn canonicalize_state_entry_with_type<H: PlaceholderKind, P: CanonicalizationErrorPolicy>(
    value: Value<H>,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    ancestry: crate::schema::FieldAncestry,
    error_policy: P,
) -> Result<CanonicalState<H>, P::Error> {
    let mut visited_refs = Vec::new();
    let resolved_type = attr_type.resolve_refs_with_defs_on_path(defs, &mut visited_refs);
    canonicalize_resolved_value_for_purpose(
        value,
        resolved_type,
        defs,
        ancestry,
        CanonicalizationPurpose::State,
        error_policy,
        &mut visited_refs,
    )
    .map(|value| {
        if crate::schema::reads_as_absent(&value, resolved_type) {
            CanonicalState::Absent
        } else {
            CanonicalState::Present(value)
        }
    })
}

fn canonicalize_resolved_value_for_purpose<
    'a,
    H: PlaceholderKind,
    B: BindingAddress,
    P: CanonicalizationErrorPolicy,
>(
    value: Value<H, B>,
    resolved_type: crate::schema::ResolvedAttrType<'a, crate::schema::Compiled>,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
    ancestry: crate::schema::FieldAncestry,
    purpose: CanonicalizationPurpose<'_>,
    error_policy: P,
    visited_refs: &mut Vec<&'a str>,
) -> Result<Value<H, B>, P::Error> {
    let resolved_attr = resolved_type.as_attr();
    if let Some(fit) = crate::schema::fit_value_to_resolved_type_on_path(
        resolved_attr,
        &value,
        defs,
        ancestry,
        purpose,
        visited_refs,
    ) {
        canonicalize_with_fit(value, &fit, defs, purpose, error_policy)
    } else {
        canonicalize_without_fit(
            value,
            resolved_attr,
            defs,
            ancestry,
            purpose,
            error_policy,
            visited_refs,
        )
    }
}

/// Encode a successful fit through the same canonicalization used by values.
/// Address-bearing or unresolved values cannot form an ordinary JSON key.
pub(crate) fn canonical_json_with_fit<H: PlaceholderKind, B: BindingAddress>(
    value: Value<H, B>,
    fit: &crate::schema::Fit<'_>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    purpose: CanonicalizationPurpose<'_>,
) -> Option<serde_json::Value> {
    let canonical = canonicalize_with_fit(value, fit, defs, purpose, RejectEnumResolutionErrors)
        .ok()?
        .try_map_placeholder::<crate::resource::NoPlaceholder, ()>(&mut |_| Err(()))
        .ok()?
        .try_into_address_free_absolute()?;
    value_to_json_inner(&canonical, None, false, JsonEncoding::Ordinary, false).ok()
}

/// Consume a successful fit witness without re-validating any selected Union
/// member or re-resolving any enum. The value and witness have identical tree
/// shapes by construction.
fn canonicalize_with_fit<H: PlaceholderKind, B: BindingAddress, P: CanonicalizationErrorPolicy>(
    value: Value<H, B>,
    fit: &crate::schema::Fit<'_>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    purpose: CanonicalizationPurpose<'_>,
    error_policy: P,
) -> Result<Value<H, B>, P::Error> {
    if purpose.canonicalizes_full_shape() && is_string_or_list_of_strings(fit.attr.as_attr(), defs)
    {
        return Ok(canonicalize_to_string_list(value));
    }

    match (value, &fit.kind) {
        (Value::Deferred(DeferredValue::Secret(value)), crate::schema::FitKind::Secret(fit)) => {
            canonicalize_with_fit(*value, fit, defs, purpose, error_policy)
                .map(|value| Value::Deferred(DeferredValue::Secret(Box::new(value))))
                .map_err(|error| error_policy.mask_secret(error))
        }
        (
            Value::Deferred(DeferredValue::Coalesce { value, default }),
            crate::schema::FitKind::Coalesce {
                value: value_fit,
                default: default_fit,
            },
        ) => {
            let value = canonicalize_with_fit(*value, value_fit, defs, purpose, error_policy)?;
            let default =
                canonicalize_with_fit(*default, default_fit, defs, purpose, error_policy)?;
            Ok(Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(value),
                default: Box::new(default),
            }))
        }
        (value, crate::schema::FitKind::Union { member, fit }) => {
            // The witness retains the declaration member as well as its
            // resolved child fit. Canonicalization needs only the child, but
            // consuming both here makes the single selection decision
            // explicit and prevents a second member lookup.
            let _chosen_member = member;
            canonicalize_with_fit(value, fit, defs, purpose, error_policy)
        }
        (
            _,
            crate::schema::FitKind::Enum {
                canonical,
                rewrite: true,
            },
        ) => Ok(Value::Concrete(ConcreteValue::CanonicalEnum(
            canonical.clone(),
        ))),
        (value, crate::schema::FitKind::Enum { rewrite: false, .. }) => Ok(value),
        (Value::Concrete(ConcreteValue::List(items)), crate::schema::FitKind::List(fits)) => {
            debug_assert_eq!(items.len(), fits.len());
            let items = items
                .into_iter()
                .zip(fits)
                .enumerate()
                .map(|(index, (item, fit))| {
                    canonicalize_with_fit(item, fit, defs, purpose, error_policy)
                        .map_err(|error| error_policy.at_list_index(index, error))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Concrete(ConcreteValue::List(items)))
        }
        (
            Value::Concrete(ConcreteValue::StringList(items)),
            crate::schema::FitKind::StringList(fits),
        ) => {
            // Reference elements require the recursive typed comparison path;
            // StringList remains the canonical union-of-plain-strings form.
            // This changes only the in-memory list representation, never raw
            // string values or their JSON payload/state representation.
            let reference_elements = fits.iter().any(|fit| {
                matches!(
                    fit.attr.as_attr().shape_with_defs(defs),
                    crate::schema::Shape::Reference { .. }
                )
            });
            debug_assert_eq!(items.len(), fits.len());
            let canonicalized = items
                .into_iter()
                .zip(fits)
                .map(|(item, fit)| {
                    canonicalize_with_fit(
                        Value::Concrete(ConcreteValue::String(item)),
                        fit,
                        defs,
                        purpose,
                        error_policy,
                    )
                })
                .enumerate()
                .map(|(index, result)| {
                    result.map_err(|error| error_policy.at_list_index(index, error))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if reference_elements {
                return Ok(Value::Concrete(ConcreteValue::List(canonicalized)));
            }
            let strings = canonicalized
                .iter()
                .map(|item| match item {
                    Value::Concrete(ConcreteValue::String(value)) => Some(value.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            Ok(strings.map_or_else(
                || Value::Concrete(ConcreteValue::List(canonicalized)),
                |strings| Value::Concrete(ConcreteValue::StringList(strings)),
            ))
        }
        (Value::Concrete(ConcreteValue::Map(map)), crate::schema::FitKind::Map(fits)) => {
            debug_assert_eq!(map.len(), fits.len());
            let entries = map
                .into_iter()
                .zip(fits)
                .map(|((key, value), fit)| {
                    let canonical_key = fit.canonical_key.clone().unwrap_or_else(|| key.clone());
                    let value =
                        canonicalize_with_fit(value, &fit.value, defs, purpose, error_policy)
                            .map_err(|error| error_policy.at_map_value(key.clone(), error))?;
                    Ok((key, canonical_key, value))
                })
                .collect::<Result<Vec<_>, P::Error>>()?;
            Ok(Value::Concrete(ConcreteValue::Map(
                collect_canonicalized_map(entries, error_policy)?,
            )))
        }
        (Value::Concrete(ConcreteValue::Map(map)), crate::schema::FitKind::Struct(fits)) => {
            debug_assert_eq!(map.len(), fits.len());
            let fields = map
                .into_iter()
                .zip(fits)
                .filter_map(|((key, value), fit)| {
                    let resolved_type = fit.attr;
                    canonicalize_with_fit(value, fit, defs, purpose, error_policy)
                        .map_err(|error| error_policy.at_struct_field(key.clone(), error))
                        .map(|value| {
                            (!(matches!(purpose, CanonicalizationPurpose::State)
                                && crate::schema::reads_as_absent(&value, resolved_type)))
                            .then_some(value)
                        })
                        .transpose()
                        .map(|result| result.map(|value| (key, value)))
                })
                .collect::<Result<IndexMap<_, _>, P::Error>>()?;
            Ok(Value::Concrete(ConcreteValue::Map(fields)))
        }
        (value, crate::schema::FitKind::PassThrough | crate::schema::FitKind::Deferred) => {
            Ok(value)
        }
        (value, _) => {
            debug_assert!(false, "fit witness and value shape diverged");
            Ok(value)
        }
    }
}

fn collect_canonicalized_map<
    H: PlaceholderKind,
    B: BindingAddress,
    P: CanonicalizationErrorPolicy,
>(
    entries: Vec<(String, String, Value<H, B>)>,
    error_policy: P,
) -> Result<IndexMap<String, Value<H, B>>, P::Error> {
    let mut seen = BTreeSet::new();
    let collision = entries.iter().find_map(|(original_key, canonical_key, _)| {
        (!seen.insert(canonical_key.clone())).then(|| (original_key.clone(), canonical_key.clone()))
    });

    if let Some((original_key, canonical_key)) = collision {
        error_policy.map_key_collision(original_key, canonical_key)?;
        return Ok(entries
            .into_iter()
            .map(|(original_key, _, value)| (original_key, value))
            .collect());
    }

    Ok(entries
        .into_iter()
        .map(|(_, canonical_key, value)| (canonical_key, value))
        .collect())
}

fn canonical_map_key_text<H: PlaceholderKind, B: BindingAddress>(
    value: Value<H, B>,
) -> Result<String, crate::schema::TypeError> {
    match value {
        Value::Concrete(ConcreteValue::CanonicalEnum(canonical)) => {
            Ok(canonical.api_value().to_string())
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => Ok(raw.as_str().to_string()),
        Value::Concrete(ConcreteValue::String(string)) => Ok(string.expose_plaintext().to_string()),
        unexpected @ (Value::Concrete(
            ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::Duration(_)
            | ConcreteValue::List(_)
            | ConcreteValue::StringList(_)
            | ConcreteValue::Map(_),
        )
        | Value::Deferred(
            DeferredValue::ModuleIf(_)
            | DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Coalesce { .. }
            | DeferredValue::IntegerArithmetic(_)
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Access(_)
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::Placeholder(_),
        )) => Err(crate::schema::TypeError::TypeMismatch {
            expected: "String or Enum map key".to_string(),
            got: unexpected.type_name(),
        }),
    }
}

fn type_contains_enum(
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> bool {
    fn walk<'a>(
        attr_type: &'a AttributeType,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
        visited: &mut std::collections::HashSet<*const AttributeType>,
    ) -> bool {
        let resolved = attr_type.resolve_refs_with_defs(defs).as_attr();
        let identity = std::ptr::from_ref(resolved);
        if !visited.insert(identity) {
            return false;
        }
        let contains = match &resolved.kind {
            AttrTypeKind::UnloadedSchemaType(_) => false,
            AttrTypeKind::Enum { .. } => true,
            AttrTypeKind::List { element_type, .. } => walk(element_type, defs, visited),
            AttrTypeKind::Map { key, value } => {
                walk(key, defs, visited) || walk(value, defs, visited)
            }
            AttrTypeKind::Struct { fields, .. } => fields
                .iter()
                .any(|field| walk(&field.field_type, defs, visited)),
            AttrTypeKind::Union(_) => crate::schema::union_members_with_defs(resolved, defs)
                .expect("resolved Union must expose its members")
                .iter()
                .flatten()
                .any(|member| walk(member.as_attr(), defs, visited)),
            AttrTypeKind::Reference { .. }
            | AttrTypeKind::String { .. }
            | AttrTypeKind::ResourceHandle { .. }
            | AttrTypeKind::StringLiteralSet { .. }
            | AttrTypeKind::Int { .. }
            | AttrTypeKind::Float { .. }
            | AttrTypeKind::Bool
            | AttrTypeKind::Duration => false,
            AttrTypeKind::Ref(_) => {
                unreachable!("resolve_refs_with_defs must peel the top-level Ref")
            }
        };
        visited.remove(&identity);
        contains
    }

    walk(attr_type, defs, &mut std::collections::HashSet::new())
}

fn finish_no_fit<H: PlaceholderKind, B: BindingAddress, P: CanonicalizationErrorPolicy>(
    value: Value<H, B>,
    expected: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    purpose: CanonicalizationPurpose<'_>,
    error_policy: P,
) -> Result<Value<H, B>, P::Error> {
    if matches!(&expected.kind, AttrTypeKind::ResourceHandle { .. })
        || (purpose.rejects_unrecognized_enum_text() && type_contains_enum(expected, defs))
    {
        let source = crate::schema::TypeError::TypeMismatch {
            expected: expected.type_name(),
            got: value.type_name(),
        };
        error_policy.canonicalization_failed(value, source)
    } else {
        Ok(value)
    }
}

/// Preserve legacy partial canonicalization and diagnostics for values that do
/// not fit at all. Union acceptance never comes from this path. Provider output
/// alone follows the closest failure shape so invalid enum text fails closed;
/// persisted state retains its established forward-compatible lenience.
fn canonicalize_without_fit<
    'a,
    H: PlaceholderKind,
    B: BindingAddress,
    P: CanonicalizationErrorPolicy,
>(
    value: Value<H, B>,
    unwrapped: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
    ancestry: crate::schema::FieldAncestry,
    purpose: CanonicalizationPurpose<'_>,
    error_policy: P,
    visited_refs: &mut Vec<&'a str>,
) -> Result<Value<H, B>, P::Error> {
    if let Value::Deferred(DeferredValue::Secret(inner)) = value {
        return canonicalize_without_fit(
            *inner,
            unwrapped,
            defs,
            ancestry,
            purpose,
            error_policy,
            visited_refs,
        )
        .map(|canonical| Value::Deferred(DeferredValue::Secret(Box::new(canonical))))
        .map_err(|error| error_policy.mask_secret(error));
    }

    if purpose.canonicalizes_full_shape() && is_string_or_list_of_strings(unwrapped, defs) {
        return Ok(canonicalize_to_string_list(value));
    }

    let shape = unwrapped
        .shape_ref_free()
        .expect("canonicalization dispatch must hold a Ref-peeled type");
    if matches!(shape, crate::schema::Shape::Union) {
        if purpose.rejects_unrecognized_enum_text()
            && let Some(concrete) = value.as_concrete()
            && let Some(member) = crate::schema::closest_union_member_for_value_on_path(
                unwrapped,
                concrete,
                defs,
                ancestry,
                visited_refs,
            )
        {
            let canonical = canonicalize_without_fit(
                value,
                member,
                defs,
                ancestry,
                purpose,
                error_policy,
                visited_refs,
            )?;
            return finish_no_fit(canonical, unwrapped, defs, purpose, error_policy);
        }
        if let Some(occurrence) = first_enum_identifier(&value) {
            let source = mask_enum_identifier_error_for_secret(
                crate::schema::TypeError::ValidationFailed {
                    message: format!(
                        "unresolved enum identifier `{}` did not match any Union member",
                        occurrence.raw
                    ),
                },
                occurrence,
            );
            return error_policy.canonicalization_failed(value, source);
        }
        return finish_no_fit(value, unwrapped, defs, purpose, error_policy);
    }

    match (value, shape) {
        (value, crate::schema::Shape::UnloadedSchemaType(_)) => error_policy
            .canonicalization_failed(
                value,
                crate::parser::CustomTypeValidationError::ProvidersNotLoaded.into(),
            ),
        (
            Value::Concrete(ConcreteValue::List(items)),
            crate::schema::Shape::List {
                element_type: inner,
                ..
            },
        ) => {
            let canonicalized = items
                .into_iter()
                .enumerate()
                .map(|(index, value)| {
                    canonicalize_with_type_for_purpose(
                        value,
                        inner,
                        defs,
                        ancestry,
                        purpose,
                        error_policy,
                    )
                    .map_err(|error| error_policy.at_list_index(index, error))
                })
                .collect::<Result<Vec<_>, _>>()?;
            finish_no_fit(
                Value::Concrete(ConcreteValue::List(canonicalized)),
                unwrapped,
                defs,
                purpose,
                error_policy,
            )
        }
        (
            Value::Concrete(ConcreteValue::StringList(items)),
            crate::schema::Shape::List {
                element_type: inner,
                ..
            },
        ) => {
            let canonicalized = items
                .into_iter()
                .enumerate()
                .map(|(index, item)| {
                    canonicalize_with_type_for_purpose(
                        Value::Concrete(ConcreteValue::String(item)),
                        inner,
                        defs,
                        ancestry,
                        purpose,
                        error_policy,
                    )
                    .map_err(|error| error_policy.at_list_index(index, error))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let strings = canonicalized
                .iter()
                .map(|item| match item {
                    Value::Concrete(ConcreteValue::String(value)) => Some(value.clone()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            let canonical = strings.map_or_else(
                || Value::Concrete(ConcreteValue::List(canonicalized)),
                |strings| Value::Concrete(ConcreteValue::StringList(strings)),
            );
            finish_no_fit(canonical, unwrapped, defs, purpose, error_policy)
        }
        (
            Value::Concrete(ConcreteValue::Map(map)),
            crate::schema::Shape::Map {
                key: key_type,
                value: value_type,
            },
        ) => {
            let canonicalized = map
                .into_iter()
                .map(|(key, value)| {
                    let lifted_key: Value<H, B> = crate::schema::lift_map_key(key_type, &key, defs)
                        .into_placeholder_type::<H>();
                    let canonical_key_value = canonicalize_with_type_for_purpose(
                        lifted_key,
                        key_type,
                        defs,
                        ancestry,
                        purpose,
                        error_policy,
                    )
                    .map_err(|error| error_policy.at_map_key(key.clone(), error))?;
                    let canonical_key = match canonical_map_key_text(canonical_key_value) {
                        Ok(canonical_key) => canonical_key,
                        Err(source) => error_policy
                            .canonicalization_failed(
                                Value::<H, B>::Concrete(ConcreteValue::<H, B>::String(
                                    key.clone().into(),
                                )),
                                source,
                            )
                            .map(|_| key.clone())
                            .map_err(|error| error_policy.at_map_key(key.clone(), error))?,
                    };
                    let canonical_value = canonicalize_with_type_for_purpose(
                        value,
                        value_type,
                        defs,
                        ancestry,
                        purpose,
                        error_policy,
                    )
                    .map_err(|error| error_policy.at_map_value(key.clone(), error))?;
                    Ok((key, canonical_key, canonical_value))
                })
                .collect::<Result<Vec<_>, P::Error>>()?;
            finish_no_fit(
                Value::Concrete(ConcreteValue::Map(collect_canonicalized_map(
                    canonicalized,
                    error_policy,
                )?)),
                unwrapped,
                defs,
                purpose,
                error_policy,
            )
        }
        (Value::Concrete(ConcreteValue::Map(map)), crate::schema::Shape::Struct { .. }) => {
            let fields = crate::schema::struct_fields_with_defs(unwrapped, defs)
                .expect("Shape::Struct must expose struct fields internally");
            let canonicalized = map
                .into_iter()
                .filter_map(|(key, value)| {
                    match crate::schema::struct_field_for_key(fields, &key, ancestry) {
                        Some(field) => {
                            let canonicalized = canonicalize_with_type_for_purpose(
                                value,
                                &field.field_type,
                                defs,
                                ancestry.descend(field),
                                purpose,
                                error_policy,
                            )
                            .map(|value| {
                                let resolved_type = field.field_type.resolve_refs_with_defs(defs);
                                (!(matches!(purpose, CanonicalizationPurpose::State)
                                    && crate::schema::reads_as_absent(&value, resolved_type)))
                                .then_some(value)
                            });
                            canonicalized
                                .map_err(|error| error_policy.at_struct_field(key.clone(), error))
                                .transpose()
                                .map(|result| result.map(|value| (key, value)))
                        }
                        None => Some(Ok((key, value))),
                    }
                })
                .collect::<Result<IndexMap<_, _>, P::Error>>()?;
            finish_no_fit(
                Value::Concrete(ConcreteValue::Map(canonicalized)),
                unwrapped,
                defs,
                purpose,
                error_policy,
            )
        }
        (
            value,
            crate::schema::Shape::Enum {
                identity: expected_identity,
                values,
                ..
            },
        ) => {
            let resolver = crate::resource::EnumValueResolver::with_defs(unwrapped, defs);
            match value {
                Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => {
                    let resolved = if purpose.resolves_identifiers_as_state_text() {
                        resolver.resolve_state_text(raw.as_str())
                    } else if let Some(provider_name) = purpose.provider_config_name() {
                        resolve_provider_config_enum_identifier(
                            &resolver,
                            &raw,
                            expected_identity,
                            provider_name,
                        )
                    } else {
                        resolver.resolve_raw(&raw)
                    };
                    match resolved {
                        Ok(canonical) => {
                            Ok(Value::Concrete(ConcreteValue::CanonicalEnum(canonical)))
                        }
                        Err(source) => error_policy.canonicalization_failed(
                            Value::Concrete(ConcreteValue::EnumIdentifier(raw)),
                            source,
                        ),
                    }
                }
                Value::Concrete(ConcreteValue::String(string))
                    if purpose.canonicalizes_full_shape()
                        && purpose.accepts_enum_string(values.is_some()) =>
                {
                    match resolver.resolve_state_text(string.expose_plaintext()) {
                        Ok(canonical) => {
                            Ok(Value::Concrete(ConcreteValue::CanonicalEnum(canonical)))
                        }
                        Err(source) => error_policy.canonicalization_failed(
                            Value::Concrete(ConcreteValue::String(string)),
                            source,
                        ),
                    }
                }
                Value::Concrete(ConcreteValue::CanonicalEnum(ref canonical))
                    if canonical.identity().same_type(expected_identity) =>
                {
                    Ok(value)
                }
                Value::Concrete(ConcreteValue::CanonicalEnum(ref canonical)) => {
                    let source = crate::schema::TypeError::TypeMismatch {
                        expected: expected_identity.to_string(),
                        got: canonical.identity().to_string(),
                    };
                    error_policy.canonicalization_failed(value, source)
                }
                other => finish_no_fit(other, unwrapped, defs, purpose, error_policy),
            }
        }
        (value, _) => finish_no_fit(value, unwrapped, defs, purpose, error_policy),
    }
}

pub(crate) fn resolve_provider_config_enum_identifier(
    resolver: &crate::resource::EnumValueResolver<'_, crate::schema::Compiled>,
    raw: &crate::resource::RawEnumIdentifier,
    expected_identity: &crate::schema::TypeIdentity,
    provider_name: &str,
) -> Result<crate::resource::CanonicalEnumValue, crate::schema::TypeError> {
    // The schema identity remains authoritative. Only after its normal
    // resolution fails may a provider block substitute its own name for the
    // identity's provider axis; the rest of the identity must stay exact.
    let first = resolver.resolve_raw(raw);
    if first.is_ok() {
        return first;
    }

    let member = match raw.parsed() {
        crate::resource::RawEnumIdentifierParts::ProviderQualified {
            provider,
            type_name,
            value,
        } if provider == provider_name
            && expected_identity.segments.is_empty()
            && type_name == &expected_identity.kind =>
        {
            value
        }
        crate::resource::RawEnumIdentifierParts::FullyQualified {
            provider,
            segments,
            type_name,
            value,
        } if provider == provider_name
            && segments == &expected_identity.segments
            && type_name == &expected_identity.kind =>
        {
            value
        }
        _ => return first,
    };

    resolver.resolve_raw(&crate::resource::RawEnumIdentifier::parse(member.clone()))
}

#[derive(Clone, Copy)]
struct EnumIdentifierOccurrence<'a> {
    raw: &'a crate::resource::RawEnumIdentifier,
    inside_secret: bool,
}

fn first_enum_identifier<H: PlaceholderKind, B: BindingAddress>(
    value: &Value<H, B>,
) -> Option<EnumIdentifierOccurrence<'_>> {
    first_enum_identifier_inner(value, false)
}

fn first_enum_identifier_inner<H: PlaceholderKind, B: BindingAddress>(
    value: &Value<H, B>,
    inside_secret: bool,
) -> Option<EnumIdentifierOccurrence<'_>> {
    match value {
        Value::Deferred(DeferredValue::ModuleIf(deferred)) => deferred
            .values()
            .into_iter()
            .find_map(|value| first_enum_identifier_inner(value, inside_secret)),
        Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => {
            Some(EnumIdentifierOccurrence { raw, inside_secret })
        }
        Value::Concrete(ConcreteValue::List(items)) => items
            .iter()
            .find_map(|value| first_enum_identifier_inner(value, inside_secret)),
        Value::Concrete(ConcreteValue::Map(map)) => map
            .values()
            .find_map(|value| first_enum_identifier_inner(value, inside_secret)),
        Value::Deferred(DeferredValue::Secret(inner)) => first_enum_identifier_inner(inner, true),
        Value::Deferred(DeferredValue::Access(access)) => {
            first_enum_identifier_inner(access.base(), inside_secret)
        }
        Value::Deferred(DeferredValue::Coalesce { value, default }) => {
            first_enum_identifier_inner(value, inside_secret)
                .or_else(|| first_enum_identifier_inner(default, inside_secret))
        }
        Value::Deferred(DeferredValue::IntegerArithmetic(expression)) => {
            first_enum_identifier_inner(expression.lhs(), inside_secret)
                .or_else(|| first_enum_identifier_inner(expression.rhs(), inside_secret))
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
            DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Unavailable { .. }
            | DeferredValue::ResolvedFunctionCall { .. }
            | DeferredValue::Unknown(_)
            | DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::RedactedSecret { .. },
        ) => None,
        Value::Deferred(DeferredValue::Placeholder(_)) => None,
    }
}

/// Body of state and desired canonicalization for the
/// `string_or_list_of_strings` case.
fn canonicalize_to_string_list<H: PlaceholderKind, B: BindingAddress>(
    value: Value<H, B>,
) -> Value<H, B> {
    match value {
        Value::Concrete(ConcreteValue::StringList(items)) => {
            Value::Concrete(ConcreteValue::StringList(items))
        }
        Value::Concrete(ConcreteValue::String(s)) => {
            Value::Concrete(ConcreteValue::StringList(vec![s]))
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let mut strings = Vec::with_capacity(items.len());
            for item in &items {
                match item {
                    Value::Concrete(ConcreteValue::String(s)) => strings.push(s.clone()),
                    _ => return Value::Concrete(ConcreteValue::List(items)),
                }
            }
            Value::Concrete(ConcreteValue::StringList(strings))
        }
        Value::Deferred(DeferredValue::Secret(inner)) => Value::Deferred(DeferredValue::Secret(
            Box::new(canonicalize_to_string_list(*inner)),
        )),
        other => other,
    }
}

/// Witness holding an exclusive borrow of resources canonicalized
/// against a specific schema registry.
///
/// The only producer is [`canonicalize_resources_with_schemas`]. While
/// the witness is alive, callers cannot mutate the underlying resources
/// through another borrow, so identity code can require this type as
/// evidence that the canonicalize pass already ran.
#[derive(Debug)]
pub struct CanonicalizedResources<
    'a,
    S = crate::resource::ModuleSource,
    H: PlaceholderKind = crate::resource::NoPlaceholder,
    B: BindingAddress = crate::resource::AbsoluteBindingAddress,
    D = crate::resource::UnexpandedDependencyFields<B>,
> {
    resources: &'a mut [crate::resource::Resource<S, H, B, D>],
}

impl<'a, S, H: PlaceholderKind, B: BindingAddress, D> CanonicalizedResources<'a, S, H, B, D> {
    pub(crate) fn into_mut_slice(self) -> &'a mut [crate::resource::Resource<S, H, B, D>] {
        self.resources
    }
}

/// A desired value could not be resolved to the enum value declared by its
/// resource or data-source schema.
///
/// The structured [`crate::schema::TypeError`] is retained as typed data while
/// this terminal diagnostic renders it exactly once.
#[derive(Debug, Error)]
#[error("{resource}: failed to canonicalize enum attribute `{attribute}`: {error}")]
pub struct EnumCanonicalizationError {
    resource: Box<crate::resource::ResourceId>,
    attribute: String,
    error: Box<crate::schema::TypeError>,
}

impl EnumCanonicalizationError {
    fn new(
        resource: crate::resource::ResourceId,
        attribute: impl Into<String>,
        error: crate::schema::TypeError,
    ) -> Self {
        Self {
            resource: Box::new(resource),
            attribute: attribute.into(),
            error: Box::new(error),
        }
    }

    pub fn resource(&self) -> &crate::resource::ResourceId {
        &self.resource
    }

    pub fn attribute(&self) -> &str {
        &self.attribute
    }

    /// Typed terminal reason for the canonicalization failure.
    pub fn error(&self) -> &crate::schema::TypeError {
        &self.error
    }
}

fn unresolved_enum_identifier_type_error(
    raw: &crate::resource::RawEnumIdentifier,
) -> crate::schema::TypeError {
    crate::schema::TypeError::ValidationFailed {
        message: format!("unresolved enum identifier `{raw}` survived canonicalization"),
    }
}

fn mask_enum_identifier_error_for_secret(
    error: crate::schema::TypeError,
    occurrence: EnumIdentifierOccurrence<'_>,
) -> crate::schema::TypeError {
    if occurrence.inside_secret {
        error.masked_for_secret()
    } else {
        error
    }
}

fn unresolved_enum_identifier_type_error_in<H: PlaceholderKind, B: BindingAddress>(
    value: &Value<H, B>,
) -> Option<crate::schema::TypeError> {
    let occurrence = first_enum_identifier(value)?;
    Some(mask_enum_identifier_error_for_secret(
        unresolved_enum_identifier_type_error(occurrence.raw),
        occurrence,
    ))
}

fn canonicalize_desired_attribute_map_with_schema<
    H: PlaceholderKind,
    B: BindingAddress,
    Attributes,
>(
    attributes: &mut Attributes,
    schema: &crate::schema::CompiledResourceSchema,
    resource: &crate::resource::ResourceId,
    source: DesiredValueSource,
) -> Result<(), EnumCanonicalizationError>
where
    Attributes: Default
        + IntoIterator<Item = (String, Value<H, B>)>
        + std::iter::FromIterator<(String, Value<H, B>)>,
{
    let mut canonicalized = Vec::new();
    for (key, value) in std::mem::take(attributes) {
        let canonical = match schema.attributes.get(&key) {
            Some(attr_schema) => canonicalize_desired_with_type_and_ancestry(
                value,
                &attr_schema.attr_type,
                &schema.defs,
                crate::schema::FieldAncestry::from_input_mode(attr_schema.input_mode()),
                source,
            )
            .map_err(|source| {
                EnumCanonicalizationError::new(resource.clone(), key.clone(), source)
            })?,
            None => {
                if let Some(source) = unresolved_enum_identifier_type_error_in(&value) {
                    return Err(EnumCanonicalizationError::new(
                        resource.clone(),
                        key,
                        source,
                    ));
                }
                value
            }
        };
        canonicalized.push((key, canonical));
    }
    *attributes = canonicalized.into_iter().collect();
    Ok(())
}

fn reject_unresolved_enum_identifiers_in_attributes<
    'a,
    H: PlaceholderKind + 'a,
    B: BindingAddress + 'a,
>(
    resource: &crate::resource::ResourceId,
    attributes: impl IntoIterator<Item = (&'a String, &'a Value<H, B>)>,
) -> Result<(), EnumCanonicalizationError> {
    for (attribute, value) in attributes {
        if let Some(source) = unresolved_enum_identifier_type_error_in(value) {
            return Err(EnumCanonicalizationError::new(
                resource.clone(),
                attribute.clone(),
                source,
            ));
        }
    }
    Ok(())
}

/// Canonicalize every schema-known value in one provider or persisted state
/// attribute map. This lenient entry point intentionally preserves
/// unrecognized enum-positioned strings as literal state text.
pub(crate) fn canonicalize_state_attribute_map_with_schema<Attributes>(
    attributes: &mut Attributes,
    schema: &crate::schema::CompiledResourceSchema,
) where
    Attributes:
        Default + IntoIterator<Item = (String, Value)> + std::iter::FromIterator<(String, Value)>,
{
    *attributes = std::mem::take(attributes)
        .into_iter()
        .filter_map(|(key, value)| {
            canonicalize_state_attribute_value_with_schema(value, schema, &key)
                .into_present()
                .map(|value| (key, value))
        })
        .collect();
}

/// Walk every desired resource attribute using its schema.
///
/// Structural rewrites such as `Union[String, list(String)]` remain, and every
/// enum-typed leaf is resolved to [`ConcreteValue::CanonicalEnum`]. Enum
/// resolution is total: an invalid member, an unselected Union containing a
/// typed identifier, or any identifier outside the known schema returns a
/// structured [`EnumCanonicalizationError`]. Unknown non-enum attributes and
/// ordinary shape mismatches remain validation's responsibility.
///
/// Call this once after `resolver::resolve_refs_*` and before the
/// differ runs, so every `Resource` flowing into the plan / state /
/// provider boundary carries the canonical shape. See #2481, #2511.
pub fn canonicalize_resources_with_schemas<'a, S, H: PlaceholderKind, B: BindingAddress, D>(
    resources: &'a mut [crate::resource::Resource<S, H, B, D>],
    registry: &crate::schema::SchemaRegistry,
) -> Result<CanonicalizedResources<'a, S, H, B, D>, EnumCanonicalizationError> {
    for resource in resources.iter_mut() {
        if let Some(schema) = registry.get_for(resource) {
            canonicalize_desired_attribute_map_with_schema(
                &mut resource.attributes,
                schema.as_schema(),
                &resource.id,
                DesiredValueSource::AuthoredDsl,
            )?;
        } else {
            reject_unresolved_enum_identifiers_in_attributes(
                &resource.id,
                resource.attributes.iter(),
            )?;
        }
    }

    Ok(CanonicalizedResources { resources })
}

/// Re-canonicalize values returned by `normalize_desired`.
///
/// Guest output is provider/state text, so strings resolve only by literal API
/// membership or a declared alias. Dotted strings are never reinterpreted as
/// namespaced DSL identifiers.
pub fn canonicalize_normalized_resources_with_schemas<S, D>(
    resources: &mut [crate::resource::Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >],
    registry: &crate::schema::SchemaRegistry,
) -> Result<(), EnumCanonicalizationError> {
    for resource in resources.iter_mut() {
        if let Some(schema) = registry.get_for(resource) {
            canonicalize_desired_attribute_map_with_schema(
                &mut resource.attributes,
                schema.as_schema(),
                &resource.id,
                DesiredValueSource::ProviderOutput,
            )?;
        } else {
            reject_unresolved_enum_identifiers_in_attributes(
                &resource.id,
                resource.attributes.iter(),
            )?;
        }
    }
    Ok(())
}

type ProviderConfigSchemaFn<'a> = dyn Fn(&str) -> Option<crate::schema::ValidatedConfigSchema> + 'a;

/// Provider configs whose schema-known attributes have been canonicalized.
///
/// Durable identity code accepts this wrapper rather than raw
/// `ProviderConfig` slices, so callers cannot skip provider config enum
/// canonicalization at the hash seam.
#[derive(Debug, Clone)]
pub struct CanonicalizedProviderConfigs<H: PlaceholderKind = crate::resource::NoPlaceholder> {
    providers: Vec<crate::parser::ProviderConfig<H, crate::resource::AbsoluteBindingAddress>>,
}

impl<H: PlaceholderKind> CanonicalizedProviderConfigs<H> {
    pub fn as_slice(
        &self,
    ) -> &[crate::parser::ProviderConfig<H, crate::resource::AbsoluteBindingAddress>] {
        &self.providers
    }

    #[cfg(test)]
    pub(crate) fn from_configs_for_test(
        providers: Vec<crate::parser::ProviderConfig<H, crate::resource::AbsoluteBindingAddress>>,
    ) -> Self {
        Self { providers }
    }
}

pub(crate) fn canonicalize_provider_config_with_type_and_ancestry<
    H: PlaceholderKind,
    B: BindingAddress,
>(
    provider_name: &str,
    value: Value<H, B>,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    ancestry: crate::schema::FieldAncestry,
) -> Result<Value<H, B>, crate::schema::TypeError> {
    let canonical = canonicalize_with_type_for_purpose(
        value,
        attr_type,
        defs,
        ancestry,
        CanonicalizationPurpose::ProviderConfig { provider_name },
        RejectEnumResolutionErrors,
    )?;
    if let Some(error) = unresolved_enum_identifier_type_error_in(&canonical) {
        return Err(error);
    }
    Ok(canonical)
}

/// A provider configuration value could not be resolved through the
/// provider-declared attribute type.
#[derive(Debug, Error)]
#[error("provider {provider}: failed to canonicalize enum attribute `{attribute}`: {error}")]
pub struct ProviderConfigCanonicalizationError {
    provider: String,
    attribute: String,
    error: Box<crate::schema::TypeError>,
}

impl ProviderConfigCanonicalizationError {
    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn attribute(&self) -> &str {
        &self.attribute
    }

    /// Typed terminal reason for the canonicalization failure.
    pub fn error(&self) -> &crate::schema::TypeError {
        &self.error
    }

    pub(crate) fn into_parts(self) -> (String, String, crate::schema::TypeError) {
        (self.provider, self.attribute, *self.error)
    }
}

/// Canonicalize one provider configuration attribute map using its
/// host-declared types. Typed canonicalization is total; the only explicit
/// identifier check is therefore for attributes without a declared type.
pub(crate) fn canonicalize_provider_config_attributes_with_types<
    H: PlaceholderKind,
    B: BindingAddress,
>(
    provider_name: &str,
    attributes: &IndexMap<String, Value<H, B>>,
    attr_types: &BTreeMap<String, AttributeType>,
) -> Result<IndexMap<String, Value<H, B>>, ProviderConfigCanonicalizationError> {
    let mut canonical = IndexMap::with_capacity(attributes.len());
    for (attribute, value) in attributes {
        let value = match attr_types.get(attribute) {
            Some(attr_type) => canonicalize_provider_config_with_type_and_ancestry(
                provider_name,
                value.clone(),
                attr_type,
                crate::schema::empty_defs_for_schema_walks(),
                crate::schema::FieldAncestry::WRITABLE,
            )
            .map_err(|error| ProviderConfigCanonicalizationError {
                provider: provider_name.to_string(),
                attribute: attribute.clone(),
                error: Box::new(error),
            })?,
            None => {
                if let Some(error) = unresolved_enum_identifier_type_error_in(value) {
                    return Err(ProviderConfigCanonicalizationError {
                        provider: provider_name.to_string(),
                        attribute: attribute.clone(),
                        error: Box::new(error),
                    });
                }
                value.clone()
            }
        };
        canonical.insert(attribute.clone(), value);
    }
    Ok(canonical)
}

/// Walk every provider config's attributes, canonicalizing enum-typed leaves
/// such as provider identity `region` before those values feed durable
/// identity hashing.
pub fn canonicalize_provider_configs_with_schemas<H: PlaceholderKind>(
    providers: &[crate::parser::ProviderConfig<H, crate::resource::AbsoluteBindingAddress>],
    provider_config_schema_fn: &ProviderConfigSchemaFn<'_>,
) -> Result<CanonicalizedProviderConfigs<H>, ProviderConfigCanonicalizationError> {
    let mut providers = providers.to_vec();
    for provider in providers.iter_mut() {
        let config_schema = provider_config_schema_fn(&provider.name).unwrap_or_default();
        provider.attributes = canonicalize_provider_config_attributes_with_types(
            &provider.name,
            &provider.attributes,
            config_schema.parts(),
        )?;
    }
    Ok(CanonicalizedProviderConfigs { providers })
}

/// [`DataSource`](crate::resource::DataSource) counterpart of
/// [`canonicalize_resources_with_schemas`]. Schema lookup routes through the
/// data-source registry (`get_for_data_source`). A data source's attributes are
/// authored `read` inputs, so `EnumIdentifier` uses authored-DSL semantics. Unknown
/// non-enum values pass through, while an identifier without a resolvable
/// schema is rejected (carina#3181, carina#3740).
pub fn canonicalize_data_sources_with_schemas<S, H: PlaceholderKind, B: BindingAddress, D>(
    data_sources: &mut [crate::resource::DataSource<S, H, B, D>],
    registry: &crate::schema::SchemaRegistry,
) -> Result<(), EnumCanonicalizationError> {
    for data_source in data_sources.iter_mut() {
        if let Some(schema) = registry.get_for_data_source(data_source) {
            canonicalize_desired_attribute_map_with_schema(
                &mut data_source.attributes,
                schema.as_schema(),
                &data_source.id,
                DesiredValueSource::AuthoredDsl,
            )?;
        } else {
            reject_unresolved_enum_identifiers_in_attributes(
                &data_source.id,
                data_source.attributes.iter(),
            )?;
        }
    }
    Ok(())
}

/// Walk every entry in the managed-current map and canonicalize attribute
/// values whose declared schema type is `Union[String, list(String)]`
/// into `Value::Concrete(ConcreteValue::StringList)`.
///
/// State files written before #2510 / #2511 (or by an apply path that
/// somehow produced the legacy shape) come back through serde as the
/// natural `Value::Concrete(ConcreteValue::String)` / `Value::Concrete(ConcreteValue::List)` form. Run this immediately
/// after managed current state is built — typically right after
/// `StateFile::build_state_for_resource` populates the map — so the
/// differ never sees a non-canonical state value compared against a
/// canonical desired value. See #2481, #2513.
pub fn canonicalize_states_with_schemas(
    states: &mut std::collections::HashMap<crate::resource::ResourceId, crate::resource::Current>,
    registry: &crate::schema::SchemaRegistry,
) {
    for current in states.values_mut() {
        let state = match current {
            crate::resource::Current::Absent(state) => state.state(),
            crate::resource::Current::Present(present) => present.state(),
            crate::resource::Current::CreateInFlight(_pending) => continue,
            crate::resource::Current::CreateOutcomeUnknown(_unknown) => continue,
            crate::resource::Current::MissingIdentifier(state) => state.state(),
        };
        let kind = if state.id.resource_type.is_empty() {
            None
        } else {
            registry.get(
                &state.id.provider,
                &state.id.resource_type,
                crate::schema::SchemaKind::Resource,
            )
        };
        let Some(schema) = kind else {
            continue;
        };
        if let Some(attributes) = current.attributes_mut() {
            canonicalize_state_attribute_map_with_schema(attributes, schema.as_schema());
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::schema::{
        PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
    };

    use super::*;

    type Value = crate::resource::ExpandedValue;
    type ConcreteValue = crate::resource::ExpandedConcreteValue;
    type DeferredValue = crate::resource::ExpandedDeferredValue;
    type InterpolationPart = crate::resource::ExpandedInterpolationPart;

    fn absolute_binding(binding: &str) -> crate::resource::AbsoluteBindingAddress {
        crate::resource::RootScope::new()
            .resolve(crate::resource::RelativeBindingName::new(binding))
    }

    fn resolved_deferred_templates(
        expressions: impl IntoIterator<Item = crate::parser::ExpandedDeferredForExpression>,
    ) -> crate::parser::ResolvedDeferredForExpressions {
        crate::parser::ResolvedDeferredForExpressions::from_expressions_for_test(expressions)
    }

    type LoopValue = crate::resource::Value<LoopSlot>;
    type LoopDeferredValue = crate::resource::DeferredValue<LoopSlot>;

    #[derive(Debug, Clone, PartialEq, serde::Serialize)]
    struct DisplayMutablePlaceholder {
        identity: String,
        #[serde(skip)]
        wording: String,
    }

    impl std::fmt::Display for DisplayMutablePlaceholder {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(&self.wording)
        }
    }

    impl crate::resource::PlaceholderKind for DisplayMutablePlaceholder {
        fn inference_type_authority(&self) -> crate::resource::PlaceholderTypeAuthority<'_> {
            crate::resource::PlaceholderTypeAuthority::AwaitingSubstitution
        }
        type DirectiveBool<B: crate::resource::BindingAddress> = bool;
        type DirectivePreventDestroy<B: crate::resource::BindingAddress> =
            crate::resource::PreventDestroy;
        fn rebind_directives<
            B: crate::resource::BindingAddress,
            C: crate::resource::BindingAddress,
        >(
            directives: crate::resource::Directives<B, Self>,
            resolve: &mut impl FnMut(B) -> C,
        ) -> crate::resource::Directives<C, Self> {
            crate::resource::directive_expr::rebind_resolved_directives(directives, resolve)
        }

        fn map_absolute_directive_values(
            _directives: &mut crate::resource::Directives<
                crate::resource::AbsoluteBindingAddress,
                Self,
            >,
            _map: &mut impl FnMut(
                &crate::resource::AbsoluteBindingAddress,
            ) -> crate::resource::AbsoluteBindingAddress,
        ) {
        }
    }

    #[test]
    fn placeholder_secret_material_uses_structured_serde_not_display_wording() {
        let value = |wording: &str| {
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
                DisplayMutablePlaceholder {
                    identity: "stable-slot".to_string(),
                    wording: wording.to_string(),
                },
            ))
        };

        let before = canonical_secret_hash_material(&value("old wording"))
            .expect("canonical material for placeholder");
        let after = canonical_secret_hash_material(&value("new wording"))
            .expect("canonical material for placeholder");
        assert_eq!(
            before, after,
            "display wording must not affect secret hashes"
        );
    }

    #[test]
    fn parser_placeholder_display_keeps_render_unknown_parentheses() {
        let value: crate::resource::Value<crate::resource::ParserPlaceholder> =
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Placeholder(
                crate::resource::ParserPlaceholder::FnParam {
                    name: "input".to_string(),
                },
            ));

        assert_eq!(
            format_value(&value),
            "(unresolved function parameter: input)"
        );
    }

    #[test]
    fn state_canonicalization_fits_provider_output_with_missing_declared_required_field() {
        use crate::resource::ValueString;

        let schema = PatternResourceSchema::new("test.Widget")
            .attribute(
                PatternAttributeSchema::provider_populated(
                    "output",
                    PatternAttributeType::union(vec![
                        PatternAttributeType::struct_(
                            "Output",
                            vec![
                                PatternStructField::required(
                                    "eventual",
                                    PatternAttributeType::string(),
                                )
                                .build(),
                                PatternStructField::optional(
                                    "mode",
                                    PatternAttributeType::union(vec![
                                        PatternAttributeType::string(),
                                        PatternAttributeType::list(PatternAttributeType::string()),
                                    ]),
                                )
                                .build(),
                            ],
                        ),
                        PatternAttributeType::map(PatternAttributeType::string()),
                    ]),
                )
                .build(),
            )
            .validate()
            .expect("schema fixture must be valid");
        let mut attributes = IndexMap::from([(
            "output".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "mode".to_string(),
                Value::Concrete(ConcreteValue::String("api_value".into())),
            )]))),
        )]);

        canonicalize_state_attribute_map_with_schema(&mut attributes, schema.as_schema());

        let Value::Concrete(ConcreteValue::Map(output)) = &attributes["output"] else {
            panic!("output must remain a map");
        };
        assert!(matches!(
            output.get("mode"),
            Some(Value::Concrete(ConcreteValue::StringList(values)))
                if values == &[ValueString::from("api_value")]
        ));
    }

    #[test]
    fn state_canonicalization_cascades_without_a_fit_and_is_idempotent() {
        let archive_policy = PatternAttributeType::struct_(
            "ArchivePolicy",
            vec![
                PatternStructField::required("retention_days", PatternAttributeType::int()).build(),
            ],
        );
        let schema = PatternResourceSchema::new("test.Widget")
            .with_def("ArchivePolicy", archive_policy)
            .attribute(
                PatternAttributeSchema::optional(
                    "archive_policy",
                    PatternAttributeType::ref_("ArchivePolicy"),
                )
                .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "settings",
                    PatternAttributeType::struct_(
                        "Settings",
                        vec![
                            PatternStructField::optional(
                                "archive_policy",
                                PatternAttributeType::ref_("ArchivePolicy"),
                            )
                            .build(),
                        ],
                    ),
                )
                .build(),
            )
            .validate()
            .expect("schema fixture must be valid");
        let empty_map = || Value::Concrete(ConcreteValue::Map(IndexMap::new()));
        let mut attributes = IndexMap::from([
            ("archive_policy".to_string(), empty_map()),
            (
                "settings".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "archive_policy".to_string(),
                    empty_map(),
                )]))),
            ),
        ]);

        assert_eq!(
            schema
                .effective_attribute("archive_policy")
                .expect("archive_policy attribute")
                .canonicalize_state(empty_map()),
            CanonicalState::Absent,
        );

        canonicalize_state_attribute_map_with_schema(&mut attributes, schema.as_schema());

        assert!(!attributes.contains_key("archive_policy"));
        assert!(
            !attributes.contains_key("settings"),
            "the parent struct must become absent after its only child becomes absent"
        );

        let canonical_once = attributes.clone();
        canonicalize_state_attribute_map_with_schema(&mut attributes, schema.as_schema());
        assert_eq!(
            attributes, canonical_once,
            "state canonicalization must be idempotent"
        );
    }

    #[test]
    fn state_canonicalization_cascades_empty_required_struct_from_successful_fit() {
        let schema = PatternResourceSchema::new("test.Widget")
            .attribute(
                PatternAttributeSchema::provider_populated(
                    "output",
                    PatternAttributeType::struct_(
                        "Output",
                        vec![
                            PatternStructField::optional(
                                "archive_policy",
                                PatternAttributeType::struct_(
                                    "ArchivePolicy",
                                    vec![
                                        PatternStructField::required(
                                            "retention_days",
                                            PatternAttributeType::int(),
                                        )
                                        .build(),
                                    ],
                                ),
                            )
                            .build(),
                        ],
                    ),
                )
                .build(),
            )
            .validate()
            .expect("schema fixture must be valid");
        let mut attributes = IndexMap::from([(
            "output".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "archive_policy".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            )]))),
        )]);

        canonicalize_state_attribute_map_with_schema(&mut attributes, schema.as_schema());

        assert!(
            !attributes.contains_key("output"),
            "the fitted parent struct must become absent after its only child becomes absent"
        );
    }

    #[test]
    fn state_canonicalization_is_idempotent_when_dropping_a_required_field_changes_fit() {
        let settings_type = PatternAttributeType::struct_(
            "Settings",
            vec![
                PatternStructField::required(
                    "config",
                    PatternAttributeType::struct_(
                        "Config",
                        vec![
                            PatternStructField::optional("label", PatternAttributeType::string())
                                .build(),
                        ],
                    ),
                )
                .build(),
                PatternStructField::optional("keep", PatternAttributeType::string()).build(),
            ],
        );
        let schema = PatternResourceSchema::new("test.Widget")
            .attribute(PatternAttributeSchema::optional("settings", settings_type).build())
            .validate()
            .expect("schema fixture must be valid");
        let settings = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "config".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            ),
            (
                "keep".to_string(),
                Value::Concrete(ConcreteValue::String("present".into())),
            ),
        ])));
        let mut visited_refs = Vec::new();
        let attribute = &schema.as_schema().attributes["settings"];
        let resolved = attribute
            .attr_type
            .resolve_refs_with_defs_on_path(&schema.as_schema().defs, &mut visited_refs);
        assert!(
            crate::schema::fit_value_to_resolved_type_on_path(
                resolved.as_attr(),
                &settings,
                &schema.as_schema().defs,
                crate::schema::FieldAncestry::from_input_mode(attribute.input_mode()),
                CanonicalizationPurpose::State,
                &mut visited_refs,
            )
            .is_some(),
            "the first pass must exercise the fit worker"
        );
        let mut attributes = IndexMap::from([("settings".to_string(), settings)]);

        canonicalize_state_attribute_map_with_schema(&mut attributes, schema.as_schema());
        let canonical_once = attributes.clone();
        let canonical_settings = &canonical_once["settings"];
        let mut visited_refs = Vec::new();
        assert!(
            crate::schema::fit_value_to_resolved_type_on_path(
                resolved.as_attr(),
                canonical_settings,
                &schema.as_schema().defs,
                crate::schema::FieldAncestry::from_input_mode(attribute.input_mode()),
                CanonicalizationPurpose::State,
                &mut visited_refs,
            )
            .is_none(),
            "dropping the required config field must route the second pass through no-fit"
        );

        canonicalize_state_attribute_map_with_schema(&mut attributes, schema.as_schema());
        assert_eq!(attributes, canonical_once);
    }

    #[test]
    fn state_canonicalization_drops_empty_all_optional_struct_and_keeps_zero_field_marker() {
        let optional_config = PatternAttributeType::struct_(
            "OptionalConfig",
            vec![PatternStructField::optional("label", PatternAttributeType::string()).build()],
        );
        let marker = PatternAttributeType::struct_("Marker", Vec::new());
        let schema = PatternResourceSchema::new("test.Widget")
            .attribute(PatternAttributeSchema::optional("marker", marker.clone()).build())
            .attribute(
                PatternAttributeSchema::optional("optional_config", optional_config.clone())
                    .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "settings",
                    PatternAttributeType::struct_(
                        "Settings",
                        vec![
                            PatternStructField::optional("optional_config", optional_config)
                                .build(),
                            PatternStructField::optional("marker", marker).build(),
                        ],
                    ),
                )
                .build(),
            )
            .validate()
            .expect("schema fixture must be valid");
        let mut attributes = IndexMap::from([
            (
                "marker".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            ),
            (
                "optional_config".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::new())),
            ),
            (
                "settings".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    (
                        "optional_config".to_string(),
                        Value::Concrete(ConcreteValue::Map(IndexMap::new())),
                    ),
                    (
                        "marker".to_string(),
                        Value::Concrete(ConcreteValue::Map(IndexMap::new())),
                    ),
                ]))),
            ),
        ]);

        let empty = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
        assert_eq!(
            schema
                .effective_attribute("optional_config")
                .expect("optional_config attribute")
                .canonicalize_state(empty.clone()),
            CanonicalState::Absent,
        );
        assert_eq!(
            schema
                .effective_attribute("marker")
                .expect("marker attribute")
                .canonicalize_state(empty.clone()),
            CanonicalState::Present(empty),
        );

        canonicalize_state_attribute_map_with_schema(&mut attributes, schema.as_schema());

        assert!(attributes.contains_key("marker"));
        assert!(!attributes.contains_key("optional_config"));
        let Some(Value::Concrete(ConcreteValue::Map(settings))) = attributes.get("settings") else {
            panic!("settings must remain a map");
        };
        assert!(!settings.contains_key("optional_config"));
        assert!(settings.contains_key("marker"));
    }

    #[test]
    fn reads_as_absent_owns_the_empty_value_and_resolved_struct_type_decision() {
        use crate::schema::CompiledStructField as StructField;

        let non_empty_struct = AttributeType::struct_(
            "Config",
            vec![StructField::optional("label", AttributeType::string()).build()],
        );
        let marker = AttributeType::struct_("Marker", Vec::new());
        let map = AttributeType::map(AttributeType::string());
        let defs = crate::schema::empty_defs_for_schema_walks();
        let empty = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
        let populated = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "label".to_string(),
            Value::Concrete(ConcreteValue::String("configured".into())),
        )])));

        assert!(crate::schema::reads_as_absent(
            &empty,
            non_empty_struct.resolve_refs_with_defs(defs),
        ));
        assert!(!crate::schema::reads_as_absent(
            &populated,
            non_empty_struct.resolve_refs_with_defs(defs),
        ));
        assert!(!crate::schema::reads_as_absent(
            &empty,
            marker.resolve_refs_with_defs(defs),
        ));
        assert!(!crate::schema::reads_as_absent(
            &empty,
            map.resolve_refs_with_defs(defs),
        ));
    }

    #[test]
    fn state_canonicalization_does_not_resolve_top_level_block_aliases() {
        let schema = PatternResourceSchema::new("test.Widget")
            .attribute(
                PatternAttributeSchema::optional(
                    "rules",
                    PatternAttributeType::union(vec![
                        PatternAttributeType::string(),
                        PatternAttributeType::list(PatternAttributeType::string()),
                    ]),
                )
                .with_block_name("rule")
                .build(),
            )
            .validate()
            .expect("schema fixture must be valid");
        let original = Value::Concrete(ConcreteValue::String("api_value".into()));
        let mut attributes = IndexMap::from([("rule".to_string(), original.clone())]);

        canonicalize_state_attribute_map_with_schema(&mut attributes, schema.as_schema());

        assert_eq!(attributes["rule"], original);
    }

    fn redacted_secret(material: &str) -> Value {
        Value::Deferred(DeferredValue::RedactedSecret {
            hash: RedactedSecretHash::generate(material.as_bytes()),
        })
    }

    fn valid_marker_shaped_string(material: &str) -> String {
        format!(
            "_secret:{}",
            RedactedSecretHash::generate(material.as_bytes()).encoded_digest()
        )
    }

    fn redacted_hash_from_persisted_json(value: &serde_json::Value) -> RedactedSecretHash {
        value
            .get(PERSISTED_SECRET_KEY)
            .and_then(serde_json::Value::as_str)
            .and_then(RedactedSecretHash::from_encoded_digest)
            .expect("structural persisted secret with a valid digest")
    }

    #[test]
    fn canonical_secret_hash_material_bytes_are_legacy_compatible() {
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "$secret".to_string(),
                Value::Concrete(ConcreteValue::String("literal".into())),
            ),
            (
                "list".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::Int(1)),
                    Value::Concrete(ConcreteValue::Bool(true)),
                ])),
            ),
        ])));

        assert_eq!(
            canonical_secret_hash_material(&value).expect("canonical material"),
            br#"{"$secret":"literal","list":[1,true]}"#,
        );
    }

    #[test]
    fn persisted_codec_never_reclassifies_marker_shaped_strings() {
        let marker = valid_marker_shaped_string("plain-marker-collision");
        let plain = Value::Concrete(ConcreteValue::String(marker.clone().into()));
        let persisted = PersistedValue::encode_provider_value(&plain, None)
            .expect("plain marker-shaped string is persistable");

        assert_eq!(persisted.as_json(), &serde_json::json!(marker));
        assert_eq!(persisted.decode(), Some(plain));
    }

    #[test]
    fn persisted_codec_keeps_nested_marker_shaped_strings_plain() {
        let marker = valid_marker_shaped_string("nested-marker-collision");
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "list".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String(marker.clone().into()),
                )])),
            ),
            (
                "map".to_string(),
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "value".to_string(),
                    Value::Concrete(ConcreteValue::String(marker.clone().into())),
                )]))),
            ),
            (
                "string_list".to_string(),
                Value::Concrete(ConcreteValue::StringList(vec![marker.clone().into()])),
            ),
        ])));
        let persisted = PersistedValue::encode_provider_value(&value, None)
            .expect("nested marker-shaped strings are persistable");
        let decoded = persisted.decode().expect("persisted map is non-null");
        let Value::Concrete(ConcreteValue::Map(decoded)) = decoded else {
            panic!("persisted value must decode as a map")
        };

        assert!(matches!(
            decoded.get("list"),
            Some(Value::Concrete(ConcreteValue::List(values)))
                if matches!(
                    values.as_slice(),
                    [Value::Concrete(ConcreteValue::String(value))]
                        if value.expose_plaintext() == marker
                )
        ));
        assert!(matches!(
            decoded.get("map"),
            Some(Value::Concrete(ConcreteValue::Map(values)))
                if matches!(
                    values.get("value"),
                    Some(Value::Concrete(ConcreteValue::String(value)))
                        if value.expose_plaintext() == marker
                )
        ));
        assert!(matches!(
            decoded.get("string_list"),
            Some(Value::Concrete(ConcreteValue::List(values)))
                if matches!(
                    values.as_slice(),
                    [Value::Concrete(ConcreteValue::String(value))]
                        if value.expose_plaintext() == marker
                )
        ));
    }

    #[test]
    fn persisted_codec_encodes_real_secrets_as_structural_tags() {
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("structural-secret".into()),
        ))));
        let persisted = PersistedValue::encode_provider_value(&secret, None)
            .expect("real secret is persistable");
        let object = persisted
            .as_json()
            .as_object()
            .expect("a redacted secret must use a structural object");

        assert_eq!(object.len(), 1);
        let digest = object
            .get("$secret")
            .and_then(serde_json::Value::as_str)
            .expect("the structural tag carries a bare digest");
        assert!(digest.starts_with("$argon2id$"));
        assert!(!digest.starts_with("_secret:"));
        assert!(matches!(
            persisted.decode(),
            Some(Value::Deferred(DeferredValue::RedactedSecret { .. }))
        ));
        assert_eq!(persisted.decoded_json(), serde_json::json!("(secret)"));
        assert!(!persisted.decoded_json().to_string().contains(digest));
    }

    #[test]
    fn persisted_codec_escapes_user_secret_keys() {
        let user_map = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "$secret".to_string(),
            Value::Concrete(ConcreteValue::String("ordinary".into())),
        )])));
        let persisted = PersistedValue::encode_provider_value(&user_map, None)
            .expect("user map is persistable");

        assert_eq!(
            persisted.as_json(),
            &serde_json::json!({"$$secret": "ordinary"})
        );
        assert_eq!(
            persisted.decoded_json(),
            serde_json::json!({"$secret": "ordinary"})
        );
        assert_eq!(persisted.decode(), Some(user_map));
    }

    #[test]
    fn persisted_codec_rejects_malformed_mixed_and_unknown_host_objects() {
        let digest = RedactedSecretHash::generate(b"host-object-errors").encoded_digest();
        for invalid in [
            serde_json::json!({"$secret": 42}),
            serde_json::json!({"$secret": "not-a-digest"}),
            serde_json::json!({"$secret": digest, "sibling": true}),
            serde_json::json!({"$unknown": true}),
        ] {
            assert!(
                serde_json::from_value::<PersistedValue>(invalid).is_err(),
                "invalid host object must fail closed",
            );
        }
    }

    #[test]
    fn exports_keep_marker_shaped_strings_literal_and_reject_secret_tags() {
        let marker = valid_marker_shaped_string("export-marker-collision");
        let plain = Value::Concrete(ConcreteValue::String(marker.clone().into()));
        let persisted = PersistedExportValue::encode(&plain).expect("plain export is persistable");

        assert_eq!(persisted.as_json(), &serde_json::json!(marker));
        assert_eq!(persisted.decode(), Some(plain));
        assert!(
            serde_json::from_value::<PersistedExportValue>(serde_json::json!({
                "$secret": RedactedSecretHash::generate(b"forbidden-export").encoded_digest(),
            }))
            .is_err(),
            "exports must reject typed redacted-secret tags",
        );
    }

    #[test]
    fn render_duration_picks_largest_clean_unit() {
        use std::time::Duration;
        assert_eq!(render_duration(Duration::from_secs(0)), "0s");
        assert_eq!(render_duration(Duration::from_secs(30)), "30s");
        assert_eq!(render_duration(Duration::from_secs(60)), "1min");
        assert_eq!(render_duration(Duration::from_secs(90)), "90s");
        assert_eq!(render_duration(Duration::from_secs(2700)), "45min");
        assert_eq!(render_duration(Duration::from_secs(3600)), "1h");
        assert_eq!(render_duration(Duration::from_secs(4500)), "75min");
        assert_eq!(render_duration(Duration::from_secs(7200)), "2h");
        // No day/week unit — values past 24h render in hours.
        assert_eq!(render_duration(Duration::from_secs(86400)), "24h");
        // Large prime-ish second count keeps the seconds form.
        assert_eq!(render_duration(Duration::from_secs(90061)), "90061s");
    }

    #[test]
    fn value_to_json_duration_emits_integer_seconds() {
        let v = Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(
            4500,
        )));
        let j = value_to_json(&v).unwrap();
        assert_eq!(j, serde_json::json!(4500));
    }

    #[test]
    fn value_duration_survives_serde_encode_decode() {
        // Confirms the `#[serde(with = "duration_secs")]` adapter on
        // `Value::Concrete(ConcreteValue::Duration)` emits and reads back integer seconds, not
        // the default `{secs, nanos}` shape.
        let v = Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(
            4500,
        )));
        let json = serde_json::to_string(&v).unwrap();
        let back: Value = serde_json::from_str(&json).unwrap();
        match back {
            Value::Concrete(ConcreteValue::Duration(d)) => {
                assert_eq!(d, std::time::Duration::from_secs(4500))
            }
            other => panic!("expected Value::Concrete(ConcreteValue::Duration), got {other:?}"),
        }
    }

    #[test]
    fn render_unknown_upstream_export() {
        use crate::resource::ExpandedAccessPath as AccessPath;
        let path = AccessPath::root_with_fields("network", "vpc", vec!["vpc_id".to_string()]);
        let r = ApplyTimeUnknown::UpstreamExport { path };
        assert_eq!(
            render_unknown(&r),
            "(known after upstream apply: network.vpc.vpc_id)"
        );
    }

    #[test]
    fn render_unknown_upstream_export_with_subscript() {
        use crate::resource::{ExpandedAccessPath as AccessPath, Subscript};
        let path = AccessPath::root_with_fields_and_subscripts(
            "network",
            "accounts",
            Vec::new(),
            vec![Subscript::Int { index: 0 }],
        );
        let r = ApplyTimeUnknown::UpstreamExport { path };
        assert_eq!(
            render_unknown(&r),
            "(known after upstream apply: network.accounts[0])"
        );
    }

    #[test]
    fn render_unknown_upstream_export_with_string_subscript() {
        use crate::resource::{ExpandedAccessPath as AccessPath, Subscript};
        let path = AccessPath::root_with_fields_and_subscripts(
            "vpc",
            "tags",
            Vec::new(),
            vec![Subscript::Str { key: "Name".into() }],
        );
        let r = ApplyTimeUnknown::UpstreamExport { path };
        assert_eq!(
            render_unknown(&r),
            "(known after upstream apply: vpc.tags['Name'])"
        );
    }

    #[test]
    fn render_unknown_pending_reread_reasons() {
        use crate::resource::{PendingReRead, ReReadReason};

        let cases = [
            (
                ReReadReason::Incomplete {
                    detail: "response omitted attributes".to_string(),
                },
                "(unknown until next refresh: read after apply was incomplete — response omitted attributes)",
            ),
            (
                ReReadReason::ReadFailed {
                    detail: "AccessDenied".to_string(),
                },
                "(unknown until next refresh: read after apply failed — AccessDenied)",
            ),
            (
                ReReadReason::NotFound,
                "(unknown until next refresh: resource was not found during read after apply)",
            ),
        ];

        for (reason, expected) in cases {
            let unknown: ApplyTimeUnknown<crate::resource::AbsoluteBindingAddress> =
                ApplyTimeUnknown::PendingReRead(PendingReRead::new(reason));
            assert_eq!(render_unknown(&unknown), expected);
        }
    }

    /// `ApplyTimeUnknown::Display` flows into the user-facing error from
    /// `map_plan_save_error` / `SerializationError::Display`. Pin
    /// the wording so a future regression does not silently degrade
    /// the diagnostic.
    #[test]
    fn unknown_reason_display_wording() {
        use crate::resource::ExpandedAccessPath as AccessPath;
        let path = AccessPath::root_with_fields("network", "vpc", vec!["vpc_id".into()]);
        assert_eq!(
            format!("{}", ApplyTimeUnknown::UpstreamExport { path }),
            "upstream value network.vpc.vpc_id"
        );
        use crate::resource::{PendingReRead, ReReadReason};
        assert_eq!(
            format!(
                "{}",
                ApplyTimeUnknown::<crate::resource::AbsoluteBindingAddress>::PendingReRead(
                    PendingReRead::new(ReReadReason::ReadFailed {
                        detail: "AccessDenied".to_string(),
                    }),
                )
            ),
            "read after apply failed: AccessDenied"
        );
    }

    #[test]
    fn test_value_to_json_string() {
        let v = Value::Concrete(ConcreteValue::String("hello".into()));
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!("hello"));
    }

    #[test]
    fn test_value_to_json_int() {
        let v = Value::Concrete(ConcreteValue::Int(42));
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!(42));
    }

    #[test]
    fn test_value_to_json_float() {
        let v = Value::Concrete(ConcreteValue::Float(1.5));
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!(1.5));
    }

    #[test]
    fn test_value_to_json_nan_returns_error() {
        let v = Value::Concrete(ConcreteValue::Float(f64::NAN));
        let result = value_to_json(&v);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("NaN"));
    }

    #[test]
    fn test_value_to_json_infinity_returns_error() {
        let v = Value::Concrete(ConcreteValue::Float(f64::INFINITY));
        let result = value_to_json(&v);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("inf"));
    }

    #[test]
    fn test_value_to_json_neg_infinity_returns_error() {
        let v = Value::Concrete(ConcreteValue::Float(f64::NEG_INFINITY));
        let result = value_to_json(&v);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("-inf"));
    }

    #[test]
    fn test_value_to_json_nan_in_list_returns_error() {
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::Float(f64::NAN)),
        ]));
        let result = value_to_json(&v);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("NaN"));
    }

    #[test]
    fn test_value_to_json_nan_in_map_returns_error() {
        let mut map = IndexMap::new();
        map.insert(
            "key".to_string(),
            Value::Concrete(ConcreteValue::Float(f64::INFINITY)),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let result = value_to_json(&v);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("inf"));
    }

    #[test]
    fn test_value_to_json_bool() {
        let v = Value::Concrete(ConcreteValue::Bool(true));
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!(true));
    }

    #[test]
    fn test_value_to_json_list() {
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::Int(2)),
        ]));
        assert_eq!(value_to_json(&v).unwrap(), serde_json::json!([1, 2]));
    }

    #[test]
    fn test_value_to_json_map() {
        let mut map = IndexMap::new();
        map.insert(
            "key".to_string(),
            Value::Concrete(ConcreteValue::String("val".into())),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        assert_eq!(
            value_to_json(&v).unwrap(),
            serde_json::json!({"key": "val"})
        );
    }

    #[test]
    fn test_value_to_json_resource_ref_returns_err() {
        // RFC #2371 #2385: `Value::Deferred(DeferredValue::ResourceRef)` reaching JSON
        // serialization is a resolver bug — surface as a structured
        // `UnresolvedResourceRef` Err instead of the legacy
        // `"${vpc.id}"` debug-string fallback.
        let v = Value::root_resource_ref("vpc", "id", vec![]);
        let err = value_to_json(&v).unwrap_err();
        assert!(
            matches!(
                &err,
                SerializationError::UnresolvedResourceRef {
                    path,
                    context: SerializationContext::ValueToJson,
                } if path == "vpc.id"
            ),
            "expected UnresolvedResourceRef/vpc.id/ValueToJson, got: {err:?}"
        );
    }

    #[test]
    fn test_value_to_json_coalesce_returns_err() {
        let value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::root_resource_ref("up", "optional", vec![])),
            default: Box::new(Value::Concrete(ConcreteValue::String("fallback".into()))),
        });
        let error = value_to_json(&value).unwrap_err();
        assert!(matches!(
            error,
            SerializationError::UnresolvedCoalesce {
                context: SerializationContext::ValueToJson,
            }
        ));
    }

    #[test]
    fn format_value_renders_residual_coalesce() {
        let value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::root_resource_ref("up", "optional", vec![])),
            default: Box::new(Value::Concrete(ConcreteValue::List(Vec::new()))),
        });
        assert_eq!(format_value(&value), "up.optional ?? []");
    }

    #[test]
    fn format_value_parenthesizes_a_coalesce_used_as_the_left_operand() {
        let inner = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::root_resource_ref("r", "comment", vec![])),
            default: Box::new(Value::root_resource_ref("r", "parent", vec![])),
        });
        let value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(inner),
            default: Box::new(Value::Concrete(ConcreteValue::String("d".into()))),
        });

        assert_eq!(format_value(&value), "(r.comment ?? r.parent) ?? 'd'");
    }

    #[test]
    fn format_value_parenthesizes_a_coalesce_used_as_an_arithmetic_operand() {
        let coalesce = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::root_resource_ref("r", "count", vec![])),
            default: Box::new(Value::Concrete(ConcreteValue::Int(0))),
        });
        let value = crate::integer_arithmetic::evaluate_integer_values(
            coalesce,
            crate::integer_arithmetic::IntegerOp::Add,
            Value::Concrete(ConcreteValue::Int(1)),
            crate::integer_arithmetic::IntegerOperatorSpan::synthetic(),
        )
        .expect("a deferred Int coalesce is a structurally valid parser-stage operand");

        assert_eq!(format_value(&value), "(r.count ?? 0) + 1");
    }

    #[test]
    fn test_value_to_json_interpolation_returns_err() {
        // RFC #2371 #2386: `Value::Deferred(DeferredValue::Interpolation)` reaching JSON
        // serialization is a canonicalize / resolver bug — surface as
        // `UnresolvedInterpolation` instead of producing a partial
        // string with embedded debug formatting.
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("hello".into()),
        ]));
        let err = value_to_json(&v).unwrap_err();
        assert!(
            matches!(
                &err,
                SerializationError::UnresolvedInterpolation {
                    context: SerializationContext::ValueToJson,
                }
            ),
            "expected UnresolvedInterpolation/ValueToJson, got: {err:?}"
        );
    }

    #[test]
    fn test_value_to_json_function_call_returns_err() {
        // RFC #2371 #2386: `Value::Deferred(DeferredValue::FunctionCall)` reaching JSON
        // serialization is a resolver bug — the function should have
        // been evaluated by this point.
        let v = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::join,
            args: vec![],
            source_line: (0).into(),
        });
        let err = value_to_json(&v).unwrap_err();
        assert!(
            matches!(
                &err,
                SerializationError::UnresolvedFunctionCall {
                    name,
                    context: SerializationContext::ValueToJson,
                } if name == "join"
            ),
            "expected UnresolvedFunctionCall/join/ValueToJson, got: {err:?}"
        );
    }

    #[test]
    fn test_value_to_json_postfix_access_returns_err() {
        use crate::resource::{AccessMissPolicy, PathSegment, Subscript, apply_postfix_access};

        let value = apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 1 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("computed access remains deferred");
        let error = value_to_json(&value).unwrap_err();

        assert!(
            matches!(
                &error,
                SerializationError::UnresolvedAccess {
                    expression,
                    context: SerializationContext::ValueToJson,
                } if expression == "split()[1]"
            ),
            "expected UnresolvedAccess/ValueToJson, got: {error:?}"
        );
    }

    #[test]
    fn test_json_to_dsl_value_string() {
        let j = serde_json::json!("hello");
        assert_eq!(
            json_to_dsl_value(&j),
            Some(Value::Concrete(ConcreteValue::String("hello".into())))
        );
    }

    #[test]
    fn state_row_load_upgrades_raw_legacy_marker_to_structural_secret_tag() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let marker = format!("_secret:argon2:{hash}");
        let mut json = serde_json::json!({ "nested": [marker.clone()] });

        let ordinary = json_to_dsl_value(&json).expect("ordinary JSON value");
        assert!(!contains_redacted_secret(&ordinary));

        let context_free = persisted_json_to_dsl_value(&json, PersistedPurpose::State)
            .expect("valid persisted JSON")
            .expect("persisted JSON value");
        assert!(
            !contains_redacted_secret(&context_free),
            "a raw legacy marker is not a context-free persisted encoding",
        );

        json = crate::resource::legacy_secret_migration::migrate_legacy_state_attribute_for_test(
            json,
            "test.resource",
            "old",
            "tags",
        )
        .expect("migrate legacy test state document");
        assert!(
            json["nested"][0][PERSISTED_SECRET_KEY]
                .as_str()
                .is_some_and(|digest| digest.starts_with("argon2-legacy:")),
        );
        let persisted = PersistedValue::from_serialized_json(json)
            .expect("migrated persisted JSON")
            .decode()
            .expect("persisted JSON value");
        let Value::Concrete(ConcreteValue::Map(map)) = persisted else {
            panic!("persisted object must remain a map")
        };
        let Some(Value::Concrete(ConcreteValue::List(values))) = map.get("nested") else {
            panic!("persisted nested value must remain a list")
        };
        assert!(matches!(
            values.as_slice(),
            [Value::Deferred(DeferredValue::RedactedSecret { hash: loaded })]
                if loaded.encoded_digest().starts_with("argon2-legacy:")
        ));
    }

    #[test]
    fn raw_legacy_secret_digest_is_not_context_free() {
        let digest = format!("argon2:{}", "ab".repeat(32));
        assert!(
            RedactedSecretHash::from_encoded_digest(&digest).is_none(),
            "a raw legacy digest must first be upgraded by its state-file row",
        );
    }

    #[test]
    fn redacted_secret_hash_deserialize_accepts_only_its_serialized_encoding() {
        let bare_hash = "ab".repeat(32);
        let marker = format!("_secret:argon2:{bare_hash}");
        let migrated =
            crate::resource::legacy_secret_migration::migrate_legacy_state_attribute_for_test(
                serde_json::json!(marker),
                "test.resource",
                "old",
                "comment",
            )
            .expect("migrate legacy test state document");
        let digest = RedactedSecretHash::from_encoded_digest(
            migrated[PERSISTED_SECRET_KEY]
                .as_str()
                .expect("migrated digest"),
        )
        .expect("state load must upgrade a raw legacy marker");
        let encoded = serde_json::to_value(&digest).expect("serialize typed digest");

        let decoded_hash: RedactedSecretHash =
            serde_json::from_value(encoded).expect("deserialize serialized digest");
        assert!(decoded_hash.encoded_digest().starts_with("argon2-legacy:"));
        assert!(
            serde_json::from_value::<RedactedSecretHash>(serde_json::json!(marker)).is_err(),
            "the full state marker is not the typed digest's serde encoding",
        );
        assert!(
            serde_json::from_value::<RedactedSecretHash>(serde_json::json!(bare_hash)).is_err(),
            "bare legacy hex is not the typed digest's serde encoding",
        );
        assert!(
            serde_json::from_value::<RedactedSecretHash>(serde_json::json!(format!(
                "argon2:{bare_hash}"
            )))
            .is_err(),
            "an unbound encoded legacy digest is not a typed digest",
        );
        assert!(
            serde_json::from_value::<RedactedSecretHash>(serde_json::json!(format!(
                "argon2-legacy:{}:{}:extra",
                hex::encode(b"salt"),
                bare_hash
            )))
            .is_err(),
            "explicit legacy serde must accept exactly the string shape it writes",
        );

        let v2 = RedactedSecretHash::generate(b"serde-v2");
        let v2_digest = v2.encoded_digest();
        let v2_encoded = serde_json::to_value(&v2).expect("serialize v2 digest");
        let decoded_v2_hash: RedactedSecretHash =
            serde_json::from_value(v2_encoded).expect("deserialize serialized v2 digest");
        assert_eq!(decoded_v2_hash.encoded_digest(), v2_digest);
        assert!(
            serde_json::from_value::<RedactedSecretHash>(serde_json::json!(format!(
                "_secret:{v2_digest}"
            )))
            .is_err(),
            "the full v2 state marker is not the typed digest's serde encoding",
        );
    }

    #[test]
    fn test_json_to_dsl_value_int() {
        let j = serde_json::json!(42);
        assert_eq!(
            json_to_dsl_value(&j),
            Some(Value::Concrete(ConcreteValue::Int(42)))
        );
    }

    #[test]
    fn test_json_to_dsl_value_float() {
        let j = serde_json::json!(1.5);
        assert_eq!(
            json_to_dsl_value(&j),
            Some(Value::Concrete(ConcreteValue::Float(1.5)))
        );
    }

    #[test]
    fn test_json_to_dsl_value_bool() {
        let j = serde_json::json!(true);
        assert_eq!(
            json_to_dsl_value(&j),
            Some(Value::Concrete(ConcreteValue::Bool(true)))
        );
    }

    #[test]
    fn test_json_to_dsl_value_array() {
        let j = serde_json::json!([1, 2]);
        assert_eq!(
            json_to_dsl_value(&j),
            Some(Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2))
            ])))
        );
    }

    #[test]
    fn test_json_to_dsl_value_null() {
        let j = serde_json::Value::Null;
        assert_eq!(json_to_dsl_value(&j), None);
    }

    #[test]
    fn test_json_to_dsl_value_null_in_array() {
        let j = serde_json::json!([1, null, 2]);
        assert_eq!(
            json_to_dsl_value(&j),
            Some(Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Int(1)),
                Value::Concrete(ConcreteValue::Int(2))
            ])))
        );
    }

    #[test]
    fn test_json_to_dsl_value_null_in_object() {
        let j = serde_json::json!({"a": 1, "b": null, "c": "hello"});
        let result = json_to_dsl_value(&j).unwrap();
        if let Value::Concrete(ConcreteValue::Map(map)) = result {
            assert_eq!(map.len(), 2);
            assert_eq!(map.get("a"), Some(&Value::Concrete(ConcreteValue::Int(1))));
            assert_eq!(map.get("b"), None);
            assert_eq!(
                map.get("c"),
                Some(&Value::Concrete(ConcreteValue::String("hello".into())))
            );
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_value_json_encode_decode() {
        let original = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("hello".into())),
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::Bool(false)),
        ]));
        let json = value_to_json(&original).unwrap();
        let back = json_to_dsl_value(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn test_format_value_string() {
        let v = Value::Concrete(ConcreteValue::String("web-sg".into()));
        assert_eq!(format_value(&v), "'web-sg'");
    }

    #[test]
    fn test_format_value_string_uses_canonical_literal_escaping() {
        let text = "quote \" slash \\ newline\n interpolation ${value}";
        let value = Value::Concrete(ConcreteValue::String(text.into()));

        assert_eq!(
            format_value(&value),
            crate::parser::display_string_literal(text)
        );
        let rendered_width = format_value(&value).len();
        assert_eq!(inline_width(&value, rendered_width - 1), None);
        assert_eq!(inline_width(&value, rendered_width), Some(rendered_width));
    }

    #[test]
    fn test_format_value_string_preserves_dotted_text() {
        for text in [
            "A.json",
            "Config.json",
            "A.x",
            "aws.s3.VersioningStatus.Enabled",
        ] {
            let value = Value::Concrete(ConcreteValue::String(text.into()));
            assert_eq!(
                format_value(&value),
                crate::parser::display_string_literal(text)
            );
        }
    }

    #[test]
    fn test_format_value_user_facing_string_is_unquoted() {
        let plaintext = "hello 'quoted'\n${name}";
        let v = Value::Concrete(ConcreteValue::String(plaintext.into()));
        assert_eq!(format_value_user_facing(&v), plaintext);
    }

    #[test]
    fn test_format_value_user_facing_non_string_matches_format_value() {
        for value in [
            Value::Concrete(ConcreteValue::Bool(true)),
            Value::Deferred(DeferredValue::Unknown(
                ApplyTimeUnknown::UpstreamAllExports {
                    binding: absolute_binding("upstream"),
                },
            )),
            Value::root_resource_ref("vpc", "id", vec![]),
        ] {
            assert_eq!(format_value_user_facing(&value), format_value(&value));
        }
    }

    #[test]
    fn test_format_value_enum_identifier_unquoted() {
        let v = Value::Concrete(ConcreteValue::EnumIdentifier(
            crate::resource::RawEnumIdentifier::parse("dedicated"),
        ));
        assert_eq!(format_value(&v), "dedicated");
    }

    #[test]
    fn test_format_value_bare_enum_string() {
        let v = Value::Concrete(ConcreteValue::String("dedicated".into()));
        assert_eq!(format_value(&v), "'dedicated'");
    }

    #[test]
    fn test_format_value_canonical_enum_unquoted() {
        let v = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                crate::schema::TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                "ap-northeast-1",
            ),
        ));
        assert_eq!(format_value(&v), "ap-northeast-1");
    }

    fn enum_shaped_user_map() -> Value {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Enum".to_string(),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                (
                    "identity".to_string(),
                    Value::Concrete(ConcreteValue::Map(IndexMap::from([
                        (
                            "provider".to_string(),
                            Value::Concrete(ConcreteValue::String("aws".into())),
                        ),
                        (
                            "segments".to_string(),
                            Value::Concrete(ConcreteValue::List(vec![
                                Value::Concrete(ConcreteValue::String("ec2".into())),
                                Value::Concrete(ConcreteValue::String("Eip".into())),
                            ])),
                        ),
                        (
                            "kind".to_string(),
                            Value::Concrete(ConcreteValue::String("Domain".into())),
                        ),
                    ]))),
                ),
                (
                    "api_value".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc".into())),
                ),
            ]))),
        )])))
    }

    #[test]
    fn persisted_enum_shaped_user_map_round_trips_as_a_map() {
        let value = enum_shaped_user_map();
        let json =
            PersistedValue::encode_desired_value(&value, None).expect("user map is persistable");

        assert_eq!(json.decode(), Some(value));
    }

    #[test]
    fn persisted_user_dollar_keys_are_escaped_and_round_trip_exactly() {
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "$enum".to_string(),
                Value::Concrete(ConcreteValue::String("user enum key".into())),
            ),
            (
                "$x".to_string(),
                Value::Concrete(ConcreteValue::String("one dollar".into())),
            ),
            (
                "$$y".to_string(),
                Value::Concrete(ConcreteValue::String("two dollars".into())),
            ),
        ])));

        let json =
            PersistedValue::encode_desired_value(&value, None).expect("user map is persistable");
        assert_eq!(
            json.as_json(),
            &serde_json::json!({
                "$$enum": "user enum key",
                "$$x": "one dollar",
                "$$$y": "two dollars",
            })
        );
        assert_eq!(
            json.decoded_json(),
            serde_json::json!({
                "$enum": "user enum key",
                "$x": "one dollar",
                "$$y": "two dollars",
            })
        );
        assert_eq!(json.decode(), Some(value));
    }

    #[test]
    fn persisted_value_serializes_canonical_enum_as_typed_object() {
        let v = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                crate::schema::TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
                "vpc",
            ),
        ));

        assert_eq!(
            PersistedValue::encode_desired_value(&v, None)
                .unwrap()
                .as_json(),
            &serde_json::json!({
                "$enum": {
                    "identity": {
                        "provider": "aws",
                        "segments": ["ec2", "Eip"],
                        "kind": "Domain"
                    },
                    "api_value": "vpc"
                }
            })
        );
        assert_eq!(
            PersistedValue::encode_desired_value(&v, None)
                .unwrap()
                .decoded_json(),
            serde_json::json!("vpc")
        );
    }

    #[test]
    fn ordinary_json_serializes_canonical_enum_as_its_api_value() {
        let value = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                crate::schema::TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
                "vpc",
            ),
        ));

        assert_eq!(value_to_json(&value).unwrap(), serde_json::json!("vpc"));
    }

    #[test]
    fn persisted_json_preserves_canonical_enum_but_ordinary_json_does_not_decode_it() {
        let v = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                crate::schema::TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
                "vpc",
            ),
        ));
        let json = PersistedValue::encode_desired_value(&v, None).unwrap();

        assert_eq!(json.decode(), Some(v));
        assert!(matches!(
            json_to_dsl_value(json.as_json()),
            Some(Value::Concrete(ConcreteValue::Map(map))) if map.contains_key("$enum")
        ));
    }

    #[test]
    fn ordinary_json_never_unescapes_dollar_prefixed_keys() {
        let json = serde_json::json!({"$$x": "user value"});

        assert!(matches!(
            json_to_dsl_value(&json),
            Some(Value::Concrete(ConcreteValue::Map(map)))
                if map.contains_key("$$x") && !map.contains_key("$x")
        ));
    }

    #[test]
    fn persisted_decoder_rejects_unknown_mixed_and_malformed_host_payloads() {
        let valid_payload = serde_json::json!({
            "identity": {
                "provider": "aws",
                "segments": ["ec2", "Eip"],
                "kind": "Domain"
            },
            "api_value": "vpc"
        });
        let cases = [
            serde_json::json!({"$future": {}}),
            serde_json::json!({"$enum": valid_payload, "user": true}),
            serde_json::json!({"$enum": {"api_value": "vpc"}}),
        ];

        for json in cases {
            assert!(
                PersistedValue::from_serialized_json(json.clone()).is_err(),
                "reserved payload must fail closed: {json}"
            );
        }
    }

    #[test]
    fn canonicalize_resources_with_schemas_replaces_enum_leaves_recursively() {
        use crate::schema::{SchemaRegistry, TypeIdentity};

        let domain = PatternAttributeType::enum_(
            TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
            Some(vec!["vpc".to_string(), "standard".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            (PatternResourceSchema::new("ec2.Eip")
                .attribute(PatternAttributeSchema::optional("domain", domain.clone()).build())
                .attribute(
                    PatternAttributeSchema::optional(
                        "domains",
                        PatternAttributeType::list(domain.clone()),
                    )
                    .build(),
                )
                .attribute(
                    PatternAttributeSchema::optional(
                        "domain_by_name",
                        PatternAttributeType::map(domain),
                    )
                    .build(),
                ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let mut resource: crate::resource::Resource =
            crate::resource::Resource::with_provider("aws", "ec2.Eip", "eip", None);
        resource.set_attr(
            "domain",
            Value::Concrete(ConcreteValue::enum_identifier("aws.ec2.Eip.Domain.vpc")),
        );
        resource.set_attr(
            "domains",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::enum_identifier("aws.ec2.Eip.Domain.standard"),
            )])),
        );
        resource.set_attr(
            "domain_by_name",
            Value::Concrete(ConcreteValue::Map(indexmap::indexmap! {
                "primary".to_string() => Value::Concrete(ConcreteValue::enum_identifier("aws.ec2.Eip.Domain.vpc")),
            })),
        );
        let mut resources = vec![resource];

        canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();

        let domain = resources[0].attributes.get("domain").unwrap();
        match domain {
            Value::Concrete(ConcreteValue::CanonicalEnum(c)) => {
                assert_eq!(c.api_value(), "vpc");
                assert_eq!(c.identity().to_string(), "aws.ec2.Eip.Domain");
            }
            other => panic!("expected CanonicalEnum, got {other:?}"),
        }
        let domains = resources[0].attributes.get("domains").unwrap();
        let Value::Concrete(ConcreteValue::List(items)) = domains else {
            panic!("expected list, got {domains:?}");
        };
        assert!(matches!(
            &items[0],
            Value::Concrete(ConcreteValue::CanonicalEnum(c)) if c.api_value() == "standard"
        ));
        let map = resources[0].attributes.get("domain_by_name").unwrap();
        let Value::Concrete(ConcreteValue::Map(map)) = map else {
            panic!("expected map, got {map:?}");
        };
        assert!(matches!(
            map.get("primary").unwrap(),
            Value::Concrete(ConcreteValue::CanonicalEnum(c)) if c.api_value() == "vpc"
        ));
    }

    #[test]
    fn canonicalize_route53_union_keeps_quoted_hosted_zone_id_on_string_arm() {
        use crate::schema::{SchemaRegistry, TypeIdentity};

        let hosted_zone = PatternAttributeType::enum_(
            TypeIdentity::new(
                Some("aws"),
                ["route53", "RecordSet", "AliasTarget"],
                "HostedZoneId",
            ),
            Some(vec!["Z2FDTNDATAQYW2".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let alias_target = PatternAttributeType::struct_(
            "AliasTarget",
            vec![
                PatternStructField::optional(
                    "hosted_zone_id",
                    PatternAttributeType::union(vec![hosted_zone, PatternAttributeType::string()]),
                )
                .build(),
            ],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            (PatternResourceSchema::new("route53.RecordSet")
                .attribute(PatternAttributeSchema::optional("alias_target", alias_target).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let canonicalize = |text: &str| {
            let mut resource: crate::resource::Resource = crate::resource::Resource::with_provider(
                "aws",
                "route53.RecordSet",
                "record",
                None,
            );
            resource.set_attr(
                "alias_target",
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "hosted_zone_id".to_string(),
                    Value::Concrete(ConcreteValue::String(text.to_string().into())),
                )]))),
            );
            let mut resources = vec![resource];
            canonicalize_resources_with_schemas(&mut resources, &registry)
                .expect("the String arm must accept a non-enum literal");
            let Value::Concrete(ConcreteValue::Map(alias_target)) =
                &resources[0].attributes["alias_target"]
            else {
                panic!("expected alias_target map")
            };
            alias_target["hosted_zone_id"].clone()
        };

        assert_eq!(
            canonicalize("Z05136711ZXUHBDOA8D5O"),
            Value::Concrete(ConcreteValue::String("Z05136711ZXUHBDOA8D5O".into())),
            "a non-member literal must remain on the String arm"
        );
        assert_eq!(
            canonicalize("Z2FDTNDATAQYW2"),
            Value::Concrete(ConcreteValue::String("Z2FDTNDATAQYW2".into())),
            "a quoted literal must stay on the String arm even when it is an Enum member"
        );
    }

    #[test]
    fn desired_union_selects_the_struct_that_validation_accepts() {
        use crate::schema::{
            CompiledAttributeType as AttributeType, CompiledStructField as StructField,
            TypeIdentity,
        };

        let mode = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget", "A"], "Mode"),
            Some(vec!["X".to_string()]),
            vec![("X".to_string(), "x_member".to_string())],
            None,
            None,
        );
        let kind = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget", "B"], "Kind"),
            Some(vec!["Y".to_string()]),
            vec![("Y".to_string(), "y_member".to_string())],
            None,
            None,
        );
        let union = AttributeType::union(vec![
            AttributeType::struct_("A", vec![StructField::optional("mode", mode).build()]),
            AttributeType::struct_("B", vec![StructField::optional("kind", kind).build()]),
        ]);
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "kind".to_string(),
            Value::Concrete(ConcreteValue::enum_identifier(
                "test.widget.B.Kind.y_member",
            )),
        )])));

        assert!(
            crate::schema::Schema::flat(union.clone())
                .validate(&value)
                .is_ok()
        );
        let canonical = canonicalize_desired_with_type(
            value,
            &union,
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::AuthoredDsl,
        )
        .expect("canonicalization must choose the same Struct arm as validation");

        let Value::Concrete(ConcreteValue::Map(map)) = canonical else {
            panic!("expected canonical Struct map")
        };
        assert!(matches!(
            map.get("kind"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.api_value() == "Y"
        ));
    }

    #[test]
    fn desired_union_prefers_string_over_enum_for_quoted_literal_in_either_order() {
        use crate::schema::{CompiledAttributeType as AttributeType, TypeIdentity};

        let enum_type = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget"], "Effect"),
            Some(vec!["Allow".to_string()]),
            vec![("Allow".to_string(), "allow".to_string())],
            None,
            None,
        );
        let dynamic_enum = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget"], "DynamicEffect"),
            None,
            Vec::new(),
            None,
            None,
        );
        let literal = Value::Concrete(ConcreteValue::String("allow".into()));

        for union in [
            AttributeType::union(vec![enum_type.clone(), AttributeType::string()]),
            AttributeType::union(vec![AttributeType::string(), enum_type.clone()]),
            AttributeType::union(vec![
                enum_type.clone(),
                AttributeType::union(vec![AttributeType::string()]),
            ]),
            AttributeType::union(vec![
                AttributeType::union(vec![AttributeType::string()]),
                enum_type.clone(),
            ]),
            AttributeType::union(vec![
                dynamic_enum.clone(),
                AttributeType::union(vec![AttributeType::string()]),
            ]),
            AttributeType::union(vec![
                AttributeType::union(vec![AttributeType::string()]),
                dynamic_enum.clone(),
            ]),
        ] {
            assert_eq!(
                canonicalize_desired_with_type(
                    literal.clone(),
                    &union,
                    crate::schema::empty_defs_for_schema_walks(),
                    DesiredValueSource::AuthoredDsl,
                )
                .unwrap(),
                literal,
            );
        }
    }

    fn status_enum() -> AttributeType {
        AttributeType::enum_(
            crate::schema::TypeIdentity::new(Some("test"), ["widget"], "Status"),
            Some(vec!["Enabled".to_string(), "Disabled".to_string()]),
            vec![
                ("Enabled".to_string(), "enabled".to_string()),
                ("Disabled".to_string(), "disabled".to_string()),
            ],
            None,
            None,
        )
    }

    fn status_union() -> AttributeType {
        let status = status_enum();
        AttributeType::union(vec![status.clone(), AttributeType::list(status)])
    }

    fn separator_folding_map_key_type() -> AttributeType {
        AttributeType::map_with_key(
            AttributeType::enum_(
                crate::schema::TypeIdentity::bare("KeyAlgorithm"),
                Some(vec!["RSA-2048".to_string()]),
                Vec::new(),
                None,
                None,
            ),
            AttributeType::string(),
        )
    }

    fn colliding_map_key_value() -> Value {
        Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "RSA-2048".to_string(),
                Value::Concrete(ConcreteValue::String("first".into())),
            ),
            (
                "RSA_2048".to_string(),
                Value::Concrete(ConcreteValue::String("second".into())),
            ),
        ])))
    }

    #[test]
    fn provider_output_union_canonicalizes_recognized_enum_string() {
        let canonical = canonicalize_desired_with_type(
            Value::Concrete(ConcreteValue::String("Enabled".into())),
            &status_union(),
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::ProviderOutput,
        )
        .expect("recognized provider enum text must fit the scalar Enum member");

        assert!(matches!(
            canonical,
            Value::Concrete(ConcreteValue::CanonicalEnum(value))
                if value.api_value() == "Enabled"
        ));
    }

    #[test]
    fn provider_output_union_canonicalizes_recognized_secret_enum_string() {
        let canonical = canonicalize_desired_with_type(
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("Enabled".into()),
            )))),
            &status_union(),
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::ProviderOutput,
        )
        .expect("the fit witness must preserve the Secret wrapper and enum choice");

        let Value::Deferred(DeferredValue::Secret(inner)) = canonical else {
            panic!("expected Secret wrapper");
        };
        assert!(matches!(
            inner.as_ref(),
            Value::Concrete(ConcreteValue::CanonicalEnum(value))
                if value.api_value() == "Enabled"
        ));
    }

    #[test]
    fn state_union_canonicalizes_recognized_enum_string() {
        let canonical = canonicalize_state_with_type(
            Value::Concrete(ConcreteValue::String("Enabled".into())),
            &status_union(),
            crate::schema::empty_defs_for_schema_walks(),
        );

        assert!(matches!(
            canonical,
            Value::Concrete(ConcreteValue::CanonicalEnum(value))
                if value.api_value() == "Enabled"
        ));
    }

    #[test]
    fn provider_output_union_rejects_unrecognized_enum_string() {
        let error = canonicalize_desired_with_type(
            Value::Concrete(ConcreteValue::String("Bogus".into())),
            &status_union(),
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::ProviderOutput,
        )
        .expect_err("unrecognized provider enum text must fail closed inside a Union");

        assert!(error.to_string().contains("Bogus"), "{error}");
    }

    #[test]
    fn state_union_preserves_unrecognized_enum_string() {
        let bogus = Value::Concrete(ConcreteValue::String("Bogus".into()));
        assert_eq!(
            canonicalize_state_with_type(
                bogus.clone(),
                &status_union(),
                crate::schema::empty_defs_for_schema_walks(),
            ),
            bogus,
            "persisted state keeps unknown enum text for forward compatibility"
        );
    }

    #[test]
    fn provider_output_treats_string_list_as_list_for_enum_validation() {
        let attr_type = AttributeType::list(status_enum());
        let error = canonicalize_desired_with_type(
            Value::Concrete(ConcreteValue::StringList(vec!["Bogus".into()])),
            &attr_type,
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::ProviderOutput,
        )
        .expect_err("an invalid enum inside StringList must not cross the provider boundary");

        assert!(error.to_string().contains("Bogus"), "{error}");
    }

    #[test]
    fn provider_output_rejects_shape_mismatch_for_enum_bearing_type() {
        let attr_type = AttributeType::list(status_enum());
        let value = Value::Concrete(ConcreteValue::Int(7));

        canonicalize_desired_with_type(
            value.clone(),
            &attr_type,
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::ProviderOutput,
        )
        .expect_err("a provider-output shape mismatch at an enum-bearing type must fail closed");

        assert_eq!(
            canonicalize_state_with_type(
                value.clone(),
                &attr_type,
                crate::schema::empty_defs_for_schema_walks(),
            ),
            value,
            "persisted state must retain its forward-compatible lenience"
        );
    }

    #[test]
    fn provider_output_rejects_nested_enum_value_shape_mismatch() {
        let attr_type = AttributeType::map_with_key(AttributeType::string(), status_enum());
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "status".to_string(),
            Value::Concrete(ConcreteValue::Int(7)),
        )])));

        let error = canonicalize_desired_with_type(
            value,
            &attr_type,
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::ProviderOutput,
        )
        .expect_err("a nested enum-positioned shape mismatch must fail closed");

        assert!(error.to_string().contains("status"), "{error}");
    }

    #[test]
    fn enum_map_key_is_canonicalized_to_api_spelling() {
        let attr_type = AttributeType::map_with_key(status_enum(), AttributeType::string());
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "enabled".to_string(),
            Value::Concrete(ConcreteValue::String("value".into())),
        )])));

        let canonical = canonicalize_desired_with_type(
            value,
            &attr_type,
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::AuthoredDsl,
        )
        .expect("a valid enum map key must canonicalize");
        let Value::Concrete(ConcreteValue::Map(map)) = canonical else {
            panic!("expected a canonicalized map");
        };
        assert_eq!(
            map.keys().map(String::as_str).collect::<Vec<_>>(),
            ["Enabled"]
        );
    }

    #[test]
    fn ref_enum_map_key_is_canonicalized_with_schema_defs() {
        let attr_type =
            AttributeType::map_with_key(AttributeType::ref_("StatusKey"), AttributeType::string());
        let defs = std::collections::BTreeMap::from([("StatusKey".to_string(), status_enum())]);
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "enabled".to_string(),
            Value::Concrete(ConcreteValue::String("value".into())),
        )])));

        let canonical = canonicalize_desired_with_type(
            value,
            &attr_type,
            &defs,
            DesiredValueSource::AuthoredDsl,
        )
        .expect("a Ref-backed enum map key must canonicalize");
        let Value::Concrete(ConcreteValue::Map(map)) = canonical else {
            panic!("expected a canonicalized map");
        };
        assert_eq!(
            map.keys().map(String::as_str).collect::<Vec<_>>(),
            ["Enabled"]
        );
    }

    #[test]
    fn provider_output_rejects_invalid_enum_map_key_but_state_preserves_it() {
        let attr_type = AttributeType::map_with_key(status_enum(), AttributeType::string());
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Bogus".to_string(),
            Value::Concrete(ConcreteValue::String("value".into())),
        )])));

        let error = canonicalize_desired_with_type(
            value.clone(),
            &attr_type,
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::ProviderOutput,
        )
        .expect_err("an invalid provider-output enum map key must fail closed");
        assert!(error.to_string().contains("Bogus"), "{error}");

        assert_eq!(
            canonicalize_state_with_type(
                value.clone(),
                &attr_type,
                crate::schema::empty_defs_for_schema_walks(),
            ),
            value
        );
    }

    #[test]
    fn desired_map_key_canonicalization_rejects_colliding_api_keys() {
        for source in [
            DesiredValueSource::AuthoredDsl,
            DesiredValueSource::ProviderOutput,
        ] {
            let error = canonicalize_desired_with_type(
                colliding_map_key_value(),
                &separator_folding_map_key_type(),
                crate::schema::empty_defs_for_schema_walks(),
                source,
            )
            .expect_err("distinct source keys must not silently merge after canonicalization");

            match error {
                crate::schema::TypeError::MapKeyError { key, .. } => {
                    assert_eq!(key, "RSA_2048");
                }
                other => panic!("expected a MapKeyError for the colliding key, got {other:?}"),
            }
        }
    }

    #[test]
    fn secret_map_key_collision_masks_the_colliding_key() {
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(colliding_map_key_value())));
        let error = canonicalize_desired_with_type(
            secret,
            &separator_folding_map_key_type(),
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::ProviderOutput,
        )
        .expect_err("a secret map with colliding canonical keys must be rejected");

        let crate::schema::TypeError::MapKeyError { key, .. } = &error else {
            panic!("expected a masked MapKeyError, got {error:?}");
        };
        assert_eq!(key, "(secret)");
        assert!(!error.to_string().contains("RSA"), "{error}");
    }

    #[test]
    fn state_map_key_collision_preserves_every_original_key() {
        let original = colliding_map_key_value();
        let canonical = canonicalize_state_with_type(
            original.clone(),
            &separator_folding_map_key_type(),
            crate::schema::empty_defs_for_schema_walks(),
        );

        assert_eq!(canonical, original);
    }

    #[test]
    fn non_textual_canonical_map_key_is_an_error() {
        assert!(matches!(
            canonical_map_key_text(Value::Concrete(ConcreteValue::Int(7))),
            Err(crate::schema::TypeError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn union_list_fit_accumulates_element_exactness() {
        let open_enum = AttributeType::enum_(
            crate::schema::TypeIdentity::bare("OpenValue"),
            None,
            Vec::new(),
            None,
            None,
        );
        let value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("x".into()),
        )]));

        for union in [
            AttributeType::union(vec![
                AttributeType::list(open_enum.clone()),
                AttributeType::list(AttributeType::string()),
            ]),
            AttributeType::union(vec![
                AttributeType::list(AttributeType::string()),
                AttributeType::list(open_enum.clone()),
            ]),
        ] {
            assert_eq!(
                canonicalize_state_with_type(
                    value.clone(),
                    &union,
                    crate::schema::empty_defs_for_schema_walks(),
                ),
                value,
                "List<String> must beat List<OpenEnum> for a string element"
            );
        }
    }

    #[test]
    fn union_struct_fit_accumulates_field_exactness() {
        let open_enum = AttributeType::enum_(
            crate::schema::TypeIdentity::bare("OpenValue"),
            None,
            Vec::new(),
            None,
            None,
        );
        let enum_struct = AttributeType::struct_(
            "EnumArm",
            vec![crate::schema::CompiledStructField::optional("a", open_enum).build()],
        );
        let string_struct = AttributeType::struct_(
            "StringArm",
            vec![
                crate::schema::CompiledStructField::optional("a", AttributeType::string()).build(),
            ],
        );
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "a".to_string(),
            Value::Concrete(ConcreteValue::String("x".into())),
        )])));

        for union in [
            AttributeType::union(vec![enum_struct.clone(), string_struct.clone()]),
            AttributeType::union(vec![string_struct.clone(), enum_struct.clone()]),
        ] {
            assert_eq!(
                canonicalize_state_with_type(
                    value.clone(),
                    &union,
                    crate::schema::empty_defs_for_schema_walks(),
                ),
                value,
                "Struct{{a:String}} must beat Struct{{a:OpenEnum}} for a string field"
            );
        }
    }

    #[test]
    fn nested_union_fit_evaluates_each_member_once() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        const DEPTH: usize = 12;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut nested = AttributeType::string();
        for level in 0..DEPTH {
            let calls = Arc::clone(&calls);
            let open_enum = AttributeType::enum_(
                crate::schema::TypeIdentity::bare(format!("OpenValue{level}")),
                None,
                Vec::new(),
                Some(crate::schema::validator(move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })),
                None,
            );
            nested = AttributeType::union(vec![open_enum, nested]);
        }

        let value = Value::Concrete(ConcreteValue::String("x".into()));
        assert_eq!(
            canonicalize_state_with_type(
                value.clone(),
                &nested,
                crate::schema::empty_defs_for_schema_walks(),
            ),
            value,
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            DEPTH,
            "bottom-up fit must visit each coercible Enum member exactly once"
        );
    }

    #[test]
    fn validation_only_union_stops_after_an_exact_member() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let first = {
            let calls = Arc::clone(&first_calls);
            AttributeType::refined_string_with_validator(
                crate::schema::TypeIdentity::bare("First"),
                None,
                None,
                crate::schema::validator(move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                None,
            )
        };
        let second = {
            let calls = Arc::clone(&second_calls);
            AttributeType::refined_string_with_validator(
                crate::schema::TypeIdentity::bare("Second"),
                None,
                None,
                crate::schema::validator(move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
                None,
            )
        };

        crate::schema::Schema::flat(AttributeType::union(vec![first, second]))
            .validate(&Value::Concrete(ConcreteValue::String("x".into())))
            .expect("the first exact String member accepts the value");
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn desired_union_list_selection_validates_past_a_deferred_first_item() {
        use crate::schema::{
            CompiledAttributeType as AttributeType, CompiledStructField as StructField,
            TypeIdentity,
        };

        let mode = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget", "A"], "Mode"),
            Some(vec!["X".to_string()]),
            vec![("X".to_string(), "x_member".to_string())],
            None,
            None,
        );
        let kind = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget", "B"], "Kind"),
            Some(vec!["Y".to_string()]),
            vec![("Y".to_string(), "y_member".to_string())],
            None,
            None,
        );
        let union = AttributeType::union(vec![
            AttributeType::list(AttributeType::struct_(
                "A",
                vec![StructField::optional("mode", mode).build()],
            )),
            AttributeType::list(AttributeType::struct_(
                "B",
                vec![StructField::optional("kind", kind).build()],
            )),
        ]);
        let value = Value::Concrete(ConcreteValue::List(vec![
            Value::Deferred(DeferredValue::BindingRef {
                binding: absolute_binding("later"),
            }),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "kind".to_string(),
                Value::Concrete(ConcreteValue::enum_identifier(
                    "test.widget.B.Kind.y_member",
                )),
            )]))),
        ]));

        assert!(
            crate::schema::Schema::flat(union.clone())
                .validate(&value)
                .is_ok()
        );
        let canonical = canonicalize_desired_with_type(
            value,
            &union,
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::AuthoredDsl,
        )
        .expect("the accepting List<Struct B> arm must be selected");

        let Value::Concrete(ConcreteValue::List(items)) = canonical else {
            panic!("expected canonical list")
        };
        let Value::Concrete(ConcreteValue::Map(second)) = &items[1] else {
            panic!("expected second list item to be a map")
        };
        assert!(matches!(
            second.get("kind"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.api_value() == "Y"
        ));
    }

    #[test]
    fn canonicalize_union_enum_identifier_resolves_against_second_matching_enum() {
        use crate::schema::{SchemaRegistry, TypeIdentity};

        let first = PatternAttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget"], "FirstMode"),
            Some(vec!["FIRST".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let second = PatternAttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget"], "SecondMode"),
            Some(vec!["SECOND".to_string()]),
            vec![("SECOND".to_string(), "second".to_string())],
            None,
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            (PatternResourceSchema::new("widget").attribute(
                PatternAttributeSchema::optional(
                    "mode",
                    PatternAttributeType::union(vec![first, second]),
                )
                .build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let mut resource: crate::resource::Resource =
            crate::resource::Resource::with_provider("test", "widget", "example", None);
        resource.set_attr(
            "mode",
            Value::Concrete(ConcreteValue::enum_identifier(
                "test.widget.SecondMode.second",
            )),
        );
        let mut resources = vec![resource];

        canonicalize_resources_with_schemas(&mut resources, &registry)
            .expect("the identifier is a member of the second Enum arm");

        assert!(matches!(
            &resources[0].attributes["mode"],
            Value::Concrete(ConcreteValue::CanonicalEnum(value))
                if value.identity().kind == "SecondMode" && value.api_value() == "SECOND"
        ));
    }

    #[test]
    fn canonicalize_secret_enum_error_masks_plaintext() {
        use crate::schema::{SchemaRegistry, TypeIdentity};

        let plaintext = "plaintext-must-not-leak";
        let mode = PatternAttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget"], "Mode"),
            Some(vec!["allowed".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            (PatternResourceSchema::new("widget")
                .attribute(PatternAttributeSchema::optional("mode", mode).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let mut resource: crate::resource::Resource =
            crate::resource::Resource::with_provider("test", "widget", "secret", None);
        resource.set_attr(
            "mode",
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::enum_identifier(plaintext),
            )))),
        );
        let mut resources = vec![resource];

        let error = canonicalize_resources_with_schemas(&mut resources, &registry)
            .expect_err("a non-member secret enum must fail canonicalization");
        let display = error.to_string();

        assert!(!display.contains(plaintext), "{display}");
        assert!(display.contains("secret"), "{display}");
    }

    #[test]
    fn desired_post_pass_masks_enum_identifier_inside_secret() {
        let plaintext = "post-pass-plaintext-must-not-leak";
        let value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::enum_identifier(plaintext),
        ))));

        let error = canonicalize_desired_with_type(
            value,
            &AttributeType::string(),
            crate::schema::empty_defs_for_schema_walks(),
            DesiredValueSource::AuthoredDsl,
        )
        .expect_err("a secret enum identifier under String must fail the desired post-pass");
        let display = error.to_string();

        assert!(!display.contains(plaintext), "{display}");
        assert!(display.contains("secret"), "{display}");
    }

    #[test]
    fn desired_none_schema_branches_mask_secret_enum_identifier() {
        use crate::schema::SchemaRegistry;

        let plaintext = "none-schema-plaintext-must-not-leak";
        let secret = || {
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::enum_identifier(plaintext),
            ))))
        };
        let registries = [SchemaRegistry::new(), {
            let mut registry = SchemaRegistry::new();
            registry.insert(
                "test",
                (PatternResourceSchema::new("widget"))
                    .validate()
                    .expect("schema fixture must be valid"),
            );
            registry
        }];

        for registry in registries {
            let resource: crate::resource::Resource =
                crate::resource::Resource::with_provider("test", "widget", "secret", None)
                    .with_attribute("unknown", secret());
            let mut resources = vec![resource];
            let error = canonicalize_resources_with_schemas(&mut resources, &registry)
                .expect_err("an unresolved secret enum must fail without a declared attribute");
            let display = error.to_string();
            assert!(!display.contains(plaintext), "{display}");
            assert!(display.contains("secret"), "{display}");
        }
    }

    #[test]
    fn resource_handle_validation_and_desired_canonicalization_reject_values() {
        let handle = AttributeType::resource_handle("test", "widget");
        let string_id = Value::Concrete(ConcreteValue::String("widget-123".into()));
        let identifier = Value::Concrete(ConcreteValue::enum_identifier("widget_binding"));

        assert!(matches!(
            handle.validate(&string_id),
            Err(crate::schema::TypeError::TypeMismatch { .. })
        ));
        assert!(matches!(
            handle.validate(&identifier),
            Err(crate::schema::TypeError::TypeMismatch { .. })
        ));
        assert!(
            canonicalize_desired_with_type(
                string_id,
                &handle,
                crate::schema::empty_defs_for_schema_walks(),
                DesiredValueSource::AuthoredDsl,
            )
            .is_err(),
            "a string value must not become a resource handle during canonicalization",
        );
        assert!(
            canonicalize_desired_with_type(
                identifier,
                &handle,
                crate::schema::empty_defs_for_schema_walks(),
                DesiredValueSource::AuthoredDsl,
            )
            .is_err(),
            "plan-time canonicalization must reject the same identifier as validation"
        );
    }

    #[test]
    fn canonicalize_resources_with_schemas_rejects_unresolved_enum_at_every_typed_depth() {
        use crate::schema::{SchemaRegistry, TypeIdentity};

        let mode = PatternAttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget"], "Mode"),
            Some(vec!["Allow".to_string(), "Deny".to_string()]),
            vec![("Allow".to_string(), "allow".to_string())],
            None,
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            (PatternResourceSchema::new("widget")
                .attribute(PatternAttributeSchema::optional("top", mode.clone()).build())
                .attribute(
                    PatternAttributeSchema::optional(
                        "nested",
                        PatternAttributeType::struct_(
                            "Settings",
                            vec![PatternStructField::optional("mode", mode.clone()).build()],
                        ),
                    )
                    .build(),
                )
                .attribute(
                    PatternAttributeSchema::optional(
                        "items",
                        PatternAttributeType::list(mode.clone()),
                    )
                    .build(),
                )
                .attribute(
                    PatternAttributeSchema::optional(
                        "unselected_union",
                        PatternAttributeType::union(vec![
                            PatternAttributeType::list(mode),
                            PatternAttributeType::map(PatternAttributeType::string()),
                        ]),
                    )
                    .build(),
                ))
            .validate()
            .expect("schema fixture must be valid"),
        );

        let invalid = || {
            Value::Concrete(ConcreteValue::enum_identifier(
                "test.widget.Mode.not_a_member",
            ))
        };
        let cases = [
            ("top", invalid()),
            (
                "nested",
                Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "mode".to_string(),
                    invalid(),
                )]))),
            ),
            (
                "items",
                Value::Concrete(ConcreteValue::List(vec![invalid()])),
            ),
            ("unselected_union", invalid()),
        ];

        for (attribute, value) in cases {
            let mut resource: crate::resource::Resource =
                crate::resource::Resource::with_provider("test", "widget", attribute, None);
            resource.set_attr(attribute, value);
            let mut resources = vec![resource];

            let error = canonicalize_resources_with_schemas(&mut resources, &registry)
                .expect_err("an unresolved enum must fail canonicalization");
            let rendered = error.to_string();

            assert!(rendered.contains(attribute), "{rendered}");
            let leaf = "test.widget.Mode.not_a_member";
            assert_eq!(rendered.matches(leaf).count(), 1, "{rendered}");
            assert!(std::error::Error::source(&error).is_none());
        }
    }

    #[test]
    fn canonicalize_resources_with_schemas_rejects_enum_identifier_outside_schema() {
        let registry = crate::schema::SchemaRegistry::new();
        let mut resources = vec![make_resource(vec![(
            "unknown",
            Value::Concrete(ConcreteValue::enum_identifier("test.Mode.allow")),
        )])];

        canonicalize_resources_with_schemas(&mut resources, &registry)
            .expect_err("no EnumIdentifier may survive desired canonicalization");
    }

    #[test]
    fn canonicalize_resources_with_schemas_keeps_deferred_enum_expressions_in_static_parity() {
        use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
        use crate::resource::InterpolationPart;
        use crate::schema::SchemaRegistry;

        let raw = || Value::Concrete(ConcreteValue::enum_identifier("test.widget.Mode.allow"));
        let runtime_ref = || Value::root_resource_ref("source", "suffix", Vec::new());
        let runtime = [
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::join,
                args: vec![
                    Value::Concrete(ConcreteValue::String("".into())).into(),
                    Value::Concrete(ConcreteValue::List(vec![raw(), runtime_ref()])).into(),
                ],
                source_line: (0).into(),
            }),
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Expr(raw()),
                InterpolationPart::Expr(runtime_ref()),
            ])),
        ];
        let static_values = [
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::join,
                args: vec![
                    Value::Concrete(ConcreteValue::String("".into())).into(),
                    Value::Concrete(ConcreteValue::List(vec![
                        raw(),
                        Value::Concrete(ConcreteValue::String("-suffix".into())),
                    ]))
                    .into(),
                ],
                source_line: (0).into(),
            }),
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Expr(raw()),
                InterpolationPart::Expr(Value::Concrete(ConcreteValue::String("-suffix".into()))),
            ])),
        ];
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            (PatternResourceSchema::new("widget").attribute(
                PatternAttributeSchema::optional("value", PatternAttributeType::string()).build(),
            ))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let source = crate::resource::Resource::with_provider("test", "source", "source", None)
            .with_root_binding("source")
            .with_attribute(
                "suffix",
                Value::Concrete(ConcreteValue::String("-suffix".into())),
            );
        let managed = vec![source];
        let current_states = std::collections::HashMap::new();
        let remote_bindings = std::collections::HashMap::new();
        let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
            managed: &managed,
            compositions: &[],
            managed_current: &current_states,
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &remote_bindings,
            wait_aliases: &[],
        });

        for (runtime, static_value) in runtime.into_iter().zip(static_values) {
            let mut resource: crate::resource::Resource =
                crate::resource::Resource::with_provider("test", "widget", "example", None);
            resource.set_attr("value", runtime);
            let mut resources = vec![resource];

            canonicalize_resources_with_schemas(&mut resources, &registry)
                .expect("a deferred expression owns evaluation of its enum-shaped arguments");
            let deferred_result = crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| {
                    crate::resolver::resolve_ref_value(
                        &resources[0].attributes["value"],
                        &bindings,
                        session,
                    )
                },
            )
            .expect("runtime expression must resolve");
            let static_result = crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| {
                    crate::resolver::resolve_ref_value(
                        &static_value,
                        &ResolvedBindings::default(),
                        session,
                    )
                },
            )
            .expect("static expression must evaluate");

            assert_eq!(deferred_result, static_result);
        }
    }

    #[test]
    fn canonicalize_provider_configs_replaces_region_enum_with_canonical_api_value() {
        use crate::parser::ProviderConfig;
        use crate::schema::TypeIdentity;

        let providers = vec![
            ProviderConfig {
                name: "aws".to_string(),
                attributes: indexmap::indexmap! {
                    "region".to_string() => Value::Concrete(ConcreteValue::enum_identifier(
                        "aws.Region.ap_northeast_1",
                    )),
                },
                default_tags: indexmap::IndexMap::new(),
                source: None,
                version: None,
                revision: None,
                unresolved_attributes: indexmap::IndexMap::new(),
                binding: None,
            },
            ProviderConfig {
                name: "awscc".to_string(),
                attributes: indexmap::indexmap! {
                    "region".to_string() => Value::Concrete(ConcreteValue::enum_identifier(
                        "awscc.Region.ap_northeast_1",
                    )),
                },
                default_tags: indexmap::IndexMap::new(),
                source: None,
                version: None,
                revision: None,
                unresolved_attributes: indexmap::IndexMap::new(),
                binding: None,
            },
        ];

        let providers = canonicalize_provider_configs_with_schemas(&providers, &|_provider| {
            crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
                "region".to_string(),
                PatternAttributeType::enum_(
                    TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                    Some(vec!["ap-northeast-1".to_string()]),
                    vec![("ap-northeast-1".to_string(), "ap_northeast_1".to_string())],
                    None,
                    None,
                ),
            )]))
            .ok()
        })
        .unwrap();

        let api_values: Vec<_> = providers
            .as_slice()
            .iter()
            .map(
                |provider| match provider.attributes.get("region").unwrap() {
                    Value::Concrete(ConcreteValue::CanonicalEnum(c)) => c.api_value().to_string(),
                    other => panic!("expected CanonicalEnum, got {other:?}"),
                },
            )
            .collect();
        assert_eq!(api_values, ["ap-northeast-1", "ap-northeast-1"]);
    }

    #[test]
    fn provider_config_accepts_identity_or_configured_provider_namespace_only() {
        use crate::schema::{CompiledAttributeType as AttributeType, TypeIdentity};

        let attr_types = BTreeMap::from([(
            "region".to_string(),
            AttributeType::enum_(
                TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                Some(vec!["ap-northeast-1".to_string()]),
                vec![("ap-northeast-1".to_string(), "ap_northeast_1".to_string())],
                None,
                None,
            ),
        )]);
        let canonicalize = |provider_name: &str, raw: &str| {
            canonicalize_provider_config_attributes_with_types(
                provider_name,
                &IndexMap::from([(
                    "region".to_string(),
                    Value::Concrete(ConcreteValue::enum_identifier(raw)),
                )]),
                &attr_types,
            )
        };

        for raw in ["awscc.Region.ap_northeast_1", "aws.Region.ap_northeast_1"] {
            let accepted = canonicalize("awscc", raw)
                .unwrap_or_else(|error| panic!("{raw} must resolve: {error}"));
            assert!(matches!(
                accepted.get("region"),
                Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                    if value.identity().provider.as_deref() == Some("aws")
                        && value.api_value() == "ap-northeast-1"
            ));
        }

        for (provider_name, raw) in [
            ("awscc", "gcp.Region.ap_northeast_1"),
            ("aws", "awscc.Region.ap_northeast_1"),
        ] {
            let error = canonicalize(provider_name, raw)
                .expect_err("an unrelated namespace must not be stripped");
            assert!(error.to_string().contains(raw), "{error}");
        }
    }

    #[test]
    fn provider_config_alias_namespace_must_preserve_type_segments() {
        use crate::schema::{CompiledAttributeType as AttributeType, TypeIdentity};

        let attr_types = BTreeMap::from([(
            "state".to_string(),
            AttributeType::enum_(
                TypeIdentity::new(Some("origin"), ["compute", "Instance"], "State"),
                Some(vec!["running".to_string()]),
                Vec::new(),
                None,
                None,
            ),
        )]);
        let canonicalize = |raw: &str| {
            canonicalize_provider_config_attributes_with_types(
                "configured",
                &IndexMap::from([(
                    "state".to_string(),
                    Value::Concrete(ConcreteValue::enum_identifier(raw)),
                )]),
                &attr_types,
            )
        };

        let accepted = canonicalize("configured.compute.Instance.State.running")
            .expect("the configured provider qualifier may replace only the provider axis");
        assert!(matches!(
            accepted.get("state"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.identity().provider.as_deref() == Some("origin")
                    && value.identity().segments == ["compute", "Instance"]
                    && value.api_value() == "running"
        ));

        for raw in [
            "configured.compute.Other.State.running",
            "configured.compute.Instance.Mode.running",
        ] {
            let error = canonicalize(raw).expect_err("segments and type name must match exactly");
            assert!(error.to_string().contains(raw), "{error}");
        }
    }

    #[test]
    fn provider_config_never_retries_a_successful_first_resolution() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_from_validator = Arc::clone(&calls);
        let region = AttributeType::enum_(
            crate::schema::TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
            None,
            Vec::new(),
            Some(crate::schema::legacy_validator(move |_| {
                let call = calls_from_validator.fetch_add(1, Ordering::SeqCst);
                (call == 0)
                    .then_some(())
                    .ok_or_else(|| "validator was called twice".to_string())
            })),
            None,
        );

        crate::schema::RootlessType::new(region)
            .canonicalize_provider_config(
                "awscc",
                Value::Concrete(ConcreteValue::enum_identifier("aws.Region.tokyo")),
            )
            .expect("a successful first result must be returned without a fallback retry");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn provider_config_canonicalization_masks_secret_enum_errors() {
        use crate::schema::CompiledAttributeType as AttributeType;

        let plaintext = "provider-config-plaintext-must-not-leak";
        let secret = || {
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::enum_identifier(plaintext),
            ))))
        };

        let typed = crate::schema::RootlessType::new(AttributeType::string())
            .canonicalize_provider_config("test", secret())
            .expect_err("a typed provider config must reject the unresolved enum");
        assert!(!typed.to_string().contains(plaintext), "{typed}");

        let attributes = IndexMap::from([("unknown".to_string(), secret())]);
        let untyped = canonicalize_provider_config_attributes_with_types(
            "test",
            &attributes,
            &BTreeMap::new(),
        )
        .expect_err("an untyped provider config must reject the unresolved enum");
        let display = untyped.to_string();
        assert!(!display.contains(plaintext), "{display}");
        assert!(display.contains("secret"), "{display}");
    }

    #[test]
    fn provider_config_typed_canonicalization_is_total() {
        let error = crate::schema::RootlessType::new(AttributeType::string())
            .canonicalize_provider_config(
                "test",
                Value::Concrete(ConcreteValue::enum_identifier("test.Mode.invalid")),
            )
            .expect_err("a typed config value must not retain an enum identifier");

        assert!(
            error
                .to_string()
                .contains("unresolved enum identifier `test.Mode.invalid`")
        );
    }

    #[test]
    fn test_format_value_int() {
        let v = Value::Concrete(ConcreteValue::Int(42));
        assert_eq!(format_value(&v), "42");
    }

    #[test]
    fn test_format_value_float() {
        let v = Value::Concrete(ConcreteValue::Float(1.5));
        assert_eq!(format_value(&v), "1.5");
    }

    #[test]
    fn test_format_value_bool() {
        let v = Value::Concrete(ConcreteValue::Bool(true));
        assert_eq!(format_value(&v), "true");
    }

    #[test]
    fn test_format_value_list() {
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::Int(2)),
        ]));
        assert_eq!(format_value(&v), "[1, 2]");
    }

    #[test]
    fn test_format_value_resource_ref() {
        let v = Value::root_resource_ref("vpc", "id", vec![]);
        assert_eq!(format_value(&v), "vpc.id");
    }

    #[test]
    fn format_value_collapses_access_chain_rooted_at_unknown() {
        use crate::resource::{AccessMissPolicy, PathSegment, apply_postfix_access};

        let inner = apply_postfix_access(
            LoopValue::Deferred(LoopDeferredValue::Placeholder(LoopSlot::Value)),
            PathSegment::Field {
                name: "resource_record".to_string(),
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("unknown access remains deferred");
        let value = apply_postfix_access(
            inner,
            PathSegment::Field {
                name: "name".to_string(),
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("unknown access remains deferred");

        assert_eq!(format_value(&value), "(known after upstream apply)");
    }

    #[test]
    fn format_value_keeps_access_chain_on_expression_base() {
        use crate::resource::{AccessMissPolicy, PathSegment, Subscript, apply_postfix_access};

        let value = apply_postfix_access(
            LoopValue::Deferred(LoopDeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    LoopValue::Concrete(crate::resource::ConcreteValue::String("/".into())).into(),
                    LoopValue::Deferred(LoopDeferredValue::Placeholder(LoopSlot::Value)).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 1 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("computed access remains deferred");

        assert_eq!(
            format_value(&value),
            "split('/', (known after upstream apply))[1]"
        );
    }

    #[test]
    fn stale_loop_display_root_does_not_hide_the_substituted_access_cause() {
        use crate::resource::{
            AccessMissPolicy, ApplyTimeUnknown, ExpandedAccessPath as AccessPath, PathSegment,
            Subscript, apply_postfix_access,
        };

        let upstream = apply_postfix_access(
            Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
                path: AccessPath::root("up", "value"),
            })),
            PathSegment::Field {
                name: "nested".to_string(),
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("unknown access remains deferred")
        .with_access_display_root("item");
        assert_eq!(
            format_value(&upstream),
            "(known after upstream apply: up.value)"
        );
        assert!(
            !is_upstream_deferred_marker_value(&upstream),
            "a substituted access is an expression, not the original loop marker",
        );

        let function = apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String("/".into())).into(),
                    Value::Concrete(ConcreteValue::String("a/b".into())).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 0 },
            },
            0,
            AccessMissPolicy::Error,
        )
        .expect("computed access remains deferred")
        .with_access_display_root("item");
        assert_eq!(format_value(&function), "split('/', 'a/b')[0]");
        assert!(!is_upstream_deferred_marker_value(&function));
    }

    /// `Value::Deferred(DeferredValue::Unknown(UpstreamExport))` renders unquoted as
    /// `(known after upstream apply: <ref>)` via `format_value_with_key`.
    /// Stage 2 of RFC #2371 — the variant replaced the NUL-prefixed
    /// `Value::Concrete(ConcreteValue::String)` sentinel from #2367.
    #[test]
    fn test_format_value_unresolved_upstream() {
        use crate::resource::ExpandedAccessPath as AccessPath;
        let path = AccessPath::root_with_fields("network", "vpc", vec!["vpc_id".to_string()]);
        let v = Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
            path,
        }));
        assert_eq!(
            format_value(&v),
            "(known after upstream apply: network.vpc.vpc_id)"
        );
    }

    #[test]
    fn pending_reread_unknown_returns_err_in_value_to_json() {
        let v = Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::PendingReRead(
            crate::resource::PendingReRead::incomplete("mock partial create"),
        )));
        let err = value_to_json(&v).unwrap_err();
        assert!(
            matches!(
                err,
                SerializationError::UnknownNotAllowed {
                    ref reason,
                    context: SerializationContext::ValueToJson,
                } if matches!(reason.as_ref(), ApplyTimeUnknown::PendingReRead(_))
            ),
            "expected UnknownNotAllowed/PendingReRead/ValueToJson, got: {err:?}"
        );
    }

    #[test]
    fn test_format_value_resource_ref_with_field_path() {
        let v = Value::root_resource_ref("web", "network", vec!["vpc_id".to_string()]);
        assert_eq!(format_value(&v), "web.network.vpc_id");
    }

    #[test]
    fn test_value_to_json_resource_ref_with_field_path_returns_err() {
        let v = Value::root_resource_ref(
            "web",
            "output",
            vec!["network".to_string(), "vpc_id".to_string()],
        );
        let err = value_to_json(&v).unwrap_err();
        assert!(
            matches!(
                &err,
                SerializationError::UnresolvedResourceRef { path, .. }
                    if path == "web.output.network.vpc_id"
            ),
            "expected UnresolvedResourceRef/web.output.network.vpc_id, got: {err:?}"
        );
    }

    #[test]
    fn test_is_list_of_maps_true() {
        let mut map = IndexMap::new();
        map.insert(
            "key".to_string(),
            Value::Concrete(ConcreteValue::String("val".into())),
        );
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(map),
        )]));
        assert!(is_list_of_maps(&v));
    }

    #[test]
    fn test_is_list_of_maps_false_empty() {
        let v = Value::Concrete(ConcreteValue::List(vec![]));
        assert!(!is_list_of_maps(&v));
    }

    #[test]
    fn test_is_list_of_maps_false_not_maps() {
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Int(1),
        )]));
        assert!(!is_list_of_maps(&v));
    }

    #[test]
    fn test_is_list_of_maps_false_not_list() {
        let v = Value::Concrete(ConcreteValue::Int(1));
        assert!(!is_list_of_maps(&v));
    }

    #[test]
    fn test_value_to_json_secret_produces_hash() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        ))));
        let json = value_to_json(&v).unwrap();
        let hash = redacted_hash_from_persisted_json(&json);
        assert!(hash.verify(serde_json::to_vec(&serde_json::json!("my-password")).unwrap()));
    }

    #[test]
    fn test_value_to_json_secret_uses_random_salt() {
        let v1 = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        ))));
        let v2 = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        ))));
        let json1 = value_to_json(&v1).unwrap();
        let json2 = value_to_json(&v2).unwrap();
        assert_ne!(json1, json2);
    }

    #[test]
    fn test_value_to_json_secret_different_values_different_hashes() {
        let v1 = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("password-1".into()),
        ))));
        let v2 = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("password-2".into()),
        ))));
        let json1 = value_to_json(&v1).unwrap();
        let json2 = value_to_json(&v2).unwrap();
        assert_ne!(json1, json2);
    }

    #[test]
    fn test_format_value_secret() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        ))));
        assert_eq!(format_value(&v), "(secret)");
    }

    #[test]
    fn test_format_value_secret_in_map() {
        let mut map = IndexMap::new();
        map.insert(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        );
        map.insert(
            "SecretTag".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("my-password".into()),
            )))),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let formatted = format_value(&v);
        // Secret values inside maps should show as (secret), not the raw value
        assert!(
            formatted.contains("(secret)"),
            "Expected (secret) in map display, got: {}",
            formatted
        );
        assert!(
            !formatted.contains("my-password"),
            "Should not contain the secret value, got: {}",
            formatted
        );
    }

    #[test]
    fn test_value_to_json_secret_in_map() {
        let mut map = IndexMap::new();
        map.insert(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        );
        map.insert(
            "SecretTag".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("my-password".into()),
            )))),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let json = value_to_json(&v).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.get("Name").unwrap().as_str().unwrap(), "test");
        let secret_val = obj.get("SecretTag").expect("secret map value");
        assert!(
            redacted_hash_from_persisted_json(secret_val)
                .encoded_digest()
                .starts_with("$argon2id$")
        );
    }

    #[test]
    fn test_format_value_redacted_secret() {
        let v = redacted_secret("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789");
        assert_eq!(format_value(&v), "(secret)");
    }

    #[test]
    fn value_to_json_generates_independently_salted_digests() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        ))));
        let json1 = value_to_json(&v).unwrap();
        let json2 = value_to_json(&v).unwrap();
        assert_ne!(
            json1, json2,
            "equal plaintexts must not reveal equality through their digests"
        );
        let material = serde_json::to_vec(&serde_json::json!("my-password")).unwrap();
        for encoded in [json1, json2] {
            let hash = redacted_hash_from_persisted_json(&encoded);
            assert!(hash.verify(&material));
        }
    }

    #[test]
    fn generated_secret_digest_is_parameterized_argon2id_phc() {
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("phc-password".into()),
        ))));
        let encoded = value_to_json(&secret).expect("serialize secret");
        let phc = encoded[PERSISTED_SECRET_KEY]
            .as_str()
            .expect("structural secret digest");
        let parsed = argon2::PasswordHash::new(phc).expect("valid PHC string");

        assert_eq!(parsed.algorithm.as_str(), "argon2id");
        assert_eq!(parsed.version, Some(19));
        assert_eq!(
            parsed.params.get("m").unwrap().decimal().unwrap(),
            argon2::Params::DEFAULT_M_COST,
        );
        assert_eq!(
            parsed.params.get("t").unwrap().decimal().unwrap(),
            argon2::Params::DEFAULT_T_COST,
        );
        assert_eq!(
            parsed.params.get("p").unwrap().decimal().unwrap(),
            argon2::Params::DEFAULT_P_COST,
        );
        let mut salt = [0_u8; 64];
        assert_eq!(
            parsed
                .salt
                .expect("PHC salt")
                .decode_b64(&mut salt)
                .expect("decode PHC salt")
                .len(),
            16,
        );
        assert_eq!(parsed.hash.as_ref().expect("PHC output").len(), 32);
    }

    #[test]
    fn persisted_secret_digest_rejects_non_carina_argon2_parameters() {
        use argon2::password_hash::{PasswordHasher, SaltString};

        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("bounded-argon2".into()),
        ))));
        let encoded = value_to_json(&secret).expect("serialize secret");
        let digest = encoded[PERSISTED_SECRET_KEY]
            .as_str()
            .expect("structural secret digest");
        let mut invalid = vec![
            (
                "memory cost",
                digest.replacen(
                    &format!("m={}", argon2::Params::DEFAULT_M_COST),
                    "m=1048576",
                    1,
                ),
            ),
            (
                "time cost",
                digest.replacen(&format!("t={}", argon2::Params::DEFAULT_T_COST), "t=3", 1),
            ),
            (
                "parallelism",
                digest.replacen(&format!("p={}", argon2::Params::DEFAULT_P_COST), "p=2", 1),
            ),
            ("version", digest.replacen("v=19", "v=16", 1)),
        ];
        for (label, salt, output_len) in [
            ("salt length", b"short-salt".as_slice(), 32),
            ("output length", b"0123456789abcdef".as_slice(), 16),
        ] {
            let params = argon2::Params::new(
                argon2::Params::DEFAULT_M_COST,
                argon2::Params::DEFAULT_T_COST,
                argon2::Params::DEFAULT_P_COST,
                Some(output_len),
            )
            .expect("test Argon2 parameters");
            let hasher =
                argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
            let salt = SaltString::encode_b64(salt).expect("test salt");
            let phc = hasher
                .hash_password(b"bounded-argon2", &salt)
                .expect("hash test password");
            invalid.push((label, phc.to_string()));
        }

        for (label, digest) in invalid {
            assert!(
                RedactedSecretHash::from_encoded_digest(&digest).is_none(),
                "state must reject a non-Carina Argon2 {label}",
            );
        }
    }

    #[test]
    fn unresolved_secret_with_null_provider_value_returns_an_error() {
        let desired = Value::Deferred(DeferredValue::Secret(Box::new(Value::root_resource_ref(
            "upstream",
            "value",
            Vec::new(),
        ))));

        let provider = PersistedValue::from_serialized_json(serde_json::Value::Null).unwrap();
        let error = merge_secrets_into_provider_json(&desired, &provider, None)
            .expect_err("top-level null secret material must be rejected without panicking");
        assert!(matches!(error, SerializationError::SecretNotAllowed { .. }));
    }

    #[test]
    fn non_structural_expression_containing_secret_masks_whole_observed_value() {
        let desired = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::root_resource_ref("resource", "comment", Vec::new())),
            default: Box::new(Value::Deferred(DeferredValue::Secret(Box::new(
                Value::Concrete(ConcreteValue::String("fallback-secret".into())),
            )))),
        });
        let plaintext = "provider-returned-secret";
        let provider = PersistedValue::encode_provider_value(
            &Value::Concrete(ConcreteValue::String(plaintext.into())),
            None,
        )
        .expect("provider plaintext is persistable");

        for masked in [
            merge_secrets_into_provider_json(&desired, &provider, None)
                .expect("provider writeback masks the whole deferred expression"),
            merge_secrets_into_observation_json(&desired, &provider, None)
                .expect("observation writeback masks the whole deferred expression"),
        ] {
            assert!(masked.as_json().get(PERSISTED_SECRET_KEY).is_some());
            assert!(!masked.as_json().to_string().contains(plaintext));
        }
    }

    #[test]
    fn provider_side_secret_hash_reuses_desired_hash_for_persisted_enum_and_dollar_map() {
        let values = [
            Value::Concrete(ConcreteValue::CanonicalEnum(
                crate::resource::CanonicalEnumValue::new_for_test(
                    crate::schema::TypeIdentity::new(None::<String>, ["widget"], "Mode"),
                    "active",
                ),
            )),
            Value::Concrete(ConcreteValue::CanonicalEnum(
                crate::resource::CanonicalEnumValue::new_for_test(
                    crate::schema::TypeIdentity::new(Some("test"), ["widget"], "Mode"),
                    "active",
                ),
            )),
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "$x".to_string(),
                Value::Concrete(ConcreteValue::String("value".into())),
            )]))),
        ];

        for inner in values {
            let desired = Value::Deferred(DeferredValue::Secret(Box::new(inner.clone())));
            let previous = PersistedValue::encode_desired_value(&desired, None)
                .expect("desired secret is persistable");
            let provider = PersistedValue::encode_provider_value(&inner, None)
                .expect("provider value is persistable");

            let refreshed =
                crate::provider::hash_provider_json_as_secret(&provider, Some(&previous))
                    .expect("provider secret can be hashed");
            assert_eq!(
                refreshed.as_json(),
                previous.as_json(),
                "refresh must reuse the desired-side hash for {inner:?}",
            );

            let merged = merge_secrets_into_provider_json(&desired, &provider, Some(&previous))
                .expect("resolved desired secret can be merged");
            assert_eq!(
                merged.as_json(),
                previous.as_json(),
                "resolved merge must reuse the desired-side hash for {inner:?}",
            );
        }
    }

    #[test]
    fn provider_secret_material_treats_nested_marker_strings_as_plaintext() {
        let nested_secret = PersistedValue::encode_provider_value(
            &Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("nested-secret".into()),
            )))),
            None,
        )
        .expect("nested secret is persistable");
        let nested_marker = format!(
            "_secret:{}",
            nested_secret.as_json()[PERSISTED_SECRET_KEY]
                .as_str()
                .expect("structural secret digest")
        );
        let provider_plaintext = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "a".to_string(),
            Value::Concrete(ConcreteValue::String(nested_marker.into())),
        )])));
        let previous = PersistedValue::encode_desired_value(
            &Value::Deferred(DeferredValue::Secret(Box::new(provider_plaintext.clone()))),
            None,
        )
        .expect("top-level secret is persistable");
        let provider = PersistedValue::encode_provider_value(&provider_plaintext, None)
            .expect("provider plaintext is persistable");

        let refreshed = crate::provider::hash_provider_json_as_secret(&provider, Some(&previous))
            .expect("nested marker-shaped plaintext can be hashed");
        assert_eq!(refreshed.as_json(), previous.as_json());

        let unresolved = Value::Deferred(DeferredValue::Secret(Box::new(
            Value::root_resource_ref("upstream", "value", Vec::new()),
        )));
        let merged = merge_secrets_into_provider_json(&unresolved, &provider, Some(&previous))
            .expect("provider material with a nested marker-shaped string can be hashed");
        assert_eq!(merged.as_json(), previous.as_json());
    }

    #[test]
    fn equal_nested_secrets_get_distinct_digests() {
        let secret = || {
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("same".into()),
            ))))
        };
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("left".to_string(), secret()),
            ("right".to_string(), secret()),
        ])));
        let json = value_to_json(&value).unwrap();
        assert_ne!(json["left"], json["right"]);
    }

    #[test]
    fn value_to_json_reuses_matching_previous_digest() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        ))));
        let json1 = PersistedValue::encode_desired_value(&v, None).unwrap();
        let json2 = PersistedValue::encode_desired_value(&v, Some(&json1)).unwrap();
        assert_eq!(
            json1.as_json(),
            json2.as_json(),
            "an unchanged secret must reuse its previous digest byte-for-byte"
        );
    }

    #[test]
    fn value_to_json_replaces_previous_digest_when_plaintext_changes() {
        let old = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("old-password".into()),
        ))));
        let new = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("new-password".into()),
        ))));
        let previous = PersistedValue::encode_desired_value(&old, None).unwrap();
        let replacement = PersistedValue::encode_desired_value(&new, Some(&previous)).unwrap();
        assert_ne!(previous.as_json(), replacement.as_json());
    }

    #[test]
    fn test_redact_secrets_in_value_replaces_secret() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        ))));
        let redacted = redact_secrets_in_value(&v).unwrap();
        // Saved plans retain the hash in a dedicated, non-provider value.
        match &redacted {
            Value::Deferred(DeferredValue::RedactedSecret { hash }) => {
                assert!(hash.encoded_digest().starts_with("$argon2id$"));
            }
            _ => panic!(
                "Expected DeferredValue::RedactedSecret after redaction, got: {:?}",
                redacted
            ),
        }
    }

    #[test]
    fn integer_arithmetic_secret_operand_redacts_and_rehydrates_recursively() {
        let candidate = Value::Deferred(DeferredValue::IntegerArithmetic(
            crate::integer_arithmetic::ValidatedIntegerExpr::new(
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::Int(42),
                )))),
                crate::integer_arithmetic::IntegerOp::Add,
                Value::Concrete(ConcreteValue::Int(1)),
                crate::integer_arithmetic::IntegerOperatorSpan::synthetic(),
            )
            .expect("a secret Int is a valid arithmetic operand"),
        ));

        let redacted = redact_secrets_in_value(&candidate).expect("redact arithmetic operand");
        assert!(contains_redacted_secret(&redacted));
        assert!(
            !serde_json::to_string(&redacted)
                .unwrap()
                .contains("\"Int\":42")
        );
        assert_eq!(
            rehydrate_redacted_secrets(&redacted, &candidate)
                .expect("matching arithmetic secret rehydrates"),
            candidate,
        );
    }

    #[test]
    fn deferred_secret_interpolation_is_one_placeholder() {
        let candidate = Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
            DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("DEFPLAIN-".into()),
                InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::ExpandedAccessPath::root("r", "identifier"),
                })),
                InterpolationPart::Literal("-TAIL".into()),
            ]),
        ))));
        let redacted = redact_secrets_in_value(&candidate).expect("redact template");
        assert!(
            matches!(
                redacted,
                Value::Deferred(DeferredValue::RedactedSecret { .. })
            ),
            "a secret wrapper must produce one placeholder: {redacted:?}",
        );
        assert_eq!(
            rehydrate_redacted_secrets(&redacted, &candidate)
                .expect("matching canonical template hash"),
            candidate,
        );

        let changed = Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
            DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("CHANGED-".into()),
                InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::ExpandedAccessPath::root("r", "identifier"),
                })),
                InterpolationPart::Literal("-TAIL".into()),
            ]),
        ))));
        assert!(matches!(
            rehydrate_redacted_secrets(&redacted, &changed),
            Err(SecretRehydrationError::HashMismatch)
        ));
    }

    fn upper_with_argument(argument: Value) -> Value {
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![argument.into()],
            source_line: (0).into(),
        })
    }

    fn unavailable_upper_with_argument(argument: Value) -> Value {
        Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![argument.into()],
            reason: crate::resource::UnavailableReason::from_builtin_error(
                "upper input could not be evaluated".to_string(),
            ),
            source_line: (0).into(),
        })
    }

    fn upper_with_secret_hash_material(hash_material: Value) -> Value {
        upper_with_argument(Value::Deferred(DeferredValue::Secret(Box::new(
            hash_material,
        ))))
    }

    fn deferred_interpolation_hash_material(secret: &str) -> Value {
        Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::root_resource_ref("origin", "identifier", vec![])),
            InterpolationPart::Literal(format!("-{secret}").into()),
        ]))
    }

    fn deferred_coalesce_hash_material(default: Value) -> Value {
        Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::root_resource_ref("origin", "comment", vec![])),
            default: Box::new(default),
        })
    }

    fn pending_env_call() -> Value {
        Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![
                Value::Concrete(ConcreteValue::String("SAVED_PLAN_SECRET_3974".into())).into(),
            ],
            source_line: (0).into(),
        })
    }

    fn unavailable_env_evaluation() -> Value {
        Value::Deferred(DeferredValue::Unavailable {
            name: crate::builtins::BuiltinFunction::env,
            args: vec![
                Value::Concrete(ConcreteValue::String("SAVED_PLAN_SECRET_3974".into())).into(),
            ],
            reason: crate::resource::UnavailableReason::from_builtin_error(
                "environment variable 'SAVED_PLAN_SECRET_3974' is not set".to_string(),
            ),
            source_line: (0).into(),
        })
    }

    #[test]
    fn secret_rehydration_verifies_builtin_wrapped_deferred_leaves() {
        for hash_material in [
            deferred_interpolation_hash_material("same-secret"),
            deferred_coalesce_hash_material(Value::Concrete(ConcreteValue::String(
                "same-secret".into(),
            ))),
        ] {
            let candidate = upper_with_secret_hash_material(hash_material);
            let saved = redact_secrets_in_value(&candidate).expect("redact builtin secret leaf");

            assert_eq!(
                rehydrate_redacted_secrets(&saved, &candidate)
                    .expect("matching deferred secret leaf must rehydrate"),
                candidate,
            );
        }
    }

    #[test]
    fn secret_rehydration_allows_non_secret_apply_time_unknowns() {
        let candidate =
            upper_with_secret_hash_material(Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                    ApplyTimeUnknown::UpstreamExport {
                        path: crate::resource::ExpandedAccessPath::root("upstream", "identifier"),
                    },
                ))),
                InterpolationPart::Literal("-same-secret".into()),
            ])));
        let saved = redact_secrets_in_value(&candidate).expect("redact builtin secret leaf");

        rehydrate_redacted_secrets(&saved, &candidate)
            .expect("apply-time unknown is stable secret hash material");
    }

    #[test]
    fn secret_rehydration_treats_pending_function_calls_as_hash_material() {
        let candidate = upper_with_secret_hash_material(pending_env_call());
        let saved = redact_secrets_in_value(&candidate).expect("redact pending function call");

        assert_eq!(
            rehydrate_redacted_secrets(&saved, &candidate)
                .expect("a pending call is not evidence that evaluation failed"),
            candidate,
        );
    }

    #[test]
    fn secret_rehydration_accepts_matching_unavailable_function_call_shape() {
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("same-secret".into()),
        ))));
        let planned = upper_with_argument(secret.clone());
        let saved = redact_secrets_in_value(&planned).expect("redact function argument");
        let unavailable = unavailable_upper_with_argument(secret);

        assert_eq!(
            rehydrate_redacted_secrets(&saved, &unavailable)
                .expect("an unavailable candidate retains the built-in call shape"),
            planned,
        );
    }

    #[test]
    fn unavailable_evaluation_serializes_and_hashes_like_its_function_call() {
        let unavailable = unavailable_env_evaluation();
        let pending = pending_env_call();

        assert_eq!(
            serde_json::to_value(&unavailable).expect("serialize unavailable call"),
            serde_json::to_value(&pending).expect("serialize pending call"),
        );
        let decoded: Value = serde_json::from_value(
            serde_json::to_value(&unavailable).expect("serialize unavailable call for round trip"),
        )
        .expect("deserialize unavailable call through persisted representation");
        assert!(matches!(
            decoded,
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::env,
                ..
            })
        ));
        for value in [&unavailable, &pending] {
            assert!(matches!(
                PersistedValue::encode_desired_value(value, None),
                Err(SerializationError::UnresolvedFunctionCall { name, .. }) if name == "env"
            ));
        }
        assert_eq!(
            canonical_secret_hash_material(&unavailable).expect("hash unavailable call"),
            canonical_secret_hash_material(&pending).expect("hash pending call"),
        );
        assert_eq!(format_value(&unavailable), "env('SAVED_PLAN_SECRET_3974')");
    }

    #[test]
    fn secret_rehydration_reports_hash_mismatch_at_builtin_wrapped_deferred_leaves() {
        for (planned_material, changed_material) in [
            (
                deferred_interpolation_hash_material("planned-secret"),
                deferred_interpolation_hash_material("changed-secret"),
            ),
            (
                deferred_coalesce_hash_material(Value::Concrete(ConcreteValue::String(
                    "planned-secret".into(),
                ))),
                deferred_coalesce_hash_material(Value::Concrete(ConcreteValue::String(
                    "changed-secret".into(),
                ))),
            ),
        ] {
            let planned = upper_with_secret_hash_material(planned_material);
            let saved = redact_secrets_in_value(&planned).expect("redact builtin secret leaf");
            let changed = upper_with_secret_hash_material(changed_material);

            assert!(matches!(
                rehydrate_redacted_secrets(&saved, &changed),
                Err(SecretRehydrationError::HashMismatch)
            ));
        }
    }

    #[test]
    fn secret_rehydration_reports_unavailable_at_builtin_wrapped_secret_leaves() {
        let planned_secret = Value::Concrete(ConcreteValue::String("planned-secret".into()));
        for (planned_material, unavailable_argument) in [
            (
                deferred_interpolation_hash_material("planned-secret"),
                Value::Deferred(DeferredValue::Interpolation(vec![
                    InterpolationPart::Expr(Value::root_resource_ref(
                        "origin",
                        "identifier",
                        vec![],
                    )),
                    InterpolationPart::Literal("-".into()),
                    InterpolationPart::Expr(Value::Deferred(DeferredValue::Secret(Box::new(
                        unavailable_env_evaluation(),
                    )))),
                ])),
            ),
            (
                deferred_coalesce_hash_material(planned_secret),
                deferred_coalesce_hash_material(Value::Deferred(DeferredValue::Secret(Box::new(
                    unavailable_env_evaluation(),
                )))),
            ),
        ] {
            let planned = upper_with_secret_hash_material(planned_material);
            let saved = redact_secrets_in_value(&planned).expect("redact builtin secret leaf");
            let unavailable = upper_with_argument(unavailable_argument);

            assert!(matches!(
                rehydrate_redacted_secrets(&saved, &unavailable),
                Err(SecretRehydrationError::EvaluationUnavailable)
            ));
        }
    }

    #[test]
    fn secret_rehydration_rejects_unavailable_in_an_unselected_candidate_subtree() {
        let planned_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("planned-secret".into()),
        ))));
        let planned = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("verified".to_string(), planned_secret.clone()),
            (
                "ordinary".to_string(),
                Value::Concrete(ConcreteValue::String("planned-ordinary".into())),
            ),
        ])));
        let saved = redact_secrets_in_value(&planned).expect("redact planned attribute");
        let unavailable_candidate = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("verified".to_string(), planned_secret),
            (
                "ordinary".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
                    DeferredValue::Interpolation(vec![
                        InterpolationPart::Expr(unavailable_env_evaluation()),
                        InterpolationPart::Literal("-suffix".into()),
                    ]),
                )))),
            ),
        ])));

        assert!(matches!(
            rehydrate_redacted_secrets(&saved, &unavailable_candidate),
            Err(SecretRehydrationError::EvaluationUnavailable)
        ));
    }

    fn resource_with_comment_secret(identity: &str, plaintext: &str) -> crate::resource::Resource {
        let mut resource = crate::resource::Resource::new(
            "mock.test.resource",
            crate::resource::ResourceIdentity::new(identity),
        );
        resource.set_attr(
            "comment",
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String(plaintext.into()),
            )))),
        );
        resource
    }

    fn resource_comment_placeholder_hash(
        resource: &crate::resource::Resource,
    ) -> &RedactedSecretHash {
        match resource.attributes.get("comment") {
            Some(Value::Deferred(DeferredValue::RedactedSecret { hash })) => hash,
            other => panic!("expected comment placeholder, got {other:?}"),
        }
    }

    #[test]
    fn saved_plan_equal_plaintexts_get_distinct_random_salts() {
        let first = redact_secrets_in_resource(&resource_with_comment_secret("first", "PLAIN"))
            .expect("redact first resource");
        let second = redact_secrets_in_resource(&resource_with_comment_secret("second", "PLAIN"))
            .expect("redact second resource");

        assert_ne!(
            resource_comment_placeholder_hash(&first),
            resource_comment_placeholder_hash(&second),
            "equal secrets must not reveal equality in a saved plan",
        );
    }

    #[test]
    fn separate_saved_plan_placeholders_get_distinct_random_salts() {
        let inner = Value::Concrete(ConcreteValue::String("SAME_IMPORT_SECRET".into()));
        let material = canonical_secret_hash_material(&inner).expect("canonical hash material");

        assert_ne!(
            RedactedSecretHash::reuse_or_generate(&material, None),
            RedactedSecretHash::reuse_or_generate(&material, None),
            "independent placeholders must not reveal equal plaintexts",
        );
    }

    #[test]
    fn saved_plan_placeholder_reuses_matching_persisted_digest() {
        let resource = resource_with_comment_secret("same-resource", "PLAIN");
        let state_value = value_to_json(resource.attributes.get("comment").expect("comment"))
            .expect("serialize state secret");
        let state_hash = redacted_hash_from_persisted_json(&state_value);
        let persisted =
            crate::provider::PersistedAttributes::from_persisted_values(HashMap::from([(
                "comment".to_string(),
                PersistedValue::from_serialized_json(state_value)
                    .expect("state value is a persisted structural secret"),
            )]))
            .expect("persisted attributes");
        let redacted = redact_secrets_in_resource_reusing_persisted_hashes(&resource, &persisted)
            .expect("redact resource");

        assert_eq!(resource_comment_placeholder_hash(&redacted), &state_hash);
    }

    #[test]
    fn persisted_dollar_key_secret_paths_redact_mask_and_reuse_hashes() {
        fn tree(leaf: impl Fn(&str) -> Value) -> Value {
            Value::Concrete(ConcreteValue::Map(IndexMap::from([
                ("$token".to_string(), leaf("TOP_PLAINTEXT")),
                (
                    "list".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                        ConcreteValue::Map(IndexMap::from([(
                            "$token".to_string(),
                            leaf("LIST_PLAINTEXT"),
                        )])),
                    )])),
                ),
                (
                    "nested".to_string(),
                    Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                        "$map".to_string(),
                        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                            "$token".to_string(),
                            leaf("MAP_PLAINTEXT"),
                        )]))),
                    )]))),
                ),
            ])))
        }

        let secret_tree = tree(|plaintext| {
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String(plaintext.into()),
            ))))
        });
        let plaintext_tree =
            tree(|plaintext| Value::Concrete(ConcreteValue::String(plaintext.into())));
        let persisted_value = PersistedValue::encode_desired_value(&secret_tree, None)
            .expect("secret tree is persistable");
        let persisted = crate::provider::PersistedAttributes::from_persisted_values(HashMap::from(
            [("settings".to_string(), persisted_value.clone())],
        ))
        .expect("persisted attributes are valid");
        let expected_redacted = persisted_value.decode().expect("non-null secret tree");

        let resource = crate::resource::Resource::new("mock.test.resource", "dollar-keys")
            .with_attribute("settings", secret_tree);
        let saved = redact_secrets_in_resource_reusing_persisted_hashes(&resource, &persisted)
            .expect("saved-plan redaction succeeds");
        assert_eq!(saved.attributes.get("settings"), Some(&expected_redacted));

        let inventory =
            crate::provider::PersistedAttributes::from_saved_plan_attributes(&saved.attributes);
        assert_eq!(
            inventory
                .encoded_value("settings")
                .expect("saved-plan inventory contains the secret tree")
                .as_json(),
            persisted_value.as_json(),
            "saved-plan inventory must preserve escaped-key hashes",
        );

        let state = make_state(vec![("settings", plaintext_tree.clone())]);
        let redacted_state = redact_secrets_in_state_using_persisted(&state, &inventory)
            .expect("historical provider state redaction succeeds");
        assert_eq!(
            redacted_state.attributes.get("settings"),
            Some(&expected_redacted),
            "persisted secret tags must classify plaintext below escaped keys",
        );

        let masked =
            mask_plaintext_using_persisted_attribute(&plaintext_tree, &inventory, "settings");
        let rendered = format_value(&masked);
        for plaintext in ["TOP_PLAINTEXT", "LIST_PLAINTEXT", "MAP_PLAINTEXT"] {
            assert!(
                !rendered.contains(plaintext),
                "display leaked {plaintext}: {rendered}",
            );
        }
        assert_eq!(rendered.matches("(secret)").count(), 3);
    }

    #[test]
    fn effect_redaction_uses_effect_values_to_classify_nested_secrets() {
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("MAP_SECRET".into()),
        ))));
        let reusable_secret = redact_secrets_in_value(&secret).expect("redact reusable secret");
        let resolved_reference_secret =
            redact_secrets_in_value(&Value::Deferred(DeferredValue::Secret(Box::new(
                Value::Concrete(ConcreteValue::String("RESOLVED_REFERENCE_SECRET".into())),
            ))))
            .expect("redact resolved reference secret");
        let reference = Value::root_resource_ref("producer", "comment", vec![]);

        let resource = crate::resource::Resource::new("mock.test.resource", "consumer")
            .with_attribute(
                "settings",
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    ("secret".to_string(), secret),
                    ("reference".to_string(), reference.clone()),
                ]))),
            );
        let guide = crate::resource::Resource::new("mock.test.resource", "consumer")
            .with_attribute(
                "settings",
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    ("secret".to_string(), reusable_secret.clone()),
                    ("reference".to_string(), resolved_reference_secret),
                ]))),
            );

        let redacted = redact_resource_using_redacted_shape(&resource, &guide)
            .expect("effect resource redaction");
        let Some(Value::Concrete(ConcreteValue::Map(settings))) =
            redacted.attributes.get("settings")
        else {
            panic!("expected redacted settings map")
        };
        assert_eq!(
            settings.get("secret"),
            Some(&reusable_secret),
            "an effect-side secret must reuse the planned resource's digest",
        );
        assert_eq!(
            settings.get("reference"),
            Some(&reference),
            "a planned secret must not turn an effect-side ResourceRef into a placeholder",
        );
    }

    #[test]
    fn saved_plan_rehydration_allows_resource_move() {
        let mut redacted =
            redact_secrets_in_resource(&resource_with_comment_secret("planned", "PLAIN"))
                .expect("redact planned resource");
        redacted.id =
            crate::resource::ResourceId::with_identity("mock.test.resource", "different-resource");
        let evaluated = resource_with_comment_secret("different-resource", "PLAIN");

        let rehydrated = rehydrate_resource_secrets(&redacted, &evaluated)
            .expect("embedded digest verification must be independent of resource identity");
        assert_eq!(rehydrated, evaluated);
    }

    #[test]
    fn saved_plan_rehydration_needs_no_position_context() {
        let evaluated = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("PLAIN".into()),
        ))));
        let redacted = redact_secrets_in_value(&evaluated).expect("redact secret");

        let rehydrated = rehydrate_redacted_secrets(&redacted, &evaluated)
            .expect("embedded digest verification needs no external context");
        assert_eq!(rehydrated, evaluated);
    }

    #[test]
    fn saved_plan_redaction_preserves_canonical_update_from_without_secrets() {
        use crate::effect::Effect;
        use crate::plan::Plan;
        use crate::resource::{ExpandedResource as Resource, ResolvedResource, ResourceId, State};

        let id = ResourceId::with_identity("mock.test.resource", "canonical");
        let canonical_from = State::existing(
            id.clone(),
            HashMap::from([(
                "items".to_string(),
                Value::Concrete(ConcreteValue::StringList(vec!["one".into(), "two".into()])),
            )]),
        );
        let historical_guide = State::existing(
            id.clone(),
            HashMap::from([(
                "items".to_string(),
                Value::Concrete(ConcreteValue::List(vec![
                    Value::Concrete(ConcreteValue::String("one".into())),
                    Value::Concrete(ConcreteValue::String("two".into())),
                ])),
            )]),
        );
        let mut to = Resource::new("mock.test.resource", "canonical");
        to.set_attr(
            "items",
            Value::Concrete(ConcreteValue::StringList(vec![
                "one".into(),
                "three".into(),
            ])),
        );
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                canonical_from.id.identity_display().to_string(),
            ),
            from: Box::new(canonical_from.clone()),
            to: ResolvedResource::new(to.clone().with_dependency_phase(())),
            changed_attributes: vec!["items".to_string()],
        });

        let redacted = redact_secrets_in_plan_with_desired_and_historical(
            &plan,
            &HashMap::from([(id.clone(), to)]),
            &HashMap::from([(id, historical_guide)]),
        )
        .expect("placeholder-free update redaction");
        let Effect::Update { from, .. } = &redacted.effects()[0] else {
            panic!("expected update")
        };
        assert_eq!(
            from.as_ref(),
            &canonical_from,
            "saved-plan redaction must preserve the live plan's canonical Update.from",
        );
    }

    #[test]
    fn semantic_redaction_handles_unevaluated_decrypt_call() {
        let plaintext = Value::Concrete(ConcreteValue::String(
            "decrypted-plaintext-must-not-leak".into(),
        ));
        let source = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::decrypt,
            args: vec![Value::Concrete(ConcreteValue::String("ciphertext".into())).into()],
            source_line: (0).into(),
        });
        let evaluated = Value::Deferred(DeferredValue::Secret(Box::new(plaintext)));

        let redacted = redact_secrets_using_evaluated(&source, &evaluated)
            .expect("semantic redaction succeeds");
        assert!(matches!(
            redacted,
            Value::Deferred(DeferredValue::RedactedSecret { .. })
        ));
        let serialized = serde_json::to_string(&redacted).expect("serialize placeholder");
        assert!(!serialized.contains("decrypted-plaintext-must-not-leak"));

        let rehydrated = rehydrate_redacted_secrets(&redacted, &evaluated)
            .expect("unchanged decrypted secret rehydrates");
        assert_eq!(rehydrated, evaluated);
    }

    #[test]
    fn raw_secret_cannot_cross_serde_boundary() {
        let plaintext = "SERDE_SECRET_MUST_NOT_LEAK_3861";
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String(plaintext.into()),
        ))));

        let error = serde_json::to_string(&secret)
            .expect_err("raw DeferredValue::Secret must fail closed at serde boundaries");
        assert!(
            error.to_string().contains("secret") && !error.to_string().contains(plaintext),
            "serialization error must explain the rejected secret without plaintext: {error}",
        );
    }

    #[test]
    fn secret_rehydration_ignores_changed_non_secret_siblings() {
        let saved_source = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "password".to_string(),
                Value::Concrete(ConcreteValue::String("plan-secret".into())),
            ),
            (
                "ordinary".to_string(),
                Value::Concrete(ConcreteValue::String("saved-value".into())),
            ),
        ])));
        let evaluated_at_plan = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "password".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("plan-secret".into()),
                )))),
            ),
            (
                "ordinary".to_string(),
                Value::Concrete(ConcreteValue::String("saved-value".into())),
            ),
        ])));
        let redacted = redact_secrets_using_evaluated(&saved_source, &evaluated_at_plan)
            .expect("redact nested secret");

        // Re-evaluation is authoritative only for secret leaves. A
        // non-secret sibling may be removed after planning without changing
        // which plaintext the saved plan is allowed to restore.
        let evaluated_at_apply = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("plan-secret".into()),
            )))),
        )])));
        let rehydrated = rehydrate_redacted_secrets(&redacted, &evaluated_at_apply)
            .expect("unchanged secret rehydrates despite unrelated config edits");

        let Value::Concrete(ConcreteValue::Map(values)) = rehydrated else {
            panic!("expected map");
        };
        assert!(matches!(
            values.get("password"),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));
        assert_eq!(
            values.get("ordinary"),
            Some(&Value::Concrete(ConcreteValue::String(
                "saved-value".into()
            )))
        );
    }

    #[test]
    fn contains_secret_walks_interpolations_and_function_arguments() {
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("sensitive".into()),
        ))));
        let interpolation = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(secret),
        ]));
        let function_call = Value::Deferred(DeferredValue::FunctionCall {
            name: crate::builtins::BuiltinFunction::upper,
            args: vec![interpolation.into()],
            source_line: (0).into(),
        });

        assert!(contains_secret(&function_call));
        assert!(contains_unredacted_secret(&function_call));

        let persisted = Value::Deferred(DeferredValue::RedactedSecret {
            hash: RedactedSecretHash::generate(b"persisted"),
        });
        assert!(contains_secret(&persisted));
        assert!(!contains_unredacted_secret(&persisted));
    }

    #[test]
    fn secret_walkers_visit_both_coalesce_operands() {
        let secret = || {
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("sensitive".into()),
            ))))
        };

        for coalesce in [
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(secret()),
                default: Box::new(Value::Concrete(ConcreteValue::String("plain".into()))),
            }),
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::Concrete(ConcreteValue::String("plain".into()))),
                default: Box::new(secret()),
            }),
        ] {
            assert!(contains_secret(&coalesce));
            assert!(contains_unredacted_secret(&coalesce));
            let redacted = redact_secrets_in_value(&coalesce).expect("redact coalesce secret");
            assert!(contains_redacted_secret(&redacted));
            assert!(
                !serde_json::to_string(&redacted)
                    .expect("serialize redacted coalesce")
                    .contains("sensitive")
            );
        }
    }

    #[test]
    fn fail_closed_mask_promotes_plaintext_for_non_structural_secret_shape() {
        let plaintext = Value::Concrete(ConcreteValue::String("selected-secret".into()));
        let shape = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::root_resource_ref("producer", "comment", vec![])),
            default: Box::new(Value::Deferred(DeferredValue::Secret(Box::new(
                Value::Concrete(ConcreteValue::String("fallback".into())),
            )))),
        });

        assert_eq!(
            mask_plaintext_using_secret_shape_fail_closed(&plaintext, &shape),
            Value::Deferred(DeferredValue::Secret(Box::new(plaintext))),
            "an unresolved non-structural shape containing a secret must never leave provider plaintext unwrapped",
        );
    }

    #[test]
    fn display_mask_projects_secrets_into_a_string_list() {
        let plaintext = Value::Concrete(ConcreteValue::StringList(vec![
            "public".into(),
            "sensitive".into(),
        ]));
        let shape = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("public".into())),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("sensitive".into()),
            )))),
        ]));

        assert_eq!(
            mask_plaintext_using_secret_shape(&plaintext, &shape),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("public".into())),
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("sensitive".into()),
                )))),
            ])),
        );
    }

    #[test]
    fn fail_closed_mask_isolates_an_unprojectable_nested_secret_shape() {
        let public = Value::Concrete(ConcreteValue::String("public".into()));
        let plain = Value::Concrete(ConcreteValue::String("plain".into()));
        let plaintext = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("selected".to_string(), public.clone()),
            ("sibling".to_string(), plain.clone()),
        ])));
        let shape = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "selected".to_string(),
                Value::Deferred(DeferredValue::Coalesce {
                    value: Box::new(Value::root_resource_ref("producer", "identifier", vec![])),
                    default: Box::new(Value::Deferred(DeferredValue::Secret(Box::new(
                        Value::Concrete(ConcreteValue::String("fallback".into())),
                    )))),
                }),
            ),
            ("sibling".to_string(), plain.clone()),
        ])));

        let masked = mask_plaintext_using_secret_shape_fail_closed(&plaintext, &shape);
        let Value::Concrete(ConcreteValue::Map(values)) = masked else {
            panic!("fail-closed masking must preserve the map container");
        };
        assert_eq!(
            values["selected"],
            Value::Deferred(DeferredValue::Secret(Box::new(public))),
        );
        assert_eq!(values["sibling"], plain);
    }

    #[test]
    fn fail_closed_mask_does_not_taint_siblings_of_an_absent_secret_position() {
        let plain = Value::Concrete(ConcreteValue::String("plain".into()));
        let plaintext = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "sibling".to_string(),
            plain.clone(),
        )])));
        let shape = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "absent".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("not-returned".into()),
            )))),
        )])));

        assert_eq!(
            mask_plaintext_using_secret_shape_fail_closed(&plaintext, &shape),
            plaintext,
        );
    }

    #[test]
    fn resolved_public_branch_overrides_unselected_authored_secret_fallback() {
        let applied = Value::Concrete(ConcreteValue::String("selected-public".into()));
        let authored = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::root_resource_ref("producer", "identifier", vec![])),
            default: Box::new(Value::Deferred(DeferredValue::Secret(Box::new(
                Value::Concrete(ConcreteValue::String("fallback".into())),
            )))),
        });

        assert_eq!(
            union_secret_shapes_after_resolution(&applied, &authored),
            applied,
            "the apply-time branch selection is authoritative for a deferred authored node",
        );
    }

    #[test]
    fn authored_secret_union_projects_nested_values_without_tainting_siblings() {
        let public = Value::Concrete(ConcreteValue::String("selected-public".into()));
        let plain = Value::Concrete(ConcreteValue::String("plain".into()));
        let authored_coalesce = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::root_resource_ref("producer", "identifier", vec![])),
            default: Box::new(Value::Deferred(DeferredValue::Secret(Box::new(
                Value::Concrete(ConcreteValue::String("fallback".into())),
            )))),
        });
        let applied = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("selected".to_string(), public.clone()),
            ("sibling".to_string(), plain.clone()),
        ])));
        let authored = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("selected".to_string(), authored_coalesce),
            ("sibling".to_string(), plain),
        ])));

        assert_eq!(
            union_secret_shapes_after_resolution(&applied, &authored),
            applied,
        );
    }

    #[test]
    fn authored_secret_union_projects_canonical_string_list_elements() {
        let applied = Value::Concrete(ConcreteValue::StringList(vec![
            "selected-public".into(),
            "plain".into(),
        ]));
        let authored = Value::Concrete(ConcreteValue::List(vec![
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::root_resource_ref("producer", "identifier", vec![])),
                default: Box::new(Value::Deferred(DeferredValue::Secret(Box::new(
                    Value::Concrete(ConcreteValue::String("fallback".into())),
                )))),
            }),
            Value::Concrete(ConcreteValue::String("plain".into())),
        ]));

        assert_eq!(
            union_secret_shapes_after_resolution(&applied, &authored),
            applied,
        );
    }

    #[test]
    fn authored_secret_union_retains_selected_and_explicit_secret_children() {
        let selected_secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("selected-secret".into()),
        ))));
        let plain = Value::Concrete(ConcreteValue::String("plain".into()));
        let authored = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "selected".to_string(),
                Value::Deferred(DeferredValue::Coalesce {
                    value: Box::new(Value::root_resource_ref("producer", "missing", vec![])),
                    default: Box::new(Value::Deferred(DeferredValue::Secret(Box::new(
                        Value::Concrete(ConcreteValue::String("fallback".into())),
                    )))),
                }),
            ),
            (
                "explicit".to_string(),
                Value::Deferred(DeferredValue::Secret(Box::new(plain.clone()))),
            ),
            ("sibling".to_string(), plain.clone()),
        ])));
        let applied = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("selected".to_string(), selected_secret.clone()),
            (
                "explicit".to_string(),
                Value::Concrete(ConcreteValue::String("explicit-secret".into())),
            ),
            ("sibling".to_string(), plain.clone()),
        ])));

        let merged = union_secret_shapes_after_resolution(&applied, &authored);
        let Value::Concrete(ConcreteValue::Map(values)) = merged else {
            panic!("merged value must remain a map");
        };
        assert_eq!(values["selected"], selected_secret);
        assert!(matches!(
            values["explicit"],
            Value::Deferred(DeferredValue::Secret(_))
        ));
        assert_eq!(values["sibling"], plain);
    }

    #[test]
    fn unprojectable_authored_secret_fails_closed_at_the_child_only() {
        let unresolved = Value::root_resource_ref("runtime", "value", vec![]);
        let plain = Value::Concrete(ConcreteValue::String("plain".into()));
        let applied = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("unresolved".to_string(), unresolved.clone()),
            ("sibling".to_string(), plain.clone()),
        ])));
        let authored = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            (
                "unresolved".to_string(),
                Value::Deferred(DeferredValue::Coalesce {
                    value: Box::new(Value::root_resource_ref("producer", "identifier", vec![])),
                    default: Box::new(Value::Deferred(DeferredValue::Secret(Box::new(
                        Value::Concrete(ConcreteValue::String("fallback".into())),
                    )))),
                }),
            ),
            ("sibling".to_string(), plain.clone()),
        ])));

        let merged = union_secret_shapes_after_resolution(&applied, &authored);
        let Value::Concrete(ConcreteValue::Map(values)) = merged else {
            panic!("merged value must remain a map");
        };
        assert_eq!(
            values["unresolved"],
            Value::Deferred(DeferredValue::Secret(Box::new(unresolved))),
        );
        assert_eq!(values["sibling"], plain);
    }

    #[test]
    fn test_redact_secrets_in_value_no_plaintext_in_serialized_output() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("super-secret-password".into()),
        ))));
        let redacted = redact_secrets_in_value(&v).unwrap();
        let json = serde_json::to_string(&redacted).unwrap();
        assert!(
            !json.contains("super-secret-password"),
            "Serialized output must not contain plaintext secret, got: {}",
            json
        );
    }

    #[test]
    fn test_redact_secrets_in_live_composition_redacts_attribute_secrets() {
        // carina#3248: compositions are now persisted in saved plans, so
        // a literal secret authored inside a module's `attributes { ... }`
        // block (which lands as a `Value::Secret` in the composition's
        // attribute map) must be redacted before serialization, the
        // same way managed-resource attributes are redacted.
        use crate::resource::ExpandedLiveCompositionAttribute as LiveCompositionAttribute;
        use crate::resource::{
            ExpandedLiveComposition as LiveComposition,
            ExpandedLiveCompositionSignature as LiveCompositionSignature, ResourceId,
        };
        use std::collections::{BTreeSet, HashSet};
        let mut attrs: indexmap::IndexMap<String, LiveCompositionAttribute> =
            indexmap::IndexMap::new();
        attrs.insert(
            "non_secret".to_string(),
            LiveCompositionAttribute::from_value(
                Value::Concrete(ConcreteValue::String("kept".into())),
                None,
            ),
        );
        attrs.insert(
            "secret_field".to_string(),
            LiveCompositionAttribute::from_value(
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("plaintext-must-not-leak".into()),
                )))),
                None,
            ),
        );
        let virt = LiveComposition {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual",
                "module_instance",
            )),
            signature: LiveCompositionSignature {
                arguments: indexmap::IndexMap::new(),
                attributes: attrs,
                pending_constraints: Vec::new(),
            },
            binding: Some(absolute_binding("module_instance")),
            dependency_bindings: BTreeSet::new(),
            module_name: "m".to_string(),
            instance: "module_instance".to_string(),
            call_path: Default::default(),
            quoted_string_attrs: HashSet::new(),
        };

        let redacted = redact_secrets_in_live_composition(&virt).expect("redact virtual");

        // The non-secret attribute survives verbatim.
        assert_eq!(
            redacted
                .signature
                .attributes
                .get("non_secret")
                .and_then(|a| a.value()),
            Some(Value::Concrete(ConcreteValue::String("kept".into()))),
        );

        // The secret attribute is replaced with the typed placeholder, not
        // plaintext or an ordinary string.
        match redacted
            .signature
            .attributes
            .get("secret_field")
            .and_then(|a| a.value())
            .as_ref()
        {
            Some(Value::Deferred(DeferredValue::RedactedSecret { hash })) => {
                assert!(hash.encoded_digest().starts_with("$argon2id$"));
            }
            other => panic!("expected redacted secret, got: {:?}", other),
        }

        // The rest of the LiveComposition (id, binding, etc.) is
        // preserved verbatim.
        assert_eq!(
            redacted.binding.as_ref().map(|binding| binding.as_str()),
            Some("module_instance")
        );
        assert_eq!(redacted.id.resource_type, "_virtual");
    }

    #[test]
    fn test_redact_secrets_in_value_nested_in_map() {
        let mut map = IndexMap::new();
        map.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("test".into())),
        );
        map.insert(
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("s3cret".into()),
            )))),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let redacted = redact_secrets_in_value(&v).unwrap();
        let json = serde_json::to_string(&redacted).unwrap();
        assert!(
            !json.contains("s3cret"),
            "Serialized map must not contain plaintext secret, got: {}",
            json
        );
        // Non-secret values should be preserved
        assert!(
            json.contains("test"),
            "Non-secret value should be preserved"
        );
    }

    #[test]
    fn test_redact_secrets_in_value_nested_in_list() {
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("visible".into())),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("hidden".into()),
            )))),
        ]));
        let redacted = redact_secrets_in_value(&v).unwrap();
        let json = serde_json::to_string(&redacted).unwrap();
        assert!(
            !json.contains("hidden"),
            "Serialized list must not contain plaintext secret, got: {}",
            json
        );
        assert!(json.contains("visible"));
    }

    #[test]
    fn test_redact_secrets_in_value_preserves_non_secret() {
        let v = Value::Concrete(ConcreteValue::String("not-a-secret".into()));
        let redacted = redact_secrets_in_value(&v).unwrap();
        assert_eq!(redacted, v);
    }

    #[test]
    fn test_redact_secrets_in_attributes() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("my-bucket".into())),
        );
        attrs.insert(
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("hunter2".into()),
            )))),
        );
        let redacted = redact_secrets_in_attributes(&attrs).unwrap();
        let json = serde_json::to_string(&redacted).unwrap();
        assert!(
            !json.contains("hunter2"),
            "Serialized attributes must not contain plaintext secret, got: {}",
            json
        );
        assert!(json.contains("my-bucket"));
    }

    // Closure-shaped tests deleted: `Value::Closure` no longer exists,
    // so `format_value` and `value_to_json` only see user-facing values.
    // The "closure cannot become data" guarantee is now enforced at the
    // type level by `EvalValue::into_value`.

    // ----- render_value_pretty tests -----

    /// Helper to construct a layout for tests.
    fn layout(parent_indent_cols: usize, key: &str) -> PrettyLayout {
        PrettyLayout::for_attribute(parent_indent_cols, key)
    }

    #[test]
    fn render_value_pretty_string_matches_format_value() {
        let v = Value::Concrete(ConcreteValue::String("hello".into()));
        assert_eq!(
            render_value_pretty(&v, layout(0, "k")).plain(),
            format_value(&v)
        );
    }

    #[test]
    fn rendered_value_records_semantic_span_kinds() {
        let value = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("hello".into())),
            Value::Concrete(ConcreteValue::Bool(true)),
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::enum_identifier("test.Mode.enabled")),
            Value::root_resource_ref("server", "id", Vec::new()),
        ]));

        let rendered = render_value(&value);
        assert_eq!(rendered.plain(), format_value(&value));
        assert_eq!(
            rendered
                .spans()
                .iter()
                .map(|span| (span.kind(), span.text()))
                .collect::<Vec<_>>(),
            vec![
                (ValueSpanKind::Structure, "["),
                (ValueSpanKind::StringLiteral, "'hello'"),
                (ValueSpanKind::Structure, ", "),
                (ValueSpanKind::Bool, "true"),
                (ValueSpanKind::Structure, ", "),
                (ValueSpanKind::Number, "42"),
                (ValueSpanKind::Structure, ", "),
                (ValueSpanKind::Enum, "test.Mode.enabled"),
                (ValueSpanKind::Structure, ", "),
                (ValueSpanKind::Reference, "server.id"),
                (ValueSpanKind::Structure, "]"),
            ]
        );
    }

    #[test]
    fn interpolation_preserves_expression_span_kinds() {
        let value = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix ".into()),
            InterpolationPart::Expr(Value::Deferred(DeferredValue::BindingRef {
                binding: absolute_binding("cert"),
            })),
            InterpolationPart::Literal(".example".into()),
        ]));

        let rendered = render_value(&value);
        assert_eq!(rendered.plain(), "\"prefix ${cert}.example\"");
        assert_eq!(
            rendered
                .spans()
                .iter()
                .map(|span| (span.kind(), span.text()))
                .collect::<Vec<_>>(),
            vec![
                (ValueSpanKind::StringLiteral, "\"prefix "),
                (ValueSpanKind::Structure, "${"),
                (ValueSpanKind::Reference, "cert"),
                (ValueSpanKind::Structure, "}"),
                (ValueSpanKind::StringLiteral, ".example\""),
            ]
        );
    }

    #[test]
    fn rendered_value_preserves_duration_and_postfix_access_color_categories() {
        let duration = render_value(&Value::Concrete(ConcreteValue::Duration(
            std::time::Duration::from_secs(3600),
        )));
        assert_eq!(
            duration
                .spans()
                .iter()
                .map(|span| (span.kind(), span.text()))
                .collect::<Vec<_>>(),
            vec![(ValueSpanKind::Other, "1h")]
        );

        let binding = render_value(&Value::Deferred(DeferredValue::BindingRef {
            binding: absolute_binding("x"),
        }));
        assert_eq!(
            binding
                .spans()
                .iter()
                .map(|span| (span.kind(), span.text()))
                .collect::<Vec<_>>(),
            vec![(ValueSpanKind::Reference, "x")]
        );

        let access = crate::resource::apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String("/".into())).into(),
                    Value::Deferred(DeferredValue::BindingRef {
                        binding: absolute_binding("x"),
                    })
                    .into(),
                ],
                source_line: (1).into(),
            }),
            crate::resource::PathSegment::Subscript {
                index: crate::resource::Subscript::Int { index: 1 },
            },
            1,
            crate::resource::AccessMissPolicy::Error,
        )
        .expect("a deferred base retains postfix access");
        let rendered = render_value(&access);
        assert_eq!(rendered.plain(), "split('/', x)[1]");
        assert_eq!(
            rendered
                .spans()
                .iter()
                .map(|span| (span.kind(), span.text()))
                .collect::<Vec<_>>(),
            vec![
                (ValueSpanKind::Other, "split"),
                (ValueSpanKind::Structure, "("),
                (ValueSpanKind::StringLiteral, "'/'"),
                (ValueSpanKind::Structure, ", "),
                (ValueSpanKind::Reference, "x"),
                (ValueSpanKind::Structure, ")"),
                (ValueSpanKind::Reference, "[1]"),
            ]
        );
    }

    #[test]
    fn map_keys_use_canonical_literal_when_not_identifier_safe() {
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("it's".to_string(), Value::Concrete(ConcreteValue::Int(1))),
            ("a-b".to_string(), Value::Concrete(ConcreteValue::Int(2))),
            (
                "kubernetes.io/role".to_string(),
                Value::Concrete(ConcreteValue::Int(3)),
            ),
            (
                "safe_key".to_string(),
                Value::Concrete(ConcreteValue::Int(4)),
            ),
        ])));

        assert_eq!(
            format_value(&value),
            r#"{'a-b': 2, "it's": 1, 'kubernetes.io/role': 3, safe_key: 4}"#
        );
    }

    #[test]
    fn rendered_key_owns_the_canonical_map_key_text() {
        assert_eq!(RenderedKey::from_raw("safe_key").plain(), "safe_key");
        assert_eq!(RenderedKey::from_raw("a-b").plain(), "'a-b'");
        assert_eq!(RenderedKey::from_raw("it's").plain(), r#""it's""#);
    }

    #[test]
    fn pretty_value_renders_only_through_the_core_formatter() {
        let key = RenderedKey::from_raw("a-b");
        let value = PrettyValue::new(Value::Concrete(ConcreteValue::Float(1.0)));

        assert_eq!(
            value.render(PrettyLayout::for_map_key(0, &key)).plain(),
            "1.0"
        );
        assert!(!value.needs_trailing_separator());
    }

    #[test]
    fn json_values_render_with_core_text_and_span_kinds() {
        let value = PersistedExportValue::from_serialized_json(serde_json::json!({
            "a-b": ["value", 1.0, true, null]
        }))
        .expect("test value is valid persisted JSON");
        let rendered = render_export_json_value(&value);

        assert_eq!(rendered.plain(), "{'a-b': ['value', 1.0, true, null]}");
        assert_eq!(
            rendered
                .spans()
                .iter()
                .map(ValueSpan::kind)
                .collect::<Vec<_>>(),
            vec![
                ValueSpanKind::Structure,
                ValueSpanKind::Key,
                ValueSpanKind::Structure,
                ValueSpanKind::StringLiteral,
                ValueSpanKind::Structure,
                ValueSpanKind::Number,
                ValueSpanKind::Structure,
                ValueSpanKind::Bool,
                ValueSpanKind::Structure,
                ValueSpanKind::Other,
                ValueSpanKind::Structure,
            ]
        );
    }

    #[test]
    fn persisted_json_display_preserves_large_unsigned_integer_exactly() {
        let raw = serde_json::json!(18_446_744_073_709_551_615_u64);
        let value = PersistedValue::from_serialized_json(raw.clone())
            .expect("u64 JSON is valid persisted JSON");
        let export = PersistedExportValue::from_serialized_json(raw.clone())
            .expect("u64 JSON is valid persisted export JSON");

        assert_eq!(value.decoded_json(), raw.clone());
        assert_eq!(export.decoded_json(), raw);
        assert_eq!(
            render_export_json_value(&export).plain(),
            "18446744073709551615",
        );
    }

    #[test]
    fn persisted_json_display_uses_value_float_formatting() {
        for (raw, expected) in [
            (serde_json::json!(1e21), "1000000000000000000000.0"),
            (serde_json::json!(1e-7), "0.0000001"),
            (serde_json::json!(1.0), "1.0"),
        ] {
            let value = PersistedValue::from_serialized_json(raw.clone())
                .expect("float JSON is valid persisted JSON");
            let export = PersistedExportValue::from_serialized_json(raw)
                .expect("float JSON is valid persisted export JSON");

            assert_eq!(value.decoded_json(), export.decoded_json());
            assert_eq!(render_export_json_value(&export).plain(), expected);
        }
    }

    #[test]
    fn persisted_canonical_enum_json_renders_as_an_enum_value() {
        let value = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                crate::schema::TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
                "vpc",
            ),
        ));
        let json = PersistedValue::encode_desired_value(&value, None)
            .expect("canonical enum is JSON-serializable");

        let rendered = render_value(&json.decode().expect("persisted enum decodes"));

        assert_eq!(rendered.plain(), "vpc");
        assert_eq!(
            rendered
                .spans()
                .iter()
                .map(|span| (span.kind(), span.text()))
                .collect::<Vec<_>>(),
            vec![(ValueSpanKind::Enum, "vpc")]
        );
    }

    #[test]
    fn persisted_enum_shaped_user_map_renders_as_a_map() {
        let json = PersistedValue::encode_desired_value(&enum_shaped_user_map(), None)
            .expect("user map is persistable");

        assert_eq!(
            render_value(&json.decode().expect("persisted user map decodes")).plain(),
            "{Enum: {api_value: 'vpc', identity: {kind: 'Domain', provider: 'aws', segments: ['ec2', 'Eip']}}}"
        );
    }

    #[test]
    fn pretty_map_keys_use_the_same_canonical_rule() {
        let long_value = Value::Concrete(ConcreteValue::String("x".repeat(70).into()));
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([
            ("it's".to_string(), long_value.clone()),
            ("a-b".to_string(), long_value.clone()),
            ("kubernetes.io/role".to_string(), long_value),
        ])));

        let rendered = render_value_pretty(&value, layout(0, "config"));
        assert!(rendered.plain().starts_with('\n'));
        assert!(rendered.plain().contains("  'a-b': "));
        assert!(rendered.plain().contains("  \"it's\": "));
        assert!(rendered.plain().contains("  'kubernetes.io/role': "));
        assert!(
            rendered
                .spans()
                .iter()
                .filter(|span| span.kind() == ValueSpanKind::Key)
                .all(|span| span.text() != "it's")
        );
    }

    #[test]
    fn render_value_pretty_int_renders_as_integer_literal() {
        let v = Value::Concrete(ConcreteValue::Int(42));
        assert_eq!(render_value_pretty(&v, layout(0, "k")).plain(), "42");
    }

    #[test]
    fn render_value_pretty_bool_renders_as_keyword() {
        let v = Value::Concrete(ConcreteValue::Bool(true));
        assert_eq!(render_value_pretty(&v, layout(0, "k")).plain(), "true");
    }

    #[test]
    fn render_value_pretty_dsl_enum_resolves_to_provider_value() {
        let v = Value::Concrete(ConcreteValue::String(
            "aws.s3.Bucket.VersioningStatus.enabled".into(),
        ));
        assert_eq!(
            render_value_pretty(&v, layout(0, "k")).plain(),
            format_value(&v)
        );
    }

    #[test]
    fn render_value_pretty_secret_masked() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        ))));
        assert_eq!(render_value_pretty(&v, layout(0, "k")).plain(), "(secret)");
    }

    #[test]
    fn render_value_pretty_unknown_renders_like_format_value() {
        let v = Value::Deferred(DeferredValue::Unknown(
            ApplyTimeUnknown::UpstreamAllExports {
                binding: absolute_binding("upstream"),
            },
        ));
        assert_eq!(
            render_value_pretty(&v, layout(0, "k")).plain(),
            format_value(&v)
        );
    }

    #[test]
    fn render_value_pretty_list_of_maps_vertical() {
        let mut s1 = IndexMap::new();
        s1.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("First".into())),
        );
        s1.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".into())),
        );
        let mut s2 = IndexMap::new();
        s2.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("Second".into())),
        );
        s2.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Deny".into())),
        );
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(s1)),
            Value::Concrete(ConcreteValue::Map(s2)),
        ]));

        // parent_indent_cols=6 → entry_indent_cols=8, where `* ` marker
        // sits; first key follows at col 10, continuation keys also at
        // col 10. Element boundaries also get a blank line. The `*`
        // marker (#2552) replaces the `- ` marker that #2545 dropped to
        // avoid the destroy-marker collision.
        let rendered = render_value_pretty(&v, layout(6, "statement"));
        let out = rendered.plain();
        let expected = "\n        * effect: 'Allow'\n          sid: 'First'\n\n        * effect: 'Deny'\n          sid: 'Second'";
        assert_eq!(out, expected);
    }

    #[test]
    fn render_value_pretty_list_of_maps_single_entry() {
        let mut m = IndexMap::new();
        m.insert(
            "k".to_string(),
            Value::Concrete(ConcreteValue::String("v".into())),
        );
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(m),
        )]));
        // parent_indent_cols=4 → entry_indent_cols=6, `* ` at col 6, key
        // at col 8.
        let rendered = render_value_pretty(&v, layout(4, "items"));
        let out = rendered.plain();
        assert_eq!(out, "\n      * k: 'v'");
        // Single element must not introduce a blank-line separator.
        assert!(
            !out.contains("\n\n"),
            "single-entry list-of-maps must not contain blank separator: {out:?}"
        );
    }

    #[test]
    fn render_value_pretty_list_of_maps_three_entries_separated_by_blank_lines() {
        let make = |sid: &str| {
            let mut m = IndexMap::new();
            m.insert(
                "sid".to_string(),
                Value::Concrete(ConcreteValue::String(sid.to_string().into())),
            );
            Value::Concrete(ConcreteValue::Map(m))
        };
        let v = Value::Concrete(ConcreteValue::List(vec![make("A"), make("B"), make("C")]));
        let rendered = render_value_pretty(&v, layout(4, "items"));
        let out = rendered.plain();
        // Exactly N-1 = 2 blank-line separators between three elements.
        // The trailing blank before the next sibling key (#2555) is
        // emitted by the parent (`format_map_vertical` /
        // `display::MapExpanded`), not by this function — so the bare
        // output contains no trailing whitespace.
        let blank_separators = out.matches("\n\n").count();
        assert_eq!(
            blank_separators, 2,
            "three-element list: 2 between-element blank lines: {out:?}"
        );
        assert!(
            !out.ends_with("\n\n"),
            "trailing whitespace must not double the resource block separator: {out:?}"
        );
    }

    /// Inside a vertical Map, a multi-element list-of-maps key followed
    /// by a sibling key gets a blank-line separator so the boundary
    /// stays visible — the `*` marker disambiguates element starts but
    /// not element *ends*. The element values are deliberately long to
    /// force the outer Map to expand vertically rather than render
    /// inline. (#2555)
    #[test]
    fn render_value_pretty_map_with_list_of_maps_then_sibling_separates() {
        let long = "x".repeat(40);
        let mut s1 = IndexMap::new();
        s1.insert(
            "sid_a".to_string(),
            Value::Concrete(ConcreteValue::String(long.clone().into())),
        );
        let mut s2 = IndexMap::new();
        s2.insert(
            "sid_b".to_string(),
            Value::Concrete(ConcreteValue::String(long.clone().into())),
        );
        let mut outer = IndexMap::new();
        outer.insert(
            "statement".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Map(s1)),
                Value::Concrete(ConcreteValue::Map(s2)),
            ])),
        );
        outer.insert(
            "version".to_string(),
            Value::Concrete(ConcreteValue::String("2012".into())),
        );
        let v = Value::Concrete(ConcreteValue::Map(outer));
        let rendered = render_value_pretty(&v, layout(2, "config"));
        let out = rendered.plain();
        // The blank-line MUST sit between the last element's last key
        // and the sibling `version:` key. Use unique key names so the
        // assertion can't be satisfied by the inter-element blank
        // between two `sid:`s.
        assert!(
            out.contains(&format!("sid_b: '{long}'\n\n")),
            "blank line must follow last list-of-maps element before sibling: {out:?}"
        );
    }

    /// A multi-element list-of-maps that is the LAST key of its parent
    /// must NOT receive a trailing blank, otherwise the resource-block
    /// separator above it doubles into two blank lines. (#2555)
    #[test]
    fn render_value_pretty_map_with_trailing_list_of_maps_no_orphan_blank() {
        let long = "x".repeat(40);
        let mut s1 = IndexMap::new();
        s1.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String(long.clone().into())),
        );
        let mut s2 = IndexMap::new();
        s2.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String(long.clone().into())),
        );
        let mut outer = IndexMap::new();
        outer.insert(
            "version".to_string(),
            Value::Concrete(ConcreteValue::String("2012".into())),
        );
        outer.insert(
            "statement".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::Map(s1)),
                Value::Concrete(ConcreteValue::Map(s2)),
            ])),
        );
        let v = Value::Concrete(ConcreteValue::Map(outer));
        let rendered = render_value_pretty(&v, layout(2, "config"));
        let out = rendered.plain();
        assert!(
            !out.ends_with("\n\n"),
            "trailing list-of-maps must not leave an orphan blank: {out:?}"
        );
    }

    #[test]
    fn render_value_pretty_empty_list_inline() {
        let v = Value::Concrete(ConcreteValue::List(vec![]));
        assert_eq!(render_value_pretty(&v, layout(0, "k")).plain(), "[]");
    }

    #[test]
    fn render_value_pretty_list_of_strings_under_80_inline() {
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::String("b".into())),
        ]));
        assert_eq!(
            render_value_pretty(&v, layout(0, "k")).plain(),
            "['a', 'b']"
        );
    }

    #[test]
    fn render_value_pretty_list_of_strings_over_80_vertical() {
        // 5 strings of ~20 chars each → inline ~110 chars
        let items: Vec<Value> = (0..5)
            .map(|i| {
                Value::Concrete(ConcreteValue::String(
                    format!("iam:LongActionName{}", i).into(),
                ))
            })
            .collect();
        let v = Value::Concrete(ConcreteValue::List(items));
        // parent_indent_cols=4, key="action" → first item starts at parent_indent+2 = col 6.
        let rendered = render_value_pretty(&v, layout(4, "action"));
        let out = rendered.plain();
        assert!(
            out.starts_with("[\n"),
            "expected bracket-newline start, got: {out}"
        );
        assert!(
            out.contains("\n      'iam:LongActionName0',"),
            "missing first item line at col 6: {out}"
        );
        assert!(
            out.ends_with("\n    ]"),
            "expected closing bracket at parent_indent col: {out}"
        );
    }

    #[test]
    fn render_value_pretty_list_of_strings_threshold_boundary_exact_80_inline() {
        // Inline form fits exactly within 80 cols at parent_indent=0, key="x" (1 char):
        // total = 0 + 1 + 2 + len(inline). For len(inline)=77, total=80 → stay inline.
        let item = "x".repeat(73); // inline = 1 + 1 + 73 + 1 + 1 = 77
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String(item.into()),
        )]));
        let inline = format_value_with_key(&v, None);
        assert_eq!(inline.len(), 77, "fixture sanity: {} chars", inline.len());
        // total budget = 0 + 1 + 2 + 77 = 80, exactly at limit → inline.
        assert_eq!(render_value_pretty(&v, layout(0, "x")).plain(), inline);
    }

    #[test]
    fn render_value_pretty_list_of_strings_threshold_boundary_81_expands() {
        // 1 char over threshold: 0 + 1 + 2 + 78 = 81 → expand.
        let item = "x".repeat(74); // inline = 78
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String(item.into()),
        )]));
        let inline = format_value_with_key(&v, None);
        assert_eq!(inline.len(), 78, "fixture sanity: {} chars", inline.len());
        let rendered = render_value_pretty(&v, layout(0, "x"));
        let out = rendered.plain();
        assert!(
            out.starts_with("[\n"),
            "1 over threshold should expand, got: {out}"
        );
    }

    #[test]
    fn render_value_pretty_list_of_strings_indent_pushes_over_threshold() {
        // Inline form is 75 chars; at parent_indent=10, key="kk" (2),
        // total = 10 + 2 + 2 + 75 = 89 → expand.
        let inline_target = 75;
        let item = "x".repeat(inline_target - 4);
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String(item.into()),
        )]));
        let inline = format_value_with_key(&v, None);
        assert_eq!(inline.len(), inline_target);
        let rendered = render_value_pretty(&v, layout(10, "kk"));
        let out = rendered.plain();
        assert!(out.starts_with("[\n"), "deep indent should expand: {out}");
    }

    #[test]
    fn render_value_pretty_list_of_maps_with_nested_map() {
        let mut inner = IndexMap::new();
        inner.insert("StringEquals".to_string(), {
            let mut m = IndexMap::new();
            m.insert(
                "aws:Tag".to_string(),
                Value::Concrete(ConcreteValue::String("prod".into())),
            );
            Value::Concrete(ConcreteValue::Map(m))
        });
        let mut entry = IndexMap::new();
        entry.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("X".into())),
        );
        entry.insert(
            "condition".to_string(),
            Value::Concrete(ConcreteValue::Map(inner)),
        );
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(entry),
        )]));
        // parent_indent_cols=4 → entry_indent_cols=6; first sorted key
        // is preceded by `* ` at col 6 (#2552).
        let rendered = render_value_pretty(&v, layout(4, "statement"));
        let out = rendered.plain();
        assert!(
            out.contains("      * condition:"),
            "expected `* condition:` at col 6, got: {out}"
        );
        assert!(out.contains("sid: 'X'"), "expected sid line, got: {out}");
    }

    #[test]
    fn render_value_pretty_list_of_maps_with_long_string_list_inside() {
        let actions: Vec<Value> = (0..6)
            .map(|i| Value::Concrete(ConcreteValue::String(format!("iam:Action{:03}", i).into())))
            .collect();
        let mut entry = IndexMap::new();
        entry.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("X".into())),
        );
        entry.insert(
            "action".to_string(),
            Value::Concrete(ConcreteValue::List(actions)),
        );
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(entry),
        )]));
        let rendered = render_value_pretty(&v, layout(4, "statement"));
        let out = rendered.plain();
        assert!(
            out.contains("action: ["),
            "expected expanded action list bracket: {out}"
        );
        assert!(
            out.contains("'iam:Action000',"),
            "expected first action on its own line: {out}"
        );
    }

    #[test]
    fn render_value_pretty_string_list_under_dynamic_key_breaks_vertically() {
        // #2528 hypothesis: when the deepest value is `Value::Concrete(ConcreteValue::StringList)`
        // (the canonical form #2511 folds `Union[String, list(String)]`
        // into) rather than `Value::Concrete(ConcreteValue::List)`, `render_value_pretty` treats
        // it as a scalar fallthrough and renders inline. This pins the
        // expected vertical break.
        let mut string_like = IndexMap::new();
        string_like.insert(
            "token.actions.githubusercontent.com:sub".to_string(),
            Value::Concrete(ConcreteValue::StringList(vec![
                "repo:carina-rs/infra:ref:refs/heads/main"
                    .to_string()
                    .into(),
                "repo:carina-rs/infra:pull_request".into(),
            ])),
        );
        let v = Value::Concrete(ConcreteValue::Map(string_like));
        // Mirror the layout under `condition.string_like:` (parent at
        // col 12 from the MapExpanded entry indentation). Inside a Map
        // the value text starts after `<key>: ` so the bracketed form
        // appears as `<key>: [\n   '...',\n   '...'\n  ]` — one element
        // per line with a trailing comma. The inline form is the
        // single-line `<key>: ["...", "..."]` with no `\n` mid-list.
        let rendered = render_value_pretty(&v, layout(12, "string_like"));
        let out = rendered.plain();
        assert!(
            out.contains("'token.actions.githubusercontent.com:sub': [\n"),
            "expected dynamic-key StringList to start its bracketed form, got:\n{out}"
        );
        assert!(
            out.contains("refs/heads/main',\n"),
            "expected first element on its own line with trailing comma, got:\n{out}"
        );
    }

    #[test]
    fn render_value_pretty_top_level_string_list_breaks_vertically_when_oversize() {
        // The same fix applies when a top-level attribute value is
        // already canonicalized to `Value::Concrete(ConcreteValue::StringList)`. Pre-fix the
        // wildcard arm collapsed it to `["a", "b", ...]` even when the
        // line exceeded `PRETTY_LINE_LIMIT`.
        let v = Value::Concrete(ConcreteValue::StringList(vec![
            "a".repeat(40).into(),
            "b".repeat(40).into(),
        ]));
        let rendered = render_value_pretty(&v, layout(0, "k"));
        let out = rendered.plain();
        assert!(
            out.starts_with("[\n"),
            "expected oversize StringList to expand vertically, got:\n{out}"
        );
    }

    #[test]
    fn render_value_pretty_string_list_does_not_reclassify_marker_shaped_text() {
        // No decoder assigns semantics from a string's spelling. A string that
        // happens to use the historical prefix stays a string.
        let marker_shaped_text = format!("_secret:argon2:{}", "deadbeef".repeat(8));
        let v = Value::Concrete(ConcreteValue::StringList(vec![
            marker_shaped_text.clone().into(),
            "x".repeat(80).into(), // force vertical
        ]));
        let rendered = render_value_pretty(&v, layout(0, "k"));
        let out = rendered.plain();
        assert!(
            out.contains(&format!("'{marker_shaped_text}',")),
            "ordinary marker-shaped text must remain visible as a string, got:\n{out}"
        );
    }

    #[test]
    fn render_value_pretty_nested_map_with_dynamic_key_list_value_breaks_vertically() {
        // #2528: an IAM trust-policy `condition.<operator>.<context-key>:
        // [list]` shape — a multi-element list-of-strings nested inside
        // a Map inside a Map inside a list-of-maps inside a Map field.
        // After #2524 the outer list-of-maps and its `action` siblings
        // break vertically, but the deepest list (under a dynamic Map
        // key) used to stay on one line because the prefix-width budget
        // check looked only at the immediate parent key, not at the
        // *sum* of all nested key prefixes once expansion bubbles down.
        //
        // Pin the expected break: the multi-element list under the
        // dynamic key must render with its bracketed multi-line form,
        // not collapse to `key: ["a", "b"]`.
        let mut string_like = IndexMap::new();
        string_like.insert(
            "token.actions.githubusercontent.com:sub".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String(
                    "repo:carina-rs/infra:ref:refs/heads/main"
                        .to_string()
                        .into(),
                )),
                Value::Concrete(ConcreteValue::String(
                    "repo:carina-rs/infra:pull_request".into(),
                )),
            ])),
        );
        let mut condition = IndexMap::new();
        condition.insert(
            "string_like".to_string(),
            Value::Concrete(ConcreteValue::Map(string_like)),
        );
        let mut entry = IndexMap::new();
        entry.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("AssumeRole".into())),
        );
        entry.insert(
            "condition".to_string(),
            Value::Concrete(ConcreteValue::Map(condition)),
        );
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(entry),
        )]));
        let rendered = render_value_pretty(&v, layout(4, "statement"));
        let out = rendered.plain();
        // The dynamic-key list value must break across lines (bracketed
        // form), not collapse to `<dynamic-key>: ["a", "b"]`.
        assert!(
            out.contains("'token.actions.githubusercontent.com:sub': ["),
            "expected dynamic-key list to start its bracket form, got:\n{out}"
        );
        assert!(
            out.contains("'repo:carina-rs/infra:ref:refs/heads/main',"),
            "expected first list element on its own line with trailing comma, got:\n{out}"
        );
    }

    #[test]
    fn render_value_pretty_empty_map_inline() {
        let v = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
        assert_eq!(render_value_pretty(&v, layout(0, "k")).plain(), "{}");
    }

    #[test]
    fn render_value_pretty_small_map_inline_fits() {
        let mut m = IndexMap::new();
        m.insert("a".to_string(), Value::Concrete(ConcreteValue::Int(1)));
        m.insert("b".to_string(), Value::Concrete(ConcreteValue::Int(2)));
        let v = Value::Concrete(ConcreteValue::Map(m));
        // {a: 1, b: 2} = 12 chars; total = 0 + 1 + 2 + 12 = 15 → inline.
        assert_eq!(
            render_value_pretty(&v, layout(0, "k")).plain(),
            "{a: 1, b: 2}"
        );
    }

    #[test]
    fn render_value_pretty_top_level_map_expands_when_over_threshold() {
        let mut m = IndexMap::new();
        m.insert(
            "first_key".to_string(),
            Value::Concrete(ConcreteValue::String("a".repeat(40).into())),
        );
        m.insert(
            "second_key".to_string(),
            Value::Concrete(ConcreteValue::String("b".repeat(40).into())),
        );
        let v = Value::Concrete(ConcreteValue::Map(m));
        let inline = format_value_with_key(&v, None);
        assert!(inline.len() > PRETTY_LINE_LIMIT, "fixture sanity");

        // parent_indent_cols=0, key="cfg" → child keys at col 2.
        let rendered = render_value_pretty(&v, layout(0, "cfg"));
        let out = rendered.plain();
        assert!(
            out.starts_with("\n  first_key: "),
            "expected child keys at col 2, got: {out}"
        );
        assert!(
            out.contains("\n  second_key: "),
            "expected second key at col 2: {out}"
        );
    }

    #[test]
    fn render_value_pretty_scalar_variants_match_format_value() {
        // Lock the "scalar fallthrough" contract.
        use crate::resource::ExpandedAccessPath as AccessPath;

        let path = AccessPath::root("vpc", "id");
        let cases = vec![
            Value::Concrete(ConcreteValue::String("hello".into())),
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::Float(2.5)),
            Value::Concrete(ConcreteValue::Bool(false)),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("pw".into()),
            )))),
            Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
                path: path.clone(),
            })),
            Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("prefix-".into()),
                InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef { path })),
            ])),
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::concat,
                args: vec![
                    Value::Concrete(ConcreteValue::String("a".into())).into(),
                    Value::Concrete(ConcreteValue::String("b".into())).into(),
                ],
                source_line: (0).into(),
            }),
        ];

        for v in &cases {
            assert_eq!(
                render_value_pretty(v, layout(0, "k")).plain(),
                format_value(v),
                "scalar fallthrough drift for variant: {v:?}"
            );
        }
    }

    // ---- canonicalize_with_type tests (#2481, #2510) ----

    fn string_or_list_of_strings<R: crate::schema::PatternForm>()
    -> crate::schema::PatternAttributeType<R> {
        PatternAttributeType::union(vec![
            PatternAttributeType::string(),
            PatternAttributeType::list(PatternAttributeType::string()),
        ])
    }

    #[test]
    fn canonicalize_scalar_to_string_list() {
        let t = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::String("repo:foo:*".into()));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(
            canon,
            Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".into()]))
        );
    }

    #[test]
    fn canonicalize_single_element_list_to_string_list() {
        let t = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("repo:foo:*".into()),
        )]));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(
            canon,
            Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".into()]))
        );
    }

    #[test]
    fn canonicalize_multi_element_list_to_string_list() {
        let t = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::String("b".into())),
            Value::Concrete(ConcreteValue::String("c".into())),
        ]));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(
            canon,
            Value::Concrete(ConcreteValue::StringList(vec![
                "a".into(),
                "b".into(),
                "c".into()
            ]))
        );
    }

    #[test]
    fn canonicalize_idempotent_on_string_list() {
        let t = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::StringList(vec!["a".into()]));
        let canon = canonicalize_state_with_type(
            v.clone(),
            &t,
            crate::schema::empty_defs_for_schema_walks(),
        );
        assert_eq!(canon, v);
    }

    #[test]
    fn canonicalize_passes_through_non_applicable_type() {
        let v = Value::Concrete(ConcreteValue::String("foo".into()));
        let canon = canonicalize_state_with_type(
            v.clone(),
            &AttributeType::string(),
            crate::schema::empty_defs_for_schema_walks(),
        );
        assert_eq!(canon, v);
    }

    #[test]
    fn canonicalize_enum_lifts_state_string_to_canonical_enum() {
        let t = AttributeType::enum_(
            crate::schema::enum_identity("Effect", Some("aws.iam.PolicyDocument")),
            Some(vec!["Allow".to_string(), "Deny".to_string()]),
            vec![
                ("Allow".to_string(), "allow".to_string()),
                ("Deny".to_string(), "deny".to_string()),
            ],
            None,
            None,
        );
        let v = Value::Concrete(ConcreteValue::String("Allow".into()));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::CanonicalEnum(c)) => {
                assert_eq!(c.identity().to_string(), "aws.iam.PolicyDocument.Effect");
                assert_eq!(c.api_value(), "Allow");
            }
            other => panic!("expected CanonicalEnum, got {other:?}"),
        }
    }

    #[test]
    fn canonicalize_passes_through_non_string_list() {
        let t = string_or_list_of_strings();
        // List with non-String elements stays as List — not the canonical
        // form. Schema validation will flag it elsewhere.
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Int(1),
        )]));
        let canon = canonicalize_state_with_type(
            v.clone(),
            &t,
            crate::schema::empty_defs_for_schema_walks(),
        );
        assert_eq!(canon, v);
    }

    #[test]
    fn canonicalize_recurses_into_struct_fields() {
        let t = AttributeType::struct_(
            "Statement".to_string(),
            vec![
                crate::schema::CompiledStructField::optional("action", string_or_list_of_strings())
                    .build(),
            ],
        );
        let mut map = IndexMap::new();
        map.insert(
            "action".to_string(),
            Value::Concrete(ConcreteValue::String("s3:GetObject".into())),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("action"),
                    Some(&Value::Concrete(ConcreteValue::StringList(vec![
                        "s3:GetObject".into()
                    ])))
                );
            }
            _ => panic!("expected Map"),
        }
    }

    #[test]
    fn canonicalize_iam_policy_document_statement_action_to_string_list() {
        let statement = AttributeType::struct_(
            "Statement".to_string(),
            vec![
                crate::schema::CompiledStructField::optional("action", string_or_list_of_strings())
                    .build(),
                crate::schema::CompiledStructField::optional(
                    "resource",
                    string_or_list_of_strings(),
                )
                .build(),
            ],
        );
        let policy_document = AttributeType::struct_(
            "PolicyDocument".to_string(),
            vec![
                crate::schema::CompiledStructField::optional(
                    "statement",
                    AttributeType::list(statement),
                )
                .build(),
            ],
        );
        let schema = crate::schema::Schema::flat(policy_document);

        let mut statement = IndexMap::new();
        statement.insert(
            "action".to_string(),
            Value::Concrete(ConcreteValue::String("sts:AssumeRole".into())),
        );
        let mut policy = IndexMap::new();
        policy.insert(
            "statement".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(statement),
            )])),
        );

        let canon = schema
            .canonicalize_state(Value::Concrete(ConcreteValue::Map(policy)))
            .into_present()
            .expect("policy document fixture must remain present");
        let Value::Concrete(ConcreteValue::Map(policy)) = canon else {
            panic!("expected policy document map");
        };
        let Some(Value::Concrete(ConcreteValue::List(statements))) = policy.get("statement") else {
            panic!("expected statement list, got {policy:?}");
        };
        let Some(Value::Concrete(ConcreteValue::Map(statement))) = statements.first() else {
            panic!("expected first statement map, got {statements:?}");
        };

        assert_eq!(
            statement.get("action"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "sts:AssumeRole".into()
            ])))
        );
    }

    #[test]
    fn canonicalize_float_preserves_integer_and_display() {
        let attr = AttributeType::float();
        let defs = crate::schema::empty_defs_for_schema_walks();
        for n in [1, 9_007_199_254_740_993] {
            for purpose in [
                CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
                CanonicalizationPurpose::Desired(DesiredValueSource::ProviderOutput),
                CanonicalizationPurpose::ProviderConfig {
                    provider_name: "mock",
                },
                CanonicalizationPurpose::State,
                CanonicalizationPurpose::WaitRhs,
            ] {
                let value: Value = Value::Concrete(ConcreteValue::Int(n));
                let fit = crate::schema::fit_value_to_resolved_type_on_path(
                    &attr,
                    &value,
                    defs,
                    crate::schema::FieldAncestry::WRITABLE,
                    purpose,
                    &mut Vec::new(),
                )
                .expect("integers fit Float attributes");
                let canonical = canonicalize_with_fit(
                    value.clone(),
                    &fit,
                    defs,
                    purpose,
                    RejectEnumResolutionErrors,
                )
                .unwrap();
                assert_eq!(canonical, value);
                assert_eq!(render_value(&canonical).plain(), n.to_string());
            }
        }
    }

    #[test]
    fn canonicalize_recurses_into_struct_via_block_name() {
        let t = AttributeType::struct_(
            "Statement".to_string(),
            vec![
                crate::schema::CompiledStructField::optional(
                    "actions",
                    string_or_list_of_strings(),
                )
                .with_block_name("action")
                .build(),
            ],
        );
        let mut map = IndexMap::new();
        map.insert(
            "action".to_string(),
            Value::Concrete(ConcreteValue::String("s3:GetObject".into())),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("action"),
                    Some(&Value::Concrete(ConcreteValue::StringList(vec![
                        "s3:GetObject".into()
                    ])))
                );
            }
            _ => panic!("expected Map"),
        }
    }

    #[test]
    fn canonicalize_does_not_treat_provider_name_as_a_struct_value_key() {
        let t = AttributeType::struct_(
            "Statement".to_string(),
            vec![
                crate::schema::CompiledStructField::optional("action", string_or_list_of_strings())
                    .with_provider_name("Action")
                    .build(),
            ],
        );
        let value = Value::Concrete(ConcreteValue::Map(IndexMap::from([(
            "Action".to_string(),
            Value::Concrete(ConcreteValue::String("s3:GetObject".into())),
        )])));

        assert!(
            crate::schema::Schema::flat(t.clone())
                .validate(&value)
                .is_err(),
            "provider-side metadata names are not host value-map keys"
        );
        assert_eq!(
            canonicalize_state_with_type(
                value.clone(),
                &t,
                crate::schema::empty_defs_for_schema_walks(),
            ),
            value,
            "canonicalization must use the same accepted-key lookup as fit"
        );
    }

    /// carina#3080: `principal` is `Union[Struct{ service:
    /// Union[String, List<String>] }, String]`. The canonicalizer must
    /// recurse through the outer `Union` into the `Struct` member so
    /// the nested `string_or_list_of_strings` `service` field is folded
    /// to `StringList` — on both the bare-scalar (desired) and the
    /// singleton-list (aws-read) spelling.
    fn principal_union() -> AttributeType {
        AttributeType::union(vec![
            AttributeType::struct_(
                "PrincipalStruct".to_string(),
                vec![
                    crate::schema::CompiledStructField::optional(
                        "service",
                        string_or_list_of_strings(),
                    )
                    .build(),
                ],
            ),
            AttributeType::string(),
        ])
    }

    #[test]
    fn canonicalize_recurses_through_union_into_struct_scalar() {
        let t = principal_union();
        let mut map = IndexMap::new();
        map.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::String("cloudfront.amazonaws.com".into())),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("service"),
                    Some(&Value::Concrete(ConcreteValue::StringList(vec![
                        "cloudfront.amazonaws.com".into()
                    ])))
                );
            }
            _ => panic!("expected Map"),
        }
    }

    #[test]
    fn canonicalize_recurses_through_union_into_struct_singleton_list() {
        let t = principal_union();
        let mut map = IndexMap::new();
        map.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("cloudfront.amazonaws.com".into()),
            )])),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("service"),
                    Some(&Value::Concrete(ConcreteValue::StringList(vec![
                        "cloudfront.amazonaws.com".into()
                    ])))
                );
            }
            _ => panic!("expected Map"),
        }
    }

    #[test]
    fn canonicalize_recurses_through_union_ref_into_struct() {
        let t = AttributeType::union(vec![
            AttributeType::ref_("PrincipalStruct"),
            AttributeType::string(),
        ]);
        let defs = std::collections::BTreeMap::from([(
            "PrincipalStruct".to_string(),
            AttributeType::struct_(
                "PrincipalStruct".to_string(),
                vec![
                    crate::schema::CompiledStructField::optional(
                        "service",
                        string_or_list_of_strings(),
                    )
                    .build(),
                ],
            ),
        )]);
        let mut map = IndexMap::new();
        map.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::String("cloudfront.amazonaws.com".into())),
        );

        let canon =
            canonicalize_state_with_type(Value::Concrete(ConcreteValue::Map(map)), &t, &defs);
        let Value::Concrete(ConcreteValue::Map(map)) = canon else {
            panic!("expected Map");
        };
        assert_eq!(
            map.get("service"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "cloudfront.amazonaws.com".into()
            ])))
        );
    }

    #[test]
    fn canonicalize_union_ref_cycle_terminates_and_selects_non_recursive_struct() {
        let recursive = AttributeType::union(vec![
            AttributeType::struct_(
                "PrincipalStruct".to_string(),
                vec![
                    crate::schema::CompiledStructField::optional(
                        "service",
                        string_or_list_of_strings(),
                    )
                    .build(),
                ],
            ),
            AttributeType::ref_("A"),
        ]);
        let schema = crate::schema::Schema {
            root: AttributeType::union(vec![AttributeType::ref_("A"), AttributeType::bool()]),
            defs: std::collections::BTreeMap::from([("A".to_string(), recursive)]),
        };
        let mut map = IndexMap::new();
        map.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::String("cloudfront.amazonaws.com".into())),
        );
        let value = Value::Concrete(ConcreteValue::Map(map));

        assert!(schema.validate(&value).is_ok());
        let canon = schema
            .canonicalize_state(value)
            .into_present()
            .expect("recursive struct fixture must remain present");
        let Value::Concrete(ConcreteValue::Map(map)) = canon else {
            panic!("expected Map");
        };
        assert_eq!(
            map.get("service"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "cloudfront.amazonaws.com".into()
            ])))
        );
    }

    #[test]
    fn canonicalize_self_alias_union_ref_selects_string_list_sibling() {
        let schema = crate::schema::Schema {
            root: AttributeType::ref_("A"),
            defs: std::collections::BTreeMap::from([(
                "A".to_string(),
                AttributeType::union(vec![AttributeType::ref_("A"), string_or_list_of_strings()]),
            )]),
        };
        let value = Value::Concrete(ConcreteValue::String("x".into()));

        assert!(schema.validate(&value).is_ok());
        let canon = schema
            .canonicalize_state(value)
            .into_present()
            .expect("string fixture must remain present");
        assert_eq!(
            canon,
            Value::Concrete(ConcreteValue::StringList(vec!["x".into()]))
        );
    }

    #[test]
    fn canonicalize_mutual_union_ref_cycle_terminates() {
        let schema = crate::schema::Schema {
            root: AttributeType::ref_("A"),
            defs: std::collections::BTreeMap::from([
                (
                    "A".to_string(),
                    AttributeType::union(vec![AttributeType::ref_("B"), AttributeType::string()]),
                ),
                (
                    "B".to_string(),
                    AttributeType::union(vec![AttributeType::ref_("A"), AttributeType::int()]),
                ),
            ]),
        };
        let value = Value::Concrete(ConcreteValue::String("x".into()));

        assert_eq!(
            schema
                .canonicalize_state(value.clone())
                .into_present()
                .expect("mutual-ref fixture must remain present"),
            value
        );
    }

    #[test]
    fn union_selection_resets_ref_path_for_list_element_probe() {
        let schema = crate::schema::Schema {
            root: AttributeType::ref_("A"),
            defs: std::collections::BTreeMap::from([(
                "A".to_string(),
                AttributeType::union(vec![
                    AttributeType::list(AttributeType::int()),
                    AttributeType::list(AttributeType::ref_("A")),
                    AttributeType::string(),
                ]),
            )]),
        };
        let value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::List(vec![Value::Concrete(ConcreteValue::String("x".into()))]),
        )]));

        assert!(schema.validate(&value).is_ok());
        let union = schema.resolve("A").expect("A must be defined");
        let members = crate::schema::union_members_with_defs(union, &schema.defs)
            .expect("A must resolve to a Union");
        let mut visited_refs = vec!["A"];
        let selected = members
            .select_on_path(&value, &mut visited_refs)
            .expect("the recursive List member must match");
        let AttrTypeKind::List {
            element_type: inner,
            ..
        } = &selected.as_attr().kind
        else {
            panic!("the selected member must be a List")
        };
        assert!(matches!(&inner.kind, AttrTypeKind::Ref(name) if name == "A"));
    }

    #[test]
    fn canonicalize_recursive_list_union_agrees_with_validation() {
        let schema = crate::schema::Schema {
            root: AttributeType::ref_("A"),
            defs: std::collections::BTreeMap::from([(
                "A".to_string(),
                AttributeType::union(vec![
                    AttributeType::list(AttributeType::int()),
                    AttributeType::list(AttributeType::ref_("A")),
                    string_or_list_of_strings(),
                ]),
            )]),
        };
        let value = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::List(vec![Value::Concrete(ConcreteValue::String("x".into()))]),
        )]));

        assert!(schema.validate(&value).is_ok());
        assert_eq!(
            schema
                .canonicalize_state(value)
                .into_present()
                .expect("recursive list fixture must remain present"),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::List(vec![Value::Concrete(ConcreteValue::StringList(vec![
                    "x".into(),
                ]))]),
            )]))
        );
    }

    #[test]
    #[should_panic(expected = "schema invariant violated")]
    fn canonicalize_top_level_missing_ref_preserves_schema_invariant_panic() {
        let schema = crate::schema::Schema {
            root: AttributeType::ref_("Missing"),
            defs: std::collections::BTreeMap::new(),
        };

        let _ = schema.canonicalize_state(Value::Concrete(ConcreteValue::String("x".into())));
    }

    #[test]
    #[should_panic(expected = "cyclic AttributeType::Ref(\"A\") in schema defs")]
    fn canonicalize_top_level_ref_alias_cycle_preserves_schema_invariant_panic() {
        let schema = crate::schema::Schema {
            root: AttributeType::ref_("A"),
            defs: std::collections::BTreeMap::from([("A".to_string(), AttributeType::ref_("A"))]),
        };

        let _ = schema.canonicalize_state(Value::Concrete(ConcreteValue::String("x".into())));
    }

    #[test]
    fn canonicalize_ref_membered_string_or_list_union_in_desired_and_state_paths() {
        let attr_type =
            AttributeType::union(vec![AttributeType::ref_("S"), AttributeType::ref_("L")]);
        let defs = std::collections::BTreeMap::from([
            ("S".to_string(), AttributeType::string()),
            (
                "L".to_string(),
                AttributeType::list(AttributeType::string()),
            ),
        ]);
        let expected = Value::Concrete(ConcreteValue::StringList(vec![
            "cloudfront.amazonaws.com".into(),
        ]));

        for value in [
            Value::Concrete(ConcreteValue::String("cloudfront.amazonaws.com".into())),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("cloudfront.amazonaws.com".into()),
            )])),
        ] {
            assert_eq!(
                canonicalize_state_with_type(value.clone(), &attr_type, &defs),
                expected
            );
            assert_eq!(
                canonicalize_desired_with_type(
                    value.clone(),
                    &attr_type,
                    &defs,
                    DesiredValueSource::AuthoredDsl,
                )
                .unwrap(),
                expected
            );
            assert_eq!(
                canonicalize_desired_with_type(
                    value,
                    &attr_type,
                    &defs,
                    DesiredValueSource::ProviderOutput,
                )
                .unwrap(),
                expected
            );
        }

        let sentinel = redacted_secret(&"deadbeef".repeat(8));
        assert_eq!(
            canonicalize_state_with_type(sentinel.clone(), &attr_type, &defs),
            sentinel
        );
    }

    /// The `String` member of the same Union, given a bare string,
    /// passes through unchanged (it is not `string_or_list_of_strings`).
    #[test]
    fn canonicalize_union_string_member_passthrough() {
        let t = principal_union();
        let v = Value::Concrete(ConcreteValue::String("*".into()));
        let canon = canonicalize_state_with_type(
            v.clone(),
            &t,
            crate::schema::empty_defs_for_schema_walks(),
        );
        assert_eq!(canon, v);
    }

    /// A Union with no member whose shape matches the value → identity
    /// (safe fallthrough; never guess-coerce).
    #[test]
    fn canonicalize_union_no_matching_member_is_identity() {
        let t = AttributeType::union(vec![AttributeType::int(), AttributeType::bool()]);
        let v = Value::Concrete(ConcreteValue::String("not-an-int".into()));
        let canon = canonicalize_state_with_type(
            v.clone(),
            &t,
            crate::schema::empty_defs_for_schema_walks(),
        );
        assert_eq!(canon, v);
    }

    #[test]
    fn canonicalize_recurses_into_map_value_type() {
        let t = AttributeType::map_with_key(AttributeType::string(), string_or_list_of_strings());
        let mut map = IndexMap::new();
        map.insert(
            "token.actions.githubusercontent.com:sub".to_string(),
            Value::Concrete(ConcreteValue::String("repo:foo:*".into())),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("token.actions.githubusercontent.com:sub"),
                    Some(&Value::Concrete(ConcreteValue::StringList(vec![
                        "repo:foo:*".into()
                    ])))
                );
            }
            _ => panic!("expected Map"),
        }
    }

    #[test]
    fn canonicalize_string_or_list_union() {
        let t = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::String("x".into()));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(
            canon,
            Value::Concrete(ConcreteValue::StringList(vec!["x".into()]))
        );
    }

    #[test]
    fn canonicalize_secret_recurses_inner() {
        let t = string_or_list_of_strings();
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("s".into()),
        ))));
        let canon =
            canonicalize_state_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Deferred(DeferredValue::Secret(inner)) => {
                assert_eq!(
                    *inner,
                    Value::Concrete(ConcreteValue::StringList(vec!["s".into()]))
                );
            }
            _ => panic!("expected Secret"),
        }
    }

    #[test]
    fn canonicalize_value_to_json_string_list_serializes_as_array() {
        let v = Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()]));
        let json = value_to_json(&v).expect("StringList serializes cleanly");
        assert_eq!(
            json,
            serde_json::Value::Array(vec![
                serde_json::Value::String("a".to_string()),
                serde_json::Value::String("b".to_string()),
            ])
        );
    }

    // ----- inline_width tests (#2434) -----

    /// Parity oracle: every variant `inline_width` claims to know the
    /// width of must report the same byte length as `format_value` would
    /// emit. Anything else is a drift bug — the optimization
    /// silently changes the inline-vs-vertical decision boundary.
    fn assert_inline_width_matches_build(v: &Value) {
        // `render_value_pretty`'s overflow check uses byte length
        // (`inline.len()`), so `inline_width` must report bytes too.
        let built = format_value(v);
        let measured = inline_width(v, usize::MAX);
        assert_eq!(
            measured,
            Some(built.len()),
            "inline_width vs format_value drift for {v:?}: built={built:?}",
        );
    }

    #[test]
    fn inline_width_parity_for_scalars() {
        for v in [
            Value::Concrete(ConcreteValue::String("hello".into())),
            Value::Concrete(ConcreteValue::String("(empty)".into())),
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::Int(-7)),
            Value::Concrete(ConcreteValue::Float(1.5)),
            Value::Concrete(ConcreteValue::Float(2.0)),
            Value::Concrete(ConcreteValue::Bool(true)),
            Value::Concrete(ConcreteValue::Bool(false)),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("hidden".into()),
            )))),
        ] {
            assert_inline_width_matches_build(&v);
        }
    }

    #[test]
    fn inline_width_parity_for_lists_and_maps() {
        let list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::Bool(true)),
        ]));
        assert_inline_width_matches_build(&list);

        let mut map = IndexMap::new();
        map.insert(
            "k1".to_string(),
            Value::Concrete(ConcreteValue::String("v1".into())),
        );
        map.insert("k2".to_string(), Value::Concrete(ConcreteValue::Int(99)));
        assert_inline_width_matches_build(&Value::Concrete(ConcreteValue::Map(map)));

        // Nested list/map.
        let mut inner = IndexMap::new();
        inner.insert("x".to_string(), Value::Concrete(ConcreteValue::Int(1)));
        let nested = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(inner)),
            Value::Concrete(ConcreteValue::String("end".into())),
        ]));
        assert_inline_width_matches_build(&nested);

        // Empty collections.
        assert_inline_width_matches_build(&Value::Concrete(ConcreteValue::List(vec![])));
        assert_inline_width_matches_build(&Value::Concrete(ConcreteValue::Map(IndexMap::new())));
    }

    #[test]
    fn inline_width_parity_for_dotted_string() {
        // Dotted strings retain their complete text. `inline_width` must
        // still match the rendered byte length.
        let v = Value::Concrete(ConcreteValue::String("aws.Region.ap_northeast_1".into()));
        assert_inline_width_matches_build(&v);
    }

    #[test]
    fn inline_width_short_circuits_above_budget() {
        // Pin the optimization's *purpose*: a deeply-nested value that
        // obviously won't fit must return None *without* recursing into
        // every leaf. The cheapest observable proxy: a budget of 1 on a
        // value whose first byte already exceeds it returns None even
        // though the value itself is structurally complex.
        let mut deep = Value::Concrete(ConcreteValue::Int(1));
        for _ in 0..10 {
            deep = Value::Concrete(ConcreteValue::List(vec![deep]));
        }
        assert_eq!(
            inline_width(&deep, 1),
            None,
            "deeply-nested value must short-circuit on a tight budget"
        );
    }

    #[test]
    fn canonicalize_format_value_string_list() {
        let v = Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()]));
        assert_eq!(format_value(&v), "['a', 'b']");
    }

    #[test]
    fn canonicalize_partial_eq_distinguishes_list_and_string_list() {
        // `Value::Concrete(ConcreteValue::List([String("x")]))` and `Value::Concrete(ConcreteValue::StringList(vec!["x"]))`
        // are *not* equal under PartialEq — the type system carries the
        // canonical-form invariant. Producers must canonicalize first.
        let a = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("x".into()),
        )]));
        let b = Value::Concrete(ConcreteValue::StringList(vec!["x".into()]));
        assert_ne!(a, b);
    }

    // ---- canonicalize_resources_with_schemas tests (#2481, #2511) ----

    fn build_test_registry() -> crate::schema::SchemaRegistry {
        use crate::schema::SchemaRegistry;
        let mut reg = SchemaRegistry::new();
        let schema = PatternResourceSchema::new("iam.policy").attribute(
            PatternAttributeSchema::optional("subject", string_or_list_of_strings()).build(),
        );
        reg.insert(
            "aws",
            schema.validate().expect("schema fixture must be valid"),
        );
        reg
    }

    fn make_resource(attrs: Vec<(&str, Value)>) -> crate::resource::Resource {
        use crate::resource::{ExpandedResource as Resource, ResourceId};
        use std::collections::{HashMap, HashSet};
        let mut attributes = IndexMap::new();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v);
        }
        Resource {
            id: ResourceId::with_provider_identity("aws", "iam.policy", "p1", None),
            attributes,
            directives: Default::default(),
            resolved_ignore_changes: Default::default(),
            prefixes: HashMap::new(),
            binding: Some(absolute_binding("p1")),
            dependencies: Default::default(),
            module_source: None,
            default_tag_keys: Default::default(),
            loop_provenance: Default::default(),
            quoted_string_attrs: HashSet::new(),
        }
    }

    #[test]
    fn canonicalize_resources_with_schemas_scalar_to_string_list() {
        let registry = build_test_registry();
        let mut resources = vec![make_resource(vec![(
            "subject",
            Value::Concrete(ConcreteValue::String("repo:foo:*".into())),
        )])];
        canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();
        assert_eq!(
            resources[0].attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "repo:foo:*".into()
            ])))
        );
    }

    #[test]
    fn canonicalize_resources_with_schemas_single_list_to_string_list() {
        let registry = build_test_registry();
        let mut resources = vec![make_resource(vec![(
            "subject",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("repo:foo:*".into()),
            )])),
        )])];
        canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();
        assert_eq!(
            resources[0].attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "repo:foo:*".into()
            ])))
        );
    }

    #[test]
    fn canonicalize_resources_with_schemas_skips_unknown_resource() {
        // No schema registered for the resource type — pass through.
        let registry = crate::schema::SchemaRegistry::new();
        let mut resources = vec![make_resource(vec![(
            "subject",
            Value::Concrete(ConcreteValue::String("x".into())),
        )])];
        canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();
        assert_eq!(
            resources[0].attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::String("x".into())))
        );
    }

    #[test]
    fn canonicalize_resources_with_schemas_passes_through_unrelated_attr() {
        // Schema has only `subject`, but the resource has an extra
        // unknown attribute — leave it alone.
        let registry = build_test_registry();
        let mut resources = vec![make_resource(vec![
            (
                "subject",
                Value::Concrete(ConcreteValue::String("x".into())),
            ),
            ("name", Value::Concrete(ConcreteValue::String("p1".into()))),
        ])];
        canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();
        assert_eq!(
            resources[0].attributes.get("name"),
            Some(&Value::Concrete(ConcreteValue::String("p1".into())))
        );
    }

    #[test]
    fn canonicalize_resources_with_schemas_scalar_and_list_yield_same_value() {
        // The acceptance criterion from #2511: scalar literal and
        // single-element list literal produce byte-equal canonical
        // values once canonicalization runs.
        let registry = build_test_registry();
        let mut a = vec![make_resource(vec![(
            "subject",
            Value::Concrete(ConcreteValue::String("repo:foo:*".into())),
        )])];
        let mut b = vec![make_resource(vec![(
            "subject",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("repo:foo:*".into()),
            )])),
        )])];
        canonicalize_resources_with_schemas(&mut a, &registry).unwrap();
        canonicalize_resources_with_schemas(&mut b, &registry).unwrap();
        assert_eq!(a[0].attributes, b[0].attributes);
    }

    // ---- canonicalize_data_sources_with_schemas tests (carina#3181, #3740) ----

    #[test]
    fn canonicalize_data_sources_with_schemas_rewrites_authored_inputs() {
        use crate::resource::ExpandedDataSource as DataSource;
        use crate::schema::{SchemaRegistry, enum_identity};

        let mode = PatternAttributeType::enum_(
            enum_identity("Mode", Some("awscc.example.Widget")),
            Some(vec!["API_A".to_string(), "FOO".to_string()]),
            vec![("API_A".to_string(), "foo".to_string())],
            None,
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "awscc",
            (PatternResourceSchema::new("example.Widget")
                .as_data_source()
                .attribute(
                    PatternAttributeSchema::optional("subject", string_or_list_of_strings())
                        .build(),
                )
                .attribute(PatternAttributeSchema::optional("mode", mode).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );
        let mut data_source: DataSource =
            DataSource::with_provider("awscc", "example.Widget", "widget", None);
        data_source.set_attr(
            "subject",
            Value::Concrete(ConcreteValue::String("repo:foo:*".into())),
        );
        data_source.set_attr(
            "mode",
            Value::Concrete(ConcreteValue::enum_identifier("foo")),
        );
        let mut data_sources = vec![data_source];

        canonicalize_data_sources_with_schemas(&mut data_sources, &registry).unwrap();

        assert_eq!(
            data_sources[0].attributes["subject"],
            Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".into()])),
            "carina#3740: data-source attributes must use the shared structural walk"
        );
        assert!(
            matches!(
                &data_sources[0].attributes["mode"],
                Value::Concrete(ConcreteValue::CanonicalEnum(value))
                    if value.api_value() == "API_A"
            ),
            "carina#3740: authored data-source EnumIdentifier values use RawDsl alias semantics"
        );
    }

    // ---- canonicalize_states_with_schemas tests (#2481, #2513) ----

    fn make_state(attrs: Vec<(&str, Value)>) -> crate::resource::State {
        use crate::resource::{ResourceId, State};
        use std::collections::HashMap;
        let mut attributes = HashMap::new();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v);
        }
        State {
            id: ResourceId::with_provider_identity("aws", "iam.policy", "p1", None),
            identifier: Some("arn:aws:iam::123:policy/p1".to_string()),
            attributes,
            exists: true,
            dependency_bindings: Default::default(),
            partial_read: None,
        }
    }

    #[test]
    fn canonicalize_states_with_schemas_scalar_to_string_list() {
        let registry = build_test_registry();
        let mut states = std::collections::HashMap::new();
        let s = make_state(vec![(
            "subject",
            Value::Concrete(ConcreteValue::String("repo:foo:*".into())),
        )]);
        states.insert(s.id.clone(), crate::resource::Current::from_state(s));
        canonicalize_states_with_schemas(&mut states, &registry);
        let state = match states.values().next().unwrap() {
            crate::resource::Current::Absent(state) => state.state(),
            crate::resource::Current::Present(present) => present.state(),
            crate::resource::Current::CreateInFlight(_pending) => {
                panic!("test fixture cannot be in flight")
            }
            crate::resource::Current::CreateOutcomeUnknown(_unknown) => {
                panic!("test fixture cannot have an unknown create outcome")
            }
            crate::resource::Current::MissingIdentifier(state) => state.state(),
        };
        assert_eq!(
            state.attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "repo:foo:*".into()
            ])))
        );
    }

    #[test]
    fn canonicalize_states_with_schemas_legacy_list_to_string_list() {
        let registry = build_test_registry();
        let mut states = std::collections::HashMap::new();
        let s = make_state(vec![(
            "subject",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("repo:foo:*".into()),
            )])),
        )]);
        states.insert(s.id.clone(), crate::resource::Current::from_state(s));
        canonicalize_states_with_schemas(&mut states, &registry);
        let state = match states.values().next().unwrap() {
            crate::resource::Current::Absent(state) => state.state(),
            crate::resource::Current::Present(present) => present.state(),
            crate::resource::Current::CreateInFlight(_pending) => {
                panic!("test fixture cannot be in flight")
            }
            crate::resource::Current::CreateOutcomeUnknown(_unknown) => {
                panic!("test fixture cannot have an unknown create outcome")
            }
            crate::resource::Current::MissingIdentifier(state) => state.state(),
        };
        assert_eq!(
            state.attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "repo:foo:*".into()
            ])))
        );
    }

    #[test]
    fn canonicalize_states_with_schemas_preserves_secret_hash_scalar() {
        let registry = build_test_registry();
        let secret_hash = redacted_secret(&"deadbeef".repeat(8));
        let mut states = std::collections::HashMap::new();
        let state = make_state(vec![("subject", secret_hash.clone())]);
        states.insert(
            state.id.clone(),
            crate::resource::Current::from_state(state),
        );

        canonicalize_states_with_schemas(&mut states, &registry);

        let state = match states.values().next().unwrap() {
            crate::resource::Current::Absent(state) => state.state(),
            crate::resource::Current::Present(present) => present.state(),
            crate::resource::Current::CreateInFlight(_pending) => {
                panic!("test fixture cannot be in flight")
            }
            crate::resource::Current::CreateOutcomeUnknown(_unknown) => {
                panic!("test fixture cannot have an unknown create outcome")
            }
            crate::resource::Current::MissingIdentifier(state) => state.state(),
        };
        assert_eq!(
            state.attributes["subject"], secret_hash,
            "carina#3740: a current-state secret hash is an opaque scalar sentinel, never a list"
        );
    }

    #[test]
    fn canonicalize_states_with_schemas_skips_unknown_resource() {
        let registry = crate::schema::SchemaRegistry::new();
        let mut states = std::collections::HashMap::new();
        let s = make_state(vec![(
            "subject",
            Value::Concrete(ConcreteValue::String("x".into())),
        )]);
        states.insert(s.id.clone(), crate::resource::Current::from_state(s));
        canonicalize_states_with_schemas(&mut states, &registry);
        let state = match states.values().next().unwrap() {
            crate::resource::Current::Absent(state) => state.state(),
            crate::resource::Current::Present(present) => present.state(),
            crate::resource::Current::CreateInFlight(_pending) => {
                panic!("test fixture cannot be in flight")
            }
            crate::resource::Current::CreateOutcomeUnknown(_unknown) => {
                panic!("test fixture cannot have an unknown create outcome")
            }
            crate::resource::Current::MissingIdentifier(state) => state.state(),
        };
        assert_eq!(
            state.attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::String("x".into())))
        );
    }

    #[test]
    fn canonicalize_states_diff_empty_after_both_sides_canonical() {
        // The acceptance criterion from #2513: a desired side written
        // as `["x"]` and a state side stored as `"x"` collapse to the
        // same `Value::Concrete(ConcreteValue::StringList(vec!["x"]))` after both pass through
        // canonicalization.
        let registry = build_test_registry();

        let mut resources = vec![make_resource(vec![(
            "subject",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("repo:foo:*".into()),
            )])),
        )])];
        canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();

        let mut states = std::collections::HashMap::new();
        let s = make_state(vec![(
            "subject",
            Value::Concrete(ConcreteValue::String("repo:foo:*".into())),
        )]);
        states.insert(s.id.clone(), crate::resource::Current::from_state(s));
        canonicalize_states_with_schemas(&mut states, &registry);

        let state = match states.values().next().unwrap() {
            crate::resource::Current::Absent(state) => state.state(),
            crate::resource::Current::Present(present) => present.state(),
            crate::resource::Current::CreateInFlight(_pending) => {
                panic!("test fixture cannot be in flight")
            }
            crate::resource::Current::CreateOutcomeUnknown(_unknown) => {
                panic!("test fixture cannot have an unknown create outcome")
            }
            crate::resource::Current::MissingIdentifier(state) => state.state(),
        };
        assert_eq!(
            resources[0].attributes.get("subject"),
            state.attributes.get("subject"),
        );
    }

    #[test]
    fn canonicalize_pipeline_dynamic_az_enum_state_api_spelling_has_no_diff() {
        use crate::differ::{Diff, diff_test};
        use crate::resource::{ExpandedResource as Resource, ResourceId, State};
        use crate::schema::SchemaRegistry;
        use std::collections::HashMap;

        let az_enum = PatternAttributeType::enum_(
            crate::schema::enum_identity("ZoneName", Some("aws.AvailabilityZone")),
            None,
            vec![],
            None,
            Some(crate::schema::DslTransform::HyphenToUnderscore),
        );
        let schema = PatternResourceSchema::new("ec2.Subnet")
            .attribute(PatternAttributeSchema::optional("availability_zone", az_enum).build());
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "awscc",
            (schema.clone())
                .validate()
                .expect("schema fixture must be valid"),
        );

        let id = ResourceId::with_provider_identity("awscc", "ec2.Subnet", "subnet", None);
        let mut desired = vec![
            Resource::with_provider("awscc", "ec2.Subnet", "subnet", None).with_attribute(
                "availability_zone",
                Value::Concrete(ConcreteValue::enum_identifier(
                    "ap_northeast_1a".to_string(),
                )),
            ),
        ];
        canonicalize_resources_with_schemas(&mut desired, &registry).unwrap();

        let mut attrs = HashMap::new();
        attrs.insert(
            "availability_zone".to_string(),
            Value::Concrete(ConcreteValue::String("ap-northeast-1a".into())),
        );
        let mut states = HashMap::new();
        states.insert(
            id.clone(),
            crate::resource::Current::from_state(State::existing(id, attrs)),
        );
        canonicalize_states_with_schemas(&mut states, &registry);

        let current = match states.values().next().expect("state exists") {
            crate::resource::Current::Absent(state) => state.state(),
            crate::resource::Current::Present(present) => present.state(),
            crate::resource::Current::CreateInFlight(_pending) => {
                panic!("test fixture cannot be in flight")
            }
            crate::resource::Current::CreateOutcomeUnknown(_unknown) => {
                panic!("test fixture cannot have an unknown create outcome")
            }
            crate::resource::Current::MissingIdentifier(state) => state.state(),
        };
        let result = diff_test(
            &desired[0],
            current,
            None,
            None,
            Some(
                registry
                    .get(
                        "awscc",
                        schema.resource_type(),
                        crate::schema::SchemaKind::Resource,
                    )
                    .unwrap()
                    .as_schema(),
            ),
            &crate::differ::exact_comparison_schema_for_tests(),
        );
        assert!(
            matches!(result, Diff::NoChange(_)),
            "dynamic AZ enum API spelling in state must canonicalize to DSL spelling; got {result:?}"
        );
    }

    /// carina#3080 end-to-end via the REAL pipeline entry
    /// (`canonicalize_*_with_schemas`), NOT the `Union` arm directly
    /// (`feedback_unit_test_path_is_not_apply_path`). `principal` is
    /// `Union[Struct{ service: Union[String, List<String>] }, String]`.
    /// Desired holds the bare scalar; state holds the aws-read
    /// singleton list. After both pass through the pipeline they must
    /// be byte-identical `StringList` — the
    /// `differ/comparison.rs:28-47` invariant ("non-canonical reaching
    /// the differ is a bug") is then satisfied, not worked around.
    #[test]
    fn canonicalize_pipeline_folds_union_nested_string_or_list_both_sides() {
        use crate::schema::SchemaRegistry;
        let principal = PatternAttributeType::union(vec![
            PatternAttributeType::struct_(
                "PrincipalStruct".to_string(),
                vec![PatternStructField::optional("service", string_or_list_of_strings()).build()],
            ),
            PatternAttributeType::string(),
        ]);
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            (PatternResourceSchema::new("iam.policy")
                .attribute(PatternAttributeSchema::optional("principal", principal).build()))
            .validate()
            .expect("schema fixture must be valid"),
        );

        // Desired: bare scalar inside the Struct member.
        let mut desired_inner = IndexMap::new();
        desired_inner.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::String("cloudfront.amazonaws.com".into())),
        );
        let mut resources = vec![make_resource(vec![(
            "principal",
            Value::Concrete(ConcreteValue::Map(desired_inner)),
        )])];
        canonicalize_resources_with_schemas(&mut resources, &registry).unwrap();

        // State: aws-read singleton list inside the Struct member.
        let mut state_inner = IndexMap::new();
        state_inner.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("cloudfront.amazonaws.com".into()),
            )])),
        );
        let mut states = std::collections::HashMap::new();
        let s = make_state(vec![(
            "principal",
            Value::Concrete(ConcreteValue::Map(state_inner)),
        )]);
        states.insert(s.id.clone(), crate::resource::Current::from_state(s));
        canonicalize_states_with_schemas(&mut states, &registry);

        let state = match states.values().next().unwrap() {
            crate::resource::Current::Absent(state) => state.state(),
            crate::resource::Current::Present(present) => present.state(),
            crate::resource::Current::CreateInFlight(_pending) => {
                panic!("test fixture cannot be in flight")
            }
            crate::resource::Current::CreateOutcomeUnknown(_unknown) => {
                panic!("test fixture cannot have an unknown create outcome")
            }
            crate::resource::Current::MissingIdentifier(state) => state.state(),
        };
        let expected = {
            let mut m = IndexMap::new();
            m.insert(
                "service".to_string(),
                Value::Concrete(ConcreteValue::StringList(vec![
                    "cloudfront.amazonaws.com".into(),
                ])),
            );
            Value::Concrete(ConcreteValue::Map(m))
        };
        assert_eq!(resources[0].attributes.get("principal"), Some(&expected));
        assert_eq!(state.attributes.get("principal"), Some(&expected));
        assert_eq!(
            resources[0].attributes.get("principal"),
            state.attributes.get("principal"),
            "carina#3080: both sides must collapse to the same StringList \
             via the real pipeline so the differ sees no phantom"
        );
    }

    // ---- LSP / display catch-up tests (#2481, #2514) ----

    #[test]
    fn validate_list_accepts_string_list() {
        // The schema's `validate_list` must accept `Value::Concrete(ConcreteValue::StringList)`
        // as the structural equivalent of `Value::Concrete(ConcreteValue::List([String, ...]))`,
        // so a Union[String, list(String)] member's `list(String)`
        // branch validates the canonical form cleanly.
        use crate::schema::CompiledAttributeType as AttributeType;
        let list_of_string = AttributeType::list(AttributeType::string());
        let v = Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()]));
        assert!(list_of_string.validate(&v).is_ok());
    }

    #[test]
    fn validate_union_accepts_canonical_string_list() {
        let union = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::StringList(vec!["x".into()]));
        assert!(union.validate(&v).is_ok());
    }

    #[test]
    fn format_value_string_list_renders_brackets() {
        // `format_value` must render `Value::Concrete(ConcreteValue::StringList)` with bracket
        // syntax — not a `_` wildcard fallback that would print debug
        // garbage in plan output.
        let special = "quote \" slash \\ newline\n interpolation ${value}";
        let v = Value::Concrete(ConcreteValue::StringList(vec!["a".into(), special.into()]));
        let formatted = format_value(&v);
        assert_eq!(
            formatted,
            format!(
                "[{}, {}]",
                crate::parser::display_string_literal("a"),
                crate::parser::display_string_literal(special)
            )
        );
    }

    #[test]
    fn format_value_interpolation_escapes_literal_parts() {
        let value = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("say \"hello\"".into()),
            InterpolationPart::Expr(Value::root_resource_ref("server", "name", Vec::new())),
        ]));

        assert_eq!(format_value(&value), "\"say \\\"hello\\\"${server.name}\"");
    }

    #[test]
    fn inline_width_returns_none_when_just_over_budget() {
        // List of three short strings: `["aa", "bb", "cc"]` = 18 bytes.
        // Budget 17 must return None; budget 18 must return Some(18).
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("aa".into())),
            Value::Concrete(ConcreteValue::String("bb".into())),
            Value::Concrete(ConcreteValue::String("cc".into())),
        ]));
        assert_eq!(
            format_value_with_key(&v, None).len(),
            18,
            "fixture sanity: rendered width should be 18 bytes"
        );
        assert_eq!(inline_width(&v, 17), None);
        assert_eq!(inline_width(&v, 18), Some(18));
        assert_eq!(inline_width(&v, 100), Some(18));
    }

    #[test]
    fn render_value_pretty_decision_unchanged_after_inline_width() {
        // End-to-end: the inline-vs-vertical decision in
        // `render_value_pretty` must be byte-identical to the pre-fix
        // build-then-measure shape. Construct values that straddle
        // PRETTY_LINE_LIMIT (80) at the prefix boundary and confirm both
        // sides of the boundary still pick the same form.
        // Just under the limit (inline expected).
        let small_list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("aaaaaaaaaaaa".into())),
            Value::Concrete(ConcreteValue::String("bbbbbbbbbbbb".into())),
        ]));
        let small = render_value_pretty(&small_list, layout(0, "k"));
        assert!(
            !small.plain().contains('\n'),
            "small list must render inline, got: {small:?}"
        );

        // Over the limit (vertical expected).
        let big_list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".repeat(40).into())),
            Value::Concrete(ConcreteValue::String("b".repeat(40).into())),
            Value::Concrete(ConcreteValue::String("c".repeat(40).into())),
        ]));
        let big = render_value_pretty(&big_list, layout(0, "k"));
        assert!(
            big.plain().contains('\n'),
            "oversize list must render vertically, got: {big:?}"
        );
    }

    /// Carina#3329: `plan --out` on the supported scenario
    /// — `import { id = "${upstream.attr}|tail" }` where `upstream.attr`
    /// is still deferred at plan time — must succeed and persist the
    /// deferred `Unknown` placeholder through `redact_secrets_in_plan`.
    /// A previous iteration of the fix routed through
    /// `redact_secrets_in_value` directly and tripped its strict
    /// "Unknown is not serializable" guard (RFC #2371 stage 4),
    /// causing `plan --out` to fail on the exact scenario the PR
    /// targets. The dedicated `redact_secrets_only` walker keeps the
    /// Secret-redaction behavior while passing `Unknown` through.
    #[test]
    fn redact_secrets_in_effect_passes_unknown_through_import_identifier() {
        use crate::effect::Effect;
        use crate::resource::{
            AccessPath, ApplyTimeUnknown, DeferredValue, InterpolationPart, ResourceId, Value,
        };
        let path = AccessPath::root_with_fields(
            "management_route53",
            "apex_zone_id",
            Vec::<String>::new(),
        );
        let identifier = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                ApplyTimeUnknown::UpstreamExport { path },
            ))),
            InterpolationPart::Literal("|registry-dev.carina-rs.dev|NS".into()),
        ]));
        let effect = Effect::Import {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "aws.route53.RecordSet",
                "r.delegation_ns",
            )),
            identifier,
        };
        let redacted = redact_secrets_in_effect(&effect)
            .expect("redaction must not error on a deferred Unknown inside an import identifier");
        match redacted {
            Effect::Import { identifier, .. } => {
                // Round-trip preserves the Unknown placeholder verbatim
                // for the saved-plan apply path to encounter.
                assert!(
                    matches!(identifier, Value::Deferred(DeferredValue::Interpolation(_))),
                    "identifier shape must survive redaction, got: {identifier:?}",
                );
            }
            other => panic!("expected Effect::Import, got {other:?}"),
        }
    }

    /// Carina#3329: a secret value reaching `Effect::Import.identifier`
    /// — possible when the import id is `"${secret_let.value}|tail"` —
    /// must be redacted in the saved-plan / persisted form just like
    /// every other secret-bearing leaf. The pre-#3329 clone() bypassed
    /// the redactor entirely because the field was a plain `String`;
    /// now that it carries a `Value`, the redactor walks the
    /// interpolation parts and replaces the secret with its hash.
    #[test]
    fn redact_secrets_in_effect_walks_import_identifier_interpolation() {
        use crate::effect::Effect;
        use crate::resource::{
            ExpandedConcreteValue as ConcreteValue, ExpandedDeferredValue as DeferredValue,
            ExpandedInterpolationPart as InterpolationPart, ExpandedValue as Value, ResourceId,
        };
        let secret_inner = Value::Concrete(ConcreteValue::String("super-secret".into()));
        let identifier = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Secret(Box::new(
                secret_inner,
            )))),
            InterpolationPart::Literal("|tail".into()),
        ]));
        let effect = Effect::Import {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "aws.s3.Bucket",
                "b",
            )),
            identifier,
        };
        let redacted = redact_secrets_in_effect(&effect).expect("redaction succeeds");
        match redacted {
            Effect::Import { identifier, .. } => {
                // The secret must no longer be reachable as plaintext.
                let rendered = format_value_with_key(&identifier, None);
                assert!(
                    !rendered.contains("super-secret"),
                    "redacted import identifier must not contain plaintext secret, got: {rendered}",
                );
                // The redactor emits a typed placeholder for the secret leaf.
                fn walk(v: &Value) -> bool {
                    use crate::resource::{
                        ExpandedDeferredValue as DeferredValue,
                        ExpandedInterpolationPart as InterpolationPart, ExpandedValue as Value,
                    };
                    match v {
                        Value::Deferred(DeferredValue::RedactedSecret { .. }) => true,
                        Value::Deferred(DeferredValue::Interpolation(parts)) => {
                            parts.iter().any(|p| match p {
                                InterpolationPart::Expr(inner) => walk(inner),
                                InterpolationPart::Literal(_) => false,
                            })
                        }
                        _ => false,
                    }
                }
                assert!(
                    walk(&identifier),
                    "redacted identifier must carry a typed placeholder, got: {identifier:?}",
                );
            }
            other => panic!("expected Effect::Import, got {other:?}"),
        }
    }

    #[test]
    fn redact_secrets_in_deferred_create_redacts_and_rehydrates_template() {
        use crate::effect::Effect;
        use crate::parser::{DeferredForExpression, ForBinding};
        use crate::plan::Plan;
        use crate::resource::PathSegment;

        let placeholder_inner = crate::resource::apply_postfix_access(
            LoopValue::Deferred(LoopDeferredValue::Placeholder(LoopSlot::Value)),
            PathSegment::Field {
                name: "resource_record".to_string(),
            },
            0,
            crate::resource::AccessMissPolicy::Error,
        )
        .expect("unknown access remains deferred");
        let placeholder = crate::resource::apply_postfix_access(
            placeholder_inner,
            PathSegment::Field {
                name: "name".to_string(),
            },
            0,
            crate::resource::AccessMissPolicy::Error,
        )
        .expect("unknown access remains deferred");
        let secret = LoopValue::Deferred(LoopDeferredValue::Secret(Box::new(LoopValue::Concrete(
            crate::resource::ConcreteValue::String("DEFERRED_TEMPLATE_PLAINTEXT".into()),
        ))));
        let mut template_resource: crate::resource::Resource<
            crate::resource::ModuleSource,
            LoopSlot,
        > = crate::resource::Resource::new("aws.route53.RecordSet", "validation_records");
        template_resource.set_attr("name", placeholder.clone());
        template_resource.set_attr("comment", secret.clone());
        let effect = Effect::DeferredCreate(crate::test_support::compiled_loop(Box::new(
            DeferredForExpression::new(
                None,
                1,
                "for opt in cert.domain_validation_options".to_string(),
                crate::parser::DeferredForTarget::bound(
                    crate::resource::absolute_binding_address_for_test("validation_records"),
                )
                .expect("fixture binding is valid"),
                Value::root_resource_ref("cert", "domain_validation_options", Vec::new()),
                ForBinding::Simple("opt".to_string()),
                template_resource,
            ),
        )));

        let redacted = redact_secrets_in_effect(&effect).expect("redact deferred secret");
        let Effect::DeferredCreate(redacted_payload) = &redacted else {
            panic!("expected deferred create");
        };
        let redacted_template = &redacted_payload.template;
        assert!(matches!(
            redacted_template.attributes().get("comment"),
            Some(LoopValue::Deferred(
                LoopDeferredValue::RedactedSecret { .. }
            ))
        ));

        assert!(matches!(
            redacted_template
                .template_resource()
                .attributes()
                .get("comment"),
            Some(LoopValue::Deferred(
                LoopDeferredValue::RedactedSecret { .. }
            ))
        ));
        let mut plan = Plan::new();
        plan.add(effect.clone());
        let redacted_plan = redact_secrets_in_plan(&plan).expect("redact deferred plan");
        let Effect::DeferredCreate(payload) = &effect else {
            unreachable!();
        };
        let deferred_templates =
            resolved_deferred_templates([crate::parser::DeferredForExpression::new(
                payload.template.file().map(str::to_owned),
                payload.template.line(),
                payload.template.header.clone(),
                payload.template.target().clone(),
                payload.template.iterable.clone(),
                payload.template.binding.clone(),
                payload
                    .template
                    .template_resource()
                    .as_resource()
                    .clone()
                    .with_dependency_phase(Default::default()),
            )]);
        let rehydrated = rehydrate_plan_desired_secrets(
            &redacted_plan,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &deferred_templates,
        )
        .expect("unchanged deferred template secret should rehydrate");
        let Effect::DeferredCreate(payload) = &rehydrated.effects()[0] else {
            panic!("expected rehydrated deferred create");
        };
        let template = &payload.template;
        assert!(matches!(
            template.attributes().get("comment"),
            Some(LoopValue::Deferred(LoopDeferredValue::Secret(_)))
        ));
        assert!(matches!(
            template.template_resource().attributes().get("comment"),
            Some(LoopValue::Deferred(LoopDeferredValue::Secret(_)))
        ));
        assert!(!template.attributes().values().any(contains_redacted_secret));
    }

    #[test]
    fn deferred_template_redaction_uses_template_resource_as_single_projection() {
        use crate::effect::Effect;
        use crate::parser::{DeferredForExpression, ForBinding};

        let secret = |plaintext: &str| {
            LoopValue::Deferred(LoopDeferredValue::Secret(Box::new(LoopValue::Concrete(
                crate::resource::ConcreteValue::String(plaintext.into()),
            ))))
        };
        let mut template_resource: crate::resource::Resource<
            crate::resource::ModuleSource,
            LoopSlot,
        > = crate::resource::Resource::new("aws.route53.RecordSet", "records");
        template_resource.set_attr("comment", secret("authoritative"));
        let effect = Effect::DeferredCreate(crate::test_support::compiled_loop(Box::new(
            DeferredForExpression::new(
                None,
                1,
                "for opt in cert.options".to_string(),
                crate::parser::DeferredForTarget::bound(
                    crate::resource::absolute_binding_address_for_test("records"),
                )
                .expect("fixture binding is valid"),
                Value::root_resource_ref("cert", "options", Vec::new()),
                ForBinding::Simple("opt".to_string()),
                template_resource,
            ),
        )));

        let redacted = redact_secrets_in_effect(&effect).expect("redact deferred template");
        let Effect::DeferredCreate(payload) = redacted else {
            panic!("expected deferred create");
        };
        let template = payload.template;
        let canonical = template
            .attributes()
            .get("comment")
            .expect("template resource comment");

        let LoopValue::Deferred(LoopDeferredValue::RedactedSecret {
            hash: canonical_hash,
        }) = canonical
        else {
            panic!("expected canonical placeholder, got {canonical:?}");
        };
        let LoopValue::Deferred(LoopDeferredValue::RedactedSecret {
            hash: projected_hash,
        }) = template
            .template_resource()
            .attributes()
            .get("comment")
            .expect("canonical comment")
        else {
            panic!("canonical template must retain the redacted secret");
        };
        assert_eq!(
            canonical_hash, projected_hash,
            "the display accessor must use the canonical template resource"
        );
    }

    #[test]
    fn deferred_template_preserves_display_only_projection_and_noops_without_placeholders() {
        use crate::effect::{DeferredCreatePayload, Effect};
        use crate::parser::{DeferredForExpression, ForBinding};

        let canonical =
            LoopValue::Concrete(crate::resource::ConcreteValue::String("canonical".into()));
        let display_only = LoopValue::Concrete(crate::resource::ConcreteValue::String(
            "display-only".into(),
        ));
        let mut template_resource: crate::resource::Resource<
            crate::resource::ModuleSource,
            LoopSlot,
        > = crate::resource::Resource::new("mock.test.resource", "children");
        template_resource.set_attr("comment", canonical.clone());
        template_resource.set_attr("identifier", display_only.clone());
        let template = DeferredForExpression::new(
            None,
            1,
            "for child in parent.children".to_string(),
            crate::parser::DeferredForTarget::bound(
                crate::resource::absolute_binding_address_for_test("children"),
            )
            .expect("fixture binding is valid"),
            Value::root_resource_ref("parent", "children", Vec::new()),
            ForBinding::Simple("child".to_string()),
            template_resource,
        );
        let effect = Effect::DeferredCreate(crate::test_support::compiled_loop(Box::new(
            template.clone(),
        )));

        let redacted = redact_secrets_in_effect(&effect).expect("redact placeholder-free template");
        let Effect::DeferredCreate(redacted_payload) = redacted else {
            panic!("expected deferred create")
        };
        let redacted_template = redacted_payload.template;
        assert_eq!(
            redacted_template.as_ref(),
            &template.clone().into_compiled()
        );

        let mut plan = crate::plan::Plan::new();
        plan.add(Effect::DeferredCreate(DeferredCreatePayload::new(
            redacted_template,
            redacted_payload.dependency_template,
        )));
        let deferred_templates = resolved_deferred_templates([]);
        let rehydrated = rehydrate_plan_desired_secrets(
            &plan,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &deferred_templates,
        )
        .expect("placeholder-free template must not need a candidate");
        let Effect::DeferredCreate(payload) = &rehydrated.effects()[0] else {
            panic!("expected deferred create")
        };
        let output = &payload.template;
        assert_eq!(output.as_ref(), &template.clone().into_compiled());
        assert_eq!(output.attributes().get("identifier"), Some(&display_only),);
    }

    fn replacement_plan_with_previous_attributes(
        previous_attributes: std::collections::HashMap<String, Value>,
        permanent_name_override: Option<crate::plan::PermanentNameOverride>,
    ) -> crate::plan::Plan {
        use crate::effect::ChangedCreateOnly;
        use crate::plan::{ReplacementDelete, ReplacementGroup};
        use crate::resource::{
            ExpandedDirectives as Directives, ExpandedResource as Resource, ResolvedResource,
            ResolvedResourceId, ResourceId,
        };
        let id = ResourceId::with_identity("aws.s3.Bucket", "bucket");
        let mut create = Resource::new("aws.s3.Bucket", "bucket");
        create.binding = Some(absolute_binding("bucket"));

        let mut plan = crate::plan::Plan::new();
        plan.add_replacement(ReplacementGroup {
            create: ResolvedResource::new(create.with_dependency_phase(())),
            delete: ReplacementDelete {
                id: ResolvedResourceId::new(id.clone()),
                identifier: crate::effect::fixture_applied_identifier("bucket-old"),
                directives: Directives::default(),
                binding: Some(absolute_binding("bucket")),
                dependencies: Default::default(),
                explicit_dependencies: Default::default(),
            },
            create_before_destroy: true,
            changed_create_only: ChangedCreateOnly::new(vec!["bucket_name".to_string()])
                .expect("test replacement has a create-only attribute"),
            cascade_ref_hints: vec![("vpc_id".to_string(), "vpc.id".to_string())],
            temporary_name: None,
            permanent_name_override,
            consumer_updates: Default::default(),
            previous_attributes,
        });
        plan
    }

    #[test]
    fn redact_secrets_in_plan_preserves_replace_display() {
        use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};
        use std::collections::HashMap;

        let plan = replacement_plan_with_previous_attributes(
            HashMap::from([(
                "bucket_name".to_string(),
                Value::Concrete(ConcreteValue::String("bucket-old".into())),
            )]),
            None,
        );
        let expected = plan.replace_display.clone();

        let redacted = redact_secrets_in_plan(&plan).expect("plan redaction succeeds");

        assert_eq!(redacted.replace_display, expected);
    }

    #[test]
    fn redact_secrets_in_plan_preserves_permanent_name_overrides() {
        use crate::plan::PermanentNameOverride;
        use crate::resource::{ResolvedResourceId, ResourceId};
        use std::collections::HashMap;

        let override_ = PermanentNameOverride {
            resource_id: ResolvedResourceId::new(ResourceId::with_identity(
                "aws.s3.Bucket",
                "bucket",
            )),
            attribute: "bucket_name".to_string(),
            temp_value: "bucket-temp".to_string(),
            original_value: Some("bucket".to_string()),
        };
        let plan = replacement_plan_with_previous_attributes(HashMap::new(), Some(override_));
        let expected = plan.permanent_name_overrides.clone();

        let redacted = redact_secrets_in_plan(&plan).expect("plan redaction succeeds");

        assert_eq!(redacted.permanent_name_overrides, expected);
    }

    #[test]
    fn redact_secrets_in_plan_redacts_secret_in_previous_attributes() {
        use crate::resource::{
            ExpandedConcreteValue as ConcreteValue, ExpandedDeferredValue as DeferredValue,
            ExpandedValue as Value,
        };
        use std::collections::HashMap;

        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("previous-secret".into()),
        ))));
        let plan = replacement_plan_with_previous_attributes(
            HashMap::from([("password".to_string(), secret)]),
            None,
        );

        let redacted = redact_secrets_in_plan(&plan).expect("plan redaction succeeds");
        let value = redacted.replace_display[0]
            .previous_attributes
            .get("password")
            .expect("previous password attribute");

        assert!(matches!(
            value,
            Value::Deferred(DeferredValue::RedactedSecret { .. })
        ));
    }
}
