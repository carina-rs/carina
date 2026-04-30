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

/// A typed access path representing a `ResourceRef` target.
///
/// The path always carries a binding name and an attribute name; nested field
/// access (e.g., `web.network.vpc_id`) is captured in `field_path`. The
/// "binding + attribute is mandatory" invariant is enforced by the type system
/// — there is no way to construct an `AccessPath` without both.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccessPath {
    binding: String,
    attribute: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    field_path: Vec<String>,
}

impl AccessPath {
    /// Create an `AccessPath` referring to a top-level attribute of a binding.
    pub fn new(binding: impl Into<String>, attribute: impl Into<String>) -> Self {
        Self {
            binding: binding.into(),
            attribute: attribute.into(),
            field_path: Vec::new(),
        }
    }

    /// Create an `AccessPath` with a nested field path (e.g., `web.network.vpc_id`).
    pub fn with_fields(
        binding: impl Into<String>,
        attribute: impl Into<String>,
        field_path: Vec<String>,
    ) -> Self {
        Self {
            binding: binding.into(),
            attribute: attribute.into(),
            field_path,
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

    /// Returns the path as `binding.attribute[.field...]`.
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
        out
    }
}

impl std::fmt::Display for AccessPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_dot_string())
    }
}

/// Attribute value of a resource
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    List(Vec<Value>),
    Map(IndexMap<String, Value>),
    /// Reference to another resource's attribute via an access path.
    ///
    /// The access path segments represent:
    /// - segment 0: binding name (e.g., "vpc")
    /// - segment 1: attribute name (e.g., "vpc_id")
    /// - segments 2+: nested field path
    ResourceRef {
        path: AccessPath,
    },
    /// String interpolation: `"prefix-${expr}-suffix"`
    /// Parts are evaluated and concatenated into a final String.
    Interpolation(Vec<InterpolationPart>),
    /// Built-in function call: `join("-", ["a", "b"])` or via pipe `["a", "b"] |> join("-")`
    /// Evaluated during reference resolution.
    FunctionCall {
        /// Function name (e.g., "join")
        name: String,
        /// Arguments to the function
        args: Vec<Value>,
    },
    /// A secret value. The inner value is sent to the provider but stored as a
    /// SHA256 hash in state. Plan output displays `(secret)` instead of the value.
    Secret(Box<Value>),
}

/// A part of a string interpolation expression
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InterpolationPart {
    /// Literal text segment
    Literal(String),
    /// An expression to be evaluated and converted to string
    Expr(Value),
}

impl Value {
    /// Create a `ResourceRef` from binding name, attribute name, and optional field path.
    ///
    /// This is the primary constructor for `ResourceRef` values, replacing direct
    /// struct literal construction.
    pub fn resource_ref(
        binding_name: impl Into<String>,
        attribute_name: impl Into<String>,
        field_path: Vec<String>,
    ) -> Self {
        Value::ResourceRef {
            path: AccessPath::with_fields(binding_name, attribute_name, field_path),
        }
    }

    /// If this is a `ResourceRef`, returns the binding name.
    pub fn ref_binding(&self) -> Option<&str> {
        match self {
            Value::ResourceRef { path } => Some(path.binding()),
            _ => None,
        }
    }

    /// If this is a `ResourceRef`, returns the attribute name.
    pub fn ref_attribute(&self) -> Option<&str> {
        match self {
            Value::ResourceRef { path } => Some(path.attribute()),
            _ => None,
        }
    }

    /// If this is a `ResourceRef`, returns the field path.
    pub fn ref_field_path(&self) -> Option<&[String]> {
        match self {
            Value::ResourceRef { path } => Some(path.field_path()),
            _ => None,
        }
    }

    /// Recursively walk this value, invoking `f` on each `ResourceRef`'s `AccessPath`.
    pub fn visit_refs(&self, f: &mut impl FnMut(&AccessPath)) {
        match self {
            Value::ResourceRef { path } => f(path),
            Value::List(items) => {
                for v in items {
                    v.visit_refs(f);
                }
            }
            Value::Map(map) => {
                for v in map.values() {
                    v.visit_refs(f);
                }
            }
            Value::Interpolation(parts) => {
                for part in parts {
                    if let InterpolationPart::Expr(v) = part {
                        v.visit_refs(f);
                    }
                }
            }
            Value::FunctionCall { args, .. } => {
                for arg in args {
                    arg.visit_refs(f);
                }
            }
            Value::Secret(inner) => inner.visit_refs(f),
            Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => {}
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
            (Value::List(a), Value::List(b)) => lists_equal(a, b),
            (Value::Map(a), Value::Map(b)) => maps_semantically_equal(a, b),
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
            Value::String(s) => s.hash(hasher),
            Value::Int(i) => i.hash(hasher),
            Value::Float(f) => {
                // Use bits for deterministic hashing (NaN == NaN for our purposes)
                f.to_bits().hash(hasher);
            }
            Value::Bool(b) => b.hash(hasher),
            Value::List(items) => {
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
            Value::Map(map) => {
                map.len().hash(hasher);
                // Sort keys for deterministic hashing
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for key in keys {
                    key.hash(hasher);
                    map[key].hash_into(hasher);
                }
            }
            Value::ResourceRef { path } => {
                path.hash(hasher);
            }
            Value::Interpolation(parts) => {
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
            Value::FunctionCall { name, args } => {
                name.hash(hasher);
                args.len().hash(hasher);
                for arg in args {
                    arg.hash_into(hasher);
                }
            }
            Value::Secret(inner) => {
                inner.hash_into(hasher);
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
        (Value::Map(desired_map), Value::Map(saved_map)) => {
            let mut merged = saved_map.clone();
            for (k, v) in desired_map {
                let merged_v = if let Some(saved_v) = saved_map.get(k) {
                    merge_with_saved(v, saved_v)
                } else {
                    v.clone()
                };
                merged.insert(k.clone(), merged_v);
            }
            Value::Map(merged)
        }
        (Value::List(desired_list), Value::List(saved_list)) => {
            Value::List(merge_lists(desired_list, saved_list))
        }
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
        if matches!(d, Value::Map(_)) {
            for (j, s) in saved.iter().enumerate() {
                if used[j] || matches!(best_idx, Some(bi) if bi == j) {
                    continue;
                }
                if !matches!(s, Value::Map(_)) {
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
        (Value::Map(am), Value::Map(bm)) => am
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

/// Lifecycle configuration for a resource
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LifecycleConfig {
    /// If true, force-delete the resource (e.g., non-empty S3 buckets)
    #[serde(default)]
    pub force_delete: bool,
    /// If true, create the new resource before destroying the old one during replacement
    #[serde(default)]
    pub create_before_destroy: bool,
    /// If true, prevent the resource from being destroyed
    #[serde(default)]
    pub prevent_destroy: bool,
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
    /// A real infrastructure resource managed by a provider
    #[default]
    Real,
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
    /// Classification of this resource (real, virtual, or data source)
    #[serde(default)]
    pub kind: ResourceKind,
    /// Lifecycle meta-argument configuration
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
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
            kind: ResourceKind::Real,
            lifecycle: LifecycleConfig::default(),
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
            kind: ResourceKind::Real,
            lifecycle: LifecycleConfig::default(),
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
