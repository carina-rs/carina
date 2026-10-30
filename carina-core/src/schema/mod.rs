//! Schema - Define type schemas for resources
//!
//! Providers define schemas for each resource type,
//! enabling type validation at parse time.
//!
//! Input mode is selected once, when a schema component builder is created.
//! Transition methods and modifiers that do not make sense for that mode are
//! intentionally absent:
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::new("value", AttributeType::string());
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::optional("value", AttributeType::string()).required();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::optional("value", AttributeType::string()).read_only();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::required("value", AttributeType::string())
//!     .with_default(Default::default());
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::required("value", AttributeType::string()).removable();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::required("value", AttributeType::string()).non_removable();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::provider_populated("value", AttributeType::string())
//!     .with_default(Default::default());
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::provider_populated("value", AttributeType::string()).write_only();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::provider_populated("value", AttributeType::string()).create_only();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::provider_populated("value", AttributeType::string()).identity();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::provider_populated("value", AttributeType::string()).removable();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::provider_populated("value", AttributeType::string()).non_removable();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::provider_populated("value", AttributeType::string())
//!     .with_block_name("values");
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::provider_populated("value", AttributeType::string())
//!     .with_completions(vec![]);
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::optional("value", AttributeType::string()).deferred_populate();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeSchema, AttributeType};
//! let _ = AttributeSchema::required("value", AttributeType::string()).deferred_populate();
//! ```
//!
//! Struct-field builders expose only nested-field modifiers:
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, StructField};
//! let _ = StructField::new("value", AttributeType::string());
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, StructField};
//! let _ = StructField::optional("value", AttributeType::string()).required();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, StructField};
//! let _ = StructField::optional("value", AttributeType::string()).read_only();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, StructField};
//! let _ = StructField::optional("value", AttributeType::string())
//!     .with_default(Default::default());
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, StructField};
//! let _ = StructField::optional("value", AttributeType::string()).create_only();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, StructField};
//! let _ = StructField::optional("value", AttributeType::string()).identity();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, StructField};
//! let _ = StructField::optional("value", AttributeType::string()).with_completions(vec![]);
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, StructField};
//! let _ = StructField::optional("value", AttributeType::string()).removable();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, StructField};
//! let _ = StructField::optional("value", AttributeType::string()).non_removable();
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, StructField};
//! let _ = StructField::provider_populated("value", AttributeType::string()).write_only();
//! ```
//!
//! Callback-bearing refinements require an identity, and lists have only
//! declarative constraints:
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, legacy_validator};
//! let _ = AttributeType::refined_string_with_validator(
//!     None, None, None, legacy_validator(|_| Ok(())), None,
//! );
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, legacy_validator};
//! let _ = AttributeType::refined_int_with_validator(
//!     None, None, legacy_validator(|_| Ok(())),
//! );
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, legacy_validator};
//! let _ = AttributeType::refined_float_with_validator(
//!     None, None, legacy_validator(|_| Ok(())),
//! );
//! ```
//!
//! ```compile_fail
//! use carina_core::schema::{AttributeType, legacy_validator};
//! let _ = AttributeType::refined_list(
//!     AttributeType::string(), true, None, legacy_validator(|_| Ok(())),
//! );
//! ```
//!
//! Resource-level callbacks are not portable and cannot be attached:
//!
//! ```compile_fail
//! use carina_core::schema::ResourceSchema;
//! let _ = ResourceSchema::new("test.Widget").with_validator(|_| Ok(()));
//! ```

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, OnceLock};

use indexmap::{IndexMap, IndexSet};

use crate::config_loader::UnresolvedConfigSnapshot;
use crate::parser::display_string_literal;
use crate::resource::{
    CanonicalEnumValue, ConcreteValue, ConcreteValueRef, DeferredValue, EnumValueResolver,
    RawEnumIdentifier, Resource, ResourceId, Value, ValueString,
};
use crate::utils::{extract_enum_value_with_values, validate_enum_namespace};
use crate::value::{CanonicalizationPurpose, DesiredValueSource, RenderedValue, render_value};

mod fingerprint;
mod resolved_attr_type;
mod resolved_union_members;
mod type_identity;

pub use carina_provider_protocol::types::DslTransform;
/// Per-resource operational configuration for provider timeouts and retries.
pub use carina_provider_protocol::{OperationConfig, ValidatorType};
pub use resolved_attr_type::ResolvedAttrType;
use resolved_union_members::UnionMemberTypes;
pub use resolved_union_members::{ResolvedUnionMember, UnionMembers};
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

type ResourceValidator = fn(&HashMap<String, Value>) -> Result<(), Vec<TypeError>>;

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

#[derive(Clone)]
pub(crate) enum RefinementBinding {
    MetadataOnly(Option<TypeIdentity>),
    Validator {
        identity: TypeIdentity,
        callback: CustomValidator,
    },
}

impl RefinementBinding {
    pub(crate) fn identity(&self) -> Option<&TypeIdentity> {
        match self {
            Self::MetadataOnly(identity) => identity.as_ref(),
            Self::Validator {
                identity,
                callback: _,
            } => Some(identity),
        }
    }

    pub(crate) fn callback(&self) -> Option<&CustomValidator> {
        match self {
            Self::MetadataOnly(_) => None,
            Self::Validator {
                identity: _,
                callback,
            } => Some(callback),
        }
    }

    fn validator_parts(&self) -> Option<(&TypeIdentity, &CustomValidator)> {
        match self {
            Self::MetadataOnly(_) => None,
            Self::Validator { identity, callback } => Some((identity, callback)),
        }
    }
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

/// External validator resolved from the exact declared refined type.
///
/// Provider-owned call sites pair this declaration with its schema owner before
/// invoking the owning factory. A [`TypeIdentity`] alone is descriptive metadata
/// and is never used as routing authority.
pub type CustomTypeLookup<'a> =
    &'a (dyn Fn(&AttributeType, &Value) -> Result<(), TypeError> + Send + Sync + 'a);

/// A [`CustomTypeLookup`] that approves every value. Pass to
/// `validate_with_origins_and_lookup` from contexts that have no
/// `ProviderContext` (snapshot tests, schema unit tests). The
/// schema-attached `validate` closure still runs for built-in
/// validators registered directly on the type.
#[cfg(test)]
pub(crate) fn no_lookup() -> CustomTypeLookup<'static> {
    &|_declared_type, _value| Ok(())
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
    attr_name: Option<&str>,
    lookup: CustomTypeLookup<'_>,
    defs: &std::collections::BTreeMap<String, AttributeType>,
    errors: &mut Vec<TypeError>,
) {
    let mut visited_refs = Vec::new();
    walk_custom_lookup_on_path(
        attr_type,
        value,
        attr_name,
        lookup,
        defs,
        errors,
        &mut visited_refs,
    );
}

fn walk_custom_lookup_on_path<'a>(
    attr_type: &'a AttributeType,
    value: &Value,
    attr_name: Option<&str>,
    lookup: CustomTypeLookup<'_>,
    defs: &'a std::collections::BTreeMap<String, AttributeType>,
    errors: &mut Vec<TypeError>,
    visited_refs: &mut Vec<&'a str>,
) {
    // `Secret` is a sensitivity wrapper, not an unresolved value. Providers
    // may validate the plaintext, but diagnostics crossing back out of this
    // boundary must discard every user-derived value/message.
    if let Value::Deferred(DeferredValue::Secret(inner)) = value {
        let mut secret_errors = Vec::new();
        walk_custom_lookup_on_path(
            attr_type,
            inner,
            attr_name,
            lookup,
            defs,
            &mut secret_errors,
            visited_refs,
        );
        errors.extend(secret_errors.into_iter().map(TypeError::masked_for_secret));
        return;
    }
    if let Value::Deferred(DeferredValue::Coalesce { value, default }) = value {
        walk_custom_lookup_on_path(
            attr_type,
            value,
            attr_name,
            lookup,
            defs,
            errors,
            visited_refs,
        );
        walk_custom_lookup_on_path(
            attr_type,
            default,
            attr_name,
            lookup,
            defs,
            errors,
            visited_refs,
        );
        return;
    }
    // Skip deferred-resolution values — same convention as
    // `AttrTypeKind::validate`, plus `ResourceRef` / `Interpolation`
    // which only resolve to a concrete string at apply time.
    if matches!(
        value,
        Value::Deferred(DeferredValue::FunctionCall { .. })
            | Value::Deferred(DeferredValue::ResolvedFunctionCall { .. })
            | Value::Deferred(DeferredValue::Access(_))
            | Value::Deferred(DeferredValue::ResourceRef { .. })
            | Value::Deferred(DeferredValue::Interpolation(_))
            | Value::Deferred(DeferredValue::Unknown(_))
    ) {
        return;
    }
    match &attr_type.kind {
        AttrTypeKind::String { binding, .. }
        | AttrTypeKind::Int { binding, .. }
        | AttrTypeKind::Float { binding, .. } => {
            if binding.callback().is_none() {
                run_identified_lookup(
                    attr_type,
                    binding.identity(),
                    value,
                    attr_name,
                    lookup,
                    errors,
                );
            }
        }
        AttrTypeKind::Enum {
            identity, validate, ..
        } => {
            if validate.is_some() {
                return;
            }
            let resolver = EnumValueResolver::with_defs(attr_type, defs);
            let canonical = match value {
                Value::Concrete(ConcreteValue::EnumIdentifier(raw)) => resolver.resolve_raw(raw),
                Value::Concrete(ConcreteValue::String(text)) => {
                    resolver.resolve_state_text(text.expose_plaintext())
                }
                Value::Concrete(ConcreteValue::CanonicalEnum(value))
                    if value.identity().same_type(identity) =>
                {
                    Ok(value.clone())
                }
                _ => return,
            };
            let Ok(canonical) = canonical else {
                // The schema validation pass reports the resolver failure. Do
                // not call a provider validator with an unresolved spelling.
                return;
            };
            let resolved = Value::Concrete(ConcreteValue::CanonicalEnum(canonical));
            if let Err(e) = lookup(attr_type, &resolved) {
                errors.push(match attr_name {
                    Some(attr_name) => e.with_attribute(attr_name),
                    None => e,
                });
            }
        }
        AttrTypeKind::List {
            element_type: inner,
            ..
        } => {
            if let Value::Concrete(ConcreteValue::List(items)) = value {
                for item in items {
                    // Descending to an item consumes one value level, so a
                    // recursive schema may legitimately revisit the same Ref.
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
            for member in members.as_raw_slice() {
                let Ok(resolution) = resolve_refs_on_path(member, defs, visited_refs) else {
                    // A missing or cyclic Ref is not a viable Union member.
                    continue;
                };
                let mut local = Vec::new();
                walk_custom_lookup_on_path(
                    resolution.attr,
                    value,
                    attr_name,
                    lookup,
                    defs,
                    &mut local,
                    visited_refs,
                );
                visited_refs.truncate(resolution.restore_len);
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
        AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. } => {}
        // `Ref`: resolve via the schema's def map and continue the
        // walk. The resolved target (typically a `Struct`) may carry
        // identity-bearing custom types whose validators must run.
        // A missing def name is reported by the effective/rootless validator
        // as `ValidationFailed`; here we just skip the custom-lookup
        // walk so a user-facing dangling-Ref does not abort the
        // process (carina#3345).
        AttrTypeKind::Ref(_) => match resolve_refs_on_path(attr_type, defs, visited_refs) {
            Ok(resolution) => {
                walk_custom_lookup_on_path(
                    resolution.attr,
                    value,
                    attr_name,
                    lookup,
                    defs,
                    errors,
                    visited_refs,
                );
                visited_refs.truncate(resolution.restore_len);
            }
            Err(_) => {
                // Skip — the `validate_attr` pass already emitted
                // the dangling-Ref or cyclic-path diagnostic for this
                // attribute/member.
            }
        },
    }
}

fn run_identified_lookup(
    attribute_type: &AttributeType,
    identity: Option<&TypeIdentity>,
    value: &Value,
    attr_name: Option<&str>,
    lookup: CustomTypeLookup<'_>,
    errors: &mut Vec<TypeError>,
) {
    if identity.is_some()
        && let Err(e) = lookup(attribute_type, value)
    {
        errors.push(match attr_name {
            Some(attr_name) => e.with_attribute(attr_name),
            None => e,
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
            .unwrap_or_else(|| match self.to_dsl {
                Some(DslTransform::HyphenToUnderscore) => dsl.replace('_', "-"),
                Some(DslTransform::ReplaceTable(table)) => table
                    .iter()
                    .find_map(|(api, mapped_dsl)| (mapped_dsl == dsl).then(|| api.clone()))
                    .unwrap_or_else(|| dsl.to_string()),
                Some(
                    DslTransform::Identity
                    | DslTransform::StripSuffix(_)
                    | DslTransform::Unknown(_),
                )
                | None => dsl.to_string(),
            })
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

/// Whether a schema value may or must be supplied by the user.
///
/// These states are mutually exclusive by construction: provider-populated
/// values cannot also be required user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    /// The user may omit this field.
    #[default]
    Optional,
    /// The user must provide this field.
    Required,
    /// The provider supplies this field; user assignments are rejected.
    ProviderPopulated,
}

impl InputMode {
    pub const fn is_required(self) -> bool {
        matches!(self, Self::Required)
    }

    pub const fn is_provider_populated(self) -> bool {
        matches!(self, Self::ProviderPopulated)
    }
}

/// A field within a Struct type.
///
/// Construction and mutation go through builders so conflicting input modes
/// cannot be assembled by assigning fields directly.
///
/// ```compile_fail
/// use carina_core::schema::{AttributeType, StructField};
///
/// let _ = StructField {
///     write_only: true,
///     ..StructField::optional("client_secret", AttributeType::string()).build()
/// };
/// ```
///
/// ```compile_fail
/// use carina_core::schema::{AttributeType, StructField};
///
/// let mut field = StructField::optional("client_secret", AttributeType::string()).build();
/// field.write_only = true;
/// ```
///
/// A nested field's declared mode is not a public read API. Its effective
/// mode is available only from a path-carrying view produced by a validated
/// schema witness:
///
/// ```compile_fail
/// use carina_core::schema::{AttributeType, StructField};
/// let field = StructField::required("value", AttributeType::string()).build();
/// let _ = field.input_mode();
/// ```
///
/// ```compile_fail
/// use carina_core::schema::{AttributeType, StructField};
/// let field = StructField::required("value", AttributeType::string()).build();
/// let _ = field.is_required();
/// ```
///
/// ```compile_fail
/// use carina_core::schema::{AttributeType, StructField};
/// let field = StructField::provider_populated("value", AttributeType::string()).build();
/// let _ = field.is_read_only();
/// ```
///
/// ```compile_fail
/// use carina_core::schema::{AttributeType, StructField};
/// let field = StructField::optional("value", AttributeType::string()).write_only().build();
/// let _ = field.is_write_only();
/// ```
///
/// The read-only view remains exhaustively destructurable for conversion
/// seams:
///
/// ```
/// use carina_core::schema::{AttributeType, StructField, StructFieldParts};
///
/// let field = StructField::optional("client_secret", AttributeType::string()).build();
/// let StructFieldParts {
///     name,
///     field_type,
///     declared_input_mode,
///     description,
///     provider_name,
///     block_name,
///     deferred_populate,
///     write_only,
/// } = field.parts();
/// let _ = (
///     name,
///     field_type,
///     declared_input_mode,
///     description,
///     provider_name,
///     block_name,
///     deferred_populate,
///     write_only,
/// );
/// ```
#[derive(Debug, Clone)]
pub struct StructField {
    /// Field name (snake_case, e.g., "ip_protocol")
    pub(crate) name: String,
    /// Field type
    pub(crate) field_type: AttributeType,
    /// Whether user input for this field is optional, required, or forbidden.
    declared_input_mode: InputMode,
    /// Description of this field
    pub(crate) description: Option<String>,
    /// Provider-side property name (e.g., "IpProtocol")
    pub(crate) provider_name: Option<String>,
    /// Alternative block name for repeated block syntax (e.g., "transition" for "transitions")
    pub(crate) block_name: Option<String>,
    /// Whether the value of this nested field is populated by the
    /// provider asynchronously *after* the Create call returns.
    /// Reached when a chained access traverses a `Struct` (e.g.
    /// `cert.domain_validation_options[0].resource_record_value` —
    /// the inner struct field is the deferred-populate one, not the
    /// outer list attribute). See `AttributeSchema::deferred_populate`
    /// (carina#3034).
    pub(crate) deferred_populate: bool,
    /// Whether this nested field is accepted on write but omitted from reads.
    /// This describes provider observability, not secrecy.
    pub(crate) write_only: bool,
}

/// Exhaustive read-only view of a [`StructField`].
///
/// This deliberately has no `#[non_exhaustive]`: conversion seams should
/// destructure every field so additions become compile errors.
#[derive(Debug, Clone, Copy)]
pub struct StructFieldParts<'a> {
    pub name: &'a str,
    pub field_type: &'a AttributeType,
    /// The mode written on this field declaration, before reference-path
    /// ancestry is applied. Ordinary consumers must use an [`EffectiveField`]
    /// obtained from a validated schema witness instead.
    pub declared_input_mode: InputMode,
    pub description: Option<&'a str>,
    pub provider_name: Option<&'a str>,
    pub block_name: Option<&'a str>,
    pub deferred_populate: bool,
    pub write_only: bool,
}

/// Builder for an optional user-input struct field.
pub struct OptionalStructFieldBuilder {
    field: StructField,
}

/// Builder for a required user-input struct field.
pub struct RequiredStructFieldBuilder {
    field: StructField,
}

/// Builder for a provider-populated struct field.
pub struct ProviderPopulatedStructFieldBuilder {
    field: StructField,
}

impl StructField {
    fn with_input_mode(
        name: impl Into<String>,
        field_type: AttributeType,
        input_mode: InputMode,
    ) -> Self {
        Self {
            name: name.into(),
            field_type,
            declared_input_mode: input_mode,
            description: None,
            provider_name: None,
            block_name: None,
            deferred_populate: false,
            write_only: false,
        }
    }

    pub fn optional(
        name: impl Into<String>,
        field_type: AttributeType,
    ) -> OptionalStructFieldBuilder {
        OptionalStructFieldBuilder {
            field: Self::with_input_mode(name, field_type, InputMode::Optional),
        }
    }

    pub fn required(
        name: impl Into<String>,
        field_type: AttributeType,
    ) -> RequiredStructFieldBuilder {
        RequiredStructFieldBuilder {
            field: Self::with_input_mode(name, field_type, InputMode::Required),
        }
    }

    pub fn provider_populated(
        name: impl Into<String>,
        field_type: AttributeType,
    ) -> ProviderPopulatedStructFieldBuilder {
        ProviderPopulatedStructFieldBuilder {
            field: Self::with_input_mode(name, field_type, InputMode::ProviderPopulated),
        }
    }

    pub fn parts(&self) -> StructFieldParts<'_> {
        StructFieldParts {
            name: &self.name,
            field_type: &self.field_type,
            declared_input_mode: self.declared_input_mode,
            description: self.description.as_deref(),
            provider_name: self.provider_name.as_deref(),
            block_name: self.block_name.as_deref(),
            deferred_populate: self.deferred_populate,
            write_only: self.write_only,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn field_type(&self) -> &AttributeType {
        &self.field_type
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn provider_name(&self) -> Option<&str> {
        self.provider_name.as_deref()
    }
}

/// An internal cursor describing the mode inherited by fields of the current
/// type. Keeping ancestry in a type forces every schema/value walker to choose
/// a concrete reference path before consulting field mode or block aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FieldAncestry {
    provider_populated: bool,
}

impl FieldAncestry {
    pub(crate) const WRITABLE: Self = Self {
        provider_populated: false,
    };
    pub(crate) const PROVIDER_POPULATED: Self = Self {
        provider_populated: true,
    };

    pub(crate) fn from_input_mode(mode: InputMode) -> Self {
        if mode.is_provider_populated() {
            Self::PROVIDER_POPULATED
        } else {
            Self::WRITABLE
        }
    }

    pub(crate) fn effective_input_mode(self, field: &StructField) -> InputMode {
        if self.provider_populated {
            InputMode::ProviderPopulated
        } else {
            field.declared_input_mode
        }
    }

    pub(crate) fn block_name(self, field: &StructField) -> Option<&str> {
        (!self.effective_input_mode(field).is_provider_populated())
            .then_some(field.block_name.as_deref())
            .flatten()
    }

    pub(crate) fn descend(self, field: &StructField) -> Self {
        Self::from_input_mode(self.effective_input_mode(field))
    }
}

macro_rules! impl_user_input_struct_field_builder {
    ($builder:ident) => {
        impl $builder {
            pub fn write_only(mut self) -> Self {
                self.field.write_only = true;
                self
            }

            pub fn with_description(mut self, description: impl Into<String>) -> Self {
                self.field.description = Some(description.into());
                self
            }

            pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
                self.field.provider_name = Some(name.into());
                self
            }

            pub fn with_block_name(mut self, name: impl Into<String>) -> Self {
                self.field.block_name = Some(name.into());
                self
            }

            /// Mark this field as populated asynchronously on output paths.
            /// Final schema validation rejects the field if any reachable path
            /// leaves it as effective user input.
            pub fn deferred_populate(mut self) -> Self {
                self.field.deferred_populate = true;
                self
            }

            pub fn build(self) -> StructField {
                self.field
            }
        }
    };
}

impl_user_input_struct_field_builder!(OptionalStructFieldBuilder);
impl_user_input_struct_field_builder!(RequiredStructFieldBuilder);

impl ProviderPopulatedStructFieldBuilder {
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.field.description = Some(description.into());
        self
    }

    pub fn deferred_populate(mut self) -> Self {
        self.field.deferred_populate = true;
        self
    }

    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.field.provider_name = Some(name.into());
        self
    }

    /// Preserve a declared authoring block name. It is inert below a
    /// provider-populated ancestor and rejected by final validation when this
    /// field itself is the first provider-populated node on a path.
    pub fn with_block_name(mut self, name: impl Into<String>) -> Self {
        self.field.block_name = Some(name.into());
        self
    }

    pub fn build(self) -> StructField {
        self.field
    }
}

/// One typed step from a top-level attribute to a nested write-only field.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum LeafPathSegment {
    StructField(String),
    ListIndex(usize),
    MapKey(String),
}

/// Location of one write-only field inside a top-level attribute value.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct LeafPath(Vec<LeafPathSegment>);

impl LeafPath {
    pub fn from_segments(segments: impl IntoIterator<Item = LeafPathSegment>) -> Self {
        Self(segments.into_iter().collect())
    }

    /// Borrow the typed steps from the containing attribute to this leaf.
    pub fn segments(&self) -> &[LeafPathSegment] {
        &self.0
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

/// An [`AttributeType`] paired with the named definitions that give its
/// [`AttrTypeKind::Ref`] nodes meaning.
///
/// Assignability compares types from two independently-owned resource schemas.
/// Carrying each definition map inside this wrapper prevents the source and
/// sink maps from being exchanged as indistinguishable positional arguments.
#[derive(Clone, Copy)]
pub struct TypeInSchema<'a> {
    attr: &'a AttributeType,
    defs: &'a BTreeMap<String, AttributeType>,
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
        binding: RefinementBinding,
        pattern: Option<String>,
        length: Option<(Option<u64>, Option<u64>)>,
        to_dsl: Option<DslTransform>,
    },
    /// A resource handle used only by the schema-level boundary relation.
    ///
    /// Handles are string-backed until #3803's transport work lands, but they
    /// retain the declared resource path so assignability can reject a handle
    /// flowing into a scalar or a different resource type.
    ResourceHandle {
        provider: String,
        resource_type: String,
    },
    /// A closed set of string literals lifted from a module-boundary type.
    /// Kept distinct from plain String so a ResourceHandle cannot satisfy a
    /// closed literal sink merely because handles are currently string-backed.
    StringLiteralSet { values: Vec<String> },
    /// Integer
    Int {
        binding: RefinementBinding,
        range: Option<(Option<i64>, Option<i64>)>,
    },
    /// Floating-point number
    Float {
        binding: RefinementBinding,
        range: Option<(Option<f64>, Option<f64>)>,
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
    Union(UnionMemberTypes),
    /// Named reference into the enclosing resource or rootless definition map.
    ///
    /// Used to express cyclic struct definitions
    /// (e.g. `Statement -> AndStatement -> List<Statement>` in
    /// `AWS::WAFv2::WebACL`) without recursing at codegen time.
    /// Resolved lazily by [`EffectiveType::validate`] or
    /// [`RootlessType::validate`] at the point of use.
    ///
    /// Callers validating a resource path must use an [`EffectiveType`];
    /// genuinely rootless callers obtain this view from a pathless owner such
    /// as [`ValidatedConfigSchema::rootless_type`].
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
        validate: Option<&'a CustomValidator>,
        to_dsl: Option<&'a DslTransform>,
    },
    /// Integer — see [`AttrTypeKind::Int { .. }`].
    Int {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<i64>, Option<i64>)>,
        validate: Option<&'a CustomValidator>,
    },
    /// Floating-point — see [`AttrTypeKind::Float { .. }`].
    Float {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<f64>, Option<f64>)>,
        validate: Option<&'a CustomValidator>,
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

    pub(crate) fn take(&mut self) -> bool {
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
/// the enclosing definition map and is structurally guaranteed not to carry
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
        validate: Option<&'a CustomValidator>,
        to_dsl: Option<&'a DslTransform>,
    },
    /// Integer — see [`AttrTypeKind::Int { .. }`].
    Int {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<i64>, Option<i64>)>,
        validate: Option<&'a CustomValidator>,
    },
    /// Floating-point — see [`AttrTypeKind::Float { .. }`].
    Float {
        identity: Option<&'a TypeIdentity>,
        range: Option<(Option<f64>, Option<f64>)>,
        validate: Option<&'a CustomValidator>,
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
    /// Named reference into the enclosing schema's definition map.
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

/// Internal rootless-schema walker. Path-derived resource types enter value
/// walkers through [`EffectiveType`] instead.
#[derive(Debug, Clone)]
pub(crate) struct Schema {
    /// The root attribute type. May be a `Ref` into `defs`, or any
    /// other shape (`Struct`, `List`, primitive, ...).
    pub(crate) root: AttributeType,
    /// Named definitions reachable via [`AttrTypeKind::Ref`].
    pub(crate) defs: std::collections::BTreeMap<String, AttributeType>,
}

/// A genuinely rootless type owned by a validated pathless schema source.
///
/// This is the public entry point for values that do not originate at a
/// resource-schema path, such as backend configuration and standalone type
/// diagnostics. Resource attributes and nested fields must instead be
/// validated through [`EffectiveAttribute`] or [`EffectiveType`], which carry
/// provider-populated ancestry.
///
/// A path-derived cursor cannot be passed back through this API:
///
/// ```compile_fail
/// use carina_core::schema::{
///     AttributeSchema, AttributeType, ResourceSchema, RootlessType,
/// };
///
/// let schema = ResourceSchema::new("test.Widget")
///     .attribute(
///         AttributeSchema::provider_populated("output", AttributeType::string()).build(),
///     )
///     .validate()
///     .unwrap();
/// let path_type = schema.effective_attribute("output").unwrap().value_type();
/// let raw_type = path_type.as_type().clone();
/// let _ = RootlessType::new(raw_type);
/// ```
///
/// Supplying the resource definitions explicitly cannot reopen the same hole:
///
/// ```compile_fail
/// use carina_core::schema::{
///     AttributeSchema, AttributeType, ResourceSchema, RootlessType,
/// };
///
/// let schema = ResourceSchema::new("test.Widget")
///     .attribute(
///         AttributeSchema::provider_populated("output", AttributeType::string()).build(),
///     )
///     .validate()
///     .unwrap();
/// let path_type = schema.effective_attribute("output").unwrap().value_type();
/// let raw_type = path_type.as_type().clone();
/// let defs = schema.as_schema().defs().clone();
/// let _ = RootlessType::with_defs(raw_type, defs);
/// ```
#[derive(Debug, Clone)]
pub struct RootlessType {
    schema: Schema,
}

impl RootlessType {
    #[cfg(test)]
    pub(crate) fn new(root: AttributeType) -> Self {
        Self::with_defs(root, BTreeMap::new())
    }

    #[cfg(test)]
    pub(crate) fn with_defs(root: AttributeType, defs: BTreeMap<String, AttributeType>) -> Self {
        Self {
            schema: Schema { root, defs },
        }
    }

    fn from_validated_config_type(root: AttributeType) -> Self {
        Self {
            schema: Schema {
                root,
                defs: BTreeMap::new(),
            },
        }
    }

    pub fn validate(&self, value: &Value) -> Result<(), TypeError> {
        self.schema.validate(value)
    }

    /// Validate a rootless value and bridge any callback-free identified
    /// refinements through an owning provider lookup. Native callbacks run in
    /// the structural pass and are skipped by the lookup walk.
    pub(crate) fn validate_with_lookup(
        &self,
        value: &Value,
        lookup: CustomTypeLookup<'_>,
    ) -> Result<(), Vec<TypeError>> {
        if let Err(error) = self.schema.validate(value) {
            return Err(vec![error]);
        }
        let mut errors = Vec::new();
        walk_custom_lookup(
            &self.schema.root,
            value,
            None,
            lookup,
            &self.schema.defs,
            &mut errors,
        );
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors
                .into_iter()
                .map(|error| match error {
                    TypeError::ResourceValidationFailed {
                        message,
                        attribute: _attribute,
                    } => TypeError::ValidationFailed { message },
                    error => error,
                })
                .collect())
        }
    }

    pub fn validate_collect(&self, value: &Value) -> Vec<(FieldPath, TypeError)> {
        self.schema.validate_collect(value)
    }

    /// Canonicalize provider or persisted state for this rootless type.
    pub fn canonicalize_state(&self, value: Value) -> Value {
        self.schema.canonicalize_state(value)
    }

    /// Canonicalize one provider-configuration value without accepting a
    /// resource-path cursor that carries different ancestry.
    pub fn canonicalize_provider_config(
        &self,
        provider_name: &str,
        value: Value,
    ) -> Result<Value, TypeError> {
        crate::value::canonicalize_provider_config_with_type_and_ancestry(
            provider_name,
            value,
            &self.schema.root,
            &self.schema.defs,
            FieldAncestry::WRITABLE,
        )
    }
}

/// Maximum number of non-consuming `Ref` hops retained on one traversal path.
const MAX_REF_PATH_HOPS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefResolutionFailureKind {
    Missing,
    Cycle,
    HopLimit,
}

#[derive(Debug, Clone, Copy)]
struct RefResolutionFailure<'a> {
    kind: RefResolutionFailureKind,
    name: &'a str,
}

impl RefResolutionFailure<'_> {
    fn into_validation_error(self) -> TypeError {
        let message = match self.kind {
            RefResolutionFailureKind::Missing => format!(
                "schema reference `{}` is not defined in the enclosing schema",
                self.name
            ),
            RefResolutionFailureKind::Cycle => {
                format!("cyclic schema reference `{}`", self.name)
            }
            RefResolutionFailureKind::HopLimit => format!(
                "schema reference chain exceeded {MAX_REF_PATH_HOPS} hops at `{}`",
                self.name
            ),
        };
        TypeError::ValidationFailed { message }
    }
}

struct RefPathResolution<'a> {
    attr: &'a AttributeType,
    restore_len: usize,
}

/// Resolve a top-level `Ref` chain while retaining the names reached on the
/// current non-consuming path.
///
/// All Ref walkers share this primitive. Callers deliberately map its three
/// failures differently: schema validation returns a typed error, invariant
/// walkers panic, and Union-member judgement treats them as no-match.
fn resolve_refs_on_path<'a>(
    attr: &'a AttributeType,
    defs: &'a BTreeMap<String, AttributeType>,
    visited_refs: &mut Vec<&'a str>,
) -> Result<RefPathResolution<'a>, RefResolutionFailure<'a>> {
    let restore_len = visited_refs.len();
    let mut current = attr;
    let mut chain_hops = 0;

    loop {
        let AttrTypeKind::Ref(name) = &current.kind else {
            return Ok(RefPathResolution {
                attr: current,
                restore_len,
            });
        };
        let name = name.as_str();
        let failure_kind = if visited_refs.contains(&name) {
            Some(RefResolutionFailureKind::Cycle)
        } else if chain_hops >= MAX_REF_PATH_HOPS {
            Some(RefResolutionFailureKind::HopLimit)
        } else {
            None
        };
        if let Some(kind) = failure_kind {
            visited_refs.truncate(restore_len);
            return Err(RefResolutionFailure { kind, name });
        }
        let Some(next) = defs.get(name) else {
            visited_refs.truncate(restore_len);
            return Err(RefResolutionFailure {
                kind: RefResolutionFailureKind::Missing,
                name,
            });
        };
        visited_refs.push(name);
        chain_hops += 1;
        current = next;
    }
}

fn panic_for_ref_resolution_failure(failure: RefResolutionFailure<'_>) -> ! {
    match failure.kind {
        RefResolutionFailureKind::Missing => panic!(
            "AttributeType::Ref(\"{}\") not found in schema defs; \
             schema invariant violated (producer must populate \
             ResourceSchema.defs for every Ref it emits)",
            failure.name
        ),
        RefResolutionFailureKind::Cycle => panic!(
            "cyclic AttributeType::Ref(\"{}\") in schema defs; \
             schema invariant violated",
            failure.name
        ),
        RefResolutionFailureKind::HopLimit => panic!(
            "AttributeType::Ref chain exceeded {MAX_REF_PATH_HOPS} hops; \
             pathological self-cycle in defs"
        ),
    }
}

impl Schema {
    /// Construct a `Schema` from `root` with an empty `defs` map. A
    /// `Ref` reached during walking surfaces as a clean
    /// `ValidationFailed` (`schema reference '<name>' is not defined
    /// in the enclosing schema`); callers expecting `Ref` to resolve
    /// must populate `defs`. Typical uses: unit-test fixtures, leaf
    /// validation, provider-config schemas that are known flat.
    #[cfg(test)]
    pub(crate) fn flat(root: AttributeType) -> Self {
        Self {
            root,
            defs: std::collections::BTreeMap::new(),
        }
    }

    /// Look up a named definition. Returns `None` if `name` is not in
    /// `defs`; callers should treat that as a type error.
    #[cfg(test)]
    pub(crate) fn resolve(&self, name: &str) -> Option<&AttributeType> {
        self.defs.get(name)
    }

    /// Resolve `ty` against this schema's definition map.
    #[cfg(test)]
    pub(crate) fn resolve_of<'a>(&'a self, ty: &'a AttributeType) -> ResolvedAttrType<'a> {
        ty.resolve_refs_with_defs(&self.defs)
    }

    /// Project `ty` onto a [`Shape`] using this schema's definition map.
    #[cfg(test)]
    pub(crate) fn shape_of<'a>(&'a self, ty: &'a AttributeType) -> Shape<'a> {
        ty.shape_with_defs(&self.defs)
    }

    /// Validate a `Value` against this schema's `root`, resolving
    /// any `Ref` it encounters by looking it up in `defs`.
    pub(crate) fn validate(&self, value: &Value) -> Result<(), TypeError> {
        self.validate_attr(&self.root, value)
    }

    /// Canonicalize provider or persisted state against this schema's root.
    pub(crate) fn canonicalize_state(&self, value: Value) -> Value {
        crate::value::canonicalize_state_with_type_and_ancestry(
            value,
            &self.root,
            &self.defs,
            FieldAncestry::WRITABLE,
        )
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
    pub(crate) fn validate_collect(&self, value: &Value) -> Vec<(FieldPath, TypeError)> {
        let mut out = Vec::new();
        Self::collect_attr_into(
            &self.root,
            &self.defs,
            &FieldPath::new(),
            value,
            FieldAncestry::WRITABLE,
            &mut out,
        );
        out
    }

    fn collect_attr_into(
        attr: &AttributeType,
        defs: &BTreeMap<String, AttributeType>,
        path: &FieldPath,
        value: &Value,
        ancestry: FieldAncestry,
        out: &mut Vec<(FieldPath, TypeError)>,
    ) {
        if fit_value_to_type_on_path(
            attr,
            value,
            defs,
            ancestry,
            CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
            &mut Vec::new(),
        )
        .is_some()
        {
            return;
        }

        let error_count = out.len();
        let mut visited_refs = Vec::new();
        Self::collect_attr_into_on_path(attr, defs, path, value, ancestry, out, &mut visited_refs);
        if out.len() == error_count {
            let error = validate_attr_with_defs_and_ancestry(attr, value, defs, ancestry)
                .expect_err("a rejected fit must produce a validation diagnostic");
            out.push((path.clone(), error));
        }
    }

    fn collect_attr_into_on_path<'a>(
        attr: &'a AttributeType,
        defs: &'a BTreeMap<String, AttributeType>,
        path: &FieldPath,
        value: &Value,
        ancestry: FieldAncestry,
        out: &mut Vec<(FieldPath, TypeError)>,
        visited_refs: &mut Vec<&'a str>,
    ) {
        if let Value::Deferred(DeferredValue::Secret(inner)) = value {
            let mut secret_errors = Vec::new();
            Self::collect_attr_into_on_path(
                attr,
                defs,
                path,
                inner,
                ancestry,
                &mut secret_errors,
                visited_refs,
            );
            out.extend(
                secret_errors
                    .into_iter()
                    .map(|(path, error)| (path, error.masked_for_secret())),
            );
            return;
        }
        // Peel Ref so the downstream arms never have to think about it.
        if matches!(&attr.kind, AttrTypeKind::Ref(_)) {
            match resolve_refs_on_path(attr, defs, visited_refs) {
                Ok(resolution) => {
                    Self::collect_attr_into_on_path(
                        resolution.attr,
                        defs,
                        path,
                        value,
                        ancestry,
                        out,
                        visited_refs,
                    );
                    visited_refs.truncate(resolution.restore_len);
                }
                Err(failure) => {
                    out.push((path.clone(), failure.into_validation_error()));
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

                let canonical_field_names: Vec<&str> =
                    fields.iter().map(|f| f.name.as_str()).collect();

                // Required-field check.
                for f in fields {
                    if ancestry.effective_input_mode(f).is_required()
                        && !struct_field_is_present(fields, map, f, ancestry)
                    {
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
                    match struct_field_for_key(fields, k, ancestry) {
                        Some(field) => {
                            let next_path = path.push_field(k.clone());
                            if let Some(error) =
                                validate_struct_field_assignment(field, k, ancestry)
                            {
                                out.push((next_path, error));
                                continue;
                            }
                            Self::collect_attr_into(
                                &field.field_type,
                                defs,
                                &next_path,
                                v,
                                ancestry.descend(field),
                                out,
                            );
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
                length,
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
                if let Err(error) = validate_list_length(*length, items.len()) {
                    out.push((path.clone(), error));
                    return;
                }
                for (i, item) in items.iter().enumerate() {
                    let next_path = path.push_index(i);
                    Self::collect_attr_into(inner, defs, &next_path, item, ancestry, out);
                }
            }
            // Keep Union membership and best-scoring failure selection
            // single-sourced in `validate_attr`. This returns one error
            // anchored at the current path; choosing and recursively
            // collecting a failed Struct member would improve LSP path
            // precision but would not change validation acceptance.
            AttrTypeKind::Union(_) => {
                if let Err(e) =
                    validate_attr_with_defs_on_path(attr, value, defs, ancestry, visited_refs)
                {
                    out.push((path.clone(), e));
                }
            }
            _ => {
                // For non-recursive shapes the existing single-shot
                // dispatcher is correct. Forward any error under the
                // current path.
                if let Err(e) = validate_attr_with_defs_and_ancestry(attr, value, defs, ancestry) {
                    out.push((path.clone(), e));
                }
            }
        }
    }

    /// Validate one recursive child against this rootless schema's definitions.
    pub(crate) fn validate_attr(
        &self,
        attr: &AttributeType,
        value: &Value,
    ) -> Result<(), TypeError> {
        validate_attr_with_defs(attr, value, &self.defs)
    }
}

fn validate_attr_with_defs(
    attr: &AttributeType,
    value: &Value,
    defs: &BTreeMap<String, AttributeType>,
) -> Result<(), TypeError> {
    validate_attr_with_defs_and_ancestry(attr, value, defs, FieldAncestry::WRITABLE)
}

pub(crate) fn validate_attr_with_defs_and_ancestry(
    attr: &AttributeType,
    value: &Value,
    defs: &BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
) -> Result<(), TypeError> {
    if fit_value_to_type_on_path(
        attr,
        value,
        defs,
        ancestry,
        CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
        &mut Vec::new(),
    )
    .is_some()
    {
        return Ok(());
    }

    let mut visited_refs = Vec::new();
    validation_error_with_defs_on_path(attr, value, defs, ancestry, &mut visited_refs).and_then(
        |()| {
            Err(TypeError::TypeMismatch {
                expected: attr.type_name(),
                got: value.type_name(),
            })
        },
    )
}

fn validate_attr_with_defs_on_path<'a>(
    attr: &'a AttributeType,
    value: &Value,
    defs: &'a BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    visited_refs: &mut Vec<&'a str>,
) -> Result<(), TypeError> {
    if fit_value_to_type_on_path(
        attr,
        value,
        defs,
        ancestry,
        CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
        visited_refs,
    )
    .is_some()
    {
        return Ok(());
    }

    validation_error_with_defs_on_path(attr, value, defs, ancestry, visited_refs).and_then(|()| {
        Err(TypeError::TypeMismatch {
            expected: attr.type_name(),
            got: value.type_name(),
        })
    })
}

/// Produce the existing path-rich diagnostic after the authoritative fit
/// computation has rejected a value. This walker is deliberately off the
/// success path: acceptance is exactly `fit.is_some()`, while the established
/// shape-based Union error attribution remains unchanged.
fn validation_error_with_defs_on_path<'a>(
    attr: &'a AttributeType,
    value: &Value,
    defs: &'a BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    visited_refs: &mut Vec<&'a str>,
) -> Result<(), TypeError> {
    if let Value::Deferred(DeferredValue::Secret(inner)) = value {
        return validation_error_with_defs_on_path(attr, inner, defs, ancestry, visited_refs)
            .map_err(TypeError::masked_for_secret);
    }
    if let Value::Deferred(DeferredValue::Coalesce { value, default }) = value {
        validate_attr_with_defs_and_ancestry(attr, value, defs, ancestry)?;
        return validate_attr_with_defs_and_ancestry(attr, default, defs, ancestry);
    }
    match &attr.kind {
        AttrTypeKind::Ref(_) => {
            let resolution = resolve_refs_on_path(attr, defs, visited_refs)
                .map_err(RefResolutionFailure::into_validation_error)?;
            let result = validation_error_with_defs_on_path(
                resolution.attr,
                value,
                defs,
                ancestry,
                visited_refs,
            );
            visited_refs.truncate(resolution.restore_len);
            result
        }
        AttrTypeKind::List {
            element_type: inner,
            length,
            ordered: _,
        } => {
            if let Some(ConcreteValueRef::List(items)) = value.as_concrete() {
                validate_list_length(*length, items.len())?;
                for (i, item) in items.iter().enumerate() {
                    if let Err(inner_err) =
                        validate_attr_with_defs_and_ancestry(inner, item, defs, ancestry)
                    {
                        return Err(TypeError::ListItemError {
                            index: i,
                            inner: Box::new(inner_err),
                        });
                    }
                }
                Ok(())
            } else if let Some(ConcreteValueRef::StringList(items)) = value.as_concrete() {
                validate_list_length(*length, items.len())?;
                for (i, item) in items.iter().enumerate() {
                    let item = Value::Concrete(ConcreteValue::String(item.clone()));
                    if let Err(inner_err) =
                        validate_attr_with_defs_and_ancestry(inner, &item, defs, ancestry)
                    {
                        return Err(TypeError::ListItemError {
                            index: i,
                            inner: Box::new(inner_err),
                        });
                    }
                }
                Ok(())
            } else if value.as_concrete().is_none() {
                Ok(())
            } else {
                Err(TypeError::TypeMismatch {
                    expected: attr.type_name(),
                    got: value.type_name(),
                })
            }
        }
        AttrTypeKind::Map { key, value: val_ty } => {
            if let Some(ConcreteValueRef::Map(map)) = value.as_concrete() {
                for (k, v) in map.iter() {
                    let key_val = lift_map_key(key, k, defs);
                    if let Err(inner_err) =
                        validate_attr_with_defs_and_ancestry(key, &key_val, defs, ancestry)
                    {
                        return Err(TypeError::MapKeyError {
                            key: k.clone(),
                            inner: Box::new(inner_err),
                        });
                    }
                    if let Err(inner_err) =
                        validate_attr_with_defs_and_ancestry(val_ty, v, defs, ancestry)
                    {
                        return Err(TypeError::MapValueError {
                            key: k.clone(),
                            inner: Box::new(inner_err),
                        });
                    }
                }
                Ok(())
            } else if value.as_concrete().is_none() {
                Ok(())
            } else {
                Err(TypeError::TypeMismatch {
                    expected: attr.type_name(),
                    got: value.type_name(),
                })
            }
        }
        AttrTypeKind::Struct { name, fields } => {
            if let Some(ConcreteValueRef::Map(map)) = value.as_concrete() {
                validate_struct_fields(
                    name,
                    fields,
                    map,
                    ancestry,
                    |field| TypeError::MissingRequired {
                        name: field.name.clone(),
                    },
                    |input_name, field, field_value| {
                        validate_attr_with_defs_and_ancestry(
                            &field.field_type,
                            field_value,
                            defs,
                            ancestry.descend(field),
                        )
                        .map_err(|inner_err| TypeError::StructFieldError {
                            field: input_name.to_string(),
                            inner: Box::new(inner_err),
                        })
                    },
                )
            } else if matches!(value.as_concrete(), Some(ConcreteValueRef::List(_))) {
                Err(TypeError::BlockSyntaxNotAllowed {
                    attribute: name.clone(),
                })
            } else if value.as_concrete().is_none() {
                Ok(())
            } else {
                Err(TypeError::TypeMismatch {
                    expected: attr.type_name(),
                    got: value.type_name(),
                })
            }
        }
        AttrTypeKind::Union(members) => {
            let Some(concrete) = value.as_concrete() else {
                return Ok(());
            };
            select_union_member_on_path(
                members.as_raw_slice(),
                value,
                concrete,
                attr.type_name(),
                defs,
                ancestry,
                visited_refs,
            )
            .map(|_| ())
        }
        _ => attr.validate_leaf_value(value),
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
                binding,
                pattern,
                length,
                to_dsl,
            } => f
                .debug_struct("String")
                .field("identity", &binding.identity())
                .field("pattern", pattern)
                .field("length", length)
                .field("to_dsl", to_dsl)
                .field("validate", &binding.callback().map(|_| "<closure>"))
                .finish(),
            AttrTypeKind::ResourceHandle {
                provider,
                resource_type,
            } => f
                .debug_struct("ResourceHandle")
                .field("provider", provider)
                .field("resource_type", resource_type)
                .finish(),
            AttrTypeKind::StringLiteralSet { values } => {
                f.debug_tuple("StringLiteralSet").field(values).finish()
            }
            AttrTypeKind::Int { binding, range } => f
                .debug_struct("Int")
                .field("identity", &binding.identity())
                .field("range", range)
                .field("validate", &binding.callback().map(|_| "<closure>"))
                .finish(),
            AttrTypeKind::Float { binding, range } => f
                .debug_struct("Float")
                .field("identity", &binding.identity())
                .field("range", range)
                .field("validate", &binding.callback().map(|_| "<closure>"))
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
            } => f
                .debug_struct("List")
                .field("element_type", element_type)
                .field("ordered", ordered)
                .field("length", length)
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
/// (carina#2996). Top-level key-type Refs are peeled through `defs`;
/// an unresolved Ref stays on the ordinary validation-error path rather
/// than panicking here. Shared by the ancestry-aware fit and diagnostic
/// walkers so map-key validation cannot drift (carina#3347).
pub(crate) fn lift_map_key(
    key_type: &AttributeType,
    key: &str,
    defs: &BTreeMap<String, AttributeType>,
) -> Value {
    let mut visited_refs = Vec::new();
    let is_enum = resolve_refs_on_path(key_type, defs, &mut visited_refs)
        .is_ok_and(|resolution| matches!(&resolution.attr.kind, AttrTypeKind::Enum { .. }));
    if is_enum {
        EnumValueResolver::with_defs(key_type, defs)
            .resolve_state_text(key)
            .map(|c| Value::Concrete(ConcreteValue::CanonicalEnum(c)))
            .unwrap_or_else(|_| Value::Concrete(ConcreteValue::enum_identifier(key.to_string())))
    } else {
        Value::Concrete(ConcreteValue::String(key.to_string().into()))
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
    /// Pair this type with the named definitions from its owning schema.
    ///
    /// Use the resulting [`TypeInSchema`] for cross-schema assignability so
    /// source and sink references are resolved against their respective maps.
    /// Prefer [`ResourceSchema::type_in_schema`] when the owning resource
    /// schema is available; this lower-level constructor is for separately
    /// derived contexts that carry a defs map explicitly. For schema-less,
    /// structurally Ref-free types, use [`TypeInSchema::schemaless`].
    pub fn in_schema<'a>(&'a self, defs: &'a BTreeMap<String, AttributeType>) -> TypeInSchema<'a> {
        TypeInSchema { attr: self, defs }
    }

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
        let mut visited_refs = Vec::new();
        self.resolve_refs_with_defs_on_path(defs, &mut visited_refs)
    }

    /// Resolve a top-level Ref chain under invariant-panic semantics while
    /// retaining its names for a subsequent non-consuming Union selection.
    pub(crate) fn resolve_refs_with_defs_on_path<'a>(
        &'a self,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
        visited_refs: &mut Vec<&'a str>,
    ) -> ResolvedAttrType<'a> {
        match resolve_refs_on_path(self, defs, visited_refs) {
            Ok(resolution) => ResolvedAttrType::new_after_peel(resolution.attr),
            Err(failure) => panic_for_ref_resolution_failure(failure),
        }
    }

    /// Peel a Ref chain for a non-consuming Union walk. Revisiting a Ref on the
    /// current path returns `None` so the walker can stop at the cyclic branch;
    /// missing definitions and overlong chains retain invariant-panic semantics.
    pub(crate) fn resolve_refs_for_union_walk_on_path<'a>(
        &'a self,
        defs: &'a std::collections::BTreeMap<String, AttributeType>,
        visited_refs: &mut Vec<&'a str>,
    ) -> Option<ResolvedAttrType<'a>> {
        match resolve_refs_on_path(self, defs, visited_refs) {
            Ok(resolution) => Some(ResolvedAttrType::new_after_peel(resolution.attr)),
            Err(RefResolutionFailure {
                kind: RefResolutionFailureKind::Cycle,
                ..
            }) => None,
            Err(failure) => panic_for_ref_resolution_failure(failure),
        }
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
    ) -> Result<Option<UnionMembers<'_>>, RefEncountered> {
        if !budget.take() {
            return Ok(None);
        }
        if let AttrTypeKind::Ref(name) = &self.kind {
            return Err(RefEncountered { name: name.clone() });
        }
        let AttrTypeKind::Union(members) = &self.kind else {
            return Ok(None);
        };
        Ok(Some(UnionMembers::new_after_union(
            members.as_raw_slice(),
            empty_defs_for_schema_walks(),
        )))
    }

    fn shape_from_resolved(resolved: &AttributeType) -> Shape<'_> {
        match &resolved.kind {
            AttrTypeKind::String {
                binding,
                pattern,
                length,
                to_dsl,
            } => Shape::String {
                identity: binding.identity(),
                pattern: pattern.as_deref(),
                length: *length,
                validate: binding.callback(),
                to_dsl: to_dsl.as_ref(),
            },
            AttrTypeKind::ResourceHandle { .. } | AttrTypeKind::StringLiteralSet { .. } => {
                Shape::String {
                    identity: None,
                    pattern: None,
                    length: None,
                    validate: None,
                    to_dsl: None,
                }
            }
            AttrTypeKind::Int { binding, range } => Shape::Int {
                identity: binding.identity(),
                range: *range,
                validate: binding.callback(),
            },
            AttrTypeKind::Float { binding, range } => Shape::Float {
                identity: binding.identity(),
                range: *range,
                validate: binding.callback(),
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
            } => Shape::List {
                element_type: element_type.as_ref(),
                ordered: *ordered,
                length: *length,
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
                binding,
                pattern,
                length,
                to_dsl,
            } => RawShape::String {
                identity: binding.identity(),
                pattern: pattern.as_deref(),
                length: *length,
                validate: binding.callback(),
                to_dsl: to_dsl.as_ref(),
            },
            AttrTypeKind::ResourceHandle { .. } | AttrTypeKind::StringLiteralSet { .. } => {
                RawShape::String {
                    identity: None,
                    pattern: None,
                    length: None,
                    validate: None,
                    to_dsl: None,
                }
            }
            AttrTypeKind::Int { binding, range } => RawShape::Int {
                identity: binding.identity(),
                range: *range,
                validate: binding.callback(),
            },
            AttrTypeKind::Float { binding, range } => RawShape::Float {
                identity: binding.identity(),
                range: *range,
                validate: binding.callback(),
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
            } => RawShape::List {
                element_type: element_type.as_ref(),
                ordered: *ordered,
                length: *length,
            },
            AttrTypeKind::Map { key, value } => RawShape::Map {
                key: key.as_ref(),
                value: value.as_ref(),
            },
            AttrTypeKind::Struct { name, fields } => RawShape::Struct {
                name: name.as_str(),
                fields: fields.as_slice(),
            },
            AttrTypeKind::Union(members) => RawShape::Union(members.as_raw_slice()),
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
                binding: RefinementBinding::MetadataOnly(None),
                pattern: None,
                length: None,
                to_dsl: None,
            },
        }
    }

    /// Lift a module `Ref(provider.resource)` declaration into the one schema
    /// relation used by BindingIndex. This is intentionally crate-private:
    /// resource handles are not part of the provider schema transport yet.
    pub(crate) fn resource_handle(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
    ) -> Self {
        AttributeType {
            kind: AttrTypeKind::ResourceHandle {
                provider: provider.into(),
                resource_type: resource_type.into(),
            },
        }
    }

    /// Lift a closed module string-literal type without widening away the
    /// distinction needed for resource-handle assignability.
    pub(crate) fn string_literal_set(values: Vec<String>) -> Self {
        AttributeType {
            kind: AttrTypeKind::StringLiteralSet { values },
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
                binding: RefinementBinding::MetadataOnly(identity),
                pattern,
                length,
                to_dsl,
            },
        }
    }

    /// Create a String type with refinement metadata and a custom validator.
    pub fn refined_string_with_validator(
        identity: TypeIdentity,
        pattern: Option<String>,
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
        to_dsl: Option<DslTransform>,
    ) -> Self {
        AttributeType {
            kind: AttrTypeKind::String {
                binding: RefinementBinding::Validator {
                    identity,
                    callback: validate,
                },
                pattern,
                length,
                to_dsl,
            },
        }
    }

    /// Create the primitive `Int` type.
    pub fn int() -> Self {
        AttributeType {
            kind: AttrTypeKind::Int {
                binding: RefinementBinding::MetadataOnly(None),
                range: None,
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
                binding: RefinementBinding::MetadataOnly(identity),
                range,
            },
        }
    }

    /// Create an Int type with refinement metadata and a custom validator.
    pub fn refined_int_with_validator(
        identity: TypeIdentity,
        range: Option<(Option<i64>, Option<i64>)>,
        validate: CustomValidator,
    ) -> Self {
        AttributeType {
            kind: AttrTypeKind::Int {
                binding: RefinementBinding::Validator {
                    identity,
                    callback: validate,
                },
                range,
            },
        }
    }

    /// Create the primitive `Float` type.
    pub fn float() -> Self {
        AttributeType {
            kind: AttrTypeKind::Float {
                binding: RefinementBinding::MetadataOnly(None),
                range: None,
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
                binding: RefinementBinding::MetadataOnly(identity),
                range,
            },
        }
    }

    /// Create a Float type with refinement metadata and a custom validator.
    pub fn refined_float_with_validator(
        identity: TypeIdentity,
        range: Option<(Option<f64>, Option<f64>)>,
        validate: CustomValidator,
    ) -> Self {
        AttributeType {
            kind: AttrTypeKind::Float {
                binding: RefinementBinding::Validator {
                    identity,
                    callback: validate,
                },
                range,
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
    ///   validation is absent because validation is dispatched through
    ///   the schema-owning factory; the protocol payload carries the
    ///   transform as data and the host applies it directly.
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
            kind: AttrTypeKind::Union(UnionMemberTypes::new(members)),
        }
    }

    /// Create a `Ref` type referencing a named definition in the
    /// enclosing resource or rootless definition map.
    pub fn ref_(name: impl Into<String>) -> Self {
        AttributeType {
            kind: AttrTypeKind::Ref(name.into()),
        }
    }

    /// Create a List type with default ordering (ordered=true, matching CloudFormation default).
    pub fn list(inner: AttributeType) -> Self {
        Self::refined_list(inner, true, None)
    }

    /// Create a List type with protocol-carried refinement metadata.
    pub fn refined_list(
        element_type: AttributeType,
        ordered: bool,
        length: Option<(Option<u64>, Option<u64>)>,
    ) -> Self {
        AttributeType {
            kind: AttrTypeKind::List {
                element_type: Box::new(element_type),
                ordered,
                length,
            },
        }
    }

    /// Create an unordered List type (insertionOrder=false).
    pub fn unordered_list(inner: AttributeType) -> Self {
        Self::refined_list(inner, false, None)
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
    ///    `FunctionCall`, `Unknown`) return `None` and are accepted
    ///    unconditionally — type fitness for those is the unified
    ///    deferred-reference validation's job (`BindingIndex::ref_type`,
    ///    `validate_resource_ref_types`). `Secret` is peeled first: it is a
    ///    sensitivity wrapper whose known inner value is validated here, with
    ///    any resulting diagnostic masked.
    /// 2. Dispatch the projected `ConcreteValueRef<'_>` to the
    ///    per-variant helper. Helpers cannot receive deferred values by
    ///    construction — the projection is the single place that filter
    ///    decision lives.
    ///
    /// Recursive descent into `List`/`Map`/`Struct` element types uses
    /// the same definition map again, so each nested element is
    /// independently re-projected. Lists may legitimately mix concrete
    /// and deferred elements (e.g. `[vpc.id, "literal"]`).
    #[cfg(test)]
    pub(crate) fn validate(&self, value: &Value) -> Result<(), TypeError> {
        validate_attr_with_defs(self, value, empty_defs_for_schema_walks())
    }

    /// Validate a non-container value after the ancestry-aware structural
    /// walker has selected the leaf type. Keeping Struct/Union/List/Map out of
    /// this dispatcher prevents a second walker from silently re-rooting at
    /// writable ancestry.
    fn validate_leaf_value(&self, value: &Value) -> Result<(), TypeError> {
        if let Value::Deferred(DeferredValue::Secret(inner)) = value {
            return self
                .validate_leaf_value(inner)
                .map_err(TypeError::masked_for_secret);
        }
        if let Value::Deferred(DeferredValue::Coalesce { value, default }) = value {
            self.validate_leaf_value(value)?;
            return self.validate_leaf_value(default);
        }
        debug_assert!(!matches!(
            self.kind,
            AttrTypeKind::Ref(_)
                | AttrTypeKind::List { .. }
                | AttrTypeKind::Map { .. }
                | AttrTypeKind::Struct { .. }
                | AttrTypeKind::Union(_)
        ));
        let Some(concrete) = value.as_concrete() else {
            return Ok(());
        };
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
                to_dsl.as_ref(),
                concrete,
            ),
            AttrTypeKind::String { .. }
            | AttrTypeKind::ResourceHandle { .. }
            | AttrTypeKind::StringLiteralSet { .. }
            | AttrTypeKind::Int { .. }
            | AttrTypeKind::Float { .. }
            | AttrTypeKind::Bool
            | AttrTypeKind::Duration => self.validate_primitive(concrete),
            AttrTypeKind::Ref(_)
            | AttrTypeKind::List { .. }
            | AttrTypeKind::Map { .. }
            | AttrTypeKind::Struct { .. }
            | AttrTypeKind::Union(_) => {
                unreachable!("validate_leaf_value must only be called after structural dispatch")
            }
        }
    }

    /// Validate one already-identified scalar declaration without opening a
    /// public rootless constructor for arbitrary resource-path types.
    pub(crate) fn validate_identified_scalar_value(&self, value: &Value) -> Result<(), TypeError> {
        debug_assert!(matches!(
            self.kind,
            AttrTypeKind::String { .. }
                | AttrTypeKind::Int { .. }
                | AttrTypeKind::Float { .. }
                | AttrTypeKind::Enum { .. }
        ));
        self.validate_leaf_value(value)
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
                    binding,
                    pattern,
                    length,
                    ..
                },
                ConcreteValueRef::String(s),
            ) => {
                validate_string_refinement(binding.identity(), pattern.as_deref(), *length, s)?;
                binding
                    .callback()
                    .map_or(Ok(()), |validate| validate(&value.to_owned_value()))
            }
            (AttrTypeKind::ResourceHandle { .. }, ConcreteValueRef::String(_)) => Ok(()),
            (AttrTypeKind::StringLiteralSet { values }, ConcreteValueRef::String(value))
                if values.iter().any(|candidate| candidate == value) =>
            {
                Ok(())
            }
            (AttrTypeKind::Int { binding, range }, ConcreteValueRef::Int(i)) => {
                validate_int_range(*range, i)?;
                binding
                    .callback()
                    .map_or(Ok(()), |validate| validate(&value.to_owned_value()))
            }
            (AttrTypeKind::Float { binding, range }, ConcreteValueRef::Float(f))
                if f.is_finite() =>
            {
                validate_float_range(*range, f)?;
                binding
                    .callback()
                    .map_or(Ok(()), |validate| validate(&value.to_owned_value()))
            }
            (AttrTypeKind::Float { .. }, ConcreteValueRef::Float(f)) => {
                Err(TypeError::ValidationFailed {
                    message: format!("non-finite float value: {f}"),
                })
            }
            (AttrTypeKind::Float { binding, range }, ConcreteValueRef::Int(i)) => {
                validate_float_range(*range, i as f64)?;
                binding
                    .callback()
                    .map_or(Ok(()), |validate| validate(&value.to_owned_value()))
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
        to_dsl: Option<&DslTransform>,
        value: ConcreteValueRef<'_>,
    ) -> Result<(), TypeError> {
        let prefix = identity.dotted_prefix();
        let prefix_ref = prefix.as_deref();
        let name = identity.kind.as_str();
        let expected = values
            .map(|values| enum_expected_variants(prefix_ref, name, values, dsl_aliases))
            .unwrap_or_default();
        let enumerated = values.map(|_| true).unwrap_or(false);
        if let ConcreteValueRef::CanonicalEnum(c) = value {
            if c.identity().same_type(identity) {
                return Ok(());
            }
            return Err(TypeError::TypeMismatch {
                expected: name.to_string(),
                got: c.identity().to_string(),
            });
        }
        if let ConcreteValueRef::EnumIdentifier(raw) = value {
            let enum_attr = AttributeType::enum_(
                identity.clone(),
                values.map(<[_]>::to_vec),
                dsl_aliases.to_vec(),
                validate.cloned(),
                to_dsl.cloned(),
            );
            if EnumValueResolver::new(&enum_attr)
                .resolve_raw(&RawEnumIdentifier::parse(raw.as_str()))
                .is_ok()
            {
                return Ok(());
            }
        }
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
                user_typed: s.into(),
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
        // must stay internal: error messages should show what the user
        // actually typed. See #2077.
        let user_input = match value {
            ConcreteValueRef::EnumIdentifier(s) => Some(s.as_str()),
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
                .any(|v| enum_value_matches(s.expose_plaintext(), v));
            let valid: Vec<&str> = values.into_iter().flatten().map(String::as_str).collect();
            let variant = if direct_match {
                s.expose_plaintext()
            } else if let Some(user_input) = user_input {
                // Namespace extraction is authorized by the parser's typed
                // EnumIdentifier source, never by this temporary String
                // representation's text shape.
                extract_enum_value_with_values(user_input, &valid)
            } else {
                s.expose_plaintext()
            };

            // Non-direct matches must have the exact form
            // `{namespace}.{name}.{variant}`. This rejects malformed
            // inputs like double-namespaced values while still allowing
            // enum values that themselves contain dots (e.g., "ipsec.1").
            if !direct_match && let Some(ns) = prefix_ref {
                let expected_prefix = format!("{}.{}.", ns, name);
                let prefix_matches = s.expose_plaintext().starts_with(&expected_prefix)
                    && &s.expose_plaintext()[expected_prefix.len()..] == variant;
                if !prefix_matches {
                    // Fall back to strict namespace validation, which
                    // produces a clear error for the common bare form.
                    let user_form = user_input
                        .map(DiagnosticValue::from)
                        .unwrap_or_else(|| DiagnosticValue::from(s));
                    validate_enum_namespace(s.expose_plaintext(), identity).map_err(|message| {
                        TypeError::ValidationFailed {
                            message: format!(
                                "Invalid {name} {}: {message}",
                                user_form.render_enum_value(InvalidEnumValueKind::Identifier)
                            ),
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
                    value: user_input
                        .map(DiagnosticValue::from)
                        .unwrap_or_else(|| DiagnosticValue::from(s)),
                    value_kind: InvalidEnumValueKind::Identifier,
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

    pub fn type_name(&self) -> String {
        match &self.kind {
            AttrTypeKind::Bool => "Bool".to_string(),
            AttrTypeKind::Duration => "Duration".to_string(),
            AttrTypeKind::Enum { identity, .. } => identity.to_string(),
            AttrTypeKind::String {
                binding,
                pattern,
                length,
                ..
            } => custom_display_name(binding.identity(), pattern.as_deref(), length.as_ref()),
            AttrTypeKind::ResourceHandle {
                provider,
                resource_type,
                ..
            } => format!("{provider}.{resource_type}"),
            AttrTypeKind::StringLiteralSet { values, .. } => values
                .iter()
                .map(|value| display_string_literal(value))
                .collect::<Vec<_>>()
                .join(" | "),
            AttrTypeKind::Int { binding, .. } => binding
                .identity()
                .map(ToString::to_string)
                .unwrap_or_else(|| "Int".to_string()),
            AttrTypeKind::Float { binding, .. } => binding
                .identity()
                .map(ToString::to_string)
                .unwrap_or_else(|| "Float".to_string()),
            AttrTypeKind::List {
                element_type: inner,
                ..
            } => format!("List<{}>", inner.type_name()),
            AttrTypeKind::Map { value: inner, .. } => format!("Map<{}>", inner.type_name()),
            AttrTypeKind::Struct { name, .. } => format!("Struct({})", name),
            AttrTypeKind::Union(types) => {
                let names: Vec<String> =
                    types.as_raw_slice().iter().map(|t| t.type_name()).collect();
                names.join(" | ")
            }
            AttrTypeKind::Ref(name) => format!("Ref({name})"),
        }
    }

    /// Check if this type is a String-based Custom type.
    /// Used for cross-schema type compatibility: all String-based Custom types
    /// are considered compatible with each other.
    pub fn is_string_based_custom(&self) -> bool {
        match &self.kind {
            AttrTypeKind::String {
                binding,
                pattern,
                length,
                ..
            } => binding.identity().is_some() || pattern.is_some() || length.is_some(),
            _ => false,
        }
    }
}

impl TypeInSchema<'_> {
    /// Check if a value of this source type can be assigned to `sink`.
    /// Directional: narrowing source → wider sink is OK, but widening source →
    /// narrower sink is NG. Each side resolves references against the
    /// definition map carried by its own [`TypeInSchema`].
    ///
    /// Rules (first match wins):
    /// 1. Peel top-level Ref chains on both sides. A missing definition,
    ///    cycle, or hop-limit failure rejects that comparison.
    /// 2. Union source: OK iff every source member is assignable to the sink.
    /// 3. Union sink: OK if source is assignable to any member.
    /// 4. List→List: recurse on the element types. Ordering and length metadata
    ///    remain intentionally ignored, matching the old `type_name` behavior.
    /// 5. Map→Map: recurse on both key and value types.
    /// 6. Struct→Struct requires the same field-name set and recursively
    ///    assignable field types. Struct→Map is allowed when every field is
    ///    assignable to the map value type, preserving the DSL's structural
    ///    object-to-homogeneous-map coercion.
    /// 7. Custom→Custom with both `identity: Some`: the source's identity
    ///    must be [`TypeIdentity::assignable_to`] the sink's and any length
    ///    range must be contained by the sink's range. Enum→Enum additionally
    ///    recurses on the two base types. This is the final verdict for
    ///    identified custom types: pattern checks are subsumed by identity
    ///    subsumption and are not consulted in this arm, while length
    ///    containment remains a structural safety check. Directional
    ///    per-axis subsumption — an
    ///    empty axis on the **sink** widens (any source matches), but an empty
    ///    axis on the **source** against a populated sink is rejected (no
    ///    evidence). So `aws.iam.Role.Arn` flows into `aws.Arn` (sink is
    ///    wider) but `aws.Arn` does not flow into `aws.iam.Role.Arn` (source
    ///    has no Role-specific evidence). `aws.Region` and `gcp.Region` are
    ///    rejected both ways (populated providers differ). Closes carina#3218.
    /// 8. Custom→Custom where at least one side has `identity: None`: check
    ///    pattern (literal equality) and length containment (source ⊆ sink),
    ///    then recurse on base. For both-identified pairs, see rule 7.
    /// 9. Custom source → non-Custom sink: recurse on `source.base`.
    /// 10. A plain, unrefined primitive source → refined sink is split by
    ///     the sink's identity. An identified sink (including every Enum) is
    ///     NG because the source has no identity evidence. An identity-less
    ///     sink is OK when the primitive bases are assignable; its
    ///     pattern/length/range is a value constraint, enforced when the
    ///     concrete value is known (carina#3805). Rules 4–6 make the same
    ///     leaf decision recursively inside List/Map/Struct, and rules 2–3
    ///     preserve their normal all/any Union semantics.
    /// 11. Otherwise: same primitive type names.
    ///
    /// # Conservative pattern/length policy
    ///
    /// Rule 8 remains evidence-preserving for a source that carries any
    /// refinement of its own. A populated sink pattern requires literal
    /// string equality; regex-language containment is not inferred. A sink
    /// length range requires the source range to be a subset (`sink.min ≤
    /// source.min` and `source.max ≤ sink.max`, with absent bounds treated
    /// as unbounded). Thus a source whose own pattern differs, whose range is
    /// wider, or whose other refinement evidence does not establish the
    /// sink's constraint is rejected.
    ///
    /// A completely unrefined source is intentionally different: it makes
    /// no refinement claim that can conflict with the sink. Rule 10 accepts
    /// its assignable primitive base only into an identity-less sink, then
    /// leaves the sink's pattern/length value check to carina#3805. It never
    /// supplies evidence for an identified custom type or Enum.
    ///
    /// **Do not loosen rule 8's evidence comparisons.** Accepting a wider or
    /// differently refined source would re-introduce the silent-false-positive
    /// class that #2218 closed. Rule 10's no-evidence exception is limited to
    /// identity-less sinks and does not weaken those comparisons.
    pub fn is_assignable_to(self, sink: TypeInSchema<'_>) -> bool {
        let mut source_visited_refs = Vec::new();
        let mut sink_visited_refs = Vec::new();
        self.is_assignable_to_on_paths(sink, &mut source_visited_refs, &mut sink_visited_refs)
    }
}

impl<'source> TypeInSchema<'source> {
    /// Pair a structurally Ref-free type with an empty definition context.
    ///
    /// Use this only when the type cannot contain [`AttrTypeKind::Ref`] at any
    /// depth, such as a type parsed from an exports annotation. A type derived
    /// from a resource schema must instead travel with that schema's defs via
    /// [`ResourceSchema::type_in_schema`] or [`AttributeType::in_schema`].
    pub fn schemaless(attr: &'source AttributeType) -> Self {
        attr.in_schema(empty_defs_for_schema_walks())
    }

    /// Return a defs-aware display name, resolving Refs recursively through
    /// Union members and List/Map element types.
    ///
    /// Missing definitions, cycles, and hop-limit failures fall back to the
    /// raw declared name at the failing node so diagnostics remain total and
    /// never panic.
    pub fn resolved_type_name(&self) -> String {
        let mut visited_refs = Vec::new();
        self.resolved_type_name_on_path(&mut visited_refs)
    }

    fn resolved_type_name_on_path(&self, visited_refs: &mut Vec<&'source str>) -> String {
        let Ok(resolution) = resolve_refs_on_path(self.attr, self.defs, visited_refs) else {
            return self.attr.type_name();
        };
        let resolved = resolution.attr;
        let name = match &resolved.kind {
            AttrTypeKind::List {
                element_type: inner,
                ..
            } => format!(
                "List<{}>",
                inner
                    .in_schema(self.defs)
                    .resolved_type_name_on_path(visited_refs)
            ),
            // `type_name` intentionally renders only a Map's value type; keep
            // that public spelling while resolving the displayed node.
            AttrTypeKind::Map { value: inner, .. } => format!(
                "Map<{}>",
                inner
                    .in_schema(self.defs)
                    .resolved_type_name_on_path(visited_refs)
            ),
            AttrTypeKind::Union(members) => members
                .as_raw_slice()
                .iter()
                .map(|member| {
                    member
                        .in_schema(self.defs)
                        .resolved_type_name_on_path(visited_refs)
                })
                .collect::<Vec<_>>()
                .join(" | "),
            _ => resolved.type_name(),
        };
        visited_refs.truncate(resolution.restore_len);
        name
    }

    fn is_assignable_to_on_paths<'sink>(
        self,
        sink: TypeInSchema<'sink>,
        source_visited_refs: &mut Vec<&'source str>,
        sink_visited_refs: &mut Vec<&'sink str>,
    ) -> bool {
        let Ok(source_resolution) = resolve_refs_on_path(self.attr, self.defs, source_visited_refs)
        else {
            return false;
        };
        let Ok(sink_resolution) = resolve_refs_on_path(sink.attr, sink.defs, sink_visited_refs)
        else {
            source_visited_refs.truncate(source_resolution.restore_len);
            return false;
        };

        let source = TypeInSchema {
            attr: source_resolution.attr,
            defs: self.defs,
        };
        let sink = TypeInSchema {
            attr: sink_resolution.attr,
            defs: sink.defs,
        };
        let result = source.is_assignable_to_resolved(sink, source_visited_refs, sink_visited_refs);
        source_visited_refs.truncate(source_resolution.restore_len);
        sink_visited_refs.truncate(sink_resolution.restore_len);
        result
    }

    fn is_assignable_to_resolved<'sink>(
        self,
        sink: TypeInSchema<'sink>,
        source_visited_refs: &mut Vec<&'source str>,
        sink_visited_refs: &mut Vec<&'sink str>,
    ) -> bool {
        use AttrTypeKind::*;
        if let Union(members) = &self.attr.kind {
            return members.as_raw_slice().iter().all(|member| {
                member.in_schema(self.defs).is_assignable_to_on_paths(
                    sink,
                    source_visited_refs,
                    sink_visited_refs,
                )
            });
        }
        if let Union(members) = &sink.attr.kind {
            return members.as_raw_slice().iter().any(|member| {
                self.is_assignable_to_on_paths(
                    member.in_schema(sink.defs),
                    source_visited_refs,
                    sink_visited_refs,
                )
            });
        }
        match (&self.attr.kind, &sink.attr.kind) {
            (
                ResourceHandle {
                    provider: source_provider,
                    resource_type: source_resource,
                    ..
                },
                ResourceHandle {
                    provider: sink_provider,
                    resource_type: sink_resource,
                    ..
                },
            ) => source_provider == sink_provider && source_resource == sink_resource,
            // #3803 transport is not implemented yet, so resource handles are
            // represented by id strings at runtime. Preserve main's
            // compatibility with every open string-shaped sink while keeping
            // closed literal sets closed.
            (ResourceHandle { .. }, String { .. }) => true,
            (ResourceHandle { .. }, StringLiteralSet { .. }) => false,
            (ResourceHandle { .. }, _) => false,
            // Literal declarations carry string evidence but not the identity
            // evidence required by a provider custom type (#2648).
            (
                StringLiteralSet { .. },
                String {
                    binding: RefinementBinding::MetadataOnly(None),
                    ..
                },
            ) => true,
            (StringLiteralSet { .. }, String { binding, .. }) if binding.identity().is_some() => {
                false
            }
            (StringLiteralSet { values: source, .. }, StringLiteralSet { values: sink, .. }) => {
                source.iter().all(|value| sink.contains(value))
            }
            // String-backed values and attributes may still flow into a Ref
            // sink until resource-handle transport is implemented. Numeric and
            // structural sources never reach this arm.
            (String { .. } | Enum { .. } | StringLiteralSet { .. }, ResourceHandle { .. }) => true,
            // Preserve the pre-existing declared String -> literal-union
            // behavior while ensuring ResourceHandle took the rejecting arm
            // above.
            (String { .. }, StringLiteralSet { .. }) => true,
            (_, ResourceHandle { .. } | StringLiteralSet { .. }) => false,
            (
                List {
                    element_type: source_element,
                    ..
                },
                List {
                    element_type: sink_element,
                    ..
                },
            ) => source_element
                .in_schema(self.defs)
                .is_assignable_to_on_paths(
                    sink_element.in_schema(sink.defs),
                    source_visited_refs,
                    sink_visited_refs,
                ),
            (
                Map {
                    key: source_key,
                    value: source_value,
                },
                Map {
                    key: sink_key,
                    value: sink_value,
                },
            ) => {
                source_key.in_schema(self.defs).is_assignable_to_on_paths(
                    sink_key.in_schema(sink.defs),
                    source_visited_refs,
                    sink_visited_refs,
                ) && source_value.in_schema(self.defs).is_assignable_to_on_paths(
                    sink_value.in_schema(sink.defs),
                    source_visited_refs,
                    sink_visited_refs,
                )
            }
            (
                Struct {
                    fields: source_fields,
                    ..
                },
                Struct {
                    fields: sink_fields,
                    ..
                },
            ) => {
                source_fields.len() == sink_fields.len()
                    && sink_fields.iter().all(|sink_field| {
                        source_fields
                            .iter()
                            .find(|source_field| source_field.name == sink_field.name)
                            .is_some_and(|source_field| {
                                source_field
                                    .field_type
                                    .in_schema(self.defs)
                                    .is_assignable_to_on_paths(
                                        sink_field.field_type.in_schema(sink.defs),
                                        source_visited_refs,
                                        sink_visited_refs,
                                    )
                            })
                    })
            }
            (Struct { fields, .. }, Map { value, .. }) => fields.iter().all(|field| {
                field
                    .field_type
                    .in_schema(self.defs)
                    .is_assignable_to_on_paths(
                        value.in_schema(sink.defs),
                        source_visited_refs,
                        sink_visited_refs,
                    )
            }),
            (
                String {
                    binding: source_binding,
                    length: s_len,
                    ..
                },
                String {
                    binding: sink_binding,
                    length: k_len,
                    ..
                },
            ) if let (Some(source_identity), Some(sink_identity)) =
                (source_binding.identity(), sink_binding.identity()) =>
            {
                source_identity.assignable_to(sink_identity)
                    && length_contains(s_len.as_ref(), k_len.as_ref())
            }
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
            ) => {
                s_id.assignable_to(k_id)
                    && s_base.in_schema(self.defs).is_assignable_to_on_paths(
                        k_base.in_schema(sink.defs),
                        source_visited_refs,
                        sink_visited_refs,
                    )
            }
            // Anonymous source → identified sink has no proof of identity.
            (
                String {
                    binding: RefinementBinding::MetadataOnly(None),
                    ..
                },
                String { binding, .. },
            ) if binding.identity().is_some() => false,
            (
                Int {
                    binding: RefinementBinding::MetadataOnly(None),
                    ..
                },
                Int { binding, .. },
            ) if binding.identity().is_some() => false,
            (
                Float {
                    binding: RefinementBinding::MetadataOnly(None),
                    ..
                },
                Float { binding, .. },
            ) if binding.identity().is_some() => false,
            // Numeric widening is directional: an Int source may flow into
            // a Float sink, matching concrete-value validation. Preserve
            // identity and range evidence when either side carries it.
            (
                Int {
                    binding: source_binding,
                    range: source_range,
                    ..
                },
                Float {
                    binding: sink_binding,
                    range: sink_range,
                    ..
                },
            ) => {
                if let Some(sink_identity) = sink_binding.identity()
                    && !source_binding
                        .identity()
                        .is_some_and(|source_identity| source_identity.assignable_to(sink_identity))
                {
                    return false;
                }
                if source_range.is_none() && sink_binding.identity().is_none() {
                    return true;
                }
                i64_range_assignable_to_f64(source_range.as_ref(), sink_range.as_ref())
            }
            // A completely unrefined primitive source carries no competing
            // refinement evidence. Identity-less sink metadata is a value
            // constraint checked when the concrete value is known (#3805).
            // The identified-sink arm above must remain ahead of this one.
            (
                String {
                    binding: RefinementBinding::MetadataOnly(None),
                    pattern: None,
                    length: None,
                    ..
                },
                String {
                    binding: RefinementBinding::MetadataOnly(None),
                    ..
                },
            ) => true,
            (
                Int {
                    binding: RefinementBinding::MetadataOnly(None),
                    range: None,
                    ..
                },
                Int {
                    binding: RefinementBinding::MetadataOnly(None),
                    ..
                },
            ) => true,
            (
                Float {
                    binding: RefinementBinding::MetadataOnly(None),
                    range: None,
                    ..
                },
                Float {
                    binding: RefinementBinding::MetadataOnly(None),
                    ..
                },
            ) => true,
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
                    binding: source_binding,
                    range: s_range,
                    ..
                },
                Int {
                    binding: sink_binding,
                    range: k_range,
                    ..
                },
            ) if let (Some(source_identity), Some(sink_identity)) =
                (source_binding.identity(), sink_binding.identity()) =>
            {
                source_identity.assignable_to(sink_identity)
                    && i64_range_contains(s_range.as_ref(), k_range.as_ref())
            }
            (Int { range: s_range, .. }, Int { range: k_range, .. }) => {
                i64_range_contains(s_range.as_ref(), k_range.as_ref())
            }
            (
                Float {
                    binding: source_binding,
                    range: s_range,
                    ..
                },
                Float {
                    binding: sink_binding,
                    range: k_range,
                    ..
                },
            ) if let (Some(source_identity), Some(sink_identity)) =
                (source_binding.identity(), sink_binding.identity()) =>
            {
                source_identity.assignable_to(sink_identity)
                    && f64_range_contains(s_range.as_ref(), k_range.as_ref())
            }
            (Float { range: s_range, .. }, Float { range: k_range, .. }) => {
                f64_range_contains(s_range.as_ref(), k_range.as_ref())
            }
            (String { .. }, _) | (Int { .. }, _) | (Float { .. }, _)
                if self.attr.type_name() != sink.attr.type_name() =>
            {
                false
            }
            (_a, _b) => self.attr.type_name() == sink.attr.type_name(),
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
    s: &ValueString,
) -> Result<(), TypeError> {
    let type_name = identity.map(|id| id.kind.clone());
    if let Some(pattern) = pattern
        && let Ok(re) = regex::Regex::new(pattern)
        && !re.is_match(s.expose_plaintext())
    {
        return Err(TypeError::PatternMismatch {
            value: s.into(),
            pattern: pattern.to_string(),
            attribute: None,
            type_name,
        });
    }
    if let Some((min, max)) = length {
        let count = s.expose_plaintext().chars().count();
        let count_u64 = count as u64;
        if min.is_some_and(|min| count_u64 < min) || max.is_some_and(|max| count_u64 > max) {
            return Err(TypeError::LengthOutOfRange {
                value: s.into(),
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
                value: format!("{count} items").into(),
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

/// Ordered exactness accumulated across an entire value tree.
///
/// Fewer coercions always win. The secondary generic-map cost preserves the
/// historical preference for a closed Struct that accepts every supplied key
/// over an otherwise-equivalent homogeneous Map. Exact ties retain declaration
/// order.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct FitExactness {
    coercions: u32,
    generic_maps: u32,
}

impl FitExactness {
    const EXACT: Self = Self {
        coercions: 0,
        generic_maps: 0,
    };

    const COERCIBLE: Self = Self {
        coercions: 1,
        generic_maps: 0,
    };

    const GENERIC_MAP: Self = Self {
        coercions: 0,
        generic_maps: 1,
    };

    fn combine(&mut self, child: Self) {
        self.coercions = self.coercions.saturating_add(child.coercions);
        self.generic_maps = self.generic_maps.saturating_add(child.generic_maps);
    }

    fn is_maximal(self) -> bool {
        self == Self::EXACT
    }
}

impl PartialOrd for FitExactness {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FitExactness {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .coercions
            .cmp(&self.coercions)
            .then_with(|| other.generic_maps.cmp(&self.generic_maps))
    }
}

/// A successful purpose-aware fit plus the decisions needed to consume it.
///
/// The witness owns every nested Union choice and resolved enum value. A
/// canonicalization caller can therefore walk the chosen tree without running
/// validation or enum resolution a second time.
pub(crate) struct Fit<'a> {
    exactness: FitExactness,
    pub(crate) attr: &'a AttributeType,
    pub(crate) kind: FitKind<'a>,
}

pub(crate) enum FitKind<'a> {
    PassThrough,
    Deferred,
    Secret(Box<Fit<'a>>),
    Coalesce {
        value: Box<Fit<'a>>,
        default: Box<Fit<'a>>,
    },
    Enum {
        canonical: CanonicalEnumValue,
        rewrite: bool,
    },
    List(Vec<Fit<'a>>),
    StringList(Vec<Fit<'a>>),
    Map(Vec<MapEntryFit<'a>>),
    Struct(Vec<Fit<'a>>),
    Union {
        member: &'a AttributeType,
        fit: Box<Fit<'a>>,
    },
}

pub(crate) struct MapEntryFit<'a> {
    pub(crate) canonical_key: Option<String>,
    pub(crate) value: Fit<'a>,
}

fn canonical_enum_key_from_fit(fit: &Fit<'_>) -> Option<String> {
    match &fit.kind {
        FitKind::Enum { canonical, .. } => Some(canonical.api_value().to_string()),
        FitKind::Union { fit, .. } => canonical_enum_key_from_fit(fit),
        FitKind::PassThrough
        | FitKind::Deferred
        | FitKind::Secret(_)
        | FitKind::Coalesce { .. }
        | FitKind::List(_)
        | FitKind::StringList(_)
        | FitKind::Map(_)
        | FitKind::Struct(_) => None,
    }
}

impl Fit<'_> {
    fn with_kind<'a>(
        exactness: FitExactness,
        attr: &'a AttributeType,
        kind: FitKind<'a>,
    ) -> Fit<'a> {
        Fit {
            exactness,
            attr,
            kind,
        }
    }
}

impl PartialEq for Fit<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.exactness == other.exactness
    }
}

impl Eq for Fit<'_> {}

impl PartialOrd for Fit<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Fit<'_> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.exactness.cmp(&other.exactness)
    }
}

struct UnionFit<'a> {
    member: &'a AttributeType,
    fit: Fit<'a>,
}

/// Compute the complete fit of `value` against `attr` for `purpose`.
///
/// Ref paths are retained only while examining the same value node. Container
/// descent starts a fresh path because it consumes one value level, allowing a
/// recursive schema to revisit the same definition for a child value.
fn fit_value_to_type_on_path<'a>(
    attr: &'a AttributeType,
    value: &Value,
    defs: &'a BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    purpose: CanonicalizationPurpose<'_>,
    visited_refs: &mut Vec<&'a str>,
) -> Option<Fit<'a>> {
    let resolution = resolve_refs_on_path(attr, defs, visited_refs).ok()?;
    let resolved = resolution.attr;
    let fit =
        fit_value_to_resolved_type_on_path(resolved, value, defs, ancestry, purpose, visited_refs);
    visited_refs.truncate(resolution.restore_len);
    fit
}

pub(crate) fn fit_value_to_resolved_type_on_path<'a>(
    attr: &'a AttributeType,
    value: &Value,
    defs: &'a BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    purpose: CanonicalizationPurpose<'_>,
    visited_refs: &mut Vec<&'a str>,
) -> Option<Fit<'a>> {
    use AttrTypeKind as AT;

    if let Value::Deferred(DeferredValue::Secret(inner)) = value {
        let inner =
            fit_value_to_resolved_type_on_path(attr, inner, defs, ancestry, purpose, visited_refs)?;
        let exactness = inner.exactness;
        return Some(Fit::with_kind(
            exactness,
            attr,
            FitKind::Secret(Box::new(inner)),
        ));
    }

    if let Value::Deferred(DeferredValue::Coalesce { value, default }) = value {
        let value_fit =
            fit_value_to_type_on_path(attr, value, defs, ancestry, purpose, &mut Vec::new())?;
        let default_fit =
            fit_value_to_type_on_path(attr, default, defs, ancestry, purpose, &mut Vec::new())?;
        let mut exactness = value_fit.exactness;
        exactness.combine(default_fit.exactness);
        return Some(Fit::with_kind(
            exactness,
            attr,
            FitKind::Coalesce {
                value: Box::new(value_fit),
                default: Box::new(default_fit),
            },
        ));
    }

    let Some(concrete) = value.as_concrete() else {
        return Some(Fit::with_kind(FitExactness::EXACT, attr, FitKind::Deferred));
    };

    match (&attr.kind, concrete) {
        (AT::Enum { identity, .. }, ConcreteValueRef::CanonicalEnum(canonical))
            if canonical.identity().same_type(identity) =>
        {
            Some(Fit::with_kind(
                FitExactness::EXACT,
                attr,
                FitKind::Enum {
                    canonical: canonical.clone(),
                    rewrite: false,
                },
            ))
        }
        (AT::Enum { identity, .. }, ConcreteValueRef::EnumIdentifier(raw)) => {
            let resolver = EnumValueResolver::with_defs(attr, defs);
            let canonical = if purpose.resolves_identifiers_as_state_text() {
                resolver.resolve_state_text(raw.as_str())
            } else if let Some(provider_name) = purpose.provider_config_name() {
                crate::value::resolve_provider_config_enum_identifier(
                    &resolver,
                    raw,
                    identity,
                    provider_name,
                )
            } else {
                resolver.resolve_raw(raw)
            }
            .ok()?;
            Some(Fit::with_kind(
                FitExactness::EXACT,
                attr,
                FitKind::Enum {
                    canonical,
                    rewrite: true,
                },
            ))
        }
        (AT::Enum { values, .. }, ConcreteValueRef::String(text))
            if purpose.accepts_enum_string(values.is_some()) =>
        {
            let canonical = EnumValueResolver::with_defs(attr, defs)
                .resolve_state_text(text.expose_plaintext())
                .ok()?;
            Some(Fit::with_kind(
                FitExactness::COERCIBLE,
                attr,
                FitKind::Enum {
                    canonical,
                    rewrite: purpose.canonicalizes_full_shape(),
                },
            ))
        }
        (
            AT::List {
                element_type,
                length,
                ..
            },
            ConcreteValueRef::List(items),
        ) => {
            validate_list_length(*length, items.len()).ok()?;
            let mut exactness = FitExactness::EXACT;
            let mut item_fits = Vec::with_capacity(items.len());
            for item in items {
                let child = fit_value_to_type_on_path(
                    element_type,
                    item,
                    defs,
                    ancestry,
                    purpose,
                    &mut Vec::new(),
                )?;
                exactness.combine(child.exactness);
                item_fits.push(child);
            }
            Some(Fit::with_kind(exactness, attr, FitKind::List(item_fits)))
        }
        (
            AT::List {
                element_type,
                length,
                ..
            },
            ConcreteValueRef::StringList(items),
        ) => {
            validate_list_length(*length, items.len()).ok()?;
            let mut exactness = FitExactness::EXACT;
            let mut item_fits = Vec::with_capacity(items.len());
            for item in items {
                let item = Value::Concrete(ConcreteValue::String(item.clone()));
                let child = fit_value_to_type_on_path(
                    element_type,
                    &item,
                    defs,
                    ancestry,
                    purpose,
                    &mut Vec::new(),
                )?;
                exactness.combine(child.exactness);
                item_fits.push(child);
            }
            Some(Fit::with_kind(
                exactness,
                attr,
                FitKind::StringList(item_fits),
            ))
        }
        (
            AT::Map {
                key,
                value: value_type,
            },
            ConcreteValueRef::Map(map),
        ) => {
            let mut exactness = FitExactness::GENERIC_MAP;
            let mut entry_fits = Vec::with_capacity(map.len());
            for (map_key, map_value) in map {
                let lifted_key = lift_map_key(key, map_key, defs);
                let key_fit = fit_value_to_type_on_path(
                    key,
                    &lifted_key,
                    defs,
                    ancestry,
                    purpose,
                    &mut Vec::new(),
                )?;
                exactness.combine(key_fit.exactness);
                let canonical_key = canonical_enum_key_from_fit(&key_fit);
                let value_fit = fit_value_to_type_on_path(
                    value_type,
                    map_value,
                    defs,
                    ancestry,
                    purpose,
                    &mut Vec::new(),
                )?;
                exactness.combine(value_fit.exactness);
                entry_fits.push(MapEntryFit {
                    canonical_key,
                    value: value_fit,
                });
            }
            Some(Fit::with_kind(exactness, attr, FitKind::Map(entry_fits)))
        }
        (AT::Struct { fields, .. }, ConcreteValueRef::Map(map)) => {
            for field in fields {
                if ancestry.effective_input_mode(field).is_required()
                    && !struct_field_is_present(fields, map, field, ancestry)
                {
                    return None;
                }
            }

            let mut exactness = FitExactness::EXACT;
            let mut field_fits = Vec::with_capacity(map.len());
            for (input_name, field_value) in map {
                let field = struct_field_for_key(fields, input_name, ancestry)?;
                if purpose.validates_struct_assignments()
                    && validate_struct_field_assignment(field, input_name, ancestry).is_some()
                {
                    return None;
                }
                let child = fit_value_to_type_on_path(
                    &field.field_type,
                    field_value,
                    defs,
                    ancestry.descend(field),
                    purpose,
                    &mut Vec::new(),
                )?;
                exactness.combine(child.exactness);
                field_fits.push(child);
            }
            Some(Fit::with_kind(exactness, attr, FitKind::Struct(field_fits)))
        }
        (AT::Union(members), _) => {
            let selected = fit_union_members_on_path(
                members.as_raw_slice(),
                value,
                defs,
                ancestry,
                purpose,
                visited_refs,
            )?;
            let exactness = selected.fit.exactness;
            Some(Fit::with_kind(
                exactness,
                attr,
                FitKind::Union {
                    member: selected.member,
                    fit: Box::new(selected.fit),
                },
            ))
        }
        (AT::Ref(_), _) => unreachable!("fit dispatch received an unresolved Ref"),
        (
            AT::String { .. }
            | AT::ResourceHandle { .. }
            | AT::StringLiteralSet { .. }
            | AT::Int { .. }
            | AT::Float { .. }
            | AT::Bool
            | AT::Duration,
            _,
        ) => {
            attr.validate_primitive(concrete).ok()?;
            let exactness = if matches!(
                (&attr.kind, concrete),
                (AT::Float { .. }, ConcreteValueRef::Int(_))
            ) {
                FitExactness::COERCIBLE
            } else {
                FitExactness::EXACT
            };
            Some(Fit::with_kind(exactness, attr, FitKind::PassThrough))
        }
        _ => None,
    }
}

fn fit_union_members_on_path<'a>(
    members: &'a [AttributeType],
    value: &Value,
    defs: &'a BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    purpose: CanonicalizationPurpose<'_>,
    visited_refs: &mut Vec<&'a str>,
) -> Option<UnionFit<'a>> {
    let mut best: Option<UnionFit<'a>> = None;
    for member in members {
        let Some(fit) =
            fit_value_to_type_on_path(member, value, defs, ancestry, purpose, visited_refs)
        else {
            continue;
        };
        if best.as_ref().is_none_or(|current| fit > current.fit) {
            best = Some(UnionFit { member, fit });
        }
        if best
            .as_ref()
            .is_some_and(|selected| selected.fit.exactness.is_maximal())
        {
            break;
        }
    }
    best
}

/// Total ordering used only after every Union member rejects, to choose the
/// member error that best explains the supplied shape.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct UnionFailureRank(u32);

impl UnionFailureRank {
    const NO_MATCH: Self = Self(0);
    const SAME_CONSTRUCTOR: Self = Self(80);
    const MAP_TO_STRUCT: Self = Self(100);

    fn is_match(self) -> bool {
        self != Self::NO_MATCH
    }

    fn list_with_inner(inner: Self) -> Self {
        Self(Self::SAME_CONSTRUCTOR.0 + inner.0 / 2)
    }
}

/// Exact supplied keys recognized by one closed-Struct Union arm.
///
/// The set is intentionally opaque: diagnostic selection reasons about field
/// identity and emptiness, never cardinality. It therefore cannot be compared
/// with [`UnionFailureRank`] or accidentally used as its numeric bonus.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RecognizedStructFields(IndexSet<String>);

impl RecognizedStructFields {
    fn new(fields: &[StructField], map: &IndexMap<String, Value>, ancestry: FieldAncestry) -> Self {
        Self(
            map.keys()
                .filter(|field| struct_field_for_key(fields, field, ancestry).is_some())
                .cloned()
                .collect(),
        )
    }

    fn union<'a>(sets: impl Iterator<Item = &'a Self>) -> Self {
        let mut union = IndexSet::new();
        for set in sets {
            union.extend(set.0.iter().cloned());
        }
        Self(union)
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn contains(&self, field: &str) -> bool {
        self.0.contains(field)
    }
}

/// Diagnostic equivalence axis carried by the shared structural judgement.
///
/// Unlike [`UnionFailureRank`], this type has no ordering. A Struct match
/// retains the actual recognized-field set so validation can distinguish one
/// uniquely relevant arm from multiple co-equal alternatives without reducing
/// those sets to counts.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UnionDiagnosticIdentity {
    ClosedStruct(RecognizedStructFields),
    OtherShape,
}

/// Shape judgement used exclusively for failure attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnionMemberJudgement {
    failure_rank: UnionFailureRank,
    diagnostic_identity: UnionDiagnosticIdentity,
}

impl UnionMemberJudgement {
    fn no_match() -> Self {
        Self::other_shape(UnionFailureRank::NO_MATCH)
    }

    fn other_shape(failure_rank: UnionFailureRank) -> Self {
        Self {
            failure_rank,
            diagnostic_identity: UnionDiagnosticIdentity::OtherShape,
        }
    }
}

/// Judge a Union member against a runtime value once, preserving both facts
/// its consumers need: the pre-#3754 structural rank and, for a closed Struct,
/// the exact supplied keys that arm recognizes.
///
/// The failure rank keeps #2219's tiers unchanged: Map↔Struct is 100,
/// same-constructor matches are 80, List peeks at its first element, nested
/// Union takes its best inner rank, and unrelated shapes are zero. Refined
/// primitives retain their declared primitive constructor. Callers that need
/// one member, such as canonicalization's closest-member selector, retain the
/// declaration-order strict-max tie-break. Diagnostic selection preserves all
/// co-maximal failures instead.
fn union_member_judgement_on_path<'a>(
    member: &'a AttributeType,
    value: ConcreteValueRef<'_>,
    defs: &'a BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    visited_refs: &mut Vec<&'a str>,
) -> UnionMemberJudgement {
    use AttrTypeKind as AT;
    let Ok(resolution) = resolve_refs_on_path(member, defs, visited_refs) else {
        return UnionMemberJudgement::no_match();
    };
    let member = resolution.attr;
    let judgement = match (&member.kind, value) {
        // Map↔Struct: preserve the original flat rank. Field recognition is
        // diagnostic identity, not a numeric ordering bonus.
        (AT::Struct { fields, .. }, ConcreteValueRef::Map(map)) => UnionMemberJudgement {
            failure_rank: UnionFailureRank::MAP_TO_STRUCT,
            diagnostic_identity: UnionDiagnosticIdentity::ClosedStruct(
                RecognizedStructFields::new(fields, map, ancestry),
            ),
        },
        // Same-constructor match — second tier.
        (AT::Map { .. }, ConcreteValueRef::Map(_))
        | (AT::String { .. }, ConcreteValueRef::String(_))
        | (AT::ResourceHandle { .. }, ConcreteValueRef::String(_))
        | (AT::StringLiteralSet { .. }, ConcreteValueRef::String(_))
        | (AT::Int { .. }, ConcreteValueRef::Int(_))
        | (AT::Float { .. }, ConcreteValueRef::Float(_))
        | (AT::Bool, ConcreteValueRef::Bool(_)) => {
            UnionMemberJudgement::other_shape(UnionFailureRank::SAME_CONSTRUCTOR)
        }
        // Enum is string-shaped for diagnostics even when membership fails:
        // this preserves InvalidEnumVariant / StringLiteralExpectedEnum and
        // their valid-value suggestions when no Union arm accepts.
        (AT::Enum { .. }, ConcreteValueRef::String(_))
        | (AT::Enum { .. }, ConcreteValueRef::EnumIdentifier(_))
        | (AT::Enum { .. }, ConcreteValueRef::CanonicalEnum(_)) => {
            UnionMemberJudgement::other_shape(UnionFailureRank::SAME_CONSTRUCTOR)
        }
        // List↔List: peek at the first element's structural match
        // against the member's inner type so `List<Struct>` outranks
        // `List<String>` for an input like `[{...}]`. Numeric tiers can
        // cross under nesting: `List<Struct>` ranks 130, above the direct
        // Map↔Struct rank 100. This is safe because an outer List value gives
        // a direct Struct arm rank zero; only List arms compete at that outer
        // shape. Halving bounds arbitrarily deep List recursion below 160
        // while preserving the pre-#3754 ordering. Empty lists fall back to
        // the bare same-constructor rank.
        // Inner peek requires
        // re-projecting the first element through `as_concrete()` —
        // a deferred element contributes no bonus (the projection
        // returns `None`). List descent consumes one value level, matching
        // The ancestry-aware validator's fresh-path element recursion;
        // retaining an
        // outer Union alias here would make selection disagree with validation.
        (
            AT::List {
                element_type: inner,
                ..
            },
            ConcreteValueRef::List(items),
        ) => {
            let inner_rank = items
                .first()
                .and_then(|first| first.as_concrete())
                .map(|first| {
                    let mut element_path = Vec::new();
                    union_member_judgement_on_path(inner, first, defs, ancestry, &mut element_path)
                        .failure_rank
                })
                .unwrap_or(UnionFailureRank::NO_MATCH);
            UnionMemberJudgement::other_shape(UnionFailureRank::list_with_inner(inner_rank))
        }
        // Nested Union: recurse and retain the first best inner judgement.
        (AT::Union(inner), v) => inner
            .as_raw_slice()
            .iter()
            .map(|member| union_member_judgement_on_path(member, v, defs, ancestry, visited_refs))
            .reduce(|best, candidate| {
                if candidate.failure_rank > best.failure_rank {
                    candidate
                } else {
                    best
                }
            })
            .unwrap_or_else(UnionMemberJudgement::no_match),
        _ => UnionMemberJudgement::no_match(),
    };
    visited_refs.truncate(resolution.restore_len);
    judgement
}

struct UnionMemberFailure {
    error: TypeError,
    diagnostic_identity: UnionDiagnosticIdentity,
}

/// A non-empty set of failures in the best structural tier.
///
/// This accumulator never selects the first member as a fallback. Its complete
/// contents are either consumed by the closed-Struct diagnostic from #3754 or
/// passed to [`UnionArmFailures`], which owns deduplication and the at-least-two
/// invariant for general Union diagnostics.
struct BestUnionMemberFailures {
    failure_rank: UnionFailureRank,
    first: UnionMemberFailure,
    tied: Vec<UnionMemberFailure>,
}

impl BestUnionMemberFailures {
    fn new(failure_rank: UnionFailureRank, first: UnionMemberFailure) -> Self {
        Self {
            failure_rank,
            first,
            tied: Vec::new(),
        }
    }

    fn into_error(
        self,
        value: ConcreteValueRef<'_>,
        members: UnionMembers<'_>,
        ancestry: FieldAncestry,
    ) -> TypeError {
        let BestUnionMemberFailures {
            failure_rank: _,
            first,
            tied,
        } = self;

        let struct_context = if let ConcreteValueRef::Map(map) = value
            && !tied.is_empty()
            && let Some(alternatives) = describe_struct_union_members(members, ancestry)
            && std::iter::once(&first).chain(&tied).all(|failure| {
                matches!(
                    &failure.diagnostic_identity,
                    UnionDiagnosticIdentity::ClosedStruct(_)
                )
            }) {
            Some((map, alternatives))
        } else {
            None
        };

        let Some((map, alternatives)) = struct_context else {
            return TypeError::from_union_member_failures(first, tied);
        };
        let mut failures = Vec::with_capacity(tied.len() + 1);
        failures.push(first);
        failures.extend(tied);

        // One non-empty recognized-field identity is a unique relevant arm,
        // even though every Struct shares the same canonical rank. Preserve
        // that member's detailed error verbatim (`{ days = "x" }`). Empty
        // identities are background alternatives, not competitors. Zero or
        // multiple non-empty identities require a Union-level diagnostic.
        let mut sole_non_empty = None;
        let mut non_empty_count = 0;
        for (index, failure) in failures.iter().enumerate() {
            let UnionDiagnosticIdentity::ClosedStruct(fields) = &failure.diagnostic_identity else {
                unreachable!("non-Struct identity rejected above")
            };
            if !fields.is_empty() {
                non_empty_count += 1;
                sole_non_empty = Some(index);
            }
        }
        if non_empty_count == 1 {
            return failures
                .swap_remove(sole_non_empty.expect("one non-empty identity has an index"))
                .error;
        }

        let recognizable_fields = RecognizedStructFields::union(failures.iter().map(|failure| {
            let UnionDiagnosticIdentity::ClosedStruct(fields) = &failure.diagnostic_identity else {
                unreachable!("non-Struct identity rejected above")
            };
            fields
        }));
        let one_alternative_accepts_all_recognizable_fields = failures.iter().any(|failure| {
            let UnionDiagnosticIdentity::ClosedStruct(fields) = &failure.diagnostic_identity else {
                unreachable!("non-Struct identity rejected above")
            };
            fields == &recognizable_fields
        });
        let reason = if !recognizable_fields.is_empty()
            && !one_alternative_accepts_all_recognizable_fields
        {
            UnionStructMismatchReason::ConflictingAlternatives
        } else {
            let unrecognized_fields = map
                .keys()
                .filter(|supplied| !recognizable_fields.contains(supplied))
                .cloned()
                .collect();
            UnionStructMismatchReason::NoAlternativeSatisfied {
                unrecognized_fields,
            }
        };

        TypeError::UnionStructMismatch {
            reason,
            supplied_fields: map.keys().cloned().collect(),
            alternatives,
        }
    }
}

fn describe_struct_union_members(
    members: UnionMembers<'_>,
    ancestry: FieldAncestry,
) -> Option<Vec<UnionStructAlternative>> {
    members
        .iter()
        .map(|member| {
            let member = member?.as_attr();
            let AttrTypeKind::Struct { name, fields } = &member.kind else {
                return None;
            };
            let mut accepted_fields = Vec::with_capacity(fields.len() * 2);
            for field in fields {
                accepted_fields.push(field.name.clone());
                if let Some(block_name) = ancestry.block_name(field)
                    && !accepted_fields.iter().any(|name| name == block_name)
                {
                    accepted_fields.push(block_name.to_string());
                }
            }
            let required_fields = fields
                .iter()
                .filter(|field| ancestry.effective_input_mode(field).is_required())
                .map(|field| field.name.clone())
                .collect();
            Some(UnionStructAlternative {
                name: name.clone(),
                accepted_fields,
                required_fields,
            })
        })
        .collect()
}

/// Pick the shape-closest Union member for strict failure canonicalization.
///
/// This is intentionally separate from fit: it is reachable only after every
/// member returned `None`, and exists so provider-output enum failures retain
/// the same leaf error as a non-Union enum. It uses the established diagnostic
/// rank and declaration-order tie break without turning shape into acceptance.
pub(crate) fn closest_union_member_for_value_on_path<'a>(
    union: &'a AttributeType,
    value: ConcreteValueRef<'_>,
    defs: &'a BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    visited_refs: &mut Vec<&'a str>,
) -> Option<&'a AttributeType> {
    let AttrTypeKind::Union(members) = &union.kind else {
        return None;
    };

    let mut best: Option<(UnionFailureRank, &'a AttributeType)> = None;
    for member in members.as_raw_slice() {
        let rank = union_member_judgement_on_path(member, value, defs, ancestry, visited_refs)
            .failure_rank;
        if rank.is_match() && best.is_none_or(|(current, _)| rank > current) {
            best = Some((rank, member));
        }
    }

    let (_, member) = best?;
    resolve_refs_on_path(member, defs, visited_refs)
        .ok()
        .map(|resolution| resolution.attr)
}

/// Select one Union member from the set that accepts the complete value under
/// the schema-aware validator.
///
/// Accepted candidates are ordered only by semantic exactness and then by
/// declaration order. If every member rejects, the old structural failure
/// rank is used solely to retain the most useful diagnostic tier. A unique
/// best failure is returned verbatim; co-maximal non-Struct failures are
/// retained in declaration order. Both Validation-only callers use this
/// routine. Canonicalization consumes the richer [`Fit`] witness directly, so
/// it never has to select the member again.
pub(crate) fn select_union_member_on_path<'a>(
    members: &'a [AttributeType],
    original: &Value,
    value: ConcreteValueRef<'_>,
    expected: String,
    defs: &'a BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    visited_refs: &mut Vec<&'a str>,
) -> Result<&'a AttributeType, TypeError> {
    if let Some(selected) = fit_union_members_on_path(
        members,
        original,
        defs,
        ancestry,
        CanonicalizationPurpose::Desired(DesiredValueSource::AuthoredDsl),
        visited_refs,
    ) {
        return Ok(selected.member);
    }

    // Only the failure path snapshots the incoming Ref path. Successful
    // selection already carries the winning fit witness and never clones it.
    let resolved_members =
        UnionMembers::new_after_union_on_path(members, defs, visited_refs.clone());
    let mut best_failure: Option<BestUnionMemberFailures> = None;

    for member in members {
        let Err(error) =
            validate_attr_with_defs_on_path(member, original, defs, ancestry, visited_refs)
        else {
            // The authoritative whole-Union fit above proved that no member
            // accepts. Reaching this branch would mean diagnostic validation
            // had drifted from fit acceptance.
            continue;
        };
        let judgement = union_member_judgement_on_path(member, value, defs, ancestry, visited_refs);
        if !judgement.failure_rank.is_match() {
            continue;
        }
        let failure = UnionMemberFailure {
            error,
            diagnostic_identity: judgement.diagnostic_identity,
        };
        match &mut best_failure {
            Some(current) if judgement.failure_rank > current.failure_rank => {
                *current = BestUnionMemberFailures::new(judgement.failure_rank, failure);
            }
            Some(current) if judgement.failure_rank == current.failure_rank => {
                current.tied.push(failure);
            }
            Some(_) => {}
            None => {
                best_failure = Some(BestUnionMemberFailures::new(
                    judgement.failure_rank,
                    failure,
                ));
            }
        }
    }

    Err(best_failure
        .map(|best| best.into_error(value, resolved_members, ancestry))
        .unwrap_or(TypeError::TypeMismatch {
            expected,
            got: value.type_name().to_string(),
        }))
}

/// Resolve one host value-map key to its canonical [`StructField`].
///
/// Host maps use the DSL/schema name or its `block_name` syntax alias.
/// `provider_name` is transport metadata and is deliberately not a host-map
/// key. The reverse walk preserves the former accepted-field map's last-entry
/// behavior if a malformed schema declares colliding aliases.
///
/// Fit, validation, diagnostics, and canonicalization all call this helper so
/// accepted-key semantics cannot drift (#2214, #3877).
pub(crate) fn struct_field_for_key<'a>(
    fields: &'a [StructField],
    key: &str,
    ancestry: FieldAncestry,
) -> Option<&'a StructField> {
    fields
        .iter()
        .rev()
        .find(|field| field.name == key || ancestry.block_name(field) == Some(key))
}

fn struct_field_is_present(
    fields: &[StructField],
    map: &IndexMap<String, Value>,
    expected: &StructField,
    ancestry: FieldAncestry,
) -> bool {
    map.keys().any(|key| {
        struct_field_for_key(fields, key, ancestry)
            .is_some_and(|found| std::ptr::eq(found, expected))
    })
}

/// Reject an assignment that targets provider-populated struct data.
///
/// Both the single-error validator and the path-collecting LSP validator call
/// this gate before type-checking the field value, keeping writability policy
/// and diagnostic precedence identical across the two walkers.
fn validate_struct_field_assignment(
    field: &StructField,
    input_name: &str,
    ancestry: FieldAncestry,
) -> Option<TypeError> {
    ancestry
        .effective_input_mode(field)
        .is_provider_populated()
        .then(|| TypeError::ReadOnlyAttribute {
            name: input_name.to_string(),
        })
}

/// Validate the shared structural contract for a struct map, then route each
/// accepted value through the caller's recursive validator.
///
/// Keeping required-field checks, canonical/`block_name` lookup, unknown-key
/// rejection, and suggestions behind one function prevents the standalone and
/// schema-aware validators from acquiring different notions of a valid struct.
fn validate_struct_fields<M, V>(
    struct_name: &str,
    fields: &[StructField],
    map: &IndexMap<String, Value>,
    ancestry: FieldAncestry,
    missing_required: M,
    mut validate_value: V,
) -> Result<(), TypeError>
where
    M: Fn(&StructField) -> TypeError,
    V: FnMut(&str, &StructField, &Value) -> Result<(), TypeError>,
{
    for field in fields {
        if ancestry.effective_input_mode(field).is_required()
            && !struct_field_is_present(fields, map, field, ancestry)
        {
            return Err(missing_required(field));
        }
    }

    let canonical_names: Vec<&str> = fields.iter().map(|field| field.name.as_str()).collect();
    for (input_name, value) in map {
        let Some(field) = struct_field_for_key(fields, input_name, ancestry) else {
            return Err(TypeError::UnknownStructField {
                struct_name: struct_name.to_string(),
                field: input_name.clone(),
                suggestion: suggest_similar_name(input_name, &canonical_names),
            });
        };
        if let Some(inner) = validate_struct_field_assignment(field, input_name, ancestry) {
            return Err(TypeError::StructFieldError {
                field: input_name.clone(),
                inner: Box::new(inner),
            });
        }
        validate_value(input_name, field, value)?;
    }

    Ok(())
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

fn i64_range_assignable_to_f64(
    source: Option<&(Option<i64>, Option<i64>)>,
    sink: Option<&(Option<f64>, Option<f64>)>,
) -> bool {
    let Some((source_min, source_max)) = source else {
        return sink.is_none();
    };
    let Some((sink_min, sink_max)) = sink else {
        return true;
    };
    let source_min = source_min.map_or(f64::NEG_INFINITY, |value| value as f64);
    let source_max = source_max.map_or(f64::INFINITY, |value| value as f64);
    let sink_min = sink_min.unwrap_or(f64::NEG_INFINITY);
    let sink_max = sink_max.unwrap_or(f64::INFINITY);
    sink_min <= source_min && source_max <= sink_max
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

pub(crate) fn enum_value_matches(input: &str, expected: &str) -> bool {
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

fn join_expected(expected: &[ExpectedEnumVariant]) -> String {
    expected
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

const SECRET_VALUE_MASK: &str = "(secret)";

fn format_string_literal_expected_enum(
    user_typed: &DiagnosticValue,
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
    let user_typed = user_typed.render_string_literal();
    format!(
        "{} expects an enum identifier, got a string literal {}. Use one of: {}",
        target, user_typed, tail
    )
}

/// Render the `InvalidEnumVariant` message with the richest available
/// context. Presence of `attribute` and `type_name` is independent — both,
/// either, or neither may be set. `expected` is rendered as-is; callers are
/// responsible for passing fully-qualified variants for namespaced enums.
fn format_invalid_enum(
    value: &DiagnosticValue,
    value_kind: InvalidEnumValueKind,
    attribute: Option<&str>,
    type_name: Option<&str>,
    expected: &[ExpectedEnumVariant],
) -> String {
    let suggestion = value.as_plain().and_then(|value| {
        let input = value.rsplit('.').next().unwrap_or(value);
        let known = expected
            .iter()
            .map(|candidate| candidate.value.0.as_str())
            .collect::<Vec<_>>();
        let suggested = suggest_similar_name(input, &known)?;
        expected
            .iter()
            .find(|candidate| candidate.value.0 == suggested)
            .map(ToString::to_string)
    });
    let value = value.render_enum_value(value_kind);
    let joined = join_expected(expected);
    let qualifier = match (attribute, type_name) {
        (Some(a), Some(t)) => format!(" for '{}' ({})", a, t),
        (Some(a), None) => format!(" for '{}'", a),
        (None, Some(t)) => format!(" for {}", t),
        (None, None) => String::new(),
    };
    let message = if qualifier.is_empty() {
        format!("Invalid enum variant {value}, expected one of: {joined}")
    } else {
        format!("Invalid value {value}{qualifier}: expected one of {joined}")
    };
    match suggestion {
        Some(suggestion) => format!("{message}; did you mean '{suggestion}'?"),
        None => message,
    }
}

fn format_invalid_value_intro(
    value: &DiagnosticValue,
    attribute: Option<&str>,
    type_name: Option<&str>,
) -> String {
    let value = value.render_string_literal();
    match (attribute, type_name) {
        (Some(a), Some(t)) => format!("Invalid value {value} for '{a}' ({t})"),
        (Some(a), None) => format!("Invalid value {value} for '{a}'"),
        (None, Some(t)) => format!("Invalid {t} value {value}"),
        (None, None) => format!("Invalid value {value}"),
    }
}

fn format_pattern_mismatch(
    value: &DiagnosticValue,
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
    value: &DiagnosticValue,
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
    /// Build the accepted fully-qualified DSL spelling directly from the
    /// structured enum identity. This avoids re-parsing a dotted display name
    /// at the binding/enum disambiguation seam.
    pub fn from_identity(
        identity: &TypeIdentity,
        value: impl Into<String>,
        is_alias: bool,
    ) -> Self {
        Self {
            provider: identity.provider.clone(),
            segments: identity.segments.clone(),
            type_name: identity.kind.clone(),
            value: DslSpelling(value.into()),
            is_alias,
        }
    }

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

/// Lexical declaration kind named by a binding/enum shadowing diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    Let,
    Argument,
}

impl fmt::Display for BindingKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Let => f.write_str("let"),
            Self::Argument => f.write_str("argument"),
        }
    }
}

/// Why a map failed every closed-Struct arm of a Union [`AttributeType`] when
/// either no arm recognized a supplied field or more than one arm did. A sole
/// non-empty recognized-field identity keeps that member's detailed error, so
/// this variant primarily represents empty maps, wholly unknown maps, and maps
/// containing fields from multiple alternatives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnionStructMismatchReason {
    /// No one alternative accepts all recognizable supplied fields.
    ConflictingAlternatives,
    /// No alternative was satisfied without fields spanning alternatives.
    /// `unrecognized_fields` names keys accepted by no declared arm and is
    /// empty whenever every supplied field is accepted by at least one retained
    /// Struct arm.
    NoAlternativeSatisfied { unrecognized_fields: Vec<String> },
}

/// Display metadata for one declared closed-Struct arm carried by
/// [`TypeError::UnionStructMismatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnionStructAlternative {
    /// Declared Struct name.
    pub name: String,
    /// Canonical field names and accepted `block_name` aliases.
    pub accepted_fields: Vec<String>,
    /// Canonical names of fields required by this alternative.
    pub required_fields: Vec<String>,
}

/// CLI/LSP union diagnostics stay within one readable line. The formatter
/// elides whole alternatives or falls back to quoted form names; it never
/// slices a field or type name to meet this limit.
const UNION_STRUCT_DIAGNOSTIC_MAX_CHARS: usize = 160;

struct UnionAlternativeLabel {
    full: String,
    compact: String,
}

fn same_union_field_set(left: &[String], right: &[String]) -> bool {
    left.len() == right.len() && left.iter().all(|field| right.contains(field))
}

fn format_union_list(items: &[String], conjunction: &str) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first} {conjunction} {second}"),
        [head @ .., last] => {
            format!("{}, {conjunction} {last}", head.join(", "))
        }
    }
}

fn format_union_field_set(fields: &[String]) -> String {
    let quoted = fields
        .iter()
        .map(|field| format!("'{field}'"))
        .collect::<Vec<_>>();
    match quoted.as_slice() {
        [] => String::new(),
        [only] => only.clone(),
        _ => format!("({})", format_union_list(&quoted, "and")),
    }
}

fn union_alternative_labels(alternatives: &[UnionStructAlternative]) -> Vec<UnionAlternativeLabel> {
    alternatives
        .iter()
        .map(|alternative| {
            let ambiguous = !alternative.required_fields.is_empty()
                && alternatives
                    .iter()
                    .filter(|other| {
                        same_union_field_set(&alternative.required_fields, &other.required_fields)
                    })
                    .count()
                    > 1;
            let fields = format_union_field_set(&alternative.required_fields);
            let full = if fields.is_empty() {
                format!("empty `{}` form", alternative.name)
            } else if ambiguous {
                format!("{fields} (`{}`)", alternative.name)
            } else {
                fields
            };
            UnionAlternativeLabel {
                full,
                compact: format!("`{}`", alternative.name),
            }
        })
        .collect()
}

fn union_struct_alternatives_are_disjoint(alternatives: &[UnionStructAlternative]) -> bool {
    alternatives.len() > 1
        && alternatives
            .iter()
            .all(|alternative| !alternative.required_fields.is_empty())
        && alternatives.iter().enumerate().all(|(index, alternative)| {
            alternatives.iter().skip(index + 1).all(|other| {
                alternative
                    .required_fields
                    .iter()
                    .all(|required| !other.accepted_fields.contains(required))
                    && other
                        .required_fields
                        .iter()
                        .all(|required| !alternative.accepted_fields.contains(required))
            })
        })
}

fn union_diagnostic_fits(message: &str) -> bool {
    message.chars().count() <= UNION_STRUCT_DIAGNOSTIC_MAX_CHARS
}

fn format_union_choice_message(prefix: &str, choices: &[String], suffix: &str) -> String {
    format!("{prefix}{}{suffix}", format_union_list(choices, "or"))
}

fn omitted_union_choices(count: usize, noun: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} more {noun}{plural}")
}

fn format_bounded_union_choices(
    prefix: &str,
    labels: &[UnionAlternativeLabel],
    suffix: &str,
    omitted_noun: &str,
) -> String {
    let full = labels
        .iter()
        .map(|label| label.full.clone())
        .collect::<Vec<_>>();
    let message = format_union_choice_message(prefix, &full, suffix);
    if union_diagnostic_fits(&message) {
        return message;
    }

    for shown in (1..labels.len()).rev() {
        let mut choices = full[..shown].to_vec();
        choices.push(omitted_union_choices(labels.len() - shown, omitted_noun));
        let message = format_union_choice_message(prefix, &choices, suffix);
        if union_diagnostic_fits(&message) {
            return message;
        }
    }

    let compact = labels
        .iter()
        .map(|label| label.compact.clone())
        .collect::<Vec<_>>();
    let message = format_union_choice_message(prefix, &compact, suffix);
    if union_diagnostic_fits(&message) {
        return message;
    }

    for shown in (1..labels.len()).rev() {
        let mut choices = compact[..shown].to_vec();
        choices.push(omitted_union_choices(labels.len() - shown, omitted_noun));
        let message = format_union_choice_message(prefix, &choices, suffix);
        if union_diagnostic_fits(&message) {
            return message;
        }
    }

    let generic = format!("{prefix}the declared union forms{suffix}");
    if union_diagnostic_fits(&generic) {
        generic
    } else {
        "Value does not match any declared union form".to_string()
    }
}

fn format_union_unknown_prefix(
    unrecognized_fields: &[String],
    alternatives: &[UnionStructAlternative],
    disjoint: bool,
) -> String {
    let expectation = if disjoint {
        "exactly one of "
    } else {
        "one of these forms: "
    };
    if let [field] = unrecognized_fields {
        let mut known_fields = Vec::new();
        for alternative in alternatives {
            for accepted in &alternative.accepted_fields {
                if !known_fields.contains(&accepted.as_str()) {
                    known_fields.push(accepted.as_str());
                }
            }
        }
        if let Some(suggestion) = suggest_similar_name(field, &known_fields) {
            return format!(
                "Unknown field '{field}', did you mean '{suggestion}'? Expected {expectation}"
            );
        }
        return format!("Unknown field '{field}': expected {expectation}");
    }

    let fields = unrecognized_fields
        .iter()
        .map(|field| format!("'{field}'"))
        .collect::<Vec<_>>();
    format!(
        "Unknown fields {}: expected {expectation}",
        format_union_list(&fields, "and")
    )
}

fn format_union_struct_mismatch(
    reason: &UnionStructMismatchReason,
    supplied_fields: &[String],
    alternatives: &[UnionStructAlternative],
) -> String {
    let labels = union_alternative_labels(alternatives);
    let disjoint = union_struct_alternatives_are_disjoint(alternatives);
    if let UnionStructMismatchReason::NoAlternativeSatisfied {
        unrecognized_fields,
    } = reason
        && !unrecognized_fields.is_empty()
    {
        return format_bounded_union_choices(
            &format_union_unknown_prefix(unrecognized_fields, alternatives, disjoint),
            &labels,
            "",
            if disjoint { "alternative" } else { "form" },
        );
    }

    if !disjoint {
        return format_bounded_union_choices("Expected one of these forms: ", &labels, "", "form");
    }

    match reason {
        UnionStructMismatchReason::ConflictingAlternatives => format_bounded_union_choices(
            "Expected exactly one of ",
            &labels,
            if alternatives.len() == 2 {
                ", but both were supplied"
            } else {
                ", but more than one was supplied"
            },
            "alternative",
        ),
        UnionStructMismatchReason::NoAlternativeSatisfied { .. } => {
            let no_required_field_was_supplied = alternatives.iter().all(|alternative| {
                alternative
                    .required_fields
                    .iter()
                    .all(|required| !supplied_fields.iter().any(|supplied| supplied == required))
            });
            format_bounded_union_choices(
                "Expected exactly one of ",
                &labels,
                if no_required_field_was_supplied {
                    ", but none were supplied"
                } else {
                    ", but no alternative was satisfied"
                },
                "alternative",
            )
        }
    }
}

/// Surface kind of a rejected enum value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidEnumValueKind {
    String,
    Identifier,
}

/// User-derived diagnostic text that may be replaced by a secret mask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticValue {
    Plain(String),
    Masked,
    TypeOnlyString,
}

impl DiagnosticValue {
    pub fn as_plain(&self) -> Option<&str> {
        match self {
            Self::Plain(value) => Some(value),
            Self::Masked | Self::TypeOnlyString => None,
        }
    }

    fn render_string_literal(&self) -> String {
        match self {
            Self::Plain(value) => display_string_literal(value),
            Self::Masked => SECRET_VALUE_MASK.to_string(),
            Self::TypeOnlyString => "string".to_string(),
        }
    }

    fn render_enum_value(&self, value_kind: InvalidEnumValueKind) -> String {
        match self {
            Self::Plain(value) => match value_kind {
                InvalidEnumValueKind::String => display_string_literal(value),
                InvalidEnumValueKind::Identifier => value.clone(),
            },
            Self::Masked => SECRET_VALUE_MASK.to_string(),
            Self::TypeOnlyString => "string".to_string(),
        }
    }
}

impl From<String> for DiagnosticValue {
    fn from(value: String) -> Self {
        Self::Plain(value)
    }
}

impl From<&str> for DiagnosticValue {
    fn from(value: &str) -> Self {
        Self::Plain(value.to_string())
    }
}

impl From<ValueString> for DiagnosticValue {
    fn from(value: ValueString) -> Self {
        Self::from(&value)
    }
}

impl From<&ValueString> for DiagnosticValue {
    fn from(value: &ValueString) -> Self {
        match value.diagnostic_literal() {
            Some(value) => Self::Plain(value.to_owned()),
            None => Self::TypeOnlyString,
        }
    }
}

/// Declaration-ordered failures from distinct, equally relevant Union arms.
///
/// The fields are private and crate code constructs this value only after
/// deduplication, so every public instance contains at least two failures.
#[derive(Debug, Clone, PartialEq)]
pub struct UnionArmFailures {
    first: Box<TypeError>,
    second: Box<TypeError>,
    rest: Vec<TypeError>,
}

struct DistinctUnionErrors {
    first: TypeError,
    rest: Vec<TypeError>,
}

impl DistinctUnionErrors {
    fn new(error: TypeError) -> Self {
        match error {
            TypeError::UnionAlternativesMismatch { failures } => {
                let (first, rest) = failures.into_parts();
                let mut distinct = Self::new(first);
                for failure in rest {
                    distinct.push(failure);
                }
                distinct
            }
            first => Self {
                first,
                rest: Vec::new(),
            },
        }
    }

    fn push(&mut self, error: TypeError) {
        let Self { first, rest } = Self::new(error);
        self.push_distinct(first);
        for error in rest {
            self.push_distinct(error);
        }
    }

    fn push_distinct(&mut self, error: TypeError) {
        if error != self.first && !self.rest.contains(&error) {
            self.rest.push(error);
        }
    }

    fn into_parts(self) -> (TypeError, Vec<TypeError>) {
        (self.first, self.rest)
    }
}

impl UnionArmFailures {
    pub(crate) fn new(
        first: TypeError,
        rest: impl IntoIterator<Item = TypeError>,
    ) -> Result<Self, TypeError> {
        let mut distinct = DistinctUnionErrors::new(first);
        for failure in rest {
            distinct.push(failure);
        }

        let (first, rest) = distinct.into_parts();
        let mut failures = rest.into_iter();
        let Some(second) = failures.next() else {
            return Err(first);
        };
        Ok(Self {
            first: Box::new(first),
            second: Box::new(second),
            rest: failures.collect(),
        })
    }

    /// Iterate over the distinct failures in Union declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &TypeError> {
        std::iter::once(self.first.as_ref())
            .chain(std::iter::once(self.second.as_ref()))
            .chain(self.rest.iter())
    }

    fn into_parts(self) -> (TypeError, Vec<TypeError>) {
        let mut rest = Vec::with_capacity(self.rest.len() + 1);
        rest.push(*self.second);
        rest.extend(self.rest);
        (*self.first, rest)
    }
}

fn format_union_alternatives_mismatch(failures: &UnionArmFailures) -> String {
    format!(
        "No union alternative accepts this value: {}",
        failures
            .iter()
            .map(|failure| {
                let rendered = failure.to_string();
                if contains_union_alternatives_mismatch(failure) {
                    format!("({rendered})")
                } else {
                    rendered
                }
            })
            .collect::<Vec<_>>()
            .join("; or ")
    )
}

fn contains_union_alternatives_mismatch(error: &TypeError) -> bool {
    match error {
        TypeError::UnionAlternativesMismatch { .. } => true,
        TypeError::ListItemError { inner, .. }
        | TypeError::MapKeyError { inner, .. }
        | TypeError::MapValueError { inner, .. }
        | TypeError::StructFieldError { inner, .. } => contains_union_alternatives_mismatch(inner),
        TypeError::TypeMismatch { .. }
        | TypeError::BindingShadowsEnumMember(_)
        | TypeError::InvalidEnumVariant { .. }
        | TypeError::PatternMismatch { .. }
        | TypeError::LengthOutOfRange { .. }
        | TypeError::StringLiteralExpectedEnum { .. }
        | TypeError::ValidationFailed { .. }
        | TypeError::CustomTypeValidation { .. }
        | TypeError::ResourceValidationFailed { .. }
        | TypeError::MissingRequired { .. }
        | TypeError::ReadOnlyAttribute { .. }
        | TypeError::UnknownAttribute { .. }
        | TypeError::UnknownStructField { .. }
        | TypeError::UnionStructMismatch { .. }
        | TypeError::BlockSyntaxNotAllowed { .. } => false,
    }
}

/// A lexical binding that shadows an enum member with the same bare spelling.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error(
        "`{name}` was interpreted as the `{binding_kind}` binding; if you meant the enum `{}` value, write `{qualified}`",
        r#enum.kind
    )]
pub struct BindingShadowsEnumMemberError {
    pub name: String,
    pub binding_kind: BindingKind,
    pub r#enum: TypeIdentity,
    pub qualified: ExpectedEnumVariant,
}

/// Type error
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TypeError {
    #[error("Type mismatch: expected {expected}, got {got}")]
    TypeMismatch { expected: String, got: String },

    #[error(transparent)]
    BindingShadowsEnumMember(Box<BindingShadowsEnumMemberError>),

    #[error(
        "{}",
        format_invalid_enum(value, *value_kind, attribute.as_deref(), type_name.as_deref(), expected)
    )]
    InvalidEnumVariant {
        value: DiagnosticValue,
        value_kind: InvalidEnumValueKind,
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
        value: DiagnosticValue,
        pattern: String,
        attribute: Option<String>,
        type_name: Option<String>,
    },

    #[error(
        "{}",
        format_length_out_of_range(value, *length, *min, *max, attribute.as_deref(), type_name.as_deref())
    )]
    LengthOutOfRange {
        value: DiagnosticValue,
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
        /// The string the user typed between the quotes, or a secret mask.
        user_typed: DiagnosticValue,
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

    #[error("{error}")]
    CustomTypeValidation {
        error: Box<crate::parser::CustomTypeValidationError>,
        attribute: Option<String>,
    },

    #[error("Resource validation failed: {message}")]
    ResourceValidationFailed {
        message: String,
        /// Optional attribute name for precise diagnostic positioning.
        attribute: Option<String>,
    },

    #[error("Required attribute '{name}' is missing")]
    MissingRequired { name: String },

    #[error("Attribute '{name}' is populated by the provider and cannot be set")]
    ReadOnlyAttribute { name: String },

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

    #[error(
        "{}",
        format_union_struct_mismatch(reason, supplied_fields, alternatives)
    )]
    UnionStructMismatch {
        /// Classification consumed by the concise renderer.
        reason: UnionStructMismatchReason,
        /// Map keys in user-supplied order.
        supplied_fields: Vec<String>,
        /// Display metadata for every declared closed-Struct arm.
        alternatives: Vec<UnionStructAlternative>,
    },

    #[error("{}", format_union_alternatives_mismatch(failures))]
    UnionAlternativesMismatch {
        /// Every distinct tied arm failure, in declaration order.
        failures: UnionArmFailures,
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

impl From<crate::parser::CustomTypeValidationError> for TypeError {
    fn from(error: crate::parser::CustomTypeValidationError) -> Self {
        Self::CustomTypeValidation {
            error: Box::new(error),
            attribute: None,
        }
    }
}

impl TypeError {
    fn from_union_member_failures(
        first: UnionMemberFailure,
        rest: impl IntoIterator<Item = UnionMemberFailure>,
    ) -> Self {
        Self::from_union_arm_errors(first.error, rest.into_iter().map(|failure| failure.error))
    }

    fn from_union_arm_errors(first: TypeError, rest: impl IntoIterator<Item = TypeError>) -> Self {
        match UnionArmFailures::new(first, rest) {
            Ok(failures) => Self::UnionAlternativesMismatch { failures },
            Err(error) => error,
        }
    }

    /// Return the authored top-level attribute carried by this diagnostic.
    /// Container and union wrappers delegate to their leaf failures so
    /// consumers do not need variant-specific position recovery.
    pub fn attribute_name(&self) -> Option<&str> {
        match self {
            TypeError::InvalidEnumVariant { attribute, .. }
            | TypeError::PatternMismatch { attribute, .. }
            | TypeError::LengthOutOfRange { attribute, .. }
            | TypeError::StringLiteralExpectedEnum { attribute, .. }
            | TypeError::ResourceValidationFailed { attribute, .. } => attribute.as_deref(),
            TypeError::ReadOnlyAttribute { name }
            | TypeError::UnknownAttribute { name, .. }
            | TypeError::BlockSyntaxNotAllowed { attribute: name } => Some(name),
            TypeError::ListItemError { inner, .. }
            | TypeError::MapKeyError { inner, .. }
            | TypeError::MapValueError { inner, .. }
            | TypeError::StructFieldError { inner, .. } => inner.attribute_name(),
            TypeError::UnionAlternativesMismatch { failures } => {
                let mut attributes = failures.iter().filter_map(TypeError::attribute_name);
                let first = attributes.next()?;
                attributes
                    .all(|attribute| attribute == first)
                    .then_some(first)
            }
            TypeError::TypeMismatch { .. }
            | TypeError::BindingShadowsEnumMember(_)
            | TypeError::ValidationFailed { .. }
            | TypeError::MissingRequired { .. }
            | TypeError::UnknownStructField { .. }
            | TypeError::UnionStructMismatch { .. } => None,
            TypeError::CustomTypeValidation { attribute, .. } => attribute.as_deref(),
        }
    }

    /// Remove user-authored data from an error produced while validating the
    /// contents of a [`DeferredValue::Secret`]. Structured schema context is
    /// retained, while free-form validator messages are replaced wholesale
    /// because providers and custom validators may echo the plaintext.
    #[must_use]
    pub(crate) fn masked_for_secret(self) -> Self {
        const MASKED_MESSAGE: &str = "(secret) failed validation";

        match self {
            TypeError::InvalidEnumVariant {
                value_kind,
                attribute,
                type_name,
                expected,
                ..
            } => TypeError::InvalidEnumVariant {
                value: DiagnosticValue::Masked,
                value_kind,
                attribute,
                type_name,
                expected,
            },
            TypeError::PatternMismatch {
                pattern,
                attribute,
                type_name,
                ..
            } => TypeError::PatternMismatch {
                value: DiagnosticValue::Masked,
                pattern,
                attribute,
                type_name,
            },
            TypeError::LengthOutOfRange {
                length,
                min,
                max,
                attribute,
                type_name,
                ..
            } => TypeError::LengthOutOfRange {
                value: DiagnosticValue::Masked,
                length,
                min,
                max,
                attribute,
                type_name,
            },
            TypeError::StringLiteralExpectedEnum {
                attribute,
                type_name,
                expected,
                extra_message,
                ..
            } => TypeError::StringLiteralExpectedEnum {
                user_typed: DiagnosticValue::Masked,
                attribute,
                type_name,
                expected,
                extra_message: extra_message.map(|_| MASKED_MESSAGE.to_string()),
            },
            TypeError::ValidationFailed { .. } => TypeError::ValidationFailed {
                message: MASKED_MESSAGE.to_string(),
            },
            TypeError::CustomTypeValidation { error, attribute } => {
                TypeError::CustomTypeValidation {
                    error: Box::new((*error).masked_for_secret()),
                    attribute,
                }
            }
            TypeError::ResourceValidationFailed { attribute, .. } => {
                TypeError::ResourceValidationFailed {
                    message: MASKED_MESSAGE.to_string(),
                    attribute,
                }
            }
            TypeError::UnknownAttribute { .. } => TypeError::UnknownAttribute {
                name: SECRET_VALUE_MASK.to_string(),
                suggestion: None,
            },
            TypeError::UnknownStructField { struct_name, .. } => TypeError::UnknownStructField {
                struct_name,
                field: SECRET_VALUE_MASK.to_string(),
                suggestion: None,
            },
            TypeError::UnionStructMismatch {
                reason,
                alternatives,
                ..
            } => TypeError::UnionStructMismatch {
                reason: match reason {
                    UnionStructMismatchReason::ConflictingAlternatives => {
                        UnionStructMismatchReason::ConflictingAlternatives
                    }
                    UnionStructMismatchReason::NoAlternativeSatisfied { .. } => {
                        UnionStructMismatchReason::NoAlternativeSatisfied {
                            unrecognized_fields: vec![SECRET_VALUE_MASK.to_string()],
                        }
                    }
                },
                supplied_fields: Vec::new(),
                alternatives,
            },
            TypeError::UnionAlternativesMismatch { failures } => {
                let (first, rest) = failures.into_parts();
                TypeError::from_union_arm_errors(
                    first.masked_for_secret(),
                    rest.into_iter().map(TypeError::masked_for_secret),
                )
            }
            TypeError::ListItemError { index, inner } => TypeError::ListItemError {
                index,
                inner: Box::new(inner.masked_for_secret()),
            },
            TypeError::MapKeyError { inner, .. } => TypeError::MapKeyError {
                key: SECRET_VALUE_MASK.to_string(),
                inner: Box::new(inner.masked_for_secret()),
            },
            TypeError::MapValueError { inner, .. } => TypeError::MapValueError {
                key: SECRET_VALUE_MASK.to_string(),
                inner: Box::new(inner.masked_for_secret()),
            },
            TypeError::StructFieldError { inner, .. } => TypeError::StructFieldError {
                field: SECRET_VALUE_MASK.to_string(),
                inner: Box::new(inner.masked_for_secret()),
            },
            TypeError::TypeMismatch { .. }
            | TypeError::BindingShadowsEnumMember(_)
            | TypeError::MissingRequired { .. }
            | TypeError::ReadOnlyAttribute { .. }
            | TypeError::BlockSyntaxNotAllowed { .. } => self,
        }
    }

    /// Attach an attribute name to errors whose diagnostics include an
    /// attribute context; other variants return `self` unchanged.
    ///
    /// Callers that know which attribute produced the error (e.g. the
    /// attribute loop in [`ValidatedResourceSchema::validate`]) wrap the primitive
    /// error before it reaches CLI/LSP diagnostic text. This keeps
    /// leaf validator unaware of attribute names while still
    /// letting the final message say `for 'target_id'`.
    ///
    #[must_use]
    pub fn with_attribute(self, attribute: impl Into<String>) -> Self {
        let attribute = attribute.into();
        match self {
            TypeError::UnionAlternativesMismatch { failures } => {
                let (first, rest) = failures.into_parts();
                TypeError::from_union_arm_errors(
                    first.with_attribute(attribute.clone()),
                    rest.into_iter()
                        .map(|failure| failure.with_attribute(attribute.clone())),
                )
            }
            TypeError::ValidationFailed { message } => TypeError::ResourceValidationFailed {
                message,
                attribute: Some(attribute),
            },
            TypeError::ResourceValidationFailed {
                message,
                attribute: existing,
            } => TypeError::ResourceValidationFailed {
                message,
                attribute: existing.or(Some(attribute)),
            },
            TypeError::CustomTypeValidation {
                error,
                attribute: existing,
            } => TypeError::CustomTypeValidation {
                error,
                attribute: existing.or(Some(attribute)),
            },
            TypeError::ListItemError { index, inner } => TypeError::ListItemError {
                index,
                inner: Box::new(inner.with_attribute(attribute)),
            },
            TypeError::MapKeyError { key, inner } => TypeError::MapKeyError {
                key,
                inner: Box::new(inner.with_attribute(attribute)),
            },
            TypeError::MapValueError { key, inner } => TypeError::MapValueError {
                key,
                inner: Box::new(inner.with_attribute(attribute)),
            },
            TypeError::StructFieldError { field, inner } => TypeError::StructFieldError {
                field,
                inner: Box::new(inner.with_attribute(attribute)),
            },
            mut other => {
                match &mut other {
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
                        *attr_slot = Some(attribute);
                    }
                    _ => {}
                }
                other
            }
        }
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
                value_kind: InvalidEnumValueKind::String,
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

    /// Host-side lifecycle marker retained from provider validation.
    pub fn host_cause(&self) -> Option<crate::provider::HostErrorCause> {
        match self {
            TypeError::CustomTypeValidation { error, .. } => error
                .provider_error()
                .and_then(|error| error.detail().host_cause()),
            TypeError::ListItemError { inner, .. }
            | TypeError::MapKeyError { inner, .. }
            | TypeError::MapValueError { inner, .. }
            | TypeError::StructFieldError { inner, .. } => inner.host_cause(),
            TypeError::UnionAlternativesMismatch { failures } => {
                failures.iter().find_map(TypeError::host_cause)
            }
            _ => None,
        }
    }

    /// Provider name retained from provider-backed validation.
    pub fn provider_name(&self) -> Option<&str> {
        match self {
            TypeError::CustomTypeValidation { error, .. } => error
                .provider_error()
                .and_then(|error| error.detail().provider_name.as_deref()),
            TypeError::ListItemError { inner, .. }
            | TypeError::MapKeyError { inner, .. }
            | TypeError::MapValueError { inner, .. }
            | TypeError::StructFieldError { inner, .. } => inner.provider_name(),
            TypeError::UnionAlternativesMismatch { failures } => {
                failures.iter().find_map(TypeError::provider_name)
            }
            _ => None,
        }
    }
}

impl Value {
    pub fn type_name(&self) -> String {
        match self {
            Value::Concrete(ConcreteValue::String(_)) => "String".to_string(),
            Value::Concrete(ConcreteValue::EnumIdentifier(_)) => "EnumIdentifier".to_string(),
            Value::Concrete(ConcreteValue::CanonicalEnum(_)) => "CanonicalEnum".to_string(),
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
            Value::Deferred(DeferredValue::Coalesce { .. }) => "Coalesce".to_string(),
            Value::Deferred(DeferredValue::Interpolation(_)) => "Interpolation".to_string(),
            Value::Deferred(
                DeferredValue::FunctionCall { name, .. } | DeferredValue::Unavailable { name, .. },
            ) => {
                format!("FunctionCall({name})")
            }
            Value::Deferred(DeferredValue::ResolvedFunctionCall { function, .. }) => {
                format!("FunctionCall({})", function.name)
            }
            Value::Deferred(DeferredValue::Access(_)) => "Access".to_string(),
            Value::Deferred(DeferredValue::Secret(_)) => "Secret".to_string(),
            Value::Deferred(DeferredValue::RedactedSecret { .. }) => "RedactedSecret".to_string(),
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
            ConcreteValueRef::CanonicalEnum(_) => "CanonicalEnum",
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
            ConcreteValueRef::String(s) => Value::Concrete(ConcreteValue::String(s.clone())),
            ConcreteValueRef::EnumIdentifier(s) => {
                Value::Concrete(ConcreteValue::enum_identifier(s.to_string()))
            }
            ConcreteValueRef::CanonicalEnum(c) => {
                Value::Concrete(ConcreteValue::CanonicalEnum(c.clone()))
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

/// Schema for one top-level resource attribute.
///
/// Construction and mutation go through builders so conflicting flags and
/// input modes cannot be assembled by assigning fields directly.
///
/// ```compile_fail
/// use carina_core::schema::{AttributeSchema, AttributeType};
///
/// let _ = AttributeSchema {
///     write_only: true,
///     ..AttributeSchema::optional("arn", AttributeType::string()).build()
/// };
/// ```
///
/// ```compile_fail
/// use carina_core::schema::{AttributeSchema, AttributeType};
///
/// let mut attribute = AttributeSchema::optional("arn", AttributeType::string()).build();
/// attribute.write_only = true;
/// ```
///
/// The read-only view remains exhaustively destructurable for conversion
/// seams:
///
/// ```
/// use carina_core::schema::{AttributeSchema, AttributeSchemaParts, AttributeType};
///
/// let attribute = AttributeSchema::optional("arn", AttributeType::string()).build();
/// let AttributeSchemaParts {
///     name,
///     attr_type,
///     declared_input_mode,
///     default,
///     description,
///     completions,
///     provider_name,
///     create_only,
///     removable,
///     block_name,
///     write_only,
///     identity,
///     deferred_populate,
/// } = attribute.parts();
/// let _ = (
///     name,
///     attr_type,
///     declared_input_mode,
///     default,
///     description,
///     completions,
///     provider_name,
///     create_only,
///     removable,
///     block_name,
///     write_only,
///     identity,
///     deferred_populate,
/// );
/// ```
#[derive(Debug, Clone)]
pub struct AttributeSchema {
    pub(crate) name: String,
    pub(crate) attr_type: AttributeType,
    /// Whether user input for this attribute is optional, required, or forbidden.
    pub(crate) input_mode: InputMode,
    pub(crate) default: Option<Value>,
    pub(crate) description: Option<String>,
    /// Completion values for this attribute (used by LSP)
    pub(crate) completions: Option<Vec<CompletionValue>>,
    /// Provider-side property name (e.g., "VpcId" for AWS Cloud Control)
    pub(crate) provider_name: Option<String>,
    /// Whether this attribute is create-only (immutable after creation)
    pub(crate) create_only: bool,
    /// Override for removability detection.
    /// `None` = auto-detect: removable when optional, mutable, and writable.
    /// `Some(false)` = explicitly non-removable (e.g., region inherited from provider).
    /// Only removable attributes trigger removal detection in the differ.
    pub(crate) removable: Option<bool>,
    /// Alternative block name for repeated block syntax (e.g., "operating_region" for "operating_regions")
    pub(crate) block_name: Option<String>,
    /// Whether this attribute is write-only (not returned by the provider's read API).
    /// Write-only attributes are sent to the provider during create/update but may not
    /// appear in read responses. This is NOT related to sensitive/secret values — it
    /// indicates a CloudFormation `writeOnlyProperties` attribute.
    pub(crate) write_only: bool,
    /// Whether this attribute contributes to anonymous resource identity.
    /// Identity attributes are included in the hash when computing anonymous resource
    /// identifiers, alongside create-only attributes. Use this for attributes that
    /// distinguish resources of the same type that share the same create-only values
    /// (e.g., Route 53 RecordSet `type` differentiates A vs AAAA records with the
    /// same name and hosted zone).
    pub(crate) identity: bool,
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
    pub(crate) deferred_populate: bool,
}

/// Exhaustive read-only view of an [`AttributeSchema`].
///
/// This deliberately has no `#[non_exhaustive]`: conversion seams should
/// destructure every field so additions become compile errors.
#[derive(Debug, Clone, Copy)]
pub struct AttributeSchemaParts<'a> {
    pub name: &'a str,
    pub attr_type: &'a AttributeType,
    /// The declared top-level mode. Top-level attributes have no schema
    /// ancestor, so this is also their effective mode.
    pub declared_input_mode: InputMode,
    pub default: Option<&'a Value>,
    pub description: Option<&'a str>,
    pub completions: Option<&'a [CompletionValue]>,
    pub provider_name: Option<&'a str>,
    pub create_only: bool,
    pub removable: Option<bool>,
    pub block_name: Option<&'a str>,
    pub write_only: bool,
    pub identity: bool,
    pub deferred_populate: bool,
}

/// Builder for an optional user-input attribute.
pub struct OptionalAttributeSchemaBuilder {
    attribute: AttributeSchema,
}

/// Builder for a required user-input attribute.
pub struct RequiredAttributeSchemaBuilder {
    attribute: AttributeSchema,
}

/// Builder for a provider-populated attribute.
pub struct ProviderPopulatedAttributeSchemaBuilder {
    attribute: AttributeSchema,
}

impl AttributeSchema {
    fn with_input_mode(
        name: impl Into<String>,
        attr_type: AttributeType,
        input_mode: InputMode,
    ) -> Self {
        Self {
            name: name.into(),
            attr_type,
            input_mode,
            default: None,
            description: None,
            completions: None,
            provider_name: None,
            create_only: false,
            removable: None,
            block_name: None,
            write_only: false,
            identity: false,
            deferred_populate: false,
        }
    }

    pub fn optional(
        name: impl Into<String>,
        attr_type: AttributeType,
    ) -> OptionalAttributeSchemaBuilder {
        OptionalAttributeSchemaBuilder {
            attribute: Self::with_input_mode(name, attr_type, InputMode::Optional),
        }
    }

    pub fn required(
        name: impl Into<String>,
        attr_type: AttributeType,
    ) -> RequiredAttributeSchemaBuilder {
        RequiredAttributeSchemaBuilder {
            attribute: Self::with_input_mode(name, attr_type, InputMode::Required),
        }
    }

    pub fn provider_populated(
        name: impl Into<String>,
        attr_type: AttributeType,
    ) -> ProviderPopulatedAttributeSchemaBuilder {
        ProviderPopulatedAttributeSchemaBuilder {
            attribute: Self::with_input_mode(name, attr_type, InputMode::ProviderPopulated),
        }
    }

    pub fn parts(&self) -> AttributeSchemaParts<'_> {
        AttributeSchemaParts {
            name: &self.name,
            attr_type: &self.attr_type,
            declared_input_mode: self.input_mode,
            default: self.default.as_ref(),
            description: self.description.as_deref(),
            completions: self.completions.as_deref(),
            provider_name: self.provider_name.as_deref(),
            create_only: self.create_only,
            removable: self.removable,
            block_name: self.block_name.as_deref(),
            write_only: self.write_only,
            identity: self.identity,
            deferred_populate: self.deferred_populate,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn attr_type(&self) -> &AttributeType {
        &self.attr_type
    }

    pub fn input_mode(&self) -> InputMode {
        self.input_mode
    }

    pub fn default_value(&self) -> Option<&Value> {
        self.default.as_ref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn completions(&self) -> Option<&[CompletionValue]> {
        self.completions.as_deref()
    }

    pub fn provider_name(&self) -> Option<&str> {
        self.provider_name.as_deref()
    }

    pub fn is_create_only(&self) -> bool {
        self.create_only
    }

    pub fn removability_override(&self) -> Option<bool> {
        self.removable
    }

    pub fn block_name(&self) -> Option<&str> {
        self.block_name.as_deref()
    }

    pub fn is_write_only(&self) -> bool {
        self.write_only
    }

    pub fn is_identity(&self) -> bool {
        self.identity
    }

    pub fn is_deferred_populate(&self) -> bool {
        self.deferred_populate
    }

    pub fn is_required(&self) -> bool {
        self.input_mode.is_required()
    }

    pub fn is_read_only(&self) -> bool {
        self.input_mode.is_provider_populated()
    }

    /// Whether this attribute can be removed from infrastructure.
    /// Auto-detected: optional (not required), mutable (not create-only), and writable
    /// (not read-only) attributes are removable by default. Can be overridden with
    /// `.removable()` or `.non_removable()`.
    pub fn is_removable(&self) -> bool {
        self.removable
            .unwrap_or(!self.is_required() && !self.create_only && !self.is_read_only())
    }
}

macro_rules! impl_user_input_attribute_builder {
    ($builder:ident) => {
        impl $builder {
            pub fn write_only(mut self) -> Self {
                self.attribute.write_only = true;
                self
            }

            pub fn create_only(mut self) -> Self {
                self.attribute.create_only = true;
                self
            }

            pub fn identity(mut self) -> Self {
                self.attribute.identity = true;
                self
            }

            pub fn with_description(mut self, description: impl Into<String>) -> Self {
                self.attribute.description = Some(description.into());
                self
            }

            pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
                self.attribute.provider_name = Some(name.into());
                self
            }

            pub fn with_block_name(mut self, name: impl Into<String>) -> Self {
                self.attribute.block_name = Some(name.into());
                self
            }

            pub fn with_completions(mut self, completions: Vec<CompletionValue>) -> Self {
                self.attribute.completions = Some(completions);
                self
            }

            pub fn build(self) -> AttributeSchema {
                self.attribute
            }
        }
    };
}

impl_user_input_attribute_builder!(OptionalAttributeSchemaBuilder);
impl_user_input_attribute_builder!(RequiredAttributeSchemaBuilder);

impl OptionalAttributeSchemaBuilder {
    pub fn with_default(mut self, value: Value) -> Self {
        self.attribute.default = Some(value);
        self
    }

    pub fn removable(mut self) -> Self {
        self.attribute.removable = Some(true);
        self
    }

    pub fn non_removable(mut self) -> Self {
        self.attribute.removable = Some(false);
        self
    }
}

impl ProviderPopulatedAttributeSchemaBuilder {
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.attribute.description = Some(description.into());
        self
    }

    pub fn with_provider_name(mut self, name: impl Into<String>) -> Self {
        self.attribute.provider_name = Some(name.into());
        self
    }

    pub fn deferred_populate(mut self) -> Self {
        self.attribute.deferred_populate = true;
        self
    }

    pub fn build(self) -> AttributeSchema {
        self.attribute
    }
}

/// Classification of a resource schema: managed (full CRUD lifecycle) vs
/// data source (read-only lookup of existing infrastructure).
///
/// See `notes/specs/2026-05-02-resource-vs-data-source-design.md` (Decision 1-1).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum SchemaKind {
    Resource,
    DataSource,
}

/// Failure to resolve a dotted attribute path through one resource schema.
///
/// The first segment names a top-level attribute. Every following segment
/// names either a field on a `Struct` or a key in a `Map`. List indexing is
/// unsupported because dotted wait predicates do not carry an index syntax.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AttributePathTypeError {
    #[error("attribute path cannot be empty")]
    Empty,
    #[error("unknown top-level attribute `{attribute}`")]
    UnknownAttribute { attribute: String },
    #[error("unknown field `{field}` on struct `{struct_name}` after `{parent_path}`")]
    UnknownStructField {
        parent_path: String,
        field: String,
        struct_name: String,
    },
    #[error(
        "cannot traverse `{path}` because it has type {type_name}; only struct fields and map values can be traversed"
    )]
    CannotTraverse { path: String, type_name: String },
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
) -> Option<UnionMembers<'a>> {
    let mut visited_refs = Vec::new();
    match attr_type
        .resolve_refs_with_defs_on_path(defs, &mut visited_refs)
        .as_attr()
        .kind()
    {
        AttrTypeKind::Union(members) => Some(UnionMembers::new_after_union_on_path(
            members.as_raw_slice(),
            defs,
            visited_refs,
        )),
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

/// How a managed resource handles create-before-destroy name conflicts.
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

/// Resource schema.
///
/// Construction and mutation go through chained builders. The read-only view
/// remains exhaustively destructurable for conversion seams:
///
/// ```
/// use carina_core::schema::{ResourceSchema, ResourceSchemaParts};
///
/// let schema = ResourceSchema::new("test.Widget");
/// let ResourceSchemaParts {
///     resource_type,
///     attributes,
///     description,
///     validation,
///     kind,
///     unique_name,
///     operation_config,
///     exclusive_required,
///     default_wait_timeout,
///     default_wait_interval,
///     defs,
/// } = schema.parts();
/// let _ = (
///     resource_type,
///     attributes,
///     description,
///     validation,
///     kind,
///     unique_name,
///     operation_config,
///     exclusive_required,
///     default_wait_timeout,
///     default_wait_interval,
///     defs,
/// );
/// ```
#[derive(Debug, Clone)]
pub struct ResourceSchema {
    pub(crate) resource_type: String,
    pub(crate) attributes: BTreeMap<String, AttributeSchema>,
    duplicate_attributes: Vec<String>,
    pub(crate) description: Option<String>,
    /// Cross-attribute validation, including its fingerprintable source.
    pub(crate) validation: ResourceValidation,
    /// Whether this is a managed resource or a data source.
    /// Data sources must be used with the `read` keyword.
    pub(crate) kind: SchemaKind,
    /// How this resource handles create-before-destroy name conflicts.
    pub(crate) unique_name: UniqueNameSpec,
    /// Per-resource operational config (timeouts, retries).
    /// When None, provider defaults are used.
    pub(crate) operation_config: Option<OperationConfig>,
    /// Declarative "exactly one of" groups. Each inner vec is a group of
    /// attribute names where exactly one must be specified. Unlike an
    /// in-process validator, this is plain data and survives the WASM plugin
    /// boundary.
    pub(crate) exclusive_required: Vec<Vec<String>>,
    /// Default total timeout for `wait <target> { ... }` polling against
    /// this resource type. `None` falls back to
    /// [`WAIT_DEFAULT_TIMEOUT`].
    pub(crate) default_wait_timeout: Option<std::time::Duration>,
    /// Default poll cadence between `read()` calls for `wait <target>`
    /// against this resource type. `None` falls back to
    /// [`WAIT_DEFAULT_INTERVAL`].
    pub(crate) default_wait_interval: Option<std::time::Duration>,
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
    pub(crate) defs: std::collections::BTreeMap<String, AttributeType>,
}

/// Exhaustive read-only view of a [`ResourceSchema`].
///
/// This deliberately has no `#[non_exhaustive]`: conversion seams should
/// destructure every field so additions become compile errors.
#[derive(Debug, Clone, Copy)]
pub struct ResourceSchemaParts<'a> {
    pub resource_type: &'a str,
    pub attributes: &'a BTreeMap<String, AttributeSchema>,
    pub description: Option<&'a str>,
    pub validation: &'a ResourceValidation,
    pub kind: SchemaKind,
    pub unique_name: &'a UniqueNameSpec,
    pub operation_config: Option<&'a OperationConfig>,
    pub exclusive_required: &'a [Vec<String>],
    pub default_wait_timeout: Option<std::time::Duration>,
    pub default_wait_interval: Option<std::time::Duration>,
    pub defs: &'a std::collections::BTreeMap<String, AttributeType>,
}

/// Source of a resource's executable cross-attribute validation.
#[derive(Debug, Clone, Default)]
pub enum ResourceValidation {
    #[default]
    None,
    /// Portable declarations received from a WASM provider by the plugin host.
    Declared(Vec<ValidatorType>),
}

impl ResourceValidation {
    /// Return the executable validator represented by this source, if any.
    fn validator(&self) -> Option<ResourceValidator> {
        match self {
            Self::None => None,
            Self::Declared(types) => build_validator_from_types(types),
        }
    }
}

/// Structured location of one whole-schema validation issue.
///
/// Attribute paths are kept distinct from schema-level declarations so wire
/// decoders and other adapters cannot accidentally present
/// `exclusive_required[0]` or `defs.Tag.key` as a resource attribute.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SchemaValidationIssueLocation {
    Attribute(String),
    Definition(String),
    ExclusiveRequired(usize),
    UniqueName,
}

impl SchemaValidationIssueLocation {
    fn with_suffix(&self, suffix: impl fmt::Display) -> Self {
        match self {
            Self::Attribute(path) => Self::Attribute(format!("{path}{suffix}")),
            Self::Definition(path) => Self::Definition(format!("{path}{suffix}")),
            Self::ExclusiveRequired(_) | Self::UniqueName => {
                unreachable!("schema-level issue locations cannot have type-path descendants")
            }
        }
    }

    pub fn attribute_path(&self) -> Option<&str> {
        match self {
            Self::Attribute(path) => Some(path),
            Self::Definition(_) | Self::ExclusiveRequired(_) | Self::UniqueName => None,
        }
    }
}

impl fmt::Display for SchemaValidationIssueLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Attribute(path) => f.write_str(path),
            Self::Definition(path) => write!(f, "defs.{path}"),
            Self::ExclusiveRequired(index) => write!(f, "exclusive_required[{index}]"),
            Self::UniqueName => f.write_str("unique_name"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SchemaDefinitionIssue {
    location: SchemaValidationIssueLocation,
    path: String,
    message: String,
}

impl fmt::Display for SchemaDefinitionIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

/// Error returned when a resource-schema draft fails whole-schema validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceSchemaValidationError {
    resource_type: String,
    issues: Vec<SchemaDefinitionIssue>,
}

impl ResourceSchemaValidationError {
    /// Return the stable schema path and message for every rejected rule.
    pub fn issues(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.issues
            .iter()
            .map(|issue| (issue.path.as_str(), issue.message.as_str()))
    }

    /// Return each issue's typed location and message.
    pub fn located_issues(
        &self,
    ) -> impl ExactSizeIterator<Item = (&SchemaValidationIssueLocation, &str)> {
        self.issues
            .iter()
            .map(|issue| (&issue.location, issue.message.as_str()))
    }
}

impl fmt::Display for ResourceSchemaValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "resource schema `{}` is invalid: ", self.resource_type)?;
        for (index, issue) in self.issues.iter().enumerate() {
            if index != 0 {
                f.write_str("; ")?;
            }
            issue.fmt(f)?;
        }
        Ok(())
    }
}

impl std::error::Error for ResourceSchemaValidationError {}

/// Error returned when provider-config attribute types fail graph validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSchemaValidationError {
    issues: Vec<SchemaDefinitionIssue>,
}

impl ConfigSchemaValidationError {
    pub fn issues(&self) -> impl ExactSizeIterator<Item = (&str, &str)> {
        self.issues
            .iter()
            .map(|issue| (issue.path.as_str(), issue.message.as_str()))
    }

    /// Return each issue's typed location and message.
    pub fn located_issues(
        &self,
    ) -> impl ExactSizeIterator<Item = (&SchemaValidationIssueLocation, &str)> {
        self.issues
            .iter()
            .map(|issue| (&issue.location, issue.message.as_str()))
    }
}

impl fmt::Display for ConfigSchemaValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("provider config schema is invalid: ")?;
        for (index, issue) in self.issues.iter().enumerate() {
            if index != 0 {
                f.write_str("; ")?;
            }
            issue.fmt(f)?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigSchemaValidationError {}

/// A top-level resource attribute read through a validated schema witness.
///
/// Top-level attributes have no schema ancestor, so their declared and
/// effective modes are identical. Descending through [`Self::value_type`]
/// carries that effective mode into every nested [`EffectiveField`].
#[derive(Debug, Clone)]
pub struct EffectiveAttribute<'a> {
    attribute: &'a AttributeSchema,
    defs: &'a BTreeMap<String, AttributeType>,
    path: Vec<String>,
}

impl<'a> EffectiveAttribute<'a> {
    pub fn name(&self) -> &'a str {
        &self.attribute.name
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub fn field_type(&self) -> &'a AttributeType {
        &self.attribute.attr_type
    }

    pub fn description(&self) -> Option<&'a str> {
        self.attribute.description.as_deref()
    }

    pub fn provider_name(&self) -> Option<&'a str> {
        self.attribute.provider_name.as_deref()
    }

    pub fn block_name(&self) -> Option<&'a str> {
        self.attribute.block_name.as_deref()
    }

    pub fn effective_input_mode(&self) -> InputMode {
        self.attribute.input_mode
    }

    pub fn is_required(&self) -> bool {
        self.effective_input_mode().is_required()
    }

    pub fn is_read_only(&self) -> bool {
        self.effective_input_mode().is_provider_populated()
    }

    pub fn is_write_only(&self) -> bool {
        self.attribute.write_only
    }

    pub fn is_deferred_populate(&self) -> bool {
        self.attribute.deferred_populate
    }

    pub fn is_create_only(&self) -> bool {
        self.attribute.create_only
    }

    pub fn value_type(&self) -> EffectiveType<'a> {
        EffectiveType {
            attr_type: &self.attribute.attr_type,
            defs: self.defs,
            path: self.path.clone(),
            field_ancestry: FieldAncestry::from_input_mode(self.effective_input_mode()),
        }
    }

    /// Validate an authored value with this attribute's effective ancestry.
    pub fn validate(&self, value: &Value) -> Result<(), TypeError> {
        self.value_type().validate(value)
    }

    /// Validate an authored value and retain paths for every diagnostic.
    pub fn validate_collect(&self, value: &Value) -> Vec<(FieldPath, TypeError)> {
        self.value_type().validate_collect(value)
    }

    /// Canonicalize provider or persisted state with this attribute's
    /// effective ancestry.
    pub fn canonicalize_state(&self, value: Value) -> Value {
        self.value_type().canonicalize_state(value)
    }

    pub fn field_candidates_with_budget(
        &self,
        budget: &mut ShapeWalkBudget,
    ) -> Vec<EffectiveField<'a>> {
        self.value_type().field_candidates_with_budget(budget)
    }

    pub fn field_candidates(&self) -> Vec<EffectiveField<'a>> {
        self.value_type().field_candidates()
    }
}

/// A nested struct field whose input mode has been resolved for one concrete
/// reference path from a validated resource or config root.
///
/// This type has no public constructor. Consequently, callers cannot inspect
/// a nested field mode without first choosing the path that determines it.
#[derive(Debug, Clone)]
pub struct EffectiveField<'a> {
    field: &'a StructField,
    defs: &'a BTreeMap<String, AttributeType>,
    path: Vec<String>,
    ancestry: FieldAncestry,
}

impl<'a> EffectiveField<'a> {
    pub fn name(&self) -> &'a str {
        &self.field.name
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub fn field_type(&self) -> &'a AttributeType {
        &self.field.field_type
    }

    pub fn description(&self) -> Option<&'a str> {
        self.field.description.as_deref()
    }

    pub fn provider_name(&self) -> Option<&'a str> {
        self.field.provider_name.as_deref()
    }

    /// Return the authoring block alias only when it is active on this path.
    /// A declared alias below provider-populated output is inert.
    pub fn block_name(&self) -> Option<&'a str> {
        self.ancestry.block_name(self.field)
    }

    pub fn effective_input_mode(&self) -> InputMode {
        self.ancestry.effective_input_mode(self.field)
    }

    pub fn is_required(&self) -> bool {
        self.effective_input_mode().is_required()
    }

    pub fn is_read_only(&self) -> bool {
        self.effective_input_mode().is_provider_populated()
    }

    pub fn is_write_only(&self) -> bool {
        self.field.write_only
    }

    pub fn is_deferred_populate(&self) -> bool {
        self.field.deferred_populate
    }

    pub fn value_type(&self) -> EffectiveType<'a> {
        EffectiveType {
            attr_type: &self.field.field_type,
            defs: self.defs,
            path: self.path.clone(),
            field_ancestry: self.ancestry.descend(self.field),
        }
    }

    pub fn field_candidates_with_budget(
        &self,
        budget: &mut ShapeWalkBudget,
    ) -> Vec<EffectiveField<'a>> {
        self.value_type().field_candidates_with_budget(budget)
    }

    pub fn field_candidates(&self) -> Vec<EffectiveField<'a>> {
        self.value_type().field_candidates()
    }
}

/// A type cursor that retains the effective ancestry of the schema node whose
/// value it describes. Transparent list/union descent therefore cannot lose
/// the mode context needed when the cursor next reaches a struct field.
#[derive(Debug, Clone)]
pub struct EffectiveType<'a> {
    attr_type: &'a AttributeType,
    defs: &'a BTreeMap<String, AttributeType>,
    path: Vec<String>,
    field_ancestry: FieldAncestry,
}

impl<'a> EffectiveType<'a> {
    pub fn as_type(&self) -> &'a AttributeType {
        self.attr_type
    }

    pub fn path(&self) -> &[String] {
        &self.path
    }

    pub(crate) fn defs(&self) -> &'a BTreeMap<String, AttributeType> {
        self.defs
    }

    pub(crate) fn ancestry(&self) -> FieldAncestry {
        self.field_ancestry
    }

    pub fn shape(&self) -> Shape<'a> {
        self.attr_type.shape_with_defs(self.defs)
    }

    /// Validate an authored value without discarding the reference path that
    /// determines nested field modes.
    pub fn validate(&self, value: &Value) -> Result<(), TypeError> {
        validate_attr_with_defs_and_ancestry(self.attr_type, value, self.defs, self.field_ancestry)
    }

    /// Validate an authored value and retain paths for every diagnostic.
    pub fn validate_collect(&self, value: &Value) -> Vec<(FieldPath, TypeError)> {
        let mut out = Vec::new();
        Schema::collect_attr_into(
            self.attr_type,
            self.defs,
            &FieldPath::new(),
            value,
            self.field_ancestry,
            &mut out,
        );
        out
    }

    /// Canonicalize provider or persisted state without discarding the
    /// reference path that determines nested field modes.
    pub fn canonicalize_state(&self, value: Value) -> Value {
        crate::value::canonicalize_state_with_type_and_ancestry(
            value,
            self.attr_type,
            self.defs,
            self.field_ancestry,
        )
    }

    pub fn list_element(&self) -> Option<Self> {
        let Shape::List { element_type, .. } = self.shape() else {
            return None;
        };
        Some(Self {
            attr_type: element_type,
            defs: self.defs,
            path: self.path.clone(),
            field_ancestry: self.field_ancestry,
        })
    }

    pub fn map_value(&self) -> Option<Self> {
        let Shape::Map { value, .. } = self.shape() else {
            return None;
        };
        Some(Self {
            attr_type: value,
            defs: self.defs,
            path: self.path.clone(),
            field_ancestry: self.field_ancestry,
        })
    }

    pub fn field_candidates_with_budget(
        &self,
        budget: &mut ShapeWalkBudget,
    ) -> Vec<EffectiveField<'a>> {
        let mut fields = Vec::new();
        collect_effective_field_candidates(
            self.attr_type,
            self.defs,
            &self.path,
            self.field_ancestry,
            budget,
            &mut fields,
        );
        fields
    }

    pub fn field_candidates(&self) -> Vec<EffectiveField<'a>> {
        let mut budget = ShapeWalkBudget::new(64);
        self.field_candidates_with_budget(&mut budget)
    }

    pub fn find_field_with_budget(
        &self,
        name: &str,
        budget: &mut ShapeWalkBudget,
    ) -> Option<EffectiveField<'a>> {
        self.field_candidates_with_budget(budget)
            .into_iter()
            .find(|field| field.name() == name || field.block_name() == Some(name))
    }

    fn find_field_by_canonical_name_with_budget(
        &self,
        name: &str,
        budget: &mut ShapeWalkBudget,
    ) -> Option<EffectiveField<'a>> {
        self.field_candidates_with_budget(budget)
            .into_iter()
            .find(|field| field.name() == name)
    }
}

fn collect_effective_field_candidates<'a>(
    attr_type: &'a AttributeType,
    defs: &'a BTreeMap<String, AttributeType>,
    parent_path: &[String],
    ancestry: FieldAncestry,
    budget: &mut ShapeWalkBudget,
    out: &mut Vec<EffectiveField<'a>>,
) {
    if !budget.take() {
        return;
    }
    match attr_type.shape_with_defs(defs) {
        Shape::Struct { name: _ } => {
            let Some(fields) = struct_fields_with_defs(attr_type, defs) else {
                return;
            };
            out.extend(fields.iter().map(|field| {
                let mut path = parent_path.to_vec();
                path.push(field.name.clone());
                EffectiveField {
                    field,
                    defs,
                    path,
                    ancestry,
                }
            }));
        }
        Shape::List { element_type, .. } => collect_effective_field_candidates(
            element_type,
            defs,
            parent_path,
            ancestry,
            budget,
            out,
        ),
        Shape::Union => {
            let Some(members) = union_members_with_defs(attr_type, defs) else {
                return;
            };
            for member in members.iter().flatten() {
                collect_effective_field_candidates(
                    member.as_attr(),
                    defs,
                    parent_path,
                    ancestry,
                    budget,
                    out,
                );
            }
        }
        Shape::String { .. }
        | Shape::Int { .. }
        | Shape::Float { .. }
        | Shape::Bool
        | Shape::Duration
        | Shape::Enum { .. }
        | Shape::Map { .. } => {}
    }
}

/// One resolved node on a path that begins at a resource attribute root.
#[derive(Debug, Clone)]
pub enum EffectiveSchemaNode<'a> {
    Attribute(EffectiveAttribute<'a>),
    Field(EffectiveField<'a>),
}

impl<'a> EffectiveSchemaNode<'a> {
    pub fn name(&self) -> &'a str {
        match self {
            Self::Attribute(attribute) => attribute.name(),
            Self::Field(field) => field.name(),
        }
    }

    pub fn path(&self) -> &[String] {
        match self {
            Self::Attribute(attribute) => attribute.path(),
            Self::Field(field) => field.path(),
        }
    }

    pub fn field_type(&self) -> &'a AttributeType {
        match self {
            Self::Attribute(attribute) => attribute.field_type(),
            Self::Field(field) => field.field_type(),
        }
    }

    pub fn description(&self) -> Option<&'a str> {
        match self {
            Self::Attribute(attribute) => attribute.description(),
            Self::Field(field) => field.description(),
        }
    }

    pub fn block_name(&self) -> Option<&'a str> {
        match self {
            Self::Attribute(attribute) => attribute.block_name(),
            Self::Field(field) => field.block_name(),
        }
    }

    pub fn effective_input_mode(&self) -> InputMode {
        match self {
            Self::Attribute(attribute) => attribute.effective_input_mode(),
            Self::Field(field) => field.effective_input_mode(),
        }
    }

    pub fn is_required(&self) -> bool {
        self.effective_input_mode().is_required()
    }

    pub fn is_read_only(&self) -> bool {
        self.effective_input_mode().is_provider_populated()
    }

    pub fn is_write_only(&self) -> bool {
        match self {
            Self::Attribute(attribute) => attribute.is_write_only(),
            Self::Field(field) => field.is_write_only(),
        }
    }

    pub fn is_deferred_populate(&self) -> bool {
        match self {
            Self::Attribute(attribute) => attribute.is_deferred_populate(),
            Self::Field(field) => field.is_deferred_populate(),
        }
    }

    pub fn value_type(&self) -> EffectiveType<'a> {
        match self {
            Self::Attribute(attribute) => attribute.value_type(),
            Self::Field(field) => field.value_type(),
        }
    }

    pub fn field_candidates_with_budget(
        &self,
        budget: &mut ShapeWalkBudget,
    ) -> Vec<EffectiveField<'a>> {
        self.value_type().field_candidates_with_budget(budget)
    }

    pub fn field_candidates(&self) -> Vec<EffectiveField<'a>> {
        self.value_type().field_candidates()
    }
}

/// A resource schema that has passed every whole-schema rule.
#[derive(Debug, Clone)]
pub struct ValidatedResourceSchema {
    schema: ResourceSchema,
}

impl ValidatedResourceSchema {
    pub fn as_schema(&self) -> &ResourceSchema {
        &self.schema
    }

    pub fn parts(&self) -> ResourceSchemaParts<'_> {
        self.schema.parts()
    }

    /// Iterate top-level attributes in canonical-name order.
    ///
    /// [`ResourceSchema`] stores attributes in a [`BTreeMap`], so every
    /// declaration, diagnostic, and completion walk gets the same order by
    /// construction.
    pub fn effective_attributes(&self) -> impl Iterator<Item = EffectiveAttribute<'_>> {
        self.schema
            .attributes
            .values()
            .map(|attribute| EffectiveAttribute {
                attribute,
                defs: &self.schema.defs,
                path: vec![attribute.name.clone()],
            })
    }

    /// Look up a top-level attribute by its canonical schema name.
    ///
    /// Provider/state/differ paths must use this lookup. Block aliases are an
    /// authored-DSL concern and are accepted only by
    /// [`Self::effective_attribute_by_authored_name`].
    pub fn effective_attribute(&self, name: &str) -> Option<EffectiveAttribute<'_>> {
        let attribute = self.schema.attributes.get(name)?;
        Some(EffectiveAttribute {
            attribute,
            defs: &self.schema.defs,
            path: vec![attribute.name.clone()],
        })
    }

    /// Look up a top-level attribute by either its canonical name or its
    /// authored block alias.
    pub fn effective_attribute_by_authored_name(
        &self,
        name: &str,
    ) -> Option<EffectiveAttribute<'_>> {
        let attribute = self.schema.attributes.get(name).or_else(|| {
            self.schema
                .attributes
                .values()
                .find(|attribute| attribute.block_name.as_deref() == Some(name))
        })?;
        Some(EffectiveAttribute {
            attribute,
            defs: &self.schema.defs,
            path: vec![attribute.name.clone()],
        })
    }

    /// Resolve a canonical provider/state path while retaining the
    /// provider-populated ancestry of the selected reference path.
    pub fn effective_node_at_path(&self, path: &[String]) -> Option<EffectiveSchemaNode<'_>> {
        let (root, rest) = path.split_first()?;
        let mut node = EffectiveSchemaNode::Attribute(self.effective_attribute(root)?);
        for segment in rest {
            let mut budget = ShapeWalkBudget::new(64);
            let field = node
                .value_type()
                .find_field_by_canonical_name_with_budget(segment, &mut budget)?;
            node = EffectiveSchemaNode::Field(field);
        }
        Some(node)
    }

    /// Resolve an authored DSL path, including active block aliases, while
    /// retaining provider-populated ancestry.
    pub fn effective_node_at_authored_path(
        &self,
        path: &[String],
    ) -> Option<EffectiveSchemaNode<'_>> {
        let (root, rest) = path.split_first()?;
        let mut node =
            EffectiveSchemaNode::Attribute(self.effective_attribute_by_authored_name(root)?);
        for segment in rest {
            // Bound each transparent-shape search independently. A long valid
            // field path must not turn a later lookup into `None` merely
            // because earlier segments consumed a shared traversal budget.
            let mut budget = ShapeWalkBudget::new(64);
            let field = node
                .value_type()
                .find_field_with_budget(segment, &mut budget)?;
            node = EffectiveSchemaNode::Field(field);
        }
        Some(node)
    }

    /// Resolve the type at a runtime attribute path while retaining the
    /// provider-populated ancestry of every struct-field descent. Map keys are
    /// runtime path segments and therefore preserve the current ancestry.
    pub fn effective_type_at_attribute_path(
        &self,
        path: &[String],
    ) -> Result<EffectiveType<'_>, AttributePathTypeError> {
        let (first, rest) = path.split_first().ok_or(AttributePathTypeError::Empty)?;
        let attribute = self.schema.attributes.get(first).ok_or_else(|| {
            AttributePathTypeError::UnknownAttribute {
                attribute: first.clone(),
            }
        })?;
        let mut current = EffectiveAttribute {
            attribute,
            defs: &self.schema.defs,
            path: vec![attribute.name.clone()],
        }
        .value_type();
        let mut traversed = first.clone();

        for segment in rest {
            current = match current.shape() {
                Shape::Struct { name: struct_name } => {
                    let fields = struct_fields_with_defs(current.attr_type, current.defs)
                        .expect("Shape::Struct must expose struct fields internally");
                    let field = fields
                        .iter()
                        .find(|field| field.name == *segment)
                        .ok_or_else(|| AttributePathTypeError::UnknownStructField {
                            parent_path: traversed.clone(),
                            field: segment.clone(),
                            struct_name: struct_name.to_string(),
                        })?;
                    let mut field_path = current.path.clone();
                    field_path.push(field.name.clone());
                    EffectiveField {
                        field,
                        defs: current.defs,
                        path: field_path,
                        ancestry: current.field_ancestry,
                    }
                    .value_type()
                }
                Shape::Map { .. } => current
                    .map_value()
                    .expect("Shape::Map must expose its value type"),
                _ => {
                    return Err(AttributePathTypeError::CannotTraverse {
                        path: traversed,
                        type_name: current.attr_type.type_name(),
                    });
                }
            };
            traversed.push('.');
            traversed.push_str(segment);
        }

        Ok(current)
    }

    #[cfg(test)]
    pub(crate) fn validate(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> Result<(), Vec<TypeError>> {
        self.schema.validate_values(attributes)
    }

    #[cfg(test)]
    pub(crate) fn validate_with_origins(
        &self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
    ) -> Result<(), Vec<TypeError>> {
        self.schema
            .validate_values_with_origins(attributes, is_string_literal)
    }

    pub(crate) fn validate_with_origins_and_lookup(
        &self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
    ) -> Result<(), Vec<TypeError>> {
        self.schema
            .validate_values_with_origins_and_lookup(attributes, is_string_literal, lookup)
    }

    pub(crate) fn validate_known_values_with_origins_and_lookup(
        &self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
    ) -> Result<(), Vec<TypeError>> {
        self.schema.validate_known_values_with_origins_and_lookup(
            attributes,
            is_string_literal,
            lookup,
        )
    }

    pub(crate) fn validate_selected_known_values_with_origins_and_lookup(
        &self,
        attributes: &HashMap<String, Value>,
        selected: &HashSet<String>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
    ) -> Result<(), Vec<TypeError>> {
        self.schema
            .validate_selected_known_values_with_origins_and_lookup(
                attributes,
                selected,
                is_string_literal,
                lookup,
            )
    }
}

/// Provider-config attribute types that have passed the shared graph validator.
#[derive(Debug, Clone, Default)]
pub struct ValidatedConfigSchema {
    attribute_types: BTreeMap<String, AttributeType>,
}

impl ValidatedConfigSchema {
    pub fn validate(
        attribute_types: BTreeMap<String, AttributeType>,
    ) -> Result<Self, ConfigSchemaValidationError> {
        let defs = BTreeMap::new();
        let mut issues = Vec::new();
        let mut visited_definitions = HashSet::new();
        let mut identified_kinds = HashMap::new();
        for (name, attr_type) in &attribute_types {
            let location = SchemaValidationIssueLocation::Attribute(name.clone());
            validate_type_definition(
                attr_type,
                &defs,
                location.clone(),
                Some(false),
                &mut visited_definitions,
                &mut issues,
            );
            validate_identified_scalar_kind_consistency(
                attr_type,
                location,
                &mut identified_kinds,
                &mut issues,
            );
        }
        normalize_issues(&mut issues);
        if issues.is_empty() {
            Ok(Self { attribute_types })
        } else {
            Err(ConfigSchemaValidationError { issues })
        }
    }

    pub fn parts(&self) -> &BTreeMap<String, AttributeType> {
        &self.attribute_types
    }

    /// Start an ancestry-carrying traversal at one provider-config type.
    /// Config roots have no input mode, so their first fields have no
    /// provider-populated ancestor.
    pub fn effective_type(&self, name: &str) -> Option<EffectiveType<'_>> {
        self.attribute_types
            .get(name)
            .map(|attr_type| EffectiveType {
                attr_type,
                defs: empty_defs_for_schema_walks(),
                path: vec![name.to_string()],
                field_ancestry: FieldAncestry::WRITABLE,
            })
    }

    /// Own one validated, genuinely pathless config type for APIs that need a
    /// standalone value validator/canonicalizer.
    ///
    /// There is deliberately no constructor that accepts an arbitrary
    /// [`AttributeType`]: a type cloned from an [`EffectiveType`] must not be
    /// re-rooted and lose its resource-path ancestry.
    pub fn rootless_type(&self, name: &str) -> Option<RootlessType> {
        self.attribute_types
            .get(name)
            .cloned()
            .map(RootlessType::from_validated_config_type)
    }

    pub fn effective_field_at_path(&self, path: &[String]) -> Option<EffectiveField<'_>> {
        let (root, rest) = path.split_first()?;
        let mut current = self.effective_type(root)?;
        let mut resolved = None;
        for segment in rest {
            let mut budget = ShapeWalkBudget::new(64);
            let field = current.find_field_with_budget(segment, &mut budget)?;
            current = field.value_type();
            resolved = Some(field);
        }
        resolved
    }
}

fn normalize_issues(issues: &mut Vec<SchemaDefinitionIssue>) {
    issues.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.message.cmp(&right.message))
    });
    issues.dedup();
}

fn push_schema_issue(
    issues: &mut Vec<SchemaDefinitionIssue>,
    location: &SchemaValidationIssueLocation,
    message: impl Into<String>,
) {
    issues.push(SchemaDefinitionIssue {
        path: location.to_string(),
        location: location.clone(),
        message: message.into(),
    });
}

fn validate_identified_scalar_kind_consistency(
    attribute_type: &AttributeType,
    location: SchemaValidationIssueLocation,
    seen: &mut HashMap<TypeIdentity, (RefinedScalarKind, SchemaValidationIssueLocation)>,
    issues: &mut Vec<SchemaDefinitionIssue>,
) {
    let identified = match attribute_type.raw_shape() {
        RawShape::String {
            identity: Some(identity),
            ..
        } => Some((identity, RefinedScalarKind::String)),
        RawShape::Int {
            identity: Some(identity),
            ..
        } => Some((identity, RefinedScalarKind::Int)),
        RawShape::Float {
            identity: Some(identity),
            ..
        } => Some((identity, RefinedScalarKind::Float)),
        RawShape::Enum {
            identity,
            base,
            values,
            dsl_aliases,
            validate: _validate,
            to_dsl,
        } => {
            let kind = scalar_kind_for_schema_base(base);
            if kind.is_none() {
                push_schema_issue(
                    issues,
                    &location,
                    format!("custom type `{identity}` must have a scalar base"),
                );
            }

            match values {
                Some(_values) => {
                    if to_dsl.is_some() {
                        push_schema_issue(
                            issues,
                            &location,
                            format!(
                                "closed enum `{identity}` has a DSL transform that protocol v12 cannot represent"
                            ),
                        );
                    }
                    if !matches!(
                        base.raw_shape(),
                        RawShape::String {
                            identity: None,
                            pattern: None,
                            length: None,
                            validate: None,
                            to_dsl: None,
                        }
                    ) {
                        push_schema_issue(
                            issues,
                            &location,
                            format!(
                                "closed enum `{identity}` has base metadata that protocol v12 cannot represent"
                            ),
                        );
                    }
                }
                None => {
                    if !dsl_aliases.is_empty() {
                        push_schema_issue(
                            issues,
                            &location,
                            format!(
                                "open enum `{identity}` has DSL aliases that protocol v12 cannot represent"
                            ),
                        );
                    }
                    if identity.dotted_prefix().is_none() {
                        push_schema_issue(
                            issues,
                            &location,
                            format!("open enum `{identity}` has no namespace for protocol v12"),
                        );
                    }
                }
            }

            kind.map(|kind| (identity, kind))
        }
        RawShape::String { identity: None, .. }
        | RawShape::Int { identity: None, .. }
        | RawShape::Float { identity: None, .. }
        | RawShape::Bool
        | RawShape::Duration
        | RawShape::List { .. }
        | RawShape::Map { .. }
        | RawShape::Struct { .. }
        | RawShape::Union(_)
        | RawShape::Ref(_) => None,
    };
    if let Some((identity, kind)) = identified {
        if let Some((first_kind, first_location)) = seen.get(identity) {
            if *first_kind != kind {
                push_schema_issue(
                    issues,
                    &location,
                    format!(
                        "custom type `{identity}` is bound to both {first_kind} at {first_location} and {kind}"
                    ),
                );
            }
        } else {
            seen.insert(identity.clone(), (kind, location.clone()));
        }
    }

    match attribute_type.raw_shape() {
        RawShape::List { element_type, .. } => validate_identified_scalar_kind_consistency(
            element_type,
            location.with_suffix(".element"),
            seen,
            issues,
        ),
        RawShape::Map { key, value } => {
            validate_identified_scalar_kind_consistency(
                key,
                location.with_suffix(".key"),
                seen,
                issues,
            );
            validate_identified_scalar_kind_consistency(
                value,
                location.with_suffix(".value"),
                seen,
                issues,
            );
        }
        RawShape::Struct { fields, .. } => {
            for field in fields {
                validate_identified_scalar_kind_consistency(
                    &field.field_type,
                    location.with_suffix(format_args!(".{}", field.name)),
                    seen,
                    issues,
                );
            }
        }
        RawShape::Union(members) => {
            for (index, member) in members.iter().enumerate() {
                validate_identified_scalar_kind_consistency(
                    member,
                    location.with_suffix(format_args!("[{index}]")),
                    seen,
                    issues,
                );
            }
        }
        RawShape::String { .. }
        | RawShape::Int { .. }
        | RawShape::Float { .. }
        | RawShape::Bool
        | RawShape::Duration
        | RawShape::Enum { .. }
        | RawShape::Ref(_) => {}
    }
}

fn scalar_kind_for_schema_base(attribute_type: &AttributeType) -> Option<RefinedScalarKind> {
    match attribute_type.raw_shape() {
        RawShape::String { .. } => Some(RefinedScalarKind::String),
        RawShape::Int { .. } => Some(RefinedScalarKind::Int),
        RawShape::Float { .. } => Some(RefinedScalarKind::Float),
        RawShape::Enum { base, .. } => scalar_kind_for_schema_base(base),
        RawShape::Bool
        | RawShape::Duration
        | RawShape::List { .. }
        | RawShape::Map { .. }
        | RawShape::Struct { .. }
        | RawShape::Union(_)
        | RawShape::Ref(_) => None,
    }
}

fn validate_attribute_mode(
    attribute: &AttributeSchema,
    location: &SchemaValidationIssueLocation,
    issues: &mut Vec<SchemaDefinitionIssue>,
) {
    match attribute.input_mode {
        InputMode::Optional => {
            if attribute.deferred_populate {
                push_schema_issue(
                    issues,
                    location,
                    "optional user input cannot be deferred-populate",
                );
            }
        }
        InputMode::Required => {
            if attribute.default.is_some() {
                push_schema_issue(
                    issues,
                    location,
                    "required user input cannot have a default",
                );
            }
            if attribute.removable.is_some() {
                push_schema_issue(
                    issues,
                    location,
                    "required user input cannot override removability",
                );
            }
            if attribute.deferred_populate {
                push_schema_issue(
                    issues,
                    location,
                    "required user input cannot be deferred-populate",
                );
            }
        }
        InputMode::ProviderPopulated => {
            if attribute.default.is_some() {
                push_schema_issue(
                    issues,
                    location,
                    "provider-populated output cannot have a default",
                );
            }
            if attribute.write_only {
                push_schema_issue(
                    issues,
                    location,
                    "provider-populated output cannot be write-only",
                );
            }
            if attribute.create_only {
                push_schema_issue(
                    issues,
                    location,
                    "provider-populated output cannot be create-only",
                );
            }
            if attribute.identity {
                push_schema_issue(
                    issues,
                    location,
                    "provider-populated output cannot be an identity attribute",
                );
            }
            if attribute.removable.is_some() {
                push_schema_issue(
                    issues,
                    location,
                    "provider-populated output cannot override removability",
                );
            }
            if attribute.block_name.is_some() {
                push_schema_issue(
                    issues,
                    location,
                    "provider-populated output cannot have a block name",
                );
            }
            if attribute.completions.is_some() {
                push_schema_issue(
                    issues,
                    location,
                    "provider-populated output cannot have completions",
                );
            }
        }
    }
}

fn validate_struct_field_mode(
    field: &StructField,
    location: &SchemaValidationIssueLocation,
    provider_populated_ancestor: Option<bool>,
    issues: &mut Vec<SchemaDefinitionIssue>,
) {
    if provider_populated_ancestor == Some(true) && field.write_only {
        push_schema_issue(
            issues,
            location,
            "write-only field appears below a provider-populated ancestor",
        );
    }
    if field.declared_input_mode.is_provider_populated() {
        if field.write_only {
            push_schema_issue(
                issues,
                location,
                "provider-populated field cannot be write-only",
            );
        }
        if provider_populated_ancestor == Some(false) && field.block_name.is_some() {
            push_schema_issue(
                issues,
                location,
                "provider-populated field cannot have a block name",
            );
        }
    }

    if let Some(provider_populated_ancestor) = provider_populated_ancestor {
        let effectively_provider_populated =
            provider_populated_ancestor || field.declared_input_mode.is_provider_populated();
        if field.deferred_populate && !effectively_provider_populated {
            push_schema_issue(
                issues,
                location,
                "user-input field cannot be deferred-populate",
            );
        }
    }
}

fn validate_type_definition(
    attr_type: &AttributeType,
    defs: &BTreeMap<String, AttributeType>,
    location: SchemaValidationIssueLocation,
    provider_populated_ancestor: Option<bool>,
    visited_definitions: &mut HashSet<(String, Option<bool>)>,
    issues: &mut Vec<SchemaDefinitionIssue>,
) {
    match &attr_type.kind {
        AttrTypeKind::Ref(name) => {
            let Some(definition) = defs.get(name) else {
                push_schema_issue(
                    issues,
                    &location,
                    format!("schema reference `{name}` is not defined in the enclosing schema"),
                );
                return;
            };
            let path_entry = (name.clone(), provider_populated_ancestor);
            if !visited_definitions.insert(path_entry) {
                return;
            }
            validate_type_definition(
                definition,
                defs,
                location,
                provider_populated_ancestor,
                visited_definitions,
                issues,
            );
        }
        AttrTypeKind::List { element_type, .. } => validate_type_definition(
            element_type,
            defs,
            location.with_suffix("[]"),
            provider_populated_ancestor,
            visited_definitions,
            issues,
        ),
        AttrTypeKind::Map { key, value } => {
            validate_type_definition(
                key,
                defs,
                location.with_suffix(".<key>"),
                provider_populated_ancestor,
                visited_definitions,
                issues,
            );
            validate_type_definition(
                value,
                defs,
                location.with_suffix(".<value>"),
                provider_populated_ancestor,
                visited_definitions,
                issues,
            );
        }
        AttrTypeKind::Struct { fields, .. } => {
            let mut names = HashSet::new();
            for field in fields {
                let field_location = location.with_suffix(format_args!(".{}", field.name));
                if !names.insert(field.name.as_str()) {
                    push_schema_issue(
                        issues,
                        &field_location,
                        format!("duplicate struct-field name `{}`", field.name),
                    );
                }
                validate_struct_field_mode(
                    field,
                    &field_location,
                    provider_populated_ancestor,
                    issues,
                );
                validate_type_definition(
                    &field.field_type,
                    defs,
                    field_location,
                    provider_populated_ancestor.map(|ancestor| {
                        ancestor || field.declared_input_mode.is_provider_populated()
                    }),
                    visited_definitions,
                    issues,
                );
            }
        }
        AttrTypeKind::Union(members) => {
            for (index, member) in members.as_raw_slice().iter().enumerate() {
                validate_type_definition(
                    member,
                    defs,
                    location.with_suffix(format_args!(".<union:{index}>")),
                    provider_populated_ancestor,
                    visited_definitions,
                    issues,
                );
            }
        }
        AttrTypeKind::String { .. }
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. } => {}
    }
}

fn validate_portable_default(
    value: &Value,
    location: SchemaValidationIssueLocation,
    issues: &mut Vec<SchemaDefinitionIssue>,
) {
    match value {
        Value::Deferred(_) => push_schema_issue(
            issues,
            &location,
            "schema default is not a portable wire value",
        ),
        Value::Concrete(ConcreteValue::Float(value)) if !value.is_finite() => {
            push_schema_issue(issues, &location, "schema default float must be finite");
        }
        Value::Concrete(ConcreteValue::List(values)) => {
            for (index, value) in values.iter().enumerate() {
                validate_portable_default(
                    value,
                    location.with_suffix(format_args!("[{index}]")),
                    issues,
                );
            }
        }
        Value::Concrete(ConcreteValue::Map(values)) => {
            for (name, value) in values {
                validate_portable_default(
                    value,
                    location.with_suffix(format_args!(".{name}")),
                    issues,
                );
            }
        }
        Value::Concrete(
            ConcreteValue::EnumIdentifier(_)
            | ConcreteValue::CanonicalEnum(_)
            | ConcreteValue::Duration(_),
        ) => push_schema_issue(
            issues,
            &location,
            "schema default is not a portable wire value",
        ),
        Value::Concrete(
            ConcreteValue::String(_)
            | ConcreteValue::Int(_)
            | ConcreteValue::Float(_)
            | ConcreteValue::Bool(_)
            | ConcreteValue::StringList(_),
        ) => {}
    }
}

fn build_validator_from_types(types: &[ValidatorType]) -> Option<ResourceValidator> {
    let mut has_tags_key_value_check = false;
    for validator in types {
        match validator {
            ValidatorType::TagsKeyValueCheck => has_tags_key_value_check = true,
        }
    }
    has_tags_key_value_check.then_some(validate_tags_key_value)
}

fn validate_tags_key_value(attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
    if let Some(Value::Concrete(ConcreteValue::Map(map))) = attributes.get("tags") {
        let (mut has_key, mut has_value) = (false, false);
        for key in map.keys() {
            if key.eq_ignore_ascii_case("key") {
                has_key = true;
            } else if key.eq_ignore_ascii_case("value") {
                has_value = true;
            }
            if has_key && has_value {
                return Err(vec![TypeError::ResourceValidationFailed {
                    message: "tags map contains both 'key' and 'value' as keys, which looks like a Key/Value pair list. Use flat map syntax instead: tags = { Name = '...' }".to_string(),
                    attribute: Some("tags".to_string()),
                }]);
            }
        }
    }
    Ok(())
}

/// Return the top-level attribute keys the schema marks write-only.
///
/// Keeping this selection in the schema layer gives state persistence and
/// provider-value protection one definition of the schema-owned key set.
pub fn schema_write_only_keys(schema: Option<&ResourceSchema>) -> HashSet<String> {
    schema
        .into_iter()
        .flat_map(|schema| schema.attributes.iter())
        .filter(|(_key, attribute)| attribute.write_only)
        .map(|(key, _attribute)| key.clone())
        .collect()
}

#[derive(Debug, Clone, Copy, Default)]
struct WriteOnlyTypeAnalysis {
    contains_write_only: bool,
    union_conflict: bool,
    field_mode_conflict: bool,
}

fn type_write_only_analysis<'schema>(
    attr_type: &'schema AttributeType,
    defs: &'schema BTreeMap<String, AttributeType>,
    ref_chain: &mut Vec<&'schema str>,
) -> WriteOnlyTypeAnalysis {
    match &attr_type.kind {
        AttrTypeKind::Ref(name) => {
            if ref_chain.contains(&name.as_str()) {
                return WriteOnlyTypeAnalysis::default();
            }
            let Some(definition) = defs.get(name) else {
                return WriteOnlyTypeAnalysis::default();
            };
            ref_chain.push(name);
            let analysis = type_write_only_analysis(definition, defs, ref_chain);
            ref_chain.pop();
            analysis
        }
        AttrTypeKind::List { element_type, .. } => {
            type_write_only_analysis(element_type, defs, ref_chain)
        }
        AttrTypeKind::Map { value, .. } => type_write_only_analysis(value, defs, ref_chain),
        AttrTypeKind::Struct { fields, .. } => {
            fields
                .iter()
                .fold(WriteOnlyTypeAnalysis::default(), |analysis, field| {
                    let nested = type_write_only_analysis(&field.field_type, defs, ref_chain);
                    WriteOnlyTypeAnalysis {
                        contains_write_only: analysis.contains_write_only
                            || field.write_only
                            || nested.contains_write_only,
                        union_conflict: analysis.union_conflict || nested.union_conflict,
                        field_mode_conflict: analysis.field_mode_conflict
                            || nested.field_mode_conflict
                            || (field.write_only
                                && field.declared_input_mode.is_provider_populated()),
                    }
                })
        }
        AttrTypeKind::Union(members) => {
            let nested = members.as_raw_slice().iter().fold(
                WriteOnlyTypeAnalysis::default(),
                |analysis, member| {
                    let member = type_write_only_analysis(member, defs, ref_chain);
                    WriteOnlyTypeAnalysis {
                        contains_write_only: analysis.contains_write_only
                            || member.contains_write_only,
                        union_conflict: analysis.union_conflict || member.union_conflict,
                        field_mode_conflict: analysis.field_mode_conflict
                            || member.field_mode_conflict,
                    }
                },
            );
            WriteOnlyTypeAnalysis {
                union_conflict: nested.contains_write_only || nested.union_conflict,
                ..nested
            }
        }
        AttrTypeKind::String { .. }
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. } => WriteOnlyTypeAnalysis::default(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOnlySchemaConflictKind {
    TopLevelAndNested,
    UnionMemberBearing,
    NestedFieldReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOnlySchemaConflict {
    attribute: String,
    kind: WriteOnlySchemaConflictKind,
}

impl WriteOnlySchemaConflict {
    pub fn attribute(&self) -> &str {
        &self.attribute
    }

    pub fn message(&self) -> &'static str {
        match self.kind {
            WriteOnlySchemaConflictKind::TopLevelAndNested => {
                "top-level attribute cannot be both write-only and write-only-bearing"
            }
            WriteOnlySchemaConflictKind::UnionMemberBearing => {
                "union members cannot be write-only-bearing"
            }
            WriteOnlySchemaConflictKind::NestedFieldReadWrite => {
                "struct field cannot be both write-only and provider-populated"
            }
        }
    }
}

fn first_write_only_conflict_in(
    attributes: &BTreeMap<String, AttributeSchema>,
    defs: &BTreeMap<String, AttributeType>,
) -> Option<WriteOnlySchemaConflict> {
    attributes.iter().find_map(|(name, attribute)| {
        let analysis = type_write_only_analysis(&attribute.attr_type, defs, &mut Vec::new());
        if analysis.field_mode_conflict {
            return Some(WriteOnlySchemaConflict {
                attribute: name.clone(),
                kind: WriteOnlySchemaConflictKind::NestedFieldReadWrite,
            });
        }
        if attribute.write_only && analysis.contains_write_only {
            return Some(WriteOnlySchemaConflict {
                attribute: name.clone(),
                kind: WriteOnlySchemaConflictKind::TopLevelAndNested,
            });
        }
        analysis.union_conflict.then(|| WriteOnlySchemaConflict {
            attribute: name.clone(),
            kind: WriteOnlySchemaConflictKind::UnionMemberBearing,
        })
    })
}

fn project_write_only_leaves<'schema>(
    attr_type: &'schema AttributeType,
    value: &Value,
    defs: &'schema BTreeMap<String, AttributeType>,
    path: &mut Vec<LeafPathSegment>,
    ref_chain: &mut Vec<&'schema str>,
    inherited_secret: bool,
    leaves: &mut Vec<(LeafPath, Value, &'schema AttributeType)>,
) {
    if let Value::Deferred(DeferredValue::Secret(inner)) = value {
        project_write_only_leaves(attr_type, inner, defs, path, ref_chain, true, leaves);
        return;
    }

    match &attr_type.kind {
        AttrTypeKind::Ref(name) => {
            if ref_chain.contains(&name.as_str()) {
                return;
            }
            let Some(definition) = defs.get(name) else {
                return;
            };
            ref_chain.push(name);
            project_write_only_leaves(
                definition,
                value,
                defs,
                path,
                ref_chain,
                inherited_secret,
                leaves,
            );
            ref_chain.pop();
        }
        AttrTypeKind::List { element_type, .. } => {
            let Value::Concrete(ConcreteValue::List(items)) = value else {
                return;
            };
            for (index, item) in items.iter().enumerate() {
                path.push(LeafPathSegment::ListIndex(index));
                project_write_only_leaves(
                    element_type,
                    item,
                    defs,
                    path,
                    &mut Vec::new(),
                    inherited_secret,
                    leaves,
                );
                path.pop();
            }
        }
        AttrTypeKind::Map {
            value: value_type, ..
        } => {
            let Value::Concrete(ConcreteValue::Map(entries)) = value else {
                return;
            };
            for (key, entry) in entries {
                path.push(LeafPathSegment::MapKey(key.clone()));
                project_write_only_leaves(
                    value_type,
                    entry,
                    defs,
                    path,
                    &mut Vec::new(),
                    inherited_secret,
                    leaves,
                );
                path.pop();
            }
        }
        AttrTypeKind::Struct { fields, .. } => {
            let Value::Concrete(ConcreteValue::Map(entries)) = value else {
                return;
            };
            for field in fields {
                let Some(field_value) = entries.get(&field.name) else {
                    continue;
                };
                path.push(LeafPathSegment::StructField(field.name.clone()));
                if field.write_only {
                    let projected = if inherited_secret
                        && !matches!(
                            field_value,
                            Value::Deferred(
                                DeferredValue::Secret(_) | DeferredValue::RedactedSecret { .. }
                            )
                        ) {
                        Value::Deferred(DeferredValue::Secret(Box::new(field_value.clone())))
                    } else {
                        field_value.clone()
                    };
                    leaves.push((
                        LeafPath::from_segments(path.iter().cloned()),
                        projected,
                        &field.field_type,
                    ));
                } else {
                    project_write_only_leaves(
                        &field.field_type,
                        field_value,
                        defs,
                        path,
                        &mut Vec::new(),
                        inherited_secret,
                        leaves,
                    );
                }
                path.pop();
            }
        }
        AttrTypeKind::Union(_)
        | AttrTypeKind::String { .. }
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. } => {}
    }
}

fn strip_write_only_leaves_from_value<'schema>(
    attr_type: &'schema AttributeType,
    value: &Value,
    defs: &'schema BTreeMap<String, AttributeType>,
    ref_chain: &mut Vec<&'schema str>,
) -> Value {
    if let Value::Deferred(DeferredValue::Secret(inner)) = value {
        return Value::Deferred(DeferredValue::Secret(Box::new(
            strip_write_only_leaves_from_value(attr_type, inner, defs, ref_chain),
        )));
    }

    match &attr_type.kind {
        AttrTypeKind::Ref(name) => {
            if ref_chain.contains(&name.as_str()) {
                return value.clone();
            }
            let Some(definition) = defs.get(name) else {
                return value.clone();
            };
            ref_chain.push(name);
            let stripped = strip_write_only_leaves_from_value(definition, value, defs, ref_chain);
            ref_chain.pop();
            stripped
        }
        AttrTypeKind::List { element_type, .. } => {
            let Value::Concrete(ConcreteValue::List(items)) = value else {
                return value.clone();
            };
            Value::Concrete(ConcreteValue::List(
                items
                    .iter()
                    .map(|item| {
                        strip_write_only_leaves_from_value(
                            element_type,
                            item,
                            defs,
                            &mut Vec::new(),
                        )
                    })
                    .collect(),
            ))
        }
        AttrTypeKind::Map {
            value: value_type, ..
        } => {
            let Value::Concrete(ConcreteValue::Map(entries)) = value else {
                return value.clone();
            };
            Value::Concrete(ConcreteValue::Map(
                entries
                    .iter()
                    .map(|(key, entry)| {
                        (
                            key.clone(),
                            strip_write_only_leaves_from_value(
                                value_type,
                                entry,
                                defs,
                                &mut Vec::new(),
                            ),
                        )
                    })
                    .collect(),
            ))
        }
        AttrTypeKind::Struct { fields, .. } => {
            let Value::Concrete(ConcreteValue::Map(entries)) = value else {
                return value.clone();
            };
            Value::Concrete(ConcreteValue::Map(
                entries
                    .iter()
                    .filter_map(|(key, entry)| {
                        let field = fields.iter().find(|field| field.name == *key);
                        match field {
                            Some(field) if field.write_only => None,
                            Some(field) => Some((
                                key.clone(),
                                strip_write_only_leaves_from_value(
                                    &field.field_type,
                                    entry,
                                    defs,
                                    &mut Vec::new(),
                                ),
                            )),
                            None => Some((key.clone(), entry.clone())),
                        }
                    })
                    .collect(),
            ))
        }
        AttrTypeKind::Union(_)
        | AttrTypeKind::String { .. }
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. } => value.clone(),
    }
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

fn reveal_secret_value_for_validation(value: &Value) -> Value {
    match value {
        Value::Deferred(DeferredValue::Secret(inner)) => reveal_secret_value_for_validation(inner),
        Value::Concrete(ConcreteValue::List(items)) => Value::Concrete(ConcreteValue::List(
            items
                .iter()
                .map(reveal_secret_value_for_validation)
                .collect(),
        )),
        Value::Concrete(ConcreteValue::Map(map)) => Value::Concrete(ConcreteValue::Map(
            map.iter()
                .map(|(key, value)| (key.clone(), reveal_secret_value_for_validation(value)))
                .collect(),
        )),
        other => other.clone(),
    }
}

fn run_resource_validator(
    validator: ResourceValidator,
    attributes: &HashMap<String, Value>,
) -> Result<(), Vec<TypeError>> {
    if attributes.values().any(crate::value::contains_secret) {
        let revealed = attributes
            .iter()
            .map(|(name, value)| (name.clone(), reveal_secret_value_for_validation(value)))
            .collect();
        validator(&revealed).map_err(|errors| {
            errors
                .into_iter()
                .map(TypeError::masked_for_secret)
                .collect()
        })
    } else {
        validator(attributes)
    }
}

impl ResourceSchema {
    pub fn new(resource_type: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            attributes: BTreeMap::new(),
            duplicate_attributes: Vec::new(),
            description: None,
            validation: ResourceValidation::None,
            kind: SchemaKind::Resource,
            unique_name: UniqueNameSpec::Conflicting,
            operation_config: None,
            exclusive_required: Vec::new(),
            default_wait_timeout: None,
            default_wait_interval: None,
            defs: std::collections::BTreeMap::new(),
        }
    }

    /// Validate the complete schema graph and produce the only resource
    /// schema type accepted at admission boundaries.
    pub fn validate(mut self) -> Result<ValidatedResourceSchema, ResourceSchemaValidationError> {
        let mut issues = Vec::new();
        let mut visited_definitions = HashSet::new();
        let mut identified_kinds = HashMap::new();

        for name in &self.duplicate_attributes {
            let location = SchemaValidationIssueLocation::Attribute(name.clone());
            push_schema_issue(
                &mut issues,
                &location,
                format!("duplicate top-level attribute name `{name}`"),
            );
        }

        for (name, attribute) in &self.attributes {
            let location = SchemaValidationIssueLocation::Attribute(name.clone());
            validate_attribute_mode(attribute, &location, &mut issues);
            if let Some(default) = &attribute.default {
                validate_portable_default(default, location.clone(), &mut issues);
            }
            validate_type_definition(
                &attribute.attr_type,
                &self.defs,
                location.clone(),
                Some(attribute.input_mode.is_provider_populated()),
                &mut visited_definitions,
                &mut issues,
            );
            validate_identified_scalar_kind_consistency(
                &attribute.attr_type,
                location,
                &mut identified_kinds,
                &mut issues,
            );
        }

        for (name, definition) in &self.defs {
            validate_identified_scalar_kind_consistency(
                definition,
                SchemaValidationIssueLocation::Definition(name.clone()),
                &mut identified_kinds,
                &mut issues,
            );
            if !visited_definitions.insert((name.clone(), None)) {
                continue;
            }
            validate_type_definition(
                definition,
                &self.defs,
                SchemaValidationIssueLocation::Definition(name.clone()),
                None,
                &mut visited_definitions,
                &mut issues,
            );
        }

        for (group_index, group) in self.exclusive_required.iter().enumerate() {
            for name in group {
                match self.attributes.get(name) {
                    None => {
                        let location =
                            SchemaValidationIssueLocation::ExclusiveRequired(group_index);
                        push_schema_issue(
                            &mut issues,
                            &location,
                            format!("exclusive_required names unknown attribute `{name}`"),
                        );
                    }
                    Some(attribute) if attribute.input_mode.is_provider_populated() => {
                        let location =
                            SchemaValidationIssueLocation::ExclusiveRequired(group_index);
                        push_schema_issue(
                            &mut issues,
                            &location,
                            format!(
                                "exclusive_required names provider-populated attribute `{name}`"
                            ),
                        );
                    }
                    Some(_) => {}
                }
            }
        }

        if let UniqueNameSpec::Attribute(name) = &self.unique_name
            && !self.attributes.contains_key(name)
        {
            let location = SchemaValidationIssueLocation::UniqueName;
            push_schema_issue(
                &mut issues,
                &location,
                format!("unique name names unknown attribute `{name}`"),
            );
        }

        if let Some(conflict) = self.first_write_only_conflict() {
            let location =
                SchemaValidationIssueLocation::Attribute(conflict.attribute().to_string());
            push_schema_issue(&mut issues, &location, conflict.message());
        }

        normalize_issues(&mut issues);
        if issues.is_empty() {
            self.duplicate_attributes.clear();
            Ok(ValidatedResourceSchema { schema: self })
        } else {
            Err(ResourceSchemaValidationError {
                resource_type: self.resource_type.clone(),
                issues,
            })
        }
    }

    pub fn parts(&self) -> ResourceSchemaParts<'_> {
        ResourceSchemaParts {
            resource_type: &self.resource_type,
            attributes: &self.attributes,
            description: self.description.as_deref(),
            validation: &self.validation,
            kind: self.kind,
            unique_name: &self.unique_name,
            operation_config: self.operation_config.as_ref(),
            exclusive_required: &self.exclusive_required,
            default_wait_timeout: self.default_wait_timeout,
            default_wait_interval: self.default_wait_interval,
            defs: &self.defs,
        }
    }

    pub fn resource_type(&self) -> &str {
        &self.resource_type
    }

    pub fn attributes(&self) -> &BTreeMap<String, AttributeSchema> {
        &self.attributes
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn validation(&self) -> &ResourceValidation {
        &self.validation
    }

    pub fn kind(&self) -> SchemaKind {
        self.kind
    }

    pub fn unique_name(&self) -> &UniqueNameSpec {
        &self.unique_name
    }

    pub fn operation_config(&self) -> Option<&OperationConfig> {
        self.operation_config.as_ref()
    }

    pub fn exclusive_required_groups(&self) -> &[Vec<String>] {
        &self.exclusive_required
    }

    pub fn default_wait_timeout(&self) -> Option<std::time::Duration> {
        self.default_wait_timeout
    }

    pub fn default_wait_interval(&self) -> Option<std::time::Duration> {
        self.default_wait_interval
    }

    pub fn defs(&self) -> &std::collections::BTreeMap<String, AttributeType> {
        &self.defs
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
        if self.attributes.contains_key(&schema.name) {
            self.duplicate_attributes.push(schema.name.clone());
        } else {
            self.attributes.insert(schema.name.clone(), schema);
        }
        self
    }

    /// Return the first schema shape that cannot safely represent nested
    /// write-only provenance.
    pub fn first_write_only_conflict(&self) -> Option<WriteOnlySchemaConflict> {
        first_write_only_conflict_in(&self.attributes, &self.defs)
    }

    /// Return whether a top-level attribute contains a nested write-only field.
    pub fn write_only_bearing(&self, key: &str) -> bool {
        self.attributes.get(key).is_some_and(|attribute| {
            type_write_only_analysis(&attribute.attr_type, &self.defs, &mut Vec::new())
                .contains_write_only
        })
    }

    /// Project the write-only fields that exist in one desired attribute value.
    ///
    /// Paths are derived from the desired value, including concrete list
    /// indices and map keys. The field type accompanies each value so callers
    /// can use the normal type-aware and secret-hash comparison path.
    pub fn write_only_leaf_projection<'schema>(
        &'schema self,
        key: &str,
        value: &Value,
    ) -> Vec<(LeafPath, Value, &'schema AttributeType)> {
        let Some(attribute) = self.attributes.get(key) else {
            return Vec::new();
        };
        let mut leaves = Vec::new();
        project_write_only_leaves(
            &attribute.attr_type,
            value,
            &self.defs,
            &mut Vec::new(),
            &mut Vec::new(),
            false,
            &mut leaves,
        );
        leaves.sort_by(|(left, _, _), (right, _, _)| left.cmp(right));
        leaves
    }

    pub(crate) fn strip_write_only_leaves(&self, key: &str, value: &Value) -> Value {
        self.attributes.get(key).map_or_else(
            || value.clone(),
            |attribute| {
                strip_write_only_leaves_from_value(
                    &attribute.attr_type,
                    value,
                    &self.defs,
                    &mut Vec::new(),
                )
            },
        )
    }

    /// Pair an attribute or derived subtype with this resource's definitions.
    ///
    /// Prefer this over [`AttributeType::in_schema`] whenever the owning
    /// resource schema is in hand so the attribute cannot accidentally be
    /// paired with another resource's same-shaped defs map.
    pub fn type_in_schema<'a>(&'a self, attr: &'a AttributeType) -> TypeInSchema<'a> {
        attr.in_schema(&self.defs)
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
    ) -> Option<UnionMembers<'a>> {
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
                    for member in members.iter().flatten() {
                        self.append_enum_values_from_top_level_attr_type(
                            member.as_attr(),
                            input,
                            out,
                        );
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
    ) -> Option<UnionMembers<'a>> {
        union_members_with_defs(ty, &self.defs)
    }

    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Attach portable validator declarations received from a provider.
    pub fn with_declared_validators(mut self, validators: Vec<ValidatorType>) -> Self {
        self.validation = ResourceValidation::Declared(validators);
        self
    }

    /// Return this schema's executable cross-attribute validator, if any.
    fn validator(&self) -> Option<ResourceValidator> {
        self.validation.validator()
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

    pub fn with_unique_name_attribute(mut self, attr: impl Into<String>) -> Self {
        self.unique_name = UniqueNameSpec::Attribute(attr.into());
        self
    }

    pub fn with_coexisting_replacement(mut self) -> Self {
        self.unique_name = UniqueNameSpec::Coexisting;
        self
    }

    pub fn with_conflicting_replacement(mut self) -> Self {
        self.unique_name = UniqueNameSpec::Conflicting;
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
            .filter(|(_, schema)| schema.is_read_only())
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Returns attributes that have default values and are not read-only.
    /// Each entry is (attribute_name, default_value).
    pub fn default_value_attributes(&self) -> Vec<(&str, &Value)> {
        self.attributes
            .iter()
            .filter(|(_, schema)| schema.default.is_some() && !schema.is_read_only())
            .map(|(name, schema)| (name.as_str(), schema.default.as_ref().unwrap()))
            .collect()
    }

    /// Returns default-value attributes not specified by the user, sorted by name.
    /// Each entry is (attribute_name, rendered_default_value).
    pub fn compute_default_attrs(&self, user_keys: &HashSet<&str>) -> Vec<(String, RenderedValue)> {
        let mut default_attrs: Vec<(&str, &Value)> = self
            .default_value_attributes()
            .into_iter()
            .filter(|(a, _)| !user_keys.contains(a))
            .collect();
        default_attrs.sort_by_key(|(a, _)| *a);
        default_attrs
            .into_iter()
            .map(|(name, val)| (name.to_string(), render_value(val)))
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

    /// Validate resource attributes.
    ///
    /// This variant does not have origin information for string values, so
    /// it cannot distinguish a user-typed `target_type = "aaa"` from a
    /// bare-identifier `target_type = aaa` — both surface as
    /// `InvalidEnumVariant`. Call `validate_with_origins` when the caller
    /// knows which attributes were written as quoted string literals
    /// (see #2094).
    #[cfg(test)]
    fn validate_values(&self, attributes: &HashMap<String, Value>) -> Result<(), Vec<TypeError>> {
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
    #[cfg(test)]
    fn validate_values_with_origins(
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
    fn validate_values_with_origins_and_lookup(
        &self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
    ) -> Result<(), Vec<TypeError>> {
        self.validate_inner(attributes, is_string_literal, lookup)
    }

    /// Validate constraints on values that are currently known.
    ///
    /// Unlike the full validation entry point, this method
    /// deliberately omits shape-of-resource checks that were already made
    /// when the source was expanded: required attributes, unknown
    /// attributes, provider-populated attributes, and exclusive-required
    /// groups. It is intended for a later resolution boundary, where a value
    /// that was deferred during expansion may have become concrete.
    ///
    /// Deferred leaves are ignored while concrete siblings in the same
    /// collection are still checked. The resource validator is re-run because
    /// it may express cross-attribute value constraints.
    fn validate_known_values_with_origins_and_lookup(
        &self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
    ) -> Result<(), Vec<TypeError>> {
        let mut errors = self.validate_value_entries(
            attributes,
            is_string_literal,
            lookup,
            ValueEntryValidation::KnownWritableOnly,
            None,
        );

        if let Some(validator) = self.validator()
            && let Err(mut validation_errors) = run_resource_validator(validator, attributes)
        {
            errors.append(&mut validation_errors);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Validate only the attributes whose values became known at the current
    /// resolution boundary.
    ///
    /// `attributes` is still the complete authored-value view so a resource
    /// validator can evaluate cross-attribute rules. Per-attribute schema and
    /// provider custom-type checks are restricted to `selected`; every other
    /// value was checked at an earlier boundary and may now be in a normalized
    /// representation that intentionally differs from the authored form.
    fn validate_selected_known_values_with_origins_and_lookup(
        &self,
        attributes: &HashMap<String, Value>,
        selected: &HashSet<String>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
    ) -> Result<(), Vec<TypeError>> {
        let mut errors = self.validate_value_entries(
            attributes,
            is_string_literal,
            lookup,
            ValueEntryValidation::KnownWritableOnly,
            Some(selected),
        );

        if !selected.is_empty()
            && let Some(validator) = self.validator()
            && let Err(mut validation_errors) = run_resource_validator(validator, attributes)
        {
            errors.append(&mut validation_errors);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
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
            if schema.is_required() && !attributes.contains_key(name) && schema.default.is_none() {
                errors.push(TypeError::MissingRequired { name: name.clone() });
            }
        }

        errors.extend(self.validate_value_entries(
            attributes,
            is_string_literal,
            lookup,
            ValueEntryValidation::FullInput,
            None,
        ));

        // Evaluate declarative exclusive-required groups (WASM-safe).
        for group in &self.exclusive_required {
            let refs: Vec<&str> = group.iter().map(|s| s.as_str()).collect();
            if let Err(mut e) = validators::validate_exclusive_required(attributes, &refs) {
                errors.append(&mut e);
            }
        }

        // Run custom validator if present
        if let Some(validator) = self.validator()
            && let Err(mut validation_errors) = run_resource_validator(validator, attributes)
        {
            errors.append(&mut validation_errors);
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn validate_value_entries(
        &self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
        lookup: CustomTypeLookup<'_>,
        mode: ValueEntryValidation,
        selected: Option<&HashSet<String>>,
    ) -> Vec<TypeError> {
        let mut errors = Vec::new();

        // Build block_name -> canonical_name map for alias resolution
        let bn_map = self.block_name_map();

        // Build suggestion candidates (canonical names + block name aliases)
        let mut known: Vec<&str> = self.attributes.keys().map(|s| s.as_str()).collect();
        for bn in bn_map.keys() {
            known.push(bn.as_str());
        }

        // Type check each attribute and reject unknown ones
        for (name, value) in attributes {
            if selected.is_some_and(|selected| !selected.contains(name)) {
                continue;
            }

            // Resolve block_name alias to canonical name
            let canonical = bn_map.get(name).map(|s| s.as_str()).unwrap_or(name);

            if let Some(schema) = self.attributes.get(canonical) {
                let effective_attribute = EffectiveAttribute {
                    attribute: schema,
                    defs: &self.defs,
                    path: vec![schema.name.clone()],
                };
                // `read_only` attributes are provider-populated, so accepting a user
                // value would silently drop it. Reject before type checking so the
                // diagnostic reports writability instead of a confusing type error.
                if schema.is_read_only() && mode == ValueEntryValidation::FullInput {
                    errors.push(TypeError::ReadOnlyAttribute { name: name.clone() });
                    continue;
                }

                if let Err(e) = effective_attribute.validate(value) {
                    // Tag the error with the attribute name the user actually
                    // wrote (which may be a block-name alias), so diagnostics
                    // point back at a token that appears in their source.
                    let tagged = e.with_attribute(name);
                    // Other schema shapes may use custom validators that return
                    // `InvalidEnumVariant`; quoted-form reshaping belongs only
                    // to an Enum attribute.
                    let reshaped = if is_string_literal(name.as_str())
                        && matches!(effective_attribute.value_type().shape(), Shape::Enum { .. })
                    {
                        tagged.into_string_literal_diagnostic()
                    } else {
                        tagged
                    };
                    errors.push(reshaped);
                }
                walk_custom_lookup(
                    &schema.attr_type,
                    value,
                    Some(name),
                    lookup,
                    &self.defs,
                    &mut errors,
                );
            } else if mode == ValueEntryValidation::FullInput {
                let suggestion = suggest_similar_name(name, &known);
                errors.push(TypeError::UnknownAttribute {
                    name: name.clone(),
                    suggestion,
                });
            }
        }

        errors
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueEntryValidation {
    FullInput,
    KnownWritableOnly,
}

/// Collect all attribute_name -> block_name mappings from all schemas.
/// This includes both top-level attributes and nested struct fields.
/// Used by the formatter to convert `= [{...}]` to block syntax.
pub fn collect_all_block_names(registry: &SchemaRegistry) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for (_provider, _resource_type, _kind, schema) in registry.iter() {
        let schema = schema.as_schema();
        for (attr_name, attr_schema) in &schema.attributes {
            if let Some(bn) = &attr_schema.block_name {
                result.insert(attr_name.clone(), bn.clone());
            }
            collect_block_names_from_type(
                &attr_schema.attr_type,
                &schema.defs,
                FieldAncestry::from_input_mode(attr_schema.input_mode),
                &mut HashSet::new(),
                &mut result,
            );
        }
    }
    result
}

fn collect_block_names_from_type(
    attr_type: &AttributeType,
    defs: &BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    visited_definitions: &mut HashSet<(String, FieldAncestry)>,
    result: &mut HashMap<String, String>,
) {
    match &attr_type.kind {
        AttrTypeKind::Ref(name) => {
            if visited_definitions.insert((name.clone(), ancestry))
                && let Some(definition) = defs.get(name)
            {
                collect_block_names_from_type(
                    definition,
                    defs,
                    ancestry,
                    visited_definitions,
                    result,
                );
            }
        }
        AttrTypeKind::Struct { fields, .. } => {
            for field in fields {
                if let Some(block_name) = ancestry.block_name(field) {
                    result.insert(field.name.clone(), block_name.to_string());
                }
                collect_block_names_from_type(
                    &field.field_type,
                    defs,
                    ancestry.descend(field),
                    visited_definitions,
                    result,
                );
            }
        }
        AttrTypeKind::List {
            element_type: inner,
            ..
        } => {
            collect_block_names_from_type(inner, defs, ancestry, visited_definitions, result);
        }
        AttrTypeKind::Map { value: inner, .. } => {
            collect_block_names_from_type(inner, defs, ancestry, visited_definitions, result);
        }
        AttrTypeKind::Union(types) => {
            for t in types.as_raw_slice() {
                collect_block_names_from_type(t, defs, ancestry, visited_definitions, result);
            }
        }
        AttrTypeKind::String { .. }
        | AttrTypeKind::ResourceHandle { .. }
        | AttrTypeKind::StringLiteralSet { .. }
        | AttrTypeKind::Int { .. }
        | AttrTypeKind::Float { .. }
        | AttrTypeKind::Bool
        | AttrTypeKind::Duration
        | AttrTypeKind::Enum { .. } => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlockNameConflict {
    resource_id: String,
    path: String,
    alias: String,
    canonical: String,
}

impl BlockNameConflict {
    pub(crate) fn resource_id(&self) -> &str {
        &self.resource_id
    }

    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    pub(crate) fn alias(&self) -> &str {
        &self.alias
    }

    pub(crate) fn canonical(&self) -> &str {
        &self.canonical
    }
}

impl fmt::Display for BlockNameConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: cannot use both '{}' and '{}' (they refer to the same attribute)",
            self.resource_id, self.alias, self.canonical
        )
    }
}

#[derive(Clone, Copy)]
struct BlockFieldRef<'a> {
    name: &'a str,
    block_name: Option<&'a str>,
    field_type: &'a AttributeType,
    child_ancestry: FieldAncestry,
}

#[derive(Clone, Copy)]
enum BlockFieldMatch<'a> {
    Canonical,
    Alias(BlockFieldRef<'a>),
}

fn nested_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}.{name}")
    }
}

fn match_block_field<'a>(
    key: &str,
    value: &Value,
    fields: &[BlockFieldRef<'a>],
    defs: &BTreeMap<String, AttributeType>,
) -> Option<BlockFieldMatch<'a>> {
    let canonical = fields.iter().copied().find(|field| field.name == key);
    let alias = fields
        .iter()
        .copied()
        .find(|field| field.block_name == Some(key) && field.name != key);

    match (canonical, alias) {
        // In the otherwise ambiguous case, a concrete list belongs to the
        // aliased field when the real field cannot itself accept a list. This
        // covers parser-produced block syntax as well as expressions already
        // inlined to lists in the authored snapshot. Every other value shape,
        // and a real list field, keeps the real-field meaning.
        (Some(canonical), Some(alias))
            if matches!(value, Value::Concrete(ConcreteValue::List(_)))
                && !matches!(
                    canonical.field_type.shape_with_defs(defs),
                    Shape::List { .. }
                ) =>
        {
            Some(BlockFieldMatch::Alias(alias))
        }
        (Some(_), _) => Some(BlockFieldMatch::Canonical),
        (None, Some(alias)) => Some(BlockFieldMatch::Alias(alias)),
        (None, None) => None,
    }
}

fn block_name_conflict(
    resource_id: &str,
    parent_path: &str,
    alias: &str,
    canonical: &str,
) -> BlockNameConflict {
    BlockNameConflict {
        resource_id: resource_id.to_string(),
        path: nested_path(parent_path, alias),
        alias: alias.to_string(),
        canonical: canonical.to_string(),
    }
}

fn value_for_field<'a>(
    map: &'a IndexMap<String, Value>,
    field: BlockFieldRef<'_>,
    fields: &[BlockFieldRef<'_>],
    defs: &BTreeMap<String, AttributeType>,
) -> Option<&'a Value> {
    if let Some(value) = map.get(field.name) {
        return Some(value);
    }
    let alias = field.block_name.filter(|alias| *alias != field.name)?;
    let value = map.get(alias)?;
    matches!(
        match_block_field(alias, value, fields, defs),
        Some(BlockFieldMatch::Alias(selected)) if selected.name == field.name
    )
    .then_some(value)
}

fn resolve_block_names_in_map(
    map: &mut IndexMap<String, Value>,
    authored_map: Option<&IndexMap<String, Value>>,
    fields: &[BlockFieldRef<'_>],
    context: BlockNameResolutionContext<'_>,
    parent_path: &str,
    conflicts: &mut Vec<BlockNameConflict>,
) {
    // Preserve the source map's order, matching the pre-walker resolver: each
    // renamed key is removed in authored order and appended under its canonical
    // spelling in that same order.
    let keys = map.keys().cloned().collect::<Vec<_>>();
    for key in keys {
        let Some(value) = map.get(&key) else {
            continue;
        };
        let decision_value = authored_map
            .and_then(|authored| authored.get(&key))
            .unwrap_or(value);
        let Some(BlockFieldMatch::Alias(field)) =
            match_block_field(&key, decision_value, fields, context.defs)
        else {
            continue;
        };

        if map.contains_key(field.name) {
            conflicts.push(block_name_conflict(
                context.resource_id,
                parent_path,
                &key,
                field.name,
            ));
            continue;
        }
        if let Some(value) = map.shift_remove(&key) {
            map.insert(field.name.to_string(), value);
        }
    }

    for field in fields {
        let authored_value = authored_map
            .and_then(|authored| value_for_field(authored, *field, fields, context.defs));
        let Some(value) = map.get_mut(field.name) else {
            continue;
        };
        resolve_block_names_in_value(
            value,
            authored_value,
            field.field_type,
            field.child_ancestry,
            context,
            &nested_path(parent_path, field.name),
            conflicts,
        );
    }
}

fn resolve_block_names_in_value(
    value: &mut Value,
    authored_value: Option<&Value>,
    attr_type: &AttributeType,
    ancestry: FieldAncestry,
    context: BlockNameResolutionContext<'_>,
    path: &str,
    conflicts: &mut Vec<BlockNameConflict>,
) {
    match attr_type.shape_with_defs(context.defs) {
        Shape::Struct { .. } => {
            let Value::Concrete(ConcreteValue::Map(map)) = value else {
                return;
            };
            let Some(struct_fields) = struct_fields_with_defs(attr_type, context.defs) else {
                return;
            };
            let fields = struct_fields
                .iter()
                .map(|field| BlockFieldRef {
                    name: &field.name,
                    block_name: ancestry.block_name(field),
                    field_type: &field.field_type,
                    child_ancestry: ancestry.descend(field),
                })
                .collect::<Vec<_>>();
            let authored_map = match authored_value {
                Some(Value::Concrete(ConcreteValue::Map(map))) => Some(map),
                _ => None,
            };
            resolve_block_names_in_map(map, authored_map, &fields, context, path, conflicts);
        }
        Shape::List { element_type, .. } => {
            if !matches!(
                element_type.shape_with_defs(context.defs),
                Shape::Struct { .. }
            ) {
                return;
            }
            let Value::Concrete(ConcreteValue::List(items)) = value else {
                return;
            };
            let Some(struct_fields) = struct_fields_with_defs(element_type, context.defs) else {
                return;
            };
            let fields = struct_fields
                .iter()
                .map(|field| BlockFieldRef {
                    name: &field.name,
                    block_name: ancestry.block_name(field),
                    field_type: &field.field_type,
                    child_ancestry: ancestry.descend(field),
                })
                .collect::<Vec<_>>();
            let authored_items = match authored_value {
                Some(Value::Concrete(ConcreteValue::List(items))) => Some(items.as_slice()),
                _ => None,
            };
            for (index, item) in items.iter_mut().enumerate() {
                let Value::Concrete(ConcreteValue::Map(map)) = item else {
                    continue;
                };
                let authored_map = authored_items
                    .and_then(|authored| authored.get(index))
                    .and_then(|item| match item {
                        Value::Concrete(ConcreteValue::Map(map)) => Some(map),
                        _ => None,
                    });
                resolve_block_names_in_map(
                    map,
                    authored_map,
                    &fields,
                    context,
                    &format!("{path}[{index}]"),
                    conflicts,
                );
            }
        }
        _ => {}
    }
}

#[derive(Clone, Copy)]
struct BlockNameResolutionContext<'a> {
    defs: &'a BTreeMap<String, AttributeType>,
    resource_id: &'a str,
}

fn block_fields(schema: &ResourceSchema) -> Vec<BlockFieldRef<'_>> {
    schema
        .attributes
        .values()
        .map(|attribute| BlockFieldRef {
            name: &attribute.name,
            block_name: attribute.block_name.as_deref(),
            field_type: &attribute.attr_type,
            child_ancestry: FieldAncestry::from_input_mode(attribute.input_mode),
        })
        .collect()
}

fn resolve_attribute_block_names(
    resource_id: &ResourceId,
    attributes: &mut IndexMap<String, Value>,
    authored_attributes: Option<&IndexMap<String, Value>>,
    schema: &ResourceSchema,
) -> Vec<BlockNameConflict> {
    let fields = block_fields(schema);
    let mut conflicts = Vec::new();
    let resource_id = resource_id.to_string();
    resolve_block_names_in_map(
        attributes,
        authored_attributes,
        &fields,
        BlockNameResolutionContext {
            defs: &schema.defs,
            resource_id: &resource_id,
        },
        "",
        &mut conflicts,
    );
    conflicts
}

fn find_block_name_alias_in_map(
    map: &IndexMap<String, Value>,
    fields: &[BlockFieldRef<'_>],
    defs: &BTreeMap<String, AttributeType>,
    resource_id: &str,
    parent_path: &str,
) -> Option<BlockNameConflict> {
    for (key, value) in map {
        if let Some(BlockFieldMatch::Alias(field)) = match_block_field(key, value, fields, defs) {
            return Some(block_name_conflict(
                resource_id,
                parent_path,
                key,
                field.name,
            ));
        }
    }

    for field in fields {
        let Some(value) = map.get(field.name) else {
            continue;
        };
        if let Some(alias) = find_block_name_alias_in_value(
            value,
            field.field_type,
            defs,
            field.child_ancestry,
            resource_id,
            &nested_path(parent_path, field.name),
        ) {
            return Some(alias);
        }
    }
    None
}

fn find_block_name_alias_in_value(
    value: &Value,
    attr_type: &AttributeType,
    defs: &BTreeMap<String, AttributeType>,
    ancestry: FieldAncestry,
    resource_id: &str,
    path: &str,
) -> Option<BlockNameConflict> {
    match attr_type.shape_with_defs(defs) {
        Shape::Struct { .. } => {
            let Value::Concrete(ConcreteValue::Map(map)) = value else {
                return None;
            };
            let fields = struct_fields_with_defs(attr_type, defs)?
                .iter()
                .map(|field| BlockFieldRef {
                    name: &field.name,
                    block_name: ancestry.block_name(field),
                    field_type: &field.field_type,
                    child_ancestry: ancestry.descend(field),
                })
                .collect::<Vec<_>>();
            find_block_name_alias_in_map(map, &fields, defs, resource_id, path)
        }
        Shape::List { element_type, .. }
            if matches!(element_type.shape_with_defs(defs), Shape::Struct { .. }) =>
        {
            let Value::Concrete(ConcreteValue::List(items)) = value else {
                return None;
            };
            let fields = struct_fields_with_defs(element_type, defs)?
                .iter()
                .map(|field| BlockFieldRef {
                    name: &field.name,
                    block_name: ancestry.block_name(field),
                    field_type: &field.field_type,
                    child_ancestry: ancestry.descend(field),
                })
                .collect::<Vec<_>>();
            items.iter().enumerate().find_map(|(index, item)| {
                let Value::Concrete(ConcreteValue::Map(map)) = item else {
                    return None;
                };
                find_block_name_alias_in_map(
                    map,
                    &fields,
                    defs,
                    resource_id,
                    &format!("{path}[{index}]"),
                )
            })
        }
        _ => None,
    }
}

/// Find the first block alias without cloning or mutating the resource.
///
/// This uses the same field matcher as the rename pass, including the narrow
/// ambiguous-name rule for parser-produced lists. Union values are left to
/// normal schema validation.
pub(crate) fn find_resource_block_name_alias<S>(
    resource: &Resource<S>,
    schema: &ResourceSchema,
) -> Option<BlockNameConflict> {
    find_block_name_alias_in_map(
        &resource.attributes,
        &block_fields(schema),
        &schema.defs,
        &resource.id.to_string(),
        "",
    )
}

fn resolve_block_names_detailed(
    resources: &mut [Resource],
    registry: &SchemaRegistry,
) -> Vec<BlockNameConflict> {
    resources
        .iter_mut()
        .filter_map(|resource| {
            let schema = registry.get_for(resource)?;
            Some(resolve_attribute_block_names(
                &resource.id,
                &mut resource.attributes,
                None,
                schema.as_schema(),
            ))
        })
        .flatten()
        .collect()
}

fn resolve_block_names_detailed_with_authored(
    resources: &mut [Resource],
    authored: &[Resource],
    registry: &SchemaRegistry,
) -> Vec<BlockNameConflict> {
    resources
        .iter_mut()
        .filter_map(|resource| {
            let schema = registry.get_for(resource)?;
            let authored_attributes = authored
                .iter()
                .find(|candidate| candidate.id == resource.id)
                .map(|candidate| &candidate.attributes);
            Some(resolve_attribute_block_names(
                &resource.id,
                &mut resource.attributes,
                authored_attributes,
                schema.as_schema(),
            ))
        })
        .flatten()
        .collect()
}

#[cfg(test)]
fn resolve_block_names(
    resources: &mut [Resource],
    registry: &SchemaRegistry,
) -> Result<(), String> {
    let conflicts = resolve_block_names_detailed(resources, registry);
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(conflicts
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

fn resolve_file_block_names<E>(
    parsed: &mut crate::parser::File<E>,
    registry: &SchemaRegistry,
) -> Vec<BlockNameConflict> {
    let mut conflicts = resolve_block_names_detailed(&mut parsed.resources, registry);
    for data_source in &mut parsed.data_sources {
        if let Some(schema) = registry.get_for_data_source(data_source) {
            conflicts.extend(resolve_attribute_block_names(
                &data_source.id,
                &mut data_source.attributes,
                None,
                schema.as_schema(),
            ));
        }
    }
    for deferred in &mut parsed.parsed_deferred_for_expressions {
        match &mut deferred.body {
            crate::parser::DeferredForBody::Resource(resource) => {
                conflicts.extend(resolve_block_names_detailed(
                    std::slice::from_mut(resource.as_mut()),
                    registry,
                ));
            }
            // These bodies never cross the module-resolution seam, so block
            // aliases cannot affect a materialized resource. Their typed body
            // error remains the authoritative diagnostic.
            crate::parser::DeferredForBody::Read(_)
            | crate::parser::DeferredForBody::ModuleCall(_) => {}
        }
    }
    // This function is also part of the post-resolution schema API. Synthetic
    // resolved fixtures and finalized production files carry only this arm;
    // raw parser output leaves it empty.
    for deferred in &mut parsed.deferred_for_expressions {
        conflicts.extend(resolve_block_names_detailed(
            std::slice::from_mut(&mut deferred.template_resource),
            registry,
        ));
        deferred.attributes = deferred
            .template_resource
            .attributes
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
    }
    conflicts
}

fn deferred_resource_by_id<'a, E>(
    parsed: &'a crate::parser::File<E>,
    id: &crate::resource::ResourceId,
) -> Option<&'a Resource> {
    parsed
        .parsed_deferred_for_expressions
        .iter()
        .find_map(|deferred| match &deferred.body {
            crate::parser::DeferredForBody::Resource(resource) if resource.id == *id => {
                Some(resource.as_ref())
            }
            crate::parser::DeferredForBody::Resource(_)
            | crate::parser::DeferredForBody::Read(_)
            | crate::parser::DeferredForBody::ModuleCall(_) => None,
        })
        .or_else(|| {
            parsed
                .deferred_for_expressions
                .iter()
                .find(|deferred| deferred.template_resource.id == *id)
                .map(|deferred| &deferred.template_resource)
        })
}

fn resolve_file_block_names_with_authored<E, A>(
    parsed: &mut crate::parser::File<E>,
    authored: &crate::parser::File<A>,
    registry: &SchemaRegistry,
) -> Vec<BlockNameConflict> {
    let mut conflicts = resolve_block_names_detailed_with_authored(
        &mut parsed.resources,
        &authored.resources,
        registry,
    );
    for data_source in &mut parsed.data_sources {
        if let Some(schema) = registry.get_for_data_source(data_source) {
            let authored_attributes = authored
                .data_sources
                .iter()
                .find(|candidate| candidate.id == data_source.id)
                .map(|candidate| &candidate.attributes);
            conflicts.extend(resolve_attribute_block_names(
                &data_source.id,
                &mut data_source.attributes,
                authored_attributes,
                schema.as_schema(),
            ));
        }
    }
    for deferred in &mut parsed.parsed_deferred_for_expressions {
        match &mut deferred.body {
            crate::parser::DeferredForBody::Resource(resource) => {
                let authored_resources = deferred_resource_by_id(authored, &resource.id)
                    .map(std::slice::from_ref)
                    .unwrap_or_default();
                conflicts.extend(resolve_block_names_detailed_with_authored(
                    std::slice::from_mut(resource.as_mut()),
                    authored_resources,
                    registry,
                ));
            }
            // Read and module-call bodies are rejected before they can enter
            // the canonical evaluated/authored resource pair.
            crate::parser::DeferredForBody::Read(_)
            | crate::parser::DeferredForBody::ModuleCall(_) => {}
        }
    }
    for deferred in &mut parsed.deferred_for_expressions {
        let authored_resources = deferred_resource_by_id(authored, &deferred.template_resource.id)
            .map(std::slice::from_ref)
            .unwrap_or_default();
        conflicts.extend(resolve_block_names_detailed_with_authored(
            std::slice::from_mut(&mut deferred.template_resource),
            authored_resources,
            registry,
        ));
        deferred.attributes = deferred
            .template_resource
            .attributes
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
    }
    conflicts
}

/// Resolve block names in one evaluated file, including deferred-for
/// templates that can materialize after refresh.
pub fn resolve_file_block_names_with_schemas<E>(
    parsed: &mut crate::parser::File<E>,
    registry: &SchemaRegistry,
) -> Result<(), Vec<BlockNameConflict>> {
    let conflicts = resolve_file_block_names(parsed, registry);
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(conflicts)
    }
}

/// Resolve block names in an evaluated configuration and its authored
/// snapshot in one schema-aware operation.
///
/// This raw primitive returns plain values and carries no pairing guarantee.
/// Only carina-cli's validated-wiring transition may mint its canonical
/// resource pair from these values.
pub fn resolve_config_block_names<E>(
    mut parsed: crate::parser::File<E>,
    unresolved: UnresolvedConfigSnapshot,
    registry: &SchemaRegistry,
) -> Result<(crate::parser::File<E>, Vec<Resource>), Vec<BlockNameConflict>> {
    let mut unresolved = unresolved.into_inner();
    let mut conflicts = resolve_file_block_names_with_authored(&mut parsed, &unresolved, registry);
    let mut seen = conflicts.iter().cloned().collect::<HashSet<_>>();
    for conflict in resolve_file_block_names(&mut unresolved, registry) {
        if seen.insert(conflict.clone()) {
            conflicts.push(conflict);
        }
    }
    if conflicts.is_empty() {
        Ok((parsed, unresolved.resources))
    } else {
        Err(conflicts)
    }
}

/// Provider-agnostic types only. AWS-specific types (arn, aws_resource_id,
/// availability_zone, etc.) belong in provider crates.
/// See carina-provider-awscc/src/schemas/generated/mod.rs for AWS types.
pub mod types {
    use super::*;

    /// Positive integer type
    pub fn positive_int() -> AttributeType {
        AttributeType::refined_int_with_validator(
            TypeIdentity::bare("PositiveInt"),
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
        )
    }

    /// IPv4 CIDR block type (e.g., "10.0.0.0/16")
    pub fn ipv4_cidr() -> AttributeType {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("Ipv4Cidr"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_ipv4_cidr(s.expose_plaintext())
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// IPv4 address type (e.g., "10.0.1.5", "192.168.0.1")
    pub fn ipv4_address() -> AttributeType {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("Ipv4Address"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_ipv4_address(s.expose_plaintext())
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// IPv6 address type (e.g., "2001:db8::1", "::1")
    pub fn ipv6_address() -> AttributeType {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("Ipv6Address"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_ipv6_address(s.expose_plaintext())
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// IPv6 CIDR block type (e.g., "2001:db8::/32", "::/0")
    pub fn ipv6_cidr() -> AttributeType {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("Ipv6Cidr"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_ipv6_cidr(s.expose_plaintext())
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
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("Email"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_email(s.expose_plaintext())
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// HTTP response status code restricted to 2XX/4XX/5XX (3-digit string).
    ///
    /// Informational (1XX) and redirect (3XX) codes are excluded by design —
    /// this matches the constraint AWS documents for ELBv2 fixed-response
    /// actions. A future generic `http_status_code()` covering 1XX–5XX can be
    /// added as a sibling if a non-restricted consumer appears.
    pub fn http_response_status_code() -> AttributeType {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("HttpResponseStatusCode"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_http_response_status_code(s.expose_plaintext())
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// ELBv2 redirect protocol restricted to `HTTP`, `HTTPS`, or `#{protocol}`.
    ///
    /// ```
    /// # use carina_core::schema::validate_redirect_protocol;
    /// assert!(validate_redirect_protocol("HTTPS").is_ok());
    /// assert!(validate_redirect_protocol("#{protocol}").is_ok());
    /// assert!(validate_redirect_protocol("#{path}").is_err());
    /// ```
    pub fn redirect_protocol() -> AttributeType {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("RedirectProtocol"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_redirect_protocol(s.expose_plaintext())
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// ELBv2 redirect host, including wildcards and `#{host}`.
    ///
    /// ```
    /// # use carina_core::schema::validate_redirect_host;
    /// assert!(validate_redirect_host("example.com").is_ok());
    /// assert!(validate_redirect_host("#{path}").is_err());
    /// ```
    pub fn redirect_host() -> AttributeType {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("RedirectHost"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_redirect_host(s.expose_plaintext())
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// ELBv2 redirect port restricted to `1..=65535` or `#{port}`.
    ///
    /// ```
    /// # use carina_core::schema::validate_redirect_port;
    /// assert!(validate_redirect_port("443").is_ok());
    /// assert!(validate_redirect_port("#{path}").is_err());
    /// ```
    pub fn redirect_port() -> AttributeType {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("RedirectPort"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_redirect_port(s.expose_plaintext())
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// ELBv2 redirect path, including wildcards and ELBv2 redirect placeholders.
    ///
    /// ```
    /// # use carina_core::schema::validate_redirect_path;
    /// assert!(validate_redirect_path("/#{host}/#{path}").is_ok());
    /// assert!(validate_redirect_path("relative").is_err());
    /// ```
    pub fn redirect_path() -> AttributeType {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("RedirectPath"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_redirect_path(s.expose_plaintext())
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }

    /// ELBv2 redirect query string, including ELBv2 redirect placeholders.
    ///
    /// ```
    /// # use carina_core::schema::validate_redirect_query;
    /// assert!(validate_redirect_query("next=#{path}&q=#{query}").is_ok());
    /// assert!(validate_redirect_query(&"a".repeat(129)).is_err());
    /// ```
    pub fn redirect_query() -> AttributeType {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("RedirectQuery"),
            None,
            None,
            legacy_validator(|value| {
                if let Value::Concrete(ConcreteValue::String(s)) = value {
                    validate_redirect_query(s.expose_plaintext())
                } else {
                    Err("Expected string".to_string())
                }
            }),
            None,
        )
    }
}

/// Primitive shape attached to a core-owned identified refinement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefinedScalarKind {
    String,
    Int,
    Float,
}

impl fmt::Display for RefinedScalarKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::String => "String",
            Self::Int => "Int",
            Self::Float => "Float",
        })
    }
}

/// Return the validators for every core-owned identified refinement.
///
/// The registry is built from the same constructors providers use, so schema
/// construction and process-boundary reconstruction cannot drift onto two
/// hand-maintained identity lists.
pub fn builtin_validator_registry()
-> &'static HashMap<(RefinedScalarKind, TypeIdentity), CustomValidator> {
    static REGISTRY: OnceLock<HashMap<(RefinedScalarKind, TypeIdentity), CustomValidator>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| {
        let builtins = [
            types::positive_int(),
            types::ipv4_cidr(),
            types::ipv4_address(),
            types::ipv6_address(),
            types::ipv6_cidr(),
            types::email(),
            types::http_response_status_code(),
            types::redirect_protocol(),
            types::redirect_host(),
            types::redirect_port(),
            types::redirect_path(),
            types::redirect_query(),
        ];
        let mut validators = HashMap::new();
        for attr_type in builtins {
            let (kind, binding) = match &attr_type.kind {
                AttrTypeKind::String { binding, .. } => (RefinedScalarKind::String, binding),
                AttrTypeKind::Int { binding, .. } => (RefinedScalarKind::Int, binding),
                AttrTypeKind::Float { binding, .. } => (RefinedScalarKind::Float, binding),
                _ => continue,
            };
            if let Some((identity, callback)) = binding.validator_parts() {
                validators.insert((kind, identity.clone()), callback.clone());
            }
        }
        validators
    })
}

pub fn builtin_validator(
    kind: RefinedScalarKind,
    identity: &TypeIdentity,
) -> Option<CustomValidator> {
    builtin_validator_registry()
        .get(&(kind, identity.clone()))
        .cloned()
}

/// Validate an IPv4 address (e.g., "10.0.1.5", "192.168.0.1")
pub fn validate_ipv4_address(ip: &str) -> Result<(), String> {
    let octets: Vec<&str> = ip.split('.').collect();
    if octets.len() != 4 {
        return Err(format!(
            "Invalid IPv4 address {}: expected 4 octets",
            display_string_literal(ip)
        ));
    }

    for octet in &octets {
        match octet.parse::<u8>() {
            Ok(_) => {}
            Err(_) => {
                return Err(format!(
                    "Invalid octet {} in IPv4 address: must be 0-255",
                    display_string_literal(octet)
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
            "Invalid CIDR format {}: expected IP/prefix",
            display_string_literal(cidr)
        ));
    }

    let ip = parts[0];
    let prefix = parts[1];

    // Validate IP address
    validate_ipv4_address(ip)?;

    // Validate prefix length
    match prefix.parse::<u8>() {
        Ok(p) if p <= 32 => Ok(()),
        Ok(p) => Err(format!(
            "Invalid prefix length {}: must be 0-32",
            display_string_literal(&p.to_string())
        )),
        Err(_) => Err(format!(
            "Invalid prefix length {}: must be a number",
            display_string_literal(prefix)
        )),
    }
}

/// Validate IPv6 CIDR block format (e.g., "2001:db8::/32", "::/0")
pub fn validate_ipv6_cidr(cidr: &str) -> Result<(), String> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return Err(format!(
            "Invalid IPv6 CIDR format {}: expected address/prefix",
            display_string_literal(cidr)
        ));
    }

    let addr = parts[0];
    let prefix = parts[1];

    // Validate IPv6 address
    validate_ipv6_address(addr)?;

    // Validate prefix length (0-128)
    match prefix.parse::<u8>() {
        Ok(p) if p <= 128 => Ok(()),
        Ok(p) => Err(format!(
            "Invalid IPv6 prefix length {}: must be 0-128",
            display_string_literal(&p.to_string())
        )),
        Err(_) => Err(format!(
            "Invalid IPv6 prefix length {}: must be a number",
            display_string_literal(prefix)
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
            return Err(format!(
                "Invalid IPv6 address {}: malformed '::'",
                display_string_literal(addr)
            ));
        }

        // Check for multiple ::
        if halves[1].contains("::") {
            return Err(format!(
                "Invalid IPv6 address {}: only one '::' allowed",
                display_string_literal(addr)
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
                "Invalid IPv6 address {}: too many groups with '::'",
                display_string_literal(addr)
            ));
        }

        for group in left_groups.iter().chain(right_groups.iter()) {
            validate_ipv6_group(group, addr)?;
        }
    } else {
        let groups: Vec<&str> = addr.split(':').collect();
        if groups.len() != 8 {
            return Err(format!(
                "Invalid IPv6 address {}: expected 8 groups, got {}",
                display_string_literal(addr),
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
            "Invalid email {}: expected exactly one '@'",
            display_string_literal(email)
        ));
    }

    let local = parts[0];
    let domain = parts[1];

    if local.is_empty() {
        return Err(format!(
            "Invalid email {}: empty local part",
            display_string_literal(email)
        ));
    }
    if local.chars().any(char::is_whitespace) {
        return Err(format!(
            "Invalid email {}: local part contains whitespace",
            display_string_literal(email)
        ));
    }

    if domain.is_empty() {
        return Err(format!(
            "Invalid email {}: empty domain",
            display_string_literal(email)
        ));
    }
    if !domain.contains('.') {
        return Err(format!(
            "Invalid email {}: domain must contain at least one dot",
            display_string_literal(email)
        ));
    }

    for label in domain.split('.') {
        if label.is_empty() {
            return Err(format!(
                "Invalid email {}: domain has empty label",
                display_string_literal(email)
            ));
        }
        if label.chars().any(char::is_whitespace) {
            return Err(format!(
                "Invalid email {}: domain label contains whitespace",
                display_string_literal(email)
            ));
        }
    }

    Ok(())
}

/// Validate a 3-digit HTTP response status code restricted to 2XX/4XX/5XX.
pub fn validate_http_response_status_code(code: &str) -> Result<(), String> {
    if code.len() != 3 {
        return Err(format!(
            "must be exactly 3 digits, got {} characters",
            code.len()
        ));
    }
    if !code.chars().all(|c| c.is_ascii_digit()) {
        return Err("must contain only digits".to_string());
    }
    let first = code.chars().next().expect("length checked above");
    if !matches!(first, '2' | '4' | '5') {
        return Err(format!(
            "must start with 2, 4, or 5 (got {})",
            display_string_literal(&first.to_string())
        ));
    }
    Ok(())
}

/// Validate an ELBv2 redirect protocol value.
pub fn validate_redirect_protocol(protocol: &str) -> Result<(), String> {
    if matches!(protocol, "HTTP" | "HTTPS" | "#{protocol}") {
        return Ok(());
    }
    Err(format!(
        "Invalid redirect protocol {}: expected HTTP, HTTPS, or #{{protocol}}",
        display_string_literal(protocol)
    ))
}

/// Validate an ELBv2 redirect host value.
pub fn validate_redirect_host(host: &str) -> Result<(), String> {
    validate_char_length(host, 1, 128, "redirect host")?;
    validate_known_placeholders(host, &["#{host}"], "redirect host")
}

/// Validate an ELBv2 redirect port value.
pub fn validate_redirect_port(port: &str) -> Result<(), String> {
    if port == "#{port}" {
        return Ok(());
    }
    if port.starts_with('+') || (port.len() > 1 && port.starts_with('0')) {
        return Err(format!(
            "Invalid redirect port {}: expected decimal integer or #{{port}}",
            display_string_literal(port)
        ));
    }
    match port.parse::<u32>() {
        Ok(value) if (1..=65535).contains(&value) => Ok(()),
        Ok(value) => Err(format!(
            "Invalid redirect port {}: expected 1..=65535, got {value}",
            display_string_literal(port)
        )),
        Err(_) => Err(format!(
            "Invalid redirect port {}: expected decimal integer or #{{port}}",
            display_string_literal(port)
        )),
    }
}

/// Validate an ELBv2 redirect path value.
pub fn validate_redirect_path(path: &str) -> Result<(), String> {
    validate_char_length(path, 1, 128, "redirect path")?;
    if !path.starts_with('/') {
        return Err("Invalid redirect path: must start with '/'".to_string());
    }
    let placeholders = ["#{host}", "#{port}", "#{path}"];
    validate_known_placeholders(path, &placeholders, "redirect path")
}

/// Validate an ELBv2 redirect query value.
pub fn validate_redirect_query(query: &str) -> Result<(), String> {
    validate_char_length(query, 0, 128, "redirect query")?;
    validate_known_placeholders(
        query,
        &["#{protocol}", "#{host}", "#{port}", "#{path}", "#{query}"],
        "redirect query",
    )
}

fn validate_char_length(value: &str, min: usize, max: usize, field: &str) -> Result<(), String> {
    let len = value.chars().count();
    if len < min || len > max {
        return Err(format!(
            "Invalid {field}: expected {min}..={max} characters, got {len}"
        ));
    }
    Ok(())
}

fn validate_known_placeholders(value: &str, allowed: &[&str], field: &str) -> Result<(), String> {
    let mut rest = value;
    while let Some(start) = rest.find("#{") {
        let candidate = &rest[start..];
        let Some(end) = candidate.find('}') else {
            return Err(format!("Invalid {field}: malformed placeholder"));
        };
        let placeholder = &candidate[..=end];
        if !allowed.contains(&placeholder) {
            return Err(format!(
                "Invalid {field}: placeholder {placeholder} is not allowed"
            ));
        }
        rest = &candidate[end + 1..];
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
            "Invalid IPv6 group {} in address {}: must be 1-4 hex digits",
            display_string_literal(group),
            display_string_literal(addr)
        ));
    }
    if !group.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid IPv6 group {} in address {}: must be hex digits",
            display_string_literal(group),
            display_string_literal(addr)
        ));
    }
    Ok(())
}

// --- SchemaRegistry ---

/// A validated resource schema inseparably paired with the provider owner
/// under which it was registered.
///
/// The fields and constructor are private: only [`SchemaRegistry`] lookups can
/// produce this routing capability. Callers therefore cannot walk one schema
/// while independently supplying another provider name to custom-type
/// validation.
///
/// Resolver-backed validation is deliberately unavailable on an unbound
/// witness, so callers cannot pair one provider's lookup with another
/// provider's schema:
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::schema::{ResourceSchema, no_lookup};
///
/// let schema = ResourceSchema::new("test.Widget").validate().unwrap();
/// let _ = schema.validate_with_origins_and_lookup(
///     &HashMap::new(),
///     &|_| false,
///     no_lookup(),
/// );
/// ```
///
/// An unbound witness also has no public structural-only validation shortcut:
///
/// ```compile_fail
/// use std::collections::HashMap;
/// use carina_core::schema::ResourceSchema;
///
/// let schema = ResourceSchema::new("test.Widget").validate().unwrap();
/// let _ = schema.validate(&HashMap::new());
/// ```
///
/// Nor can external callers install an approve-all lookup for provider config:
///
/// ```compile_fail
/// use carina_core::schema::no_lookup;
///
/// let _ = no_lookup();
/// ```
///
/// A rootless config type likewise cannot accept an arbitrary lookup:
///
/// ```compile_fail
/// use std::collections::BTreeMap;
/// use carina_core::resource::{ConcreteValue, Value};
/// use carina_core::schema::{AttributeType, ValidatedConfigSchema};
///
/// let config = ValidatedConfigSchema::validate(BTreeMap::from([
///     ("name".to_string(), AttributeType::string()),
/// ])).unwrap();
/// let rootless = config.rootless_type("name").unwrap();
/// let value = Value::Concrete(ConcreteValue::String("ok".into()));
/// let _ = rootless.validate_with_lookup(&value, &|_, _| Ok(()));
/// ```
#[derive(Debug, Clone, Copy)]
pub struct OwnerBoundResourceSchema<'a> {
    owner: &'a str,
    schema: &'a ValidatedResourceSchema,
}

impl<'a> OwnerBoundResourceSchema<'a> {
    fn new(owner: &'a str, schema: &'a ValidatedResourceSchema) -> Self {
        Self { owner, schema }
    }

    pub fn owner(self) -> &'a str {
        self.owner
    }

    pub fn schema(self) -> &'a ValidatedResourceSchema {
        self.schema
    }

    /// Validate authored resource values with custom-type routing derived from
    /// this schema's registered owner.
    pub fn validate_with_resolver(
        self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
        resolver: &dyn crate::parser::CustomTypeResolver,
    ) -> Result<(), Vec<TypeError>> {
        let lookup = crate::parser::custom_type_lookup_for_schema(resolver, self);
        self.schema
            .validate_with_origins_and_lookup(attributes, is_string_literal, &lookup)
    }

    /// Validate every concrete value known at the provider boundary, routing
    /// identified declarations through this schema's registered owner.
    pub fn validate_known_values_with_resolver(
        self,
        attributes: &HashMap<String, Value>,
        is_string_literal: &dyn Fn(&str) -> bool,
        resolver: &dyn crate::parser::CustomTypeResolver,
    ) -> Result<(), Vec<TypeError>> {
        let lookup = crate::parser::custom_type_lookup_for_schema(resolver, self);
        self.schema.validate_known_values_with_origins_and_lookup(
            attributes,
            is_string_literal,
            &lookup,
        )
    }

    pub(crate) fn validate_selected_known_values_with_resolver(
        self,
        attributes: &HashMap<String, Value>,
        selected: &HashSet<String>,
        is_string_literal: &dyn Fn(&str) -> bool,
        resolver: &dyn crate::parser::CustomTypeResolver,
    ) -> Result<(), Vec<TypeError>> {
        let lookup = crate::parser::custom_type_lookup_for_schema(resolver, self);
        self.schema
            .validate_selected_known_values_with_origins_and_lookup(
                attributes,
                selected,
                is_string_literal,
                &lookup,
            )
    }
}

impl std::ops::Deref for OwnerBoundResourceSchema<'_> {
    type Target = ValidatedResourceSchema;

    fn deref(&self) -> &Self::Target {
        self.schema
    }
}

/// A registry that holds resource schemas keyed by `(provider, resource_type)`
/// and `SchemaKind`. The same `(provider, resource_type)` may have **two
/// independent entries** — a `Managed` one and a `DataSource` one — so that
/// a type like `aws.s3.Bucket` can be used both for new-resource creation
/// and for `read`-keyword lookup of existing infrastructure.
///
/// See `notes/specs/2026-05-02-resource-vs-data-source-design.md` (Decision 1-2).
#[derive(Debug, Clone, Default)]
pub struct SchemaRegistry {
    managed: HashMap<(String, String), ValidatedResourceSchema>,
    data_sources: HashMap<(String, String), ValidatedResourceSchema>,
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a validated schema under the given provider. The `kind` field
    /// on the schema decides which sub-map it goes into. Raw drafts cannot
    /// cross this admission boundary:
    ///
    /// ```compile_fail
    /// use carina_core::schema::{ResourceSchema, SchemaRegistry};
    ///
    /// let mut registry = SchemaRegistry::new();
    /// registry.insert("test", ResourceSchema::new("test.Widget"));
    /// ```
    pub fn insert(&mut self, provider: impl Into<String>, schema: ValidatedResourceSchema) {
        let key = (provider.into(), schema.as_schema().resource_type.clone());
        match schema.as_schema().kind {
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
    ) -> Option<OwnerBoundResourceSchema<'_>> {
        let key = (provider.to_string(), resource_type.to_string());
        let map = match kind {
            SchemaKind::Resource => &self.managed,
            SchemaKind::DataSource => &self.data_sources,
        };
        map.get_key_value(&key)
            .map(|((owner, _resource_type), schema)| OwnerBoundResourceSchema::new(owner, schema))
    }

    /// Look up the `Managed` schema for a given [`Resource`].
    pub fn get_for<S>(
        &self,
        resource: &crate::resource::Resource<S>,
    ) -> Option<OwnerBoundResourceSchema<'_>> {
        self.get(
            &resource.id.provider,
            &resource.id.resource_type,
            SchemaKind::Resource,
        )
    }

    /// Look up the `DataSource` schema for a given [`DataSource`].
    pub fn get_for_data_source<S>(
        &self,
        resource: &crate::resource::DataSource<S>,
    ) -> Option<OwnerBoundResourceSchema<'_>> {
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

    /// Iterate every schema in the registry without discarding its validation
    /// witness. Consumers that need the raw conversion view must opt in with
    /// [`ValidatedResourceSchema::as_schema`].
    pub fn iter(
        &self,
    ) -> impl Iterator<Item = (&str, &str, SchemaKind, &ValidatedResourceSchema)> + '_ {
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
