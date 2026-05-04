//! Schema - Define type schemas for resources
//!
//! Providers define schemas for each resource type,
//! enabling type validation at parse time.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::resource::{Resource, Value};
use crate::utils::{extract_enum_value_with_values, validate_enum_namespace};
use crate::value::format_value_with_key;

/// Type alias for resource validator functions
pub type ResourceValidator = fn(&HashMap<String, Value>) -> Result<(), Vec<TypeError>>;

/// Validator stored on `AttributeType::Custom`. Boxed as `Arc<dyn Fn>` so it
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

/// A [`CustomValidator`] that accepts every value. Useful for tests and
/// for placeholder Custom types whose validation is delegated elsewhere
/// (e.g. plugin-loaded providers route validation through
/// `ProviderContext.validators`).
pub fn noop_validator() -> CustomValidator {
    validator(|_| Ok(()))
}

/// External validator looked up by `AttributeType::Custom.semantic_name`
/// at validation time. Used to bridge to provider-supplied validators
/// (the `ProviderContext.validators` map and WASM factory's
/// `validate_custom_type`) that the schema itself cannot carry across
/// the WASM boundary — see #2354.
///
/// Implementors normalize the type name as needed (typically PascalCase
/// → snake_case via `parser::pascal_to_snake`) before looking up the
/// real validator.
pub type CustomTypeLookup<'a> =
    &'a (dyn Fn(&str, &Value) -> Result<(), TypeError> + Send + Sync + 'a);

/// A [`CustomTypeLookup`] that approves every value. Pass to
/// `validate_with_origins_and_lookup` from contexts that have no
/// `ProviderContext` (snapshot tests, schema unit tests). The
/// schema-attached `validate` closure still runs for built-in
/// validators registered directly on the type.
pub fn no_lookup() -> CustomTypeLookup<'static> {
    &|_name, _value| Ok(())
}

/// Walk an [`AttributeType`] and apply `lookup` to every `Custom` node
/// reached. Pushes any returned error into `errors`, tagged with
/// `attr_name` so it points back at the user-visible attribute. Used
/// by `ResourceSchema::validate_inner` to bridge provider-supplied
/// validators that the schema's own closure cannot carry (e.g. WASM
/// plugins, where the schema arrives with `noop_validator()` and the
/// real validator lives behind the factory's `validate_custom_type`).
fn walk_custom_lookup(
    attr_type: &AttributeType,
    value: &Value,
    attr_name: &str,
    lookup: CustomTypeLookup<'_>,
    errors: &mut Vec<TypeError>,
) {
    // Skip deferred-resolution values — same convention as
    // `AttributeType::validate`, plus `ResourceRef` / `Interpolation`
    // which only resolve to a concrete string at apply time.
    if matches!(
        value,
        Value::FunctionCall { .. }
            | Value::Secret(_)
            | Value::ResourceRef { .. }
            | Value::Interpolation(_)
            | Value::Unknown(_)
    ) {
        return;
    }
    match attr_type {
        AttributeType::Custom { semantic_name, .. } => {
            if let Some(name) = semantic_name
                && let Err(e) = lookup(name, value)
            {
                // Re-wrap as `ResourceValidationFailed` so the attribute
                // slot survives. `with_attribute` only enriches the two
                // enum-variant errors; bare `ValidationFailed` would
                // arrive at the LSP without a position hint and get
                // filtered out by the resource-level dedup.
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
        AttributeType::List { inner, .. } => {
            if let Value::List(items) = value {
                for item in items {
                    walk_custom_lookup(inner, item, attr_name, lookup, errors);
                }
            }
        }
        AttributeType::Map { value: inner, .. } => {
            if let Value::Map(map) = value {
                for v in map.values() {
                    walk_custom_lookup(inner, v, attr_name, lookup, errors);
                }
            }
        }
        AttributeType::Struct { fields, .. } => {
            if let Value::Map(map) = value {
                for f in fields {
                    if let Some(v) = map.get(&f.name) {
                        walk_custom_lookup(&f.field_type, v, attr_name, lookup, errors);
                    }
                }
            }
        }
        AttributeType::Union(members) => {
            for member in members {
                walk_custom_lookup(member, value, attr_name, lookup, errors);
            }
        }
        AttributeType::String
        | AttributeType::Int
        | AttributeType::Float
        | AttributeType::Bool
        | AttributeType::StringEnum { .. } => {}
    }
}

pub type StringEnumParts<'a> = (
    &'a str,
    &'a [String],
    Option<&'a str>,
    Option<fn(&str) -> String>,
);
pub type NamespacedEnumParts<'a> = (&'a str, &'a str, Option<fn(&str) -> String>);

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
}

/// Attribute type
#[derive(Clone)]
pub enum AttributeType {
    /// String
    String,
    /// Integer
    Int,
    /// Floating-point number
    Float,
    /// Boolean
    Bool,
    /// String enum with optional namespace-aware DSL syntax support
    StringEnum {
        name: String,
        values: Vec<String>,
        namespace: Option<String>,
        to_dsl: Option<fn(&str) -> String>,
    },
    /// Custom type (with validation function)
    Custom {
        /// Some(name) when this type carries a semantic identity (e.g. "VpcId",
        /// "AwsAccountId"). None when this is a generic string/int pattern type
        /// synthesized by codegen for a property without a named semantic.
        semantic_name: Option<String>,
        base: Box<AttributeType>,
        /// Optional regex pattern constraint (for structural comparison).
        pattern: Option<String>,
        /// Optional length bounds (min, max).
        length: Option<(Option<u64>, Option<u64>)>,
        validate: CustomValidator,
        /// Namespace for resolving shorthand enum values (e.g., "aws.vpc")
        /// When set, allows `dedicated` to be resolved to `aws.vpc.InstanceTenancy.dedicated`
        namespace: Option<String>,
        /// Optional callback to normalize AWS values to DSL format.
        /// For example, availability_zone uses `|s| s.replace('-', "_")` to convert
        /// "ap-northeast-1a" to "ap_northeast_1a" for DSL identifier form.
        to_dsl: Option<fn(&str) -> String>,
    },
    /// List
    /// `ordered`: if true, element order matters (sequential comparison);
    /// if false, order is ignored (multiset comparison).
    /// Defaults to true (matching CloudFormation's insertionOrder default).
    List {
        inner: Box<AttributeType>,
        ordered: bool,
    },
    /// Map with typed keys and values.
    /// `key`: type constraint for map keys (e.g., `String` for unconstrained,
    /// `StringEnum` for condition operators).
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
}

impl fmt::Debug for AttributeType {
    // Hand-written so that `Custom.validate` (an `Arc<dyn Fn>`, which does
    // not implement `Debug`) can be rendered as a placeholder. Every other
    // variant matches what `#[derive(Debug)]` would produce.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttributeType::String => f.write_str("String"),
            AttributeType::Int => f.write_str("Int"),
            AttributeType::Float => f.write_str("Float"),
            AttributeType::Bool => f.write_str("Bool"),
            AttributeType::StringEnum {
                name,
                values,
                namespace,
                to_dsl,
            } => f
                .debug_struct("StringEnum")
                .field("name", name)
                .field("values", values)
                .field("namespace", namespace)
                .field("to_dsl", to_dsl)
                .finish(),
            AttributeType::Custom {
                semantic_name,
                base,
                pattern,
                length,
                namespace,
                to_dsl,
                validate: _,
            } => f
                .debug_struct("Custom")
                .field("semantic_name", semantic_name)
                .field("base", base)
                .field("pattern", pattern)
                .field("length", length)
                .field("namespace", namespace)
                .field("to_dsl", to_dsl)
                .field("validate", &"<closure>")
                .finish(),
            AttributeType::List { inner, ordered } => f
                .debug_struct("List")
                .field("inner", inner)
                .field("ordered", ordered)
                .finish(),
            AttributeType::Map { key, value } => f
                .debug_struct("Map")
                .field("key", key)
                .field("value", value)
                .finish(),
            AttributeType::Struct { name, fields } => f
                .debug_struct("Struct")
                .field("name", name)
                .field("fields", fields)
                .finish(),
            AttributeType::Union(types) => f.debug_tuple("Union").field(types).finish(),
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
/// a particular [`TypeError`]. Used by [`AttributeType::validate_collect`]
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

impl AttributeType {
    /// Create a List type with default ordering (ordered=true, matching CloudFormation default).
    pub fn list(inner: AttributeType) -> Self {
        AttributeType::List {
            inner: Box::new(inner),
            ordered: true,
        }
    }

    /// Create an unordered List type (insertionOrder=false).
    pub fn unordered_list(inner: AttributeType) -> Self {
        AttributeType::List {
            inner: Box::new(inner),
            ordered: false,
        }
    }

    /// Create a Map type with unconstrained string keys.
    pub fn map(value: AttributeType) -> Self {
        Self::map_with_key(AttributeType::String, value)
    }

    /// Create a Map type with a typed key constraint.
    pub fn map_with_key(key: AttributeType, value: AttributeType) -> Self {
        AttributeType::Map {
            key: Box::new(key),
            value: Box::new(value),
        }
    }

    fn resolve_enum_input(name: &str, namespace: Option<&str>, value: &Value) -> Value {
        if matches!(value, Value::ResourceRef { .. }) {
            return value.clone();
        }
        crate::utils::expand_enum_shorthand(value, name, namespace)
    }

    pub fn string_enum_parts(&self) -> Option<StringEnumParts<'_>> {
        match self {
            AttributeType::StringEnum {
                name,
                values,
                namespace,
                to_dsl,
            } => Some((name, values, namespace.as_deref(), *to_dsl)),
            _ => None,
        }
    }

    pub fn namespaced_enum_parts(&self) -> Option<NamespacedEnumParts<'_>> {
        match self {
            AttributeType::StringEnum {
                name,
                namespace: Some(namespace),
                to_dsl,
                ..
            }
            | AttributeType::Custom {
                semantic_name: Some(name),
                namespace: Some(namespace),
                to_dsl,
                ..
            } => Some((name, namespace, *to_dsl)),
            _ => None,
        }
    }

    /// Check if a value conforms to this type.
    ///
    /// Top-level dispatcher: each `AttributeType` variant has its own
    /// `validate_*` helper. Values that resolve at runtime
    /// (`Value::FunctionCall`, `Value::Secret`) bypass validation here so
    /// every per-variant helper can assume it is dealing with a concrete
    /// value (or one of the variant-specific dynamic shapes like
    /// `ResourceRef`/`Interpolation` that the helpers handle individually).
    pub fn validate(&self, value: &Value) -> Result<(), TypeError> {
        // Deferred-resolution values carry no concrete type at plan
        // time — skip them.
        if matches!(
            value,
            Value::FunctionCall { .. } | Value::Secret(_) | Value::Unknown(_)
        ) {
            return Ok(());
        }

        match self {
            AttributeType::StringEnum { .. } => self.validate_string_enum(value),
            AttributeType::Custom { .. } => self.validate_custom(value),
            AttributeType::List { .. } => self.validate_list(value),
            AttributeType::Map { .. } => self.validate_map(value),
            AttributeType::Struct { .. } => self.validate_struct(value),
            AttributeType::Union(_) => self.validate_union(value),
            AttributeType::String
            | AttributeType::Int
            | AttributeType::Float
            | AttributeType::Bool => self.validate_primitive(value),
        }
    }

    /// Validate a value and collect every error along with the
    /// [`FieldPath`] from the value's root to the offending location
    /// (#2214).
    ///
    /// This is the path-tagged sibling of [`Self::validate`]: where
    /// `validate` returns the first error wrapped in
    /// `StructFieldError` / `ListItemError` / `MapValueError` enclosure
    /// and stops, `validate_collect` keeps walking through every
    /// field of every nested struct (and every item of a list-of-struct)
    /// so a downstream tool like the LSP can surface every problem at
    /// once with positional information.
    ///
    /// For non-Struct, non-List<Struct> shapes the result is exactly
    /// one error (or none) — there is no nested context to descend
    /// into, so this method is no slower than `validate` for primitives.
    pub fn validate_collect(&self, value: &Value) -> Vec<(FieldPath, TypeError)> {
        let mut out = Vec::new();
        self.collect_into(&FieldPath::new(), value, &mut out);
        out
    }

    fn collect_into(&self, path: &FieldPath, value: &Value, out: &mut Vec<(FieldPath, TypeError)>) {
        // Same skip rule as `validate` — deferred-resolution values.
        if matches!(
            value,
            Value::FunctionCall { .. } | Value::Secret(_) | Value::Unknown(_)
        ) {
            return;
        }

        match self {
            AttributeType::Struct { name, fields } => {
                self.collect_struct(path, name, fields, value, out);
            }
            AttributeType::List { inner, .. } => {
                self.collect_list(path, inner, value, out);
            }
            // For everything else the existing single-shot validator is
            // already correct: it returns at most one error and there
            // is no nested structure to recurse into. Forward the error
            // (if any) under the current path.
            _ => {
                if let Err(e) = self.validate(value) {
                    out.push((path.clone(), e));
                }
            }
        }
    }

    fn collect_struct(
        &self,
        path: &FieldPath,
        name: &str,
        fields: &[StructField],
        value: &Value,
        out: &mut Vec<(FieldPath, TypeError)>,
    ) {
        // Block syntax → Value::List([Value::Map(...)])  for List<Struct>;
        // bare Struct rejects List with `BlockSyntaxNotAllowed`.
        if matches!(value, Value::List(_)) {
            out.push((
                path.clone(),
                TypeError::BlockSyntaxNotAllowed {
                    attribute: name.to_string(),
                },
            ));
            return;
        }
        let Value::Map(map) = value else {
            out.push((
                path.clone(),
                TypeError::TypeMismatch {
                    expected: self.type_name(),
                    got: value.type_name(),
                },
            ));
            return;
        };

        // Map every accepted name (canonical + block_name alias) to the
        // canonical `StructField`. Pre-#2214 the LSP did this alias
        // resolution itself; now the core validator is the single
        // source of truth and `validate_struct` shares the same map
        // (#2214 reconciliation).
        let accepted = build_accepted_field_map(fields);
        let canonical_field_names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();

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

        // Each present field — descend (for nested struct/list of
        // struct) or run the leaf validator.
        for (k, v) in map {
            match accepted.get(k.as_str()) {
                Some(field) => {
                    let next_path = path.push_field(k.clone());
                    // ResourceRef / Interpolation values are placeholders
                    // resolved at apply time — skip type checking but
                    // still descend into ResourceRef-free shapes.
                    if matches!(v, Value::ResourceRef { .. }) {
                        continue;
                    }
                    field.field_type.collect_into(&next_path, v, out);
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

    fn collect_list(
        &self,
        path: &FieldPath,
        inner: &AttributeType,
        value: &Value,
        out: &mut Vec<(FieldPath, TypeError)>,
    ) {
        let Value::List(items) = value else {
            out.push((
                path.clone(),
                TypeError::TypeMismatch {
                    expected: self.type_name(),
                    got: value.type_name(),
                },
            ));
            return;
        };
        // For List<Struct> we want per-item descent so each item's
        // errors carry a `[i]` step in the path. For plain List<T>
        // this still works — each item runs the leaf validator under
        // the indexed path.
        for (i, item) in items.iter().enumerate() {
            inner.collect_into(&path.push_index(i), item, out);
        }
    }

    /// Validate a primitive (`String`/`Int`/`Float`/`Bool`) value.
    /// `ResourceRef` and `Interpolation` resolve to strings at runtime and
    /// are accepted for `String`. `Float` accepts integers as valid numbers
    /// and rejects non-finite floats explicitly.
    fn validate_primitive(&self, value: &Value) -> Result<(), TypeError> {
        match (self, value) {
            (
                AttributeType::String,
                Value::String(_) | Value::ResourceRef { .. } | Value::Interpolation(_),
            ) => Ok(()),
            (AttributeType::Int, Value::Int(_)) => Ok(()),
            (AttributeType::Float, Value::Float(f)) if f.is_finite() => Ok(()),
            (AttributeType::Float, Value::Float(f)) => Err(TypeError::ValidationFailed {
                message: format!("non-finite float value: {f}"),
            }),
            (AttributeType::Float, Value::Int(_)) => Ok(()), // integers are valid numbers
            (AttributeType::Bool, Value::Bool(_)) => Ok(()),
            _ => Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: value.type_name(),
            }),
        }
    }

    /// Validate against a `StringEnum` variant.
    ///
    /// Panics if `self` is not a `StringEnum` — only called from the
    /// top-level dispatcher.
    fn validate_string_enum(&self, value: &Value) -> Result<(), TypeError> {
        let AttributeType::StringEnum {
            name,
            values,
            namespace,
            to_dsl,
        } = self
        else {
            unreachable!("validate_string_enum called on non-StringEnum");
        };

        // Interpolation values resolve to strings at runtime, so accept them
        if matches!(value, Value::Interpolation(_)) {
            return Ok(());
        }
        let resolved_value = Self::resolve_enum_input(name, namespace.as_deref(), value);
        if matches!(resolved_value, Value::ResourceRef { .. }) {
            return Ok(());
        }
        // Capture the user's original input for diagnostics. The parser
        // collapses both quoted literals (`"aaa"`) and bare identifiers
        // (`dedicated`) into `Value::String`, and `resolve_enum_input`
        // rewrites the non-dotted form into a synthesized namespaced
        // string for lookup. That synthesized form must stay internal:
        // error messages should quote what the user actually typed.
        // See #2077.
        let user_input = match value {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        };
        if let Value::String(s) = &resolved_value {
            // Check if the raw string directly matches a valid enum value
            // before namespace validation. This handles values containing
            // dots (e.g., "ipsec.1") that would be misinterpreted as
            // namespace separators.
            let direct_match = values.iter().any(|v| string_enum_value_matches(s, v));
            let valid: Vec<&str> = values.iter().map(String::as_str).collect();
            let variant = if direct_match {
                s.as_str()
            } else {
                extract_enum_value_with_values(s, &valid)
            };

            // Non-direct matches must have the exact form
            // `{namespace}.{name}.{variant}`. This rejects malformed
            // inputs like double-namespaced values while still allowing
            // enum values that themselves contain dots (e.g., "ipsec.1").
            if !direct_match && let Some(ns) = namespace.as_deref() {
                let expected_prefix = format!("{}.{}.", ns, name);
                let prefix_matches =
                    s.starts_with(&expected_prefix) && &s[expected_prefix.len()..] == variant;
                if !prefix_matches {
                    // Fall back to strict namespace validation, which
                    // produces a clear error for the common bare form.
                    let user_form = user_input.unwrap_or(s.as_str());
                    validate_enum_namespace(s, name, ns).map_err(|message| {
                        TypeError::ValidationFailed {
                            message: format!("Invalid {} '{}': {}", name, user_form, message),
                        }
                    })?;
                }
            }
            let matches_canonical = values.iter().any(|v| string_enum_value_matches(variant, v));
            let matches_alias = to_dsl.is_some_and(|f| {
                values
                    .iter()
                    .any(|v| string_enum_value_matches(variant, &f(v)))
            });
            if matches_canonical || matches_alias {
                Ok(())
            } else {
                // Aliases produced by `to_dsl` are tagged `is_alias = true`
                // so LSP code actions can prefer the canonical form.
                let mut expected: Vec<ExpectedEnumVariant> = Vec::new();
                let mut push = |variant_value: &str, is_alias: bool| {
                    let entry = ExpectedEnumVariant::from_namespaced(
                        namespace.as_deref(),
                        name,
                        variant_value,
                        is_alias,
                    );
                    if !expected.contains(&entry) {
                        expected.push(entry);
                    }
                };
                for v in values {
                    push(v, false);
                }
                if let Some(f) = to_dsl {
                    for v in values {
                        let alias = f(v);
                        if alias != *v {
                            push(&alias, true);
                        }
                    }
                }
                Err(TypeError::InvalidEnumVariant {
                    value: user_input.unwrap_or(s.as_str()).to_string(),
                    attribute: None,
                    type_name: Some(name.clone()),
                    expected,
                })
            }
        } else {
            Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: resolved_value.type_name(),
            })
        }
    }

    /// Validate against a `Custom` variant by delegating to its `validate`
    /// closure after expanding any enum-shorthand identifier in `value`.
    fn validate_custom(&self, value: &Value) -> Result<(), TypeError> {
        let AttributeType::Custom {
            validate,
            semantic_name,
            namespace,
            ..
        } = self
        else {
            unreachable!("validate_custom called on non-Custom");
        };

        // ResourceRef and Interpolation values resolve to strings at runtime,
        // so they're valid for Custom types
        if matches!(value, Value::ResourceRef { .. } | Value::Interpolation(_)) {
            return Ok(());
        }
        let name_for_resolve = semantic_name.as_deref().unwrap_or("");
        let resolved_value =
            Self::resolve_enum_input(name_for_resolve, namespace.as_deref(), value);
        validate(&resolved_value)
    }

    /// Validate a `List` variant by validating each item with the inner type.
    fn validate_list(&self, value: &Value) -> Result<(), TypeError> {
        let AttributeType::List { inner, .. } = self else {
            unreachable!("validate_list called on non-List");
        };
        let Value::List(items) = value else {
            return Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: value.type_name(),
            });
        };
        for (i, item) in items.iter().enumerate() {
            inner.validate(item).map_err(|e| TypeError::ListItemError {
                index: i,
                inner: Box::new(e),
            })?;
        }
        Ok(())
    }

    /// Validate a `Map` variant: keys against `key`, values against `value`.
    fn validate_map(&self, value: &Value) -> Result<(), TypeError> {
        let AttributeType::Map {
            key: key_type,
            value: inner,
        } = self
        else {
            unreachable!("validate_map called on non-Map");
        };
        let Value::Map(map) = value else {
            return Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: value.type_name(),
            });
        };
        // Validate keys against key type
        for k in map.keys() {
            key_type
                .validate(&Value::String(k.clone()))
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
    /// Block syntax produces `Value::List([Value::Map(...)])`, but bare
    /// `Struct` requires map assignment syntax (`attr = { ... }`); a
    /// `Value::List` is rejected explicitly with `BlockSyntaxNotAllowed`.
    fn validate_struct(&self, value: &Value) -> Result<(), TypeError> {
        let AttributeType::Struct { name, fields } = self else {
            unreachable!("validate_struct called on non-Struct");
        };

        // Struct type rejects Value::List (block syntax)
        if matches!(value, Value::List(_)) {
            return Err(TypeError::BlockSyntaxNotAllowed {
                attribute: name.clone(),
            });
        }
        let Value::Map(map) = value else {
            return Err(TypeError::TypeMismatch {
                expected: self.type_name(),
                got: value.type_name(),
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
    /// the input's (Map↔Struct, List↔List, String↔StringEnum, scalar↔
    /// scalar) outscore unrelated members. The first member at the
    /// maximum score wins — declaration order is preserved by the
    /// strict `>` comparison below, so the prior Map↔Struct preference
    /// still holds when multiple Struct members tie. When no member
    /// shares any structural similarity, fall through to `TypeMismatch`.
    /// See #2219.
    fn validate_union(&self, value: &Value) -> Result<(), TypeError> {
        let AttributeType::Union(types) = self else {
            unreachable!("validate_union called on non-Union");
        };
        let mut best: Option<(u32, TypeError)> = None;
        for member in types {
            match member.validate(value) {
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
            got: value.type_name(),
        }))
    }

    pub fn type_name(&self) -> String {
        match self {
            AttributeType::String => "String".to_string(),
            AttributeType::Int => "Int".to_string(),
            AttributeType::Float => "Float".to_string(),
            AttributeType::Bool => "Bool".to_string(),
            AttributeType::StringEnum { name, .. } => name.clone(),
            AttributeType::Custom {
                semantic_name,
                pattern,
                length,
                ..
            } => custom_display_name(
                semantic_name.as_deref(),
                pattern.as_deref(),
                length.as_ref(),
            ),
            AttributeType::List { inner, .. } => format!("List<{}>", inner.type_name()),
            AttributeType::Map { value: inner, .. } => format!("Map<{}>", inner.type_name()),
            AttributeType::Struct { name, .. } => format!("Struct({})", name),
            AttributeType::Union(types) => {
                let names: Vec<String> = types.iter().map(|t| t.type_name()).collect();
                names.join(" | ")
            }
        }
    }

    /// Check if a type name is accepted by this type.
    /// For Union types, returns true if any member accepts the name.
    /// For other types, returns true if self.type_name() == name.
    pub fn accepts_type_name(&self, name: &str) -> bool {
        match self {
            AttributeType::Union(types) => types.iter().any(|t| t.accepts_type_name(name)),
            _ => self.type_name() == name,
        }
    }

    /// Check if this type is a String-based Custom type.
    /// Used for cross-schema type compatibility: all String-based Custom types
    /// are considered compatible with each other.
    pub fn is_string_based_custom(&self) -> bool {
        matches!(self, AttributeType::Custom { base, .. } if matches!(**base, AttributeType::String))
    }

    /// Check if a value of `self`'s type can be assigned to a sink of
    /// `sink`'s type. Directional: narrowing source → wider sink is OK,
    /// but widening source → narrower sink is NG.
    ///
    /// Rules (first match wins):
    /// 1. Union sink: OK if source is assignable to any member.
    /// 2. Union source: OK iff source is assignable to sink for every member.
    /// 3. Custom→Custom with both `semantic_name: Some` and names differ: NG.
    /// 4. Custom→Custom: check pattern (pat-1 literal equality) and length
    ///    containment (source ⊆ sink), then recurse on base.
    /// 5. Custom source → non-Custom sink: recurse on `source.base`.
    /// 6. non-Custom source → Custom sink: NG (source has no proof of
    ///    satisfying the sink's semantic/pattern/length).
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
        use AttributeType::*;
        if let Union(members) = sink {
            return members.iter().any(|m| self.is_assignable_to(m));
        }
        if let Union(members) = self {
            return members.iter().all(|m| m.is_assignable_to(sink));
        }
        match (self, sink) {
            (
                Custom {
                    semantic_name: Some(s_name),
                    ..
                },
                Custom {
                    semantic_name: Some(k_name),
                    ..
                },
            ) if s_name != k_name => false,
            // Anonymous source → semantic sink has no proof of identity.
            (
                Custom {
                    semantic_name: None,
                    ..
                },
                Custom {
                    semantic_name: Some(_),
                    ..
                },
            ) => false,
            (
                Custom {
                    pattern: s_pat,
                    length: s_len,
                    base: s_base,
                    ..
                },
                Custom {
                    pattern: k_pat,
                    length: k_len,
                    base: k_base,
                    ..
                },
            ) => {
                if let (Some(sp), Some(kp)) = (s_pat, k_pat) {
                    if sp != kp {
                        return false;
                    }
                } else if k_pat.is_some() && s_pat.is_none() {
                    return false;
                }
                if !length_contains(s_len.as_ref(), k_len.as_ref()) {
                    return false;
                }
                s_base.is_assignable_to(k_base)
            }
            (Custom { base, .. }, non_custom) => base.is_assignable_to(non_custom),
            (_non_custom, Custom { .. }) => false,
            (a, b) => a.type_name() == b.type_name(),
        }
    }
}

/// Rank a Union member against a runtime value by structural distance:
/// how close the member's outer constructor is to the input's. Higher
/// is closer; 0 means no shared structure. Used by `validate_union`
/// to pick which member's error message to surface on failure (#2219).
///
/// Map↔Struct stays the highest (preserves the prior heuristic). The
/// other constructor pairs — Map↔Map, List↔List, String↔String /
/// StringEnum, scalar↔scalar — get the next tier. `Custom` defers to
/// its declared `base`, so a Union of `Int | positive_int()`
/// validating an `Int` input still surfaces the predicate's message.
/// `Union` members recurse and take the best inner score so nested
/// unions still produce a meaningful error.
///
/// On a tie, the first member at the maximum wins — `validate_union`
/// uses strict `>` so declaration order is preserved.
fn union_member_score(member: &AttributeType, value: &Value) -> u32 {
    use AttributeType as AT;
    match (member, value) {
        // Map↔Struct: the original heuristic. Highest score so a
        // Struct member's "Unknown field 'x'" wins over a sibling's
        // generic `TypeMismatch`.
        (AT::Struct { .. }, Value::Map(_)) => 100,
        // Same-constructor match — second tier.
        (AT::Map { .. }, Value::Map(_))
        | (AT::String, Value::String(_))
        | (AT::Int, Value::Int(_))
        | (AT::Float, Value::Float(_))
        | (AT::Bool, Value::Bool(_))
        | (AT::StringEnum { .. }, Value::String(_)) => 80,
        // List↔List: peek at the first element's structural match
        // against the member's inner type so `List<Struct>` outranks
        // `List<String>` for an input like `[{...}]`. The inner
        // contribution is halved so arbitrarily deep nesting can't
        // exceed the Map↔Struct tier (100). Empty lists fall back to
        // the bare same-constructor score.
        (AT::List { inner, .. }, Value::List(items)) => {
            let inner_bonus = items
                .first()
                .map(|first| union_member_score(inner, first) / 2)
                .unwrap_or(0);
            80 + inner_bonus
        }
        // Custom defers to its `base`. A Custom with a Struct/Int/
        // String base ranks the same as that base would — its
        // validator's `ValidationFailed` message is then the one
        // `validate_union` surfaces on failure.
        (AT::Custom { base, .. }, v) => union_member_score(base, v),
        // Nested Union: recurse and take the best inner match.
        (AT::Union(inner), v) => inner
            .iter()
            .map(|m| union_member_score(m, v))
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}

/// Map every accepted field name to its canonical [`StructField`].
/// Both the canonical `name` and any `block_name` alias resolve to the
/// same field, so users can write either form interchangeably.
///
/// Used by both [`AttributeType::validate`] (single-shot) and
/// [`AttributeType::validate_collect`] (path-tagged) so the two paths
/// agree on which keys are accepted (#2214 — the LSP previously did
/// this alias resolution itself, which let the two validators drift).
fn build_accepted_field_map(fields: &[StructField]) -> HashMap<&str, &StructField> {
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

impl fmt::Display for AttributeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.type_name())
    }
}

fn custom_display_name(
    semantic_name: Option<&str>,
    pattern: Option<&str>,
    length: Option<&(Option<u64>, Option<u64>)>,
) -> String {
    if let Some(n) = semantic_name {
        return n.to_string();
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

fn string_enum_value_matches(input: &str, expected: &str) -> bool {
    input == expected
        || input.eq_ignore_ascii_case(expected)
        || input.replace('_', "-").eq_ignore_ascii_case(expected)
}

/// Render the `InvalidEnumVariant` message with the richest available
/// context. Presence of `attribute` and `type_name` is independent — both,
/// either, or neither may be set. `expected` is rendered as-is; callers are
/// responsible for passing fully-qualified variants for namespaced enums.
/// Reshape an error from `AttributeType::validate` into a shape-mismatch
/// diagnostic when the attribute's value came from a quoted string literal
/// and the schema expects an enum-shaped identifier (a `StringEnum`, or a
/// namespaced `Custom` type).
///
/// For `StringEnum`, `into_string_literal_diagnostic` does the work since
/// the underlying error already carries type name and variants. For a
/// namespaced `Custom`, validation returns `ValidationFailed { message }`
/// with no structured fields — we still emit
/// `StringLiteralExpectedEnum` using the semantic name, leaving `expected`
/// empty because the custom validator doesn't enumerate variants. The
/// originating message is carried along through the formatter's
/// `expected` slot so it doesn't get lost.
fn reshape_for_string_literal(
    tagged: TypeError,
    attr_type: &AttributeType,
    value: &Value,
    attr_name: &str,
) -> TypeError {
    // StringEnum: the error already has enough structure to reshape cleanly.
    if matches!(attr_type, AttributeType::StringEnum { .. }) {
        return tagged.into_string_literal_diagnostic();
    }

    // Namespaced Custom: manually build the shape-mismatch diagnostic from
    // the semantic name. `ValidationFailed` has no attribute slot so
    // `with_attribute` is a no-op; we thread the attribute name in
    // explicitly. `expected` stays empty — custom validators don't
    // enumerate variants — and the original validator message rides on
    // `extra_message` so its detail (which often lists valid forms)
    // stays visible without polluting the structured candidates list.
    if let AttributeType::Custom {
        semantic_name: Some(name),
        namespace: Some(_),
        ..
    } = attr_type
        && let Value::String(typed) = value
        && let TypeError::ValidationFailed { message } = &tagged
    {
        return TypeError::StringLiteralExpectedEnum {
            user_typed: typed.clone(),
            attribute: Some(attr_name.to_string()),
            type_name: name.clone(),
            expected: Vec::new(),
            extra_message: Some(message.clone()),
        };
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
    /// The variant value as the provider declared it (`"AWS_ACCOUNT"`,
    /// or `"Enabled"`).
    pub value: String,
    /// `true` when this entry came from a `to_dsl` alias rather than the
    /// canonical provider-side variant. Code actions should prefer the
    /// canonical form (`is_alias = false`) when offering a fix.
    pub is_alias: bool,
}

impl ExpectedEnumVariant {
    /// `namespace` head becomes `provider`; the rest become `segments`.
    /// `None` produces a bare-form variant.
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
            value: value.to_string(),
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
#[derive(Debug, Clone, thiserror::Error)]
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
        /// `AttributeType::validate` primitive itself doesn't know the name.
        attribute: Option<String>,
        /// Name of the `StringEnum` type that was being matched against
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
    /// Attach an attribute name to the error. Currently only affects
    /// `InvalidEnumVariant`; other variants return `self` unchanged.
    ///
    /// Callers that know which attribute produced the error (e.g. the
    /// attribute loop in `ResourceSchema::validate`) wrap the primitive
    /// error before it reaches CLI/LSP diagnostic text. This keeps
    /// `AttributeType::validate` unaware of attribute names while still
    /// letting the final message say `for 'target_id'`.
    ///
    /// See #2098. `InvalidEnumVariant` is the only variant enriched for
    /// now; adding the same slot to `ValidationFailed` / `TypeMismatch`
    /// is tracked as future work.
    #[must_use]
    pub fn with_attribute(mut self, attribute: impl Into<String>) -> Self {
        match &mut self {
            TypeError::InvalidEnumVariant {
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
            Value::String(_) => "String".to_string(),
            Value::Int(_) => "Int".to_string(),
            Value::Float(_) => "Float".to_string(),
            Value::Bool(_) => "Bool".to_string(),
            Value::List(_) => "List".to_string(),
            Value::Map(_) => "Map".to_string(),
            Value::ResourceRef { path } => {
                format!("ResourceRef({})", path.to_dot_string())
            }
            Value::Interpolation(_) => "Interpolation".to_string(),
            Value::FunctionCall { name, .. } => format!("FunctionCall({})", name),
            Value::Secret(_) => "Secret".to_string(),
            Value::Unknown(_) => "Unknown".to_string(),
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
    /// use carina_core::resource::Value;
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
/// See `docs/specs/2026-05-02-resource-vs-data-source-design.md` (Decision 1-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaKind {
    Managed,
    DataSource,
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
}

impl ResourceSchema {
    pub fn new(resource_type: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            attributes: HashMap::new(),
            description: None,
            validator: None,
            kind: SchemaKind::Managed,
            name_attribute: None,
            force_replace: false,
            operation_config: None,
            exclusive_required: Vec::new(),
        }
    }

    pub fn attribute(mut self, schema: AttributeSchema) -> Self {
        self.attributes.insert(schema.name.clone(), schema);
        self
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
    /// `AttributeType::Custom` value reached during traversal so
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
                if let Err(e) = schema.attr_type.validate(value) {
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
                walk_custom_lookup(&schema.attr_type, value, name, lookup, &mut errors);
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
    }
    result
}

fn collect_block_names_from_type(attr_type: &AttributeType, result: &mut HashMap<String, String>) {
    match attr_type {
        AttributeType::Struct { fields, .. } => {
            for field in fields {
                if let Some(bn) = &field.block_name {
                    result.insert(field.name.clone(), bn.clone());
                }
                collect_block_names_from_type(&field.field_type, result);
            }
        }
        AttributeType::List { inner, .. } => {
            collect_block_names_from_type(inner, result);
        }
        AttributeType::Map { value: inner, .. } => {
            collect_block_names_from_type(inner, result);
        }
        AttributeType::Union(types) => {
            for t in types {
                collect_block_names_from_type(t, result);
            }
        }
        _ => {}
    }
}

/// Resolve block name aliases in a map using struct field definitions.
///
/// For each key in `map` that matches a `block_name` on a struct field,
/// renames it to the canonical field name. Also recurses into nested
/// struct values to resolve block names at all nesting levels.
fn resolve_block_names_in_map(
    map: &mut IndexMap<String, Value>,
    fields: &[StructField],
    resource_id: &str,
    errors: &mut Vec<String>,
) {
    // Build block_name -> canonical field name mapping
    let bn_map: HashMap<String, String> = fields
        .iter()
        .filter_map(|f| f.block_name.as_ref().map(|bn| (bn.clone(), f.name.clone())))
        .collect();

    // Rename block name keys to canonical names, but only when the value
    // is a List (from block syntax). Non-list values (e.g., Value::Map from
    // attribute assignment) target the actual field with that name.
    let renames: Vec<(String, String)> = map
        .keys()
        .filter_map(|key| {
            bn_map.get(key).and_then(|canon| {
                // Only rename if the value is a List (block-originated)
                if matches!(map.get(key), Some(Value::List(_))) {
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
        match &field.field_type {
            AttributeType::Struct { fields: inner, .. } => {
                if let Value::Map(inner_map) = value {
                    resolve_block_names_in_map(inner_map, inner, resource_id, errors);
                }
            }
            AttributeType::List { inner, .. } => {
                if let AttributeType::Struct { fields: inner, .. } = inner.as_ref()
                    && let Value::List(items) = value
                {
                    for item in items.iter_mut() {
                        if let Value::Map(item_map) = item {
                            resolve_block_names_in_map(item_map, inner, resource_id, errors);
                        }
                    }
                }
            }
            _ => {}
        }
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
        // (e.g., Value::Map from attribute assignment) target the actual field with that name.
        let renames: Vec<(String, String)> = resource
            .attributes
            .keys()
            .filter_map(|key| {
                bn_map.get(key).and_then(|canon| {
                    if matches!(resource.get_attr(key), Some(Value::List(_))) {
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

        // Recurse into nested struct values to resolve block names at all levels
        for (attr_name, attr_schema) in &schema.attributes {
            let value = match resource.attributes.get_mut(attr_name) {
                Some(v) => v,
                None => continue,
            };
            match &attr_schema.attr_type {
                AttributeType::Struct { fields, .. } => {
                    if let Value::Map(inner_map) = value {
                        resolve_block_names_in_map(
                            inner_map,
                            fields,
                            &resource.id.to_string(),
                            &mut all_errors,
                        );
                    }
                }
                AttributeType::List { inner, .. } => {
                    if let AttributeType::Struct { fields, .. } = inner.as_ref()
                        && let Value::List(items) = value
                    {
                        for item in items.iter_mut() {
                            if let Value::Map(item_map) = item {
                                resolve_block_names_in_map(
                                    item_map,
                                    fields,
                                    &resource.id.to_string(),
                                    &mut all_errors,
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
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
        AttributeType::Custom {
            semantic_name: Some("PositiveInt".to_string()),
            base: Box::new(AttributeType::Int),
            pattern: None,
            length: None,
            validate: legacy_validator(|value| {
                if let Value::Int(n) = value {
                    if *n > 0 {
                        Ok(())
                    } else {
                        Err("Value must be positive".to_string())
                    }
                } else {
                    Err("Expected integer".to_string())
                }
            }),
            namespace: None,
            to_dsl: None,
        }
    }

    /// IPv4 CIDR block type (e.g., "10.0.0.0/16")
    pub fn ipv4_cidr() -> AttributeType {
        AttributeType::Custom {
            semantic_name: Some("Ipv4Cidr".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: legacy_validator(|value| {
                if let Value::String(s) = value {
                    validate_ipv4_cidr(s)
                } else {
                    Err("Expected string".to_string())
                }
            }),
            namespace: None,
            to_dsl: None,
        }
    }

    /// IPv4 address type (e.g., "10.0.1.5", "192.168.0.1")
    pub fn ipv4_address() -> AttributeType {
        AttributeType::Custom {
            semantic_name: Some("Ipv4Address".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: legacy_validator(|value| {
                if let Value::String(s) = value {
                    validate_ipv4_address(s)
                } else {
                    Err("Expected string".to_string())
                }
            }),
            namespace: None,
            to_dsl: None,
        }
    }

    /// IPv6 address type (e.g., "2001:db8::1", "::1")
    pub fn ipv6_address() -> AttributeType {
        AttributeType::Custom {
            semantic_name: Some("Ipv6Address".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: legacy_validator(|value| {
                if let Value::String(s) = value {
                    validate_ipv6_address(s)
                } else {
                    Err("Expected string".to_string())
                }
            }),
            namespace: None,
            to_dsl: None,
        }
    }

    /// IPv6 CIDR block type (e.g., "2001:db8::/32", "::/0")
    pub fn ipv6_cidr() -> AttributeType {
        AttributeType::Custom {
            semantic_name: Some("Ipv6Cidr".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: legacy_validator(|value| {
                if let Value::String(s) = value {
                    validate_ipv6_cidr(s)
                } else {
                    Err("Expected string".to_string())
                }
            }),
            namespace: None,
            to_dsl: None,
        }
    }

    /// CIDR block type that accepts both IPv4 and IPv6 (e.g., "10.0.0.0/16" or "2001:db8::/32")
    pub fn cidr() -> AttributeType {
        AttributeType::Union(vec![ipv4_cidr(), ipv6_cidr()])
    }

    /// Email address type (RFC 5322-ish lightweight validation).
    ///
    /// Validation is intentionally pragmatic, not a full RFC 5322 parser:
    /// requires a non-empty local part, a single `@`, and a domain that
    /// contains at least one dot with non-empty labels.
    pub fn email() -> AttributeType {
        AttributeType::Custom {
            semantic_name: Some("Email".to_string()),
            base: Box::new(AttributeType::String),
            pattern: None,
            length: None,
            validate: legacy_validator(|value| {
                if let Value::String(s) = value {
                    validate_email(s)
                } else {
                    Err("Expected string".to_string())
                }
            }),
            namespace: None,
            to_dsl: None,
        }
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

/// Compute Levenshtein edit distance between two strings
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_len = a.len();
    let b_len = b.len();

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
/// See `docs/specs/2026-05-02-resource-vs-data-source-design.md` (Decision 1-2).
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
            SchemaKind::Managed => {
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
            SchemaKind::Managed => self.managed.get(&key),
            SchemaKind::DataSource => self.data_sources.get(&key),
        }
    }

    /// Look up the schema appropriate for a given `Resource`. Picks the
    /// `Managed` entry for normal resources and the `DataSource` entry for
    /// `read`-keyword resources (`ResourceKind::DataSource`).
    pub fn get_for(&self, resource: &crate::resource::Resource) -> Option<&ResourceSchema> {
        let kind = if resource.is_data_source() {
            SchemaKind::DataSource
        } else {
            SchemaKind::Managed
        };
        self.get(&resource.id.provider, &resource.id.resource_type, kind)
    }

    pub fn has_managed(&self, provider: &str, resource_type: &str) -> bool {
        self.get(provider, resource_type, SchemaKind::Managed)
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
            .map(|((p, t), s)| (p.as_str(), t.as_str(), SchemaKind::Managed, s))
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
