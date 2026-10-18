//! Value conversion and formatting utilities

use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;

use indexmap::IndexMap;
use thiserror::Error;

use crate::resource::{
    CanonicalEnumValue, ConcreteValue, DeferredValue, InterpolationPart, RedactedSecretHash,
    UnknownReason, Value,
};
use crate::schema::{AttrTypeKind, AttributeType, TypeIdentity};

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
/// [`UnknownReason`] (rather than a stringified rendition) so the
/// top-level CLI handler can build an actionable diagnostic — e.g.
/// it can mention the specific upstream path or the for-binding kind
/// that produced the placeholder, without re-parsing a flattened
/// message string.
#[derive(Debug, Error)]
pub enum SerializationError {
    /// A `Value::Deferred(DeferredValue::Unknown)` reached a serialization boundary. Producers
    /// must strip / resolve it before this point — see
    /// [`crate::resource::value_contains_unknown`] for recursive
    /// detection before crossing a serialization boundary.
    #[error("cannot serialize at {context}: value is not yet known ({reason})")]
    UnknownNotAllowed {
        reason: UnknownReason,
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
    /// structured `AccessPath` (cf. `UnknownReason::UpstreamExport`)
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
    /// A saved-plan placeholder reached a runtime serialization boundary
    /// without first being rehydrated and verified.
    #[error("cannot serialize at {context}: saved-plan secret was not rehydrated")]
    RedactedSecretNotAllowed { context: SerializationContext },
}

impl SerializationError {
    /// Mask user-derived payload text when an error originated below a
    /// [`DeferredValue::Secret`] wrapper.
    #[must_use]
    pub fn masked_for_secret(self) -> Self {
        const MASKED: &str = "(secret)";

        match self {
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
#[derive(Debug, Error)]
#[error("{location}: {source}")]
pub struct SecretRehydrationSiteError {
    pub location: String,
    #[source]
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

impl std::fmt::Display for UnknownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnknownReason::UpstreamExport { path } => {
                write!(f, "upstream value {}", path.to_dot_string())
            }
            UnknownReason::UpstreamAllExports { binding } => {
                write!(f, "upstream value {}", binding)
            }
            UnknownReason::ForKey => write!(f, "deferred for-binding key"),
            UnknownReason::ForIndex => write!(f, "deferred for-binding index"),
            UnknownReason::ForValue => write!(f, "deferred for-binding value"),
            UnknownReason::FnParam { name } => write!(f, "function parameter {name}"),
            UnknownReason::FnLocal { name } => write!(f, "function local {name}"),
            UnknownReason::ForValuePath { path } => {
                write!(f, "deferred for-binding value {}", path.to_dot_string())
            }
            UnknownReason::EmptyInterpolation => write!(f, "empty interpolation"),
            UnknownReason::PostCreateReadIncomplete { detail } => {
                write!(f, "post-create read failed: {detail}")
            }
        }
    }
}

/// Render an `UnknownReason` to its plan-display string.
pub fn render_unknown(reason: &UnknownReason) -> String {
    match reason {
        UnknownReason::UpstreamExport { path } => {
            format!("(known after upstream apply: {})", path.to_dot_string())
        }
        UnknownReason::UpstreamAllExports { binding } => {
            format!("(known after upstream apply: {})", binding)
        }
        UnknownReason::ForKey => "(known after upstream apply: key)".to_string(),
        UnknownReason::ForIndex => "(known after upstream apply: index)".to_string(),
        UnknownReason::ForValue => "(known after upstream apply)".to_string(),
        UnknownReason::FnParam { name } => format!("(unresolved function parameter: {name})"),
        UnknownReason::FnLocal { name } => format!("(unresolved function local: {name})"),
        UnknownReason::ForValuePath { path } => {
            format!("(known after upstream apply: {})", path.to_dot_string())
        }
        UnknownReason::EmptyInterpolation => "(empty interpolation)".to_string(),
        UnknownReason::PostCreateReadIncomplete { detail } => {
            format!("(known after next apply: post-create read failed — {detail})")
        }
    }
}

/// Secret value prefix used in state serialization.
pub const SECRET_PREFIX: &str = RedactedSecretHash::V2_MARKER_PREFIX;
pub const LEGACY_SECRET_PREFIX: &str = RedactedSecretHash::LEGACY_MARKER_PREFIX;
pub const EXPLICIT_LEGACY_SECRET_PREFIX: &str = RedactedSecretHash::EXPLICIT_LEGACY_MARKER_PREFIX;

/// Convert `Value` to `serde_json::Value`.
///
/// Returns an error if `value` contains a non-finite float (NaN or infinity)
/// because JSON cannot represent these values.
///
pub fn value_to_json(value: &Value) -> Result<serde_json::Value, SerializationError> {
    value_to_json_reusing_secret_hashes(value, None)
}

/// Convert a value for persisted state, reusing matching secret digests from
/// `previous` at the same nested path.
pub fn value_to_json_reusing_secret_hashes(
    value: &Value,
    previous: Option<&serde_json::Value>,
) -> Result<serde_json::Value, SerializationError> {
    value_to_json_inner(value, previous, true)
}

fn value_to_json_inner(
    value: &Value,
    previous: Option<&serde_json::Value>,
    persist_secrets: bool,
) -> Result<serde_json::Value, SerializationError> {
    let ctx = SerializationContext::ValueToJson;
    match value {
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
        Value::Concrete(ConcreteValue::CanonicalEnum(c)) => Ok(canonical_enum_to_json(c)),
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
            let arr: Result<Vec<_>, _> = items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    value_to_json_inner(
                        item,
                        previous
                            .and_then(serde_json::Value::as_array)
                            .and_then(|items| items.get(index)),
                        persist_secrets,
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
            let obj: Result<serde_json::Map<_, _>, _> = map
                .iter()
                .map(|(key, value)| {
                    value_to_json_inner(
                        value,
                        previous
                            .and_then(serde_json::Value::as_object)
                            .and_then(|values| values.get(key)),
                        persist_secrets,
                    )
                    .map(|json| (key.clone(), json))
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
                path: binding.clone(),
                context: ctx,
            })
        }
        Value::Deferred(DeferredValue::Interpolation(_)) => {
            Err(SerializationError::UnresolvedInterpolation { context: ctx })
        }
        Value::Deferred(DeferredValue::FunctionCall { name, .. }) => {
            Err(SerializationError::UnresolvedFunctionCall {
                name: name.clone(),
                context: ctx,
            })
        }
        Value::Deferred(DeferredValue::Secret(inner)) => {
            if persist_secrets {
                let material = canonical_secret_hash_material(inner)?;
                let previous = previous
                    .and_then(serde_json::Value::as_str)
                    .and_then(persisted_secret_hash);
                Ok(serde_json::Value::String(
                    RedactedSecretHash::reuse_or_generate(&material, previous.as_ref())
                        .persisted_marker(),
                ))
            } else {
                value_to_json_inner(inner, None, false)
            }
        }
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => {
            Err(SerializationError::RedactedSecretNotAllowed { context: ctx })
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            Err(SerializationError::UnknownNotAllowed {
                reason: reason.clone(),
                context: ctx,
            })
        }
    }
}

/// Encode one materialized secret, reusing a matching v2 digest when present.
///
/// This is the single digest-selection point for state writeback. Callers walk
/// containers and pass the previous value at the same path.
pub(crate) fn persist_secret_json(
    plaintext: &serde_json::Value,
    previous: Option<&serde_json::Value>,
) -> Result<serde_json::Value, SerializationError> {
    let plaintext = json_to_dsl_value(plaintext).ok_or(SerializationError::SecretNotAllowed {
        context: SerializationContext::StateWriteback,
    })?;
    let material = canonical_secret_hash_material(&plaintext)?;
    let previous = previous
        .and_then(serde_json::Value::as_str)
        .and_then(persisted_secret_hash);
    Ok(serde_json::Value::String(
        RedactedSecretHash::reuse_or_generate(&material, previous.as_ref()).persisted_marker(),
    ))
}

/// Produce the only byte representation that may be hashed as secret material.
///
/// Secret wrappers are sensitivity metadata, not plaintext: nested wrappers
/// are recursively unwrapped. Deferred values retain their stable template
/// representation for saved-plan verification.
pub(crate) fn canonical_secret_hash_material(value: &Value) -> Result<Vec<u8>, SerializationError> {
    let canonical = match value_to_json_inner(value, None, false) {
        Ok(json) => json,
        Err(
            SerializationError::UnknownNotAllowed { .. }
            | SerializationError::UnresolvedResourceRef { .. }
            | SerializationError::UnresolvedInterpolation { .. }
            | SerializationError::UnresolvedFunctionCall { .. }
            | SerializationError::SecretNotAllowed { .. },
        ) => canonical_secret_template(value)?,
        Err(error) => return Err(error),
    };
    serde_json::to_vec(&canonical).map_err(|_| SerializationError::SecretNotAllowed {
        context: SerializationContext::SecretRedaction,
    })
}

pub fn canonical_enum_to_json(c: &CanonicalEnumValue) -> serde_json::Value {
    let identity = c.identity();
    serde_json::json!({
        "Enum": {
            "identity": {
                "provider": identity.provider.clone(),
                "segments": identity.segments.clone(),
                "kind": identity.kind.clone(),
            },
            "api_value": c.api_value(),
        }
    })
}

/// Convert `serde_json::Value` to DSL `Value`.
///
/// Returns `None` for JSON null, since null represents a missing/unset value
/// rather than a meaningful attribute value. Callers should filter out `None`
/// entries when building attribute maps.
pub fn json_to_dsl_value(json: &serde_json::Value) -> Option<Value> {
    json_to_dsl_value_inner(json, false)
}

/// Convert persisted state JSON to a DSL value, lifting self-contained PHC and
/// explicit-salt legacy markers into their opaque typed representation.
/// Raw pre-v12 legacy markers are accepted only by the state-file loader,
/// which upgrades them before this conversion runs.
pub fn persisted_json_to_dsl_value(json: &serde_json::Value) -> Option<Value> {
    json_to_dsl_value_inner(json, true)
}

fn json_to_dsl_value_inner(json: &serde_json::Value, lift_secret_hashes: bool) -> Option<Value> {
    match json {
        serde_json::Value::String(s) => {
            let hash = lift_secret_hashes.then(|| persisted_secret_hash(s));
            match hash.flatten() {
                Some(hash) => Some(Value::Deferred(DeferredValue::RedactedSecret { hash })),
                None => Some(Value::Concrete(ConcreteValue::String(s.clone().into()))),
            }
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
            items
                .iter()
                .filter_map(|item| json_to_dsl_value_inner(item, lift_secret_hashes))
                .collect(),
        ))),
        serde_json::Value::Object(map) => {
            if let Some(canonical) = json_to_canonical_enum(map) {
                return Some(Value::Concrete(ConcreteValue::CanonicalEnum(canonical)));
            }
            let m: IndexMap<_, _> = map
                .iter()
                .filter_map(|(k, v)| {
                    json_to_dsl_value_inner(v, lift_secret_hashes).map(|val| (k.clone(), val))
                })
                .collect();
            Some(Value::Concrete(ConcreteValue::Map(m)))
        }
        serde_json::Value::Null => None,
    }
}

fn json_to_canonical_enum(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Option<CanonicalEnumValue> {
    let serde_json::Value::Object(payload) = map.get("Enum")? else {
        return None;
    };
    if map.len() != 1 {
        return None;
    }

    let serde_json::Value::Object(identity) = payload.get("identity")? else {
        return None;
    };
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
pub fn format_value(value: &Value) -> String {
    format_value_with_key(value, None)
}

/// Format a `Value` for user-visible messages where DSL literal quotes
/// around bare strings get in the way.
pub fn format_value_user_facing(value: &Value) -> String {
    let formatted = format_value(value);
    if matches!(value, Value::Concrete(ConcreteValue::String(_))) {
        formatted
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(&formatted)
            .to_string()
    } else {
        formatted
    }
}

/// Format a `Value` for display, with an optional key for context
pub fn format_value_with_key(value: &Value, _key: Option<&str>) -> String {
    let mut sink = StringSink::default();
    // `StringSink::write_str` never returns Err (it has no overflow
    // budget), so the `Result` here is structurally `Ok(())`. Treat the
    // unwrap as proof of that — never an actual fallible path.
    format_value_into(value, &mut sink).expect("StringSink writes are infallible");
    sink.buf
}

/// Marker returned by [`FormatSink::write_str`] when a budget-bounded
/// sink (e.g. [`WidthCounter`]) has exceeded its limit. [`StringSink`]
/// never returns this.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Overflow;

/// Output sink for [`format_value_into`]. The trait lets the same
/// formatting code path drive either a [`StringSink`] (for the public
/// `String`-returning APIs like `format_value_with_key`) or a
/// [`WidthCounter`] (for the byte-length-only fast path used by
/// [`format_value_pretty`]'s inline-vs-vertical decision). #2434.
///
/// Historically the byte-length path was its own arm-by-arm
/// `inline_width` mirror of `format_value_with_key`, kept consistent by
/// a parity test. That shape silently rotted when a `Value` variant was
/// added on one side but not the other; the trait fuses both into a
/// single source of truth so a new variant only needs one update.
pub(crate) trait FormatSink {
    /// Append `s` to the sink. `Err(Overflow)` short-circuits the
    /// whole render — used by the budget-bounded sink to bail out as
    /// soon as the running width exceeds the limit, without visiting
    /// the rest of the value tree.
    fn write_str(&mut self, s: &str) -> Result<(), Overflow>;
}

#[derive(Default)]
pub(crate) struct StringSink {
    pub(crate) buf: String,
}

impl FormatSink for StringSink {
    fn write_str(&mut self, s: &str) -> Result<(), Overflow> {
        self.buf.push_str(s);
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
    fn write_str(&mut self, s: &str) -> Result<(), Overflow> {
        let next = self.running.checked_add(s.len()).ok_or(Overflow)?;
        if next > self.budget {
            return Err(Overflow);
        }
        self.running = next;
        Ok(())
    }
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
/// the public `format_value_with_key` output. The single source of
/// truth for plan-display value formatting; sinks downstream of this
/// function decide whether to materialise a `String` or just count
/// bytes (#2434).
pub(crate) fn format_value_into<S: FormatSink>(
    value: &Value,
    sink: &mut S,
) -> Result<(), Overflow> {
    match value {
        Value::Concrete(ConcreteValue::String(s)) => {
            sink.write_str("\"")?;
            sink.write_str(s.expose_plaintext())?;
            sink.write_str("\"")
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => {
            // Preserve unresolved parser-surface enum syntax verbatim. The
            // typed variant, rather than its text shape, is what authorizes
            // unquoted rendering.
            sink.write_str(s.as_str())
        }
        Value::Concrete(ConcreteValue::CanonicalEnum(c)) => sink.write_str(c.api_value()),
        Value::Concrete(ConcreteValue::Int(n)) => sink.write_str(&n.to_string()),
        Value::Concrete(ConcreteValue::Duration(d)) => sink.write_str(&render_duration(*d)),
        Value::Concrete(ConcreteValue::Float(f)) => {
            let s = f.to_string();
            sink.write_str(&s)?;
            if !s.contains('.') {
                sink.write_str(".0")?;
            }
            Ok(())
        }
        Value::Concrete(ConcreteValue::Bool(b)) => {
            sink.write_str(if *b { "true" } else { "false" })
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            sink.write_str("[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    sink.write_str(", ")?;
                }
                format_value_into(item, sink)?;
            }
            sink.write_str("]")
        }
        Value::Concrete(ConcreteValue::StringList(items)) => {
            // Canonicalised string-or-list-of-strings shape (#2510).
            // Renders with the same `[ "a", "b" ]` form as `Value::Concrete(ConcreteValue::List)`
            // of `Value::Concrete(ConcreteValue::String)` so plan output stays uniform.
            sink.write_str("[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    sink.write_str(", ")?;
                }
                sink.write_str("\"")?;
                sink.write_str(item.expose_plaintext())?;
                sink.write_str("\"")?;
            }
            sink.write_str("]")
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            sink.write_str("{")?;
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    sink.write_str(", ")?;
                }
                sink.write_str(k)?;
                sink.write_str(": ")?;
                format_value_into(&map[*k], sink)?;
            }
            sink.write_str("}")
        }
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            sink.write_str(&path.to_dot_string())
        }
        Value::Deferred(DeferredValue::BindingRef { binding }) => sink.write_str(binding),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            sink.write_str("\"")?;
            for part in parts {
                match part {
                    InterpolationPart::Literal(s) => sink.write_str(s.expose_plaintext())?,
                    InterpolationPart::Expr(v) => {
                        sink.write_str("${")?;
                        format_value_into(v, sink)?;
                        sink.write_str("}")?;
                    }
                }
            }
            sink.write_str("\"")
        }
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            sink.write_str(name)?;
            sink.write_str("(")?;
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    sink.write_str(", ")?;
                }
                format_value_into(arg, sink)?;
            }
            sink.write_str(")")
        }
        Value::Deferred(DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. }) => {
            sink.write_str("(secret)")
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => sink.write_str(&render_unknown(reason)),
    }
}

/// Check if a Value contains any Secret values at any nesting depth.
pub fn contains_secret(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. }) => true,
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(contains_secret),
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(contains_secret),
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts
            .iter()
            .any(|part| matches!(part, InterpolationPart::Expr(value) if contains_secret(value))),
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
            args.iter().any(contains_secret)
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
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
            args.iter().any(contains_unredacted_secret)
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
    provider_json: &serde_json::Value,
    previous: Option<&serde_json::Value>,
) -> Result<serde_json::Value, SerializationError> {
    match desired {
        Value::Deferred(DeferredValue::Secret(inner)) => {
            match value_to_json_inner(inner, None, false) {
                Ok(plaintext) => persist_secret_json(&plaintext, previous),
                Err(
                    SerializationError::UnresolvedResourceRef { .. }
                    | SerializationError::UnresolvedInterpolation { .. }
                    | SerializationError::UnresolvedFunctionCall { .. }
                    | SerializationError::UnknownNotAllowed { .. },
                ) => {
                    if matches!(provider_json, serde_json::Value::String(value) if is_persisted_secret(value))
                    {
                        return Ok(provider_json.clone());
                    }
                    persist_secret_json(provider_json, previous)
                }
                Err(error) => Err(error),
            }
        }
        Value::Concrete(ConcreteValue::Map(desired_map)) => {
            if let serde_json::Value::Object(provider_obj) = provider_json {
                let mut merged = provider_obj.clone();
                for (k, desired_val) in desired_map {
                    if contains_secret(desired_val) {
                        if let Some(provider_val) = provider_obj.get(k) {
                            merged.insert(
                                k.clone(),
                                merge_secrets_into_provider_json(
                                    desired_val,
                                    provider_val,
                                    previous
                                        .and_then(serde_json::Value::as_object)
                                        .and_then(|values| values.get(k)),
                                )?,
                            );
                        } else {
                            // Key only in desired (not returned by provider); use desired hash
                            merged.insert(
                                k.clone(),
                                value_to_json_reusing_secret_hashes(
                                    desired_val,
                                    previous
                                        .and_then(serde_json::Value::as_object)
                                        .and_then(|values| values.get(k)),
                                )?,
                            );
                        }
                    }
                }
                Ok(serde_json::Value::Object(merged))
            } else {
                // Provider didn't return a map; fall back to desired
                value_to_json_reusing_secret_hashes(desired, previous)
            }
        }
        Value::Concrete(ConcreteValue::List(desired_items)) => {
            if let serde_json::Value::Array(provider_arr) = provider_json {
                let mut merged = Vec::with_capacity(provider_arr.len());
                for (i, provider_elem) in provider_arr.iter().enumerate() {
                    if let Some(desired_elem) = desired_items.get(i) {
                        if contains_secret(desired_elem) {
                            merged.push(merge_secrets_into_provider_json(
                                desired_elem,
                                provider_elem,
                                previous
                                    .and_then(serde_json::Value::as_array)
                                    .and_then(|items| items.get(i)),
                            )?);
                        } else {
                            merged.push(provider_elem.clone());
                        }
                    } else {
                        merged.push(provider_elem.clone());
                    }
                }
                Ok(serde_json::Value::Array(merged))
            } else {
                value_to_json_reusing_secret_hashes(desired, previous)
            }
        }
        _ => Ok(provider_json.clone()),
    }
}

/// Secret-only redactor used inside deferred wrappers (`Interpolation`,
/// `FunctionCall`) where a `Value::Deferred(Unknown(UpstreamExport))` is a
/// load-bearing plan-time placeholder, not an error.
///
/// Hashes every `Secret(_)` leaf while passing `Unknown` and other deferred
/// shapes through verbatim.
fn redact_secrets_only(value: &Value) -> Result<Value, SerializationError> {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => redact_secret_inner(inner),
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => Ok(value.clone()),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let redacted: Result<IndexMap<String, Value>, _> = map
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
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            let redacted: Result<Vec<Value>, _> = args.iter().map(redact_secrets_only).collect();
            Ok(Value::Deferred(DeferredValue::FunctionCall {
                name: name.clone(),
                args: redacted?,
            }))
        }
        // `Unknown` and every other deferred / concrete shape pass through.
        other => Ok(other.clone()),
    }
}

/// Redact a secret's inner value without requiring it to be fully resolved.
///
/// Every secret wrapper becomes exactly one placeholder. For a deferred inner
/// expression, the placeholder hashes a canonical template representation;
/// references and unknowns are represented structurally, so no individual
/// literal fragment is exposed to an independent brute-force attempt.
fn redact_secret_inner(inner: &Value) -> Result<Value, SerializationError> {
    match inner {
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => Ok(inner.clone()),
        _ => Ok(Value::Deferred(DeferredValue::RedactedSecret {
            hash: secret_hash(inner, None)?,
        })),
    }
}

fn secret_hash(
    inner: &Value,
    previous: Option<&RedactedSecretHash>,
) -> Result<RedactedSecretHash, SerializationError> {
    let material = canonical_secret_hash_material(inner)?;
    Ok(RedactedSecretHash::reuse_or_generate(&material, previous))
}

/// Canonical hash material for a secret that is still deferred. Tags keep
/// expression nodes disjoint from user-authored maps while deliberately
/// retaining only stable resolver inputs (paths, bindings, literals, and
/// unknown reasons). This representation is hashed but never persisted.
fn canonical_secret_template(value: &Value) -> Result<serde_json::Value, SerializationError> {
    use serde_json::{Map, Value as JsonValue};

    fn tagged(tag: &str, value: JsonValue) -> JsonValue {
        let mut object = Map::new();
        object.insert("kind".to_string(), JsonValue::String(tag.to_string()));
        object.insert("value".to_string(), value);
        JsonValue::Object(object)
    }

    Ok(match value {
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
            tagged("binding_ref", JsonValue::String(binding.clone()))
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
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            let mut call = Map::new();
            call.insert("name".to_string(), JsonValue::String(name.clone()));
            call.insert(
                "args".to_string(),
                JsonValue::Array(
                    args.iter()
                        .map(canonical_secret_template)
                        .collect::<Result<_, _>>()?,
                ),
            );
            tagged("function_call", JsonValue::Object(call))
        }
        Value::Deferred(DeferredValue::Secret(inner)) => canonical_secret_template(inner)?,
        Value::Deferred(DeferredValue::RedactedSecret { .. }) => {
            return Err(SerializationError::RedactedSecretNotAllowed {
                context: SerializationContext::SecretRedaction,
            });
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => tagged(
            "unknown",
            serde_json::to_value(reason).expect("UnknownReason is serializable"),
        ),
    })
}

/// Return whether a value tree contains a saved-plan secret placeholder.
pub fn contains_redacted_secret(value: &Value) -> bool {
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
        Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
            args.iter().any(contains_redacted_secret)
        }
        _ => false,
    }
}

fn attributes_contain_redacted_secret<'a>(values: impl Iterator<Item = &'a Value>) -> bool {
    values.into_iter().any(contains_redacted_secret)
}

fn deferred_template_contains_redacted_secret(
    template: &crate::parser::DeferredForExpression,
) -> bool {
    let crate::parser::DeferredForExpression {
        file: _file,
        line: _line,
        header: _header,
        resource_type: _resource_type,
        attributes,
        binding_name: _binding_name,
        iterable_binding: _iterable_binding,
        iterable_attr: _iterable_attr,
        binding: _binding,
        template_resource,
    } = template;
    attributes_contain_redacted_secret(attributes.iter().map(|(_, value)| value))
        || attributes_contain_redacted_secret(template_resource.attributes.values())
}

pub(crate) fn effect_contains_redacted_secret(effect: &crate::effect::Effect) -> bool {
    use crate::effect::Effect;

    match effect {
        Effect::Read { resource } => {
            attributes_contain_redacted_secret(resource.attributes.values())
        }
        Effect::Create(resource) => {
            attributes_contain_redacted_secret(resource.attributes.values())
        }
        Effect::Update {
            from,
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
        } => false,
        Effect::Wait {
            identity: _identity,
            target_id: _target_id,
            until,
            until_surface: _until_surface,
            timeout: _timeout,
            interval: _interval,
            explicit_dependencies: _explicit_dependencies,
        } => contains_redacted_secret(until.value()),
        Effect::DeferredCreate {
            id: _id,
            upstream_binding: _upstream_binding,
            template,
        } => deferred_template_contains_redacted_secret(template),
        Effect::DeferredReplace(payload) => {
            let crate::effect::DeferredReplacePayload {
                deletes: _deletes,
                id: _id,
                upstream_binding: _upstream_binding,
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
    Persisted(&'a serde_json::Value),
    /// Desired `Secret` wrappers are authoritative; persisted state supplies
    /// only a same-path digest that may be reused.
    PersistedDesired(&'a serde_json::Value),
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
            Self::Persisted(serde_json::Value::String(value)) if is_persisted_secret(value) => {
                Some(plaintext)
            }
            Self::PersistedDesired(serde_json::Value::String(value))
                if is_persisted_secret(value) =>
            {
                match source {
                    Value::Deferred(
                        DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. },
                    ) => Some(plaintext),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn previous_hash(self) -> Option<RedactedSecretHash> {
        match self {
            Self::RedactedShape(Value::Deferred(DeferredValue::RedactedSecret { hash }))
            | Self::RedactedDesired(Value::Deferred(DeferredValue::RedactedSecret { hash })) => {
                Some(hash.clone())
            }
            Self::Persisted(serde_json::Value::String(marker))
            | Self::PersistedDesired(serde_json::Value::String(marker)) => {
                persisted_secret_hash(marker)
            }
            _ => None,
        }
    }

    fn map_child(self, key: &str) -> Option<Self> {
        match self {
            Self::Evaluated(Value::Concrete(ConcreteValue::Map(values))) => {
                values.get(key).map(Self::Evaluated)
            }
            Self::SecretShape(Value::Concrete(ConcreteValue::Map(values))) => {
                values.get(key).map(Self::SecretShape)
            }
            Self::RedactedShape(Value::Concrete(ConcreteValue::Map(values))) => {
                values.get(key).map(Self::RedactedShape)
            }
            Self::RedactedDesired(Value::Concrete(ConcreteValue::Map(values))) => {
                values.get(key).map(Self::RedactedDesired)
            }
            Self::Persisted(serde_json::Value::Object(values)) => {
                values.get(key).map(Self::Persisted)
            }
            Self::PersistedDesired(serde_json::Value::Object(values)) => {
                values.get(key).map(Self::PersistedDesired)
            }
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
            Self::Persisted(serde_json::Value::Array(values)) => {
                values.get(index).map(Self::Persisted)
            }
            Self::PersistedDesired(serde_json::Value::Array(values)) => {
                values.get(index).map(Self::PersistedDesired)
            }
            _ => None,
        }
    }
}

/// Parse the typed digest carried by a complete persisted secret marker.
pub fn persisted_secret_hash(value: &str) -> Option<RedactedSecretHash> {
    RedactedSecretHash::parse_persisted_marker(value)
}

/// Whether `value` is a complete persisted secret marker.
pub fn is_persisted_secret(value: &str) -> bool {
    RedactedSecretHash::is_persisted_marker(value)
}

/// Reconstruct the byte-identical state/provider wire marker for an opaque
/// persisted hash.
pub fn persisted_secret_marker(hash: &RedactedSecretHash) -> String {
    hash.persisted_marker()
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
        return Ok(Value::Deferred(DeferredValue::RedactedSecret {
            hash: secret_hash(secret_value, previous.as_ref())?,
        }));
    }

    match source {
        Value::Concrete(ConcreteValue::Map(values)) => {
            let values = values
                .iter()
                .map(|(key, value)| {
                    let value = match guide.map_child(key) {
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
/// so this function never infers marker semantics from its source value.
fn redact_plaintext_using_persisted_state(
    plaintext: &Value,
    persisted: &serde_json::Value,
) -> Result<Value, SerializationError> {
    redact_with_guide(plaintext, SecretRedactionGuide::Persisted(persisted))
}

/// Apply secret sensitivity to provider plaintext for display only.
///
/// Unlike saved-plan redaction this does not hash or verify anything. It
/// preserves the plaintext for execution while ensuring formatters render
/// every path marked secret by the desired value as `(secret)`.
pub fn mask_plaintext_using_secret_shape(plaintext: &Value, shape: &Value) -> Value {
    if matches!(
        shape,
        Value::Deferred(DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. })
    ) {
        return Value::Deferred(DeferredValue::Secret(Box::new(plaintext.clone())));
    }
    match (plaintext, shape) {
        (
            Value::Concrete(ConcreteValue::Map(values)),
            Value::Concrete(ConcreteValue::Map(shapes)),
        ) => Value::Concrete(ConcreteValue::Map(
            values
                .iter()
                .map(|(key, value)| {
                    let masked = shapes.get(key).map_or_else(
                        || value.clone(),
                        |shape| mask_plaintext_using_secret_shape(value, shape),
                    );
                    (key.clone(), masked)
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
                        |shape| mask_plaintext_using_secret_shape(value, shape),
                    )
                })
                .collect(),
        )),
        _ => plaintext.clone(),
    }
}

/// Apply persisted secret sensitivity to provider plaintext.
///
/// This masks values for display and for pre-apply validation without copying
/// a stored hash into the runtime value. The raw JSON walker stays private;
/// callers must supply the opaque persisted-state view and an attribute key.
fn mask_plaintext_using_persisted_json_shape(
    plaintext: &Value,
    persisted: &serde_json::Value,
) -> Value {
    if persisted.as_str().is_some_and(is_persisted_secret) {
        return Value::Deferred(DeferredValue::Secret(Box::new(plaintext.clone())));
    }
    match (plaintext, persisted) {
        (Value::Concrete(ConcreteValue::Map(values)), serde_json::Value::Object(shapes)) => {
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
        }
        (Value::Concrete(ConcreteValue::List(values)), serde_json::Value::Array(shapes)) => {
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
        }
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
        |shape| mask_plaintext_using_persisted_json_shape(plaintext, shape),
    )
}

fn rehydrate_redacted_secrets_impl(
    saved: &Value,
    candidate: &Value,
    candidate_is_secret: bool,
) -> Result<Value, SecretRehydrationError> {
    if !contains_redacted_secret(saved) {
        return Ok(saved.clone());
    }
    if candidate_is_secret && secret_candidate_evaluation_is_unavailable(candidate) {
        return Err(SecretRehydrationError::EvaluationUnavailable);
    }

    // A secret interpolation/function may have been saved as a redacted
    // structural template because its final value depended on an apply-time
    // reference. Verify every template leaf against the fresh secret's inner
    // expression, then return the fresh typed Secret so taint is preserved.
    if candidate_is_secret
        && !matches!(saved, Value::Deferred(DeferredValue::RedactedSecret { .. }))
        && let Value::Deferred(DeferredValue::Secret(inner)) = candidate
    {
        rehydrate_redacted_secrets_impl(saved, inner, false)?;
        return Ok(candidate.clone());
    }

    match saved {
        Value::Deferred(DeferredValue::RedactedSecret { hash }) => {
            let (inner, replacement) = if candidate_is_secret {
                let Value::Deferred(DeferredValue::Secret(inner)) = candidate else {
                    return Err(SecretRehydrationError::ExpectedSecret);
                };
                (inner.as_ref(), candidate.clone())
            } else {
                (candidate, candidate.clone())
            };
            if !hash.verify(canonical_secret_hash_material(inner)?) {
                return Err(SecretRehydrationError::HashMismatch);
            }
            Ok(replacement)
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
                        rehydrate_redacted_secrets_impl(value, candidate, candidate_is_secret)?
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
                    rehydrate_redacted_secrets_impl(saved, candidate, candidate_is_secret)
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
                            rehydrate_redacted_secrets_impl(saved, candidate, candidate_is_secret)
                                .map(InterpolationPart::Expr)
                        }
                        Some(InterpolationPart::Literal(candidate)) => {
                            let candidate =
                                Value::Concrete(ConcreteValue::String(candidate.clone()));
                            let value = rehydrate_redacted_secrets_impl(saved, &candidate, false)?;
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
        Value::Deferred(DeferredValue::FunctionCall { name, args: saved }) => {
            let Value::Deferred(DeferredValue::FunctionCall {
                args: candidate, ..
            }) = candidate
            else {
                return Err(SecretRehydrationError::ShapeMismatch);
            };
            let args = saved
                .iter()
                .enumerate()
                .map(|(index, saved)| {
                    if !contains_redacted_secret(saved) {
                        return Ok(saved.clone());
                    }
                    let candidate = candidate
                        .get(index)
                        .ok_or(SecretRehydrationError::ShapeMismatch)?;
                    rehydrate_redacted_secrets_impl(saved, candidate, candidate_is_secret)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::Deferred(DeferredValue::FunctionCall {
                name: name.clone(),
                args,
            }))
        }
        _ => Err(SecretRehydrationError::ShapeMismatch),
    }
}

fn secret_candidate_evaluation_is_unavailable(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::Unknown(_) | DeferredValue::FunctionCall { .. }) => true,
        Value::Deferred(DeferredValue::Interpolation(parts)) => parts.iter().any(|part| {
            matches!(
                part,
                InterpolationPart::Expr(value)
                    if secret_candidate_evaluation_is_unavailable(value)
            )
        }),
        Value::Deferred(DeferredValue::Secret(inner)) => {
            secret_candidate_evaluation_is_unavailable(inner)
        }
        Value::Concrete(ConcreteValue::Map(values)) => values
            .values()
            .any(secret_candidate_evaluation_is_unavailable),
        Value::Concrete(ConcreteValue::List(values)) => values
            .iter()
            .any(secret_candidate_evaluation_is_unavailable),
        _ => false,
    }
}

/// Restore saved-plan placeholders from freshly evaluated configuration
/// values, verifying the plan-time hash at every placeholder.
pub fn rehydrate_redacted_secrets(
    saved: &Value,
    evaluated: &Value,
) -> Result<Value, SecretRehydrationError> {
    rehydrate_redacted_secrets_impl(saved, evaluated, true)
}

/// Restore historical saved-plan placeholders from a fresh provider read,
/// verifying the plan-time hash before returning provider plaintext.
pub fn rehydrate_redacted_secrets_from_plaintext(
    saved: &Value,
    plaintext: &Value,
) -> Result<Value, SecretRehydrationError> {
    rehydrate_redacted_secrets_impl(saved, plaintext, false)
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
pub fn rehydrate_secret_entry(
    saved: &Value,
    candidate: Option<&Value>,
    location: impl Into<String>,
    candidate_kind: SecretRehydrationCandidate,
) -> Result<Option<Value>, SecretRehydrationSiteError> {
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
        Value::Deferred(DeferredValue::Secret(inner)) => redact_secret_inner(inner),
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
        Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
            let redacted: Result<Vec<Value>, _> = args.iter().map(redact_secrets_only).collect();
            Ok(Value::Deferred(DeferredValue::FunctionCall {
                name: name.clone(),
                args: redacted?,
            }))
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            Err(SerializationError::UnknownNotAllowed {
                reason: reason.clone(),
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
pub fn redact_secrets_in_resource(
    resource: &crate::resource::Resource,
) -> Result<crate::resource::Resource, SerializationError> {
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
pub fn redact_secrets_in_resource_reusing_persisted_hashes(
    resource: &crate::resource::Resource,
    persisted: &crate::provider::PersistedAttributes,
) -> Result<crate::resource::Resource, SerializationError> {
    let attributes = redact_attrs_with_guide(resource.attributes.iter(), |key, value| {
        match persisted.encoded_value(key) {
            Some(previous) => {
                redact_with_guide(value, SecretRedactionGuide::PersistedDesired(previous))
            }
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
fn redact_resource_using_redacted_shape(
    resource: &crate::resource::Resource,
    guide: &crate::resource::Resource,
) -> Result<crate::resource::Resource, SerializationError> {
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
pub fn redact_secrets_in_resource_using_evaluated(
    source: &crate::resource::Resource,
    evaluated: &crate::resource::Resource,
) -> Result<crate::resource::Resource, SerializationError> {
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
pub fn rehydrate_resource_secrets(
    saved: &crate::resource::Resource,
    evaluated: &crate::resource::Resource,
) -> Result<crate::resource::Resource, SecretRehydrationSiteError> {
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
pub fn redact_secrets_in_data_source(
    resource: &crate::resource::DataSource,
) -> Result<crate::resource::DataSource, SerializationError> {
    let attributes = redact_attrs_with_guide(resource.attributes.iter(), |_, value| {
        redact_secrets_in_value(value)
    })?;
    Ok(crate::resource::DataSource {
        attributes,
        ..resource.clone()
    })
}

pub fn redact_secrets_in_data_source_using_evaluated(
    source: &crate::resource::DataSource,
    evaluated: &crate::resource::DataSource,
) -> Result<crate::resource::DataSource, SerializationError> {
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

pub fn rehydrate_data_source_secrets(
    saved: &crate::resource::DataSource,
    evaluated: &crate::resource::DataSource,
) -> Result<crate::resource::DataSource, SecretRehydrationSiteError> {
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
/// [`Composition`](crate::resource::Composition) (carina#3248).
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
pub fn redact_secrets_in_virtual(
    resource: &crate::resource::Composition,
) -> Result<crate::resource::Composition, SerializationError> {
    let arguments: Result<indexmap::IndexMap<String, crate::resource::CompositionArgument>, _> =
        resource
            .signature
            .arguments
            .iter()
            .map(|(k, argument)| {
                redact_secrets_in_value(argument.value())
                    .map(|rv| (k.clone(), argument.with_value(rv)))
            })
            .collect();
    let attributes: Result<indexmap::IndexMap<String, crate::resource::CompositionAttribute>, _> =
        resource
            .signature
            .attributes
            .iter()
            .map(|(k, attr)| {
                redact_secrets_in_value(&attr.to_value())
                    .map(|value| (k.clone(), attr.with_value(value)))
            })
            .collect();
    let mut out = resource.clone();
    out.signature.arguments = arguments?;
    out.signature.attributes = attributes?;
    Ok(out)
}

pub fn rehydrate_composition_secrets(
    saved: &crate::resource::Composition,
    evaluated: &crate::resource::Composition,
) -> Result<crate::resource::Composition, SecretRehydrationSiteError> {
    let mut out = saved.clone();
    for (key, saved_argument) in &saved.signature.arguments {
        let value = rehydrate_secret_entry(
            saved_argument.value(),
            evaluated
                .signature
                .arguments
                .get(key)
                .map(crate::resource::CompositionArgument::value),
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
            .map(crate::resource::CompositionAttribute::to_value);
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
pub fn redact_secrets_in_state_using_resource(
    state: &crate::resource::State,
    desired: &crate::resource::Resource,
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
                Some(marker) => redact_plaintext_using_persisted_state(value, marker),
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
    template: &crate::parser::DeferredForExpression,
) -> Result<crate::parser::DeferredForExpression, SerializationError> {
    let mut redacted = template.clone();
    redacted.template_resource.attributes = template
        .template_resource
        .attributes
        .iter()
        .map(|(key, value)| redact_secrets_only(value).map(|value| (key.clone(), value)))
        .collect::<Result<_, _>>()?;
    redacted.attributes = template
        .attributes
        .iter()
        .map(|(key, value)| {
            if let Some(canonical) = redacted.template_resource.attributes.get(key) {
                Ok((key.clone(), canonical.clone()))
            } else {
                redact_secrets_only(value).map(|value| (key.clone(), value))
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(redacted)
}

/// Redact all secrets in an `Effect`, returning a new Effect with secrets replaced by hashes.
pub fn redact_secrets_in_effect(
    effect: &crate::effect::Effect,
) -> Result<crate::effect::Effect, SerializationError> {
    use crate::effect::{DeferredReplacePayload, Effect};
    Ok(match effect {
        Effect::Read { resource } => Effect::Read {
            resource: crate::resource::ResolvedDataSource::new(redact_secrets_in_data_source(
                resource,
            )?),
        },
        Effect::Create(resource) => Effect::Create(crate::resource::ResolvedResource::new(
            redact_secrets_in_resource(resource)?,
        )),
        Effect::Update {
            from,
            to,
            changed_attributes,
        } => Effect::Update {
            from: Box::new(redact_secrets_in_state(from)?),
            to: crate::resource::ResolvedResource::new(redact_secrets_in_resource(to)?),
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
        Effect::Move { from, to } => Effect::Move {
            from: from.clone(),
            to: to.clone(),
        },
        // Wait effects carry no secret-bearing fields — `until` is a
        // typed predicate over scalar values, surface form is the
        // user-authored source. Clone through unchanged.
        Effect::Wait { .. } => effect.clone(),
        Effect::DeferredCreate {
            id,
            upstream_binding,
            template,
        } => Effect::DeferredCreate {
            id: id.clone(),
            upstream_binding: upstream_binding.clone(),
            template: Box::new(redact_deferred_template(template)?),
        },
        Effect::DeferredReplace(payload) => {
            Effect::DeferredReplace(Box::new(DeferredReplacePayload {
                deletes: payload.deletes.clone(),
                id: payload.id.clone(),
                upstream_binding: payload.upstream_binding.clone(),
                template: Box::new(redact_deferred_template(&payload.template)?),
            }))
        }
    })
}

/// Redact all secrets in a `Plan`, returning a new Plan with secrets replaced by hashes.
pub fn redact_secrets_in_plan(
    plan: &crate::plan::Plan,
) -> Result<crate::plan::Plan, SerializationError> {
    redact_secrets_in_plan_with_desired_and_historical(plan, &HashMap::new(), &HashMap::new())
}

fn create_effect_ids(plan: &crate::plan::Plan) -> Vec<Option<crate::resource::ResourceId>> {
    plan.effects()
        .iter()
        .map(|effect| match effect {
            crate::effect::Effect::Create(resource) => Some(resource.id.clone()),
            _ => None,
        })
        .collect()
}

pub fn redact_secrets_in_plan_with_desired_and_historical(
    plan: &crate::plan::Plan,
    planned_resources: &HashMap<crate::resource::ResourceId, crate::resource::Resource>,
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
                crate::effect::Effect::Create(resource) => match reusable_resource(&resource.id) {
                    Some(planned) => {
                        crate::effect::Effect::Create(crate::resource::ResolvedResource::new(
                            redact_resource_using_redacted_shape(resource, planned)?,
                        ))
                    }
                    None => redact_secrets_in_effect(effect)?,
                },
                crate::effect::Effect::Update {
                    from,
                    to,
                    changed_attributes,
                } => crate::effect::Effect::Update {
                    from: Box::new(redact_secrets_in_state(from)?),
                    to: match reusable_resource(&to.id) {
                        Some(planned) => crate::resource::ResolvedResource::new(
                            redact_resource_using_redacted_shape(to, planned)?,
                        ),
                        None => {
                            crate::resource::ResolvedResource::new(redact_secrets_in_resource(to)?)
                        }
                    },
                    changed_attributes: changed_attributes.clone(),
                },
                _ => redact_secrets_in_effect(effect)?,
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
pub fn rehydrate_plan_desired_secrets(
    plan: &crate::plan::Plan,
    resources: &HashMap<crate::resource::ResourceId, crate::resource::Resource>,
    data_sources: &HashMap<crate::resource::ResourceId, crate::resource::DataSource>,
    import_identifiers: &HashMap<crate::resource::ResourceId, Value>,
    deferred_templates: &[crate::parser::DeferredForExpression],
) -> Result<crate::plan::Plan, SecretRehydrationSiteError> {
    use crate::effect::{DeferredReplacePayload, Effect};

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
                    resource: crate::resource::ResolvedDataSource::new(
                        rehydrate_data_source_secrets(resource, fresh)?,
                    ),
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
                Ok(Effect::Create(crate::resource::ResolvedResource::new(
                    rehydrate_resource_secrets(resource, fresh)?,
                )))
            }
            Effect::Update {
                from,
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
                    to: crate::resource::ResolvedResource::new(rehydrate_resource_secrets(
                        to, fresh,
                    )?),
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
            Effect::DeferredCreate {
                id,
                upstream_binding,
                template,
            } => {
                let template =
                    rehydrate_deferred_template(template, upstream_binding, deferred_templates)?;
                Ok(Effect::DeferredCreate {
                    id: id.clone(),
                    upstream_binding: upstream_binding.clone(),
                    template: Box::new(template),
                })
            }
            Effect::DeferredReplace(payload) => {
                let template = rehydrate_deferred_template(
                    &payload.template,
                    &payload.upstream_binding,
                    deferred_templates,
                )?;
                Ok(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
                    deletes: payload.deletes.clone(),
                    id: payload.id.clone(),
                    upstream_binding: payload.upstream_binding.clone(),
                    template: Box::new(template),
                })))
            }
            _ => Ok(effect.clone()),
        })
        .collect::<Result<Vec<_>, SecretRehydrationSiteError>>()?;
    *out.effects_mut() = effects;
    Ok(out)
}

fn rehydrate_deferred_template(
    saved: &crate::parser::DeferredForExpression,
    upstream_binding: &str,
    candidates: &[crate::parser::DeferredForExpression],
) -> Result<crate::parser::DeferredForExpression, SecretRehydrationSiteError> {
    let has_placeholder = deferred_template_contains_redacted_secret(saved);
    if !has_placeholder {
        return Ok(saved.clone());
    }

    let mut out = saved.clone();

    let candidate = candidates
        .iter()
        .find(|candidate| {
            candidate.binding_name == saved.binding_name
                && candidate.resource_type == saved.resource_type
                && candidate.iterable_binding == saved.iterable_binding
                && candidate.iterable_binding == upstream_binding
                && candidate.iterable_attr == saved.iterable_attr
        })
        .ok_or_else(|| {
            SecretRehydrationSiteError::new(
                format!("deferred for {}", saved.binding_name),
                SecretRehydrationError::ShapeMismatch,
            )
        })?;

    for (key, saved_value) in &mut out.template_resource.attributes {
        if let Some(value) = rehydrate_secret_entry(
            saved_value,
            candidate.template_resource.attributes.get(key),
            format!("deferred resource {}.{key}", saved.binding_name),
            SecretRehydrationCandidate::Evaluated,
        )? {
            *saved_value = value;
        }
    }
    for (key, saved_value) in &mut out.attributes {
        if let Some(canonical) = out.template_resource.attributes.get(key) {
            *saved_value = canonical.clone();
            continue;
        }
        if let Some(value) = rehydrate_secret_entry(
            saved_value,
            candidate
                .attributes
                .iter()
                .find_map(|(candidate_key, value)| (candidate_key == key).then_some(value)),
            format!("deferred display resource {}.{key}", saved.binding_name),
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
pub fn rehydrate_plan_historical_secrets_from_verified_states(
    plan: &crate::plan::Plan,
    redacted_states: &HashMap<crate::resource::ResourceId, crate::resource::State>,
    verified_states: &HashMap<crate::resource::ResourceId, crate::resource::State>,
) -> Result<crate::plan::Plan, SecretRehydrationSiteError> {
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
                to,
                changed_attributes,
            } if from.attributes.values().any(contains_redacted_secret) => {
                let (redacted, verified) = state_evidence(&from.id)?;
                Ok(Effect::Update {
                    from: Box::new(restore_state(from, redacted, verified)?),
                    to: to.clone(),
                    changed_attributes: changed_attributes.clone(),
                })
            }
            _ => Ok(effect.clone()),
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
/// in `format_value_pretty`. Fixed (not terminal-derived) so snapshot tests are
/// deterministic and CI/PR-comment readers see identical output.
pub(crate) const PRETTY_LINE_LIMIT: usize = 80;

/// Layout context for `format_value_pretty`.
///
/// Carries the column at which the parent attribute's *key* is rendered
/// (`parent_indent_cols`) and the parent attribute's *key* itself. The
/// helper uses these to decide whether the value fits inline (`<indent>key:
/// <inline-value>` ≤ 80 cols) and at what column to indent vertical
/// continuation lines (children render at `parent_indent_cols + 2`,
/// YAML-conventional).
///
/// The struct exists so the two `usize` / `&str` fields can't be passed
/// in the wrong order, and to leave room for future fields (e.g. a
/// custom width budget) without breaking the call signature.
#[derive(Debug, Clone, Copy)]
pub struct PrettyLayout<'a> {
    /// Column at which the parent attribute's key is rendered (i.e. how
    /// many leading spaces precede `<key>:` on its line).
    pub parent_indent_cols: usize,
    /// Parent attribute's key, used to compute `<indent>key: ` width when
    /// deciding inline-vs-vertical.
    pub key: &'a str,
}

impl<'a> PrettyLayout<'a> {
    /// Width of the prefix (`<indent>key: `) that the caller has already
    /// emitted, in columns. Used as the budget consumed before any value
    /// content can fit on the same line. Map keys can carry non-ASCII
    /// characters (the `Value::Concrete(ConcreteValue::Map)` key type is `String` with no
    /// encoding constraint), so width is measured in Unicode scalar values.
    fn prefix_cols(&self) -> usize {
        self.parent_indent_cols + self.key.chars().count() + 2
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
/// - Scalar variants are identical to `format_value_with_key(value, None)`.
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
pub fn format_value_pretty(value: &Value, layout: PrettyLayout<'_>) -> String {
    match value {
        Value::Concrete(ConcreteValue::List(items)) => {
            if items.is_empty() {
                return "[]".to_string();
            }
            if is_list_of_maps(value) {
                return format_list_of_maps_vertical(items, layout.child_indent_cols());
            }
            // #2434: measure first, build only when we know it fits.
            // Pre-fix this called `format_value_with_key` unconditionally
            // and discarded the result on overflow — quadratic on deep
            // nested values because `format_*_vertical` recurses back
            // into `format_value_pretty`, which built-and-discarded again
            // at every level.
            let budget = PRETTY_LINE_LIMIT.saturating_sub(layout.prefix_cols());
            if inline_width(value, budget).is_some() {
                return format_value_with_key(value, None);
            }
            format_list_of_scalars_vertical(items, layout.child_indent_cols())
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
            // stays byte-identical to `format_value_with_key`'s arm.
            if items.is_empty() {
                return "[]".to_string();
            }
            let budget = PRETTY_LINE_LIMIT.saturating_sub(layout.prefix_cols());
            if inline_width(value, budget).is_some() {
                return format_value_with_key(value, None);
            }
            let lifted: Vec<Value> = items
                .iter()
                .cloned()
                .map(|s| Value::Concrete(ConcreteValue::String(s)))
                .collect();
            format_list_of_scalars_vertical(&lifted, layout.child_indent_cols())
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            if map.is_empty() {
                return "{}".to_string();
            }
            let budget = PRETTY_LINE_LIMIT.saturating_sub(layout.prefix_cols());
            if inline_width(value, budget).is_some() {
                return format_value_with_key(value, None);
            }
            format_map_vertical(map, layout.child_indent_cols())
        }
        _ => format_value_with_key(value, None),
    }
}

/// Compute the byte length of `format_value_with_key(value, None)` without
/// allocating the rendered string, short-circuiting to `None` as soon as
/// the running total exceeds `budget` (#2434).
///
/// Implemented as a thin wrapper around [`format_value_into`] with a
/// [`WidthCounter`] sink, so the byte count is byte-for-byte identical
/// to what `format_value_with_key` would emit by construction —
/// adding a new `Value` variant only needs one site update (the
/// `format_value_into` arm), not a measure/build pair that could rot
/// out of sync.
///
/// `format_value_pretty` is the only intended caller; `pub(crate)`
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
fn format_list_of_maps_vertical(items: &[Value], entry_indent_cols: usize) -> String {
    let entry_indent = " ".repeat(entry_indent_cols);
    let continuation_indent = " ".repeat(entry_indent_cols + 2);
    let mut out = String::new();
    let mut first_element = true;
    for item in items {
        if let Value::Concrete(ConcreteValue::Map(map)) = item {
            if !first_element {
                out.push('\n');
            }
            first_element = false;
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            for (i, k) in keys.iter().enumerate() {
                let child_layout = PrettyLayout {
                    parent_indent_cols: entry_indent_cols + 2,
                    key: k,
                };
                let val_str = format_value_pretty(&map[*k], child_layout);
                out.push('\n');
                if i == 0 {
                    out.push_str(&entry_indent);
                    out.push_str("* ");
                } else {
                    out.push_str(&continuation_indent);
                }
                out.push_str(k);
                out.push_str(": ");
                out.push_str(&val_str);
            }
        }
    }
    out
}

/// Render a list-of-scalars vertically inside a bracketed block. Items
/// indent at `item_indent_cols`; closing `]` aligns with the parent line
/// (one level shallower).
fn format_list_of_scalars_vertical(items: &[Value], item_indent_cols: usize) -> String {
    // Invariant: `item_indent_cols` is always `child_indent_cols()` of some
    // `PrettyLayout`, which is `parent_indent_cols + 2`, so >= 2.
    debug_assert!(item_indent_cols >= 2);
    let item_indent = " ".repeat(item_indent_cols);
    let close_indent = " ".repeat(item_indent_cols - 2);
    let mut out = String::from("[\n");
    for item in items {
        out.push_str(&item_indent);
        out.push_str(&format_value_with_key(item, None));
        out.push(',');
        out.push('\n');
    }
    out.push_str(&close_indent);
    out.push(']');
    out
}

/// Render a map vertically. Each key is at `key_indent_cols` and its value
/// recurses with that key as the new parent key. A blank line is injected
/// before any key that follows a multi-element list-of-maps so the list
/// boundary stays visible — the `*` per-element marker disambiguates
/// element starts but not element *ends* (#2555). The blank is only
/// inserted when a sibling actually follows, avoiding orphan whitespace
/// at the end of a block.
fn format_map_vertical(map: &IndexMap<String, Value>, key_indent_cols: usize) -> String {
    let mut keys: Vec<_> = map.keys().collect();
    keys.sort();
    let key_indent = " ".repeat(key_indent_cols);
    let mut out = String::new();
    let mut prev_needs_separator = false;
    for k in keys {
        let child_layout = PrettyLayout {
            parent_indent_cols: key_indent_cols,
            key: k,
        };
        let val_str = format_value_pretty(&map[k], child_layout);
        if prev_needs_separator {
            out.push('\n');
        }
        prev_needs_separator = needs_trailing_separator(&map[k]);
        out.push('\n');
        out.push_str(&key_indent);
        out.push_str(k);
        out.push_str(": ");
        out.push_str(&val_str);
    }
    out
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
pub fn needs_trailing_separator(value: &Value) -> bool {
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
    let unwrapped = peel_custom(attr_type);
    let Some(members) = crate::schema::union_members_with_defs(unwrapped, defs) else {
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
        match &peel_custom(member.as_attr()).kind {
            AttrTypeKind::String { .. } => has_string = true,
            AttrTypeKind::List {
                element_type: inner,
                ..
            } if matches!(
                &peel_custom(inner.as_ref()).kind,
                AttrTypeKind::String { .. }
            ) =>
            {
                has_list_of_string = true;
            }
            _ => return false,
        }
    }
    has_string && has_list_of_string
}

fn peel_custom(t: &AttributeType) -> &AttributeType {
    t
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

    fn canonicalization_failed(
        self,
        original: Value,
        source: crate::schema::TypeError,
    ) -> Result<Value, Self::Error>;

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

    fn canonicalization_failed(
        self,
        _original: Value,
        source: crate::schema::TypeError,
    ) -> Result<Value, Self::Error> {
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

    fn canonicalization_failed(
        self,
        original: Value,
        _source: crate::schema::TypeError,
    ) -> Result<Value, Self::Error> {
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
fn canonicalize_desired_with_type(
    value: Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    source: DesiredValueSource,
) -> Result<Value, crate::schema::TypeError> {
    let canonical = canonicalize_with_type_for_purpose(
        value,
        attr_type,
        defs,
        CanonicalizationPurpose::Desired(source),
        RejectEnumResolutionErrors,
    )?;
    if let Some(error) = unresolved_enum_identifier_type_error_in(&canonical) {
        return Err(error);
    }
    Ok(canonical)
}

/// Canonicalize provider or persisted state. Unrecognized enum-positioned
/// strings legitimately remain strings for validation and comparison.
pub(crate) fn canonicalize_state_with_type(
    value: Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Value {
    unwrap_infallible(canonicalize_with_type_for_purpose(
        value,
        attr_type,
        defs,
        CanonicalizationPurpose::State,
        PreserveEnumResolutionErrors,
    ))
}

/// Resolve typed wait-RHS enum syntax without reclassifying quoted strings or
/// applying unrelated structural rewrites.
pub(crate) fn canonicalize_wait_rhs_with_type(
    value: Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Value {
    unwrap_infallible(canonicalize_with_type_for_purpose(
        value,
        attr_type,
        defs,
        CanonicalizationPurpose::WaitRhs,
        PreserveEnumResolutionErrors,
    ))
}

fn canonicalize_with_type_for_purpose<P: CanonicalizationErrorPolicy>(
    value: Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    purpose: CanonicalizationPurpose<'_>,
    error_policy: P,
) -> Result<Value, P::Error> {
    let mut visited_refs = Vec::new();
    let unwrapped = peel_custom(
        attr_type
            .resolve_refs_with_defs_on_path(defs, &mut visited_refs)
            .as_attr(),
    );
    if let Some(fit) = crate::schema::fit_value_to_resolved_type_on_path(
        unwrapped,
        &value,
        defs,
        purpose,
        &mut visited_refs,
    ) {
        return canonicalize_with_fit(value, fit, defs, purpose, error_policy);
    }

    canonicalize_without_fit(
        value,
        unwrapped,
        defs,
        purpose,
        error_policy,
        &mut visited_refs,
    )
}

/// Consume a successful fit witness without re-validating any selected Union
/// member or re-resolving any enum. The value and witness have identical tree
/// shapes by construction.
fn canonicalize_with_fit<P: CanonicalizationErrorPolicy>(
    value: Value,
    fit: crate::schema::Fit<'_>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    purpose: CanonicalizationPurpose<'_>,
    error_policy: P,
) -> Result<Value, P::Error> {
    if purpose.canonicalizes_full_shape() && is_string_or_list_of_strings(fit.attr, defs) {
        return Ok(canonicalize_to_string_list(value));
    }

    match (value, fit.kind) {
        (Value::Deferred(DeferredValue::Secret(value)), crate::schema::FitKind::Secret(fit)) => {
            canonicalize_with_fit(*value, *fit, defs, purpose, error_policy)
                .map(|value| Value::Deferred(DeferredValue::Secret(Box::new(value))))
                .map_err(|error| error_policy.mask_secret(error))
        }
        (value, crate::schema::FitKind::Union { member, fit }) => {
            // The witness retains the declaration member as well as its
            // resolved child fit. Canonicalization needs only the child, but
            // consuming both here makes the single selection decision
            // explicit and prevents a second member lookup.
            let _chosen_member = member;
            canonicalize_with_fit(value, *fit, defs, purpose, error_policy)
        }
        (
            _,
            crate::schema::FitKind::Enum {
                canonical,
                rewrite: true,
            },
        ) => Ok(Value::Concrete(ConcreteValue::CanonicalEnum(canonical))),
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
                        canonicalize_with_fit(value, fit.value, defs, purpose, error_policy)
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
                .map(|((key, value), fit)| {
                    let value = canonicalize_with_fit(value, fit, defs, purpose, error_policy)
                        .map_err(|error| error_policy.at_struct_field(key.clone(), error))?;
                    Ok((key, value))
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

fn collect_canonicalized_map<P: CanonicalizationErrorPolicy>(
    entries: Vec<(String, String, Value)>,
    error_policy: P,
) -> Result<IndexMap<String, Value>, P::Error> {
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

fn canonical_map_key_text(value: Value) -> Result<String, crate::schema::TypeError> {
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
            DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::Interpolation(_)
            | DeferredValue::FunctionCall { .. }
            | DeferredValue::Secret(_)
            | DeferredValue::RedactedSecret { .. }
            | DeferredValue::Unknown(_),
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
            AttrTypeKind::String { .. }
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

fn finish_no_fit<P: CanonicalizationErrorPolicy>(
    value: Value,
    expected: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    purpose: CanonicalizationPurpose<'_>,
    error_policy: P,
) -> Result<Value, P::Error> {
    if purpose.rejects_unrecognized_enum_text() && type_contains_enum(expected, defs) {
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
fn canonicalize_without_fit<'a, P: CanonicalizationErrorPolicy>(
    value: Value,
    unwrapped: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
    purpose: CanonicalizationPurpose<'_>,
    error_policy: P,
    visited_refs: &mut Vec<&'a str>,
) -> Result<Value, P::Error> {
    if let Value::Deferred(DeferredValue::Secret(inner)) = value {
        return canonicalize_without_fit(
            *inner,
            unwrapped,
            defs,
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
                visited_refs,
            )
        {
            let canonical = canonicalize_without_fit(
                value,
                peel_custom(member),
                defs,
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
                    canonicalize_with_type_for_purpose(value, inner, defs, purpose, error_policy)
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
                    let lifted_key = crate::schema::lift_map_key(key_type, &key, defs);
                    let canonical_key_value = canonicalize_with_type_for_purpose(
                        lifted_key,
                        key_type,
                        defs,
                        purpose,
                        error_policy,
                    )
                    .map_err(|error| error_policy.at_map_key(key.clone(), error))?;
                    let canonical_key = match canonical_map_key_text(canonical_key_value) {
                        Ok(canonical_key) => canonical_key,
                        Err(source) => error_policy
                            .canonicalization_failed(
                                Value::Concrete(ConcreteValue::String(key.clone().into())),
                                source,
                            )
                            .map(|_| key.clone())
                            .map_err(|error| error_policy.at_map_key(key.clone(), error))?,
                    };
                    let canonical_value = canonicalize_with_type_for_purpose(
                        value,
                        value_type,
                        defs,
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
                .map(|(key, value)| {
                    let canonical = match crate::schema::struct_field_for_key(fields, &key) {
                        Some(field) => canonicalize_with_type_for_purpose(
                            value,
                            &field.field_type,
                            defs,
                            purpose,
                            error_policy,
                        )
                        .map_err(|error| error_policy.at_struct_field(key.clone(), error))?,
                        None => value,
                    };
                    Ok((key, canonical))
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
    resolver: &crate::resource::EnumValueResolver<'_>,
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

fn first_enum_identifier(value: &Value) -> Option<EnumIdentifierOccurrence<'_>> {
    first_enum_identifier_inner(value, false)
}

fn first_enum_identifier_inner(
    value: &Value,
    inside_secret: bool,
) -> Option<EnumIdentifierOccurrence<'_>> {
    match value {
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
            | DeferredValue::Unknown(_)
            | DeferredValue::ResourceRef { .. }
            | DeferredValue::BindingRef { .. }
            | DeferredValue::RedactedSecret { .. },
        ) => None,
    }
}

/// Body of state and desired canonicalization for the
/// `string_or_list_of_strings` case.
fn canonicalize_to_string_list(value: Value) -> Value {
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
pub struct CanonicalizedResources<'a> {
    resources: &'a mut [crate::resource::Resource],
}

impl<'a> CanonicalizedResources<'a> {
    pub fn as_mut_slice(&mut self) -> &mut [crate::resource::Resource] {
        self.resources
    }
}

/// A desired value could not be resolved to the enum value declared by its
/// resource or data-source schema.
///
/// The structured [`crate::schema::TypeError`] remains the source so callers
/// can inspect the resolver failure without parsing a rendered diagnostic.
#[derive(Debug, Error)]
#[error("{resource}: failed to canonicalize enum attribute `{attribute}`: {source}")]
pub struct EnumCanonicalizationError {
    resource: Box<crate::resource::ResourceId>,
    attribute: String,
    #[source]
    source: Box<crate::schema::TypeError>,
}

impl EnumCanonicalizationError {
    fn new(
        resource: crate::resource::ResourceId,
        attribute: impl Into<String>,
        source: crate::schema::TypeError,
    ) -> Self {
        Self {
            resource: Box::new(resource),
            attribute: attribute.into(),
            source: Box::new(source),
        }
    }

    pub fn resource(&self) -> &crate::resource::ResourceId {
        &self.resource
    }

    pub fn attribute(&self) -> &str {
        &self.attribute
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

fn unresolved_enum_identifier_type_error_in(value: &Value) -> Option<crate::schema::TypeError> {
    let occurrence = first_enum_identifier(value)?;
    Some(mask_enum_identifier_error_for_secret(
        unresolved_enum_identifier_type_error(occurrence.raw),
        occurrence,
    ))
}

fn canonicalize_desired_attribute_map_with_schema<Attributes>(
    attributes: &mut Attributes,
    schema: &crate::schema::ResourceSchema,
    resource: &crate::resource::ResourceId,
    source: DesiredValueSource,
) -> Result<(), EnumCanonicalizationError>
where
    Attributes:
        Default + IntoIterator<Item = (String, Value)> + std::iter::FromIterator<(String, Value)>,
{
    let mut canonicalized = Vec::new();
    for (key, value) in std::mem::take(attributes) {
        let canonical = match schema.attributes.get(&key) {
            Some(attr_schema) => {
                canonicalize_desired_with_type(value, &attr_schema.attr_type, &schema.defs, source)
                    .map_err(|source| {
                        EnumCanonicalizationError::new(resource.clone(), key.clone(), source)
                    })?
            }
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

fn reject_unresolved_enum_identifiers_in_attributes<'a>(
    resource: &crate::resource::ResourceId,
    attributes: impl IntoIterator<Item = (&'a String, &'a Value)>,
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
    schema: &crate::schema::ResourceSchema,
) where
    Attributes:
        Default + IntoIterator<Item = (String, Value)> + std::iter::FromIterator<(String, Value)>,
{
    *attributes = std::mem::take(attributes)
        .into_iter()
        .map(|(key, value)| {
            let canonical = match schema.attributes.get(&key) {
                Some(attr_schema) => {
                    canonicalize_state_with_type(value, &attr_schema.attr_type, &schema.defs)
                }
                None => value,
            };
            (key, canonical)
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
pub fn canonicalize_resources_with_schemas<'a>(
    resources: &'a mut [crate::resource::Resource],
    registry: &crate::schema::SchemaRegistry,
) -> Result<CanonicalizedResources<'a>, EnumCanonicalizationError> {
    for resource in resources.iter_mut() {
        if let Some(schema) = registry.get_for(resource) {
            canonicalize_desired_attribute_map_with_schema(
                &mut resource.attributes,
                schema,
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
pub fn canonicalize_normalized_resources_with_schemas(
    resources: &mut [crate::resource::Resource],
    registry: &crate::schema::SchemaRegistry,
) -> Result<(), EnumCanonicalizationError> {
    for resource in resources.iter_mut() {
        if let Some(schema) = registry.get_for(resource) {
            canonicalize_desired_attribute_map_with_schema(
                &mut resource.attributes,
                schema,
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

type ProviderConfigAttributeTypeFn<'a> = dyn Fn(&str, &str) -> Option<AttributeType> + 'a;

/// Provider configs whose schema-known attributes have been canonicalized.
///
/// Durable identity code accepts this wrapper rather than raw
/// `ProviderConfig` slices, so callers cannot skip provider config enum
/// canonicalization at the hash seam.
#[derive(Debug, Clone)]
pub struct CanonicalizedProviderConfigs {
    providers: Vec<crate::parser::ProviderConfig>,
}

impl CanonicalizedProviderConfigs {
    pub fn as_slice(&self) -> &[crate::parser::ProviderConfig] {
        &self.providers
    }

    #[cfg(test)]
    pub(crate) fn from_configs_for_test(providers: Vec<crate::parser::ProviderConfig>) -> Self {
        Self { providers }
    }
}

/// Canonicalize one value using the same enum-resolution rules as provider
/// configuration attributes.
pub fn canonicalize_provider_config_with_type(
    provider_name: &str,
    value: Value,
    attr_type: &AttributeType,
) -> Result<Value, crate::schema::TypeError> {
    let defs = crate::schema::empty_defs_for_schema_walks();
    let canonical = canonicalize_with_type_for_purpose(
        value,
        attr_type,
        defs,
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
#[error("provider {provider}: failed to canonicalize enum attribute `{attribute}`: {source}")]
pub struct ProviderConfigCanonicalizationError {
    provider: String,
    attribute: String,
    #[source]
    source: Box<crate::schema::TypeError>,
}

impl ProviderConfigCanonicalizationError {
    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn attribute(&self) -> &str {
        &self.attribute
    }

    pub(crate) fn into_parts(self) -> (String, String, crate::schema::TypeError) {
        (self.provider, self.attribute, *self.source)
    }
}

/// Canonicalize one provider configuration attribute map using its
/// host-declared types. Typed canonicalization is total; the only explicit
/// identifier check is therefore for attributes without a declared type.
pub(crate) fn canonicalize_provider_config_attributes_with_types(
    provider_name: &str,
    attributes: &IndexMap<String, Value>,
    attr_types: &HashMap<String, AttributeType>,
) -> Result<IndexMap<String, Value>, ProviderConfigCanonicalizationError> {
    let mut canonical = IndexMap::with_capacity(attributes.len());
    for (attribute, value) in attributes {
        let value = match attr_types.get(attribute) {
            Some(attr_type) => {
                canonicalize_provider_config_with_type(provider_name, value.clone(), attr_type)
                    .map_err(|source| ProviderConfigCanonicalizationError {
                        provider: provider_name.to_string(),
                        attribute: attribute.clone(),
                        source: Box::new(source),
                    })?
            }
            None => {
                if let Some(source) = unresolved_enum_identifier_type_error_in(value) {
                    return Err(ProviderConfigCanonicalizationError {
                        provider: provider_name.to_string(),
                        attribute: attribute.clone(),
                        source: Box::new(source),
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
pub fn canonicalize_provider_configs_with_attribute_types(
    providers: &[crate::parser::ProviderConfig],
    provider_config_attribute_type_fn: &ProviderConfigAttributeTypeFn<'_>,
) -> Result<CanonicalizedProviderConfigs, ProviderConfigCanonicalizationError> {
    let mut providers = providers.to_vec();
    for provider in providers.iter_mut() {
        let attr_types = provider
            .attributes
            .keys()
            .filter_map(|attribute| {
                provider_config_attribute_type_fn(&provider.name, attribute)
                    .map(|attr_type| (attribute.clone(), attr_type))
            })
            .collect();
        provider.attributes = canonicalize_provider_config_attributes_with_types(
            &provider.name,
            &provider.attributes,
            &attr_types,
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
pub fn canonicalize_data_sources_with_schemas(
    data_sources: &mut [crate::resource::DataSource],
    registry: &crate::schema::SchemaRegistry,
) -> Result<(), EnumCanonicalizationError> {
    for data_source in data_sources.iter_mut() {
        if let Some(schema) = registry.get_for_data_source(data_source) {
            canonicalize_desired_attribute_map_with_schema(
                &mut data_source.attributes,
                schema,
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

/// Walk every entry in a `current_states` map and canonicalize attribute
/// values whose declared schema type is `Union[String, list(String)]`
/// into `Value::Concrete(ConcreteValue::StringList)`.
///
/// State files written before #2510 / #2511 (or by an apply path that
/// somehow produced the legacy shape) come back through serde as the
/// natural `Value::Concrete(ConcreteValue::String)` / `Value::Concrete(ConcreteValue::List)` form. Run this immediately
/// after `current_states` is built — typically right after
/// `StateFile::build_state_for_resource` populates the map — so the
/// differ never sees a non-canonical state value compared against a
/// canonical desired value. See #2481, #2513.
pub fn canonicalize_states_with_schemas(
    states: &mut std::collections::HashMap<crate::resource::ResourceId, crate::resource::State>,
    registry: &crate::schema::SchemaRegistry,
) {
    for state in states.values_mut() {
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
        canonicalize_state_attribute_map_with_schema(&mut state.attributes, schema);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redacted_secret(material: &str) -> Value {
        Value::Deferred(DeferredValue::RedactedSecret {
            hash: RedactedSecretHash::generate(material.as_bytes()),
        })
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
        use crate::resource::AccessPath;
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".to_string()]);
        let r = UnknownReason::UpstreamExport { path };
        assert_eq!(
            render_unknown(&r),
            "(known after upstream apply: network.vpc.vpc_id)"
        );
    }

    #[test]
    fn render_unknown_upstream_export_with_subscript() {
        use crate::resource::{AccessPath, Subscript};
        let path = AccessPath::with_fields_and_subscripts(
            "network",
            "accounts",
            Vec::new(),
            vec![Subscript::Int { index: 0 }],
        );
        let r = UnknownReason::UpstreamExport { path };
        assert_eq!(
            render_unknown(&r),
            "(known after upstream apply: network.accounts[0])"
        );
    }

    #[test]
    fn render_unknown_upstream_export_with_string_subscript() {
        use crate::resource::{AccessPath, Subscript};
        let path = AccessPath::with_fields_and_subscripts(
            "vpc",
            "tags",
            Vec::new(),
            vec![Subscript::Str { key: "Name".into() }],
        );
        let r = UnknownReason::UpstreamExport { path };
        assert_eq!(
            render_unknown(&r),
            "(known after upstream apply: vpc.tags[\"Name\"])"
        );
    }

    #[test]
    fn render_unknown_for_key() {
        assert_eq!(
            render_unknown(&UnknownReason::ForKey),
            "(known after upstream apply: key)"
        );
    }

    #[test]
    fn render_unknown_for_index() {
        assert_eq!(
            render_unknown(&UnknownReason::ForIndex),
            "(known after upstream apply: index)"
        );
    }

    #[test]
    fn render_unknown_for_value() {
        assert_eq!(
            render_unknown(&UnknownReason::ForValue),
            "(known after upstream apply)"
        );
    }

    #[test]
    fn render_unknown_for_value_path() {
        // carina#3136: the path-carrying loop-var placeholder renders
        // the path like every other payload-carrying arm (not the bare
        // `ForValue` string), so an unresolved chained loop-var access
        // stays distinguishable in plan output.
        use crate::resource::AccessPath;
        let r = UnknownReason::ForValuePath {
            path: AccessPath::with_fields("opt", "resource_record", vec!["name".to_string()]),
        };
        assert_eq!(
            render_unknown(&r),
            "(known after upstream apply: opt.resource_record.name)"
        );
    }

    #[test]
    fn render_unknown_post_create_read_incomplete() {
        let r = UnknownReason::PostCreateReadIncomplete {
            detail: "AccessDenied".to_string(),
        };
        assert_eq!(
            render_unknown(&r),
            "(known after next apply: post-create read failed — AccessDenied)"
        );
    }

    /// `UnknownReason::Display` flows into the user-facing error from
    /// `format_plan_save_error` / `SerializationError::Display`. Pin
    /// the wording so a future regression does not silently degrade
    /// the diagnostic.
    #[test]
    fn unknown_reason_display_wording() {
        use crate::resource::AccessPath;
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        assert_eq!(
            format!("{}", UnknownReason::UpstreamExport { path }),
            "upstream value network.vpc.vpc_id"
        );
        assert_eq!(
            format!("{}", UnknownReason::ForKey),
            "deferred for-binding key"
        );
        assert_eq!(
            format!("{}", UnknownReason::ForIndex),
            "deferred for-binding index"
        );
        assert_eq!(
            format!("{}", UnknownReason::ForValue),
            "deferred for-binding value"
        );
        assert_eq!(
            format!(
                "{}",
                UnknownReason::PostCreateReadIncomplete {
                    detail: "AccessDenied".to_string()
                }
            ),
            "post-create read failed: AccessDenied"
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
        let v = Value::resource_ref("vpc", "id", vec![]);
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
            name: "join".into(),
            args: vec![],
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
    fn test_json_to_dsl_value_string() {
        let j = serde_json::json!("hello");
        assert_eq!(
            json_to_dsl_value(&j),
            Some(Value::Concrete(ConcreteValue::String("hello".into())))
        );
    }

    #[test]
    fn state_row_load_upgrades_raw_legacy_markers_to_self_contained_markers() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let marker = format!("{LEGACY_SECRET_PREFIX}{hash}");
        let json = serde_json::json!({ "nested": [marker.clone()] });

        let ordinary = json_to_dsl_value(&json).expect("ordinary JSON value");
        assert!(!contains_redacted_secret(&ordinary));

        let context_free = persisted_json_to_dsl_value(&json).expect("persisted JSON value");
        assert!(
            !contains_redacted_secret(&context_free),
            "a raw legacy marker is not a context-free persisted encoding",
        );

        let id = crate::resource::ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "old",
            None,
        );
        let attributes = crate::provider::persisted_attributes_from_test_state_row(
            &id,
            HashMap::from([("tags".to_string(), json)]),
        )
        .expect("state row decode");
        let upgraded = attributes
            .encoded_value("tags")
            .expect("upgraded persisted JSON");
        assert!(
            upgraded["nested"][0]
                .as_str()
                .is_some_and(|marker| marker.starts_with(EXPLICIT_LEGACY_SECRET_PREFIX)),
        );
        let persisted = attributes.get("tags").expect("persisted JSON value");
        let Value::Concrete(ConcreteValue::Map(map)) = persisted else {
            panic!("persisted object must remain a map")
        };
        let Some(Value::Concrete(ConcreteValue::List(values))) = map.get("nested") else {
            panic!("persisted nested value must remain a list")
        };
        assert!(matches!(
            values.as_slice(),
            [Value::Deferred(DeferredValue::RedactedSecret { hash: loaded })]
                if loaded.persisted_marker().starts_with(EXPLICIT_LEGACY_SECRET_PREFIX)
        ));
    }

    #[test]
    fn raw_legacy_secret_digest_is_not_context_free() {
        let marker = format!("{LEGACY_SECRET_PREFIX}{}", "ab".repeat(32));
        assert!(
            persisted_secret_hash(&marker).is_none(),
            "a raw legacy digest must first be upgraded by its state-file row",
        );
    }

    #[test]
    fn redacted_secret_hash_deserialize_accepts_only_its_serialized_encoding() {
        let bare_hash = "ab".repeat(32);
        let marker = format!("{LEGACY_SECRET_PREFIX}{bare_hash}");
        let id = crate::resource::ResourceId::with_provider_identity(
            "mock",
            "test.resource",
            "old",
            None,
        );
        let attributes = crate::provider::persisted_attributes_from_test_state_row(
            &id,
            HashMap::from([("comment".to_string(), serde_json::json!(marker))]),
        )
        .expect("state row decode");
        let digest = attributes
            .redacted_secret_hash("comment")
            .expect("state load must upgrade a raw legacy marker");
        let encoded = serde_json::to_value(&digest).expect("serialize typed digest");

        let decoded_hash: RedactedSecretHash =
            serde_json::from_value(encoded).expect("deserialize serialized digest");
        assert!(
            decoded_hash
                .persisted_marker()
                .starts_with(EXPLICIT_LEGACY_SECRET_PREFIX)
        );
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
        let v2_marker = v2.persisted_marker();
        let v2_encoded = serde_json::to_value(&v2).expect("serialize v2 digest");
        let decoded_v2_hash: RedactedSecretHash =
            serde_json::from_value(v2_encoded).expect("deserialize serialized v2 digest");
        assert_eq!(decoded_v2_hash.persisted_marker(), v2_marker);
        assert!(
            serde_json::from_value::<RedactedSecretHash>(serde_json::json!(v2_marker)).is_err(),
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
        let v = Value::Concrete(ConcreteValue::String("hello".into()));
        assert_eq!(format_value(&v), "\"hello\"");
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
            assert_eq!(format_value(&value), format!("\"{text}\""));
        }
    }

    #[test]
    fn test_format_value_user_facing_string_is_unquoted() {
        let v = Value::Concrete(ConcreteValue::String("hello".into()));
        assert_eq!(format_value_user_facing(&v), "hello");
    }

    #[test]
    fn test_format_value_user_facing_non_string_matches_format_value() {
        for value in [
            Value::Concrete(ConcreteValue::Bool(true)),
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
            Value::resource_ref("vpc", "id", vec![]),
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
        assert_eq!(format_value(&v), "\"dedicated\"");
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

    #[test]
    fn value_to_json_serializes_canonical_enum_as_typed_object() {
        let v = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                crate::schema::TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
                "vpc",
            ),
        ));

        assert_eq!(
            value_to_json(&v).unwrap(),
            serde_json::json!({
                "Enum": {
                    "identity": {
                        "provider": "aws",
                        "segments": ["ec2", "Eip"],
                        "kind": "Domain"
                    },
                    "api_value": "vpc"
                }
            })
        );
    }

    #[test]
    fn json_to_dsl_value_preserves_canonical_enum_typed_object() {
        let v = Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                crate::schema::TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
                "vpc",
            ),
        ));
        let json = value_to_json(&v).unwrap();

        assert_eq!(json_to_dsl_value(&json), Some(v));
    }

    #[test]
    fn canonicalize_resources_with_schemas_replaces_enum_leaves_recursively() {
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeIdentity,
        };

        let domain = AttributeType::enum_(
            TypeIdentity::new(Some("aws"), ["ec2", "Eip"], "Domain"),
            Some(vec!["vpc".to_string(), "standard".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            ResourceSchema::new("ec2.Eip")
                .attribute(AttributeSchema::new("domain", domain.clone()))
                .attribute(AttributeSchema::new(
                    "domains",
                    AttributeType::list(domain.clone()),
                ))
                .attribute(AttributeSchema::new(
                    "domain_by_name",
                    AttributeType::map(domain),
                )),
        );
        let mut resource = crate::resource::Resource::with_provider("aws", "ec2.Eip", "eip", None);
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
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
            TypeIdentity,
        };

        let hosted_zone = AttributeType::enum_(
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
        let alias_target = AttributeType::struct_(
            "AliasTarget",
            vec![StructField::new(
                "hosted_zone_id",
                AttributeType::union(vec![hosted_zone, AttributeType::string()]),
            )],
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            ResourceSchema::new("route53.RecordSet")
                .attribute(AttributeSchema::new("alias_target", alias_target)),
        );

        let canonicalize = |text: &str| {
            let mut resource = crate::resource::Resource::with_provider(
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
        use crate::schema::{AttributeType, StructField, TypeIdentity};

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
            AttributeType::struct_("A", vec![StructField::new("mode", mode)]),
            AttributeType::struct_("B", vec![StructField::new("kind", kind)]),
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
        use crate::schema::{AttributeType, TypeIdentity};

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
            vec![crate::schema::StructField::new("a", open_enum)],
        );
        let string_struct = AttributeType::struct_(
            "StringArm",
            vec![crate::schema::StructField::new(
                "a",
                AttributeType::string(),
            )],
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
                Some(crate::schema::TypeIdentity::bare("First")),
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
                Some(crate::schema::TypeIdentity::bare("Second")),
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
        use crate::schema::{AttributeType, StructField, TypeIdentity};

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
                vec![StructField::new("mode", mode)],
            )),
            AttributeType::list(AttributeType::struct_(
                "B",
                vec![StructField::new("kind", kind)],
            )),
        ]);
        let value = Value::Concrete(ConcreteValue::List(vec![
            Value::Deferred(DeferredValue::BindingRef {
                binding: "later".to_string(),
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
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeIdentity,
        };

        let first = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget"], "FirstMode"),
            Some(vec!["FIRST".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let second = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget"], "SecondMode"),
            Some(vec!["SECOND".to_string()]),
            vec![("SECOND".to_string(), "second".to_string())],
            None,
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            ResourceSchema::new("widget").attribute(AttributeSchema::new(
                "mode",
                AttributeType::union(vec![first, second]),
            )),
        );
        let mut resource =
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
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeIdentity,
        };

        let plaintext = "plaintext-must-not-leak";
        let mode = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget"], "Mode"),
            Some(vec!["allowed".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            ResourceSchema::new("widget").attribute(AttributeSchema::new("mode", mode)),
        );
        let mut resource =
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
        use crate::schema::{ResourceSchema, SchemaRegistry};

        let plaintext = "none-schema-plaintext-must-not-leak";
        let secret = || {
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::enum_identifier(plaintext),
            ))))
        };
        let registries = [SchemaRegistry::new(), {
            let mut registry = SchemaRegistry::new();
            registry.insert("test", ResourceSchema::new("widget"));
            registry
        }];

        for registry in registries {
            let mut resources = vec![
                crate::resource::Resource::with_provider("test", "widget", "secret", None)
                    .with_attribute("unknown", secret()),
            ];
            let error = canonicalize_resources_with_schemas(&mut resources, &registry)
                .expect_err("an unresolved secret enum must fail without a declared attribute");
            let display = error.to_string();
            assert!(!display.contains(plaintext), "{display}");
            assert!(display.contains("secret"), "{display}");
        }
    }

    #[test]
    fn resource_handle_validation_and_desired_canonicalization_agree_on_string_wire_value() {
        let handle = AttributeType::resource_handle("test", "widget");
        let string_id = Value::Concrete(ConcreteValue::String("widget-123".into()));
        let identifier = Value::Concrete(ConcreteValue::enum_identifier("widget_binding"));

        assert!(handle.validate(&string_id).is_ok());
        assert!(matches!(
            handle.validate(&identifier),
            Err(crate::schema::TypeError::TypeMismatch { .. })
        ));
        assert_eq!(
            canonicalize_desired_with_type(
                string_id.clone(),
                &handle,
                crate::schema::empty_defs_for_schema_walks(),
                DesiredValueSource::AuthoredDsl,
            )
            .unwrap(),
            string_id,
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
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
            TypeIdentity,
        };

        let mode = AttributeType::enum_(
            TypeIdentity::new(Some("test"), ["widget"], "Mode"),
            Some(vec!["Allow".to_string(), "Deny".to_string()]),
            vec![("Allow".to_string(), "allow".to_string())],
            None,
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            ResourceSchema::new("widget")
                .attribute(AttributeSchema::new("top", mode.clone()))
                .attribute(AttributeSchema::new(
                    "nested",
                    AttributeType::struct_(
                        "Settings",
                        vec![StructField::new("mode", mode.clone())],
                    ),
                ))
                .attribute(AttributeSchema::new(
                    "items",
                    AttributeType::list(mode.clone()),
                ))
                .attribute(AttributeSchema::new(
                    "unselected_union",
                    AttributeType::union(vec![
                        AttributeType::list(mode),
                        AttributeType::map(AttributeType::string()),
                    ]),
                )),
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
            let mut resource =
                crate::resource::Resource::with_provider("test", "widget", attribute, None);
            resource.set_attr(attribute, value);
            let mut resources = vec![resource];

            let error = canonicalize_resources_with_schemas(&mut resources, &registry)
                .expect_err("an unresolved enum must fail canonicalization");

            assert!(error.to_string().contains(attribute), "{error}");
            assert!(
                std::error::Error::source(&error).is_some(),
                "canonicalization errors must retain the resolver error source"
            );
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
        use crate::binding_index::{PreApplyInputs, ResolvedBindings};
        use crate::resource::InterpolationPart;
        use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};

        let raw = || Value::Concrete(ConcreteValue::enum_identifier("test.widget.Mode.allow"));
        let runtime_ref = || Value::resource_ref("source", "suffix", Vec::new());
        let runtime = [
            Value::Deferred(DeferredValue::FunctionCall {
                name: "join".to_string(),
                args: vec![
                    Value::Concrete(ConcreteValue::String("".into())),
                    Value::Concrete(ConcreteValue::List(vec![raw(), runtime_ref()])),
                ],
            }),
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Expr(raw()),
                InterpolationPart::Expr(runtime_ref()),
            ])),
        ];
        let static_values = [
            Value::Deferred(DeferredValue::FunctionCall {
                name: "join".to_string(),
                args: vec![
                    Value::Concrete(ConcreteValue::String("".into())),
                    Value::Concrete(ConcreteValue::List(vec![
                        raw(),
                        Value::Concrete(ConcreteValue::String("-suffix".into())),
                    ])),
                ],
            }),
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Expr(raw()),
                InterpolationPart::Expr(Value::Concrete(ConcreteValue::String("-suffix".into()))),
            ])),
        ];
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "test",
            ResourceSchema::new("widget")
                .attribute(AttributeSchema::new("value", AttributeType::string())),
        );
        let source = crate::resource::Resource::with_provider("test", "source", "source", None)
            .with_binding("source")
            .with_attribute(
                "suffix",
                Value::Concrete(ConcreteValue::String("-suffix".into())),
            );
        let managed = vec![source];
        let current_states = std::collections::HashMap::new();
        let remote_bindings = std::collections::HashMap::new();
        let bindings = ResolvedBindings::pre_apply(PreApplyInputs {
            managed: &managed,
            compositions: &[],
            data_sources: &[],
            current_states: &current_states,
            remote_bindings: &remote_bindings,
            wait_aliases: &[],
        });

        for (runtime, static_value) in runtime.into_iter().zip(static_values) {
            let mut resource =
                crate::resource::Resource::with_provider("test", "widget", "example", None);
            resource.set_attr("value", runtime);
            let mut resources = vec![resource];

            canonicalize_resources_with_schemas(&mut resources, &registry)
                .expect("a deferred expression owns evaluation of its enum-shaped arguments");
            let deferred_result =
                crate::resolver::resolve_ref_value(&resources[0].attributes["value"], &bindings)
                    .expect("runtime expression must resolve");
            let static_result =
                crate::resolver::resolve_ref_value(&static_value, &ResolvedBindings::default())
                    .expect("static expression must evaluate");

            assert_eq!(deferred_result, static_result);
        }
    }

    #[test]
    fn canonicalize_provider_configs_replaces_region_enum_with_canonical_api_value() {
        use crate::parser::ProviderConfig;
        use crate::schema::{AttributeType, TypeIdentity};

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

        let providers =
            canonicalize_provider_configs_with_attribute_types(&providers, &|_provider, attr| {
                (attr == "region").then(|| {
                    AttributeType::enum_(
                        TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Region"),
                        Some(vec!["ap-northeast-1".to_string()]),
                        vec![("ap-northeast-1".to_string(), "ap_northeast_1".to_string())],
                        None,
                        None,
                    )
                })
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
        use crate::schema::{AttributeType, TypeIdentity};

        let attr_types = HashMap::from([(
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
        use crate::schema::{AttributeType, TypeIdentity};

        let attr_types = HashMap::from([(
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

        canonicalize_provider_config_with_type(
            "awscc",
            Value::Concrete(ConcreteValue::enum_identifier("aws.Region.tokyo")),
            &region,
        )
        .expect("a successful first result must be returned without a fallback retry");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn provider_config_canonicalization_masks_secret_enum_errors() {
        use crate::schema::AttributeType;

        let plaintext = "provider-config-plaintext-must-not-leak";
        let secret = || {
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::enum_identifier(plaintext),
            ))))
        };

        let typed =
            canonicalize_provider_config_with_type("test", secret(), &AttributeType::string())
                .expect_err("a typed provider config must reject the unresolved enum");
        assert!(!typed.to_string().contains(plaintext), "{typed}");

        let attributes = IndexMap::from([("unknown".to_string(), secret())]);
        let untyped = canonicalize_provider_config_attributes_with_types(
            "test",
            &attributes,
            &HashMap::new(),
        )
        .expect_err("an untyped provider config must reject the unresolved enum");
        let display = untyped.to_string();
        assert!(!display.contains(plaintext), "{display}");
        assert!(display.contains("secret"), "{display}");
    }

    #[test]
    fn provider_config_typed_canonicalization_is_total() {
        let error = canonicalize_provider_config_with_type(
            "test",
            Value::Concrete(ConcreteValue::enum_identifier("test.Mode.invalid")),
            &AttributeType::string(),
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
        let v = Value::resource_ref("vpc", "id", vec![]);
        assert_eq!(format_value(&v), "vpc.id");
    }

    /// `Value::Deferred(DeferredValue::Unknown(UpstreamExport))` renders unquoted as
    /// `(known after upstream apply: <ref>)` via `format_value_with_key`.
    /// Stage 2 of RFC #2371 — the variant replaced the NUL-prefixed
    /// `Value::Concrete(ConcreteValue::String)` sentinel from #2367.
    #[test]
    fn test_format_value_unresolved_upstream() {
        use crate::resource::{AccessPath, UnknownReason};
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".to_string()]);
        let v = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
            path,
        }));
        assert_eq!(
            format_value(&v),
            "(known after upstream apply: network.vpc.vpc_id)"
        );
    }

    /// RFC #2371 stage 4 contract pin: serialization boundaries return
    /// `Err(SerializationError::UnknownNotAllowed { reason })` rather
    /// than panicking. The `reason` field must round-trip the variant
    /// passed in so the caller can render an actionable diagnostic.
    /// A silent fallback (e.g. `Ok(Value::Concrete(ConcreteValue::String("Unknown(...)")))`)
    /// would re-introduce the v1 corruption bug (#2375).
    #[test]
    fn unknown_returns_err_in_value_to_json() {
        let v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey));
        let err = value_to_json(&v).unwrap_err();
        assert!(
            matches!(
                err,
                SerializationError::UnknownNotAllowed {
                    reason: UnknownReason::ForKey,
                    context: SerializationContext::ValueToJson,
                }
            ),
            "expected UnknownNotAllowed/ForKey/ValueToJson, got: {err:?}"
        );
    }

    #[test]
    fn post_create_read_incomplete_unknown_returns_err_in_value_to_json() {
        let v = Value::Deferred(DeferredValue::Unknown(
            UnknownReason::PostCreateReadIncomplete {
                detail: "mock partial create".to_string(),
            },
        ));
        let err = value_to_json(&v).unwrap_err();
        assert!(
            matches!(
                err,
                SerializationError::UnknownNotAllowed {
                    reason: UnknownReason::PostCreateReadIncomplete { .. },
                    context: SerializationContext::ValueToJson,
                }
            ),
            "expected UnknownNotAllowed/PostCreateReadIncomplete/ValueToJson, got: {err:?}"
        );
    }

    #[test]
    fn unknown_returns_err_in_redact_secrets_in_value() {
        let v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey));
        let err = redact_secrets_in_value(&v).unwrap_err();
        assert!(
            matches!(
                err,
                SerializationError::UnknownNotAllowed {
                    reason: UnknownReason::ForKey,
                    context: SerializationContext::SecretRedaction,
                }
            ),
            "expected UnknownNotAllowed/ForKey/SecretRedaction, got: {err:?}"
        );
    }

    #[test]
    fn test_format_value_resource_ref_with_field_path() {
        let v = Value::resource_ref("web", "network", vec!["vpc_id".to_string()]);
        assert_eq!(format_value(&v), "web.network.vpc_id");
    }

    #[test]
    fn test_value_to_json_resource_ref_with_field_path_returns_err() {
        let v = Value::resource_ref(
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
        let s = json.as_str().unwrap();
        assert!(
            s.starts_with(SECRET_PREFIX),
            "Expected secret hash prefix, got: {}",
            s
        );
        let hash = persisted_secret_hash(s).expect("v2 marker carries a validated PHC digest");
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
        let secret_val = obj.get("SecretTag").unwrap().as_str().unwrap();
        assert!(
            secret_val.starts_with(SECRET_PREFIX),
            "Expected secret hash in map value JSON, got: {}",
            secret_val
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
        for marker in [json1, json2] {
            let hash = persisted_secret_hash(marker.as_str().unwrap()).unwrap();
            assert!(hash.verify(&material));
        }
    }

    #[test]
    fn generated_secret_marker_is_parameterized_argon2id_phc() {
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("phc-password".into()),
        ))));
        let marker = value_to_json(&secret).expect("serialize secret");
        let marker = marker.as_str().expect("secret marker string");
        let phc = marker
            .strip_prefix("_secret:")
            .expect("persisted marker prefix");
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
    fn persisted_secret_marker_rejects_non_carina_argon2_parameters() {
        use argon2::password_hash::{PasswordHasher, SaltString};

        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("bounded-argon2".into()),
        ))));
        let marker = value_to_json(&secret).expect("serialize secret");
        let marker = marker.as_str().expect("secret marker string");
        let mut invalid = vec![
            (
                "memory cost",
                marker.replacen(
                    &format!("m={}", argon2::Params::DEFAULT_M_COST),
                    "m=1048576",
                    1,
                ),
            ),
            (
                "time cost",
                marker.replacen(&format!("t={}", argon2::Params::DEFAULT_T_COST), "t=3", 1),
            ),
            (
                "parallelism",
                marker.replacen(&format!("p={}", argon2::Params::DEFAULT_P_COST), "p=2", 1),
            ),
            ("version", marker.replacen("v=19", "v=16", 1)),
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
            invalid.push((label, format!("_secret:{phc}")));
        }

        for (label, marker) in invalid {
            assert!(
                persisted_secret_hash(&marker).is_none(),
                "state must reject a non-Carina Argon2 {label}",
            );
        }
    }

    #[test]
    fn unresolved_secret_with_null_provider_value_returns_an_error() {
        let desired = Value::Deferred(DeferredValue::Secret(Box::new(Value::resource_ref(
            "upstream",
            "value",
            Vec::new(),
        ))));

        let error = merge_secrets_into_provider_json(&desired, &serde_json::Value::Null, None)
            .expect_err("top-level null secret material must be rejected without panicking");
        assert!(matches!(error, SerializationError::SecretNotAllowed { .. }));
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
        let json1 = value_to_json(&v).unwrap();
        let json2 = value_to_json_reusing_secret_hashes(&v, Some(&json1)).unwrap();
        assert_eq!(
            json1, json2,
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
        let previous = value_to_json(&old).unwrap();
        let replacement = value_to_json_reusing_secret_hashes(&new, Some(&previous)).unwrap();
        assert_ne!(previous, replacement);
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
    fn deferred_secret_interpolation_is_one_placeholder() {
        let candidate = Value::Deferred(DeferredValue::Secret(Box::new(Value::Deferred(
            DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("DEFPLAIN-".into()),
                InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef {
                    path: crate::resource::AccessPath::new("r", "identifier"),
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
                    path: crate::resource::AccessPath::new("r", "identifier"),
                })),
                InterpolationPart::Literal("-TAIL".into()),
            ]),
        ))));
        assert!(matches!(
            rehydrate_redacted_secrets(&redacted, &changed),
            Err(SecretRehydrationError::HashMismatch)
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

        assert_ne!(
            secret_hash(&inner, None).expect("first hash"),
            secret_hash(&inner, None).expect("second hash"),
            "independent placeholders must not reveal equal plaintexts",
        );
    }

    #[test]
    fn saved_plan_placeholder_reuses_matching_persisted_digest() {
        let resource = resource_with_comment_secret("same-resource", "PLAIN");
        let state_value = value_to_json(resource.attributes.get("comment").expect("comment"))
            .expect("serialize state secret");
        let state_hash = persisted_secret_hash(state_value.as_str().unwrap()).unwrap();
        let persisted: crate::provider::PersistedAttributes =
            serde_json::from_value(serde_json::json!({ "comment": state_value })).unwrap();
        let redacted = redact_secrets_in_resource_reusing_persisted_hashes(&resource, &persisted)
            .expect("redact resource");

        assert_eq!(resource_comment_placeholder_hash(&redacted), &state_hash);
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
        let reference = Value::resource_ref("producer", "comment", vec![]);

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
        use crate::resource::{ResolvedResource, Resource, ResourceId, State};

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
            from: Box::new(canonical_from.clone()),
            to: ResolvedResource::new(to.clone()),
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
            name: "decrypt".to_string(),
            args: vec![Value::Concrete(ConcreteValue::String("ciphertext".into()))],
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
            name: "format".to_string(),
            args: vec![interpolation],
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
    fn test_redact_secrets_in_virtual_redacts_attribute_secrets() {
        // carina#3248: compositions are now persisted in saved plans, so
        // a literal secret authored inside a module's `attributes { ... }`
        // block (which lands as a `Value::Secret` in the composition's
        // attribute map) must be redacted before serialization, the
        // same way managed-resource attributes are redacted.
        use crate::resource::CompositionAttribute;
        use crate::resource::{Composition, ResourceId, Signature};
        use std::collections::{BTreeSet, HashSet};
        let mut attrs: indexmap::IndexMap<String, CompositionAttribute> = indexmap::IndexMap::new();
        attrs.insert(
            "non_secret".to_string(),
            CompositionAttribute::from_value(
                Value::Concrete(ConcreteValue::String("kept".into())),
                None,
            ),
        );
        attrs.insert(
            "secret_field".to_string(),
            CompositionAttribute::from_value(
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("plaintext-must-not-leak".into()),
                )))),
                None,
            ),
        );
        let virt = Composition {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual",
                "module_instance",
            )),
            signature: Signature {
                arguments: indexmap::IndexMap::new(),
                attributes: attrs,
                pending_constraints: Vec::new(),
            },
            binding: Some("module_instance".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "m".to_string(),
            instance: "module_instance".to_string(),
            provenance: Default::default(),
            quoted_string_attrs: HashSet::new(),
        };

        let redacted = redact_secrets_in_virtual(&virt).expect("redact virtual");

        // The non-secret attribute survives verbatim.
        assert_eq!(
            redacted
                .signature
                .attributes
                .get("non_secret")
                .map(|a| a.to_value()),
            Some(Value::Concrete(ConcreteValue::String("kept".into()))),
        );

        // The secret attribute is replaced with the typed placeholder, not
        // plaintext or an ordinary string.
        match redacted
            .signature
            .attributes
            .get("secret_field")
            .map(|a| a.to_value())
            .as_ref()
        {
            Some(Value::Deferred(DeferredValue::RedactedSecret { hash })) => {
                assert!(hash.encoded_digest().starts_with("$argon2id$"));
            }
            other => panic!("expected redacted secret, got: {:?}", other),
        }

        // The rest of the Composition (id, binding, etc.) is
        // preserved verbatim.
        assert_eq!(redacted.binding.as_deref(), Some("module_instance"));
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

    // ----- format_value_pretty tests -----

    /// Helper to construct a layout for tests.
    fn layout(parent_indent_cols: usize, key: &str) -> PrettyLayout<'_> {
        PrettyLayout {
            parent_indent_cols,
            key,
        }
    }

    #[test]
    fn format_value_pretty_string_matches_format_value() {
        let v = Value::Concrete(ConcreteValue::String("hello".into()));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), format_value(&v));
    }

    #[test]
    fn format_value_pretty_int_renders_as_integer_literal() {
        let v = Value::Concrete(ConcreteValue::Int(42));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), "42");
    }

    #[test]
    fn format_value_pretty_bool_renders_as_keyword() {
        let v = Value::Concrete(ConcreteValue::Bool(true));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), "true");
    }

    #[test]
    fn format_value_pretty_dsl_enum_resolves_to_provider_value() {
        let v = Value::Concrete(ConcreteValue::String(
            "aws.s3.Bucket.VersioningStatus.enabled".into(),
        ));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), format_value(&v));
    }

    #[test]
    fn format_value_pretty_secret_masked() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".into()),
        ))));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), "(secret)");
    }

    #[test]
    fn format_value_pretty_unknown_renders_like_format_value() {
        let v = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), format_value(&v));
    }

    #[test]
    fn format_value_pretty_list_of_maps_vertical() {
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
        let out = format_value_pretty(&v, layout(6, "statement"));
        let expected = "\n        * effect: \"Allow\"\n          sid: \"First\"\n\n        * effect: \"Deny\"\n          sid: \"Second\"";
        assert_eq!(out, expected);
    }

    #[test]
    fn format_value_pretty_list_of_maps_single_entry() {
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
        let out = format_value_pretty(&v, layout(4, "items"));
        assert_eq!(out, "\n      * k: \"v\"");
        // Single element must not introduce a blank-line separator.
        assert!(
            !out.contains("\n\n"),
            "single-entry list-of-maps must not contain blank separator: {out:?}"
        );
    }

    #[test]
    fn format_value_pretty_list_of_maps_three_entries_separated_by_blank_lines() {
        let make = |sid: &str| {
            let mut m = IndexMap::new();
            m.insert(
                "sid".to_string(),
                Value::Concrete(ConcreteValue::String(sid.to_string().into())),
            );
            Value::Concrete(ConcreteValue::Map(m))
        };
        let v = Value::Concrete(ConcreteValue::List(vec![make("A"), make("B"), make("C")]));
        let out = format_value_pretty(&v, layout(4, "items"));
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
    fn format_value_pretty_map_with_list_of_maps_then_sibling_separates() {
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
        let out = format_value_pretty(&v, layout(2, "config"));
        // The blank-line MUST sit between the last element's last key
        // and the sibling `version:` key. Use unique key names so the
        // assertion can't be satisfied by the inter-element blank
        // between two `sid:`s.
        assert!(
            out.contains(&format!("sid_b: \"{long}\"\n\n")),
            "blank line must follow last list-of-maps element before sibling: {out:?}"
        );
    }

    /// A multi-element list-of-maps that is the LAST key of its parent
    /// must NOT receive a trailing blank, otherwise the resource-block
    /// separator above it doubles into two blank lines. (#2555)
    #[test]
    fn format_value_pretty_map_with_trailing_list_of_maps_no_orphan_blank() {
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
        let out = format_value_pretty(&v, layout(2, "config"));
        assert!(
            !out.ends_with("\n\n"),
            "trailing list-of-maps must not leave an orphan blank: {out:?}"
        );
    }

    #[test]
    fn format_value_pretty_empty_list_inline() {
        let v = Value::Concrete(ConcreteValue::List(vec![]));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), "[]");
    }

    #[test]
    fn format_value_pretty_list_of_strings_under_80_inline() {
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            Value::Concrete(ConcreteValue::String("b".into())),
        ]));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), "[\"a\", \"b\"]");
    }

    #[test]
    fn format_value_pretty_list_of_strings_over_80_vertical() {
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
        let out = format_value_pretty(&v, layout(4, "action"));
        assert!(
            out.starts_with("[\n"),
            "expected bracket-newline start, got: {out}"
        );
        assert!(
            out.contains("\n      \"iam:LongActionName0\","),
            "missing first item line at col 6: {out}"
        );
        assert!(
            out.ends_with("\n    ]"),
            "expected closing bracket at parent_indent col: {out}"
        );
    }

    #[test]
    fn format_value_pretty_list_of_strings_threshold_boundary_exact_80_inline() {
        // Inline form fits exactly within 80 cols at parent_indent=0, key="x" (1 char):
        // total = 0 + 1 + 2 + len(inline). For len(inline)=77, total=80 → stay inline.
        let item = "x".repeat(73); // inline = 1 + 1 + 73 + 1 + 1 = 77
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String(item.into()),
        )]));
        let inline = format_value_with_key(&v, None);
        assert_eq!(inline.len(), 77, "fixture sanity: {} chars", inline.len());
        // total budget = 0 + 1 + 2 + 77 = 80, exactly at limit → inline.
        assert_eq!(format_value_pretty(&v, layout(0, "x")), inline);
    }

    #[test]
    fn format_value_pretty_list_of_strings_threshold_boundary_81_expands() {
        // 1 char over threshold: 0 + 1 + 2 + 78 = 81 → expand.
        let item = "x".repeat(74); // inline = 78
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String(item.into()),
        )]));
        let inline = format_value_with_key(&v, None);
        assert_eq!(inline.len(), 78, "fixture sanity: {} chars", inline.len());
        let out = format_value_pretty(&v, layout(0, "x"));
        assert!(
            out.starts_with("[\n"),
            "1 over threshold should expand, got: {out}"
        );
    }

    #[test]
    fn format_value_pretty_list_of_strings_indent_pushes_over_threshold() {
        // Inline form is 75 chars; at parent_indent=10, key="kk" (2),
        // total = 10 + 2 + 2 + 75 = 89 → expand.
        let inline_target = 75;
        let item = "x".repeat(inline_target - 4);
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String(item.into()),
        )]));
        let inline = format_value_with_key(&v, None);
        assert_eq!(inline.len(), inline_target);
        let out = format_value_pretty(&v, layout(10, "kk"));
        assert!(out.starts_with("[\n"), "deep indent should expand: {out}");
    }

    #[test]
    fn format_value_pretty_list_of_maps_with_nested_map() {
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
        let out = format_value_pretty(&v, layout(4, "statement"));
        assert!(
            out.contains("      * condition:"),
            "expected `* condition:` at col 6, got: {out}"
        );
        assert!(out.contains("sid: \"X\""), "expected sid line, got: {out}");
    }

    #[test]
    fn format_value_pretty_list_of_maps_with_long_string_list_inside() {
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
        let out = format_value_pretty(&v, layout(4, "statement"));
        assert!(
            out.contains("action: ["),
            "expected expanded action list bracket: {out}"
        );
        assert!(
            out.contains("\"iam:Action000\","),
            "expected first action on its own line: {out}"
        );
    }

    #[test]
    fn format_value_pretty_string_list_under_dynamic_key_breaks_vertically() {
        // #2528 hypothesis: when the deepest value is `Value::Concrete(ConcreteValue::StringList)`
        // (the canonical form #2511 folds `Union[String, list(String)]`
        // into) rather than `Value::Concrete(ConcreteValue::List)`, `format_value_pretty` treats
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
        // appears as `<key>: [\n   "...",\n   "..."\n  ]` — one element
        // per line with a trailing comma. The inline form is the
        // single-line `<key>: ["...", "..."]` with no `\n` mid-list.
        let out = format_value_pretty(&v, layout(12, "string_like"));
        assert!(
            out.contains(":sub: [\n"),
            "expected dynamic-key StringList to start its bracketed form, got:\n{out}"
        );
        assert!(
            out.contains("refs/heads/main\",\n"),
            "expected first element on its own line with trailing comma, got:\n{out}"
        );
    }

    #[test]
    fn format_value_pretty_top_level_string_list_breaks_vertically_when_oversize() {
        // The same fix applies when a top-level attribute value is
        // already canonicalized to `Value::Concrete(ConcreteValue::StringList)`. Pre-fix the
        // wildcard arm collapsed it to `["a", "b", ...]` even when the
        // line exceeded `PRETTY_LINE_LIMIT`.
        let v = Value::Concrete(ConcreteValue::StringList(vec![
            "a".repeat(40).into(),
            "b".repeat(40).into(),
        ]));
        let out = format_value_pretty(&v, layout(0, "k"));
        assert!(
            out.starts_with("[\n"),
            "expected oversize StringList to expand vertically, got:\n{out}"
        );
    }

    #[test]
    fn format_value_pretty_string_list_does_not_reclassify_marker_shaped_text() {
        // Only the persisted-state decoder assigns marker semantics. A plain
        // string that happens to use the reserved prefix stays a string.
        let marker_shaped_text = format!("{}{}", SECRET_PREFIX, "deadbeef".repeat(8));
        let v = Value::Concrete(ConcreteValue::StringList(vec![
            marker_shaped_text.clone().into(),
            "x".repeat(80).into(), // force vertical
        ]));
        let out = format_value_pretty(&v, layout(0, "k"));
        assert!(
            out.contains(&format!("\"{marker_shaped_text}\",")),
            "ordinary marker-shaped text must remain visible as a string, got:\n{out}"
        );
    }

    #[test]
    fn format_value_pretty_nested_map_with_dynamic_key_list_value_breaks_vertically() {
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
        let out = format_value_pretty(&v, layout(4, "statement"));
        // The dynamic-key list value must break across lines (bracketed
        // form), not collapse to `<dynamic-key>: ["a", "b"]`.
        assert!(
            out.contains(":sub: ["),
            "expected dynamic-key list to start its bracket form, got:\n{out}"
        );
        assert!(
            out.contains("\"repo:carina-rs/infra:ref:refs/heads/main\","),
            "expected first list element on its own line with trailing comma, got:\n{out}"
        );
    }

    #[test]
    fn format_value_pretty_empty_map_inline() {
        let v = Value::Concrete(ConcreteValue::Map(IndexMap::new()));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), "{}");
    }

    #[test]
    fn format_value_pretty_small_map_inline_fits() {
        let mut m = IndexMap::new();
        m.insert("a".to_string(), Value::Concrete(ConcreteValue::Int(1)));
        m.insert("b".to_string(), Value::Concrete(ConcreteValue::Int(2)));
        let v = Value::Concrete(ConcreteValue::Map(m));
        // {a: 1, b: 2} = 12 chars; total = 0 + 1 + 2 + 12 = 15 → inline.
        assert_eq!(format_value_pretty(&v, layout(0, "k")), "{a: 1, b: 2}");
    }

    #[test]
    fn format_value_pretty_top_level_map_expands_when_over_threshold() {
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
        let out = format_value_pretty(&v, layout(0, "cfg"));
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
    fn format_value_pretty_scalar_variants_match_format_value_with_key() {
        // Lock the "scalar fallthrough" contract.
        use crate::resource::AccessPath;

        let path = AccessPath::new("vpc", "id");
        let cases = vec![
            Value::Concrete(ConcreteValue::String("hello".into())),
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::Float(2.5)),
            Value::Concrete(ConcreteValue::Bool(false)),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("pw".into()),
            )))),
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey)),
            Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
                path: path.clone(),
            })),
            Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("prefix-".into()),
                InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef { path })),
            ])),
            Value::Deferred(DeferredValue::FunctionCall {
                name: "concat".to_string(),
                args: vec![
                    Value::Concrete(ConcreteValue::String("a".into())),
                    Value::Concrete(ConcreteValue::String("b".into())),
                ],
            }),
        ];

        for v in &cases {
            assert_eq!(
                format_value_pretty(v, layout(0, "k")),
                format_value_with_key(v, None),
                "scalar fallthrough drift for variant: {v:?}"
            );
        }
    }

    // ---- canonicalize_with_type tests (#2481, #2510) ----

    fn string_or_list_of_strings() -> AttributeType {
        AttributeType::union(vec![
            AttributeType::string(),
            AttributeType::list(AttributeType::string()),
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
            vec![crate::schema::StructField::new(
                "action",
                string_or_list_of_strings(),
            )],
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
                crate::schema::StructField::new("action", string_or_list_of_strings()),
                crate::schema::StructField::new("resource", string_or_list_of_strings()),
            ],
        );
        let policy_document = AttributeType::struct_(
            "PolicyDocument".to_string(),
            vec![crate::schema::StructField::new(
                "statement",
                AttributeType::list(statement),
            )],
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

        let canon = schema.canonicalize_state(Value::Concrete(ConcreteValue::Map(policy)));
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
    fn canonicalize_recurses_into_struct_via_block_name() {
        let t = AttributeType::struct_(
            "Statement".to_string(),
            vec![
                crate::schema::StructField::new("actions", string_or_list_of_strings())
                    .with_block_name("action"),
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
                crate::schema::StructField::new("action", string_or_list_of_strings())
                    .with_provider_name("Action"),
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
                vec![crate::schema::StructField::new(
                    "service",
                    string_or_list_of_strings(),
                )],
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
                vec![crate::schema::StructField::new(
                    "service",
                    string_or_list_of_strings(),
                )],
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
                vec![crate::schema::StructField::new(
                    "service",
                    string_or_list_of_strings(),
                )],
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
        let canon = schema.canonicalize_state(value);
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
        let canon = schema.canonicalize_state(value);
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

        assert_eq!(schema.canonicalize_state(value.clone()), value);
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
            schema.canonicalize_state(value),
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
    /// width of must report the same byte length as `format_value_with_key`
    /// would emit. Anything else is a drift bug — the optimization
    /// silently changes the inline-vs-vertical decision boundary.
    fn assert_inline_width_matches_build(v: &Value) {
        // `format_value_pretty`'s overflow check uses byte length
        // (`inline.len()`), so `inline_width` must report bytes too.
        let built = format_value_with_key(v, None);
        let measured = inline_width(v, usize::MAX);
        assert_eq!(
            measured,
            Some(built.len()),
            "inline_width vs format_value_with_key drift for {v:?}: built={built:?}",
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
        assert_eq!(format_value(&v), "[\"a\", \"b\"]");
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
        use crate::schema::{AttributeSchema, ResourceSchema, SchemaRegistry};
        let mut reg = SchemaRegistry::new();
        let schema = ResourceSchema::new("iam.policy")
            .attribute(AttributeSchema::new("subject", string_or_list_of_strings()));
        reg.insert("aws", schema);
        reg
    }

    fn make_resource(attrs: Vec<(&str, Value)>) -> crate::resource::Resource {
        use crate::resource::{Resource, ResourceId};
        use std::collections::{BTreeSet, HashMap, HashSet};
        let mut attributes = IndexMap::new();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v);
        }
        Resource {
            id: ResourceId::with_provider_identity("aws", "iam.policy", "p1", None),
            attributes,
            directives: Default::default(),
            prefixes: HashMap::new(),
            binding: Some("p1".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            default_tag_keys: Default::default(),
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
        use crate::resource::DataSource;
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, enum_identity,
        };

        let mode = AttributeType::enum_(
            enum_identity("Mode", Some("awscc.example.Widget")),
            Some(vec!["API_A".to_string(), "FOO".to_string()]),
            vec![("API_A".to_string(), "foo".to_string())],
            None,
            None,
        );
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "awscc",
            ResourceSchema::new("example.Widget")
                .as_data_source()
                .attribute(AttributeSchema::new("subject", string_or_list_of_strings()))
                .attribute(AttributeSchema::new("mode", mode)),
        );
        let mut data_source = DataSource::with_provider("awscc", "example.Widget", "widget", None);
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
        use std::collections::{BTreeSet, HashMap};
        let mut attributes = HashMap::new();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v);
        }
        State {
            id: ResourceId::with_provider_identity("aws", "iam.policy", "p1", None),
            identifier: Some("arn:aws:iam::123:policy/p1".to_string()),
            attributes,
            exists: true,
            dependency_bindings: BTreeSet::new(),
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
        states.insert(s.id.clone(), s);
        canonicalize_states_with_schemas(&mut states, &registry);
        let state = states.values().next().unwrap();
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
        states.insert(s.id.clone(), s);
        canonicalize_states_with_schemas(&mut states, &registry);
        let state = states.values().next().unwrap();
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
        states.insert(state.id.clone(), state);

        canonicalize_states_with_schemas(&mut states, &registry);

        let state = states.values().next().unwrap();
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
        states.insert(s.id.clone(), s);
        canonicalize_states_with_schemas(&mut states, &registry);
        let state = states.values().next().unwrap();
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
        states.insert(s.id.clone(), s);
        canonicalize_states_with_schemas(&mut states, &registry);

        let state = states.values().next().unwrap();
        assert_eq!(
            resources[0].attributes.get("subject"),
            state.attributes.get("subject"),
        );
    }

    #[test]
    fn canonicalize_pipeline_dynamic_az_enum_state_api_spelling_has_no_diff() {
        use crate::differ::{Diff, diff_test};
        use crate::resource::{Resource, ResourceId, State};
        use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
        use std::collections::HashMap;

        let az_enum = AttributeType::enum_(
            crate::schema::enum_identity("ZoneName", Some("aws.AvailabilityZone")),
            None,
            vec![],
            None,
            Some(crate::schema::DslTransform::HyphenToUnderscore),
        );
        let schema = ResourceSchema::new("ec2.Subnet")
            .attribute(AttributeSchema::new("availability_zone", az_enum));
        let mut registry = SchemaRegistry::new();
        registry.insert("awscc", schema.clone());

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
        states.insert(id.clone(), State::existing(id, attrs));
        canonicalize_states_with_schemas(&mut states, &registry);

        let result = diff_test(
            &desired[0],
            states.values().next().expect("state exists"),
            None,
            None,
            Some(&schema),
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
        use crate::schema::{
            AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
        };
        let principal = AttributeType::union(vec![
            AttributeType::struct_(
                "PrincipalStruct".to_string(),
                vec![StructField::new("service", string_or_list_of_strings())],
            ),
            AttributeType::string(),
        ]);
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "aws",
            ResourceSchema::new("iam.policy")
                .attribute(AttributeSchema::new("principal", principal)),
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
        states.insert(s.id.clone(), s);
        canonicalize_states_with_schemas(&mut states, &registry);

        let state = states.values().next().unwrap();
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
        use crate::schema::AttributeType;
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
        let v = Value::Concrete(ConcreteValue::StringList(vec!["a".into(), "b".into()]));
        let formatted = format_value(&v);
        assert_eq!(formatted, "[\"a\", \"b\"]");
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
    fn format_value_pretty_decision_unchanged_after_inline_width() {
        // End-to-end: the inline-vs-vertical decision in
        // `format_value_pretty` must be byte-identical to the pre-fix
        // build-then-measure shape. Construct values that straddle
        // PRETTY_LINE_LIMIT (80) at the prefix boundary and confirm both
        // sides of the boundary still pick the same form.
        // Just under the limit (inline expected).
        let small_list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("aaaaaaaaaaaa".into())),
            Value::Concrete(ConcreteValue::String("bbbbbbbbbbbb".into())),
        ]));
        let small = format_value_pretty(&small_list, layout(0, "k"));
        assert!(
            !small.contains('\n'),
            "small list must render inline, got: {small:?}"
        );

        // Over the limit (vertical expected).
        let big_list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".repeat(40).into())),
            Value::Concrete(ConcreteValue::String("b".repeat(40).into())),
            Value::Concrete(ConcreteValue::String("c".repeat(40).into())),
        ]));
        let big = format_value_pretty(&big_list, layout(0, "k"));
        assert!(
            big.contains('\n'),
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
            AccessPath, DeferredValue, InterpolationPart, ResourceId, UnknownReason, Value,
        };
        let path =
            AccessPath::with_fields("management_route53", "apex_zone_id", Vec::<String>::new());
        let identifier = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                UnknownReason::UpstreamExport { path },
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
        use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart, ResourceId, Value};
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
                    use crate::resource::{DeferredValue, InterpolationPart, Value};
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
        use crate::resource::{
            AccessPath, DeferredValue, Resource, ResourceId, UnknownReason, Value,
        };

        let placeholder = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValuePath {
            path: AccessPath::with_fields("opt", "resource_record", vec!["name".to_string()]),
        }));
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("DEFERRED_TEMPLATE_PLAINTEXT".into()),
        ))));
        let mut template_resource = Resource::new("aws.route53.RecordSet", "validation_records");
        template_resource.set_attr("name", placeholder.clone());
        template_resource.set_attr("comment", secret.clone());
        let effect = Effect::DeferredCreate {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "validation_records",
            )),
            upstream_binding: "cert".to_string(),
            template: Box::new(DeferredForExpression {
                file: None,
                line: 1,
                header: "for opt in cert.domain_validation_options".to_string(),
                resource_type: "aws.route53.RecordSet".to_string(),
                attributes: vec![
                    ("name".to_string(), placeholder.clone()),
                    ("comment".to_string(), secret),
                ],
                binding_name: "validation_records".to_string(),
                iterable_binding: "cert".to_string(),
                iterable_attr: "domain_validation_options".to_string(),
                binding: ForBinding::Simple("opt".to_string()),
                template_resource,
            }),
        };

        let redacted = redact_secrets_in_effect(&effect).expect("redact deferred secret");
        let Effect::DeferredCreate {
            template: redacted_template,
            ..
        } = &redacted
        else {
            panic!("expected deferred create");
        };
        assert!(matches!(
            redacted_template
                .attributes
                .iter()
                .find_map(|(key, value)| (key == "comment").then_some(value)),
            Some(Value::Deferred(DeferredValue::RedactedSecret { .. }))
        ));
        assert!(matches!(
            redacted_template
                .template_resource
                .attributes
                .get("comment"),
            Some(Value::Deferred(DeferredValue::RedactedSecret { .. }))
        ));

        let mut plan = Plan::new();
        plan.add(effect.clone());
        let redacted_plan = redact_secrets_in_plan(&plan).expect("redact deferred plan");
        let Effect::DeferredCreate { template, .. } = &effect else {
            unreachable!();
        };
        let rehydrated = rehydrate_plan_desired_secrets(
            &redacted_plan,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            std::slice::from_ref(template.as_ref()),
        )
        .expect("unchanged deferred template secret should rehydrate");
        let Effect::DeferredCreate { template, .. } = &rehydrated.effects()[0] else {
            panic!("expected rehydrated deferred create");
        };
        assert!(matches!(
            template
                .attributes
                .iter()
                .find_map(|(key, value)| (key == "comment").then_some(value)),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));
        assert!(matches!(
            template.template_resource.attributes.get("comment"),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));
        assert!(
            !template
                .attributes
                .iter()
                .any(|(_, value)| contains_redacted_secret(value))
        );
    }

    #[test]
    fn deferred_template_redaction_uses_template_resource_as_single_projection() {
        use crate::effect::Effect;
        use crate::parser::{DeferredForExpression, ForBinding};
        use crate::resource::{DeferredValue, Resource, ResourceId};

        let secret = |plaintext: &str| {
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String(plaintext.into()),
            ))))
        };
        let mut template_resource = Resource::new("aws.route53.RecordSet", "records");
        template_resource.set_attr("comment", secret("authoritative"));
        let effect = Effect::DeferredCreate {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "records",
            )),
            upstream_binding: "cert".to_string(),
            template: Box::new(DeferredForExpression {
                file: None,
                line: 1,
                header: "for opt in cert.options".to_string(),
                resource_type: "aws.route53.RecordSet".to_string(),
                attributes: vec![("comment".to_string(), secret("stale-projection"))],
                binding_name: "records".to_string(),
                iterable_binding: "cert".to_string(),
                iterable_attr: "options".to_string(),
                binding: ForBinding::Simple("opt".to_string()),
                template_resource,
            }),
        };

        let redacted = redact_secrets_in_effect(&effect).expect("redact deferred template");
        let Effect::DeferredCreate { template, .. } = redacted else {
            panic!("expected deferred create");
        };
        let projected = template
            .attributes
            .iter()
            .find_map(|(key, value)| (key == "comment").then_some(value))
            .expect("projected comment");
        let canonical = template
            .template_resource
            .attributes
            .get("comment")
            .expect("template resource comment");

        let Value::Deferred(DeferredValue::RedactedSecret {
            hash: projected_hash,
        }) = projected
        else {
            panic!("expected projected placeholder, got {projected:?}");
        };
        let Value::Deferred(DeferredValue::RedactedSecret {
            hash: canonical_hash,
        }) = canonical
        else {
            panic!("expected canonical placeholder, got {canonical:?}");
        };
        assert_eq!(
            projected_hash, canonical_hash,
            "the compatibility projection must mirror the canonical template resource"
        );
    }

    #[test]
    fn deferred_template_preserves_display_only_projection_and_noops_without_placeholders() {
        use crate::effect::Effect;
        use crate::parser::{DeferredForExpression, ForBinding};
        use crate::resource::{Resource, ResourceId};

        let canonical = Value::Concrete(ConcreteValue::String("canonical".into()));
        let display_only = Value::Concrete(ConcreteValue::String("display-only".into()));
        let mut template_resource = Resource::new("mock.test.resource", "children");
        template_resource.set_attr("comment", canonical.clone());
        let template = DeferredForExpression {
            file: None,
            line: 1,
            header: "for child in parent.children".to_string(),
            resource_type: "mock.test.resource".to_string(),
            attributes: vec![
                ("comment".to_string(), canonical),
                ("identifier".to_string(), display_only.clone()),
            ],
            binding_name: "children".to_string(),
            iterable_binding: "parent".to_string(),
            iterable_attr: "children".to_string(),
            binding: ForBinding::Simple("child".to_string()),
            template_resource,
        };
        let effect = Effect::DeferredCreate {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "children",
            )),
            upstream_binding: "parent".to_string(),
            template: Box::new(template.clone()),
        };

        let redacted = redact_secrets_in_effect(&effect).expect("redact placeholder-free template");
        let Effect::DeferredCreate {
            template: redacted_template,
            ..
        } = redacted
        else {
            panic!("expected deferred create")
        };
        assert_eq!(
            redacted_template.attributes, template.attributes,
            "redaction must retain display-only projection keys",
        );

        let mut plan = crate::plan::Plan::new();
        plan.add(Effect::DeferredCreate {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "children",
            )),
            upstream_binding: "parent".to_string(),
            template: redacted_template,
        });
        let rehydrated = rehydrate_plan_desired_secrets(
            &plan,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &[],
        )
        .expect("placeholder-free template must not need a candidate");
        let Effect::DeferredCreate {
            template: output, ..
        } = &rehydrated.effects()[0]
        else {
            panic!("expected deferred create")
        };
        assert_eq!(output.as_ref(), &template);
        assert_eq!(
            output
                .attributes
                .iter()
                .find_map(|(key, value)| (key == "identifier").then_some(value)),
            Some(&display_only),
        );
    }

    fn replacement_plan_with_previous_attributes(
        previous_attributes: std::collections::HashMap<String, Value>,
        permanent_name_override: Option<crate::plan::PermanentNameOverride>,
    ) -> crate::plan::Plan {
        use crate::effect::ChangedCreateOnly;
        use crate::plan::{ReplacementDelete, ReplacementGroup};
        use crate::resource::{
            Directives, ResolvedResource, ResolvedResourceId, Resource, ResourceId,
        };
        use std::collections::HashSet;

        let id = ResourceId::with_identity("aws.s3.Bucket", "bucket");
        let mut create = Resource::new("aws.s3.Bucket", "bucket");
        create.binding = Some("bucket".to_string());

        let mut plan = crate::plan::Plan::new();
        plan.add_replacement(ReplacementGroup {
            create: ResolvedResource::new(create),
            delete: ReplacementDelete {
                id: ResolvedResourceId::new(id.clone()),
                identifier: "bucket-old".to_string(),
                directives: Directives::default(),
                binding: Some("bucket".to_string()),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
            },
            create_before_destroy: true,
            changed_create_only: ChangedCreateOnly::new(vec!["bucket_name".to_string()])
                .expect("test replacement has a create-only attribute"),
            cascade_ref_hints: vec![("vpc_id".to_string(), "vpc.id".to_string())],
            temporary_name: None,
            permanent_name_override,
            consumer_updates: HashSet::new(),
            previous_attributes,
        });
        plan
    }

    #[test]
    fn redact_secrets_in_plan_preserves_replace_display() {
        use crate::resource::{ConcreteValue, Value};
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
        use crate::resource::{ConcreteValue, DeferredValue, Value};
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
