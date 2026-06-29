//! Value conversion and formatting utilities

use std::collections::HashMap;

use argon2::Argon2;
use indexmap::IndexMap;
use thiserror::Error;

use crate::resource::{
    CanonicalEnumValue, ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
};
use crate::schema::{AttrTypeKind, AttributeType, TypeIdentity};
use crate::utils::{enum_display_value, extract_enum_value_with_values};

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
    /// Backend lock JSON.
    BackendLock,
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
            Self::BackendLock => write!(f, "backend lock"),
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
    /// `PlanPreprocessor::strip_unknown_attributes` for the WASM
    /// boundary stripping pass.
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
    /// structured `AccessPath` (cf. `UnknownReason::UpstreamRef`)
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
}

impl std::fmt::Display for UnknownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnknownReason::UpstreamRef { path } => {
                write!(f, "upstream value {}", path.to_dot_string())
            }
            UnknownReason::UpstreamBareRef { binding } => {
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
        UnknownReason::UpstreamRef { path } => {
            format!("(known after upstream apply: {})", path.to_dot_string())
        }
        UnknownReason::UpstreamBareRef { binding } => {
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
pub const SECRET_PREFIX: &str = "_secret:argon2:";

/// Fallback salt for Argon2id hashing when no context is available.
const ARGON2_FALLBACK_SALT: &[u8] = b"carina-secret-v1";

/// Context for deterministic salt generation when hashing secrets.
///
/// The salt is derived from the resource context to ensure that the same
/// password on different resources produces different hashes.
#[derive(Debug, Clone)]
pub struct SecretHashContext {
    pub resource_type: String,
    pub resource_name: String,
    pub attribute_key: String,
}

impl SecretHashContext {
    pub fn new(
        resource_type: impl Into<String>,
        resource_name: impl Into<String>,
        attribute_key: impl Into<String>,
    ) -> Self {
        Self {
            resource_type: resource_type.into(),
            resource_name: resource_name.into(),
            attribute_key: attribute_key.into(),
        }
    }

    /// Build a deterministic salt from the context.
    fn salt(&self) -> String {
        format!(
            "carina:{}:{}:{}",
            self.resource_type, self.resource_name, self.attribute_key
        )
    }
}

/// Hash bytes using Argon2id, returning a hex string.
///
/// When `context` is provided, a deterministic salt derived from the resource
/// context is used. Otherwise, a fixed fallback salt is used.
pub(crate) fn argon2id_hash(input: &[u8], context: Option<&SecretHashContext>) -> String {
    let salt_string;
    let salt: &[u8] = match context {
        Some(ctx) => {
            salt_string = ctx.salt();
            salt_string.as_bytes()
        }
        None => ARGON2_FALLBACK_SALT,
    };
    let mut output = [0u8; 32];
    Argon2::default()
        .hash_password_into(input, salt, &mut output)
        .expect("Argon2id hashing should not fail");
    output.iter().map(|b| format!("{b:02x}")).collect()
}

/// Convert `Value` to `serde_json::Value`.
///
/// Returns an error if `value` contains a non-finite float (NaN or infinity)
/// because JSON cannot represent these values.
///
/// For `Value::Deferred(DeferredValue::Secret)`, uses the fallback salt. Use `value_to_json_with_context`
/// to provide resource context for deterministic context-specific salt.
pub fn value_to_json(value: &Value) -> Result<serde_json::Value, SerializationError> {
    value_to_json_with_context(value, None)
}

/// Convert `Value` to `serde_json::Value` with optional secret hash context.
///
/// When `context` is provided and the value contains `Value::Deferred(DeferredValue::Secret)`, the hash
/// uses a deterministic salt derived from the resource context. This ensures
/// that the same password on different resources produces different hashes.
pub fn value_to_json_with_context(
    value: &Value,
    context: Option<&SecretHashContext>,
) -> Result<serde_json::Value, SerializationError> {
    let ctx = SerializationContext::ValueToJson;
    match value {
        Value::Concrete(ConcreteValue::String(s)) => {
            // Both serialize as a flat JSON string. The schema-aware
            // state loader re-classifies `EnumIdentifier` from the
            // attribute's declared type, so the on-disk JSON stays
            // unchanged. See carina#2986 design doc §5.
            Ok(serde_json::Value::String(s.clone()))
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
                .map(|item| value_to_json_with_context(item, context))
                .collect();
            Ok(serde_json::Value::Array(arr?))
        }
        Value::Concrete(ConcreteValue::StringList(items)) => Ok(serde_json::Value::Array(
            items
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => {
            let obj: Result<serde_json::Map<_, _>, _> = map
                .iter()
                .map(|(k, v)| value_to_json_with_context(v, context).map(|jv| (k.clone(), jv)))
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
            let inner_json = value_to_json_with_context(inner, context)?;
            // `serde_json::Value -> String` only fails on a custom
            // `Serialize` impl or invalid map keys, neither of which a
            // freshly-built `serde_json::Value` can produce.
            let json_str = serde_json::to_string(&inner_json)
                .expect("serde_json::Value -> String is infallible");
            let hash_hex = argon2id_hash(json_str.as_bytes(), context);
            Ok(serde_json::Value::String(format!(
                "{SECRET_PREFIX}{hash_hex}",
            )))
        }
        Value::Deferred(DeferredValue::Unknown(reason)) => {
            Err(SerializationError::UnknownNotAllowed {
                reason: reason.clone(),
                context: ctx,
            })
        }
    }
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
    match json {
        serde_json::Value::String(s) => Some(Value::Concrete(ConcreteValue::String(s.clone()))),
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
            items.iter().filter_map(json_to_dsl_value).collect(),
        ))),
        serde_json::Value::Object(map) => {
            if let Some(canonical) = json_to_canonical_enum(map) {
                return Some(Value::Concrete(ConcreteValue::CanonicalEnum(canonical)));
            }
            let m: IndexMap<_, _> = map
                .iter()
                .filter_map(|(k, v)| json_to_dsl_value(v).map(|val| (k.clone(), val)))
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
            // Secret hash strings should display as "(secret)" to avoid
            // leaking internal hash representation in plan output.
            if s.starts_with(SECRET_PREFIX) {
                return sink.write_str("(secret)");
            }
            // Namespaced enum text: shorten for display before quoting.
            if let Some(resolved) = enum_display_value(s) {
                sink.write_str("\"")?;
                sink.write_str(resolved)?;
                return sink.write_str("\"");
            }
            sink.write_str("\"")?;
            sink.write_str(s)?;
            sink.write_str("\"")
        }
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => {
            // Enum identifiers render unquoted in plan/diff output to
            // match how the user typed them. The resolver has already
            // canonicalized any namespaced form, so `s` is the bare
            // identifier ready for direct display.
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
                sink.write_str(item)?;
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
                    InterpolationPart::Literal(s) => sink.write_str(s)?,
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
        Value::Deferred(DeferredValue::Secret(_)) => sink.write_str("(secret)"),
        Value::Deferred(DeferredValue::Unknown(reason)) => sink.write_str(&render_unknown(reason)),
    }
}

/// Check if a Value contains any Secret values at any nesting depth.
pub fn contains_secret(value: &Value) -> bool {
    match value {
        Value::Deferred(DeferredValue::Secret(_)) => true,
        Value::Concrete(ConcreteValue::Map(map)) => map.values().any(contains_secret),
        Value::Concrete(ConcreteValue::List(items)) => items.iter().any(contains_secret),
        _ => false,
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
///
/// When `context` is provided, it is passed through to `value_to_json_with_context`
/// for deterministic context-specific salt in Argon2id hashing.
pub fn merge_secrets_into_provider_json(
    desired: &Value,
    provider_json: &serde_json::Value,
    context: Option<&SecretHashContext>,
) -> Result<serde_json::Value, SerializationError> {
    match desired {
        Value::Deferred(DeferredValue::Secret(_)) => value_to_json_with_context(desired, context),
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
                                    context,
                                )?,
                            );
                        } else {
                            // Key only in desired (not returned by provider); use desired hash
                            merged.insert(
                                k.clone(),
                                value_to_json_with_context(desired_val, context)?,
                            );
                        }
                    }
                }
                Ok(serde_json::Value::Object(merged))
            } else {
                // Provider didn't return a map; fall back to desired
                value_to_json_with_context(desired, context)
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
                                context,
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
                value_to_json_with_context(desired, context)
            }
        }
        _ => Ok(provider_json.clone()),
    }
}

/// Recursively replace all `Value::Deferred(DeferredValue::Secret(inner))` with `Value::Concrete(ConcreteValue::String(hash))`.
///
/// This ensures that when a `Value` tree is serialized (e.g., via serde), no
/// secret plaintext is ever written. The hash uses Argon2id with the fallback
/// salt (not context-aware). This is suitable for plan file serialization where
/// the goal is redaction, not state comparison.
/// Secret-only redactor used inside deferred wrappers (`Interpolation`,
/// `FunctionCall`) where a `Value::Deferred(Unknown(UpstreamRef))` is a
/// load-bearing plan-time placeholder, not an error.
///
/// Hashes every `Secret(_)` leaf — matching `redact_secrets_in_value`
/// for that case — but, unlike the top-level redactor, passes
/// `Unknown` and other deferred shapes through verbatim. This is the
/// Unknown-tolerant counterpart introduced by carina#3329 so that
/// `plan --out` on an `import { id = "${upstream.attr}|tail" }`
/// (where the upstream is not yet applied) does not fail with
/// `UnknownNotAllowed`. The apply-side enforcement still happens:
/// `resolve_import_identifier` rejects any non-concrete identifier
/// before the provider is called.
fn redact_secrets_only(value: &Value) -> Result<Value, SerializationError> {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => {
            let inner_json = value_to_json(inner)?;
            let json_str = serde_json::to_string(&inner_json)
                .expect("serde_json::Value -> String is infallible");
            let hash_hex = argon2id_hash(json_str.as_bytes(), None);
            Ok(Value::Concrete(ConcreteValue::String(format!(
                "{SECRET_PREFIX}{hash_hex}"
            ))))
        }
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

pub fn redact_secrets_in_value(value: &Value) -> Result<Value, SerializationError> {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => {
            let inner_json = value_to_json(inner)?;
            let json_str = serde_json::to_string(&inner_json)
                .expect("serde_json::Value -> String is infallible");
            let hash_hex = argon2id_hash(json_str.as_bytes(), None);
            Ok(Value::Concrete(ConcreteValue::String(format!(
                "{SECRET_PREFIX}{hash_hex}"
            ))))
        }
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
        // `Unknown(UpstreamRef)` produced by plan-time stamping (RFC
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

/// Redact all secrets in an attributes map.
pub fn redact_secrets_in_attributes(
    attrs: &HashMap<String, Value>,
) -> Result<HashMap<String, Value>, SerializationError> {
    attrs
        .iter()
        .map(|(k, v)| redact_secrets_in_value(v).map(|rv| (k.clone(), rv)))
        .collect()
}

/// Redact all secrets in a `Resource`, returning a new Resource with secrets replaced by hashes.
pub fn redact_secrets_in_resource(
    resource: &crate::resource::Resource,
) -> Result<crate::resource::Resource, SerializationError> {
    let attributes: Result<_, _> = resource
        .attributes
        .iter()
        .map(|(k, e)| redact_secrets_in_value(e).map(|rv| (k.clone(), rv)))
        .collect();
    Ok(crate::resource::Resource {
        attributes: attributes?,
        ..resource.clone()
    })
}

/// Redact all secrets in a [`Resource`](crate::resource::Resource).
///
/// carina#3181 PR D: `Effect` payloads are typestate structs, so the
/// redaction pass needs a typed entry point per arm.
pub fn redact_secrets_in_managed(
    resource: &crate::resource::Resource,
) -> Result<crate::resource::Resource, SerializationError> {
    let attributes: Result<_, _> = resource
        .attributes
        .iter()
        .map(|(k, e)| redact_secrets_in_value(e).map(|rv| (k.clone(), rv)))
        .collect();
    Ok(crate::resource::Resource {
        attributes: attributes?,
        ..resource.clone()
    })
}

/// Redact secrets in a managed resource while preserving deferred placeholders.
///
/// Deferred-for templates intentionally contain `Unknown(ForValue*)`
/// placeholders until apply-time expansion substitutes the iterable
/// values. They still need secret redaction for saved plans, but they
/// must not use the strict provider-bound redactor.
fn redact_secrets_in_managed_only(
    resource: &crate::resource::Resource,
) -> Result<crate::resource::Resource, SerializationError> {
    let attributes: Result<_, _> = resource
        .attributes
        .iter()
        .map(|(k, e)| redact_secrets_only(e).map(|rv| (k.clone(), rv)))
        .collect();
    Ok(crate::resource::Resource {
        attributes: attributes?,
        ..resource.clone()
    })
}

/// Redact all secrets in a [`DataSource`](crate::resource::DataSource).
pub fn redact_secrets_in_data_source(
    resource: &crate::resource::DataSource,
) -> Result<crate::resource::DataSource, SerializationError> {
    let attributes: Result<_, _> = resource
        .attributes
        .iter()
        .map(|(k, e)| redact_secrets_in_value(e).map(|rv| (k.clone(), rv)))
        .collect();
    Ok(crate::resource::DataSource {
        attributes: attributes?,
        ..resource.clone()
    })
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
    // Reify each `CompositionAttribute` to a `Value`, redact secrets,
    // then re-classify with `CompositionAttribute::from_value` so
    // single-hop alias structure is preserved across the round-trip.
    let attributes: Result<indexmap::IndexMap<String, crate::resource::CompositionAttribute>, _> =
        resource
            .signature
            .attributes
            .iter()
            .map(|(k, attr)| {
                redact_secrets_in_value(&attr.to_value()).map(|rv| {
                    (
                        k.clone(),
                        crate::resource::CompositionAttribute::from_value(rv),
                    )
                })
            })
            .collect();
    let mut out = resource.clone();
    out.signature.attributes = attributes?;
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

/// Redact all secrets in an `Effect`, returning a new Effect with secrets replaced by hashes.
pub fn redact_secrets_in_effect(
    effect: &crate::effect::Effect,
) -> Result<crate::effect::Effect, SerializationError> {
    use crate::effect::Effect;
    Ok(match effect {
        Effect::Read { resource } => Effect::Read {
            resource: crate::resource::ResolvedDataSource::new(redact_secrets_in_data_source(
                resource,
            )?),
        },
        Effect::Create(resource) => Effect::Create(crate::resource::ResolvedResource::new(
            redact_secrets_in_managed(resource)?,
        )),
        Effect::Update {
            from,
            to,
            changed_attributes,
        } => Effect::Update {
            from: Box::new(redact_secrets_in_state(from)?),
            to: crate::resource::ResolvedResource::new(redact_secrets_in_managed(to)?),
            changed_attributes: changed_attributes.clone(),
        },
        Effect::Delete {
            id,
            identifier,
            directives,
            binding,
            dependencies,
            explicit_dependencies,
            blocked_by_updates,
        } => Effect::Delete {
            id: id.clone(),
            identifier: identifier.clone(),
            directives: directives.clone(),
            binding: binding.clone(),
            dependencies: dependencies.clone(),
            explicit_dependencies: explicit_dependencies.clone(),
            blocked_by_updates: blocked_by_updates.clone(),
        },
        Effect::Import { id, identifier } => Effect::Import {
            id: id.clone(),
            // carina#3329: `identifier` is a `Value` (not the legacy
            // `String`), so a `"${secret_let.value}|tail"` interpolation
            // can carry a `DeferredValue::Secret(...)` segment under an
            // `Expr` part. Route through `redact_secrets_in_value` so
            // the saved-plan / persisted form replaces the secret with
            // its hash; without this, the plain `clone()` would persist
            // the in-memory plaintext.
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
        } => {
            let mut redacted_template = (**template).clone();
            redacted_template.attributes = redacted_template
                .attributes
                .iter()
                .map(|(key, value)| Ok((key.clone(), redact_secrets_only(value)?)))
                .collect::<Result<Vec<_>, SerializationError>>()?;
            redacted_template.template_resource =
                redact_secrets_in_managed_only(&redacted_template.template_resource)?;
            Effect::DeferredCreate {
                id: id.clone(),
                upstream_binding: upstream_binding.clone(),
                template: Box::new(redacted_template),
            }
        }
        Effect::DeferredReplace {
            deletes,
            id,
            upstream_binding,
            template,
        } => {
            let mut redacted_template = (**template).clone();
            redacted_template.attributes = redacted_template
                .attributes
                .iter()
                .map(|(key, value)| Ok((key.clone(), redact_secrets_only(value)?)))
                .collect::<Result<Vec<_>, SerializationError>>()?;
            redacted_template.template_resource =
                redact_secrets_in_managed_only(&redacted_template.template_resource)?;
            Effect::DeferredReplace {
                deletes: deletes.clone(),
                id: id.clone(),
                upstream_binding: upstream_binding.clone(),
                template: Box::new(redacted_template),
            }
        }
    })
}

/// Redact all secrets in a `Plan`, returning a new Plan with secrets replaced by hashes.
pub fn redact_secrets_in_plan(
    plan: &crate::plan::Plan,
) -> Result<crate::plan::Plan, SerializationError> {
    let mut redacted = plan.clone();
    let redacted_effects: Vec<_> = plan
        .effects()
        .iter()
        .map(redact_secrets_in_effect)
        .collect::<Result<_, _>>()?;
    *redacted.effects_mut() = redacted_effects;

    for metadata in redacted.replace_display_mut() {
        for value in metadata.previous_attributes.values_mut() {
            *value = redact_secrets_in_value(value)?;
        }
    }

    Ok(redacted)
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
            // rendering (SECRET_PREFIX redaction, DSL-enum resolution)
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
/// `EnumIdentifier` is accepted (carina#3075): a `List<Enum>`
/// reaches the renderer with `EnumIdentifier` elements on both sides
/// (state lifted by `lift_saved_state_enums`, desired emitted
/// by the parser per carina#2986). Treating it as its string payload —
/// the same `String`/`EnumIdentifier` interchangeability the differ's
/// `Enum` arm already relies on — lets the string-list diff path
/// (and its schema-aware enum canonicalization) engage instead of the
/// value falling to a coarse inline `Changed` row.
pub fn as_string_list(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::Concrete(ConcreteValue::StringList(items)) => Some(items.clone()),
        Value::Concrete(ConcreteValue::List(items)) => items
            .iter()
            .map(|v| match v {
                Value::Concrete(ConcreteValue::String(s)) => Some(s.clone()),
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

/// Count the number of shared key-value pairs between two map Values.
/// Uses semantically_equal for value comparison so nested lists are order-insensitive.
/// Returns 0 if either value is not a Map.
pub fn map_similarity(a: &Value, b: &Value) -> usize {
    match (a, b) {
        (Value::Concrete(ConcreteValue::Map(ma)), Value::Concrete(ConcreteValue::Map(mb))) => ma
            .iter()
            .filter(|(k, v)| {
                mb.get(*k)
                    .map(|bv| v.semantically_equal(bv))
                    .unwrap_or(false)
            })
            .count(),
        _ => 0,
    }
}

/// Returns true when `attr_type` is exactly the IAM-style
/// `string_or_list_of_strings` shape — `Union(vec![String, list(String)])`
/// in either order — peeling through `Custom` wrappers.
fn is_string_or_list_of_strings(attr_type: &AttributeType) -> bool {
    let unwrapped = peel_custom(attr_type);
    let AttrTypeKind::Union(members) = &unwrapped.kind else {
        return false;
    };
    if members.len() != 2 {
        return false;
    }
    let mut has_string = false;
    let mut has_list_of_string = false;
    for m in members {
        match &peel_custom(m).kind {
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

/// Convert `value` to the canonical `Value::Concrete(ConcreteValue::StringList)` form when
/// `attr_type` is the `string_or_list_of_strings` shape, recursing into
/// containers (List, Map, Struct) so nested fields are also
/// canonicalized.
///
/// Conversion rules for `string_or_list_of_strings`:
/// - `Value::Concrete(ConcreteValue::String(s))` → `Value::Concrete(ConcreteValue::StringList(vec![s]))`
/// - `Value::Concrete(ConcreteValue::List([Value::String(_), ...]))` (every element a String) →
///   `Value::Concrete(ConcreteValue::StringList(vec![..]))`
/// - `Value::Concrete(ConcreteValue::StringList(_))` is returned unchanged
/// - any other shape (e.g. a list with non-string elements, a Map, a
///   ResourceRef, an unresolved Interpolation/FunctionCall) is returned
///   unchanged. Such shapes either fail validation downstream (wrong
///   type for the schema) or carry an unresolved expression that must
///   be canonicalized after resolution by a later pass.
///
/// `defs` carries the enclosing [`crate::schema::ResourceSchema::defs`]
/// map so cyclic CFN definitions (`AttributeType::Ref`) are followed
/// during the walk (carina#3340). The `Ref` arm resolves and recurses;
/// primitives / unions terminate as before. Pass
/// [`crate::schema::empty_defs_for_schema_walks()`] when the caller is confident no
/// `Ref` is reachable.
///
/// See #2481, #2510.
pub(crate) fn canonicalize_with_type(
    value: Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
) -> Value {
    canonicalize_with_type_for_enum_phase(value, attr_type, defs, EnumIdentifierPhase::RawDsl)
}

#[derive(Clone, Copy)]
enum EnumIdentifierPhase {
    RawDsl,
    StateText,
}

fn canonicalize_with_type_for_enum_phase(
    value: Value,
    attr_type: &AttributeType,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    enum_identifier_phase: EnumIdentifierPhase,
) -> Value {
    let unwrapped = peel_custom(attr_type);
    if is_string_or_list_of_strings(unwrapped) {
        return canonicalize_to_string_list(value);
    }
    // Dispatch via `Shape` so the `Ref` arm cannot fall into a tuple
    // wildcard. `shape(defs)` peels any top-level `Ref` chain before
    // returning, so the match below sees the resolved shape directly.
    // Without this, the historical wildcard `(v, _) => v` silently
    // passed Ref-typed values through without canonicalization —
    // exactly the carina#3340 / carina#3349 bug class.
    match (value, unwrapped.shape_with_defs(defs)) {
        (
            Value::Concrete(ConcreteValue::List(items)),
            crate::schema::Shape::List {
                element_type: inner,
                ..
            },
        ) => {
            let canonicalized = items
                .into_iter()
                .map(|v| {
                    canonicalize_with_type_for_enum_phase(v, inner, defs, enum_identifier_phase)
                })
                .collect();
            Value::Concrete(ConcreteValue::List(canonicalized))
        }
        (Value::Concrete(ConcreteValue::Map(map)), crate::schema::Shape::Map { value: vt, .. }) => {
            let canonicalized = map
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        canonicalize_with_type_for_enum_phase(v, vt, defs, enum_identifier_phase),
                    )
                })
                .collect();
            Value::Concrete(ConcreteValue::Map(canonicalized))
        }
        (Value::Concrete(ConcreteValue::Map(map)), crate::schema::Shape::Struct { .. }) => {
            let fields = crate::schema::struct_fields_with_defs(unwrapped, defs)
                .expect("Shape::Struct must expose struct fields internally");
            let canonicalized = map
                .into_iter()
                .map(|(k, v)| {
                    let field_type = fields
                        .iter()
                        .find(|f| f.name == k || f.provider_name.as_deref() == Some(k.as_str()))
                        .map(|f| &f.field_type);
                    let canon = match field_type {
                        Some(ft) => canonicalize_with_type_for_enum_phase(
                            v,
                            ft,
                            defs,
                            enum_identifier_phase,
                        ),
                        None => v,
                    };
                    (k, canon)
                })
                .collect();
            Value::Concrete(ConcreteValue::Map(canonicalized))
        }
        (Value::Deferred(DeferredValue::Secret(inner)), _) => Value::Deferred(
            DeferredValue::Secret(Box::new(canonicalize_with_type_for_enum_phase(
                *inner,
                attr_type,
                defs,
                enum_identifier_phase,
            ))),
        ),
        // Enum must not fall through to the `(v, _) => v` wildcard.
        // That is the same failure mode as carina#3080's Union gap:
        // the ranker/canonicalizer path looked correct for other
        // branches while this leaf silently skipped normalization.
        (val, crate::schema::Shape::Enum { .. }) => {
            let resolver = crate::resource::EnumValueResolver::with_defs(unwrapped, defs);
            match val {
                Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => {
                    let resolved = match enum_identifier_phase {
                        EnumIdentifierPhase::RawDsl => resolver.resolve_raw(&raw),
                        EnumIdentifierPhase::StateText => resolver.resolve_state_text(raw.as_str()),
                    };
                    resolved
                        .map(|c| Value::Concrete(ConcreteValue::CanonicalEnum(c)))
                        .unwrap_or_else(|_| Value::Concrete(ConcreteValue::EnumIdentifier(raw)))
                }
                Value::Concrete(ConcreteValue::String(s)) => resolver
                    .resolve_state_text(&s)
                    .map(|c| Value::Concrete(ConcreteValue::CanonicalEnum(c)))
                    .unwrap_or_else(|_| Value::Concrete(ConcreteValue::String(s))),
                Value::Concrete(ConcreteValue::CanonicalEnum(_)) => val,
                other => other,
            }
        }
        // Union: the missing nesting kind (List/Map/Struct/Secret
        // already recurse; Union was the lone gap — carina#3080).
        // `principal` is `Union[Struct{ service: Union[String,
        // List<String>] }, String]`, so without descending into the
        // matching member the nested `string_or_list_of_strings`
        // `service` never folds to `StringList`, and a bare scalar
        // (desired) vs singleton list (aws-read) reaches the differ as
        // a never-converging phantom. Pick the member with the SAME
        // scorer `validate_union` uses (`select_union_member` wraps
        // `union_member_score`) — one ranking function, not a second
        // parallel shape predicate that could drift from the
        // validator's — then re-dispatch so the existing arms
        // canonicalize it. `None` (no member shares the value's shape)
        // is identity — never guess-coerce.
        (val, crate::schema::Shape::Union) => {
            let members = crate::schema::union_members_with_defs(unwrapped, defs)
                .expect("Shape::Union must expose union members internally");
            match crate::schema::select_union_member(members, &val) {
                Some(member) => {
                    canonicalize_with_type_for_enum_phase(val, member, defs, enum_identifier_phase)
                }
                None => val,
            }
        }
        (v, _) => v,
    }
}

/// Body of [`canonicalize_with_type`] for the
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
pub struct CanonicalizedResources<'a> {
    resources: &'a mut [crate::resource::Resource],
}

impl<'a> CanonicalizedResources<'a> {
    pub fn as_mut_slice(&mut self) -> &mut [crate::resource::Resource] {
        self.resources
    }
}

/// Walk every resource's attributes, canonicalizing values whose
/// declared schema type is `Union[String, list(String)]` into
/// `Value::Concrete(ConcreteValue::StringList)`. Resources whose schema is not in the registry
/// (provider not loaded, unknown resource type) are skipped — schema
/// validation surfaces the mismatch elsewhere.
///
/// Call this once after `resolver::resolve_refs_*` and before the
/// differ runs, so every `Resource` flowing into the plan / state /
/// provider boundary carries the canonical shape. See #2481, #2511.
pub fn canonicalize_resources_with_schemas<'a>(
    resources: &'a mut [crate::resource::Resource],
    registry: &crate::schema::SchemaRegistry,
) -> CanonicalizedResources<'a> {
    for resource in resources.iter_mut() {
        let Some(schema) = registry.get_for(resource) else {
            continue;
        };
        let mut new_attrs: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        for (key, value) in std::mem::take(&mut resource.attributes) {
            let canon = match schema.attributes.get(&key) {
                Some(attr_schema) => {
                    canonicalize_with_type(value, &attr_schema.attr_type, &schema.defs)
                }
                None => value,
            };
            new_attrs.insert(key, canon);
        }
        resource.attributes = new_attrs;
    }

    CanonicalizedResources { resources }
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

fn canonicalize_provider_config_with_type(value: Value, attr_type: &AttributeType) -> Value {
    let defs = crate::schema::empty_defs_for_schema_walks();
    canonicalize_with_type_for_enum_phase(value, attr_type, defs, EnumIdentifierPhase::StateText)
}

/// Walk every provider config's attributes, canonicalizing enum-typed leaves
/// such as provider identity `region` before those values feed durable
/// identity hashing.
pub fn canonicalize_provider_configs_with_attribute_types(
    providers: &[crate::parser::ProviderConfig],
    provider_config_attribute_type_fn: &ProviderConfigAttributeTypeFn<'_>,
) -> CanonicalizedProviderConfigs {
    let mut providers = providers.to_vec();
    for provider in providers.iter_mut() {
        let mut new_attrs = indexmap::IndexMap::new();
        for (key, value) in std::mem::take(&mut provider.attributes) {
            let canon = match provider_config_attribute_type_fn(&provider.name, &key) {
                Some(attr_type) => canonicalize_provider_config_with_type(value, &attr_type),
                None => value,
            };
            new_attrs.insert(key, canon);
        }
        provider.attributes = new_attrs;
    }
    CanonicalizedProviderConfigs { providers }
}

/// [`DataSource`](crate::resource::DataSource) counterpart of
/// [`canonicalize_resources_with_schemas`]. Schema lookup routes through
/// the data-source registry (`get_for_data_source`); data sources whose
/// schema is not registered are skipped (carina#3181).
pub fn canonicalize_data_sources_with_schemas(
    data_sources: &mut [crate::resource::DataSource],
    registry: &crate::schema::SchemaRegistry,
) {
    for data_source in data_sources.iter_mut() {
        let Some(schema) = registry.get_for_data_source(data_source) else {
            continue;
        };
        let mut new_attrs: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        for (key, value) in std::mem::take(&mut data_source.attributes) {
            let canon = match schema.attributes.get(&key) {
                Some(attr_schema) => {
                    canonicalize_with_type(value, &attr_schema.attr_type, &schema.defs)
                }
                None => value,
            };
            new_attrs.insert(key, canon);
        }
        data_source.attributes = new_attrs;
    }
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
        let mut new_attrs = std::collections::HashMap::with_capacity(state.attributes.len());
        for (key, value) in std::mem::take(&mut state.attributes) {
            let canon = match schema.attributes.get(&key) {
                Some(attr_schema) => {
                    canonicalize_with_type(value, &attr_schema.attr_type, &schema.defs)
                }
                None => value,
            };
            new_attrs.insert(key, canon);
        }
        state.attributes = new_attrs;
    }
}

/// Resolve a single value's enum alias (DSL spelling → AWS canonical,
/// e.g. `IpProtocol.all` → `"-1"`), recursing into lists and maps.
///
/// Moved here from carina-cli (`resolve_value_alias`) so the apply
/// executor can re-apply it post reference-resolution — it is plan-time
/// pipeline stage 3 and, like `normalize_desired`, undone by apply-time
/// re-resolution (carina#3063). Pure: depends only on
/// `crate::utils` + the `ProviderFactory` trait (already carina-core).
/// Public so the carina-cli state-side alias pass
/// (`resolve_enum_aliases_in_states`) reuses this exact implementation
/// rather than keeping a divergent copy.
///
/// Single-shot convenience wrapper. Callers resolving multiple values for
/// the same factory should fetch [`crate::provider::ProviderFactory::schemas`]
/// once and call [`resolve_value_alias_with_schemas`] to avoid cloning the
/// provider's full schema list per attribute.
pub fn resolve_value_alias(
    value: &mut Value,
    resource_type: &str,
    attr_name: &str,
    factory: &dyn crate::provider::ProviderFactory,
) {
    let schemas = factory.schemas();
    resolve_value_alias_with_schemas(value, resource_type, attr_name, factory, &schemas);
}

/// Resolve a single value's enum alias using a caller-provided schema snapshot.
///
/// This is the hot-loop variant of [`resolve_value_alias`]. `schemas` must be
/// the result of [`crate::provider::ProviderFactory::schemas`] for `factory`.
pub fn resolve_value_alias_with_schemas(
    value: &mut Value,
    resource_type: &str,
    attr_name: &str,
    factory: &dyn crate::provider::ProviderFactory,
    schemas: &[crate::schema::ResourceSchema],
) {
    match value {
        Value::Concrete(ConcreteValue::String(s)) if enum_display_value(s).is_some() => {
            let valid_values: Vec<String> = schemas
                .iter()
                .filter(|schema| schema.resource_type == resource_type)
                .flat_map(|schema| schema.enum_valid_values_for_attr_alias(attr_name, s))
                .collect();
            // If the schema snapshot does not know `(resource_type, attr_name)`
            // as an enum field, a reverse-alias hit would mean the schema and
            // factory have drifted. Do not heal that silently with schema-free
            // extraction; leave the value unchanged.
            if !valid_values.is_empty() {
                let valid_refs: Vec<&str> = valid_values.iter().map(String::as_str).collect();
                let raw = extract_enum_value_with_values(s, &valid_refs);
                if let Some(canonical) =
                    factory.get_enum_alias_reverse(resource_type, attr_name, raw)
                {
                    *s = canonical;
                }
            }
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            for item in items.iter_mut() {
                resolve_value_alias_with_schemas(item, resource_type, attr_name, factory, schemas);
            }
        }
        Value::Concrete(ConcreteValue::Map(map)) => {
            let map_keys: Vec<String> = map.keys().cloned().collect();
            for map_key in map_keys {
                if let Some(v) = map.get_mut(&map_key) {
                    resolve_value_alias_with_schemas(v, resource_type, &map_key, factory, schemas);
                }
            }
        }
        _ => {}
    }
}

/// Re-apply enum-alias resolution to every resource, looking up the
/// factory per resource by `id.provider` (the same `find_factory`
/// dispatch the plan and apply paths use). Single source of truth for
/// enum-alias resolution so the two cannot diverge on this stage again
/// (carina#3063).
pub fn resolve_enum_aliases_for_resources(
    resources: &mut [crate::resource::Resource],
    factories: &[Box<dyn crate::provider::ProviderFactory>],
) {
    for resource in resources.iter_mut() {
        if resource.id.provider.is_empty() {
            continue;
        }
        let Some(factory) = crate::provider::find_factory(factories, &resource.id.provider) else {
            continue;
        };
        let resource_type = resource.id.resource_type.clone();
        let keys: Vec<String> = resource.attributes.keys().cloned().collect();
        let schemas = factory.schemas();
        for key in keys {
            if let Some(value) = resource.attributes.get_mut(&key) {
                resolve_value_alias_with_schemas(value, &resource_type, &key, factory, &schemas);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct SchemaAliasFactory {
        schemas: Vec<crate::schema::ResourceSchema>,
        resource_type: &'static str,
        attr_name: &'static str,
        alias_value: &'static str,
        canonical: Option<&'static str>,
    }

    impl crate::provider::ProviderFactory for SchemaAliasFactory {
        fn name(&self) -> &str {
            "aws"
        }

        fn display_name(&self) -> &str {
            "AWS"
        }

        fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
            HashMap::new()
        }

        fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
            Ok(())
        }

        fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
            String::new()
        }

        fn create_provider(
            &self,
            _binding: Option<&str>,
            _attributes: &IndexMap<String, Value>,
        ) -> futures::future::BoxFuture<
            '_,
            crate::provider::ProviderResult<Box<dyn crate::provider::Provider>>,
        > {
            unreachable!("SchemaAliasFactory::create_provider is not used in these tests")
        }

        fn schemas(&self) -> Vec<crate::schema::ResourceSchema> {
            self.schemas.clone()
        }

        fn get_enum_alias_reverse(
            &self,
            resource_type: &str,
            attr_name: &str,
            value: &str,
        ) -> Option<String> {
            match (
                resource_type == self.resource_type,
                attr_name == self.attr_name,
                value == self.alias_value,
                self.canonical,
            ) {
                (true, true, true, Some(canonical)) => Some(canonical.to_string()),
                _ => None,
            }
        }
    }

    fn status_enum(namespace: Option<&str>) -> AttributeType {
        use crate::schema::enum_identity;

        AttributeType::enum_(
            namespace
                .map(|ns| enum_identity("Status", Some(ns)))
                .unwrap_or_else(|| crate::schema::TypeIdentity::bare("Status")),
            Some(vec!["enabled".to_string(), "disabled".to_string()]),
            Vec::new(),
            None,
            None,
        )
    }

    fn schema_with_attr(
        resource_type: &'static str,
        attr_name: &'static str,
        attr_type: AttributeType,
    ) -> crate::schema::ResourceSchema {
        use crate::schema::{AttributeSchema, ResourceSchema};

        ResourceSchema::new(resource_type).attribute(AttributeSchema::new(attr_name, attr_type))
    }

    fn resolve_with_schema_alias_factory(
        schema: crate::schema::ResourceSchema,
        resource_type: &'static str,
        attr_name: &'static str,
        input: &str,
    ) -> Value {
        let factory = SchemaAliasFactory {
            schemas: vec![schema],
            resource_type,
            attr_name,
            alias_value: "disabled",
            canonical: Some("Disabled"),
        };
        let mut value = Value::Concrete(ConcreteValue::String(input.to_string()));
        resolve_value_alias(&mut value, resource_type, attr_name, &factory);
        value
    }

    #[test]
    fn resolve_value_alias_skips_schema_free_fallback_when_valid_values_empty() {
        let factory = SchemaAliasFactory {
            schemas: Vec::new(),
            resource_type: "ec2.Subnet",
            attr_name: "hostname_type",
            alias_value: "HostnameType.ip_name",
            canonical: Some("ip-name"),
        };
        let original = "aws.ec2.Subnet.PrivateDnsNameOptionsOnLaunch.HostnameType.ip_name";
        let mut value = Value::Concrete(ConcreteValue::String(original.to_string()));

        resolve_value_alias_with_schemas(&mut value, "ec2.Subnet", "hostname_type", &factory, &[]);

        assert_eq!(
            value,
            Value::Concrete(ConcreteValue::String(original.to_string()))
        );
    }

    #[test]
    fn resolve_value_alias_extracts_top_level_list_enum() {
        let schema = schema_with_attr(
            "svc.Resource",
            "statuses",
            AttributeType::list(status_enum(Some("aws.svc.Resource"))),
        );

        assert_eq!(
            resolve_with_schema_alias_factory(
                schema,
                "svc.Resource",
                "statuses",
                "aws.svc.Resource.Status.disabled",
            ),
            Value::Concrete(ConcreteValue::String("Disabled".to_string()))
        );
    }

    #[test]
    fn resolve_value_alias_extracts_map_value_enum() {
        let schema = schema_with_attr(
            "svc.Resource",
            "status_by_name",
            AttributeType::map(status_enum(Some("aws.svc.Resource"))),
        );

        assert_eq!(
            resolve_with_schema_alias_factory(
                schema,
                "svc.Resource",
                "status_by_name",
                "aws.svc.Resource.Status.disabled",
            ),
            Value::Concrete(ConcreteValue::String("Disabled".to_string()))
        );
    }

    #[test]
    fn resolve_value_alias_extracts_union_enum_member() {
        let schema = schema_with_attr(
            "svc.Resource",
            "status",
            AttributeType::union(vec![
                AttributeType::string(),
                status_enum(Some("aws.svc.Resource")),
            ]),
        );

        assert_eq!(
            resolve_with_schema_alias_factory(
                schema,
                "svc.Resource",
                "status",
                "aws.svc.Resource.Status.disabled",
            ),
            Value::Concrete(ConcreteValue::String("Disabled".to_string()))
        );
    }

    #[test]
    fn resolve_value_alias_extracts_bare_enum_by_name() {
        let schema = schema_with_attr("svc.Resource", "status", status_enum(None));

        assert_eq!(
            resolve_with_schema_alias_factory(schema, "svc.Resource", "status", "Status.disabled"),
            Value::Concrete(ConcreteValue::String("Disabled".to_string()))
        );
    }

    #[test]
    fn enum_alias_resolution_does_not_explode_on_recursive_struct_schema() {
        use crate::schema::{AttributeSchema, ResourceSchema, StructField, enum_identity};

        fn recursive_statement_def() -> AttributeType {
            let mut union_arms = vec![
                AttributeType::struct_(
                    "ByteMatchStatement",
                    vec![StructField::new("search_string", AttributeType::string())],
                ),
                AttributeType::struct_(
                    "RegexMatchStatement",
                    vec![StructField::new("regex_string", AttributeType::string())],
                ),
            ];
            for idx in 0..10 {
                union_arms.push(AttributeType::struct_(
                    format!("RecursiveArm{idx}"),
                    vec![StructField::new(
                        "statement",
                        AttributeType::ref_("Statement"),
                    )],
                ));
            }

            AttributeType::struct_(
                "Statement",
                vec![
                    StructField::new("predicate", AttributeType::string()),
                    StructField::new(
                        "and_statement",
                        AttributeType::struct_(
                            "AndStatement",
                            vec![StructField::new(
                                "statements",
                                AttributeType::list(AttributeType::ref_("Statement")),
                            )],
                        ),
                    ),
                    StructField::new(
                        "or_statement",
                        AttributeType::struct_(
                            "OrStatement",
                            vec![StructField::new(
                                "statements",
                                AttributeType::list(AttributeType::ref_("Statement")),
                            )],
                        ),
                    ),
                    StructField::new(
                        "not_statement",
                        AttributeType::struct_(
                            "NotStatement",
                            vec![StructField::new(
                                "statement",
                                AttributeType::ref_("Statement"),
                            )],
                        ),
                    ),
                    StructField::new("union_with_many_arms", AttributeType::union(union_arms)),
                ],
            )
        }

        let scope = AttributeType::enum_(
            enum_identity("Scope", Some("aws.wafv2.WebAcl")),
            Some(vec!["CLOUDFRONT".to_string(), "REGIONAL".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let schema = ResourceSchema::new("wafv2.WebAcl")
            .attribute(AttributeSchema::new("scope", scope))
            .attribute(AttributeSchema::new(
                "statement",
                AttributeType::ref_("Statement"),
            ))
            .with_def("Statement", recursive_statement_def());
        let factory = SchemaAliasFactory {
            schemas: vec![schema.clone()],
            resource_type: "wafv2.WebAcl",
            attr_name: "scope",
            alias_value: "CLOUDFRONT",
            canonical: Some("CLOUDFRONT"),
        };
        let mut value = Value::Concrete(ConcreteValue::String(
            "aws.wafv2.WebAcl.Scope.CLOUDFRONT".to_string(),
        ));

        let started = std::time::Instant::now();
        resolve_value_alias_with_schemas(&mut value, "wafv2.WebAcl", "scope", &factory, &[schema]);

        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "recursive schema enum alias lookup should stay O(1), elapsed {:?}",
            started.elapsed()
        );
        assert_eq!(
            value,
            Value::Concrete(ConcreteValue::String("CLOUDFRONT".to_string()))
        );
    }

    #[test]
    fn resolve_value_alias_does_not_collect_values_for_mismatched_identity() {
        let schema = schema_with_attr(
            "s3.BucketLifecycleConfiguration",
            "status",
            status_enum(Some("aws.s3.OtherStruct.Status")),
        );
        let original = "aws.s3.BucketLifecycleConfiguration.Rules.Status.disabled";

        assert_eq!(
            resolve_with_schema_alias_factory(
                schema,
                "s3.BucketLifecycleConfiguration",
                "status",
                original,
            ),
            Value::Concrete(ConcreteValue::String(original.to_string()))
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
        let j = value_to_json_with_context(&v, None).unwrap();
        assert_eq!(j, serde_json::json!(4500));
    }

    #[test]
    fn value_duration_round_trips_serde() {
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
    fn render_unknown_upstream_ref() {
        use crate::resource::AccessPath;
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".to_string()]);
        let r = UnknownReason::UpstreamRef { path };
        assert_eq!(
            render_unknown(&r),
            "(known after upstream apply: network.vpc.vpc_id)"
        );
    }

    #[test]
    fn render_unknown_upstream_ref_with_subscript() {
        use crate::resource::{AccessPath, Subscript};
        let path = AccessPath::with_fields_and_subscripts(
            "network",
            "accounts",
            Vec::new(),
            vec![Subscript::Int { index: 0 }],
        );
        let r = UnknownReason::UpstreamRef { path };
        assert_eq!(
            render_unknown(&r),
            "(known after upstream apply: network.accounts[0])"
        );
    }

    #[test]
    fn render_unknown_upstream_ref_with_string_subscript() {
        use crate::resource::{AccessPath, Subscript};
        let path = AccessPath::with_fields_and_subscripts(
            "vpc",
            "tags",
            Vec::new(),
            vec![Subscript::Str {
                key: "Name".to_string(),
            }],
        );
        let r = UnknownReason::UpstreamRef { path };
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
            format!("{}", UnknownReason::UpstreamRef { path }),
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
        let v = Value::Concrete(ConcreteValue::String("hello".to_string()));
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
            Value::Concrete(ConcreteValue::String("val".to_string())),
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
            Some(Value::Concrete(ConcreteValue::String("hello".to_string())))
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
                Some(&Value::Concrete(ConcreteValue::String("hello".to_string())))
            );
        } else {
            panic!("Expected Map");
        }
    }

    #[test]
    fn test_roundtrip_value_json() {
        let original = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("hello".to_string())),
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::Bool(false)),
        ]));
        let json = value_to_json(&original).unwrap();
        let back = json_to_dsl_value(&json).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn test_format_value_string() {
        let v = Value::Concrete(ConcreteValue::String("hello".to_string()));
        assert_eq!(format_value(&v), "\"hello\"");
    }

    #[test]
    fn test_format_value_user_facing_string_is_unquoted() {
        let v = Value::Concrete(ConcreteValue::String("hello".to_string()));
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
    fn test_format_value_dsl_enum() {
        let v = Value::Concrete(ConcreteValue::String(
            "aws.s3.VersioningStatus.Enabled".to_string(),
        ));
        assert_eq!(format_value(&v), "\"Enabled\"");
    }

    #[test]
    fn test_format_value_dsl_enum_region() {
        // Region displays in DSL form (underscored) until provider alias tables
        // are extended to include to_dsl reverse mappings (see issue #1675).
        let v = Value::Concrete(ConcreteValue::String(
            "aws.Region.ap_northeast_1".to_string(),
        ));
        assert_eq!(format_value(&v), "\"ap_northeast_1\"");
    }

    #[test]
    fn test_format_value_dsl_enum_5_part() {
        let v = Value::Concrete(ConcreteValue::String(
            "awscc.ec2.Vpc.InstanceTenancy.dedicated".to_string(),
        ));
        assert_eq!(format_value(&v), "\"dedicated\"");
    }

    #[test]
    fn test_format_value_two_part_enum_string() {
        // Two-part enum strings like "InstanceTenancy.dedicated" are formatted
        // through the display-shortening helper, which extracts the value part.
        let v = Value::Concrete(ConcreteValue::String(
            "InstanceTenancy.dedicated".to_string(),
        ));
        assert_eq!(format_value(&v), "\"dedicated\"");
    }

    #[test]
    fn test_format_value_bare_enum_string() {
        let v = Value::Concrete(ConcreteValue::String("dedicated".to_string()));
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
    fn json_to_dsl_value_round_trips_canonical_enum_typed_object() {
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

        canonicalize_resources_with_schemas(&mut resources, &registry);

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
    fn canonicalize_provider_configs_replaces_region_enum_with_canonical_api_value() {
        use crate::parser::ProviderConfig;
        use crate::schema::{AttributeType, DslTransform, TypeIdentity};

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
                is_default: true,
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
                is_default: true,
            },
        ];

        let providers =
            canonicalize_provider_configs_with_attribute_types(&providers, &|provider, attr| {
                (attr == "region").then(|| {
                    AttributeType::enum_(
                        TypeIdentity::new(Some(provider), Vec::<String>::new(), "Region"),
                        None,
                        Vec::new(),
                        None,
                        Some(DslTransform::HyphenToUnderscore),
                    )
                })
            });

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

    /// `Value::Deferred(DeferredValue::Unknown(UpstreamRef))` renders unquoted as
    /// `(known after upstream apply: <ref>)` via `format_value_with_key`.
    /// Stage 2 of RFC #2371 — the variant replaced the NUL-prefixed
    /// `Value::Concrete(ConcreteValue::String)` sentinel from #2367.
    #[test]
    fn test_format_value_unresolved_upstream() {
        use crate::resource::{AccessPath, UnknownReason};
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".to_string()]);
        let v = Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef { path }));
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
            Value::Concrete(ConcreteValue::String("val".to_string())),
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
    fn test_map_similarity_matching() {
        let mut m1 = IndexMap::new();
        m1.insert("a".to_string(), Value::Concrete(ConcreteValue::Int(1)));
        m1.insert("b".to_string(), Value::Concrete(ConcreteValue::Int(2)));
        let mut m2 = IndexMap::new();
        m2.insert("a".to_string(), Value::Concrete(ConcreteValue::Int(1)));
        m2.insert("b".to_string(), Value::Concrete(ConcreteValue::Int(3)));
        assert_eq!(
            map_similarity(
                &Value::Concrete(ConcreteValue::Map(m1)),
                &Value::Concrete(ConcreteValue::Map(m2))
            ),
            1
        );
    }

    #[test]
    fn test_map_similarity_non_maps() {
        assert_eq!(
            map_similarity(
                &Value::Concrete(ConcreteValue::Int(1)),
                &Value::Concrete(ConcreteValue::Int(1))
            ),
            0
        );
    }

    #[test]
    fn test_value_to_json_secret_produces_hash() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
        ))));
        let json = value_to_json(&v).unwrap();
        let s = json.as_str().unwrap();
        assert!(
            s.starts_with(SECRET_PREFIX),
            "Expected secret hash prefix, got: {}",
            s
        );
        // Argon2id with 32-byte output = 64 hex characters
        let hash = s.strip_prefix(SECRET_PREFIX).unwrap();
        assert_eq!(hash.len(), 64, "Expected 64-char hex hash, got: {}", hash);
    }

    #[test]
    fn test_value_to_json_secret_is_deterministic() {
        let v1 = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
        ))));
        let v2 = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
        ))));
        let json1 = value_to_json(&v1).unwrap();
        let json2 = value_to_json(&v2).unwrap();
        assert_eq!(json1, json2);
    }

    #[test]
    fn test_value_to_json_secret_different_values_different_hashes() {
        let v1 = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("password-1".to_string()),
        ))));
        let v2 = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("password-2".to_string()),
        ))));
        let json1 = value_to_json(&v1).unwrap();
        let json2 = value_to_json(&v2).unwrap();
        assert_ne!(json1, json2);
    }

    #[test]
    fn test_format_value_secret() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
        ))));
        assert_eq!(format_value(&v), "(secret)");
    }

    #[test]
    fn test_format_value_secret_in_map() {
        let mut map = IndexMap::new();
        map.insert(
            "Name".to_string(),
            Value::Concrete(ConcreteValue::String("test".to_string())),
        );
        map.insert(
            "SecretTag".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("my-password".to_string()),
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
            Value::Concrete(ConcreteValue::String("test".to_string())),
        );
        map.insert(
            "SecretTag".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("my-password".to_string()),
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
    fn test_format_value_secret_hash_string() {
        // State stores secret hashes as strings; they should also display as "(secret)"
        let hash_str = format!(
            "{}{}",
            SECRET_PREFIX, "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
        let v = Value::Concrete(ConcreteValue::String(hash_str));
        assert_eq!(format_value(&v), "(secret)");
    }

    #[test]
    fn test_value_to_json_with_context_different_resources_different_hashes() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
        ))));
        let ctx1 = SecretHashContext::new("ec2.Vpc", "vpc-1", "password");
        let ctx2 = SecretHashContext::new("rds.db_instance", "my-db", "password");
        let json1 = value_to_json_with_context(&v, Some(&ctx1)).unwrap();
        let json2 = value_to_json_with_context(&v, Some(&ctx2)).unwrap();
        assert_ne!(
            json1, json2,
            "Same password on different resources should produce different hashes"
        );
    }

    #[test]
    fn test_value_to_json_with_context_different_attributes_different_hashes() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
        ))));
        let ctx1 = SecretHashContext::new("rds.db_instance", "my-db", "master_password");
        let ctx2 = SecretHashContext::new("rds.db_instance", "my-db", "admin_password");
        let json1 = value_to_json_with_context(&v, Some(&ctx1)).unwrap();
        let json2 = value_to_json_with_context(&v, Some(&ctx2)).unwrap();
        assert_ne!(
            json1, json2,
            "Same password on different attributes should produce different hashes"
        );
    }

    #[test]
    fn test_value_to_json_with_context_same_context_is_deterministic() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
        ))));
        let ctx = SecretHashContext::new("rds.db_instance", "my-db", "master_password");
        let json1 = value_to_json_with_context(&v, Some(&ctx)).unwrap();
        let json2 = value_to_json_with_context(&v, Some(&ctx)).unwrap();
        assert_eq!(
            json1, json2,
            "Same password with same context should produce identical hashes"
        );
    }

    #[test]
    fn test_value_to_json_with_context_differs_from_no_context() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
        ))));
        let ctx = SecretHashContext::new("rds.db_instance", "my-db", "master_password");
        let json_with_ctx = value_to_json_with_context(&v, Some(&ctx)).unwrap();
        let json_no_ctx = value_to_json(&v).unwrap();
        assert_ne!(
            json_with_ctx, json_no_ctx,
            "Context-based hash should differ from fallback hash"
        );
    }

    #[test]
    fn test_redact_secrets_in_value_replaces_secret() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
        ))));
        let redacted = redact_secrets_in_value(&v).unwrap();
        // Should be a String starting with the secret prefix, not a Secret variant
        match &redacted {
            Value::Concrete(ConcreteValue::String(s)) => {
                assert!(
                    s.starts_with(SECRET_PREFIX),
                    "Expected secret hash prefix, got: {}",
                    s
                );
            }
            _ => panic!(
                "Expected Value::Concrete(ConcreteValue::String) after redaction, got: {:?}",
                redacted
            ),
        }
    }

    #[test]
    fn test_redact_secrets_in_value_no_plaintext_in_serialized_output() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("super-secret-password".to_string()),
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
            CompositionAttribute::from_value(Value::Concrete(ConcreteValue::String(
                "kept".to_string(),
            ))),
        );
        attrs.insert(
            "secret_field".to_string(),
            CompositionAttribute::from_value(Value::Deferred(DeferredValue::Secret(Box::new(
                Value::Concrete(ConcreteValue::String("plaintext-must-not-leak".to_string())),
            )))),
        );
        let virt = Composition {
            id: ResourceId::with_identity("_virtual", "module_instance"),
            signature: Signature {
                arguments: indexmap::IndexMap::new(),
                attributes: attrs,
            },
            binding: Some("module_instance".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_name: "m".to_string(),
            instance: "module_instance".to_string(),
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
            Some(Value::Concrete(ConcreteValue::String("kept".to_string()))),
        );

        // The secret attribute is replaced with the hash prefix, not the
        // plaintext.
        match redacted
            .signature
            .attributes
            .get("secret_field")
            .map(|a| a.to_value())
            .as_ref()
        {
            Some(Value::Concrete(ConcreteValue::String(s))) => {
                assert!(
                    s.starts_with(SECRET_PREFIX),
                    "expected redacted hash prefix, got: {}",
                    s
                );
                assert!(
                    !s.contains("plaintext-must-not-leak"),
                    "redacted form must not leak plaintext, got: {}",
                    s
                );
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
            Value::Concrete(ConcreteValue::String("test".to_string())),
        );
        map.insert(
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("s3cret".to_string()),
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
            Value::Concrete(ConcreteValue::String("visible".to_string())),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("hidden".to_string()),
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
        let v = Value::Concrete(ConcreteValue::String("not-a-secret".to_string()));
        let redacted = redact_secrets_in_value(&v).unwrap();
        assert_eq!(redacted, v);
    }

    #[test]
    fn test_redact_secrets_in_attributes() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("my-bucket".to_string())),
        );
        attrs.insert(
            "password".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("hunter2".to_string()),
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
        let v = Value::Concrete(ConcreteValue::String("hello".to_string()));
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
            "aws.s3.Bucket.VersioningStatus.enabled".to_string(),
        ));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), format_value(&v));
    }

    #[test]
    fn format_value_pretty_secret_masked() {
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("my-password".to_string()),
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
            Value::Concrete(ConcreteValue::String("First".to_string())),
        );
        s1.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Allow".to_string())),
        );
        let mut s2 = IndexMap::new();
        s2.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("Second".to_string())),
        );
        s2.insert(
            "effect".to_string(),
            Value::Concrete(ConcreteValue::String("Deny".to_string())),
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
            Value::Concrete(ConcreteValue::String("v".to_string())),
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
                Value::Concrete(ConcreteValue::String(sid.to_string())),
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
            Value::Concrete(ConcreteValue::String(long.clone())),
        );
        let mut s2 = IndexMap::new();
        s2.insert(
            "sid_b".to_string(),
            Value::Concrete(ConcreteValue::String(long.clone())),
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
            Value::Concrete(ConcreteValue::String("2012".to_string())),
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
            Value::Concrete(ConcreteValue::String(long.clone())),
        );
        let mut s2 = IndexMap::new();
        s2.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String(long.clone())),
        );
        let mut outer = IndexMap::new();
        outer.insert(
            "version".to_string(),
            Value::Concrete(ConcreteValue::String("2012".to_string())),
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
            Value::Concrete(ConcreteValue::String("a".to_string())),
            Value::Concrete(ConcreteValue::String("b".to_string())),
        ]));
        assert_eq!(format_value_pretty(&v, layout(0, "k")), "[\"a\", \"b\"]");
    }

    #[test]
    fn format_value_pretty_list_of_strings_over_80_vertical() {
        // 5 strings of ~20 chars each → inline ~110 chars
        let items: Vec<Value> = (0..5)
            .map(|i| Value::Concrete(ConcreteValue::String(format!("iam:LongActionName{}", i))))
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
            ConcreteValue::String(item),
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
            ConcreteValue::String(item),
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
            ConcreteValue::String(item),
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
                Value::Concrete(ConcreteValue::String("prod".to_string())),
            );
            Value::Concrete(ConcreteValue::Map(m))
        });
        let mut entry = IndexMap::new();
        entry.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("X".to_string())),
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
            .map(|i| Value::Concrete(ConcreteValue::String(format!("iam:Action{:03}", i))))
            .collect();
        let mut entry = IndexMap::new();
        entry.insert(
            "sid".to_string(),
            Value::Concrete(ConcreteValue::String("X".to_string())),
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
                "repo:carina-rs/infra:ref:refs/heads/main".to_string(),
                "repo:carina-rs/infra:pull_request".to_string(),
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
            "a".repeat(40),
            "b".repeat(40),
        ]));
        let out = format_value_pretty(&v, layout(0, "k"));
        assert!(
            out.starts_with("[\n"),
            "expected oversize StringList to expand vertically, got:\n{out}"
        );
    }

    #[test]
    fn format_value_pretty_string_list_vertical_redacts_secret_prefix_strings() {
        // Pin parity with `Value::Concrete(ConcreteValue::List)<Value::Concrete(ConcreteValue::String)>`: a string with
        // the SECRET_PREFIX must render as `(secret)` even when reached
        // through the StringList vertical path. Pre-fix attempt
        // (a dedicated `format_list_of_strings_vertical` that quoted the
        // raw &str) would have leaked the hash; the current shape lifts
        // items to `Value::Concrete(ConcreteValue::String)` and reuses
        // `format_list_of_scalars_vertical`, so the SECRET_PREFIX arm
        // in `format_value_with_key` runs unchanged.
        let v = Value::Concrete(ConcreteValue::StringList(vec![
            format!("{}deadbeef", SECRET_PREFIX),
            "x".repeat(80), // force vertical
        ]));
        let out = format_value_pretty(&v, layout(0, "k"));
        assert!(
            out.contains("(secret),"),
            "expected SECRET_PREFIX item to render as `(secret),` in the vertical form, got:\n{out}"
        );
        assert!(
            !out.contains("deadbeef"),
            "expected hash bytes to not leak into the vertical form, got:\n{out}"
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
                    "repo:carina-rs/infra:ref:refs/heads/main".to_string(),
                )),
                Value::Concrete(ConcreteValue::String(
                    "repo:carina-rs/infra:pull_request".to_string(),
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
            Value::Concrete(ConcreteValue::String("AssumeRole".to_string())),
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
            Value::Concrete(ConcreteValue::String("a".repeat(40))),
        );
        m.insert(
            "second_key".to_string(),
            Value::Concrete(ConcreteValue::String("b".repeat(40))),
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
            Value::Concrete(ConcreteValue::String("hello".to_string())),
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::Float(2.5)),
            Value::Concrete(ConcreteValue::Bool(false)),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("pw".to_string()),
            )))),
            Value::Deferred(DeferredValue::Unknown(UnknownReason::ForKey)),
            Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef {
                path: path.clone(),
            })),
            Value::Deferred(DeferredValue::ResourceRef { path: path.clone() }),
            Value::Deferred(DeferredValue::Interpolation(vec![
                InterpolationPart::Literal("prefix-".to_string()),
                InterpolationPart::Expr(Value::Deferred(DeferredValue::ResourceRef { path })),
            ])),
            Value::Deferred(DeferredValue::FunctionCall {
                name: "concat".to_string(),
                args: vec![
                    Value::Concrete(ConcreteValue::String("a".to_string())),
                    Value::Concrete(ConcreteValue::String("b".to_string())),
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
        let v = Value::Concrete(ConcreteValue::String("repo:foo:*".to_string()));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(
            canon,
            Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".to_string()]))
        );
    }

    #[test]
    fn canonicalize_single_element_list_to_string_list() {
        let t = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("repo:foo:*".to_string()),
        )]));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(
            canon,
            Value::Concrete(ConcreteValue::StringList(vec!["repo:foo:*".to_string()]))
        );
    }

    #[test]
    fn canonicalize_multi_element_list_to_string_list() {
        let t = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".to_string())),
            Value::Concrete(ConcreteValue::String("b".to_string())),
            Value::Concrete(ConcreteValue::String("c".to_string())),
        ]));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(
            canon,
            Value::Concrete(ConcreteValue::StringList(vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string()
            ]))
        );
    }

    #[test]
    fn canonicalize_idempotent_on_string_list() {
        let t = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::StringList(vec!["a".to_string()]));
        let canon =
            canonicalize_with_type(v.clone(), &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(canon, v);
    }

    #[test]
    fn canonicalize_passes_through_non_applicable_type() {
        let v = Value::Concrete(ConcreteValue::String("foo".to_string()));
        let canon = canonicalize_with_type(
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
        let v = Value::Concrete(ConcreteValue::String("Allow".to_string()));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
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
        let canon =
            canonicalize_with_type(v.clone(), &t, crate::schema::empty_defs_for_schema_walks());
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
            Value::Concrete(ConcreteValue::String("s3:GetObject".to_string())),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("action"),
                    Some(&Value::Concrete(ConcreteValue::StringList(vec![
                        "s3:GetObject".to_string()
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
            Value::Concrete(ConcreteValue::String("sts:AssumeRole".to_string())),
        );
        let mut policy = IndexMap::new();
        policy.insert(
            "statement".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(statement),
            )])),
        );

        let canon = schema.canonicalize(Value::Concrete(ConcreteValue::Map(policy)));
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
                "sts:AssumeRole".to_string()
            ])))
        );
    }

    #[test]
    fn canonicalize_recurses_into_struct_via_provider_name() {
        let t = AttributeType::struct_(
            "Statement".to_string(),
            vec![
                crate::schema::StructField::new("action", string_or_list_of_strings())
                    .with_provider_name("Action"),
            ],
        );
        let mut map = IndexMap::new();
        map.insert(
            "Action".to_string(),
            Value::Concrete(ConcreteValue::String("s3:GetObject".to_string())),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("Action"),
                    Some(&Value::Concrete(ConcreteValue::StringList(vec![
                        "s3:GetObject".to_string()
                    ])))
                );
            }
            _ => panic!("expected Map"),
        }
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
            Value::Concrete(ConcreteValue::String(
                "cloudfront.amazonaws.com".to_string(),
            )),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("service"),
                    Some(&Value::Concrete(ConcreteValue::StringList(vec![
                        "cloudfront.amazonaws.com".to_string()
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
                ConcreteValue::String("cloudfront.amazonaws.com".to_string()),
            )])),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("service"),
                    Some(&Value::Concrete(ConcreteValue::StringList(vec![
                        "cloudfront.amazonaws.com".to_string()
                    ])))
                );
            }
            _ => panic!("expected Map"),
        }
    }

    /// The `String` member of the same Union, given a bare string,
    /// passes through unchanged (it is not `string_or_list_of_strings`).
    #[test]
    fn canonicalize_union_string_member_passthrough() {
        let t = principal_union();
        let v = Value::Concrete(ConcreteValue::String("*".to_string()));
        let canon =
            canonicalize_with_type(v.clone(), &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(canon, v);
    }

    /// A Union with no member whose shape matches the value → identity
    /// (safe fallthrough; never guess-coerce).
    #[test]
    fn canonicalize_union_no_matching_member_is_identity() {
        let t = AttributeType::union(vec![AttributeType::int(), AttributeType::bool()]);
        let v = Value::Concrete(ConcreteValue::String("not-an-int".to_string()));
        let canon =
            canonicalize_with_type(v.clone(), &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(canon, v);
    }

    #[test]
    fn canonicalize_recurses_into_map_value_type() {
        let t = AttributeType::map_with_key(AttributeType::string(), string_or_list_of_strings());
        let mut map = IndexMap::new();
        map.insert(
            "token.actions.githubusercontent.com:sub".to_string(),
            Value::Concrete(ConcreteValue::String("repo:foo:*".to_string())),
        );
        let v = Value::Concrete(ConcreteValue::Map(map));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Concrete(ConcreteValue::Map(m)) => {
                assert_eq!(
                    m.get("token.actions.githubusercontent.com:sub"),
                    Some(&Value::Concrete(ConcreteValue::StringList(vec![
                        "repo:foo:*".to_string()
                    ])))
                );
            }
            _ => panic!("expected Map"),
        }
    }

    #[test]
    fn canonicalize_string_or_list_union() {
        let t = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::String("x".to_string()));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        assert_eq!(
            canon,
            Value::Concrete(ConcreteValue::StringList(vec!["x".to_string()]))
        );
    }

    #[test]
    fn canonicalize_secret_recurses_inner() {
        let t = string_or_list_of_strings();
        let v = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            ConcreteValue::String("s".to_string()),
        ))));
        let canon = canonicalize_with_type(v, &t, crate::schema::empty_defs_for_schema_walks());
        match canon {
            Value::Deferred(DeferredValue::Secret(inner)) => {
                assert_eq!(
                    *inner,
                    Value::Concrete(ConcreteValue::StringList(vec!["s".to_string()]))
                );
            }
            _ => panic!("expected Secret"),
        }
    }

    #[test]
    fn canonicalize_value_to_json_string_list_serializes_as_array() {
        let v = Value::Concrete(ConcreteValue::StringList(vec![
            "a".to_string(),
            "b".to_string(),
        ]));
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
            Value::Concrete(ConcreteValue::String("hello".to_string())),
            Value::Concrete(ConcreteValue::String("(empty)".to_string())),
            Value::Concrete(ConcreteValue::Int(42)),
            Value::Concrete(ConcreteValue::Int(-7)),
            Value::Concrete(ConcreteValue::Float(1.5)),
            Value::Concrete(ConcreteValue::Float(2.0)),
            Value::Concrete(ConcreteValue::Bool(true)),
            Value::Concrete(ConcreteValue::Bool(false)),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String("hidden".to_string()),
            )))),
        ] {
            assert_inline_width_matches_build(&v);
        }
    }

    #[test]
    fn inline_width_parity_for_lists_and_maps() {
        let list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".to_string())),
            Value::Concrete(ConcreteValue::Int(1)),
            Value::Concrete(ConcreteValue::Bool(true)),
        ]));
        assert_inline_width_matches_build(&list);

        let mut map = IndexMap::new();
        map.insert(
            "k1".to_string(),
            Value::Concrete(ConcreteValue::String("v1".to_string())),
        );
        map.insert("k2".to_string(), Value::Concrete(ConcreteValue::Int(99)));
        assert_inline_width_matches_build(&Value::Concrete(ConcreteValue::Map(map)));

        // Nested list/map.
        let mut inner = IndexMap::new();
        inner.insert("x".to_string(), Value::Concrete(ConcreteValue::Int(1)));
        let nested = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::Map(inner)),
            Value::Concrete(ConcreteValue::String("end".to_string())),
        ]));
        assert_inline_width_matches_build(&nested);

        // Empty collections.
        assert_inline_width_matches_build(&Value::Concrete(ConcreteValue::List(vec![])));
        assert_inline_width_matches_build(&Value::Concrete(ConcreteValue::Map(IndexMap::new())));
    }

    #[test]
    fn inline_width_parity_for_dsl_enum_string() {
        // DSL enum identifiers (e.g. `aws.s3.VersioningStatus.Enabled`)
        // resolve to their provider value before being quoted in
        // `format_value_with_key`. inline_width must follow the same
        // resolution to match the rendered byte length.
        let v = Value::Concrete(ConcreteValue::String(
            "aws.Region.ap_northeast_1".to_string(),
        ));
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
        let v = Value::Concrete(ConcreteValue::StringList(vec![
            "a".to_string(),
            "b".to_string(),
        ]));
        assert_eq!(format_value(&v), "[\"a\", \"b\"]");
    }

    #[test]
    fn canonicalize_partial_eq_distinguishes_list_and_string_list() {
        // `Value::Concrete(ConcreteValue::List([String("x")]))` and `Value::Concrete(ConcreteValue::StringList(vec!["x"]))`
        // are *not* equal under PartialEq — the type system carries the
        // canonical-form invariant. Producers must canonicalize first.
        let a = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("x".to_string()),
        )]));
        let b = Value::Concrete(ConcreteValue::StringList(vec!["x".to_string()]));
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
        use crate::resource::{Resource, ResourceId, ResourceIdentity};
        use std::collections::{BTreeSet, HashMap, HashSet};
        let mut attributes = IndexMap::new();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v);
        }
        Resource {
            id: ResourceId {
                provider: "aws".to_string(),
                resource_type: "iam.policy".to_string(),
                identity: Some(ResourceIdentity::new("p1")),
                provider_instance: None,
            },
            attributes,
            directives: Default::default(),
            prefixes: HashMap::new(),
            binding: Some("p1".to_string()),
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }

    #[test]
    fn canonicalize_resources_with_schemas_scalar_to_string_list() {
        let registry = build_test_registry();
        let mut resources = vec![make_resource(vec![(
            "subject",
            Value::Concrete(ConcreteValue::String("repo:foo:*".to_string())),
        )])];
        canonicalize_resources_with_schemas(&mut resources, &registry);
        assert_eq!(
            resources[0].attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "repo:foo:*".to_string()
            ])))
        );
    }

    #[test]
    fn canonicalize_resources_with_schemas_single_list_to_string_list() {
        let registry = build_test_registry();
        let mut resources = vec![make_resource(vec![(
            "subject",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("repo:foo:*".to_string()),
            )])),
        )])];
        canonicalize_resources_with_schemas(&mut resources, &registry);
        assert_eq!(
            resources[0].attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "repo:foo:*".to_string()
            ])))
        );
    }

    #[test]
    fn canonicalize_resources_with_schemas_skips_unknown_resource() {
        // No schema registered for the resource type — pass through.
        let registry = crate::schema::SchemaRegistry::new();
        let mut resources = vec![make_resource(vec![(
            "subject",
            Value::Concrete(ConcreteValue::String("x".to_string())),
        )])];
        canonicalize_resources_with_schemas(&mut resources, &registry);
        assert_eq!(
            resources[0].attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::String("x".to_string())))
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
                Value::Concrete(ConcreteValue::String("x".to_string())),
            ),
            (
                "name",
                Value::Concrete(ConcreteValue::String("p1".to_string())),
            ),
        ])];
        canonicalize_resources_with_schemas(&mut resources, &registry);
        assert_eq!(
            resources[0].attributes.get("name"),
            Some(&Value::Concrete(ConcreteValue::String("p1".to_string())))
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
            Value::Concrete(ConcreteValue::String("repo:foo:*".to_string())),
        )])];
        let mut b = vec![make_resource(vec![(
            "subject",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("repo:foo:*".to_string()),
            )])),
        )])];
        canonicalize_resources_with_schemas(&mut a, &registry);
        canonicalize_resources_with_schemas(&mut b, &registry);
        assert_eq!(a[0].attributes, b[0].attributes);
    }

    // ---- canonicalize_states_with_schemas tests (#2481, #2513) ----

    fn make_state(attrs: Vec<(&str, Value)>) -> crate::resource::State {
        use crate::resource::{ResourceId, ResourceIdentity, State};
        use std::collections::{BTreeSet, HashMap};
        let mut attributes = HashMap::new();
        for (k, v) in attrs {
            attributes.insert(k.to_string(), v);
        }
        State {
            id: ResourceId {
                provider: "aws".to_string(),
                resource_type: "iam.policy".to_string(),
                identity: Some(ResourceIdentity::new("p1")),
                provider_instance: None,
            },
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
            Value::Concrete(ConcreteValue::String("repo:foo:*".to_string())),
        )]);
        states.insert(s.id.clone(), s);
        canonicalize_states_with_schemas(&mut states, &registry);
        let state = states.values().next().unwrap();
        assert_eq!(
            state.attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "repo:foo:*".to_string()
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
                ConcreteValue::String("repo:foo:*".to_string()),
            )])),
        )]);
        states.insert(s.id.clone(), s);
        canonicalize_states_with_schemas(&mut states, &registry);
        let state = states.values().next().unwrap();
        assert_eq!(
            state.attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::StringList(vec![
                "repo:foo:*".to_string()
            ])))
        );
    }

    #[test]
    fn canonicalize_states_with_schemas_skips_unknown_resource() {
        let registry = crate::schema::SchemaRegistry::new();
        let mut states = std::collections::HashMap::new();
        let s = make_state(vec![(
            "subject",
            Value::Concrete(ConcreteValue::String("x".to_string())),
        )]);
        states.insert(s.id.clone(), s);
        canonicalize_states_with_schemas(&mut states, &registry);
        let state = states.values().next().unwrap();
        assert_eq!(
            state.attributes.get("subject"),
            Some(&Value::Concrete(ConcreteValue::String("x".to_string())))
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
                ConcreteValue::String("repo:foo:*".to_string()),
            )])),
        )])];
        canonicalize_resources_with_schemas(&mut resources, &registry);

        let mut states = std::collections::HashMap::new();
        let s = make_state(vec![(
            "subject",
            Value::Concrete(ConcreteValue::String("repo:foo:*".to_string())),
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
        use crate::differ::{Diff, diff};
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
        canonicalize_resources_with_schemas(&mut desired, &registry);

        let mut attrs = HashMap::new();
        attrs.insert(
            "availability_zone".to_string(),
            Value::Concrete(ConcreteValue::String("ap-northeast-1a".to_string())),
        );
        let mut states = HashMap::new();
        states.insert(id.clone(), State::existing(id, attrs));
        canonicalize_states_with_schemas(&mut states, &registry);

        let result = diff(
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
            Value::Concrete(ConcreteValue::String(
                "cloudfront.amazonaws.com".to_string(),
            )),
        );
        let mut resources = vec![make_resource(vec![(
            "principal",
            Value::Concrete(ConcreteValue::Map(desired_inner)),
        )])];
        canonicalize_resources_with_schemas(&mut resources, &registry);

        // State: aws-read singleton list inside the Struct member.
        let mut state_inner = IndexMap::new();
        state_inner.insert(
            "service".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("cloudfront.amazonaws.com".to_string()),
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
                    "cloudfront.amazonaws.com".to_string(),
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
        let v = Value::Concrete(ConcreteValue::StringList(vec![
            "a".to_string(),
            "b".to_string(),
        ]));
        assert!(list_of_string.validate(&v).is_ok());
    }

    #[test]
    fn validate_union_accepts_canonical_string_list() {
        let union = string_or_list_of_strings();
        let v = Value::Concrete(ConcreteValue::StringList(vec!["x".to_string()]));
        assert!(union.validate(&v).is_ok());
    }

    #[test]
    fn format_value_string_list_renders_brackets() {
        // `format_value` must render `Value::Concrete(ConcreteValue::StringList)` with bracket
        // syntax — not a `_` wildcard fallback that would print debug
        // garbage in plan output.
        let v = Value::Concrete(ConcreteValue::StringList(vec![
            "a".to_string(),
            "b".to_string(),
        ]));
        let formatted = format_value(&v);
        assert_eq!(formatted, "[\"a\", \"b\"]");
    }

    #[test]
    fn inline_width_returns_none_when_just_over_budget() {
        // List of three short strings: `["aa", "bb", "cc"]` = 18 bytes.
        // Budget 17 must return None; budget 18 must return Some(18).
        let v = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("aa".to_string())),
            Value::Concrete(ConcreteValue::String("bb".to_string())),
            Value::Concrete(ConcreteValue::String("cc".to_string())),
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
            Value::Concrete(ConcreteValue::String("aaaaaaaaaaaa".to_string())),
            Value::Concrete(ConcreteValue::String("bbbbbbbbbbbb".to_string())),
        ]));
        let small = format_value_pretty(&small_list, layout(0, "k"));
        assert!(
            !small.contains('\n'),
            "small list must render inline, got: {small:?}"
        );

        // Over the limit (vertical expected).
        let big_list = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".repeat(40))),
            Value::Concrete(ConcreteValue::String("b".repeat(40))),
            Value::Concrete(ConcreteValue::String("c".repeat(40))),
        ]));
        let big = format_value_pretty(&big_list, layout(0, "k"));
        assert!(
            big.contains('\n'),
            "oversize list must render vertically, got: {big:?}"
        );
    }

    /// Carina#3329 (round-4): `plan --out` on the supported scenario
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
                UnknownReason::UpstreamRef { path },
            ))),
            InterpolationPart::Literal("|registry-dev.carina-rs.dev|NS".to_string()),
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
        let secret_inner = Value::Concrete(ConcreteValue::String("super-secret".to_string()));
        let identifier = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Secret(Box::new(
                secret_inner,
            )))),
            InterpolationPart::Literal("|tail".to_string()),
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
                // The redactor emits a `_carina_secret_` hash prefix
                // for the secret's leaf; that prefix must be present
                // somewhere in the redacted Value tree.
                fn walk(v: &Value) -> bool {
                    use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};
                    match v {
                        Value::Concrete(ConcreteValue::String(s)) => {
                            s.starts_with(crate::value::SECRET_PREFIX)
                        }
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
                    "redacted identifier must carry the secret-prefix marker, got: {identifier:?}",
                );
            }
            other => panic!("expected Effect::Import, got {other:?}"),
        }
    }

    #[test]
    fn redact_secrets_in_deferred_create_preserves_for_unknowns() {
        use crate::effect::Effect;
        use crate::parser::{DeferredForExpression, ForBinding};
        use crate::plan::Plan;
        use crate::resource::{
            AccessPath, DeferredValue, Resource, ResourceId, UnknownReason, Value,
        };

        let placeholder = Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValuePath {
            path: AccessPath::with_fields("opt", "resource_record", vec!["name".to_string()]),
        }));
        let mut template_resource = Resource::new("aws.route53.RecordSet", "validation_records");
        template_resource.set_attr("name", placeholder.clone());
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
                attributes: vec![("name".to_string(), placeholder.clone())],
                binding_name: "validation_records".to_string(),
                iterable_binding: "cert".to_string(),
                iterable_attr: "domain_validation_options".to_string(),
                binding: ForBinding::Simple("opt".to_string()),
                template_resource,
            }),
        };

        let redacted = redact_secrets_in_effect(&effect)
            .expect("deferred-for template placeholders must survive effect redaction");
        assert_deferred_create_placeholder_survives(&redacted, &placeholder);

        let mut plan = Plan::new();
        plan.add(effect);
        let redacted_plan = redact_secrets_in_plan(&plan)
            .expect("deferred-for template placeholders must survive plan redaction");
        assert_deferred_create_placeholder_survives(&redacted_plan.effects()[0], &placeholder);
    }

    fn assert_deferred_create_placeholder_survives(
        effect: &crate::effect::Effect,
        expected: &Value,
    ) {
        fn assert_for_value_path(value: Option<&Value>, expected: &Value) {
            match (value, expected) {
                (
                    Some(Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValuePath {
                        path,
                    }))),
                    Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValuePath {
                        path: expected_path,
                    })),
                ) => {
                    assert_eq!(path.binding(), expected_path.binding());
                    assert_eq!(path.attribute(), expected_path.attribute());
                    assert_eq!(path.segments(), expected_path.segments());
                }
                (actual, expected) => {
                    panic!("expected preserved ForValuePath {expected:?}, got {actual:?}")
                }
            }
        }

        match effect {
            crate::effect::Effect::DeferredCreate { template, .. } => {
                assert_for_value_path(
                    template
                        .attributes
                        .iter()
                        .find(|(key, _)| key == "name")
                        .map(|(_, value)| value),
                    expected,
                );
                assert_for_value_path(template.template_resource.attributes.get("name"), expected);
            }
            other => panic!("expected Effect::DeferredCreate, got {other:?}"),
        }
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
                Value::Concrete(ConcreteValue::String("bucket-old".to_string())),
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
            ConcreteValue::String("previous-secret".to_string()),
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

        match value {
            Value::Concrete(ConcreteValue::String(redacted_secret)) => {
                assert!(redacted_secret.starts_with(SECRET_PREFIX));
                assert!(!redacted_secret.contains("previous-secret"));
            }
            other => panic!("expected concrete redacted secret string, got {other:?}"),
        }
    }
}
