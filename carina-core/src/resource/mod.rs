//! Resource - Representing resources and their state

use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// The `name` portion of a `ResourceId`.
///
/// Anonymous resources start out as `Pending` because the parser sees
/// the resource block before it has extracted the `name` attribute.
/// A later post-processing pass converts `Pending` to `Bound(name)`
/// once the attribute has been read. Encoding this transient state in
/// the type makes it impossible to confuse "anonymous, ID not yet
/// assigned" with "actual ID is the empty string" (#2225).
///
/// On disk the variant is collapsed to a plain JSON string for
/// backward compatibility with v5 state files: `Pending` round-trips
/// through `""`, `Bound(s)` through `s`. The discriminant is
/// reconstructed on deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ResourceName {
    /// An identifier already extracted (from a `let` binding or the
    /// `name` attribute of an anonymous resource).
    Bound(String),
    /// An anonymous resource whose `name` attribute has not yet been
    /// promoted to the `ResourceId`. Must be replaced with `Bound`
    /// before the value can flow to plan generation, state, or
    /// providers.
    Pending,
}

impl ResourceName {
    /// True when this `ResourceName` has not yet been bound to a
    /// concrete identifier.
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    /// Borrow the bound identifier as a `&str`. `Pending` returns the
    /// empty string — sites that need to distinguish must `match` on
    /// the variant directly.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Bound(s) => s,
            Self::Pending => "",
        }
    }
}

impl std::fmt::Display for ResourceName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for ResourceName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ResourceName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(if s.is_empty() {
            Self::Pending
        } else {
            Self::Bound(s)
        })
    }
}

/// Unique identifier for a resource
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceId {
    /// Provider name (e.g., "aws", "awscc")
    pub provider: String,
    /// Resource type (e.g., "s3.Bucket", "ec2.Instance")
    pub resource_type: String,
    /// Resource name (identifier specified in DSL).
    ///
    /// `Pending` means the resource is anonymous and the `name`
    /// attribute has not yet been promoted into the `ResourceId`.
    /// All downstream consumers (state, plan, providers) require
    /// `Bound`.
    pub name: ResourceName,
}

impl ResourceId {
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            provider: String::new(),
            resource_type: resource_type.into(),
            name: ResourceName::from_string(name.into()),
        }
    }

    pub fn with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            provider: provider.into(),
            resource_type: resource_type.into(),
            name: ResourceName::from_string(name.into()),
        }
    }

    /// Borrow the resolved identifier as `&str`. `Pending` returns
    /// the empty string; sites that distinguish should `match` on
    /// `self.name` directly.
    pub fn name_str(&self) -> &str {
        self.name.as_str()
    }

    /// Set the resource's name, replacing any existing `Pending` or
    /// `Bound` variant with `Bound(name)`.
    pub fn set_name(&mut self, name: impl Into<String>) {
        self.name = ResourceName::Bound(name.into());
    }

    /// Returns the display type including provider prefix if available
    pub fn display_type(&self) -> String {
        if self.provider.is_empty() {
            self.resource_type.clone()
        } else {
            format!("{}.{}", self.provider, self.resource_type)
        }
    }

    /// Borrow this id as a human-facing display value.
    ///
    /// Unlike the default `Display`, which renders the canonical dotted
    /// form (`provider.resource_type.name`) used as a logical identifier
    /// for hashmap keys, binding fallbacks, and DSL syntax, this wrapper
    /// renders `provider.resource_type` and `name` separated by a single
    /// space — making the type/address boundary visible to readers
    /// (carina-rs/carina#2572).
    ///
    /// Use this in progress UIs and human-readable plan/apply output;
    /// keep using `Display` for any context that round-trips through
    /// state files, lookup keys, or DSL.
    pub fn human(&self) -> ResourceIdDisplay<'_> {
        ResourceIdDisplay(self)
    }
}

/// Human-facing display wrapper for [`ResourceId`].
///
/// Construct with [`ResourceId::human`]; renders via `Display` as
/// `provider.resource_type<SPACE>name` (or `resource_type<SPACE>name`
/// when the provider segment is empty).
///
/// The wrapper exists as a distinct type, rather than as a second
/// inherent method that returns `String`, so the choice between
/// "human-readable display" and "canonical logical identifier" is
/// visible at every call site instead of being a string-formatting
/// convention. See carina-rs/carina#2572 for context.
pub struct ResourceIdDisplay<'a>(&'a ResourceId);

impl std::fmt::Display for ResourceIdDisplay<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let id = self.0;
        if id.provider.is_empty() {
            write!(f, "{} {}", id.resource_type, id.name)
        } else {
            write!(f, "{}.{} {}", id.provider, id.resource_type, id.name)
        }
    }
}

impl ResourceName {
    /// Convert a string into a `ResourceName`. Empty input becomes
    /// `Pending`; any other input becomes `Bound`. Used by
    /// `ResourceId::new` / `with_provider` to keep the legacy
    /// `String` constructors compatible.
    fn from_string(s: String) -> Self {
        if s.is_empty() {
            Self::Pending
        } else {
            Self::Bound(s)
        }
    }
}

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.provider.is_empty() {
            write!(f, "{}.{}", self.resource_type, self.name)
        } else {
            write!(f, "{}.{}.{}", self.provider, self.resource_type, self.name)
        }
    }
}

/// A `[index]` subscript appended to an `AccessPath`'s field chain.
///
/// `binding.field[0]` and `binding.field["k"]` parse into the same
/// `binding` + `attribute` + `field_path` as `binding.field`, plus a
/// trailing `Subscript` capturing the `[…]` form. Distinguishing
/// integer from string at this layer lets cross-directory shape checks
/// reject `[0]` against a `map(_)` export and `["k"]` against a
/// `list(_)` export with type-aware diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Subscript {
    /// Integer subscript: `[0]`. Valid against `list(_)` exports.
    Int { index: i64 },
    /// String subscript: `["k"]`. Valid against `map(_)` exports.
    Str { key: String },
}

impl Subscript {
    /// Append this subscript to `out` in source-form: `[0]` or `["k"]`.
    /// String keys go through `{:?}` so embedded quotes/backslashes
    /// round-trip as valid DSL source — the same form is used by
    /// diagnostic messages so escapes matter there too.
    pub fn append_to_dot_string(&self, out: &mut String) {
        use std::fmt::Write as _;
        match self {
            Subscript::Int { index } => {
                let _ = write!(out, "[{}]", index);
            }
            Subscript::Str { key } => {
                let _ = write!(out, "[{:?}]", key);
            }
        }
    }
}

/// A typed access path representing a `ResourceRef` target.
///
/// The path always carries a binding name and an attribute name; nested
/// field access (e.g., `web.network.vpc_id`) is captured in
/// `field_path`, and any trailing `[index]` subscripts in `subscripts`.
/// The grammar accepts only `binding.field[…]…`, never `binding[…].field`,
/// so the two chains never interleave — pre-field index access is folded
/// into the binding name string by the parser as before.
///
/// The "binding + attribute is mandatory" invariant is enforced by the
/// type system — there is no way to construct an `AccessPath` without
/// both.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccessPath {
    binding: String,
    attribute: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    field_path: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    subscripts: Vec<Subscript>,
}

impl AccessPath {
    /// Create an `AccessPath` referring to a top-level attribute of a binding.
    ///
    /// `attribute` must be non-empty. A bare-binding reference (no
    /// `.attr`) is represented by [`Value::BindingRef`], not by an
    /// `AccessPath` with an empty attribute.
    pub fn new(binding: impl Into<String>, attribute: impl Into<String>) -> Self {
        let binding = binding.into();
        let attribute = attribute.into();
        assert!(
            !attribute.is_empty(),
            "AccessPath::new with empty attribute for binding {:?}; \
             use Value::BindingRef instead (#2847)",
            binding
        );
        Self {
            binding,
            attribute,
            field_path: Vec::new(),
            subscripts: Vec::new(),
        }
    }

    /// Create an `AccessPath` with a nested field path (e.g., `web.network.vpc_id`).
    ///
    /// `attribute` must be non-empty. See [`AccessPath::new`].
    pub fn with_fields(
        binding: impl Into<String>,
        attribute: impl Into<String>,
        field_path: Vec<String>,
    ) -> Self {
        let binding = binding.into();
        let attribute = attribute.into();
        assert!(
            !attribute.is_empty(),
            "AccessPath::with_fields with empty attribute for binding {:?}; \
             use Value::BindingRef instead (#2847)",
            binding
        );
        Self {
            binding,
            attribute,
            field_path,
            subscripts: Vec::new(),
        }
    }

    /// Create an `AccessPath` with both a field chain and trailing
    /// `[index]` subscripts. Used by the parser when source contains
    /// `binding.field[idx]` or `binding.field.subfield[idx]…`.
    ///
    /// `attribute` must be non-empty. See [`AccessPath::new`].
    pub fn with_fields_and_subscripts(
        binding: impl Into<String>,
        attribute: impl Into<String>,
        field_path: Vec<String>,
        subscripts: Vec<Subscript>,
    ) -> Self {
        let binding = binding.into();
        let attribute = attribute.into();
        assert!(
            !attribute.is_empty(),
            "AccessPath::with_fields_and_subscripts with empty attribute for binding {:?}; \
             use Value::BindingRef instead (#2847)",
            binding
        );
        Self {
            binding,
            attribute,
            field_path,
            subscripts,
        }
    }

    /// Returns the binding name.
    pub fn binding(&self) -> &str {
        &self.binding
    }

    /// Returns the attribute name.
    pub fn attribute(&self) -> &str {
        &self.attribute
    }

    /// Returns the nested field path (empty if the reference targets a
    /// top-level attribute).
    pub fn field_path(&self) -> &[String] {
        &self.field_path
    }

    /// Returns the trailing `[index]` subscripts (empty if the
    /// reference doesn't subscript past the field chain).
    pub fn subscripts(&self) -> &[Subscript] {
        &self.subscripts
    }

    /// Returns the path in source-form: `binding.attribute` followed by
    /// `.field…[idx]…` segments as written.
    pub fn to_dot_string(&self) -> String {
        let mut out = String::with_capacity(
            self.binding.len()
                + self.attribute.len()
                + 1
                + self.field_path.iter().map(|s| s.len() + 1).sum::<usize>(),
        );
        out.push_str(&self.binding);
        out.push('.');
        out.push_str(&self.attribute);
        for field in &self.field_path {
            out.push('.');
            out.push_str(field);
        }
        for sub in &self.subscripts {
            sub.append_to_dot_string(&mut out);
        }
        out
    }
}

impl std::fmt::Display for AccessPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_dot_string())
    }
}

/// Serde adapter that maps `std::time::Duration` ↔ integer seconds.
///
/// Without this, the default `Duration` serde impl emits
/// `{ "secs": N, "nanos": N }`, but the project's design decision is to
/// store durations as a plain integer at every JSON boundary
/// (state file, plan file, WIT plugin contract — see
/// `notes/specs/2026-05-10-duration-design.md`). The variant carries
/// `#[serde(with = "duration_secs")]` so this module is used uniformly
/// on serialise and deserialise.
pub(crate) mod duration_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}

/// Attribute value of a resource.
///
/// Phase 5 of [RFC #2972](https://github.com/carina-rs/carina/issues/2972):
/// physically split into the concrete / deferred axis. Every
/// pattern site explicitly acknowledges which axis it handles, so
/// "deferred Value leaked into concrete-only path" bugs are
/// structurally unrepresentable workspace-wide (not just inside
/// `validate_*`).
///
/// The serde representation uses `#[serde(untagged)]` so existing
/// state files (in the pre-Phase-5 flat shape) round-trip via the
/// inner enums' externally-tagged defaults — exactly the legacy
/// JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Concrete(ConcreteValue),
    Deferred(DeferredValue),
}

/// Concrete-axis variants: values that carry their own runtime type
/// and are safe to type-check at validate time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConcreteValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// Time duration carried as `std::time::Duration`.
    ///
    /// Constructed from a `<integer><unit>` literal in DSL source
    /// `75min`, `1h`, `30s`. Serialises to JSON as integer seconds at
    /// every value-tree boundary (state file, plan file, WIT plugin
    /// contract).
    #[serde(with = "duration_secs")]
    Duration(std::time::Duration),
    List(Vec<Value>),
    /// Canonical form for fields whose schema type is
    /// `Union(vec![String, list(String)])` — the IAM-style
    /// `string_or_list_of_strings` shape. See #2481, #2510.
    StringList(Vec<String>),
    Map(IndexMap<String, Value>),
}

/// Deferred-axis variants: placeholders that resolve later
/// (apply time, upstream load, function evaluation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeferredValue {
    /// Reference to another resource's attribute via an access path.
    ResourceRef { path: AccessPath },
    /// Reference to a binding without an attribute selector.
    BindingRef { binding: String },
    /// String interpolation: `"prefix-${expr}-suffix"`
    Interpolation(Vec<InterpolationPart>),
    /// Built-in function call: `join("-", ["a", "b"])` or via pipe
    /// `["a", "b"] |> join("-")`. Evaluated during reference resolution.
    FunctionCall { name: String, args: Vec<Value> },
    /// A secret value. The inner value is sent to the provider but
    /// stored as a SHA256 hash in state.
    Secret(Box<Value>),
    /// A value not known at plan time. RFC #2371.
    #[serde(skip)]
    Unknown(UnknownReason),
}

/// Backward-compatible variant constructors. Call sites can write
/// `Value::String(s)` etc. exactly as in the pre-Phase-5 flat enum;
/// pattern positions descend through `Value::Concrete(ConcreteValue::X(...))`.
#[allow(non_snake_case)]
impl Value {
    #[inline]
    pub fn String(s: String) -> Self {
        Value::Concrete(ConcreteValue::String(s))
    }
    #[inline]
    pub fn Int(n: i64) -> Self {
        Value::Concrete(ConcreteValue::Int(n))
    }
    #[inline]
    pub fn Float(f: f64) -> Self {
        Value::Concrete(ConcreteValue::Float(f))
    }
    #[inline]
    pub fn Bool(b: bool) -> Self {
        Value::Concrete(ConcreteValue::Bool(b))
    }
    #[inline]
    pub fn Duration(d: std::time::Duration) -> Self {
        Value::Concrete(ConcreteValue::Duration(d))
    }
    #[inline]
    pub fn List(items: Vec<Value>) -> Self {
        Value::Concrete(ConcreteValue::List(items))
    }
    #[inline]
    pub fn StringList(items: Vec<String>) -> Self {
        Value::Concrete(ConcreteValue::StringList(items))
    }
    #[inline]
    pub fn Map(map: IndexMap<String, Value>) -> Self {
        Value::Concrete(ConcreteValue::Map(map))
    }
    #[inline]
    pub fn Interpolation(parts: Vec<InterpolationPart>) -> Self {
        Value::Deferred(DeferredValue::Interpolation(parts))
    }
    #[inline]
    pub fn Secret(inner: Box<Value>) -> Self {
        Value::Deferred(DeferredValue::Secret(inner))
    }
    #[inline]
    pub fn Unknown(reason: UnknownReason) -> Self {
        Value::Deferred(DeferredValue::Unknown(reason))
    }
}

/// Borrowing projection of [`Value`] restricted to the **concrete** axis
/// — variants that carry their own runtime type and are safe to type-check
/// at validate time.
///
/// Phase 1 of [RFC #2972](https://github.com/carina-rs/carina/issues/2972).
/// Sub-systems that only operate on resolved values (`validate`, the
/// differ, the serializer) take `&ConcreteValueRef<'_>` so the deferred
/// case is structurally unreachable rather than a runtime `matches!`
/// guard. Today `Value` is still flat; once every concrete-only path
/// has migrated to this view, the underlying `Value` enum will be
/// physically split into `Value { Concrete(ConcreteValue), Deferred(DeferredValue) }`
/// and this borrow type will become a thin wrapper over the inner
/// `ConcreteValue`.
///
/// Recursive container variants (`List`, `Map`, `StringList`) borrow
/// from the parent `Value`; the inner element / key types stay
/// `Value` because lists and maps can mix concrete and deferred
/// elements (e.g. `["literal", vpc.id]`).
#[derive(Debug, Clone, Copy)]
pub enum ConcreteValueRef<'a> {
    String(&'a str),
    Int(i64),
    Float(f64),
    Bool(bool),
    Duration(std::time::Duration),
    List(&'a [Value]),
    StringList(&'a [String]),
    Map(&'a IndexMap<String, Value>),
}

/// Borrowing projection of [`Value`] restricted to the **deferred** axis
/// — placeholders that resolve later (apply time, upstream load,
/// function evaluation).
///
/// Sub-systems that need to walk only deferred values (e.g.
/// `check_upstream_state_field_types`, `validate_resource_ref_types`,
/// dependency analysis) take `&DeferredValueRef<'_>` so they cannot
/// accidentally consume a concrete value as a placeholder.
///
/// Phase 1 of [RFC #2972](https://github.com/carina-rs/carina/issues/2972).
#[derive(Debug, Clone, Copy)]
pub enum DeferredValueRef<'a> {
    ResourceRef { path: &'a AccessPath },
    BindingRef { binding: &'a str },
    Interpolation(&'a [InterpolationPart]),
    FunctionCall { name: &'a str, args: &'a [Value] },
    Secret(&'a Value),
    Unknown(&'a UnknownReason),
}

impl Value {
    /// If this value is on the concrete axis, return a borrowing
    /// projection. Returns `None` for deferred-resolution variants
    /// ([`Value::ResourceRef`], [`Value::BindingRef`],
    /// [`Value::Interpolation`], [`Value::FunctionCall`],
    /// [`Value::Secret`], [`Value::Unknown`]).
    ///
    /// Concrete-only sub-systems should consume `Value` exclusively
    /// through this accessor — the returned [`ConcreteValueRef`]
    /// cannot represent a deferred value, so the "deferred leaked
    /// into concrete-only path" bug class becomes structurally
    /// unrepresentable. See RFC #2972.
    pub fn as_concrete(&self) -> Option<ConcreteValueRef<'_>> {
        match self {
            Value::Concrete(c) => Some(match c {
                ConcreteValue::String(s) => ConcreteValueRef::String(s),
                ConcreteValue::Int(n) => ConcreteValueRef::Int(*n),
                ConcreteValue::Float(f) => ConcreteValueRef::Float(*f),
                ConcreteValue::Bool(b) => ConcreteValueRef::Bool(*b),
                ConcreteValue::Duration(d) => ConcreteValueRef::Duration(*d),
                ConcreteValue::List(items) => ConcreteValueRef::List(items),
                ConcreteValue::StringList(items) => ConcreteValueRef::StringList(items),
                ConcreteValue::Map(map) => ConcreteValueRef::Map(map),
            }),
            Value::Deferred(_) => None,
        }
    }

    /// If this value is on the deferred axis, return a borrowing
    /// projection. Returns `None` for concrete variants. Mirror of
    /// [`Self::as_concrete`].
    pub fn as_deferred(&self) -> Option<DeferredValueRef<'_>> {
        match self {
            Value::Deferred(d) => Some(match d {
                DeferredValue::ResourceRef { path } => DeferredValueRef::ResourceRef { path },
                DeferredValue::BindingRef { binding } => DeferredValueRef::BindingRef { binding },
                DeferredValue::Interpolation(parts) => DeferredValueRef::Interpolation(parts),
                DeferredValue::FunctionCall { name, args } => {
                    DeferredValueRef::FunctionCall { name, args }
                }
                DeferredValue::Secret(inner) => DeferredValueRef::Secret(inner),
                DeferredValue::Unknown(reason) => DeferredValueRef::Unknown(reason),
            }),
            Value::Concrete(_) => None,
        }
    }
}

/// Why a `Value::Unknown` is unknown. Each variant carries only the
/// information its own consumer needs — no shared "context" field invites
/// drift across reasons. See RFC #2371.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UnknownReason {
    /// Plan-time reference into an `upstream_state` binding that did not
    /// resolve (state file missing, or the referenced export was absent).
    /// Renders as `(known after upstream apply: <path.to_dot_string()>)`.
    /// Holds the original `AccessPath` so display retains subscripts and
    /// chained field access (`network.accounts[0]`, `vpc.tags["Name"]`).
    UpstreamRef { path: AccessPath },
    /// Plan-time reference into an `upstream_state` binding written
    /// without an attribute selector — `let v = bootstrap` (since the
    /// type-split in #2856 lowers a bare identifier to
    /// `Value::BindingRef`). Renders as
    /// `(known after upstream apply: <binding>)`. Parallel to
    /// `UpstreamRef`; kept as a distinct variant so the type system
    /// carries the "no attribute" condition rather than a runtime
    /// `path.attribute().is_empty()` check inside `UpstreamRef`. See
    /// #2876.
    UpstreamBareRef { binding: String },
    /// Map-binding key in a deferred for-expression
    /// (`for (k, _) in iterable`). Substituted with the actual key when
    /// the iterable is later resolved.
    ForKey,
    /// Indexed-binding index in a deferred for-expression
    /// (`for (i, _) in iterable`). Substituted with the actual index.
    ForIndex,
    /// Loop-variable value in a deferred for-expression
    /// (`for v in iterable`). Substituted with the actual element.
    ForValue,
    /// Mid-edit empty `${}` interpolation. Carries no payload — the
    /// presence of the marker is enough for the LSP to surface a
    /// diagnostic at the `${}` span and for downstream resolvers to
    /// stay tolerant. See #2480.
    EmptyInterpolation,
}

/// A part of a string interpolation expression
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InterpolationPart {
    /// Literal text segment
    Literal(String),
    /// An expression to be evaluated and converted to string
    Expr(Value),
}

/// `PartialEq` for `Value` is hand-rolled (not derived) to enforce one
/// invariant the differ depends on: **`Value::Unknown` is never equal
/// to anything**, including another `Value::Unknown` with the same
/// reason. An unresolved value is, by definition, unknown — two
/// independently-unresolved attributes are not the "same value", so a
/// derived `PartialEq` would silently suppress real diffs in
/// `merge_lists_hashed` and `semantically_equal`. See RFC #2371.
impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            // Constraint: Unknown is never equal to anything.
            (Value::Deferred(DeferredValue::Unknown(_)), _)
            | (_, Value::Deferred(DeferredValue::Unknown(_))) => false,
            (
                Value::Concrete(ConcreteValue::String(a)),
                Value::Concrete(ConcreteValue::String(b)),
            ) => a == b,
            (Value::Concrete(ConcreteValue::Int(a)), Value::Concrete(ConcreteValue::Int(b))) => {
                a == b
            }
            (
                Value::Concrete(ConcreteValue::Float(a)),
                Value::Concrete(ConcreteValue::Float(b)),
            ) => a == b,
            (Value::Concrete(ConcreteValue::Bool(a)), Value::Concrete(ConcreteValue::Bool(b))) => {
                a == b
            }
            (
                Value::Concrete(ConcreteValue::Duration(a)),
                Value::Concrete(ConcreteValue::Duration(b)),
            ) => a == b,
            (Value::Concrete(ConcreteValue::List(a)), Value::Concrete(ConcreteValue::List(b))) => {
                a == b
            }
            (
                Value::Concrete(ConcreteValue::StringList(a)),
                Value::Concrete(ConcreteValue::StringList(b)),
            ) => a == b,
            (Value::Concrete(ConcreteValue::Map(a)), Value::Concrete(ConcreteValue::Map(b))) => {
                a == b
            }
            (
                Value::Deferred(DeferredValue::ResourceRef { path: a }),
                Value::Deferred(DeferredValue::ResourceRef { path: b }),
            ) => a == b,
            (
                Value::Deferred(DeferredValue::BindingRef { binding: a }),
                Value::Deferred(DeferredValue::BindingRef { binding: b }),
            ) => a == b,
            (
                Value::Deferred(DeferredValue::Interpolation(a)),
                Value::Deferred(DeferredValue::Interpolation(b)),
            ) => a == b,
            (
                Value::Deferred(DeferredValue::FunctionCall { name: an, args: aa }),
                Value::Deferred(DeferredValue::FunctionCall { name: bn, args: ba }),
            ) => an == bn && aa == ba,
            (
                Value::Deferred(DeferredValue::Secret(a)),
                Value::Deferred(DeferredValue::Secret(b)),
            ) => a == b,
            _ => false,
        }
    }
}

/// Body of `Value::canonicalize_in_place` for the `Interpolation` arm:
/// fold simple `Expr(scalar)` into a `Literal` (consuming the scalar's
/// string), merge adjacent `Literal`s, and collapse the result to
/// `Value::String` when no `Expr` parts remain.
fn canonicalize_interpolation(parts: Vec<InterpolationPart>) -> Value {
    let mut merged: Vec<InterpolationPart> = Vec::with_capacity(parts.len());
    for part in parts {
        let next = match part {
            InterpolationPart::Literal(s) => InterpolationPart::Literal(s),
            InterpolationPart::Expr(mut v) => {
                v.canonicalize_in_place();
                match value_into_literal(v) {
                    Ok(s) => InterpolationPart::Literal(s),
                    Err(other) => InterpolationPart::Expr(other),
                }
            }
        };
        match (merged.last_mut(), next) {
            (Some(InterpolationPart::Literal(prev)), InterpolationPart::Literal(s)) => {
                prev.push_str(&s);
            }
            (_, p) => merged.push(p),
        }
    }
    if merged.is_empty() {
        return Value::Concrete(ConcreteValue::String(String::new()));
    }
    if merged.len() == 1 {
        // Pop the sole element. If it is a Literal, we collapse to
        // `Value::String`; otherwise it is a non-foldable Expr and we
        // rebuild the single-element Interpolation.
        match merged.pop().expect("len == 1") {
            InterpolationPart::Literal(s) => return Value::Concrete(ConcreteValue::String(s)),
            expr @ InterpolationPart::Expr(_) => merged.push(expr),
        }
    }
    Value::Deferred(DeferredValue::Interpolation(merged))
}

/// If `v` is a string-shaped scalar, return its string form (consuming
/// `v`); otherwise return `v` unchanged so the caller can keep wrapping
/// it as an `Expr`.
///
/// `Secret(_)` is intentionally **not** folded: stripping the wrapper
/// would let the secret travel as a plain `Literal` and bypass
/// redaction in plan display, state serialization, and logging. Secrets
/// stay as `Expr(Secret(...))`.
fn value_into_literal(v: Value) -> Result<String, Value> {
    match v {
        Value::Concrete(ConcreteValue::String(s)) => Ok(s),
        Value::Concrete(ConcreteValue::Int(n)) => Ok(n.to_string()),
        Value::Concrete(ConcreteValue::Float(f)) => Ok(f.to_string()),
        Value::Concrete(ConcreteValue::Bool(b)) => Ok(b.to_string()),
        other => Err(other),
    }
}

impl Value {
    /// Recursively normalize this `Value` so that downstream code does not
    /// have to handle redundant `Interpolation` shapes.
    ///
    /// Applied bottom-up:
    ///
    /// - Adjacent `InterpolationPart::Literal`s are merged.
    /// - `InterpolationPart::Expr(v)` whose `v` is a bare `String`/`Int`/
    ///   `Float`/`Bool` is folded into a `Literal`, then merged with
    ///   neighbors per the previous rule. `Secret(_)` is intentionally
    ///   not folded — keeping it wrapped preserves redaction in plan
    ///   display, state serialization, and logging.
    /// - An `Interpolation` whose parts collapse to a single `Literal` is
    ///   replaced with `Value::String(s)`.
    ///
    /// `List`, `Map`, `Secret`, and `FunctionCall` recurse into their
    /// children at the `Value` level. Other variants are returned
    /// unchanged. The transformation is idempotent.
    ///
    /// See #2227.
    pub fn canonicalize(mut self) -> Value {
        self.canonicalize_in_place();
        self
    }

    /// In-place variant of [`Self::canonicalize`]. Useful when the caller
    /// only has a `&mut Value` (e.g. inside `IndexMap::values_mut()`).
    pub fn canonicalize_in_place(&mut self) {
        match self {
            Value::Concrete(ConcreteValue::List(items)) => {
                for item in items {
                    item.canonicalize_in_place();
                }
            }
            Value::Concrete(ConcreteValue::Map(map)) => {
                for v in map.values_mut() {
                    v.canonicalize_in_place();
                }
            }
            Value::Deferred(DeferredValue::Secret(inner)) => inner.canonicalize_in_place(),
            Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
                for arg in args {
                    arg.canonicalize_in_place();
                }
            }
            Value::Deferred(DeferredValue::Interpolation(_)) => {
                // Move the parts out so we can consume and rebuild them.
                let parts = match std::mem::replace(
                    self,
                    Value::Deferred(DeferredValue::Interpolation(Vec::new())),
                ) {
                    Value::Deferred(DeferredValue::Interpolation(parts)) => parts,
                    _ => unreachable!("matched Value::Interpolation"),
                };
                *self = canonicalize_interpolation(parts);
            }
            _ => {}
        }
    }

    /// Create a `ResourceRef` from binding name, attribute name, and optional field path.
    ///
    /// `attribute_name` must be non-empty. A bare-binding reference (no
    /// `.attr`) is a [`Value::BindingRef`], not a `ResourceRef` with an
    /// empty attribute — see #2847 for the regression that motivated
    /// the type-level split.
    pub fn resource_ref(
        binding_name: impl Into<String>,
        attribute_name: impl Into<String>,
        field_path: Vec<String>,
    ) -> Self {
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::with_fields(binding_name, attribute_name, field_path),
        })
    }

    /// If this is a `ResourceRef`, returns the binding name.
    pub fn ref_binding(&self) -> Option<&str> {
        match self {
            Value::Deferred(DeferredValue::ResourceRef { path }) => Some(path.binding()),
            _ => None,
        }
    }

    /// If this is a `ResourceRef`, returns the attribute name.
    pub fn ref_attribute(&self) -> Option<&str> {
        match self {
            Value::Deferred(DeferredValue::ResourceRef { path }) => Some(path.attribute()),
            _ => None,
        }
    }

    /// If this is a `ResourceRef`, returns the field path.
    pub fn ref_field_path(&self) -> Option<&[String]> {
        match self {
            Value::Deferred(DeferredValue::ResourceRef { path }) => Some(path.field_path()),
            _ => None,
        }
    }

    /// Recursively walk this value, invoking `f` on each `ResourceRef`'s `AccessPath`.
    pub fn visit_refs(&self, f: &mut impl FnMut(&AccessPath)) {
        match self {
            Value::Deferred(DeferredValue::ResourceRef { path }) => f(path),
            Value::Concrete(ConcreteValue::List(items)) => {
                for v in items {
                    v.visit_refs(f);
                }
            }
            Value::Concrete(ConcreteValue::Map(map)) => {
                for v in map.values() {
                    v.visit_refs(f);
                }
            }
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                for part in parts {
                    if let InterpolationPart::Expr(v) = part {
                        v.visit_refs(f);
                    }
                }
            }
            Value::Deferred(DeferredValue::FunctionCall { args, .. }) => {
                for arg in args {
                    arg.visit_refs(f);
                }
            }
            Value::Deferred(DeferredValue::Secret(inner)) => inner.visit_refs(f),
            Value::Concrete(ConcreteValue::String(_))
            | Value::Concrete(ConcreteValue::Int(_))
            | Value::Concrete(ConcreteValue::Float(_))
            | Value::Concrete(ConcreteValue::Bool(_))
            | Value::Concrete(ConcreteValue::Duration(_))
            | Value::Concrete(ConcreteValue::StringList(_)) => {}
            // `BindingRef` carries no attribute, so attribute-walking
            // visitors have nothing to do. Callers that *do* care about
            // bare-binding references walk them explicitly via
            // `visit_binding_refs`.
            Value::Deferred(DeferredValue::BindingRef { .. }) => {}
            // `Value::Unknown` is what a previously-unresolved
            // `ResourceRef` was *replaced with* by `stamp_unresolved_upstream`.
            // It carries an `AccessPath` for display, but it is no longer
            // a live reference to walk — dependency analysis happens
            // upstream of the stamping pass.
            Value::Deferred(DeferredValue::Unknown(_)) => {}
        }
    }
}

/// Project an `IndexMap<String, Value>` (the shape `Resource.attributes`
/// uses since #2222) into a plain `HashMap<String, Value>` for callers
/// that only need key-based lookup (state merging, ResourceRef
/// resolution, provider trait inputs). Source-order is dropped on
/// purpose at this boundary — keep it on `Resource.attributes` itself
/// when iteration order matters.
///
/// Despite the historical name (the helper used to operate on the now-removed
/// `Expr` newtype), this function does **not** resolve `Value::ResourceRef` /
/// `Value::Interpolation` / `Value::FunctionCall` / `Value::Secret` — it just
/// projects ordered attribute storage to a hashmap. Callers that need concrete
/// values must run `carina_core::resolver::resolve_refs_with_state_and_remote`
/// (or an equivalent) first. See #1683 for a regression caused by assuming
/// this method performed resolution.
pub fn attrs_to_hashmap(attrs: &IndexMap<String, Value>) -> HashMap<String, Value> {
    attrs.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

/// Check if a Value contains any ResourceRef (possibly nested)
pub fn contains_resource_ref(value: &Value) -> bool {
    let mut found = false;
    value.visit_refs(&mut |_| found = true);
    found
}

impl Value {
    /// Semantic equality: for Lists, compares as multisets (order-insensitive);
    /// for Maps, compares values recursively with semantic equality;
    /// for single-element List([Map]) vs Map, treats them as equivalent (bare struct);
    /// for all other variants, falls back to PartialEq.
    pub fn semantically_equal(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Concrete(ConcreteValue::List(a)), Value::Concrete(ConcreteValue::List(b))) => {
                lists_equal(a, b)
            }
            (Value::Concrete(ConcreteValue::Map(a)), Value::Concrete(ConcreteValue::Map(b))) => {
                maps_semantically_equal(a, b)
            }
            _ => self == other,
        }
    }

    /// Produce a deterministic hash for use in hash-based multiset comparison.
    /// For Maps, keys are sorted to ensure deterministic output.
    fn canonical_hash(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash_into(&mut hasher);
        hasher.finish()
    }

    /// Feed a deterministic representation of this Value into a Hasher.
    fn hash_into(&self, hasher: &mut impl Hasher) {
        std::mem::discriminant(self).hash(hasher);
        match self {
            Value::Concrete(ConcreteValue::String(s)) => s.hash(hasher),
            Value::Concrete(ConcreteValue::Int(i)) => i.hash(hasher),
            Value::Concrete(ConcreteValue::Float(f)) => {
                // Use bits for deterministic hashing (NaN == NaN for our purposes)
                f.to_bits().hash(hasher);
            }
            Value::Concrete(ConcreteValue::Bool(b)) => b.hash(hasher),
            Value::Concrete(ConcreteValue::Duration(d)) => d.as_secs().hash(hasher),
            Value::Concrete(ConcreteValue::List(items)) => {
                // For list hashing, use an order-independent combination (wrapping sum)
                // so that lists with same elements in different order hash the same.
                // Wrapping sum is preferred over XOR because XOR causes all lists
                // with duplicate elements to collide (e.g., [1,1] XOR = 0, [2,2] XOR = 0).
                items.len().hash(hasher);
                let mut sum_hash: u64 = 0;
                for item in items {
                    sum_hash = sum_hash.wrapping_add(item.canonical_hash());
                }
                sum_hash.hash(hasher);
            }
            Value::Concrete(ConcreteValue::StringList(items)) => {
                // Hash with the same order-independent shape as
                // `Value::List` so that a `List([String("x")])` and a
                // `StringList(vec!["x"])` cannot collide on hash equality
                // (the outer discriminant separates them) but each
                // individually preserves the merge-list invariant.
                items.len().hash(hasher);
                let mut sum_hash: u64 = 0;
                for s in items {
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    s.hash(&mut h);
                    sum_hash = sum_hash.wrapping_add(h.finish());
                }
                sum_hash.hash(hasher);
            }
            Value::Concrete(ConcreteValue::Map(map)) => {
                map.len().hash(hasher);
                // Sort keys for deterministic hashing
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for key in keys {
                    key.hash(hasher);
                    map[key].hash_into(hasher);
                }
            }
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                path.hash(hasher);
            }
            Value::Deferred(DeferredValue::BindingRef { binding }) => {
                binding.hash(hasher);
            }
            Value::Deferred(DeferredValue::Interpolation(parts)) => {
                parts.len().hash(hasher);
                for part in parts {
                    match part {
                        InterpolationPart::Literal(s) => {
                            0u8.hash(hasher);
                            s.hash(hasher);
                        }
                        InterpolationPart::Expr(v) => {
                            1u8.hash(hasher);
                            v.hash_into(hasher);
                        }
                    }
                }
            }
            Value::Deferred(DeferredValue::FunctionCall { name, args }) => {
                name.hash(hasher);
                args.len().hash(hasher);
                for arg in args {
                    arg.hash_into(hasher);
                }
            }
            Value::Deferred(DeferredValue::Secret(inner)) => {
                inner.hash_into(hasher);
            }
            Value::Deferred(DeferredValue::Unknown(reason)) => {
                // `Value::Unknown` reaches `merge_lists_hashed` → this
                // function whenever a list element is unresolved. Hash
                // deterministically: the outer `discriminant(self)`
                // already separates `Unknown` from concrete variants;
                // here we add the reason discriminant + payload so two
                // structurally-identical `Unknown`s produce the same
                // hash. Note: `PartialEq for Value` still returns
                // `false` between any two `Unknown`s (RFC #2371) — the
                // `a == b ⇒ hash(a) == hash(b)` precondition only
                // matters between concrete values.
                std::mem::discriminant(reason).hash(hasher);
                match reason {
                    // Reuse `AccessPath`'s native `Hash` impl (matches
                    // the `ResourceRef` arm above) — no per-call String
                    // allocation.
                    UnknownReason::UpstreamRef { path } => path.hash(hasher),
                    UnknownReason::UpstreamBareRef { binding } => binding.hash(hasher),
                    // `For{Key,Index,Value}` and `EmptyInterpolation`
                    // carry no payload; the discriminant alone already
                    // distinguishes them.
                    UnknownReason::ForKey
                    | UnknownReason::ForIndex
                    | UnknownReason::ForValue
                    | UnknownReason::EmptyInterpolation => {}
                }
            }
        }
    }
}

/// Compare two maps using semantic equality for their values.
/// This ensures nested lists within maps are compared order-insensitively.
fn maps_semantically_equal(a: &IndexMap<String, Value>, b: &IndexMap<String, Value>) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .all(|(k, v)| b.get(k).map(|bv| v.semantically_equal(bv)).unwrap_or(false))
}

/// Threshold below which we use the simple O(n^2) algorithm.
/// For small lists, the overhead of hashing is not worth it.
const HASH_THRESHOLD: usize = 20;

/// Multiset (bag) comparison for two lists of Values.
/// Returns true if both lists contain the same elements with the same multiplicities,
/// regardless of order.
///
/// For small lists (< 20 elements), uses O(n^2) matching.
/// For large lists, uses hash-based bucketing to achieve O(n) average case.
fn lists_equal(a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if a.len() < HASH_THRESHOLD {
        return lists_equal_quadratic(a, b);
    }
    lists_equal_hashed(a, b)
}

/// O(n^2) multiset comparison for small lists.
fn lists_equal_quadratic(a: &[Value], b: &[Value]) -> bool {
    let mut matched = vec![false; b.len()];
    for item_a in a {
        let mut found = false;
        for (j, item_b) in b.iter().enumerate() {
            if !matched[j] && item_a.semantically_equal(item_b) {
                matched[j] = true;
                found = true;
                break;
            }
        }
        if !found {
            return false;
        }
    }
    true
}

/// Hash-based multiset comparison for large lists.
/// Groups elements by hash, then does quadratic matching only within same-hash buckets.
/// Average case O(n), worst case O(n^2) on hash collisions.
fn lists_equal_hashed(a: &[Value], b: &[Value]) -> bool {
    // Build hash buckets for b
    let mut b_buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (j, item) in b.iter().enumerate() {
        b_buckets.entry(item.canonical_hash()).or_default().push(j);
    }

    let mut matched = vec![false; b.len()];
    for item_a in a {
        let hash = item_a.canonical_hash();
        let mut found = false;
        if let Some(bucket) = b_buckets.get(&hash) {
            for &j in bucket {
                if !matched[j] && item_a.semantically_equal(&b[j]) {
                    matched[j] = true;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return false;
        }
    }
    true
}

/// Merge desired value with saved state to fill in unmanaged nested fields.
/// For Maps: start with saved, overlay desired fields on top (desired wins).
/// For Lists: match elements by similarity, merge each pair.
/// For cross-type Map/List([Map]): unwrap the single-element list and merge as Maps.
/// For other types: return desired as-is.
pub fn merge_with_saved(desired: &Value, saved: &Value) -> Value {
    match (desired, saved) {
        (
            Value::Concrete(ConcreteValue::Map(desired_map)),
            Value::Concrete(ConcreteValue::Map(saved_map)),
        ) => {
            let mut merged = saved_map.clone();
            for (k, v) in desired_map {
                let merged_v = if let Some(saved_v) = saved_map.get(k) {
                    merge_with_saved(v, saved_v)
                } else {
                    v.clone()
                };
                merged.insert(k.clone(), merged_v);
            }
            Value::Concrete(ConcreteValue::Map(merged))
        }
        (
            Value::Concrete(ConcreteValue::List(desired_list)),
            Value::Concrete(ConcreteValue::List(saved_list)),
        ) => Value::Concrete(ConcreteValue::List(merge_lists(desired_list, saved_list))),
        _ => desired.clone(),
    }
}

/// Merge two lists by pairing elements via similarity score, then merging each pair.
///
/// For large lists, uses hash-based bucketing to narrow candidate matches.
/// For small lists, uses the simple O(n^2) scan.
fn merge_lists(desired: &[Value], saved: &[Value]) -> Vec<Value> {
    if desired.is_empty() {
        return desired.to_vec();
    }
    if saved.len() < HASH_THRESHOLD {
        return merge_lists_quadratic(desired, saved);
    }
    merge_lists_hashed(desired, saved)
}

/// O(n^2) merge for small lists.
fn merge_lists_quadratic(desired: &[Value], saved: &[Value]) -> Vec<Value> {
    let mut used = vec![false; saved.len()];
    let mut result = Vec::with_capacity(desired.len());

    for d in desired {
        let mut best_idx = None;
        let mut best_score = 0;

        for (j, s) in saved.iter().enumerate() {
            if used[j] {
                continue;
            }
            let score = similarity_score(d, s);
            if score > best_score {
                best_score = score;
                best_idx = Some(j);
            }
        }

        if let Some(idx) = best_idx {
            used[idx] = true;
            result.push(merge_with_saved(d, &saved[idx]));
        } else {
            result.push(d.clone());
        }
    }

    result
}

/// Hash-based merge for large lists.
/// For Map values, tries exact hash match first, then falls back to scanning
/// same-discriminant elements for best similarity. For non-Map values, uses
/// hash bucketing for O(1) lookup.
fn merge_lists_hashed(desired: &[Value], saved: &[Value]) -> Vec<Value> {
    // Build hash buckets for saved elements
    let mut saved_buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (j, item) in saved.iter().enumerate() {
        saved_buckets
            .entry(item.canonical_hash())
            .or_default()
            .push(j);
    }

    let mut used = vec![false; saved.len()];
    let mut result = Vec::with_capacity(desired.len());

    for d in desired {
        let hash = d.canonical_hash();
        let mut best_idx = None;
        let mut best_score = 0;

        // First, check exact hash matches (most common case for identical elements)
        if let Some(bucket) = saved_buckets.get(&hash) {
            for &j in bucket {
                if used[j] {
                    continue;
                }
                let score = similarity_score(d, &saved[j]);
                if score > best_score {
                    best_score = score;
                    best_idx = Some(j);
                }
            }
        }

        // For Maps, also check other saved Maps for partial matches
        // (a Map may have extra fields from saved state, giving a different hash)
        if matches!(d, Value::Concrete(ConcreteValue::Map(_))) {
            for (j, s) in saved.iter().enumerate() {
                if used[j] || matches!(best_idx, Some(bi) if bi == j) {
                    continue;
                }
                if !matches!(s, Value::Concrete(ConcreteValue::Map(_))) {
                    continue;
                }
                let score = similarity_score(d, s);
                if score > best_score {
                    best_score = score;
                    best_idx = Some(j);
                }
            }
        }

        if let Some(idx) = best_idx {
            used[idx] = true;
            result.push(merge_with_saved(d, &saved[idx]));
        } else {
            result.push(d.clone());
        }
    }

    result
}

/// Compute a similarity score between two Values.
/// For Maps: count matching key-value pairs.
/// For equal scalars: return 1.
/// Otherwise: return 0.
fn similarity_score(a: &Value, b: &Value) -> usize {
    match (a, b) {
        (Value::Concrete(ConcreteValue::Map(am)), Value::Concrete(ConcreteValue::Map(bm))) => am
            .iter()
            .filter(|(k, v)| {
                bm.get(*k)
                    .map(|bv| v.semantically_equal(bv))
                    .unwrap_or(false)
            })
            .count(),
        _ => {
            if a.semantically_equal(b) {
                1
            } else {
                0
            }
        }
    }
}

/// Carina-side directives for a resource.
///
/// These are instructions to Carina about how to handle the resource,
/// not metadata about the resource itself. Mirrors the WIT
/// `directives` record.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Directives {
    /// If true, force-delete the resource (e.g., non-empty S3 buckets)
    #[serde(default)]
    pub force_delete: bool,
    /// If true, create the new resource before destroying the old one during replacement
    #[serde(default)]
    pub create_before_destroy: bool,
    /// If true, prevent the resource from being destroyed
    #[serde(default)]
    pub prevent_destroy: bool,
    /// Explicit ordering edges declared by the user. Each element is the
    /// binding name of a sibling `let` (resource / wait / module).
    /// Set semantics (deduplicated, order-insensitive); represented as
    /// Vec to preserve source order for `carina fmt` round-tripping.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

/// Source of a resource (root or from a module)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModuleSource {
    /// Resource defined at the root level
    Root,
    /// Resource from a module instantiation
    Module {
        /// Module name (e.g., "web_tier")
        name: String,
        /// Instance binding name (e.g., "web")
        instance: String,
    },
}

impl ModuleSource {
    /// Create a Module source
    pub fn module(name: impl Into<String>, instance: impl Into<String>) -> Self {
        Self::Module {
            name: name.into(),
            instance: instance.into(),
        }
    }

    /// Check if this is the root source
    pub fn is_root(&self) -> bool {
        matches!(self, Self::Root)
    }
}

/// Classification of a resource in the IR
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum ResourceKind {
    /// A managed infrastructure resource with full CRUD lifecycle.
    #[default]
    Managed,
    /// A virtual resource created by the module resolver to expose module attributes.
    /// Virtual resources are not sent to providers; they exist only in the IR.
    Virtual {
        module_name: String,
        instance: String,
    },
    /// A data source (read-only) that is queried but not managed
    DataSource,
}

/// Desired state declared in DSL
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    /// Source-order preserving map of attribute name → expression.
    ///
    /// `IndexMap` (not `HashMap`) so iteration order matches the order
    /// the user wrote attributes in the `.crn` file. Anything that
    /// re-renders attributes — diagnostic messages, formatter output,
    /// plan display, snapshot tests — depends on this stability (#2222).
    pub attributes: IndexMap<String, Value>,
    /// Classification of this resource (managed, virtual, or data source)
    #[serde(default)]
    pub kind: ResourceKind,
    /// `directives` meta-argument block: Carina-side instructions for
    /// how to handle this resource (force-delete, create-before-destroy,
    /// prevent-destroy).
    #[serde(default)]
    pub directives: Directives,
    /// Attribute prefixes: maps attribute name -> prefix string
    /// e.g., {"bucket_name": "my-app-"} from `bucket_name_prefix = "my-app-"`
    #[serde(default)]
    pub prefixes: HashMap<String, String>,
    /// Binding name from `let` bindings in DSL (e.g., `let vpc = ...`)
    #[serde(default)]
    pub binding: Option<String>,
    /// Binding names of resources this resource depends on (via ResourceRef).
    ///
    /// Set semantics (BTreeSet): the same binding referenced multiple times
    /// in the resource's attributes contributes a single entry, and
    /// iteration is alphabetically sorted so consumers (plan display,
    /// state files) see a deterministic order. See #2228.
    #[serde(default)]
    pub dependency_bindings: BTreeSet<String>,
    /// Module source info for resources that belong to a module
    #[serde(default)]
    pub module_source: Option<ModuleSource>,
    /// Top-level attribute names whose value was written as a quoted
    /// string literal (`attr = "..."`) in the source `.crn`.
    ///
    /// **Why on `Resource`, not on `Value`:** the alternative is a
    /// `Value::QuotedString` variant, but that ripples through every
    /// `match` arm in the codebase. Co-locating the bit with the
    /// owning resource is enough for the only consumer that needs it
    /// (enum-attribute diagnostics — see #2094) without that blast
    /// radius. Sharing a struct with the attributes also makes the
    /// lookup rename-proof: there is no separate identifier keying
    /// the metadata, so `compute_anonymous_identifiers` can rewrite
    /// `Resource.id.name` freely (#2229).
    ///
    /// Parse-time only; `#[serde(skip)]` keeps it out of state.
    #[serde(default, skip)]
    pub quoted_string_attrs: HashSet<String>,
}

impl Resource {
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: ResourceId::new(resource_type, name),
            attributes: IndexMap::new(),
            kind: ResourceKind::Managed,
            directives: Directives::default(),
            prefixes: HashMap::new(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }

    pub fn with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            id: ResourceId::with_provider(provider, resource_type, name),
            attributes: IndexMap::new(),
            kind: ResourceKind::Managed,
            directives: Directives::default(),
            prefixes: HashMap::new(),
            binding: None,
            dependency_bindings: BTreeSet::new(),
            module_source: None,
            quoted_string_attrs: HashSet::new(),
        }
    }

    /// Returns the attributes projected to a `HashMap<String, Value>`.
    ///
    /// Lookup-only callers (validation, differ, plan display) still
    /// receive a `HashMap` — iteration over user-authored order is done
    /// directly via `self.attributes` (`IndexMap`), so flipping this
    /// helper would just force every downstream caller to widen its
    /// signature for no order benefit.
    pub fn resolved_attributes(&self) -> HashMap<String, Value> {
        attrs_to_hashmap(&self.attributes)
    }

    /// Get an attribute value by key, returning `Option<&Value>`.
    pub fn get_attr(&self, key: &str) -> Option<&Value> {
        self.attributes.get(key)
    }

    /// Get a mutable attribute value by key, returning `Option<&mut Value>`.
    pub fn get_attr_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.attributes.get_mut(key)
    }

    /// Set an attribute value.
    pub fn set_attr(&mut self, key: impl Into<String>, value: Value) {
        self.attributes.insert(key.into(), value);
    }

    pub fn with_attribute(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }

    /// Set attributes from a `HashMap<String, Value>`.
    pub fn with_value_attributes(mut self, attrs: HashMap<String, Value>) -> Self {
        self.attributes = attrs.into_iter().collect();
        self
    }

    pub fn with_read_only(mut self, read_only: bool) -> Self {
        if read_only {
            self.kind = ResourceKind::DataSource;
        }
        self
    }

    pub fn with_kind(mut self, kind: ResourceKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_binding(mut self, binding: impl Into<String>) -> Self {
        self.binding = Some(binding.into());
        self
    }

    pub fn with_dependency_bindings(mut self, deps: BTreeSet<String>) -> Self {
        self.dependency_bindings = deps;
        self
    }

    pub fn with_module_source(mut self, source: ModuleSource) -> Self {
        self.module_source = Some(source);
        self
    }

    /// Returns true if this resource is a data source (read-only)
    pub fn is_data_source(&self) -> bool {
        matches!(self.kind, ResourceKind::DataSource)
    }

    /// Returns true if this resource is a virtual resource (module attribute container).
    ///
    /// Virtual resources are created by the module resolver to expose module
    /// `attributes` values as a structured record. They should not be sent to
    /// providers for reading, creating, or updating.
    pub fn is_virtual(&self) -> bool {
        matches!(self.kind, ResourceKind::Virtual { .. })
    }
}

/// Current state fetched from actual infrastructure
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub id: ResourceId,
    /// AWS internal identifier (e.g., vpc-xxx, subnet-xxx)
    pub identifier: Option<String>,
    pub attributes: HashMap<String, Value>,
    /// Whether this state exists
    pub exists: bool,
    /// Binding names this resource depended on when it was last applied.
    /// Used by the executor to determine delete ordering during replace operations.
    ///
    /// Set semantics (BTreeSet) — see Resource::dependency_bindings (#2228).
    pub dependency_bindings: BTreeSet<String>,
}

impl State {
    pub fn not_found(id: ResourceId) -> Self {
        Self {
            id,
            identifier: None,
            attributes: HashMap::new(),
            exists: false,
            dependency_bindings: BTreeSet::new(),
        }
    }

    pub fn existing(id: ResourceId, attributes: HashMap<String, Value>) -> Self {
        Self {
            id,
            identifier: None,
            attributes,
            exists: true,
            dependency_bindings: BTreeSet::new(),
        }
    }

    pub fn with_identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
        self
    }

    pub fn with_dependency_bindings(mut self, deps: BTreeSet<String>) -> Self {
        self.dependency_bindings = deps;
        self
    }
}

#[cfg(test)]
mod tests;
