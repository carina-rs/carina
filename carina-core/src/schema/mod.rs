//! Schema - Define type schemas for resources
//!
//! Providers define schemas for each resource type,
//! enabling type validation at parse time.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::resource::{ConcreteValue, ConcreteValueRef, DeferredValue, Resource, Value};
use crate::utils::{extract_enum_value_with_values, validate_enum_namespace};
use crate::value::format_value_with_key;

mod resolved_attr_type;
mod type_identity;

pub use carina_provider_protocol::types::DslTransform;
pub use resolved_attr_type::ResolvedAttrType;
pub use type_identity::TypeIdentity;

/// Error returned when a bare projection reaches a schema-bound
/// [`AttrTypeKind::Ref`] without a defs map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEncountered {
    pub name: String,
}

impl fmt::Display for RefEncountered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unresolved AttributeType::Ref(\"{}\") has no defs in scope",
            self.name
        )
    }
}

/// Type alias for resource validator functions
pub type ResourceValidator = fn(&HashMap<String, Value>) -> Result<(), Vec<TypeError>>;

/// Validator stored on refined primitive/list shapes. Boxed as `Arc<dyn Fn>` so it
/// can capture provider-side state (region, account ID, schema handles) and
/// return a structured `TypeError` directly — both of which a bare `fn`
/// pointer cannot do. See #2217.
pub type CustomValidator = Arc<dyn Fn(&Value) -> Result<(), TypeError> + Send + Sync>;

/// Build a [`CustomValidator`] from any closure that returns a structured
/// [`TypeError`]. This is the preferred constructor: validators that emit
/// `TypeError::InvalidEnumVariant` (or other structured variants) flow
/// straight into the LSP's quick-fix path.
pub fn validator<F>(f: F) -> CustomValidator
where
    F: Fn(&Value) -> Result<(), TypeError> + Send + Sync + 'static,
{
    Arc::new(f)
}

fn noop_validator() -> CustomValidator {
    validator(|_| Ok(()))
}

/// Build a [`CustomValidator`] from a closure that still uses the legacy
/// `Result<(), String>` shape. The returned message is wrapped in
/// `TypeError::ValidationFailed`. Use this for builtins that haven't yet
/// been migrated to structured errors.
pub fn legacy_validator<F>(f: F) -> CustomValidator
where
    F: Fn(&Value) -> Result<(), String> + Send + Sync + 'static,
{
    Arc::new(move |v| f(v).map_err(|message| TypeError::ValidationFailed { message }))
}

/// External validator looked up by refined primitive identity.
/// at validation time. Used to bridge to provider-supplied validators
/// (the `ProviderContext.validators` map and WASM factory's
/// `validate_custom_type`) that the schema itself cannot carry across
/// the WASM boundary — see #2354.
///
/// The lookup is keyed on a structured [`TypeIdentity`], not a flat
/// type-name string, so two providers' same-named custom types resolve
/// to distinct validators instead of colliding first-wins.
pub type CustomTypeLookup<'a> =
    &'a (dyn Fn(&TypeIdentity, &Value) -> Result<(), TypeError> + Send + Sync + 'a);

/// A [`CustomTypeLookup`] that approves every value. Pass to
/// `validate_with_origins_and_lookup` from contexts that have no
/// `ProviderContext` (snapshot tests, schema unit tests). The
/// schema-attached `validate` closure still runs for built-in
/// validators registered directly on the type.
pub fn no_lookup() -> CustomTypeLookup<'static> {
    &|_name, _value| Ok(())
}

/// If `value` is a bare reference to a `let` binding (no access path,
/// no attribute selector), return the binding name. Otherwise return
/// `None`. Used by `AttrTypeKind::validate` to detect the collision
/// case from #2978: a Enum attribute receives a bare identifier
/// that the parser already resolved as a binding reference rather than
/// as the enum's DSL alias of the same spelling.
fn bare_binding_name(value: &Value) -> Option<&str> {
    match value {
        Value::Deferred(DeferredValue::BindingRef { binding }) => Some(binding.as_str()),
        _ => None,
    }
}

/// True when `binding` matches a canonical enum value or a DSL alias
/// of `(values, dsl_aliases)`. Drives the #2978 collision check.
fn enum_alias_collides_with(
    binding: &str,
    values: &[String],
    dsl_aliases: &[(String, String)],
) -> bool {
    if values.iter().any(|v| v == binding) {
        return true;
    }
    dsl_aliases.iter().any(|(_api, dsl)| dsl == binding)
}

/// Build the user-facing error message for the #2978 collision case.
fn enum_binding_collision_message(
    binding: &str,
    type_name: &str,
    namespace: Option<&str>,
    values: &[String],
    dsl_aliases: &[(String, String)],
) -> String {
    // Prefer the DSL spelling if one exists for the colliding API value;
    // that is the form the user almost certainly meant to write.
    let dsl_spelling = dsl_aliases
        .iter()
        .find(|(_api, dsl)| dsl == binding)
        .map(|(_api, dsl)| dsl.as_str())
        .or_else(|| values.iter().find(|v| *v == binding).map(String::as_str))
        .unwrap_or(binding);
    let type_qualified = format!("{}.{}", type_name, dsl_spelling);
    let fully_qualified = match namespace {
        Some(ns) => format!("{}.{}.{}", ns, type_name, dsl_spelling),
        None => type_qualified.clone(),
    };
    format!(
        "bare identifier `{binding}` is shadowed by a `let` binding of the same name; \
         to use the enum value, write `{type_qualified}`, `{fully_qualified}`, or `'{dsl_spelling}'`"
    )
}

/// Walk an [`AttributeType`] and apply `lookup` to every identified refined node
/// reached. Pushes any returned error into `errors`, tagged with
/// `attr_name` so it points back at the user-visible attribute. Used
/// by `ResourceSchema::validate_inner` to bridge provider-supplied
/// validators that the schema's own closure cannot carry (e.g. WASM
/// plugins, where the real validator lives behind the factory's
/// `validate_custom_type`).
fn walk_custom_lookup(
    attr_type: &AttributeType,
    value: &Value,
    attr_name: &str,
    lookup: CustomTypeLookup<'_>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    errors: &mut Vec<TypeError>,
) {
    // Skip deferred-resolution values — same convention as
    // `AttrTypeKind::validate`, plus `ResourceRef` / `Interpolation`
    // which only resolve to a concrete string at apply time.
    if matches!(
        value,
        Value::Deferred(DeferredValue::FunctionCall { .. })
            | Value::Deferred(DeferredValue::Secret(_))
            | Value::Deferred(DeferredValue::ResourceRef { .. })
            | Value::Deferred(DeferredValue::Interpolation(_))
            | Value::Deferred(DeferredValue::Unknown(_))
    ) {
        return;
    }
    match &attr_type.kind {
        AttrTypeKind::String { identity, .. }
        | AttrTypeKind::Int { identity, .. }
        | AttrTypeKind::Float { identity, .. } => {
            run_identified_lookup(identity.as_ref(), value, attr_name, lookup, errors)
        }
        AttrTypeKind::Enum { identity, .. } => {
            if let Err(e) = lookup(identity, value) {
                let message = match e {
                    TypeError::ValidationFailed { message } => message,
                    other => other.to_string(),
                };
                errors.push(TypeError::ResourceValidationFailed {
                    message,
                    attribute: Some(attr_name.to_string()),
                });
            }
        }
        AttrTypeKind::List {
            element_type: inner,
            ..
        } => {
            if let Value::Concrete(ConcreteValue::List(items)) = value {
                for item in items {
                    walk_custom_lookup(inner, item, attr_name, lookup, defs, errors);
                }
            }
        }
        AttrTypeKind::Map { value: inner, .. } => {
            if let Value::Concrete(ConcreteValue::Map(map)) = value {
                for v in map.values() {
                    walk_custom_lookup(inner, v, attr_name, lookup, defs, errors);
                }
            }
        }
        AttrTypeKind::Struct { fields, .. } => {
            if let Value::Concrete(ConcreteValue::Map(map)) = value {
                for f in fields {
                    if let Some(v) = map.get(&f.name) {
                        walk_custom_lookup(&f.field_type, v, attr_name, lookup, defs, errors);
                    }
                }
            }
        }
        AttrTypeKind::Union(members) => {
            // Union semantics: a value is valid if *any* member accepts
            // it. The previous loop walked every member and pushed
            // every error, so a value that legitimately matched one arm
            // (e.g. an IPv4 CIDR like `"10.0.0.0/8"` matching
            // `ipv4_cidr()` inside `types::cidr()`) still surfaced the
            // failure from the sibling IPv6 arm's validator. See
            // awscc#217.
            //
            // Walk every member into a local buffer; if any member
            // emits no errors the Union succeeds and the sibling
            // failures are discarded. If every member fails, keep the
            // smallest error set so the user-facing diagnostic stays
            // pointed at the closest near-match.
            let mut best: Option<Vec<TypeError>> = None;
            for member in members {
                let mut local = Vec::new();
                walk_custom_lookup(member, value, attr_name, lookup, defs, &mut local);
                if local.is_empty() {
                    return;
                }
                if best.as_ref().is_none_or(|b| local.len() < b.len()) {
                    best = Some(local);
                }
            }
            if let Some(b) = best {
                errors.extend(b);
            }
        }
        AttrTypeKind::Bool | AttrTypeKind::Duration => {}
        // `Ref`: resolve via the schema's def map and continue the
        // walk. The resolved target (typically a `Struct`) may carry
        // identity-bearing custom types whose validators must run.
        // A missing def name is reported by `Schema::validate_attr`
        // as `ValidationFailed`; here we just skip the custom-lookup
        // walk so a user-facing dangling-Ref does not abort the
        // process (carina#3345).
        AttrTypeKind::Ref(name) => match defs.get(name) {
            Some(resolved) => {
                walk_custom_lookup(resolved, value, attr_name, lookup, defs, errors);
            }
            None => {
                // Skip — the `validate_attr` pass already emitted
                // the dangling-Ref diagnostic for this attribute.
            }
        },
    }
}

fn run_identified_lookup(
    identity: Option<&TypeIdentity>,
    value: &Value,
    attr_name: &str,
    lookup: CustomTypeLookup<'_>,
    errors: &mut Vec<TypeError>,
) {
    if let Some(id) = identity
        && let Err(e) = lookup(id, value)
    {
        let message = match e {
            TypeError::ValidationFailed { message } => message,
            other => other.to_string(),
        };
        errors.push(TypeError::ResourceValidationFailed {
            message,
            attribute: Some(attr_name.to_string()),
        });
    }
}

pub type EnumParts<'a> = (
    &'a TypeIdentity,
    Option<&'a [String]>,
    &'a [(String, String)],
    Option<&'a CustomValidator>,
    DslMap<'a>,
);

/// API-canonical ↔ DSL-spelling mapping carried by an `Enum`.
///
/// Alias data and transform callbacks are consulted by the same object
/// so enum consumers do not rebuild separate closed-vs-dynamic code
/// paths. Alias entries take precedence; the transform is the fallback
/// for open value spaces such as regions and availability zones.
#[derive(Clone, Copy)]
pub struct DslMap<'a> {
    aliases: &'a [(String, String)],
    to_dsl: Option<&'a DslTransform>,
}

impl<'a> DslMap<'a> {
    pub fn new(aliases: &'a [(String, String)], to_dsl: Option<&'a DslTransform>) -> Self {
        Self { aliases, to_dsl }
    }

    /// Translate an API spelling to its DSL spelling. Returns the
    /// input unchanged when no mapping applies.
    pub fn dsl_for<'b>(&self, api: &'b str) -> Cow<'b, str> {
        self.aliases
            .iter()
            .find_map(|(a, d)| (a == api).then(|| Cow::Owned(d.clone())))
            .unwrap_or_else(|| {
                self.to_dsl
                    .map_or_else(|| Cow::Borrowed(api), |transform| transform.apply(api))
            })
    }

    /// Translate a DSL spelling back to its API-canonical spelling.
    /// Returns the input unchanged when no mapping applies.
    ///
    /// Mirror of [`dsl_for`]. Used by providers that have a DSL-spelled
    /// enum value in hand and need the API-canonical form to feed into
    /// an SDK builder. Without this, hand-written provider code that
    /// extracts the trailing segment of a namespaced identifier passes
    /// the DSL alias (e.g. `"enabled"`) straight to the SDK and the API
    /// rejects it (e.g. S3 `MalformedXML`).
    ///
    /// When two `(api, dsl)` entries share the same `dsl` spelling, the
    /// first match wins. Codegen is expected to ensure DSL spellings are
    /// unique within a single enum's alias table.
    pub fn api_for(&self, dsl: &str) -> String {
        self.aliases
            .iter()
            .find_map(|(a, d)| (d == dsl).then(|| a.clone()))
            .unwrap_or_else(|| dsl.to_string())
    }

    /// True only when the mapping carries no DSL-side rewrite
    /// machinery: no alias-table entries and no transform callback.
    ///
    /// Dynamic enums normally have an empty alias table plus a
    /// transform callback, so this returns `false` for them even
    /// though `aliases` itself is empty.
    pub fn is_empty(&self) -> bool {
        self.aliases.is_empty() && self.to_dsl.is_none()
    }
}

/// A field within a Struct type
#[derive(Debug, Clone)]
pub struct StructField {
    /// Field name (snake_case, e.g., "ip_protocol")
    pub name: String,
    /// Field type
    pub field_type: AttributeType,
    /// Whether this field is required
    pub required: bool,
    /// Description of this field
    pub description: Option<String>,
    /// Provider-side property name (e.g., "IpProtocol")
    pub provider_name: Option<String>,
    /// Alternative block name for repeated block syntax (e.g., "transition" for "transitions")
    pub block_name: Option<String>,
    /// Whether the value of this nested field is populated by the
    /// provider asynchronously *after* the Create call returns.
    /// Reached when a chained access traverses a `Struct` (e.g.
    /// `cert.domain_validation_options[0].resource_record_value` —
    /// the inner struct field is the deferred-populate one, not the
    /// outer list attribute). See `AttributeSchema::deferred_populate`
    /// (carina#3034).
    pub deferred_populate: bool,
}

impl StructField {
    pub fn new(name: impl Into<String>, field_type: AttributeType) -> Self {
        Self {
            name: name.into(),
            field_type,
            required: false,
            description: None,
            provider_name: None,
            block_name: None,
            deferred_populate: false,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = Some(name.into());
        self
    }

    pub fn with_block_name(mut self, name: impl Into<String>) -> Self {
        self.block_name = Some(name.into());
        self
    }

    /// Mark this nested field as populated asynchronously by the
    /// provider after Create. See the field doc on `deferred_populate`.
    pub fn deferred_populate(mut self) -> Self {
        self.deferred_populate = true;
        self
    }
}

/// Attribute type — opaque public type wrapping an internal
/// [`AttrTypeKind`] enum. External code constructs values via the
/// `pub` constructors (`AttrTypeKind::string()`, `::list(...)`,
/// `::ref_(...)`, etc.) and inspects via [`AttrTypeKind::shape`] (or,
/// in-crate, [`AttrTypeKind::kind`]).
///
/// The inner enum is `pub(crate)`, so callers outside `carina-core`
/// cannot pattern-match a raw `&AttributeType` against the
/// [`AttrTypeKind::Ref`] variant — the carina#3349 bug class
/// (silent `Ref` drop in a wildcard arm of an external `match`) is
/// structurally impossible at the type level.
#[derive(Clone)]
pub struct AttributeType {
    pub(crate) kind: AttrTypeKind,
}

/// Internal variant enum carried by [`AttributeType`]. `pub(crate)` so
/// it is visible to every file inside `carina-core` (validation, value
/// canonicalization, differ, detail rows, etc.) but hidden from
/// downstream crates. External code uses [`AttrTypeKind::shape`] (the
/// `Ref`-peeled view) or the constructor methods on [`AttributeType`].
#[derive(Clone)]
pub(crate) enum AttrTypeKind {
    /// String
    String {
        identity: Option<TypeIdentity>,
        pattern: Option<String>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
        to_dsl: Option<DslTransform>,
    },
    /// Integer
    Int {
        identity: Option<TypeIdentity>,
        range: Option<(Option<i64>, Option<i64>)>,
        validate: CustomValidator,
    },
    /// Floating-point number
    Float {
        identity: Option<TypeIdentity>,
        range: Option<(Option<f64>, Option<f64>)>,
        validate: CustomValidator,
    },
    /// Boolean
    Bool,
    /// Time duration. Values use the `<integer><unit>` literal
    /// (`75min`, `1h`, `30s`); internally a `std::time::Duration`.
    /// Serialised as integer seconds at every value-tree boundary.
    Duration,
    /// Namespaced enum with DSL shorthand support.
    Enum {
        /// Structured identity. Mandatory so every enum has a stable
        /// namespace/type name for validation, lifting, and diagnostics.
        identity: TypeIdentity,
        /// Underlying value shape carried by provider protocol enum
        /// declarations. Most enum values are strings, but preserving
        /// the base keeps the WASM wire form's type evidence available
        /// after conversion into the unified core enum.
        base: Box<AttributeType>,
        /// Closed API values when the provider can enumerate them.
        /// `None` keeps host-side validator semantics for dynamic enum
        /// spaces such as regions and availability zones.
        values: Option<Vec<String>>,
        /// API → DSL spelling aliases for closed enums.
        dsl_aliases: Vec<(String, String)>,
        validate: Option<CustomValidator>,
        /// Optional API → DSL transform for dynamic enum spaces.
        to_dsl: Option<DslTransform>,
    },
    /// List
    /// `ordered`: if true, element order matters (sequential comparison);
    /// if false, order is ignored (multiset comparison).
    /// Defaults to true (matching CloudFormation's insertionOrder default).
    List {
        element_type: Box<AttributeType>,
        ordered: bool,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
    },
    /// Map with typed keys and values.
    /// `key`: type constraint for map keys (e.g., `String` for unconstrained,
    /// `Enum` for condition operators).
    /// `value`: type of map values.
    Map {
        key: Box<AttributeType>,
        value: Box<AttributeType>,
    },
    /// Struct (named object with typed fields)
    Struct {
        name: String,
        fields: Vec<StructField>,
    },
    /// Union of multiple types (value is valid if it matches any member)
    Union(Vec<AttributeType>),
    /// Named reference into the enclosing [`Schema`]'s definition map.
    ///
    /// Used to express cyclic struct definitions
    /// (e.g. `Statement -> AndStatement -> List<Statement>` in
    /// `AWS::WAFv2::WebACL`) without recursing at codegen time.
    /// Resolved lazily by [`Schema::validate`] at the point of use.
    ///
    /// A `Ref` outside of a `Schema` context cannot be self-validated —
    /// [`AttrTypeKind::validate`] returns a `TypeError::ValidationFailed`
    /// in that case. Callers who need to validate a schema that contains
    /// `Ref` must go through [`Schema::validate`].
    Ref(String),
}

/// A view over an [`AttributeType`] with **every top-level `Ref` peeled**.
///
/// Returned by [`AttrTypeKind::shape`]. The enum intentionally omits a
/// `Ref` variant, so a `match shape { ... }` with a wildcard arm is
/// type-system-safe — there is no path that lets a `Ref` reach the
/// match. This is the carina#3349 invariant lifted into the type system:
/// every walk-site that previously matched on a raw `&AttributeType` and
/// could silently drop `Ref` in a wildcard arm now matches on a
/// `Shape<'a>` instead, and the bug class is structurally impossible.
///
/// All variants carry borrowed data, so constructing a `Shape` is a
/// cheap reborrow plus a `Ref` chain walk; the lifetime is tied to the
/// `AttributeType` plus the `defs` map passed to `shape`.
///
/// Mirrors [`AttributeType`] variant-for-variant except for `Ref`. New
/// `AttributeType` variants must be added here as well; the
/// `AttrTypeKind::shape` match is non-exhaustive against the source
/// enum and the compiler will surface the omission.
#[derive(Clone, Copy)]
pub enum Shape<'a> {
    /// String — see [`AttrTypeKind::String { .. }`].
    String {
        identity: Option<&'a TypeIdentity>,
        pattern: Option<&'a str>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: &'a CustomValidator,
        to_dsl: Option<&'a DslTransform>,
    },
    /// Integer — see [`AttrTypeKind::Int { .. }`].
    Int {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<i64>, Option<i64>)>,
        validate: &'a CustomValidator,
    },
    /// Floating-point — see [`AttrTypeKind::Float { .. }`].
    Float {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<f64>, Option<f64>)>,
        validate: &'a CustomValidator,
    },
    /// Boolean — see [`AttrTypeKind::Bool`].
    Bool,
    /// Time duration — see [`AttrTypeKind::Duration`].
    Duration,
    /// Namespaced enum — see [`AttrTypeKind::Enum`].
    Enum {
        identity: &'a TypeIdentity,
        base: &'a AttributeType,
        values: Option<&'a [String]>,
        dsl_aliases: &'a [(String, String)],
        validate: Option<&'a CustomValidator>,
        to_dsl: Option<&'a DslTransform>,
    },
    /// List with element type and ordering — see [`AttrTypeKind::List`].
    List {
        element_type: &'a AttributeType,
        ordered: bool,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: &'a CustomValidator,
    },
    /// Map with typed key/value — see [`AttrTypeKind::Map`].
    Map {
        key: &'a AttributeType,
        value: &'a AttributeType,
    },
    /// Named struct — see [`AttrTypeKind::Struct`].
    Struct { name: &'a str },
    /// Union of types — see [`AttrTypeKind::Union`].
    Union,
    // Intentionally NO Ref variant — see type-level docs.
}

/// Explicit traversal budget for public schema accessors that expose
/// nested struct fields or union members.
///
/// The public [`Shape`] view classifies `Struct` and `Union` without
/// handing out iterable branch lists. Callers outside `carina-core` that
/// genuinely need to walk those branches must carry this budget, making
/// an unbounded schema graph DFS unrepresentable by the accessor
/// signatures.
#[derive(Debug, Clone)]
pub struct ShapeWalkBudget {
    remaining: usize,
}

impl ShapeWalkBudget {
    pub fn new(max_steps: usize) -> Self {
        Self {
            remaining: max_steps,
        }
    }

    fn take(&mut self) -> bool {
        let Some(next) = self.remaining.checked_sub(1) else {
            return false;
        };
        self.remaining = next;
        true
    }
}

impl fmt::Debug for Shape<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Shape::String {
                identity,
                pattern,
                length,
                validate: _,
                to_dsl: _,
            } => f
                .debug_struct("Shape::String { .. }")
                .field("identity", identity)
                .field("pattern", pattern)
                .field("length", length)
                .finish_non_exhaustive(),
            Shape::Int {
                identity,
                range,
                validate: _,
            } => f
                .debug_struct("Shape::Int { .. }")
                .field("identity", identity)
                .field("range", range)
                .finish_non_exhaustive(),
            Shape::Float {
                identity,
                range,
                validate: _,
            } => f
                .debug_struct("Shape::Float { .. }")
                .field("identity", identity)
                .field("range", range)
                .finish_non_exhaustive(),
            Shape::Bool => f.write_str("Shape::Bool"),
            Shape::Duration => f.write_str("Shape::Duration"),
            Shape::Enum {
                identity,
                base,
                values,
                dsl_aliases,
                validate: _,
                to_dsl: _,
            } => f
                .debug_struct("Shape::Enum")
                .field("identity", identity)
                .field("base", base)
                .field("values", values)
                .field("dsl_aliases", dsl_aliases)
                .finish_non_exhaustive(),
            Shape::List {
                element_type,
                ordered,
                length,
                validate: _,
            } => f
                .debug_struct("Shape::List")
                .field("element_type", element_type)
                .field("ordered", ordered)
                .field("length", length)
                .finish_non_exhaustive(),
            Shape::Map { key, value } => f
                .debug_struct("Shape::Map")
                .field("key", key)
                .field("value", value)
                .finish(),
            Shape::Struct { name } => f.debug_struct("Shape::Struct").field("name", name).finish(),
            Shape::Union => f.debug_tuple("Shape::Union").finish(),
        }
    }
}

/// A `Ref`-preserving projection of [`AttributeType`] for callers that
/// must round-trip the type *across a boundary* without resolving
/// `Ref` against `defs` (typically: WIT/JSON serializers in WASM
/// plugin guests that emit a [`crate::schema`] schema to the host
/// alongside a `defs` map of its own).
///
/// [`Shape`] is the right view for **walk-sites** (differ, detail_rows,
/// LSP, validation): it resolves `Ref` against the enclosing
/// [`Schema`]'s `defs` map and is structurally guaranteed not to carry
/// `Ref`. [`RawShape`] is the right view for **transport-sites**: the
/// caller is not consuming the schema's meaning, only relaying its
/// structural form, and resolving `Ref` would either (a) infinite-loop
/// on cyclic schemas like WAFv2 `WebACL.Statement`, or (b) flatten the
/// `defs` map into the root and lose the cyclic structure the receiver
/// needs to re-build.
///
/// Mirrors [`Shape`] variant-for-variant **plus** a [`RawShape::Ref`]
/// variant. New `AttributeType` variants must be added to both
/// projections; the `AttrTypeKind::raw_shape` match is non-exhaustive
/// against the source enum and the compiler will surface the omission.
#[derive(Clone, Copy)]
pub enum RawShape<'a> {
    /// String — see [`AttrTypeKind::String { .. }`].
    String {
        identity: Option<&'a TypeIdentity>,
        pattern: Option<&'a str>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: &'a CustomValidator,
        to_dsl: Option<&'a DslTransform>,
    },
    /// Integer — see [`AttrTypeKind::Int { .. }`].
    Int {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<i64>, Option<i64>)>,
        validate: &'a CustomValidator,
    },
    /// Floating-point — see [`AttrTypeKind::Float { .. }`].
    Float {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<f64>, Option<f64>)>,
        validate: &'a CustomValidator,
    },
    /// Boolean — see [`AttrTypeKind::Bool`].
    Bool,
    /// Time duration — see [`AttrTypeKind::Duration`].
    Duration,
    /// Namespaced enum — see [`AttrTypeKind::Enum`].
    Enum {
        identity: &'a TypeIdentity,
        base: &'a AttributeType,
        values: Option<&'a [String]>,
        dsl_aliases: &'a [(String, String)],
        validate: Option<&'a CustomValidator>,
        to_dsl: Option<&'a DslTransform>,
    },
    /// List with element type and ordering — see [`AttrTypeKind::List`].
    List {
        element_type: &'a AttributeType,
        ordered: bool,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: &'a CustomValidator,
    },
    /// Map with typed key/value — see [`AttrTypeKind::Map`].
    Map {
        key: &'a AttributeType,
        value: &'a AttributeType,
    },
    /// Named struct — see [`AttrTypeKind::Struct`].
    Struct {
        name: &'a str,
        fields: &'a [StructField],
    },
    /// Union of types — see [`AttrTypeKind::Union`].
    Union(&'a [AttributeType]),
    /// Named reference into the enclosing [`Schema`]'s `defs` map.
    /// Carries the name; the receiver re-emits an `AttributeType::ref_(name)`
    /// and looks up the definition in its own copy of `defs`.
    Ref(&'a str),
}

impl fmt::Debug for RawShape<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RawShape::String {
                identity,
                pattern,
                length,
                validate: _,
                to_dsl: _,
            } => f
                .debug_struct("RawShape::String { .. }")
                .field("identity", identity)
                .field("pattern", pattern)
                .field("length", length)
                .finish_non_exhaustive(),
            RawShape::Int {
                identity,
                range,
                validate: _,
            } => f
                .debug_struct("RawShape::Int { .. }")
                .field("identity", identity)
                .field("range", range)
                .finish_non_exhaustive(),
            RawShape::Float {
                identity,
                range,
                validate: _,
            } => f
                .debug_struct("RawShape::Float { .. }")
                .field("identity", identity)
                .field("range", range)
                .finish_non_exhaustive(),
            RawShape::Bool => f.write_str("RawShape::Bool"),
            RawShape::Duration => f.write_str("RawShape::Duration"),
            RawShape::Enum {
                identity,
                base,
                values,
                dsl_aliases,
                validate: _,
                to_dsl: _,
            } => f
                .debug_struct("RawShape::Enum")
                .field("identity", identity)
                .field("base", base)
                .field("values", values)
                .field("dsl_aliases", dsl_aliases)
                .finish_non_exhaustive(),
            RawShape::List {
                element_type,
                ordered,
                length,
                validate: _,
            } => f
                .debug_struct("RawShape::List")
                .field("element_type", element_type)
                .field("ordered", ordered)
                .field("length", length)
                .finish_non_exhaustive(),
            RawShape::Map { key, value } => f
                .debug_struct("RawShape::Map")
                .field("key", key)
                .field("value", value)
                .finish(),
            RawShape::Struct { name, fields } => f
                .debug_struct("RawShape::Struct")
                .field("name", name)
                .field("fields", fields)
                .finish(),
            RawShape::Union(members) => f.debug_tuple("RawShape::Union").field(members).finish(),
            RawShape::Ref(name) => f.debug_tuple("RawShape::Ref").field(name).finish(),
        }
    }
}

/// A complete schema: a root attribute type together with the named
/// definitions it can reference via [`AttrTypeKind::Ref`].
///
/// Introduced in carina#3340 to model cyclic CloudFormation definition
/// graphs (WAFv2 `WebACL.Statement`, AppSync `GraphQLApi`,
/// StepFunctions state machine bodies, ...). The validate / diff / LSP
/// walk-sites that may encounter a `Ref` take `&Schema` as `&self`, so
/// the resolve step is required by the type signature rather than by
/// convention.
#[derive(Debug, Clone)]
pub struct Schema {
    /// The root attribute type. May be a `Ref` into `defs`, or any
    /// other shape (`Struct`, `List`, primitive, ...).
    pub root: AttributeType,
    /// Named definitions reachable via [`AttrTypeKind::Ref`].
    pub defs: std::collections::BTreeMap<String, AttributeType>,
}

impl Schema {
    /// Construct a `Schema` from `root` with an empty `defs` map. A
    /// `Ref` reached during walking surfaces as a clean
    /// `ValidationFailed` (`schema reference '<name>' is not defined
    /// in the enclosing schema`); callers expecting `Ref` to resolve
    /// must populate `defs`. Typical uses: unit-test fixtures, leaf
    /// validation, provider-config schemas that are known flat.
    pub fn flat(root: AttributeType) -> Self {
        Self {
            root,
            defs: std::collections::BTreeMap::new(),
        }
    }

    /// Construct a `Schema` that carries only `defs`, with a
    /// placeholder `root`. Use when the call shape is "iterate
    /// attributes against a shared def map" and the per-call
    /// attribute is supplied through [`Self::validate_attr`]
    /// (which ignores `self.root`). Examples:
    /// `ResourceSchema::validate_inner`, provider-config validation
    /// in `carina-core::validation` and `carina-lsp::diagnostics`.
    ///
    /// **Do not** call [`Self::validate`] or [`Self::validate_collect`]
    /// on a Schema produced by `with_defs`; those entry points walk
    /// `self.root`, which here is a non-load-bearing
    /// `AttrTypeKind::String { .. }` and will silently validate every
    /// value as a String.
    pub fn with_defs(defs: std::collections::BTreeMap<String, AttributeType>) -> Self {
        Self {
            // `validate_attr(attr, value)` ignores `self.root`; pick
            // the cheapest leaf as a non-load-bearing placeholder.
            root: AttributeType::string(),
            defs,
        }
    }

    /// Look up a named definition. Returns `None` if `name` is not in
    /// `defs`; callers should treat that as a type error.
    pub fn resolve(&self, name: &str) -> Option<&AttributeType> {
        self.defs.get(name)
    }

    /// Resolve `ty` against this schema's definition map.
    pub fn resolve_of<'a>(&'a self, ty: &'a AttributeType) -> ResolvedAttrType<'a> {
        ty.resolve_refs_with_defs(&self.defs)
    }

    /// Project `ty` onto a [`Shape`] using this schema's definition map.
    pub fn shape_of<'a>(&'a self, ty: &'a AttributeType) -> Shape<'a> {
        ty.shape_with_defs(&self.defs)
    }

    /// Validate a `Value` against this schema's `root`, resolving
    /// any `Ref` it encounters by looking it up in `defs`.
    pub fn validate(&self, value: &Value) -> Result<(), TypeError> {
        self.validate_attr(&self.root, value)
    }

    /// Canonicalize a `Value` against `attr_type` using this schema's
    /// `defs` map. The Schema-aware entry point for the
    /// `string_or_list_of_strings` and `Ref` walks (carina#3345). This
    /// is the only way to invoke canonicalization from outside
    /// `carina-core`, so callers cannot accidentally drop `defs`.
    pub fn canonicalize_attr(&self, attr: &AttributeType, value: Value) -> Value {
        crate::value::canonicalize_with_type(value, attr, &self.defs)
    }

    /// Canonicalize a `Value` against this schema's `root`. Wrapper
    /// over [`Self::canonicalize_attr`] for the common case where the
    /// caller is driving from `Schema::root`.
    pub fn canonicalize(&self, value: Value) -> Value {
        self.canonicalize_attr(&self.root, value)
    }

    /// Schema-aware path-collecting validator. Equivalent to
    /// [`AttrTypeKind::validate_collect`] but resolves
    /// `AttrTypeKind::Ref` against `defs` at every walk-site, so the
    /// LSP per-keystroke diagnostic pass surfaces real per-field
    /// errors against cyclic CFN-style schemas instead of the
    /// standalone-validator sentinel (carina#3345).
    ///
    /// Returns `(FieldPath, TypeError)` pairs anchored at each
    /// offending location, mirroring the non-Schema-aware variant for
    /// downstream consumers like the LSP range mapper.
    pub fn validate_collect(&self, value: &Value) -> Vec<(FieldPath, TypeError)> {
        let mut out = Vec::new();
        self.collect_attr_into(&self.root, &FieldPath::new(), value, &mut out);
        out
    }

    fn collect_attr_into(
        &self,
        attr: &AttributeType,
        path: &FieldPath,
        value: &Value,
        out: &mut Vec<(FieldPath, TypeError)>,
    ) {
        // Peel Ref so the downstream arms never have to think about it.
        if let AttrTypeKind::Ref(name) = &attr.kind {
            match self.resolve(name) {
                Some(resolved) => {
                    self.collect_attr_into(resolved, path, value, out);
                }
                None => {
                    out.push((
                        path.clone(),
                        TypeError::ValidationFailed {
                            message: format!(
                                "schema reference `{name}` is not defined in the enclosing schema"
                            ),
                        },
                    ));
                }
            }
            return;
        }
        let Some(concrete) = value.as_concrete() else {
            return;
        };
        match &attr.kind {
            AttrTypeKind::Struct { name, fields } => {
                // Mirror the pre-#3345 standalone-validator collect_struct
                // behavior, but route field walks through
                // collect_attr_into so Ref-typed fields are resolved
                // against self.defs.
                if matches!(concrete, ConcreteValueRef::List(_)) {
                    out.push((
                        path.clone(),
                        TypeError::BlockSyntaxNotAllowed {
                            attribute: name.to_string(),
                        },
                    ));
                    return;
                }
                let Some(map) = (match concrete {
                    ConcreteValueRef::Map(m) => Some(m),
                    _ => None,
                }) else {
                    out.push((
                        path.clone(),
                        TypeError::TypeMismatch {
                            expected: attr.type_name(),
                            got: concrete.type_name().to_string(),
                        },
                    ));
                    return;
                };

                let accepted = build_accepted_field_map(fields);
                let canonical_field_names: Vec<&str> =
                    fields.iter().map(|f| f.name.as_str()).collect();

                // Required-field check.
                for f in fields {
                    if f.required && !map.contains_key(&f.name) {
                        let field_path = path.push_field(f.name.clone());
                        out.push((
                            field_path,
                            TypeError::MissingRequired {
                                name: f.name.clone(),
                            },
                        ));
                    }
                }

                for (k, v) in map {
                    match accepted.get(k.as_str()) {
                        Some(field) => {
                            let next_path = path.push_field(k.clone());
                            self.collect_attr_into(&field.field_type, &next_path, v, out);
                        }
                        None => {
                            let suggestion = suggest_similar_name(k, &canonical_field_names);
                            out.push((
                                path.push_field(k.clone()),
                                TypeError::UnknownStructField {
                                    struct_name: name.to_string(),
                                    field: k.clone(),
                                    suggestion,
                                },
                            ));
                        }
                    }
                }
            }
            AttrTypeKind::List {
                element_type: inner,
                ..
            } => {
                let Some(items) = (match concrete {
                    ConcreteValueRef::List(items) => Some(items),
                    _ => None,
                }) else {
                    out.push((
                        path.clone(),
                        TypeError::TypeMismatch {
                            expected: attr.type_name(),
                            got: concrete.type_name().to_string(),
                        },
                    ));
                    return;
                };
                for (i, item) in items.iter().enumerate() {
                    let next_path = path.push_index(i);
                    self.collect_attr_into(inner, &next_path, item, out);
                }
            }
            // Union of leaves is the only Union shape in the current
            // schema vocabulary (e.g. `string_or_list_of_strings`).
            // Delegate to `validate_attr` which runs the best-scoring
            // member selection and returns a single error anchored at
            // the current path. If a Union-of-structs shape is ever
            // introduced, this arm should recurse via collect_attr_into
            // on the selected member to preserve per-field paths.
            AttrTypeKind::Union(_) => {
                if let Err(e) = self.validate_attr(attr, value) {
                    out.push((path.clone(), e));
                }
            }
            _ => {
                // For non-recursive shapes the existing single-shot
                // dispatcher is correct. Forward any error under the
                // current path.
                if let Err(e) = self.validate_attr(attr, value) {
                    out.push((path.clone(), e));
                }
            }
        }
    }

    /// Validate against an arbitrary `AttributeType` in the context of
    /// this schema. Used internally to recurse through `Ref`; exposed
    /// publicly so callers that already hold a sub-attribute (e.g. a
    /// resource's attribute type plus the resource's struct-definition
    /// map) can validate without re-rooting the schema.
    pub fn validate_attr(&self, attr: &AttributeType, value: &Value) -> Result<(), TypeError> {
        match &attr.kind {
            AttrTypeKind::Ref(name) => match self.resolve(name) {
                Some(resolved) => self.validate_attr(resolved, value),
                None => Err(TypeError::ValidationFailed {
                    message: format!(
                        "schema reference `{name}` is not defined in the enclosing schema"
                    ),
                }),
            },
            AttrTypeKind::List {
                element_type: inner,
                ordered,
                length,
                validate,
            } => {
                if let Some(ConcreteValueRef::List(items)) = value.as_concrete() {
                    for (i, item) in items.iter().enumerate() {
                        if let Err(inner_err) = self.validate_attr(inner, item) {
                            return Err(TypeError::ListItemError {
                                index: i,
                                inner: Box::new(inner_err),
                            });
                        }
                    }
                    Ok(())
                } else if value.as_concrete().is_none() {
                    // Deferred — leave for the deferred-aware checker.
                    Ok(())
                } else {
                    // Fall back to the standalone validator for the
                    // not-a-list error. `ordered` is irrelevant when
                    // the value is not even a list.
                    let _ = ordered;
                    AttributeType {
                        kind: AttrTypeKind::List {
                            element_type: inner.clone(),
                            ordered: *ordered,
                            length: *length,
                            validate: validate.clone(),
                        },
                    }
                    .validate(value)
                }
            }
            AttrTypeKind::Map { key, value: val_ty } => {
                if let Some(ConcreteValueRef::Map(map)) = value.as_concrete() {
                    for (k, v) in map.iter() {
                        let key_val = lift_map_key(key, k);
                        if let Err(inner_err) = self.validate_attr(key, &key_val) {
                            return Err(TypeError::MapKeyError {
                                key: k.clone(),
                                inner: Box::new(inner_err),
                            });
                        }
                        if let Err(inner_err) = self.validate_attr(val_ty, v) {
                            return Err(TypeError::MapValueError {
                                key: k.clone(),
                                inner: Box::new(inner_err),
                            });
                        }
                    }
                    Ok(())
                } else {
                    AttributeType {
                        kind: AttrTypeKind::Map {
                            key: key.clone(),
                            value: val_ty.clone(),
                        },
                    }
                    .validate(value)
                }
            }
            AttrTypeKind::Struct { name, fields } => {
                if let Some(ConcreteValueRef::Map(map)) = value.as_concrete() {
                    for field in fields {
                        match map.get(&field.name) {
                            Some(field_val) => {
                                if let Err(inner_err) =
                                    self.validate_attr(&field.field_type, field_val)
                                {
                                    return Err(TypeError::StructFieldError {
                                        field: field.name.clone(),
                                        inner: Box::new(inner_err),
                                    });
                                }
                            }
                            None if field.required => {
                                return Err(TypeError::MissingRequired {
                                    name: field.name.clone(),
                                });
                            }
                            None => {}
                        }
                    }
                    let _ = name;
                    Ok(())
                } else {
                    AttributeType {
                        kind: AttrTypeKind::Struct {
                            name: name.clone(),
                            fields: fields.clone(),
                        },
                    }
                    .validate(value)
                }
            }
            AttrTypeKind::Union(members) => {
                // Mirror `validate_union`'s best-scoring selection so
                // the Enum-rich diagnostic (variant list, alias
                // hints) survives when the user-supplied value shape
                // most closely matches a Enum member — the LSP
                // quick-fix depends on the structured payload (#2309).
                let Some(concrete) = value.as_concrete() else {
                    return Ok(());
                };
                let mut best: Option<(u32, TypeError)> = None;
                for member in members {
                    match self.validate_attr(member, value) {
                        Ok(()) => return Ok(()),
                        Err(e) => {
                            let score = union_member_score(member, concrete);
                            if score > 0
                                && best
                                    .as_ref()
                                    .is_none_or(|(prev_score, _)| score > *prev_score)
                            {
                                best = Some((score, e));
                            }
                        }
                    }
                }
                Err(best.map(|(_, e)| e).unwrap_or(TypeError::TypeMismatch {
                    expected: attr.type_name(),
                    got: concrete.type_name().to_string(),
                }))
            }
            // For non-recursive variants, delegate to the standalone
            // validator. Any `Ref` they could reach has already been
            // peeled off by the arms above.
            _ => attr.validate(value),
        }
    }
}

impl fmt::Debug for AttrTypeKind {
    // Forward to AttributeType's Debug to keep one source of truth for
    // formatting. The wrapper is a transparent newtype-style struct,
    // so the rendered output is identical to the historical Debug.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Wrap in a transient AttributeType and dispatch through its
        // Debug impl.
        let wrapped = AttributeType { kind: self.clone() };
        fmt::Debug::fmt(&wrapped, f)
    }
}

impl fmt::Debug for AttributeType {
    // Hand-written so that `Custom.validate` (an `Arc<dyn Fn>`, which does
    // not implement `Debug`) can be rendered as a placeholder. Every other
    // variant matches what `#[derive(Debug)]` would produce.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            AttrTypeKind::String {
                identity,
                pattern,
                length,
                to_dsl,
                validate: _,
            } => f
                .debug_struct("String")
                .field("identity", identity)
                .field("pattern", pattern)
                .field("length", length)
                .field("to_dsl", to_dsl)
                .field("validate", &"<closure>")
                .finish(),
            AttrTypeKind::Int {
                identity,
                range,
                validate: _,
            } => f
                .debug_struct("Int")
                .field("identity", identity)
                .field("range", range)
                .field("validate", &"<closure>")
                .finish(),
            AttrTypeKind::Float {
                identity,
                range,
                validate: _,
            } => f
                .debug_struct("Float")
                .field("identity", identity)
                .field("range", range)
                .field("validate", &"<closure>")
                .finish(),
            AttrTypeKind::Bool => f.write_str("Bool"),
            AttrTypeKind::Duration => f.write_str("Duration"),
            AttrTypeKind::Enum {
                identity,
                base,
                values,
                dsl_aliases,
                to_dsl,
                validate: _,
            } => f
                .debug_struct("Enum")
                .field("values", values)
                .field("identity", identity)
                .field("base", base)
                .field("dsl_aliases", dsl_aliases)
                .field("to_dsl", to_dsl)
                .field("validate", &"<closure>")
                .finish(),
            AttrTypeKind::List {
                element_type,
                ordered,
                length,
                validate: _,
            } => f
                .debug_struct("List")
                .field("element_type", element_type)
                .field("ordered", ordered)
                .field("length", length)
                .field("validate", &"<closure>")
                .finish(),
            AttrTypeKind::Map { key, value } => f
                .debug_struct("Map")
                .field("key", key)
                .field("value", value)
                .finish(),
            AttrTypeKind::Struct { name, fields } => f
                .debug_struct("Struct")
                .field("name", name)
                .field("fields", fields)
                .finish(),
            AttrTypeKind::Union(types) => f.debug_tuple("Union").field(types).finish(),
            AttrTypeKind::Ref(name) => f.debug_tuple("Ref").field(name).finish(),
        }
    }
}

/// One step in a [`FieldPath`]. Either a struct field name or a list
/// index — what the path needs to express to point a downstream tool
/// (e.g. the LSP) at the offending location in the source DSL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldPathStep {
    Field(String),
    Index(usize),
}

impl fmt::Display for FieldPathStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FieldPathStep::Field(name) => write!(f, "{}", name),
            FieldPathStep::Index(i) => write!(f, "[{}]", i),
        }
    }
}

/// Path from the validated value's root to the location that produced
/// a particular [`TypeError`]. Used by [`AttrTypeKind::validate_collect`]
/// so the LSP can map errors back to source positions without
/// re-running validation itself (#2214).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldPath {
    steps: Vec<FieldPathStep>,
}

impl FieldPath {
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    pub fn steps(&self) -> &[FieldPathStep] {
        &self.steps
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Append a struct-field step and return a new path. Cheap clone
    /// because validation paths are tiny (depth-of-struct, typically
    /// < 5).
    pub fn push_field(&self, name: impl Into<String>) -> Self {
        let mut next = self.clone();
        next.steps.push(FieldPathStep::Field(name.into()));
        next
    }

    /// Append a list-index step and return a new path.
    pub fn push_index(&self, index: usize) -> Self {
        let mut next = self.clone();
        next.steps.push(FieldPathStep::Index(index));
        next
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut first = true;
        for step in &self.steps {
            match step {
                FieldPathStep::Field(name) => {
                    if !first {
                        write!(f, ".")?;
                    }
                    write!(f, "{}", name)?;
                }
                FieldPathStep::Index(i) => write!(f, "[{}]", i)?,
            }
            first = false;
        }
        Ok(())
    }
}

/// Lift a `Map` key string into the `Value` shape its `key_type`
/// expects. Enum map keys are lifted to `EnumIdentifier` so
/// the strict carina#2986 validator accepts the bare-identifier form
/// users write in source; every other key type lowers to `String`
/// (carina#2996). Shared between `Schema::validate_attr`'s Map arm
/// and the standalone `validate_map` so the two walkers cannot
/// drift again (carina#3347).
pub(crate) fn lift_map_key(key_type: &AttributeType, key: &str) -> Value {
    if matches!(&key_type.kind, AttrTypeKind::Enum { .. }) {
        Value::Concrete(ConcreteValue::EnumIdentifier(key.to_string()))
    } else {
        Value::Concrete(ConcreteValue::String(key.to_string()))
    }
}

/// Empty definition map for walk-sites whose resource schema carries
/// no cyclic struct definitions (the common case before carina#3340's
/// chain reaches awscc's WAFv2 WebACL).
///
/// Returned by reference so a single `&BTreeMap` reference can be
/// threaded through every walk-site without per-call allocation.
/// Walk-sites that hold a [`ResourceSchema`] should prefer `&rs.defs`
/// to capture cyclic definitions; this constant is for sites that
/// genuinely have no resource schema in scope (synthetic fixtures,
/// validation helpers operating on a bare `AttributeType`).
pub(crate) fn empty_defs_for_schema_walks()
-> &'static std::collections::BTreeMap<String, AttributeType> {
    use std::sync::OnceLock;
    static EMPTY: OnceLock<std::collections::BTreeMap<String, AttributeType>> = OnceLock::new();
    EMPTY.get_or_init(std::collections::BTreeMap::new)
}

impl AttributeType {
    /// Walk through any [`AttrTypeKind::Ref`] chain at the top of this
    /// type, returning the first non-`Ref` target wrapped in
    /// [`ResolvedAttrType`].
    ///
    /// The wrapped return type is the type-level encoding of the
    /// carina#3340 invariant: every walk-site (differ, detail_rows,
    /// LSP, block-name resolution, ...) must peel `Ref` *before*
    /// matching on the type, so the wildcard arm cannot silently
    /// accept a `Ref` and discard its schema information. The
    /// `defs` map is normally `&ResourceSchema::defs`; pass
    /// [`empty_defs_for_schema_walks`] when no resource is in scope.
    ///
    /// # Panics
    ///
    /// Panics if a `Ref` points to a name not present in `defs`. This
    /// is a schema invariant (a producer that emits `Ref` must also
    /// populate the matching def); a violation indicates a codegen
    /// or wire-format bug, not user input.
    pub(crate) fn resolve_refs_with_defs<'a>(
        &'a self,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
    ) -> ResolvedAttrType<'a> {
        let mut cur = self;
        // A small upper bound to catch pathological cycles
        // (`Ref("X") -> Ref("X")`) without hanging the walker.
        for _ in 0..256 {
            match &cur.kind {
                AttrTypeKind::Ref(name) => match defs.get(name) {
                    Some(next) => cur = next,
                    None => panic!(
                        "AttributeType::Ref(\"{name}\") not found in schema defs; \
                         schema invariant violated (producer must populate \
                         ResourceSchema.defs for every Ref it emits)"
                    ),
                },
                _ => return ResolvedAttrType::new_after_peel(cur),
            }
        }
        panic!("AttributeType::Ref chain exceeded 256 hops; pathological self-cycle in defs")
    }

    /// Project this type onto a [`Shape`] view with every top-level
    /// `Ref` already peeled against `defs`.
    ///
    /// Callers that need to branch on the type's shape should `match`
    /// the returned [`Shape`] instead of `&AttributeType`. The
    /// `Shape` enum has no `Ref` variant, so a wildcard arm in
    /// `match attr.shape_with_defs(defs) { ... }` cannot silently swallow a
    /// `Ref` — the carina#3349 bug class is unreachable at the type
    /// level.
    ///
    /// `defs` is normally `&ResourceSchema::defs` or `&Schema::defs`;
    /// pass [`empty_defs_for_schema_walks`] when no resource is in scope.
    ///
    /// # Panics
    ///
    /// Panics if a `Ref` points to a name not present in `defs` (same
    /// schema invariant as [`Self::resolve_refs_with_defs`]).
    pub(crate) fn shape_with_defs<'a>(
        &'a self,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
    ) -> Shape<'a> {
        let resolved = self.resolve_refs_with_defs(defs).as_attr();
        Self::shape_from_resolved(resolved)
    }

    /// Project this type without a defs map. Returns an error instead
    /// of panicking if the top-level type is a schema-bound `Ref`.
    pub fn shape_ref_free(&self) -> Result<Shape<'_>, RefEncountered> {
        if let AttrTypeKind::Ref(name) = &self.kind {
            return Err(RefEncountered { name: name.clone() });
        }
        Ok(Self::shape_from_resolved(self))
    }

    pub fn struct_fields_ref_free_with_budget(
        &self,
        budget: &mut ShapeWalkBudget,
    ) -> Result<Option<&[StructField]>, RefEncountered> {
        if !budget.take() {
            return Ok(None);
        }
        if let AttrTypeKind::Ref(name) = &self.kind {
            return Err(RefEncountered { name: name.clone() });
        }
        Ok(match &self.kind {
            AttrTypeKind::Struct { fields, .. } => Some(fields.as_slice()),
            _ => None,
        })
    }

    pub fn union_members_ref_free_with_budget(
        &self,
        budget: &mut ShapeWalkBudget,
    ) -> Result<Option<&[AttributeType]>, RefEncountered> {
        if !budget.take() {
            return Ok(None);
        }
        if let AttrTypeKind::Ref(name) = &self.kind {
            return Err(RefEncountered { name: name.clone() });
        }
        Ok(match &self.kind {
            AttrTypeKind::Union(members) => Some(members.as_slice()),
            _ => None,
        })
    }

    fn shape_from_resolved(resolved: &AttributeType) -> Shape<'_> {
        match &resolved.kind {
            AttrTypeKind::String {
                identity,
                pattern,
                length,
                validate,
                to_dsl,
            } => Shape::String {
                identity: identity.as_ref(),
                pattern: pattern.as_deref(),
                length: *length,
                validate,
                to_dsl: to_dsl.as_ref(),
            },
            AttrTypeKind::Int {
                identity,
                range,
                validate,
            } => Shape::Int {
                identity: identity.as_ref(),
                range: *range,
                validate,
            },
            AttrTypeKind::Float {
                identity,
                range,
                validate,
            } => Shape::Float {
                identity: identity.as_ref(),
                range: *range,
                validate,
            },
            AttrTypeKind::Bool => Shape::Bool,
            AttrTypeKind::Duration => Shape::Duration,
            AttrTypeKind::Enum {
                identity,
                base,
                values,
                dsl_aliases,
                validate,
                to_dsl,
            } => Shape::Enum {
                identity,
                base: base.as_ref(),
                values: values.as_deref(),
                dsl_aliases: dsl_aliases.as_slice(),
                validate: validate.as_ref(),
                to_dsl: to_dsl.as_ref(),
            },
            AttrTypeKind::List {
                element_type,
                ordered,
                length,
                validate,
            } => Shape::List {
                element_type: element_type.as_ref(),
                ordered: *ordered,
                length: *length,
                validate,
            },
            AttrTypeKind::Map { key, value } => Shape::Map {
                key: key.as_ref(),
                value: value.as_ref(),
            },
            AttrTypeKind::Struct { name, fields: _ } => Shape::Struct {
                name: name.as_str(),
            },
            AttrTypeKind::Union(_members) => Shape::Union,
            AttrTypeKind::Ref(_) => unreachable!(
                "resolve_refs guarantees the returned attr is not Ref; \
                 reaching this arm violates ResolvedAttrType's invariant"
            ),
        }
    }

    /// Project this type onto a [`RawShape`] view that **preserves
    /// `Ref`** instead of resolving it against any `defs` map.
    ///
    /// Intended for callers that round-trip the type across a
    /// transport boundary (the WASM plugin↔host WIT/JSON serializers
    /// in `carina-provider-{aws,awscc}/src/convert.rs`,
    /// `carina-plugin-host/src/wasm_convert.rs`). At a transport
    /// site, resolving `Ref` is wrong on two counts: cyclic schemas
    /// like WAFv2 `WebACL.Statement` recurse forever, and even on
    /// acyclic schemas, flattening `Ref` discards the `defs` shape
    /// the receiver needs to reconstruct.
    ///
    /// Walk-sites (differ, detail_rows, LSP, validation, ...) must
    /// keep using [`Self::shape`] — `RawShape::Ref` is reachable here
    /// by design and a wildcard arm would silently re-introduce the
    /// carina#3349 bug class at the walk-site.
    pub fn raw_shape<'a>(&'a self) -> RawShape<'a> {
        match &self.kind {
            AttrTypeKind::String {
                identity,
                pattern,
                length,
                validate,
                to_dsl,
            } => RawShape::String {
                identity: identity.as_ref(),
                pattern: pattern.as_deref(),
                length: *length,
                validate,
                to_dsl: to_dsl.as_ref(),
            },
            AttrTypeKind::Int {
                identity,
                range,
                validate,
            } => RawShape::Int {
                identity: identity.as_ref(),
                range: *range,
                validate,
            },
            AttrTypeKind::Float {
                identity,
                range,
                validate,
            } => RawShape::Float {
                identity: identity.as_ref(),
                range: *range,
                validate,
            },
            AttrTypeKind::Bool => RawShape::Bool,
            AttrTypeKind::Duration => RawShape::Duration,
            AttrTypeKind::Enum {
                identity,
                base,
                values,
                dsl_aliases,
                validate,
                to_dsl,
            } => RawShape::Enum {
                identity,
                base: base.as_ref(),
                values: values.as_deref(),
                dsl_aliases: dsl_aliases.as_slice(),
                validate: validate.as_ref(),
                to_dsl: to_dsl.as_ref(),
            },
            AttrTypeKind::List {
                element_type,
                ordered,
                length,
                validate,
            } => RawShape::List {
                element_type: element_type.as_ref(),
                ordered: *ordered,
                length: *length,
                validate,
            },
            AttrTypeKind::Map { key, value } => RawShape::Map {
                key: key.as_ref(),
                value: value.as_ref(),
            },
            AttrTypeKind::Struct { name, fields } => RawShape::Struct {
                name: name.as_str(),
                fields: fields.as_slice(),
            },
            AttrTypeKind::Union(members) => RawShape::Union(members.as_slice()),
            AttrTypeKind::Ref(name) => RawShape::Ref(name.as_str()),
        }
    }

    /// In-crate inspector for the wrapped variant enum. The
    /// `pub(crate)` visibility keeps external code on
    /// [`Self::shape`] (the `Ref`-peeled view); internal code that
    /// needs to dispatch on a `Ref`-bearing raw view uses this.
    #[inline]
    pub(crate) fn kind(&self) -> &AttrTypeKind {
        &self.kind
    }

    /// Create the primitive `String` type.
    pub fn string() -> Self {
        AttributeType {
            kind: AttrTypeKind::String {
                identity: None,
                pattern: None,
                length: None,
                validate: noop_validator(),
                to_dsl: None,
            },
        }
    }

    /// Create a String type with protocol-carried refinement metadata.
    pub fn refined_string(
        identity: Option<TypeIdentity>,
        pattern: Option<String>,
        length: Option<(Option<u64>, Option<u64>)>,
        to_dsl: Option<DslTransform>,
    ) -> Self {
        AttributeType {
            kind: AttrTypeKind::String {
                identity,
                pattern,
                length,
                validate: noop_validator(),
                to_dsl,
            },
        }
    }

    /// Create the primitive `Int` type.
    pub fn int() -> Self {
        AttributeType {
            kind: AttrTypeKind::Int {
                identity: None,
                range: None,
                validate: noop_validator(),
            },
        }
    }

    /// Create an Int type with protocol-carried refinement metadata.
    pub fn refined_int(
        identity: Option<TypeIdentity>,
        range: Option<(Option<i64>, Option<i64>)>,
    ) -> Self {
        AttributeType {
            kind: AttrTypeKind::Int {
                identity,
                range,
                validate: noop_validator(),
            },
        }
    }

    /// Create the primitive `Float` type.
    pub fn float() -> Self {
        AttributeType {
            kind: AttrTypeKind::Float {
                identity: None,
                range: None,
                validate: noop_validator(),
            },
        }
    }

    /// Create a Float type with protocol-carried refinement metadata.
    pub fn refined_float(
        identity: Option<TypeIdentity>,
        range: Option<(Option<f64>, Option<f64>)>,
    ) -> Self {
        AttributeType {
            kind: AttrTypeKind::Float {
                identity,
                range,
                validate: noop_validator(),
            },
        }
    }

    /// Create the primitive `Bool` type.
    pub fn bool() -> Self {
        AttributeType {
            kind: AttrTypeKind::Bool,
        }
    }

    /// Create the `Duration` primitive type.
    pub fn duration() -> Self {
        AttributeType {
            kind: AttrTypeKind::Duration,
        }
    }

    /// Create an enum type whose underlying value shape is `String`.
    ///
    /// `identity` is mandatory. Former bare enum construction sites
    /// that passed no identity should use `TypeIdentity::bare(name)`;
    /// this keeps diagnostics and enum lifting keyed on the same
    /// structured identity even when there is no provider namespace.
    ///
    /// The `values`, `dsl_aliases`, `validate`, and `to_dsl`
    /// parameters describe the enum's value source and API-to-DSL
    /// spelling strategy:
    ///
    /// - `values: Some(...)`, non-empty `dsl_aliases`,
    ///   `validate: None`, `to_dsl: None`: a closed enum whose API
    ///   values and DSL spellings differ per variant, such as IAM
    ///   `Effect` / `Version`. The alias table carries the per-value
    ///   rewrite in both directions.
    /// - `values: Some(...)`, empty `dsl_aliases`, `validate: None`,
    ///   `to_dsl: None`: a closed enum whose API and DSL spellings are
    ///   identical.
    /// - `values: None`, empty `dsl_aliases`, `validate: Some(...)`,
    ///   `to_dsl: Some(...)`: an open or dynamic enum, such as
    ///   provider regions or availability zones, with a host-side
    ///   validator and transform.
    /// - `values: None`, empty `dsl_aliases`, `validate: None`,
    ///   `to_dsl: Some(...)`: a WASM-bridged dynamic enum. Schema-local
    ///   validation is absent because validation happens through
    ///   `ProviderContext.validators`; the protocol payload carries
    ///   the transform as data and the host applies it directly.
    ///
    /// At least one of `values: Some(...)`, `validate: Some(...)`, or a
    /// separately registered provider validator should exist for real
    /// provider schemas. With no values and no validator, the enum has
    /// no schema-local membership check and may accept arbitrary strings
    /// at validation sites that do not have provider lookup context.
    ///
    /// The WASM path cannot carry function pointers, so providers send
    /// the protocol's `dsl_transform` enum and the host stores that
    /// data in `to_dsl`.
    ///
    /// See `notes/specs/2026-06-07-enum-state-coherence-design.md`
    /// for the enum state-coherence design and the reason these former
    /// enum origins now share one `Enum` representation.
    pub fn enum_(
        identity: TypeIdentity,
        values: Option<Vec<String>>,
        dsl_aliases: Vec<(String, String)>,
        validate: Option<CustomValidator>,
        to_dsl: Option<DslTransform>,
    ) -> Self {
        Self::enum_with_base(
            identity,
            AttributeType::string(),
            values,
            dsl_aliases,
            validate,
            to_dsl,
        )
    }

    /// Create an enum type with an explicit underlying value shape.
    ///
    /// The `values`, `dsl_aliases`, `validate`, and `to_dsl`
    /// combinations have the same meaning as [`AttributeType::enum_`].
    /// `base` carries the underlying provider-declared value shape for
    /// cases where a structural fallback is meaningful, for example a
    /// process-boundary dynamic enum that historically wrapped a
    /// `Custom { pattern, length }` base. Most direct callers should
    /// use [`AttributeType::enum_`], which defaults the base to
    /// [`AttributeType::string`].
    ///
    /// When this constructor is used from WASM proto conversion, the
    /// protocol carries a data-driven transform enum that the host
    /// applies directly; no host-process registry lookup is needed.
    pub fn enum_with_base(
        identity: TypeIdentity,
        base: AttributeType,
        values: Option<Vec<String>>,
        dsl_aliases: Vec<(String, String)>,
        validate: Option<CustomValidator>,
        to_dsl: Option<DslTransform>,
    ) -> Self {
        AttributeType {
            kind: AttrTypeKind::Enum {
                identity,
                base: Box::new(base),
                values,
                dsl_aliases,
                validate,
                to_dsl,
            },
        }
    }

    /// Create a structural `Custom` type.
    pub fn custom(
        identity: Option<TypeIdentity>,
        base: AttributeType,
        pattern: Option<String>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
        to_dsl: Option<fn(&str) -> String>,
    ) -> Self {
        assert!(
            to_dsl.is_none(),
            "custom shim: function-pointer to_dsl cannot be represented as DslTransform"
        );
        AttributeType {
            kind: match base.kind {
                AttrTypeKind::String { to_dsl, .. } => AttrTypeKind::String {
                    identity,
                    pattern,
                    length,
                    validate,
                    to_dsl,
                },
                AttrTypeKind::Int { .. } => AttrTypeKind::Int {
                    identity,
                    range: length.map(|(min, max)| (min.map(|v| v as i64), max.map(|v| v as i64))),
                    validate,
                },
                AttrTypeKind::Float { .. } => AttrTypeKind::Float {
                    identity,
                    range: None,
                    validate,
                },
                AttrTypeKind::List {
                    element_type,
                    ordered,
                    ..
                } => AttrTypeKind::List {
                    element_type,
                    ordered,
                    length,
                    validate,
                },
                other => panic!("custom shim: unexpected base {other:?}"),
            },
        }
    }

    /// Create a `Struct` type.
    pub fn struct_(name: impl Into<String>, fields: Vec<StructField>) -> Self {
        AttributeType {
            kind: AttrTypeKind::Struct {
                name: name.into(),
                fields,
            },
        }
    }

    /// Create a `Union` type from member types.
    pub fn union(members: Vec<AttributeType>) -> Self {
        AttributeType {
            kind: AttrTypeKind::Union(members),
        }
    }

    /// Create a `Ref` type referencing a named definition in the
    /// enclosing [`Schema`]'s `defs` map.
    pub fn ref_(name: impl Into<String>) -> Self {
        AttributeType {
            kind: AttrTypeKind::Ref(name.into()),
        }
    }

    /// Create a List type with default ordering (ordered=true, matching CloudFormation default).
    pub fn list(inner: AttributeType) -> Self {
        AttributeType {
            kind: AttrTypeKind::List {
                element_type: Box::new(inner),
                ordered: true,
                length: None,
                validate: noop_validator(),
            },
        }
    }

    /// Create an unordered List type (insertionOrder=false).
    pub fn unordered_list(inner: AttributeType) -> Self {
        AttributeType {
            kind: AttrTypeKind::List {
                element_type: Box::new(inner),
                ordered: false,
                length: None,
                validate: noop_validator(),
            },
        }
    }

    /// Create a Map type with unconstrained string keys.
    pub fn map(value: AttributeType) -> Self {
        Self::map_with_key(AttributeType::string(), value)
    }

    /// Create a Map type with a typed key constraint.
    pub fn map_with_key(key: AttributeType, value: AttributeType) -> Self {
        AttributeType {
            kind: AttrTypeKind::Map {
                key: Box::new(key),
                value: Box::new(value),
            },
        }
    }

    /// Lift a [`ConcreteValueRef`] into an owned [`Value`] and run
    /// `expand_enum_shorthand` on it. Returns `Value` because
    /// `expand_enum_shorthand` is the existing backbone for namespaced
    /// enum normalization and operates on owned `Value`.
    ///
    /// Phase 2 of RFC #2972: the pre-Phase-2 `ResourceRef` short-circuit
    /// is gone — deferred values are filtered by the dispatcher in
    /// [`Self::validate`] and cannot reach this path.
    fn resolve_enum_input(identity: &TypeIdentity, value: ConcreteValueRef<'_>) -> Value {
        let owned = value.to_owned_value();
        crate::utils::expand_enum_shorthand(&owned, identity)
    }

    pub fn enum_parts(&self) -> Option<EnumParts<'_>> {
        match &self.kind {
            AttrTypeKind::Enum {
                identity,
                values,
                dsl_aliases,
                validate,
                to_dsl,
                ..
            } => Some((
                identity,
                values.as_deref(),
                dsl_aliases.as_slice(),
                validate.as_ref(),
                DslMap::new(dsl_aliases, to_dsl.as_ref()),
            )),
            _ => None,
        }
    }

    /// Check if a value conforms to this type.
    ///
    /// Top-level dispatcher (Phase 2 of RFC #2972):
    /// 1. Project `value` through `Value::as_concrete()`. Deferred-axis
    ///    values (`ResourceRef`, `BindingRef`, `Interpolation`,
    ///    `FunctionCall`, `Secret`, `Unknown`) return `None` and are
    ///    accepted unconditionally — type fitness for those is the
    ///    deferred-aware checker's job (`check_upstream_state_field_types`,
    ///    `validate_resource_ref_types`).
    /// 2. Dispatch the projected `ConcreteValueRef<'_>` to the
    ///    per-variant helper. Helpers cannot receive deferred values by
    ///    construction — the projection is the single place that filter
    ///    decision lives.
    ///
    /// Recursive descent into `List`/`Map`/`Struct` element types uses
    /// `inner.validate(&Value)` again, so each nested element is
    /// independently re-projected. Lists may legitimately mix concrete
    /// and deferred elements (e.g. `[vpc.id, "literal"]`).
    pub(crate) fn validate(&self, value: &Value) -> Result<(), TypeError> {
        // `Ref` cannot be resolved without a `Schema` context. Callers
        // who hold a schema that contains `Ref` must go through
        // `Schema::validate` / `Schema::validate_attr`; falling through
        // here would silently accept any value, defeating type safety
        // (carina#3340).
        if let AttrTypeKind::Ref(name) = &self.kind {
            return Err(TypeError::ValidationFailed {
                message: format!(
                    "internal: AttributeType::Ref(\"{name}\") reached the standalone \
                     validator; this attribute must be validated through Schema::validate"
                ),
            });
        }
        // Enum attributes assigned a bare `BindingRef` are the
        // shadowing collision case described in #2978: the user wrote a
        // bare identifier that happens to be a `let` binding *and* is
        // also a DSL alias for one of this enum's variants. Surface a
        // pointed error so they can disambiguate (use `TypeName.value`,
        // fully-qualified, or quoted string). Without this check the
        // deferred value flows through validation unchecked and only
        // surfaces later as a `${vpc}`-style error from the resolver.
        if let AttrTypeKind::Enum {
            identity,
            values: Some(values),
            dsl_aliases,
            ..
        } = &self.kind
            && let Some(binding) = bare_binding_name(value)
            && enum_alias_collides_with(binding, values, dsl_aliases)
        {
            let prefix = identity.dotted_prefix();
            return Err(TypeError::ValidationFailed {
                message: enum_binding_collision_message(
                    binding,
                    &identity.kind,
                    prefix.as_deref(),
                    values,
                    dsl_aliases,
                ),
            });
        }
        let Some(concrete) = value.as_concrete() else {
            return Ok(());
        };
        self.validate_concrete(concrete)
    }

    fn validate_concrete(&self, value: ConcreteValueRef<'_>) -> Result<(), TypeError> {
        match &self.kind {
            AttrTypeKind::Enum {
                identity,
                values,
                dsl_aliases,
                validate,
                to_dsl,
                ..
            } => Self::validate_enum(
                identity,
                values.as_deref(),
                dsl_aliases,
                DslMap::new(dsl_aliases, to_dsl.as_ref()),
                validate.as_ref(),
                value,
            ),
            AttrTypeKind::List { .. } => self.validate_list(value),
            AttrTypeKind::Map { .. } => self.validate_map(value),
            AttrTypeKind::Struct { .. } => self.validate_struct(value),
            AttrTypeKind::Union(_) => self.validate_union(value),
            AttrTypeKind::String { .. }
            | AttrTypeKind::Int { .. }
            | AttrTypeKind::Float { .. }
            | AttrTypeKind::Bool
            | AttrTypeKind::Duration => self.validate_primitive(value),
            // Unreachable: `validate` rejects `Ref` early before
            // descending into the concrete-value dispatch. Kept as an
            // explicit arm so the compiler enforces handling.
            AttrTypeKind::Ref(name) => Err(TypeError::ValidationFailed {
                message: format!(
                    "internal: AttributeType::Ref(\"{name}\") in validate_concrete; \
                     this attribute must be validated through Schema::validate"
                ),
            }),
        }
    }

    /// Validate a primitive (`String`/`Int`/`Float`/`Bool`/`Duration`) value.
    /// `Float` accepts integers as valid numbers and rejects non-finite
    /// floats explicitly.
    ///
    /// Phase 2 of RFC #2972: takes [`ConcreteValueRef`], so deferred
    /// values (`ResourceRef`, `Interpolation`, ...) cannot reach this
    /// path — they were filtered at the dispatcher in [`Self::validate`].
    /// The pre-Phase-2 `Value::Concrete(ConcreteValue::String(_)) | Value::Deferred(DeferredValue::ResourceRef{ .. }) |
    /// Value::Deferred(DeferredValue::Interpolation(_))` arm is now structurally unrepresentable.
    fn validate_primitive(&self, value: ConcreteValueRef<'_>) -> Result<(), TypeError> {
        match (&self.kind, value) {
            (
                AttrTypeKind::String {
                    identity,
                    pattern,
                    length,
                    validate,
                    ..
                },
                ConcreteValueRef::String(s),
            ) => {
                validate_string_refinement(identity.as_ref(), pattern.as_deref(), *length, s)?;
                validate(&value.to_owned_value())
            }
            (
                AttrTypeKind::Int {
                    range, validate, ..
                },
                ConcreteValueRef::Int(i),
            ) => {
                validate_int_range(*range, i)?;
                validate(&value.to_owned_value())
            }
            (
                AttrTypeKind::Float {
                    range, validate, ..
                },
                ConcreteValueRef::Float(f),
            ) if f.is_finite() => {
                validate_float_range(*range, f)?;
                validate(&value.to_owned_value())
            }
            (AttrTypeKind::Float { .. }, ConcreteValueRef::Float(f)) => {
                Err(TypeError::ValidationFailed {
                    message: format!("non-finite float value: {f}"),
                })
            }
            (
                AttrTypeKind::Float {
                    range, validate, ..
                },
                ConcreteValueRef::Int(i),
            ) => {
                validate_float_range(*range, i as f64)?;
                validate(&value.to_owned_value())
            }
            (AttrTypeKind::Bool, ConcreteValueRef::Bool(_)) => Ok(()),
            (AttrTypeKind::Duration, ConcreteValueRef::Duration(_)) => Ok(()),
            _ => Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: value.type_name().to_string(),
            }),
        }
    }

    /// Validate against an `Enum` variant.
    ///
    /// Phase 2 of RFC #2972: takes [`ConcreteValueRef`]. Pre-Phase-2
    /// `Value::Deferred(DeferredValue::Interpolation)` and `Value::Deferred(DeferredValue::ResourceRef)` short-circuits
    /// are gone — the dispatcher filters deferred values.
    fn validate_enum(
        identity: &TypeIdentity,
        values: Option<&[String]>,
        dsl_aliases: &[(String, String)],
        dsl_map: DslMap<'_>,
        validate: Option<&CustomValidator>,
        value: ConcreteValueRef<'_>,
    ) -> Result<(), TypeError> {
        let prefix = identity.dotted_prefix();
        let prefix_ref = prefix.as_deref();
        let name = identity.kind.as_str();
        let expected = values
            .map(|values| enum_expected_variants(prefix_ref, name, values, dsl_aliases))
            .unwrap_or_default();
        let enumerated = values.map(|_| true).unwrap_or(false);
        let resolved_value = Self::resolve_enum_input(identity, value);
        let validation_result = validate.map(|validate| validate(&resolved_value));
        let validation_ok = validation_result
            .as_ref()
            .map_or(!enumerated, Result::is_ok);

        if let ConcreteValueRef::String(s) = value {
            if !enumerated && validation_ok {
                return Ok(());
            }
            return Err(TypeError::StringLiteralExpectedEnum {
                user_typed: s.to_string(),
                attribute: None,
                type_name: name.to_string(),
                expected,
                extra_message: validation_result
                    .and_then(Result::err)
                    .map(validation_error_message),
            });
        }

        // Capture the user's original input for diagnostics. The parser
        // emits `ConcreteValue::EnumIdentifier` for bare identifier short
        // forms (`dedicated`) and dotted forms (`aws.s3.Bucket.VersioningStatus.Enabled`).
        // `resolve_enum_input` rewrites the non-dotted form into a
        // synthesized namespaced string for lookup. That synthesized form
        // must stay internal: error messages should quote what the user
        // actually typed. See #2077.
        let user_input = match value {
            ConcreteValueRef::EnumIdentifier(s) => Some(s),
            _ => None,
        };
        if let Value::Concrete(ConcreteValue::String(s)) = &resolved_value {
            // Check if the raw string directly matches a valid enum value
            // before namespace validation. This handles values containing
            // dots (e.g., "ipsec.1") that would be misinterpreted as
            // namespace separators.
            let direct_match = values
                .into_iter()
                .flatten()
                .any(|v| enum_value_matches(s, v));
            let valid: Vec<&str> = values.into_iter().flatten().map(String::as_str).collect();
            let variant = if direct_match {
                s.as_str()
            } else {
                extract_enum_value_with_values(s, &valid)
            };

            // Non-direct matches must have the exact form
            // `{namespace}.{name}.{variant}`. This rejects malformed
            // inputs like double-namespaced values while still allowing
            // enum values that themselves contain dots (e.g., "ipsec.1").
            if !direct_match && let Some(ns) = prefix_ref {
                let expected_prefix = format!("{}.{}.", ns, name);
                let prefix_matches =
                    s.starts_with(&expected_prefix) && &s[expected_prefix.len()..] == variant;
                if !prefix_matches {
                    // Fall back to strict namespace validation, which
                    // produces a clear error for the common bare form.
                    let user_form = user_input.unwrap_or(s.as_str());
                    validate_enum_namespace(s, identity).map_err(|message| {
                        TypeError::ValidationFailed {
                            message: format!("Invalid {} '{}': {}", name, user_form, message),
                        }
                    })?;
                }
            }
            let matches_canonical = values
                .into_iter()
                .flatten()
                .any(|v| enum_value_matches(variant, v));
            let matches_alias = dsl_aliases
                .iter()
                .any(|(_api, dsl)| enum_value_matches(variant, dsl));
            // DSL surface convention is snake_case (see
            // `feedback_dsl_enum_snake_case_convention.md`). When an
            // enum has a `dsl_aliases` entry that rewrites its API
            // spelling (e.g. `("-1", "all")`, `("VPC", "vpc")`,
            // `("BucketOwnerEnforced", "bucket_owner_enforced")`),
            // the validator must reject the API form on DSL input —
            // the user is supposed to write the DSL spelling. An
            // enum whose `dsl_aliases` table is empty (or whose
            // matched value has no rewrite registered) keeps the
            // canonical fall-through enabled, so the change is a
            // no-op until codegen populates the table per enum.
            //
            // See carina#2980 for the full sweep plan.
            let rewritten_by_alias = dsl_map.api_for(variant) != variant
                || dsl_aliases
                    .iter()
                    .any(|(api, dsl)| api != dsl && enum_value_matches(variant, api));
            let canonical_ok = matches_canonical && !rewritten_by_alias;
            if matches_alias || canonical_ok || (!enumerated && validation_ok) {
                Ok(())
            } else {
                Err(TypeError::InvalidEnumVariant {
                    value: user_input.unwrap_or(s.as_str()).to_string(),
                    attribute: None,
                    type_name: Some(name.to_string()),
                    expected,
                })
            }
        } else {
            Err(TypeError::TypeMismatch {
                expected: name.to_string(),
                got: resolved_value.type_name(),
            })
        }
    }

    /// Validate a `List` variant by validating each item with the inner type.
    ///
    /// Phase 2 of RFC #2972 (closes #2954): takes [`ConcreteValueRef`].
    /// The pre-Phase-2 path was reachable from any `Value` and produced
    /// a spurious `Type mismatch: expected List<T>, got ResourceRef(...)`
    /// for upstream-typed list refs. After Phase 2, deferred values
    /// cannot reach this helper — the dispatcher's `as_concrete()`
    /// projection returns `None` for them and the dispatcher returns
    /// `Ok(())` immediately. Type fitness for upstream list refs is
    /// the deferred-aware checker's job.
    fn validate_list(&self, value: ConcreteValueRef<'_>) -> Result<(), TypeError> {
        let AttrTypeKind::List {
            element_type: inner,
            length,
            validate,
            ..
        } = &self.kind
        else {
            unreachable!("validate_list called on non-List");
        };
        // `ConcreteValueRef::StringList` is the canonicalized form for
        // fields typed as `Union[String, list(String)]` (see #2510).
        // Structurally equivalent to a `List` of strings — accept the
        // same way.
        if let ConcreteValueRef::StringList(items) = value {
            validate_list_length(*length, items.len())?;
            for (i, s) in items.iter().enumerate() {
                inner
                    .validate(&Value::Concrete(ConcreteValue::String(s.clone())))
                    .map_err(|e| TypeError::ListItemError {
                        index: i,
                        inner: Box::new(e),
                    })?;
            }
            return validate(&value.to_owned_value());
        }
        let ConcreteValueRef::List(items) = value else {
            return Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: value.type_name().to_string(),
            });
        };
        validate_list_length(*length, items.len())?;
        for (i, item) in items.iter().enumerate() {
            inner.validate(item).map_err(|e| TypeError::ListItemError {
                index: i,
                inner: Box::new(e),
            })?;
        }
        validate(&value.to_owned_value())
    }

    /// Validate a `Map` variant: keys against `key`, values against `value`.
    ///
    /// Phase 2 of RFC #2972: takes [`ConcreteValueRef`]. Mirrors the
    /// `validate_list` migration — deferred values are filtered at the
    /// dispatcher and cannot reach here.
    fn validate_map(&self, value: ConcreteValueRef<'_>) -> Result<(), TypeError> {
        let AttrTypeKind::Map {
            key: key_type,
            value: inner,
        } = &self.kind
        else {
            unreachable!("validate_map called on non-Map");
        };
        let ConcreteValueRef::Map(map) = value else {
            return Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: value.type_name().to_string(),
            });
        };
        for k in map.keys() {
            let key_value = lift_map_key(key_type, k);
            key_type
                .validate(&key_value)
                .map_err(|e| TypeError::MapKeyError {
                    key: k.clone(),
                    inner: Box::new(e),
                })?;
        }
        for (k, v) in map {
            inner.validate(v).map_err(|e| TypeError::MapValueError {
                key: k.clone(),
                inner: Box::new(e),
            })?;
        }
        Ok(())
    }

    /// Validate a `Struct` variant: required fields, known field names, and
    /// recursively check each field's value.
    ///
    /// Block syntax produces `Value::Concrete(ConcreteValue::List([Value::Map(...)]))`, but bare
    /// `Struct` requires map assignment syntax (`attr = { ... }`); a
    /// `List` is rejected explicitly with `BlockSyntaxNotAllowed`.
    ///
    /// Phase 2 of RFC #2972: takes [`ConcreteValueRef`]; deferred values
    /// are filtered upstream by the dispatcher.
    fn validate_struct(&self, value: ConcreteValueRef<'_>) -> Result<(), TypeError> {
        let AttrTypeKind::Struct { name, fields } = &self.kind else {
            unreachable!("validate_struct called on non-Struct");
        };

        // Struct type rejects List (block syntax)
        if matches!(value, ConcreteValueRef::List(_)) {
            return Err(TypeError::BlockSyntaxNotAllowed {
                attribute: name.clone(),
            });
        }
        let ConcreteValueRef::Map(map) = value else {
            return Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: value.type_name().to_string(),
            });
        };

        // Check required fields
        for field in fields {
            if field.required && !map.contains_key(&field.name) {
                return Err(TypeError::StructFieldError {
                    field: field.name.clone(),
                    inner: Box::new(TypeError::MissingRequired {
                        name: field.name.clone(),
                    }),
                });
            }
        }
        // Type-check each field value. Use the same accepted-name map
        // (canonical + block_name alias) the path-tagged validator
        // builds; before #2214 this branch only knew canonical names
        // and silently rejected aliases that the LSP happily accepted.
        let accepted = build_accepted_field_map(fields);
        let canonical_names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        for (k, v) in map {
            if let Some(field) = accepted.get(k.as_str()) {
                field
                    .field_type
                    .validate(v)
                    .map_err(|e| TypeError::StructFieldError {
                        field: k.clone(),
                        inner: Box::new(e),
                    })?;
            } else {
                let suggestion = suggest_similar_name(k, &canonical_names);
                return Err(TypeError::UnknownStructField {
                    struct_name: name.clone(),
                    field: k.clone(),
                    suggestion,
                });
            }
        }
        Ok(())
    }

    /// Validate a `Union` variant: succeed if any member accepts the value.
    ///
    /// On failure, return the structurally-closest member's error rather
    /// than a generic `TypeMismatch`. "Closest" is measured by
    /// [`union_member_score`]: members whose outer constructor matches
    /// the input's (Map↔Struct, List↔List, String↔Enum, scalar↔
    /// scalar) outscore unrelated members. The first member at the
    /// maximum score wins — declaration order is preserved by the
    /// strict `>` comparison below, so the prior Map↔Struct preference
    /// still holds when multiple Struct members tie. When no member
    /// shares any structural similarity, fall through to `TypeMismatch`.
    /// See #2219.
    fn validate_union(&self, value: ConcreteValueRef<'_>) -> Result<(), TypeError> {
        let AttrTypeKind::Union(types) = &self.kind else {
            unreachable!("validate_union called on non-Union");
        };
        let mut best: Option<(u32, TypeError)> = None;
        for member in types {
            match member.validate_concrete(value) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let score = union_member_score(member, value);
                    if score > 0
                        && best
                            .as_ref()
                            .is_none_or(|(prev_score, _)| score > *prev_score)
                    {
                        best = Some((score, e));
                    }
                }
            }
        }
        Err(best.map(|(_, e)| e).unwrap_or(TypeError::TypeMismatch {
            expected: self.type_name(),
            got: value.type_name().to_string(),
        }))
    }

    pub fn type_name(&self) -> String {
        match &self.kind {
            AttrTypeKind::Bool => "Bool".to_string(),
            AttrTypeKind::Duration => "Duration".to_string(),
            AttrTypeKind::Enum { identity, .. } => identity.to_string(),
            AttrTypeKind::String {
                identity,
                pattern,
                length,
                ..
            } => custom_display_name(identity.as_ref(), pattern.as_deref(), length.as_ref()),
            AttrTypeKind::Int { identity, .. } => identity
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "Int".to_string()),
            AttrTypeKind::Float { identity, .. } => identity
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "Float".to_string()),
            AttrTypeKind::List {
                element_type: inner,
                ..
            } => format!("List<{}>", inner.type_name()),
            AttrTypeKind::Map { value: inner, .. } => format!("Map<{}>", inner.type_name()),
            AttrTypeKind::Struct { name, .. } => format!("Struct({})", name),
            AttrTypeKind::Union(types) => {
                let names: Vec<String> = types.iter().map(|t| t.type_name()).collect();
                names.join(" | ")
            }
            AttrTypeKind::Ref(name) => format!("Ref({name})"),
        }
    }

    /// Check if a type name is accepted by this type.
    /// For Union types, returns true if any member accepts the name.
    /// For other types, returns true if self.type_name() == name.
    pub fn accepts_type_name(&self, name: &str) -> bool {
        match &self.kind {
            AttrTypeKind::Union(types) => types.iter().any(|t| t.accepts_type_name(name)),
            _ => self.type_name() == name,
        }
    }

    /// Check if this type is a String-based Custom type.
    /// Used for cross-schema type compatibility: all String-based Custom types
    /// are considered compatible with each other.
    pub fn is_string_based_custom(&self) -> bool {
        matches!(
            &self.kind,
            AttrTypeKind::String {
                identity: Some(_),
                ..
            } | AttrTypeKind::String {
                pattern: Some(_),
                ..
            } | AttrTypeKind::String {
                length: Some(_),
                ..
            }
        )
    }

    /// Check if a value of `self`'s type can be assigned to a sink of
    /// `sink`'s type. Directional: narrowing source → wider sink is OK,
    /// but widening source → narrower sink is NG.
    ///
    /// Rules (first match wins):
    /// 1. Union sink: OK if source is assignable to any member.
    /// 2. Union source: OK iff source is assignable to sink for every member.
    /// 3. Custom→Custom with both `identity: Some`: the source's identity
    ///    must be [`TypeIdentity::assignable_to`] the sink's, any length
    ///    range must be contained by the sink's range, and the base types
    ///    must also be assignable. This is the final verdict for identified
    ///    custom types: pattern checks are subsumed by identity subsumption
    ///    and are not consulted in this arm, while length containment remains
    ///    a structural safety check. Directional per-axis subsumption — an
    ///    empty axis on the **sink** widens (any source matches), but an empty
    ///    axis on the **source** against a populated sink is rejected (no
    ///    evidence). So `aws.iam.Role.Arn` flows into `aws.Arn` (sink is
    ///    wider) but `aws.Arn` does not flow into `aws.iam.Role.Arn` (source
    ///    has no Role-specific evidence). `aws.Region` and `gcp.Region` are
    ///    rejected both ways (populated providers differ). Closes carina#3218.
    /// 4. Custom→Custom where at least one side has `identity: None`: check
    ///    pattern (literal equality) and length containment (source ⊆ sink),
    ///    then recurse on base. For both-identified pairs, see rule 3.
    /// 5. Custom source → non-Custom sink: recurse on `source.base`.
    /// 6. non-Custom source → Custom sink: NG (source has no proof of
    ///    satisfying the sink's identity/pattern/length).
    /// 7. Otherwise: same primitive type names.
    ///
    /// # Conservative pattern/length policy
    ///
    /// Pattern compatibility is decided by **literal string equality**,
    /// not by regex-language containment. Two `pattern: Some(...)` values
    /// that describe the same regex language but differ by a single
    /// character are still considered incompatible. Proving regex
    /// containment in the general case is undecidable for arbitrary
    /// PCRE-style patterns, so we err toward false negatives (a few
    /// rejected refs the user must split with an explicit cast) over
    /// false positives (assignment that compiles but fails at apply time).
    ///
    /// Length compatibility is a strict subset check: `sink.min ≤
    /// source.min` and `source.max ≤ sink.max`, treating absent bounds
    /// as unbounded on that side. A source with `length: None` cannot
    /// satisfy a sink with `length: Some(...)` — the source carries no
    /// proof of its values' length range. Likewise for `pattern: None`
    /// against `pattern: Some(_)`.
    ///
    /// **Do not loosen these checks** without a concrete plan to track
    /// regex-containment proofs through the type system. Loosening here
    /// re-introduces the silent-false-positive class that #2218 closed.
    pub fn is_assignable_to(&self, sink: &AttributeType) -> bool {
        use AttrTypeKind::*;
        if let Union(members) = &sink.kind {
            return members.iter().any(|m| self.is_assignable_to(m));
        }
        if let Union(members) = &self.kind {
            return members.iter().all(|m| m.is_assignable_to(sink));
        }
        match (&self.kind, &sink.kind) {
            (
                String {
                    identity: Some(s_id),
                    length: s_len,
                    pattern: _,
                    ..
                },
                String {
                    identity: Some(k_id),
                    length: k_len,
                    pattern: _,
                    ..
                },
            ) => s_id.assignable_to(k_id) && length_contains(s_len.as_ref(), k_len.as_ref()),
            (Enum { identity: s_id, .. }, Enum { identity: k_id, .. })
                if !s_id.assignable_to(k_id) =>
            {
                false
            }
            (
                Enum {
                    identity: s_id,
                    base: s_base,
                    ..
                },
                Enum {
                    identity: k_id,
                    base: k_base,
                    ..
                },
            ) => s_id.assignable_to(k_id) && s_base.is_assignable_to(k_base),
            // Anonymous source → identified sink has no proof of identity.
            (
                String { identity: None, .. }
                | Int { identity: None, .. }
                | Float { identity: None, .. },
                String {
                    identity: Some(_), ..
                }
                | Int {
                    identity: Some(_), ..
                }
                | Float {
                    identity: Some(_), ..
                },
            ) => false,
            (
                String {
                    pattern: s_pat,
                    length: s_len,
                    ..
                },
                String {
                    pattern: k_pat,
                    length: k_len,
                    ..
                },
            ) => {
                if !pattern_compatible(s_pat.as_deref(), k_pat.as_deref()) {
                    return false;
                }
                if !length_contains(s_len.as_ref(), k_len.as_ref()) {
                    return false;
                }
                true
            }
            (
                Int {
                    identity: Some(s_id),
                    range: s_range,
                    ..
                },
                Int {
                    identity: Some(k_id),
                    range: k_range,
                    ..
                },
            ) => s_id.assignable_to(k_id) && i64_range_contains(s_range.as_ref(), k_range.as_ref()),
            (Int { range: s_range, .. }, Int { range: k_range, .. }) => {
                i64_range_contains(s_range.as_ref(), k_range.as_ref())
            }
            (
                Float {
                    identity: Some(s_id),
                    range: s_range,
                    ..
                },
                Float {
                    identity: Some(k_id),
                    range: k_range,
                    ..
                },
            ) => s_id.assignable_to(k_id) && f64_range_contains(s_range.as_ref(), k_range.as_ref()),
            (Float { range: s_range, .. }, Float { range: k_range, .. }) => {
                f64_range_contains(s_range.as_ref(), k_range.as_ref())
            }
            (String { .. }, _) | (Int { .. }, _) | (Float { .. }, _)
                if self.type_name() != sink.type_name() =>
            {
                false
            }
            (_a, _b) => self.type_name() == sink.type_name(),
        }
    }
}

/// Build a structured [`TypeIdentity`] from a `(name, namespace)`
/// pair — the inverse of [`TypeIdentity::dotted_prefix`].
///
/// `namespace` is a dot-joined `provider.<segments...>` string of the
/// shape providers used to carry on the pre-#3222 `Custom.namespace` /
/// `Enum.namespace` field. The provider segment is the head and
/// every following segment goes into `segments`; `name` becomes the
/// `kind`. A bare `name` with no namespace yields
/// `TypeIdentity::bare(name)`.
///
/// Kept as a public helper because provider repositories
/// (`carina-aws-types`, `carina-provider-{aws,awscc}`) still build
/// `Enum.identity` / `Enum.identity` from the same dotted
/// inputs their codegen has carried for years; expressing that as
/// "the inverse of `dotted_prefix`" is clearer than re-splitting at
/// every call site.
pub fn enum_identity(name: &str, namespace: Option<&str>) -> TypeIdentity {
    match namespace {
        Some(ns) if !ns.is_empty() => {
            let mut parts = ns.split('.');
            let provider = parts.next().map(String::from);
            let segments: Vec<String> = parts.map(String::from).collect();
            TypeIdentity {
                provider,
                segments,
                kind: name.to_string(),
            }
        }
        _ => TypeIdentity::bare(name),
    }
}

fn validation_error_message(err: TypeError) -> String {
    match err {
        TypeError::ValidationFailed { message } => message,
        other => other.to_string(),
    }
}

fn validate_string_refinement(
    identity: Option<&TypeIdentity>,
    pattern: Option<&str>,
    length: Option<(Option<u64>, Option<u64>)>,
    s: &str,
) -> Result<(), TypeError> {
    let type_name = identity.map(|id| id.kind.clone());
    if let Some(pattern) = pattern
        && let Ok(re) = regex::Regex::new(pattern)
        && !re.is_match(s)
    {
        return Err(TypeError::PatternMismatch {
            value: s.to_string(),
            pattern: pattern.to_string(),
            attribute: None,
            type_name,
        });
    }
    if let Some((min, max)) = length {
        let count = s.chars().count();
        let count_u64 = count as u64;
        if min.is_some_and(|min| count_u64 < min) || max.is_some_and(|max| count_u64 > max) {
            return Err(TypeError::LengthOutOfRange {
                value: s.to_string(),
                length: count,
                min,
                max,
                attribute: None,
                type_name,
            });
        }
    }
    Ok(())
}

fn validate_list_length(
    length: Option<(Option<u64>, Option<u64>)>,
    count: usize,
) -> Result<(), TypeError> {
    if let Some((min, max)) = length {
        let count_u64 = count as u64;
        if min.is_some_and(|min| count_u64 < min) || max.is_some_and(|max| count_u64 > max) {
            return Err(TypeError::LengthOutOfRange {
                value: format!("{count} items"),
                length: count,
                min,
                max,
                attribute: None,
                type_name: Some("List".to_string()),
            });
        }
    }
    Ok(())
}

fn validate_int_range(
    range: Option<(Option<i64>, Option<i64>)>,
    value: i64,
) -> Result<(), TypeError> {
    if let Some((min, max)) = range
        && (min.is_some_and(|min| value < min) || max.is_some_and(|max| value > max))
    {
        return Err(TypeError::ValidationFailed {
            message: format!("value {value} is outside allowed range {min:?}..={max:?}"),
        });
    }
    Ok(())
}

fn validate_float_range(
    range: Option<(Option<f64>, Option<f64>)>,
    value: f64,
) -> Result<(), TypeError> {
    if let Some((min, max)) = range
        && (min.is_some_and(|min| value < min) || max.is_some_and(|max| value > max))
    {
        return Err(TypeError::ValidationFailed {
            message: format!("value {value} is outside allowed range {min:?}..={max:?}"),
        });
    }
    Ok(())
}

/// Rank a Union member against a runtime value by structural distance:
/// how close the member's outer constructor is to the input's. Higher
/// is closer; 0 means no shared structure. Used by `validate_union`
/// to pick which member's error message to surface on failure (#2219).
///
/// Map↔Struct stays the highest (preserves the prior heuristic). The
/// other constructor pairs — Map↔Map, List↔List, String↔String /
/// Enum, scalar↔scalar — get the next tier. `Custom` defers to
/// its declared `base`, so a Union of `Int | positive_int()`
/// validating an `Int` input still surfaces the predicate's message.
/// `Union` members recurse and take the best inner score so nested
/// unions still produce a meaningful error.
///
/// On a tie, the first member at the maximum wins — `validate_union`
/// uses strict `>` so declaration order is preserved.
pub(crate) fn union_member_score(member: &AttributeType, value: ConcreteValueRef<'_>) -> u32 {
    use AttrTypeKind as AT;
    match (&member.kind, value) {
        // Map↔Struct: the original heuristic. Highest score so a
        // Struct member's "Unknown field 'x'" wins over a sibling's
        // generic `TypeMismatch`.
        (AT::Struct { .. }, ConcreteValueRef::Map(_)) => 100,
        // Same-constructor match — second tier.
        //
        // `Enum` matches both `String` (quoted literal form) and
        // `EnumIdentifier` (identifier form, carina#2986). The strict
        // validator rejects the `String` shape inside `validate_enum`
        // and surfaces `StringLiteralExpectedEnum`; the union picker
        // still routes through this member so that error reaches the
        // caller instead of a generic `TypeMismatch`.
        (AT::Map { .. }, ConcreteValueRef::Map(_))
        | (AT::String { .. }, ConcreteValueRef::String(_))
        | (AT::Int { .. }, ConcreteValueRef::Int(_))
        | (AT::Float { .. }, ConcreteValueRef::Float(_))
        | (AT::Bool, ConcreteValueRef::Bool(_))
        | (AT::Enum { .. }, ConcreteValueRef::String(_))
        | (AT::Enum { .. }, ConcreteValueRef::EnumIdentifier(_)) => 80,
        // List↔List: peek at the first element's structural match
        // against the member's inner type so `List<Struct>` outranks
        // `List<String>` for an input like `[{...}]`. The inner
        // contribution is halved so arbitrarily deep nesting can't
        // exceed the Map↔Struct tier (100). Empty lists fall back to
        // the bare same-constructor score. Inner peek requires
        // re-projecting the first element through `as_concrete()` —
        // a deferred element contributes no bonus (the projection
        // returns `None`).
        (
            AT::List {
                element_type: inner,
                ..
            },
            ConcreteValueRef::List(items),
        ) => {
            let inner_bonus = items
                .first()
                .and_then(|first| first.as_concrete())
                .map(|first| union_member_score(inner, first) / 2)
                .unwrap_or(0);
            80 + inner_bonus
        }
        // Nested Union: recurse and take the best inner match.
        (AT::Union(inner), v) => inner
            .iter()
            .map(|m| union_member_score(m, v))
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}

/// Pick the Union member a concrete value structurally "is", using the
/// **same scoring function** [`validate_union`] uses for error
/// attribution ([`union_member_score`], #2219). Reusing the one scorer
/// — rather than authoring a second parallel shape predicate — is
/// deliberate (carina#3080 design): the canonicalizer
/// ([`crate::value::canonicalize_with_type`]'s `Union` arm) and the
/// validator must not drift apart in how they judge a value's member.
/// There is one ranking function here, not two kept in sync by review.
///
/// Selection rules:
/// - Project the value to its concrete shape (deferred values have no
///   shape → `None`, identity fallthrough at the call site).
/// - Score **every** member; the **strict-max** member wins, so on a
///   tie the earliest-declared member is kept (strict `>`, the same
///   declaration-order preference `validate_union` applies, e.g.
///   `string_or_principal_struct`'s deliberate Struct-before-String).
/// - All members score `0` (no shared structure) → `None`. The caller
///   treats `None` as identity (never guess-coerce); a `Map` value can
///   never select a `String` member because `(String, Map)` scores `0`.
///
/// Note the deliberate scope difference from `validate_union`: that
/// function scores only members whose `validate_concrete` *failed*
/// (the first member that validates `Ok` short-circuits — its job is
/// to attribute an *error* message). This function scores *all*
/// members because its job is the opposite: pick the structurally
/// best-matching member to canonicalize *into*, whether or not it
/// would also validate. For the IAM-union shapes this fix targets
/// (`Union[Struct, String]`, `Union[String, List<String>]`) the
/// members are shape-disjoint — exactly one scores `> 0` for a given
/// value — so the two functions select the same member regardless;
/// the broader scoring only matters for hypothetical overlapping
/// unions, where "best structural match" is the correct rule for a
/// canonicalizer (and a no-op for `None`-on-tie safety).
pub(crate) fn select_union_member<'a>(
    members: &'a [AttributeType],
    value: &Value,
) -> Option<&'a AttributeType> {
    let projected = value.as_concrete()?;
    let mut best: Option<(u32, &AttributeType)> = None;
    for member in members {
        let score = union_member_score(member, projected);
        if score > 0 && best.as_ref().is_none_or(|(prev, _)| score > *prev) {
            best = Some((score, member));
        }
    }
    best.map(|(_, m)| m)
}

/// Map every accepted field name to its canonical [`StructField`].
/// Both the canonical `name` and any `block_name` alias resolve to the
/// same field, so users can write either form interchangeably.
///
/// Used by both [`AttrTypeKind::validate`] (single-shot) and
/// [`AttrTypeKind::validate_collect`] (path-tagged) so the two paths
/// agree on which keys are accepted (#2214 — the LSP previously did
/// this alias resolution itself, which let the two validators drift).
pub(crate) fn build_accepted_field_map(fields: &[StructField]) -> HashMap<&str, &StructField> {
    let mut accepted: HashMap<&str, &StructField> = HashMap::new();
    for f in fields {
        accepted.insert(f.name.as_str(), f);
        if let Some(block) = f.block_name.as_deref() {
            accepted.insert(block, f);
        }
    }
    accepted
}

/// Source length is contained in sink length (narrow ⊆ wide).
/// Missing bounds are treated as unbounded on that side.
fn length_contains(
    source: Option<&(Option<u64>, Option<u64>)>,
    sink: Option<&(Option<u64>, Option<u64>)>,
) -> bool {
    let Some((s_min, s_max)) = source else {
        return sink.is_none();
    };
    let Some((k_min, k_max)) = sink else {
        return true;
    };
    let s_min = s_min.unwrap_or(0);
    let s_max = s_max.unwrap_or(u64::MAX);
    let k_min = k_min.unwrap_or(0);
    let k_max = k_max.unwrap_or(u64::MAX);
    k_min <= s_min && s_max <= k_max
}

fn pattern_compatible(source: Option<&str>, sink: Option<&str>) -> bool {
    match (source, sink) {
        (_, None) => true,
        (Some(source), Some(sink)) => source == sink,
        (None, Some(_)) => false,
    }
}

fn i64_range_contains(
    source: Option<&(Option<i64>, Option<i64>)>,
    sink: Option<&(Option<i64>, Option<i64>)>,
) -> bool {
    let Some((s_min, s_max)) = source else {
        return sink.is_none();
    };
    let Some((k_min, k_max)) = sink else {
        return true;
    };
    let s_min = s_min.unwrap_or(i64::MIN);
    let s_max = s_max.unwrap_or(i64::MAX);
    let k_min = k_min.unwrap_or(i64::MIN);
    let k_max = k_max.unwrap_or(i64::MAX);
    k_min <= s_min && s_max <= k_max
}

fn f64_range_contains(
    source: Option<&(Option<f64>, Option<f64>)>,
    sink: Option<&(Option<f64>, Option<f64>)>,
) -> bool {
    let Some((s_min, s_max)) = source else {
        return sink.is_none();
    };
    let Some((k_min, k_max)) = sink else {
        return true;
    };
    let s_min = s_min.unwrap_or(f64::NEG_INFINITY);
    let s_max = s_max.unwrap_or(f64::INFINITY);
    let k_min = k_min.unwrap_or(f64::NEG_INFINITY);
    let k_max = k_max.unwrap_or(f64::INFINITY);
    k_min <= s_min && s_max <= k_max
}

impl fmt::Display for AttributeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.type_name())
    }
}

fn custom_display_name(
    identity: Option<&TypeIdentity>,
    pattern: Option<&str>,
    length: Option<&(Option<u64>, Option<u64>)>,
) -> String {
    if let Some(id) = identity {
        return id.to_string();
    }
    let mut s = String::from("String");
    let has_pattern = pattern.is_some();
    let has_length = length.is_some();
    if has_pattern || has_length {
        s.push('(');
        if has_pattern {
            s.push_str("pattern");
        }
        if let Some((min, max)) = length {
            if has_pattern {
                s.push_str(", ");
            }
            s.push_str(&format!(
                "len: {}",
                length_display(min.as_ref(), max.as_ref())
            ));
        }
        s.push(')');
    }
    s
}

fn length_display(min: Option<&u64>, max: Option<&u64>) -> String {
    match (min, max) {
        (Some(lo), Some(hi)) => format!("{}..={}", lo, hi),
        (Some(lo), None) => format!("{}..", lo),
        (None, Some(hi)) => format!("..={}", hi),
        (None, None) => "..".to_string(),
    }
}

fn enum_value_matches(input: &str, expected: &str) -> bool {
    input == expected
        || input.eq_ignore_ascii_case(expected)
        || input.replace('_', "-").eq_ignore_ascii_case(expected)
}

fn enum_expected_variants(
    namespace: Option<&str>,
    type_name: &str,
    values: &[String],
    dsl_aliases: &[(String, String)],
) -> Vec<ExpectedEnumVariant> {
    let dsl_map = DslMap::new(dsl_aliases, None);
    let mut expected = Vec::new();
    let mut seen_values = HashSet::new();
    let mut canonical_dsl_values = HashSet::new();

    for value in values {
        let dsl_value = dsl_map.dsl_for(value);
        let owned = dsl_value.into_owned();
        canonical_dsl_values.insert(owned.clone());
        if !seen_values.contains(&owned) {
            expected.push(ExpectedEnumVariant::from_namespaced(
                namespace, type_name, &owned, false,
            ));
            seen_values.insert(owned);
        }
    }

    for (_api, dsl_value) in dsl_aliases {
        if !canonical_dsl_values.contains(dsl_value) && seen_values.insert(dsl_value.clone()) {
            expected.push(ExpectedEnumVariant::from_namespaced(
                namespace, type_name, dsl_value, true,
            ));
        }
    }

    expected
}

/// Render the `InvalidEnumVariant` message with the richest available
/// context. Presence of `attribute` and `type_name` is independent — both,
/// either, or neither may be set. `expected` is rendered as-is; callers are
/// responsible for passing fully-qualified variants for namespaced enums.
/// Reshape an error from `AttrTypeKind::validate` into a shape-mismatch
/// diagnostic when the attribute's value came from a quoted string literal
/// and the schema expects an enum-shaped identifier (a `Enum`, or a
/// namespaced `Custom` type).
///
/// For `Enum`, `into_string_literal_diagnostic` does the work since
/// the underlying error already carries type name and variants.
fn reshape_for_string_literal(
    tagged: TypeError,
    attr_type: &AttributeType,
    _value: &Value,
    _attr_name: &str,
) -> TypeError {
    // Enum: the error already has enough structure to reshape cleanly.
    if matches!(&attr_type.kind, AttrTypeKind::Enum { .. }) {
        return tagged.into_string_literal_diagnostic();
    }

    tagged
}

fn join_expected(expected: &[ExpectedEnumVariant]) -> String {
    expected
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_string_literal_expected_enum(
    user_typed: &str,
    attribute: Option<&str>,
    type_name: &str,
    expected: &[ExpectedEnumVariant],
    extra_message: Option<&str>,
) -> String {
    let target = match attribute {
        Some(a) => format!("'{}' ({})", a, type_name),
        None => type_name.to_string(),
    };
    // When `expected` is empty (Custom-namespaced reshape path) the
    // validator's own message stands in for the variants list, so the
    // tail reads "Use one of: <validator message>" — byte-identical to
    // the pre-#2220 string-vec rendering.
    let tail = if expected.is_empty() {
        extra_message.unwrap_or("").to_string()
    } else {
        join_expected(expected)
    };
    format!(
        "{} expects an enum identifier, got a string literal \"{}\". Use one of: {}",
        target, user_typed, tail
    )
}

fn format_invalid_enum(
    value: &str,
    attribute: Option<&str>,
    type_name: Option<&str>,
    expected: &[ExpectedEnumVariant],
) -> String {
    let joined = join_expected(expected);
    let qualifier = match (attribute, type_name) {
        (Some(a), Some(t)) => format!(" for '{}' ({})", a, t),
        (Some(a), None) => format!(" for '{}'", a),
        (None, Some(t)) => format!(" for {}", t),
        (None, None) => String::new(),
    };
    if qualifier.is_empty() {
        format!(
            "Invalid enum variant '{}', expected one of: {}",
            value, joined
        )
    } else {
        format!(
            "Invalid value '{}'{}: expected one of {}",
            value, qualifier, joined
        )
    }
}

fn format_invalid_value_intro(
    value: &str,
    attribute: Option<&str>,
    type_name: Option<&str>,
) -> String {
    match (attribute, type_name) {
        (Some(a), Some(t)) => format!("Invalid value '{}' for '{}' ({})", value, a, t),
        (Some(a), None) => format!("Invalid value '{}' for '{}'", value, a),
        (None, Some(t)) => format!("Invalid {} value '{}'", t, value),
        (None, None) => format!("Invalid value '{}'", value),
    }
}

fn format_pattern_mismatch(
    value: &str,
    pattern: &str,
    attribute: Option<&str>,
    type_name: Option<&str>,
) -> String {
    format!(
        "{}: does not match required pattern /{}/",
        format_invalid_value_intro(value, attribute, type_name),
        pattern
    )
}

fn format_length_out_of_range(
    value: &str,
    length: usize,
    min: Option<u64>,
    max: Option<u64>,
    attribute: Option<&str>,
    type_name: Option<&str>,
) -> String {
    let min = min.map(|v| v.to_string()).unwrap_or_default();
    let max = max.map(|v| v.to_string()).unwrap_or_default();
    format!(
        "{}: length {} is outside allowed range [{}, {}]",
        format_invalid_value_intro(value, attribute, type_name),
        length,
        min,
        max
    )
}

/// A DSL-spelled enum value that can be typed in a `.crn` file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DslSpelling(String);

impl DslSpelling {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DslSpelling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for DslSpelling {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for DslSpelling {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for DslSpelling {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

impl PartialEq<DslSpelling> for str {
    fn eq(&self, other: &DslSpelling) -> bool {
        self == other.0
    }
}

impl PartialEq<DslSpelling> for &str {
    fn eq(&self, other: &DslSpelling) -> bool {
        *self == other.0
    }
}

impl PartialEq<DslSpelling> for String {
    fn eq(&self, other: &DslSpelling) -> bool {
        *self == other.0
    }
}

/// One candidate variant carried by `TypeError::InvalidEnumVariant` and
/// `TypeError::StringLiteralExpectedEnum`. Splits a fully-qualified enum
/// identifier into structured pieces so IDE / LSP code actions can
/// synthesize a fix without re-parsing the rendered string. The `Display`
/// impl re-renders the same form the user should type — fully-qualified
/// (`awscc.sso.Assignment.TargetType.AWS_ACCOUNT`) when `provider` is set,
/// bare (`fast`) otherwise. See #2220.
///
/// Serializes via `serde` so the LSP can ferry the structured data
/// through `Diagnostic.data` for `textDocument/codeAction` consumption
/// (#2309).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExpectedEnumVariant {
    /// Provider segment of the namespace (`"awscc"` for
    /// `awscc.sso.Assignment.TargetType.AWS_ACCOUNT`). `None` for
    /// non-namespaced enums whose variants are bare identifiers.
    pub provider: Option<String>,
    /// Service / resource segments between the provider and the enum
    /// type name (`["sso", "Assignment"]` for the example above). Empty
    /// for non-namespaced enums.
    pub segments: Vec<String>,
    /// Name of the enum type (`"TargetType"`).
    pub type_name: String,
    /// The variant value in the DSL spelling the user can type.
    pub value: DslSpelling,
    /// `true` when this entry came from a `to_dsl` alias rather than the
    /// canonical provider-side variant. Code actions should prefer the
    /// canonical form (`is_alias = false`) when offering a fix.
    pub is_alias: bool,
}

impl ExpectedEnumVariant {
    /// `namespace` head becomes `provider`; the rest become `segments`.
    /// `None` produces a bare-form variant.
    ///
    /// Callers must pass a DSL-spelled value; the sole production
    /// producer `enum_expected_variants` guarantees this via
    /// `DslMap::dsl_for`.
    pub fn from_namespaced(
        namespace: Option<&str>,
        type_name: &str,
        value: &str,
        is_alias: bool,
    ) -> Self {
        let (provider, segments) = match namespace {
            Some(ns) => {
                let mut parts = ns.split('.').map(String::from);
                let head = parts.next();
                (head, parts.collect())
            }
            None => (None, Vec::new()),
        };
        Self {
            provider,
            segments,
            type_name: type_name.to_string(),
            value: DslSpelling(value.to_string()),
            is_alias,
        }
    }
}

impl fmt::Display for ExpectedEnumVariant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.provider {
            Some(provider) => {
                write!(f, "{}", provider)?;
                for seg in &self.segments {
                    write!(f, ".{}", seg)?;
                }
                write!(f, ".{}.{}", self.type_name, self.value)
            }
            None => write!(f, "{}", self.value),
        }
    }
}

/// Type error
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TypeError {
    #[error("Type mismatch: expected {expected}, got {got}")]
    TypeMismatch { expected: String, got: String },

    #[error(
        "{}",
        format_invalid_enum(value, attribute.as_deref(), type_name.as_deref(), expected)
    )]
    InvalidEnumVariant {
        value: String,
        /// Attribute the value was assigned to (e.g. `"target_id"`). Set by
        /// caller-side wrapping (see `TypeError::with_attribute`) — the
        /// `AttrTypeKind::validate` primitive itself doesn't know the name.
        attribute: Option<String>,
        /// Name of the `Enum` type that was being matched against
        /// (e.g. `"TargetType"`). Set when available so the diagnostic can
        /// tell the reader which enum is expected; None for callers that
        /// build the error by hand without type context.
        type_name: Option<String>,
        /// Allowed variants as structured records. The `Display` impl on
        /// each entry renders the form the user should type
        /// (fully-qualified for namespaced enums, bare otherwise);
        /// programmatic consumers (LSP code actions, `carina explain-error`)
        /// can read the structured fields directly. See #2220.
        expected: Vec<ExpectedEnumVariant>,
    },

    #[error(
        "{}",
        format_pattern_mismatch(value, pattern, attribute.as_deref(), type_name.as_deref())
    )]
    PatternMismatch {
        value: String,
        pattern: String,
        attribute: Option<String>,
        type_name: Option<String>,
    },

    #[error(
        "{}",
        format_length_out_of_range(value, *length, *min, *max, attribute.as_deref(), type_name.as_deref())
    )]
    LengthOutOfRange {
        value: String,
        length: usize,
        min: Option<u64>,
        max: Option<u64>,
        attribute: Option<String>,
        type_name: Option<String>,
    },

    /// The value was written in the source as a quoted string literal
    /// (e.g. `target_type = "aaa"`) on an attribute whose type is an enum
    /// of namespaced identifiers. This is a shape mismatch — the user
    /// needs to drop the quotes and type one of the enum identifiers —
    /// and is reported separately from `InvalidEnumVariant` so the
    /// message can explain the form, not just list valid variants.
    /// See #2094.
    #[error(
        "{}",
        format_string_literal_expected_enum(user_typed, attribute.as_deref(), type_name, expected, extra_message.as_deref())
    )]
    StringLiteralExpectedEnum {
        /// The string the user actually typed between the quotes
        /// (e.g. `"aaa"`).
        user_typed: String,
        /// Attribute the value was assigned to (e.g. `"target_type"`).
        attribute: Option<String>,
        /// Name of the enum type the value was being matched against
        /// (e.g. `"TargetType"`). Always set for this variant — callers
        /// only build it when they already know the enum type.
        type_name: String,
        /// Allowed variants as structured records. Same shape as
        /// `InvalidEnumVariant.expected`. May be empty when the upstream
        /// schema is `Custom` and the validator does not enumerate
        /// variants — in that case `extra_message` carries the
        /// validator's text instead. See #2220.
        expected: Vec<ExpectedEnumVariant>,
        /// Free-form text used as the message tail **only when
        /// `expected` is empty** — i.e. the `Custom` namespaced reshape
        /// path, where the validator does not enumerate variants. When
        /// `expected` is non-empty, this field is silently ignored by
        /// the renderer; callers should not set both at once.
        extra_message: Option<String>,
    },

    #[error("Validation failed: {message}")]
    ValidationFailed { message: String },

    #[error("Resource validation failed: {message}")]
    ResourceValidationFailed {
        message: String,
        /// Optional attribute name for precise diagnostic positioning.
        attribute: Option<String>,
    },

    #[error("Required attribute '{name}' is missing")]
    MissingRequired { name: String },

    #[error("Unknown attribute '{name}'{}", suggestion.as_ref().map(|s| format!(", did you mean '{}'?", s)).unwrap_or_default())]
    UnknownAttribute {
        name: String,
        suggestion: Option<String>,
    },

    #[error("Unknown field '{field}' in {struct_name}{}", suggestion.as_ref().map(|s| format!(", did you mean '{}'?", s)).unwrap_or_default())]
    UnknownStructField {
        struct_name: String,
        field: String,
        suggestion: Option<String>,
    },

    #[error("List item at index {index}: {inner}")]
    ListItemError { index: usize, inner: Box<TypeError> },

    #[error("Map key '{key}': {inner}")]
    MapKeyError { key: String, inner: Box<TypeError> },

    #[error("Map value for key '{key}': {inner}")]
    MapValueError { key: String, inner: Box<TypeError> },

    #[error("Struct field '{field}': {inner}")]
    StructFieldError {
        field: String,
        inner: Box<TypeError>,
    },

    #[error("'{attribute}' cannot use block syntax; use map assignment: {attribute} = {{ ... }}")]
    BlockSyntaxNotAllowed { attribute: String },
}

impl TypeError {
    /// Attach an attribute name to errors whose diagnostics include an
    /// attribute context; other variants return `self` unchanged.
    ///
    /// Callers that know which attribute produced the error (e.g. the
    /// attribute loop in `ResourceSchema::validate`) wrap the primitive
    /// error before it reaches CLI/LSP diagnostic text. This keeps
    /// `AttrTypeKind::validate` unaware of attribute names while still
    /// letting the final message say `for 'target_id'`.
    ///
    /// See #2098. Adding the same slot to `ValidationFailed` /
    /// `TypeMismatch` is tracked as future work.
    #[must_use]
    pub fn with_attribute(mut self, attribute: impl Into<String>) -> Self {
        match &mut self {
            TypeError::InvalidEnumVariant {
                attribute: attr_slot,
                ..
            }
            | TypeError::PatternMismatch {
                attribute: attr_slot,
                ..
            }
            | TypeError::LengthOutOfRange {
                attribute: attr_slot,
                ..
            }
            | TypeError::StringLiteralExpectedEnum {
                attribute: attr_slot,
                ..
            } => {
                *attr_slot = Some(attribute.into());
            }
            _ => {}
        }
        self
    }

    /// If this error describes an enum-variant mismatch on a value that
    /// was originally written as a quoted string literal, reshape it into
    /// `StringLiteralExpectedEnum` so the message reports the form
    /// mismatch rather than a missing variant. Returns the error
    /// unchanged when the variant doesn't carry a known enum type.
    #[must_use]
    pub fn into_string_literal_diagnostic(self) -> Self {
        match self {
            TypeError::InvalidEnumVariant {
                value,
                attribute,
                type_name: Some(type_name),
                expected,
            } => TypeError::StringLiteralExpectedEnum {
                user_typed: value,
                attribute,
                type_name,
                expected,
                extra_message: None,
            },
            other => other,
        }
    }
}

impl Value {
    fn type_name(&self) -> String {
        match self {
            Value::Concrete(ConcreteValue::String(_)) => "String".to_string(),
            Value::Concrete(ConcreteValue::EnumIdentifier(_)) => "EnumIdentifier".to_string(),
            Value::Concrete(ConcreteValue::Int(_)) => "Int".to_string(),
            Value::Concrete(ConcreteValue::Float(_)) => "Float".to_string(),
            Value::Concrete(ConcreteValue::Bool(_)) => "Bool".to_string(),
            Value::Concrete(ConcreteValue::Duration(_)) => "Duration".to_string(),
            Value::Concrete(ConcreteValue::List(_)) => "List".to_string(),
            Value::Concrete(ConcreteValue::StringList(_)) => "StringList".to_string(),
            Value::Concrete(ConcreteValue::Map(_)) => "Map".to_string(),
            Value::Deferred(DeferredValue::ResourceRef { path }) => {
                format!("ResourceRef({})", path.to_dot_string())
            }
            Value::Deferred(DeferredValue::BindingRef { binding }) => {
                format!("BindingRef({})", binding)
            }
            Value::Deferred(DeferredValue::Interpolation(_)) => "Interpolation".to_string(),
            Value::Deferred(DeferredValue::FunctionCall { name, .. }) => {
                format!("FunctionCall({})", name)
            }
            Value::Deferred(DeferredValue::Secret(_)) => "Secret".to_string(),
            Value::Deferred(DeferredValue::Unknown(_)) => "Unknown".to_string(),
        }
    }
}

impl ConcreteValueRef<'_> {
    /// Concrete-axis name used in type-mismatch diagnostics. Mirrors
    /// the labels [`Value::type_name`] produces for the same axis.
    fn type_name(&self) -> &'static str {
        match self {
            ConcreteValueRef::String(_) => "String",
            ConcreteValueRef::EnumIdentifier(_) => "EnumIdentifier",
            ConcreteValueRef::Int(_) => "Int",
            ConcreteValueRef::Float(_) => "Float",
            ConcreteValueRef::Bool(_) => "Bool",
            ConcreteValueRef::Duration(_) => "Duration",
            ConcreteValueRef::List(_) => "List",
            ConcreteValueRef::StringList(_) => "StringList",
            ConcreteValueRef::Map(_) => "Map",
        }
    }

    /// Materialize an owned [`Value`] from this borrow. Used at the
    /// few helper boundaries (`expand_enum_shorthand`, list-of-string
    /// inner re-validation) that still operate on `&Value` and cannot
    /// be migrated to the projection without a wider API change.
    fn to_owned_value(self) -> Value {
        match self {
            ConcreteValueRef::String(s) => Value::Concrete(ConcreteValue::String(s.to_string())),
            ConcreteValueRef::EnumIdentifier(s) => {
                Value::Concrete(ConcreteValue::EnumIdentifier(s.to_string()))
            }
            ConcreteValueRef::Int(n) => Value::Concrete(ConcreteValue::Int(n)),
            ConcreteValueRef::Float(f) => Value::Concrete(ConcreteValue::Float(f)),
            ConcreteValueRef::Bool(b) => Value::Concrete(ConcreteValue::Bool(b)),
            ConcreteValueRef::Duration(d) => Value::Concrete(ConcreteValue::Duration(d)),
            ConcreteValueRef::List(items) => Value::Concrete(ConcreteValue::List(items.to_vec())),
            ConcreteValueRef::StringList(items) => {
                Value::Concrete(ConcreteValue::StringList(items.to_vec()))
            }
            ConcreteValueRef::Map(map) => Value::Concrete(ConcreteValue::Map(map.clone())),
        }
    }
}

/// Common validation patterns for resource schemas
pub mod validators {
    use super::*;

    /// Helper function to validate that exactly one of the specified fields is present.
    /// Returns `Ok(())` if exactly one field is present, `Err` otherwise.
    ///
    /// Use this in custom validator functions for mutually exclusive required fields.
    ///
    /// # Example
    /// ```
    /// use std::collections::HashMap;
    /// use carina_core::resource::{ConcreteValue, DeferredValue, Value};
    /// use carina_core::schema::{validators, TypeError};
    ///
    /// fn my_validator(attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
    ///     validators::validate_exclusive_required(attributes, &["option_a", "option_b"])
    /// }
    /// ```
    pub fn validate_exclusive_required(
        attributes: &HashMap<String, Value>,
        fields: &[&str],
    ) -> Result<(), Vec<TypeError>> {
        let present_fields: Vec<&str> = fields
            .iter()
            .filter(|&&name| attributes.contains_key(name))
            .copied()
            .collect();

        match present_fields.len() {
            0 => Err(vec![TypeError::ResourceValidationFailed {
                message: format!("Exactly one of [{}] must be specified", fields.join(", ")),
                attribute: None,
            }]),
            1 => Ok(()),
            _ => Err(vec![TypeError::ResourceValidationFailed {
                message: format!(
                    "Only one of [{}] can be specified, but found: {}",
                    fields.join(", "),
                    present_fields.join(", ")
                ),
                attribute: present_fields.first().map(|s| s.to_string()),
            }]),
        }
    }
}

/// Completion value for LSP completions
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletionValue {
    /// The value to insert (e.g., "aws.vpc.InstanceTenancy.default")
    pub value: String,
    /// Description shown in completion popup
    pub description: String,
}

impl CompletionValue {
    pub fn new(value: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: description.into(),
        }
    }
}

/// Attribute schema
#[derive(Debug, Clone)]
pub struct AttributeSchema {
    pub name: String,
    pub attr_type: AttributeType,
    pub required: bool,
    pub default: Option<Value>,
    pub description: Option<String>,
    /// Completion values for this attribute (used by LSP)
    pub completions: Option<Vec<CompletionValue>>,
    /// Provider-side property name (e.g., "VpcId" for AWS Cloud Control)
    pub provider_name: Option<String>,
    /// Whether this attribute is create-only (immutable after creation)
    pub create_only: bool,
    /// Whether this attribute is read-only (set by the provider, cannot be updated)
    pub read_only: bool,
    /// Override for removability detection.
    /// `None` = auto-detect: removable if `!required && !create_only`.
    /// `Some(false)` = explicitly non-removable (e.g., region inherited from provider).
    /// Only removable attributes trigger removal detection in the differ.
    pub removable: Option<bool>,
    /// Alternative block name for repeated block syntax (e.g., "operating_region" for "operating_regions")
    pub block_name: Option<String>,
    /// Whether this attribute is write-only (not returned by the provider's read API).
    /// Write-only attributes are sent to the provider during create/update but may not
    /// appear in read responses. This is NOT related to sensitive/secret values — it
    /// indicates a CloudFormation `writeOnlyProperties` attribute.
    pub write_only: bool,
    /// Whether this attribute contributes to anonymous resource identity.
    /// Identity attributes are included in the hash when computing anonymous resource
    /// identifiers, alongside create-only attributes. Use this for attributes that
    /// distinguish resources of the same type that share the same create-only values
    /// (e.g., Route 53 RecordSet `type` differentiates A vs AAAA records with the
    /// same name and hosted zone).
    pub identity: bool,
    /// Whether the value of this attribute is populated by the provider
    /// asynchronously *after* the Create call returns. Downstream
    /// resources that read this attribute via a chained access
    /// (`<binding>.<this_attr>...`) without a synchronizing `wait`
    /// block on the binding will be rejected at validate time
    /// (carina#3034). Examples:
    /// - ACM `Certificate.status` (PENDING_VALIDATION → ISSUED)
    /// - CloudFront `Distribution.domain_name`
    /// - RDS `DBInstance.endpoint`
    ///
    /// Independent of `read_only`: a deferred-populate attribute may
    /// also be `read_only` (the user cannot set it), but a `read_only`
    /// attribute is not necessarily deferred-populate (it may be
    /// populated synchronously, e.g. an ARN echoed back by Create).
    pub deferred_populate: bool,
}

impl AttributeSchema {
    pub fn new(name: impl Into<String>, attr_type: AttributeType) -> Self {
        Self {
            name: name.into(),
            attr_type,
            required: false,
            default: None,
            description: None,
            completions: None,
            provider_name: None,
            create_only: false,
            read_only: false,
            removable: None,
            block_name: None,
            write_only: false,
            identity: false,
            deferred_populate: false,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn create_only(mut self) -> Self {
        self.create_only = true;
        self
    }

    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub fn write_only(mut self) -> Self {
        self.write_only = true;
        self
    }

    pub fn identity(mut self) -> Self {
        self.identity = true;
        self
    }

    /// Mark this attribute as populated asynchronously by the provider
    /// after Create. See the field doc on `deferred_populate`.
    pub fn deferred_populate(mut self) -> Self {
        self.deferred_populate = true;
        self
    }

    pub fn removable(mut self) -> Self {
        self.removable = Some(true);
        self
    }

    pub fn non_removable(mut self) -> Self {
        self.removable = Some(false);
        self
    }

    /// Whether this attribute can be removed from infrastructure.
    /// Auto-detected: optional (not required), mutable (not create-only), and writable
    /// (not read-only) attributes are removable by default. Can be overridden with
    /// `.removable()` or `.non_removable()`.
    pub fn is_removable(&self) -> bool {
        self.removable
            .unwrap_or(!self.required && !self.create_only && !self.read_only)
    }

    pub fn with_default(mut self, value: Value) -> Self {
        self.default = Some(value);
        self
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_completions(mut self, completions: Vec<CompletionValue>) -> Self {
        self.completions = Some(completions);
        self
    }

    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.provider_name = Some(name.into());
        self
    }

    pub fn with_block_name(mut self, name: impl Into<String>) -> Self {
        self.block_name = Some(name.into());
        self
    }
}

/// Per-resource operational configuration for provider-specific timeouts and retries.
///
/// Providers can set these on individual resource schemas to override default
/// polling/retry behavior. This avoids hardcoding resource-type string matches
/// in provider implementations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperationConfig {
    /// Polling timeout for delete operations in seconds.
    /// Default: provider-specific (e.g., 600s for CloudControl).
    pub delete_timeout_secs: Option<u64>,
    /// Maximum retry attempts for retryable delete errors.
    /// Default: provider-specific (e.g., 12 for CloudControl).
    pub delete_max_retries: Option<u32>,
    /// Polling timeout for create operations in seconds.
    /// Default: provider-specific (e.g., 600s for CloudControl).
    pub create_timeout_secs: Option<u64>,
    /// Maximum retry attempts for retryable create errors.
    /// Default: provider-specific (e.g., 12 for CloudControl).
    pub create_max_retries: Option<u32>,
}

/// Classification of a resource schema: managed (full CRUD lifecycle) vs
/// data source (read-only lookup of existing infrastructure).
///
/// See `notes/specs/2026-05-02-resource-vs-data-source-design.md` (Decision 1-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaKind {
    Resource,
    DataSource,
}

pub(crate) fn struct_fields_with_defs<'a>(
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Option<&'a [StructField]> {
    match attr_type.resolve_refs_with_defs(defs).as_attr().kind() {
        AttrTypeKind::Struct { fields, .. } => Some(fields.as_slice()),
        _ => None,
    }
}

pub(crate) fn union_members_with_defs<'a>(
    attr_type: &'a AttributeType,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
) -> Option<&'a [AttributeType]> {
    match attr_type.resolve_refs_with_defs(defs).as_attr().kind() {
        AttrTypeKind::Union(members) => Some(members.as_slice()),
        _ => None,
    }
}

fn enum_identity_matches_input(
    input: &str,
    enum_name: &str,
    identity: Option<&TypeIdentity>,
) -> bool {
    if input.strip_prefix(&format!("{enum_name}.")).is_some() {
        return true;
    }
    if let Some(identity) = identity {
        return input
            .strip_prefix(&format!("{identity}."))
            .is_some_and(|value| !value.is_empty());
    }
    false
}

/// Resource schema
#[derive(Debug, Clone)]
pub struct ResourceSchema {
    pub resource_type: String,
    pub attributes: HashMap<String, AttributeSchema>,
    pub description: Option<String>,
    /// Optional validator function for cross-attribute validation
    /// (e.g., mutually exclusive required fields)
    pub validator: Option<ResourceValidator>,
    /// Whether this is a managed resource or a data source.
    /// Data sources must be used with the `read` keyword.
    pub kind: SchemaKind,
    /// The attribute that serves as the unique name for this resource type.
    /// Used for automatic unique name generation during create-before-destroy replacement.
    /// (e.g., "bucket_name" for s3.bucket, "log_group_name" for logs.log_group)
    pub name_attribute: Option<String>,
    /// If true, updates are not supported for this resource type.
    /// The differ will always generate Replace instead of Update.
    /// Used for resource types where the provider API rejects updates
    /// despite the schema indicating update support.
    pub force_replace: bool,
    /// Per-resource operational config (timeouts, retries).
    /// When None, provider defaults are used.
    pub operation_config: Option<OperationConfig>,
    /// Declarative "exactly one of" groups. Each inner vec is a group of
    /// attribute names where exactly one must be specified. Unlike `validator`
    /// (a function pointer), this is plain data and survives the WASM plugin
    /// boundary.
    pub exclusive_required: Vec<Vec<String>>,
    /// Default total timeout for `wait <target> { ... }` polling against
    /// this resource type. `None` falls back to
    /// [`WAIT_DEFAULT_TIMEOUT`].
    pub default_wait_timeout: Option<std::time::Duration>,
    /// Default poll cadence between `read()` calls for `wait <target>`
    /// against this resource type. `None` falls back to
    /// [`WAIT_DEFAULT_INTERVAL`].
    pub default_wait_interval: Option<std::time::Duration>,
    /// Named definitions reachable via [`AttrTypeKind::Ref`] from this
    /// resource's attribute types. Empty for resources whose attribute
    /// graph contains no cycles (the common case).
    ///
    /// Introduced in carina#3340 so cyclic CloudFormation definition
    /// graphs (WAFv2 `WebACL.Statement`, AppSync `GraphQLApi`, ...) can
    /// be represented in the type system without flattening to
    /// `Map(String, String)` blobs. Walk-sites that traverse
    /// `AttributeType` (differ, detail_rows, LSP) MUST consult `defs`
    /// to resolve `Ref` variants rather than fall through a wildcard.
    pub defs: std::collections::BTreeMap<String, AttributeType>,
}

/// Fallback total timeout when neither the user nor the resource schema
/// declares one. Conservative — real provider workloads should declare
/// their own (e.g. ACM Certificate at 75min, EC2 Instance Running at
/// 5min). Used by the differ when emitting `Effect::Wait`.
pub const WAIT_DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Fallback poll cadence when the resource schema declares none. The
/// executor pauses for this between `read()` calls. AWS API rate limits
/// drive the lower bound; 5 seconds is the same default Terraform uses
/// for `aws_acm_certificate_validation`.
pub const WAIT_DEFAULT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

impl ResourceSchema {
    pub fn new(resource_type: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            attributes: HashMap::new(),
            description: None,
            validator: None,
            kind: SchemaKind::Resource,
            name_attribute: None,
            force_replace: false,
            operation_config: None,
            exclusive_required: Vec::new(),
            default_wait_timeout: None,
            default_wait_interval: None,
            defs: std::collections::BTreeMap::new(),
        }
    }

    /// Attach a named definition reachable via [`AttrTypeKind::Ref`].
    ///
    /// Used by codegen to register cyclic CFN struct definitions
    /// (WAFv2 `WebACL.Statement`, AppSync `GraphQLApi`, ...). Each
    /// `Ref(name)` inside this resource's attribute types is resolved
    /// against this map.
    pub fn with_def(mut self, name: impl Into<String>, ty: AttributeType) -> Self {
        self.defs.insert(name.into(), ty);
        self
    }

    /// Set the schema-declared default total timeout for `wait` polling.
    pub fn with_default_wait_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.default_wait_timeout = Some(timeout);
        self
    }

    /// Set the schema-declared default poll interval for `wait` polling.
    pub fn with_default_wait_interval(mut self, interval: std::time::Duration) -> Self {
        self.default_wait_interval = Some(interval);
        self
    }

    pub fn attribute(mut self, schema: AttributeSchema) -> Self {
        self.attributes.insert(schema.name.clone(), schema);
        self
    }

    /// Resolve `ty` against this resource schema's definition map.
    pub fn resolve_of<'a>(&'a self, ty: &'a AttributeType) -> ResolvedAttrType<'a> {
        ty.resolve_refs_with_defs(&self.defs)
    }

    /// Project `ty` onto a [`Shape`] using this resource schema's
    /// definition map.
    pub fn shape_of<'a>(&'a self, ty: &'a AttributeType) -> Shape<'a> {
        ty.shape_with_defs(&self.defs)
    }

    pub fn struct_fields_with_budget<'a>(
        &'a self,
        ty: &'a AttributeType,
        budget: &mut ShapeWalkBudget,
    ) -> Option<&'a [StructField]> {
        if !budget.take() {
            return None;
        }
        struct_fields_with_defs(ty, &self.defs)
    }

    pub fn union_members_with_budget<'a>(
        &'a self,
        ty: &'a AttributeType,
        budget: &mut ShapeWalkBudget,
    ) -> Option<&'a [AttributeType]> {
        if !budget.take() {
            return None;
        }
        union_members_with_defs(ty, &self.defs)
    }

    /// Return the valid API values for a top-level Enum attribute
    /// referenced by a namespaced DSL alias.
    ///
    /// This intentionally does not walk into `Struct` fields. Enum alias
    /// resolution already dispatches with the user-visible attribute name:
    /// list items keep the same name and map values use the map key as the
    /// next name. Looking up that top-level attribute and peeling only
    /// transparent wrappers keeps recursive schemas (for example WAFv2
    /// `Statement`) out of the alias hot path.
    pub fn enum_valid_values_for_attr_alias(&self, attr_name: &str, input: &str) -> Vec<String> {
        let Some(attr_schema) = self.attributes.get(attr_name) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        self.append_enum_values_from_top_level_attr_type(&attr_schema.attr_type, input, &mut out);
        out
    }

    fn append_enum_values_from_top_level_attr_type(
        &self,
        attr_type: &AttributeType,
        input: &str,
        out: &mut Vec<String>,
    ) {
        match self.shape_of(attr_type) {
            Shape::Enum {
                identity, values, ..
            } => {
                let identity_matches =
                    enum_identity_matches_input(input, &identity.kind, Some(identity));
                for value in values.into_iter().flatten().filter(|_| identity_matches) {
                    if !out.contains(value) {
                        out.push(value.clone());
                    }
                }
            }
            Shape::List {
                element_type: inner,
                ..
            } => {
                self.append_enum_values_from_top_level_attr_type(inner, input, out);
            }
            Shape::Map { value, .. } => {
                self.append_enum_values_from_top_level_attr_type(value, input, out);
            }
            Shape::Union => {
                if let Some(members) = self.union_members_of(attr_type) {
                    for member in members {
                        self.append_enum_values_from_top_level_attr_type(member, input, out);
                    }
                }
            }
            Shape::String { .. }
            | Shape::Int { .. }
            | Shape::Float { .. }
            | Shape::Bool
            | Shape::Duration
            | Shape::Struct { .. } => {}
        }
    }

    pub(crate) fn union_members_of<'a>(
        &'a self,
        ty: &'a AttributeType,
    ) -> Option<&'a [AttributeType]> {
        union_members_with_defs(ty, &self.defs)
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    pub fn with_validator(mut self, validator: ResourceValidator) -> Self {
        self.validator = Some(validator);
        self
    }

    /// Declare that exactly one of the given attributes must be specified.
    ///
    /// Equivalent to a CloudFormation `oneOf` of required properties. Stored
    /// as data (not a closure) so the constraint survives serialization —
    /// in particular, crossing the WASM plugin boundary.
    ///
    /// Multiple calls append additional groups; each group is evaluated
    /// independently by `validate()`.
    pub fn exclusive_required(mut self, fields: &[&str]) -> Self {
        self.exclusive_required
            .push(fields.iter().map(|s| s.to_string()).collect());
        self
    }

    pub fn as_data_source(mut self) -> Self {
        self.kind = SchemaKind::DataSource;
        self
    }

    pub fn is_data_source(&self) -> bool {
        matches!(self.kind, SchemaKind::DataSource)
    }

    pub fn with_name_attribute(mut self, attr: impl Into<String>) -> Self {
        self.name_attribute = Some(attr.into());
        self
    }

    pub fn force_replace(mut self) -> Self {
        self.force_replace = true;
        self
    }

    pub fn with_operation_config(mut self, config: OperationConfig) -> Self {
        self.operation_config = Some(config);
        self
    }

    /// Returns a map of block_name -> canonical attribute name
    /// for all attributes that have a block_name set.
    pub fn block_name_map(&self) -> HashMap<String, String> {
        self.attributes
            .iter()
            .filter_map(|(attr_name, schema)| {
                schema
                    .block_name
                    .as_ref()
                    .map(|bn| (bn.clone(), attr_name.clone()))
            })
            .collect()
    }

    /// Returns the names of read-only attributes (set by the provider after creation)
    pub fn read_only_attributes(&self) -> Vec<&str> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.read_only)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Returns attributes that have default values and are not read-only.
    /// Each entry is (attribute_name, default_value).
    pub fn default_value_attributes(&self) -> Vec<(&str, &Value)> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.default.is_some() && !schema.read_only)
            .map(|(name, schema)| (name.as_str(), schema.default.as_ref().unwrap()))
            .collect()
    }

    /// Returns default-value attributes not specified by the user, sorted by name.
    /// Each entry is (attribute_name, formatted_default_value).
    pub fn compute_default_attrs(&self, user_keys: &HashSet<&str>) -> Vec<(String, String)> {
        let mut default_attrs: Vec<(&str, &Value)> = self
            .default_value_attributes()
            .into_iter()
            .filter(|(a, _)| !user_keys.contains(a))
            .collect();
        default_attrs.sort_by_key(|(a, _)| *a);
        default_attrs
            .into_iter()
            .map(|(name, val)| (name.to_string(), format_value_with_key(val, Some(name))))
            .collect()
    }

    /// Returns read-only attribute names not specified by the user, sorted.
    pub fn compute_read_only_attrs(&self, user_keys: &HashSet<&str>) -> Vec<String> {
        let mut ro_attrs: Vec<&str> = self
            .read_only_attributes()
            .into_iter()
            .filter(|a| !user_keys.contains(a))
            .collect();
        ro_attrs.sort();
        ro_attrs.into_iter().map(|a| a.to_string()).collect()
    }

    /// Returns the names of create-only (immutable) attributes
    pub fn create_only_attributes(&self) -> Vec<&str> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.create_only)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Returns the names of identity attributes (contribute to anonymous resource hashing)
    pub fn identity_attributes(&self) -> Vec<&str> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.identity)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Returns the names of removable attributes.
    /// By default, optional and mutable attributes are removable.
    pub fn removable_attributes(&self) -> Vec<&str> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.is_removable())
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Construct a [`Schema`] view rooted at `root` and carrying this
    /// resource's `defs` map.
    ///
    /// Use this in every place that needs to validate or canonicalize
    /// against a single attribute of the resource: the resulting
    /// `Schema` is the only API that resolves `AttrTypeKind::Ref`
    /// against this resource's def map, so a future caller that needs
    /// per-attribute validation cannot accidentally drop `defs`
    /// (carina#3345). The `root` argument is typically
    /// `attr_schema.attr_type.clone()`.
    pub fn schema_view_for(&self, root: AttributeType) -> Schema {
        Schema {
            root,
            defs: self.defs.clone(),
        }
    }

    /// Validate resource attributes.
    ///
    /// This variant does not have origin information for string values, so
    /// it cannot distinguish a user-typed `target_type = "aaa"` from a
    /// bare-identifier `target_type = aaa` — both surface as
    /// `InvalidEnumVariant`. Call `validate_with_origins` when the caller
    /// knows which attributes were written as quoted string literals
    /// (see #2094).
    pub fn validate(&self, attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
        self.validate_inner(attributes, &|_attr_name| false, no_lookup())
    }

    /// Validate resource attributes, reshaping enum-variant errors into
    /// `StringLiteralExpectedEnum` for attributes whose value was written
    /// in the source as a quoted string literal.
    ///
    /// `is_string_literal` answers "was this top-level attribute on the
    /// current resource written as `attr = \"...\"`?". A `true` response
    /// upgrades any `InvalidEnumVariant` for that attribute into
    /// `StringLiteralExpectedEnum` so the error message describes the
    /// form mismatch instead of asking the user to match a list of
    /// variants. Non-enum errors are passed through unchanged.
    pub fn validate_with_origins(
        &self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
    ) -> Result<(), Vec<TypeError>> {
        self.validate_inner(attributes, is_string_literal, no_lookup())
    }

    /// As [`validate_with_origins`], but also runs `lookup` on every
    /// refined primitive identity reached during traversal so
    /// provider-supplied validators that the schema itself cannot
    /// carry (WASM plugin path) still get to reject bad values.
    pub fn validate_with_origins_and_lookup(
        &self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
    ) -> Result<(), Vec<TypeError>> {
        self.validate_inner(attributes, is_string_literal, lookup)
    }

    fn validate_inner(
        &self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
    ) -> Result<(), Vec<TypeError>> {
        let mut errors = Vec::new();

        // Check required attributes
        for (name, schema) in &self.attributes {
            if schema.required && !attributes.contains_key(name) && schema.default.is_none() {
                errors.push(TypeError::MissingRequired { name: name.clone() });
            }
        }

        // Build block_name -> canonical_name map for alias resolution
        let bn_map = self.block_name_map();

        // Build suggestion candidates (canonical names + block name aliases)
        let mut known: Vec<&str> = self.attributes.keys().map(|s| s.as_str()).collect();
        for bn in bn_map.keys() {
            known.push(bn.as_str());
        }

        // Build a `Schema` view over this resource's `defs` once so
        // every attribute validation walks the same def map. Routing
        // through `Schema::validate_attr` is what makes cyclic CFN
        // attributes (`AttrTypeKind::Ref`) resolve correctly; the
        // `defs`-less `AttrTypeKind::validate(value)` call this
        // replaced trips the "reached the standalone validator"
        // sentinel for every Ref-containing attribute (carina#3345,
        // post-#3340 awscc `s3.Bucket/*` failures).
        let schema_view = Schema::with_defs(self.defs.clone());

        // Type check each attribute and reject unknown ones
        for (name, value) in attributes {
            // Skip parser-internal attributes (leading `_`, e.g.
            // `_type`, `_default_tag_keys`); they have no schema entry.
            // Prefer a typed field on `Resource` for new internal state
            // — see #2224.
            if name.starts_with('_') {
                continue;
            }

            // Resolve block_name alias to canonical name
            let canonical = bn_map.get(name).map(|s| s.as_str()).unwrap_or(name);

            if let Some(schema) = self.attributes.get(canonical) {
                if let Err(e) = schema_view.validate_attr(&schema.attr_type, value) {
                    // Tag the error with the attribute name the user actually
                    // wrote (which may be a block-name alias), so diagnostics
                    // point back at a token that appears in their source.
                    let tagged = e.with_attribute(name);
                    let reshaped = if is_string_literal(name.as_str()) {
                        reshape_for_string_literal(tagged, &schema.attr_type, value, name)
                    } else {
                        tagged
                    };
                    errors.push(reshaped);
                }
                walk_custom_lookup(
                    &schema.attr_type,
                    value,
                    name,
                    lookup,
                    &self.defs,
                    &mut errors,
                );
            } else {
                let suggestion = suggest_similar_name(name, &known);
                errors.push(TypeError::UnknownAttribute {
                    name: name.clone(),
                    suggestion,
                });
            }
        }

        // Evaluate declarative exclusive-required groups (WASM-safe).
        for group in &self.exclusive_required {
            let refs: Vec<&str> = group.iter().map(|s| s.as_str()).collect();
            if let Err(mut e) = validators::validate_exclusive_required(attributes, &refs) {
                errors.append(&mut e);
            }
        }

        // Run custom validator if present
        if let Some(validator) = self.validator
            && let Err(mut validation_errors) = validator(attributes)
        {
            errors.append(&mut validation_errors);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Collect all attribute_name -> block_name mappings from all schemas.
/// This includes both top-level attributes and nested struct fields.
/// Used by the formatter to convert `= [{...}]` to block syntax.
pub fn collect_all_block_names(registry: &SchemaRegistry) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for (_provider, _resource_type, _kind, schema) in registry.iter() {
        for (attr_name, attr_schema) in &schema.attributes {
            if let Some(bn) = &attr_schema.block_name {
                result.insert(attr_name.clone(), bn.clone());
            }
            // Also collect from nested struct fields
            collect_block_names_from_type(&attr_schema.attr_type, &mut result);
        }
        // Walk cyclic-struct definitions separately so block names on
        // fields inside a `Ref` target (e.g. `Statement.AndStatement`)
        // are picked up. The `Ref` arm in `collect_block_names_from_type`
        // stops recursion to avoid loops; the visit-defs pass closes
        // the gap (carina#3340).
        for def in schema.defs.values() {
            collect_block_names_from_type(def, &mut result);
        }
    }
    result
}

fn collect_block_names_from_type(attr_type: &AttributeType, result: &mut HashMap<String, String>) {
    match &attr_type.kind {
        AttrTypeKind::Struct { fields, .. } => {
            for field in fields {
                if let Some(bn) = &field.block_name {
                    result.insert(field.name.clone(), bn.clone());
                }
                collect_block_names_from_type(&field.field_type, result);
            }
        }
        AttrTypeKind::List {
            element_type: inner,
            ..
        } => {
            collect_block_names_from_type(inner, result);
        }
        AttrTypeKind::Map { value: inner, .. } => {
            collect_block_names_from_type(inner, result);
        }
        AttrTypeKind::Union(types) => {
            for t in types {
                collect_block_names_from_type(t, result);
            }
        }
        // `Ref`: do not follow — the caller visits `schema.defs`
        // entries directly to avoid infinite recursion on cyclic
        // schemas (carina#3340).
        AttrTypeKind::Ref(_) => {}
        AttrTypeKind::String { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. } => {}
    }
}

/// Resolve block name aliases in a map using struct field definitions.
///
/// For each key in `map` that matches a `block_name` on a struct field,
/// renames it to the canonical field name. Also recurses into nested
/// struct values to resolve block names at all nesting levels.
///
/// `defs` is the resource schema's `defs` map; nested fields typed as
/// [`AttrTypeKind::Ref`] are peeled against it via
/// [`AttrTypeKind::resolve_refs`] before recursion so block-name
/// resolution reaches inside cyclic CFN-style schemas (carina#3340 /
/// carina#3349). A `Ref` whose name is not in `defs` is a schema
/// invariant violation and panics through `resolve_refs`.
fn resolve_block_names_in_map(
    map: &mut IndexMap<String, Value>,
    fields: &[StructField],
    defs: &std::collections::BTreeMap<String, AttributeType>,
    resource_id: &str,
    errors: &mut Vec<String>,
) {
    // Build block_name -> canonical field name mapping
    let bn_map: HashMap<String, String> = fields
        .iter()
        .filter_map(|f| f.block_name.as_ref().map(|bn| (bn.clone(), f.name.clone())))
        .collect();

    // Rename block name keys to canonical names, but only when the value
    // is a List (from block syntax). Non-list values (e.g., Value::Concrete(ConcreteValue::Map) from
    // attribute assignment) target the actual field with that name.
    let renames: Vec<(String, String)> = map
        .keys()
        .filter_map(|key| {
            bn_map.get(key).and_then(|canon| {
                // Only rename if the value is a List (block-originated)
                if matches!(map.get(key), Some(Value::Concrete(ConcreteValue::List(_)))) {
                    Some((key.clone(), canon.clone()))
                } else {
                    None
                }
            })
        })
        .collect();

    for (block_key, canon_key) in renames {
        // When block_name == canonical name, no rename is needed
        if block_key == canon_key {
            continue;
        }
        if map.contains_key(&canon_key) {
            errors.push(format!(
                "{}: cannot use both '{}' and '{}' (they refer to the same attribute)",
                resource_id, block_key, canon_key
            ));
            continue;
        }
        let value = map.shift_remove(&block_key).unwrap();
        map.insert(canon_key, value);
    }

    // Recurse into nested struct values
    for field in fields {
        let value = match map.get_mut(&field.name) {
            Some(v) => v,
            None => continue,
        };
        recurse_block_names_into_value(&field.field_type, value, defs, resource_id, errors);
    }
}

/// Drive the block-name recursion through a single `(field_type, value)`
/// pair, peeling any [`AttrTypeKind::Ref`] hops against `defs` before
/// dispatching on `Struct` / `List`. Factored out so the top-level
/// `resolve_block_names` walk and the nested `resolve_block_names_in_map`
/// walk share the same Ref-handling code path — both used to drop `Ref`
/// into a `_ => {}` arm (carina#3349).
///
/// `Ref` peeling delegates to [`AttrTypeKind::resolve_refs`] for both
/// the outer type and a nested `List<Ref>` element, so the panic
/// semantics on dangling refs match every other walk-site (carina#3340).
fn recurse_block_names_into_value(
    field_type: &AttributeType,
    value: &mut Value,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    resource_id: &str,
    errors: &mut Vec<String>,
) {
    // Project onto `Shape` so `Ref` is peeled at the type level
    // (carina#3349). The `Shape` enum has no `Ref` variant, so the
    // wildcard arm below cannot silently swallow a `Ref` — the bug
    // class is structurally impossible.
    match field_type.shape_with_defs(defs) {
        Shape::Struct { .. } => {
            if let Value::Concrete(ConcreteValue::Map(inner_map)) = value {
                let inner = struct_fields_with_defs(field_type, defs)
                    .expect("Shape::Struct must expose struct fields internally");
                resolve_block_names_in_map(inner_map, inner, defs, resource_id, errors);
            }
        }
        Shape::List {
            element_type: inner,
            ..
        } => {
            // `List<Ref>`: peel the element type too via `shape(defs)`
            // so the walk reaches the underlying struct fields.
            if let Shape::Struct { .. } = inner.shape_with_defs(defs)
                && let Value::Concrete(ConcreteValue::List(items)) = value
            {
                let inner_fields = struct_fields_with_defs(inner, defs)
                    .expect("Shape::Struct must expose struct fields internally");
                for item in items.iter_mut() {
                    if let Value::Concrete(ConcreteValue::Map(item_map)) = item {
                        resolve_block_names_in_map(
                            item_map,
                            inner_fields,
                            defs,
                            resource_id,
                            errors,
                        );
                    }
                }
            }
        }
        // Other shapes (primitives, Enum, Custom, Map, Union) do
        // not carry block-name aliases reachable from here. `Ref` is
        // structurally absent from `Shape`.
        _ => {}
    }
}

/// Resolve block name aliases in resources.
///
/// For each resource attribute key that matches a `block_name` in the schema,
/// renames it to the canonical attribute name. Errors if both the block_name
/// (singular) and the canonical attribute name (plural) are present.
///
/// Also recursively resolves block names in nested struct values.
pub fn resolve_block_names(
    resources: &mut [Resource],
    registry: &SchemaRegistry,
) -> Result<(), String> {
    let mut all_errors = Vec::new();

    for resource in resources.iter_mut() {
        let schema = match registry.get_for(resource) {
            Some(s) => s,
            None => continue,
        };

        let bn_map = schema.block_name_map();

        // Collect keys to rename: (block_name_key, canonical_attr_name)
        // Only rename when the value is a List (from block syntax). Non-list values
        // (e.g., Value::Concrete(ConcreteValue::Map) from attribute assignment) target the actual field with that name.
        let renames: Vec<(String, String)> = resource
            .attributes
            .keys()
            .filter_map(|key| {
                bn_map.get(key).and_then(|canon| {
                    if matches!(
                        resource.get_attr(key),
                        Some(Value::Concrete(ConcreteValue::List(_)))
                    ) {
                        Some((key.clone(), canon.clone()))
                    } else {
                        None
                    }
                })
            })
            .collect();

        for (block_key, canon_key) in renames {
            // When block_name == canonical name, no rename is needed
            if block_key == canon_key {
                continue;
            }
            if resource.attributes.contains_key(&canon_key) {
                all_errors.push(format!(
                    "{}: cannot use both '{}' and '{}' (they refer to the same attribute)",
                    resource.id, block_key, canon_key
                ));
                continue;
            }

            // `shift_remove` keeps the rest of the source-authored
            // order intact; `swap_remove` would reorder remaining
            // attributes — see #2222.
            let expr = resource.attributes.shift_remove(&block_key).unwrap();
            resource.attributes.insert(canon_key, expr);
        }

        // Recurse into nested struct values to resolve block names at
        // all levels. `recurse_block_names_into_value` peels
        // `AttrTypeKind::Ref` against `schema.defs` so cyclic CFN
        // schemas (carina#3340) still see their block-name aliases
        // (carina#3349). The top-level walk previously dropped `Ref`
        // into a `_ => {}` arm, which made `awscc.s3.Bucket`'s
        // `lifecycle_configuration` (typed `Ref("LifecycleConfiguration")`)
        // reject the documented `rule { }` block syntax with
        // "Required attribute 'rules' is missing".
        let resource_id = resource.id.to_string();
        for (attr_name, attr_schema) in &schema.attributes {
            let value = match resource.attributes.get_mut(attr_name) {
                Some(v) => v,
                None => continue,
            };
            recurse_block_names_into_value(
                &attr_schema.attr_type,
                value,
                &schema.defs,
                &resource_id,
                &mut all_errors,
            );
        }
    }

    if all_errors.is_empty() {
        Ok(())
    } else {
        Err(all_errors.join("\n"))
    }
}

/// Provider-agnostic types only. AWS-specific types (arn, aws_resource_id,
/// availability_zone, etc.) belong in provider crates.
/// See carina-provider-awscc/src/schemas/generated/mod.rs for AWS types.
pub mod types {
    use super::*;

    /// Positive integer type
    pub fn positive_int() -> AttributeType {
        AttributeType::custom(
            Some(TypeIdentity::bare("PositiveInt")),
            AttributeType::int(),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::Int(n)) = value {
                    if *n > 0 {
                        Ok(())
                    } else {
                        Err("Value must be positive".to_string())
                    }
                } else {
                    Err("Expected integer".to_string())
                }
            }),
            None,
        )
    }

    /// IPv4 CIDR block type (e.g., "10.0.0.0/16")
    pub fn ipv4_cidr() -> AttributeType {
        AttributeType::custom(
            Some(TypeIdentity::bare("Ipv4Cidr")),
            AttributeType::string(),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_ipv4_cidr(s)
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// IPv4 address type (e.g., "10.0.1.5", "192.168.0.1")
    pub fn ipv4_address() -> AttributeType {
        AttributeType::custom(
            Some(TypeIdentity::bare("Ipv4Address")),
            AttributeType::string(),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_ipv4_address(s)
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// IPv6 address type (e.g., "2001:db8::1", "::1")
    pub fn ipv6_address() -> AttributeType {
        AttributeType::custom(
            Some(TypeIdentity::bare("Ipv6Address")),
            AttributeType::string(),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_ipv6_address(s)
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// IPv6 CIDR block type (e.g., "2001:db8::/32", "::/0")
    pub fn ipv6_cidr() -> AttributeType {
        AttributeType::custom(
            Some(TypeIdentity::bare("Ipv6Cidr")),
            AttributeType::string(),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_ipv6_cidr(s)
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// CIDR block type that accepts both IPv4 and IPv6 (e.g., "10.0.0.0/16" or "2001:db8::/32")
    pub fn cidr() -> AttributeType {
        AttributeType::union(vec![ipv4_cidr(), ipv6_cidr()])
    }

    /// Email address type (RFC 5322-ish lightweight validation).
    ///
    /// Validation is intentionally pragmatic, not a full RFC 5322 parser:
    /// requires a non-empty local part, a single `@`, and a domain that
    /// contains at least one dot with non-empty labels.
    pub fn email() -> AttributeType {
        AttributeType::custom(
            Some(TypeIdentity::bare("Email")),
            AttributeType::string(),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_email(s)
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }
}

/// Validate an IPv4 address (e.g., "10.0.1.5", "192.168.0.1")
pub fn validate_ipv4_address(ip: &str) -> Result<(), String> {
    let octets: Vec<&str> = ip.split('.').collect();
    if octets.len() != 4 {
        return Err(format!("Invalid IPv4 address '{}': expected 4 octets", ip));
    }

    for octet in &octets {
        match octet.parse::<u8>() {
            Ok(_) => {}
            Err(_) => {
                return Err(format!(
                    "Invalid octet '{}' in IPv4 address: must be 0-255",
                    octet
                ));
            }
        }
    }

    Ok(())
}

/// Validate IPv4 CIDR block format (e.g., "10.0.0.0/16")
pub fn validate_ipv4_cidr(cidr: &str) -> Result<(), String> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid CIDR format '{}': expected IP/prefix",
            cidr
        ));
    }

    let ip = parts[0];
    let prefix = parts[1];

    // Validate IP address
    validate_ipv4_address(ip)?;

    // Validate prefix length
    match prefix.parse::<u8>() {
        Ok(p) if p <= 32 => Ok(()),
        Ok(p) => Err(format!("Invalid prefix length '{}': must be 0-32", p)),
        Err(_) => Err(format!(
            "Invalid prefix length '{}': must be a number",
            prefix
        )),
    }
}

/// Validate IPv6 CIDR block format (e.g., "2001:db8::/32", "::/0")
pub fn validate_ipv6_cidr(cidr: &str) -> Result<(), String> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid IPv6 CIDR format '{}': expected address/prefix",
            cidr
        ));
    }

    let addr = parts[0];
    let prefix = parts[1];

    // Validate IPv6 address
    validate_ipv6_address(addr)?;

    // Validate prefix length (0-128)
    match prefix.parse::<u8>() {
        Ok(p) if p <= 128 => Ok(()),
        Ok(p) => Err(format!("Invalid IPv6 prefix length '{}': must be 0-128", p)),
        Err(_) => Err(format!(
            "Invalid IPv6 prefix length '{}': must be a number",
            prefix
        )),
    }
}

/// Validate an IPv6 address (supports `::` shorthand)
pub fn validate_ipv6_address(addr: &str) -> Result<(), String> {
    if addr.is_empty() {
        return Err("Empty IPv6 address".to_string());
    }

    // Handle :: shorthand
    if addr.contains("::") {
        let halves: Vec<&str> = addr.splitn(2, "::").collect();
        if halves.len() != 2 {
            return Err(format!("Invalid IPv6 address '{}': malformed '::'", addr));
        }

        // Check for multiple ::
        if halves[1].contains("::") {
            return Err(format!(
                "Invalid IPv6 address '{}': only one '::' allowed",
                addr
            ));
        }

        let left_groups: Vec<&str> = if halves[0].is_empty() {
            vec![]
        } else {
            halves[0].split(':').collect()
        };
        let right_groups: Vec<&str> = if halves[1].is_empty() {
            vec![]
        } else {
            halves[1].split(':').collect()
        };

        let total = left_groups.len() + right_groups.len();
        if total > 7 {
            return Err(format!(
                "Invalid IPv6 address '{}': too many groups with '::'",
                addr
            ));
        }

        for group in left_groups.iter().chain(right_groups.iter()) {
            validate_ipv6_group(group, addr)?;
        }
    } else {
        let groups: Vec<&str> = addr.split(':').collect();
        if groups.len() != 8 {
            return Err(format!(
                "Invalid IPv6 address '{}': expected 8 groups, got {}",
                addr,
                groups.len()
            ));
        }
        for group in &groups {
            validate_ipv6_group(group, addr)?;
        }
    }

    Ok(())
}

/// Validate an email address using a pragmatic, RFC 5322-ish lightweight check.
///
/// Requirements:
/// - Exactly one `@` separator
/// - Non-empty local part (no whitespace)
/// - Non-empty domain containing at least one `.`
/// - Every dot-separated domain label is non-empty (no leading/trailing dot,
///   no consecutive dots) and free of whitespace
///
/// This is intentionally not a full RFC 5322 parser; it catches the common
/// formatting mistakes without rejecting unusual-but-valid addresses.
pub fn validate_email(email: &str) -> Result<(), String> {
    if email.is_empty() {
        return Err("Empty email address".to_string());
    }

    let parts: Vec<&str> = email.split('@').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid email '{}': expected exactly one '@'",
            email
        ));
    }

    let local = parts[0];
    let domain = parts[1];

    if local.is_empty() {
        return Err(format!("Invalid email '{}': empty local part", email));
    }
    if local.chars().any(char::is_whitespace) {
        return Err(format!(
            "Invalid email '{}': local part contains whitespace",
            email
        ));
    }

    if domain.is_empty() {
        return Err(format!("Invalid email '{}': empty domain", email));
    }
    if !domain.contains('.') {
        return Err(format!(
            "Invalid email '{}': domain must contain at least one dot",
            email
        ));
    }

    for label in domain.split('.') {
        if label.is_empty() {
            return Err(format!("Invalid email '{}': domain has empty label", email));
        }
        if label.chars().any(char::is_whitespace) {
            return Err(format!(
                "Invalid email '{}': domain label contains whitespace",
                email
            ));
        }
    }

    Ok(())
}

/// Compute Levenshtein edit distance between two strings.
pub(crate) fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_len = a.chars().count();
    let b_len = b.chars().count();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0; b_len + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_len]
}

/// Suggest the most similar field name, if one is close enough
pub fn suggest_similar_name(unknown: &str, known: &[&str]) -> Option<String> {
    let max_distance = match unknown.len() {
        0..=2 => 1,
        3..=5 => 2,
        _ => 3,
    };

    known
        .iter()
        .map(|name| (*name, levenshtein_distance(unknown, name)))
        .filter(|(_, dist)| *dist <= max_distance)
        .min_by_key(|(_, dist)| *dist)
        .map(|(name, _)| name.to_string())
}

/// Validate a single IPv6 group (1-4 hex digits)
fn validate_ipv6_group(group: &str, addr: &str) -> Result<(), String> {
    if group.is_empty() || group.len() > 4 {
        return Err(format!(
            "Invalid IPv6 group '{}' in address '{}': must be 1-4 hex digits",
            group, addr
        ));
    }
    if !group.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid IPv6 group '{}' in address '{}': must be hex digits",
            group, addr
        ));
    }
    Ok(())
}

// --- SchemaRegistry ---

/// A registry that holds resource schemas keyed by `(provider, resource_type)`
/// and `SchemaKind`. The same `(provider, resource_type)` may have **two
/// independent entries** — a `Managed` one and a `DataSource` one — so that
/// a type like `aws.s3.Bucket` can be used both for new-resource creation
/// and for `read`-keyword lookup of existing infrastructure.
///
/// See `notes/specs/2026-05-02-resource-vs-data-source-design.md` (Decision 1-2).
#[derive(Debug, Clone, Default)]
pub struct SchemaRegistry {
    managed: HashMap<(String, String), ResourceSchema>,
    data_sources: HashMap<(String, String), ResourceSchema>,
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a schema under the given provider. The `kind` field on the
    /// schema decides which sub-map it goes into.
    pub fn insert(&mut self, provider: impl Into<String>, schema: ResourceSchema) {
        let key = (provider.into(), schema.resource_type.clone());
        match schema.kind {
            SchemaKind::Resource => {
                self.managed.insert(key, schema);
            }
            SchemaKind::DataSource => {
                self.data_sources.insert(key, schema);
            }
        }
    }

    /// Look up a schema by explicit `(provider, resource_type, kind)`.
    pub fn get(
        &self,
        provider: &str,
        resource_type: &str,
        kind: SchemaKind,
    ) -> Option<&ResourceSchema> {
        let key = (provider.to_string(), resource_type.to_string());
        match kind {
            SchemaKind::Resource => self.managed.get(&key),
            SchemaKind::DataSource => self.data_sources.get(&key),
        }
    }

    /// Look up the `Managed` schema for a given [`Resource`].
    pub fn get_for(&self, resource: &crate::resource::Resource) -> Option<&ResourceSchema> {
        self.get(
            &resource.id.provider,
            &resource.id.resource_type,
            SchemaKind::Resource,
        )
    }

    /// Look up the `DataSource` schema for a given [`DataSource`].
    pub fn get_for_data_source(
        &self,
        resource: &crate::resource::DataSource,
    ) -> Option<&ResourceSchema> {
        self.get(
            &resource.id.provider,
            &resource.id.resource_type,
            SchemaKind::DataSource,
        )
    }

    pub fn has_managed(&self, provider: &str, resource_type: &str) -> bool {
        self.get(provider, resource_type, SchemaKind::Resource)
            .is_some()
    }

    pub fn has_data_source(&self, provider: &str, resource_type: &str) -> bool {
        self.get(provider, resource_type, SchemaKind::DataSource)
            .is_some()
    }

    /// Iterate every schema in the registry, yielding `(provider,
    /// resource_type, kind, &ResourceSchema)`.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, SchemaKind, &ResourceSchema)> + '_ {
        self.managed
            .iter()
            .map(|((p, t), s)| (p.as_str(), t.as_str(), SchemaKind::Resource, s))
            .chain(
                self.data_sources
                    .iter()
                    .map(|((p, t), s)| (p.as_str(), t.as_str(), SchemaKind::DataSource, s)),
            )
    }

    pub fn len(&self) -> usize {
        self.managed.len() + self.data_sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.managed.is_empty() && self.data_sources.is_empty()
    }
}

#[cfg(test)]
mod tests;
