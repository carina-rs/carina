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

/// An unevaluated expression in the DSL.
///
/// `Expr` is a newtype wrapper around `Value` that represents values which may need
/// resolution before becoming final. The parser produces `Expr` values for resource
/// attributes; the resolver resolves references within the inner `Value` (e.g.,
/// replacing `ResourceRef` variants with concrete values).
///
/// `Expr` wraps `Value` to enforce a type-level distinction between pre-resolution
/// and post-resolution data. This prevents downstream code from accidentally receiving
/// unresolved expressions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Expr(pub Value);

/// A single segment in an access path.
///
/// For now, only `Field` is used. `Index` and `Key` will be added in the future
/// for array indexing and map key access.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PathSegment {
    /// Named field access (e.g., `vpc`, `id`, `vpc_id`)
    Field(String),
}

impl PathSegment {
    /// Returns the field name if this is a `Field` segment.
    pub fn as_field(&self) -> Option<&str> {
        match self {
            PathSegment::Field(name) => Some(name),
        }
    }
}

impl std::fmt::Display for PathSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathSegment::Field(name) => write!(f, "{}", name),
        }
    }
}

/// A unified access path representing a chain of field accesses.
///
/// For a `ResourceRef`, the path contains:
/// - segment 0: binding name (e.g., "vpc")
/// - segment 1: attribute name (e.g., "vpc_id")
/// - segments 2+: nested field path (e.g., "network", "id")
///
/// This replaces the asymmetric `binding_name` / `attribute_name` / `field_path`
/// representation where the 2nd segment was named differently but treated the same
/// as subsequent segments.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccessPath(pub Vec<PathSegment>);

impl AccessPath {
    /// Create an AccessPath from the legacy binding_name, attribute_name, field_path fields.
    pub fn from_ref(
        binding_name: impl Into<String>,
        attribute_name: impl Into<String>,
        field_path: Vec<String>,
    ) -> Self {
        let mut segments = Vec::with_capacity(2 + field_path.len());
        segments.push(PathSegment::Field(binding_name.into()));
        segments.push(PathSegment::Field(attribute_name.into()));
        for field in field_path {
            segments.push(PathSegment::Field(field));
        }
        AccessPath(segments)
    }

    /// Returns the binding name (first segment).
    pub fn binding(&self) -> &str {
        self.0.first().and_then(|s| s.as_field()).unwrap_or("")
    }

    /// Returns the attribute name (second segment).
    pub fn attribute(&self) -> &str {
        self.0.get(1).and_then(|s| s.as_field()).unwrap_or("")
    }

    /// Returns the remaining field path (segments after the first two) as strings.
    pub fn field_path(&self) -> Vec<&str> {
        self.0.iter().skip(2).filter_map(|s| s.as_field()).collect()
    }

    /// Returns all segments as a dot-separated string.
    pub fn to_dot_string(&self) -> String {
        self.0
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .join(".")
    }

    /// Returns the number of segments.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns true if the path has no segments.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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

/// Legacy alias for `InterpolationPart`.
pub type ExprPart = InterpolationPart;

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
            path: AccessPath::from_ref(binding_name, attribute_name, field_path),
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
    pub fn ref_field_path(&self) -> Option<Vec<&str>> {
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

impl Expr {
    /// Returns a reference to the inner `Value`.
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// Consumes self and returns the inner `Value`.
    pub fn into_value(self) -> Value {
        self.0
    }

    /// Returns true if the inner value contains no `ResourceRef` variants.
    ///
    /// This checks recursively: `ResourceRef`s nested inside `List`, `Map`,
    /// `Interpolation`, `FunctionCall`, or `Secret` are detected.
    /// Note that `Interpolation` and `FunctionCall` variants without nested
    /// `ResourceRef`s are considered resolved by this method.
    pub fn is_resolved(&self) -> bool {
        !contains_resource_ref(&self.0)
    }

    /// Extract the inner `Value` of each entry in an `Expr` attribute map.
    ///
    /// Despite the legacy name, this method does **not** resolve
    /// `Value::ResourceRef` / `Value::Interpolation` / `Value::FunctionCall`
    /// / `Value::Secret` — it simply unwraps `Expr` → `Value`. Callers
    /// that need concrete values must run
    /// `carina_core::resolver::resolve_refs_with_state_and_remote` (or an
    /// equivalent) first. See #1683 for a regression caused by assuming
    /// this method performed resolution.
    /// Project an `IndexMap<String, Expr>` (the shape `Resource.attributes`
    /// uses since #2222) into a plain `HashMap<String, Value>` for callers
    /// that only need key-based lookup (state merging, ResourceRef
    /// resolution, provider trait inputs). Source-order is dropped on
    /// purpose at this boundary — keep it on `Resource.attributes` itself
    /// when iteration order matters.
    pub fn resolve_map(attrs: &IndexMap<String, Expr>) -> HashMap<String, Value> {
        attrs
            .iter()
            .map(|(k, e)| (k.clone(), e.0.clone()))
            .collect()
    }

    /// Wrap any `(String, Value)` iterator (`HashMap<String, Value>`,
    /// `IndexMap<String, Value>`, `Vec<(String, Value)>`, …) into the
    /// `IndexMap<String, Expr>` that `Resource.attributes` expects.
    /// Source order follows the iteration order of `attrs`.
    pub fn wrap_map<I>(attrs: I) -> IndexMap<String, Expr>
    where
        I: IntoIterator<Item = (String, Value)>,
    {
        attrs.into_iter().map(|(k, v)| (k, Expr(v))).collect()
    }
}

impl From<Value> for Expr {
    fn from(value: Value) -> Self {
        Expr(value)
    }
}

impl PartialEq<Value> for Expr {
    fn eq(&self, other: &Value) -> bool {
        self.0 == *other
    }
}

impl std::ops::Deref for Expr {
    type Target = Value;
    fn deref(&self) -> &Value {
        &self.0
    }
}

impl std::ops::DerefMut for Expr {
    fn deref_mut(&mut self) -> &mut Value {
        &mut self.0
    }
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
    pub attributes: IndexMap<String, Expr>,
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

    /// Returns the resolved attributes as a `HashMap<String, Value>`.
    ///
    /// Lookup-only callers (validation, differ, plan display) still
    /// receive a `HashMap` — iteration over user-authored order is done
    /// directly via `self.attributes` (`IndexMap`), so flipping this
    /// helper would just force every downstream caller to widen its
    /// signature for no order benefit.
    pub fn resolved_attributes(&self) -> HashMap<String, Value> {
        self.attributes
            .iter()
            .map(|(k, e)| (k.clone(), e.0.clone()))
            .collect()
    }

    /// Get an attribute value by key, returning `Option<&Value>`.
    ///
    /// Convenience method that unwraps the `Expr` wrapper.
    pub fn get_attr(&self, key: &str) -> Option<&Value> {
        self.attributes.get(key).map(|e| &e.0)
    }

    /// Get a mutable attribute value by key, returning `Option<&mut Value>`.
    pub fn get_attr_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.attributes.get_mut(key).map(|e| &mut e.0)
    }

    /// Set an attribute value, wrapping it in `Expr`.
    pub fn set_attr(&mut self, key: impl Into<String>, value: Value) {
        self.attributes.insert(key.into(), Expr(value));
    }

    pub fn with_attribute(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(key.into(), Expr(value));
        self
    }

    pub fn with_expr_attribute(mut self, key: impl Into<String>, expr: Expr) -> Self {
        self.attributes.insert(key.into(), expr);
        self
    }

    /// Set attributes from a `HashMap<String, Value>`, wrapping each value in `Expr`.
    pub fn with_value_attributes(mut self, attrs: HashMap<String, Value>) -> Self {
        self.attributes = Expr::wrap_map(attrs);
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
mod tests {
    use super::*;

    #[test]
    fn value_serde_round_trip() {
        let values = vec![
            Value::String("hello".to_string()),
            Value::Int(42),
            Value::Float(2.5),
            Value::Float(-0.5),
            Value::Bool(true),
            Value::List(vec![Value::String("a".to_string()), Value::Int(1)]),
            Value::Map(IndexMap::from([
                ("key".to_string(), Value::String("val".to_string())),
                ("num".to_string(), Value::Int(10)),
            ])),
            Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
            Value::String("dedicated".to_string()),
            Value::String("InstanceTenancy.dedicated".to_string()),
            Value::Interpolation(vec![
                InterpolationPart::Literal("prefix-".to_string()),
                InterpolationPart::Expr(Value::resource_ref(
                    "vpc".to_string(),
                    "id".to_string(),
                    vec![],
                )),
                InterpolationPart::Literal("-suffix".to_string()),
            ]),
            Value::FunctionCall {
                name: "join".to_string(),
                args: vec![
                    Value::String("-".to_string()),
                    Value::List(vec![
                        Value::String("a".to_string()),
                        Value::String("b".to_string()),
                    ]),
                ],
            },
            Value::Secret(Box::new(Value::String("my-password".to_string()))),
        ];

        for value in values {
            let json = serde_json::to_string(&value).unwrap();
            let deserialized: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value, deserialized, "Round-trip failed for {:?}", value);
        }
    }

    #[test]
    fn resource_id_serde_round_trip() {
        let id = ResourceId::with_provider("awscc", "ec2.Vpc", "main-vpc");
        let json = serde_json::to_string(&id).unwrap();
        let deserialized: ResourceId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deserialized);
    }

    // The "anonymous, awaiting `name` extraction" state is type-distinct
    // from a bound name, so the parser cannot accidentally produce a
    // `ResourceId` whose `name` is the empty string and have it be
    // mistaken for a valid identifier (#2225).

    #[test]
    fn resource_name_pending_is_distinct_from_bound_empty() {
        let pending = ResourceName::Pending;
        let bound_empty = ResourceName::Bound(String::new());
        assert_ne!(pending, bound_empty);
        assert!(pending.is_pending());
        assert!(!bound_empty.is_pending());
    }

    #[test]
    fn resource_id_pending_serde_round_trips_as_empty_string() {
        // V5 state files persist `name` as a plain JSON string. To preserve
        // backward compatibility, ResourceName::Pending serializes to "" and
        // deserializes from "" — round-trip is exact.
        let id = ResourceId {
            provider: "aws".to_string(),
            resource_type: "ec2.Subnet".to_string(),
            name: ResourceName::Pending,
        };
        let json = serde_json::to_string(&id).unwrap();
        assert!(json.contains("\"name\":\"\""), "got: {json}");
        let deserialized: ResourceId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deserialized);
        assert!(deserialized.name.is_pending());
    }

    #[test]
    fn resource_id_bound_serde_round_trips_as_string() {
        let id = ResourceId::with_provider("aws", "ec2.Subnet", "my-subnet");
        let json = serde_json::to_string(&id).unwrap();
        assert!(json.contains("\"name\":\"my-subnet\""), "got: {json}");
        let deserialized: ResourceId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deserialized);
        match deserialized.name {
            ResourceName::Bound(s) => assert_eq!(s, "my-subnet"),
            _ => panic!("expected Bound"),
        }
    }

    /// The AC test from #2225: lookups keyed by `ResourceId` must remain
    /// valid across the name-resolution pass. This is achieved by ensuring
    /// that the parser starts with `Pending`, then any rename to `Bound`
    /// produces a stable identifier.
    /// We assert that two different mutation paths produce equal IDs.
    #[test]
    fn resource_id_rename_pending_to_bound() {
        let mut id = ResourceId {
            provider: "aws".to_string(),
            resource_type: "ec2.Subnet".to_string(),
            name: ResourceName::Pending,
        };
        // The post-pass converts Pending → Bound with the extracted name.
        id.set_name("app-subnet".to_string());
        match &id.name {
            ResourceName::Bound(s) => assert_eq!(s, "app-subnet"),
            _ => panic!("expected Bound after set_name"),
        }
        // After renaming, the same string can produce an equal ResourceId
        // from any other code path (e.g. building a key for a sibling map).
        let constructed = ResourceId::with_provider("aws", "ec2.Subnet", "app-subnet");
        assert_eq!(id, constructed);
    }

    #[test]
    fn state_serde_round_trip() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-bucket".to_string()));
        attrs.insert("versioning".to_string(), Value::Bool(true));

        let state = State::existing(
            ResourceId::with_provider("aws", "s3.Bucket", "my-bucket"),
            attrs,
        )
        .with_identifier("my-bucket");

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: State = serde_json::from_str(&json).unwrap();
        assert_eq!(state, deserialized);
    }

    #[test]
    fn lifecycle_config_serde_with_create_before_destroy() {
        let config = LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: LifecycleConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
        assert!(deserialized.create_before_destroy);
    }

    #[test]
    fn lifecycle_config_backward_compatible_deserialize() {
        // Old JSON without all fields should deserialize with defaults
        let json = r#"{"create_before_destroy":true}"#;
        let config: LifecycleConfig = serde_json::from_str(json).unwrap();
        assert!(config.create_before_destroy);
        assert!(!config.force_delete);
        assert!(!config.prevent_destroy);
    }

    #[test]
    fn lifecycle_config_with_force_delete() {
        let config = LifecycleConfig {
            force_delete: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: LifecycleConfig = serde_json::from_str(&json).unwrap();
        assert!(deserialized.force_delete);
        assert!(!deserialized.create_before_destroy);
        assert!(!deserialized.prevent_destroy);
    }

    #[test]
    fn semantically_equal_lists_same_order() {
        let a = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_different_order() {
        let a = Value::List(vec![Value::Int(3), Value::Int(1), Value::Int(2)]);
        let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_different_content() {
        let a = Value::List(vec![Value::Int(1), Value::Int(2)]);
        let b = Value::List(vec![Value::Int(1), Value::Int(3)]);
        assert!(!a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_different_lengths() {
        let a = Value::List(vec![Value::Int(1), Value::Int(2)]);
        let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert!(!a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_with_duplicates() {
        let a = Value::List(vec![Value::Int(1), Value::Int(1), Value::Int(2)]);
        let b = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(1)]);
        assert!(a.semantically_equal(&b));

        // Different multiplicities should not be equal
        let c = Value::List(vec![Value::Int(1), Value::Int(1), Value::Int(2)]);
        let d = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(2)]);
        assert!(!c.semantically_equal(&d));
    }

    #[test]
    fn semantically_equal_empty_lists() {
        let a = Value::List(vec![]);
        let b = Value::List(vec![]);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_of_maps_different_order() {
        let mut map1 = IndexMap::new();
        map1.insert("port".to_string(), Value::Int(80));
        map1.insert("protocol".to_string(), Value::String("tcp".to_string()));

        let mut map2 = IndexMap::new();
        map2.insert("port".to_string(), Value::Int(443));
        map2.insert("protocol".to_string(), Value::String("tcp".to_string()));

        let a = Value::List(vec![Value::Map(map1.clone()), Value::Map(map2.clone())]);
        let b = Value::List(vec![Value::Map(map2), Value::Map(map1)]);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_lists_of_strings() {
        let a = Value::List(vec![
            Value::String("b".to_string()),
            Value::String("a".to_string()),
        ]);
        let b = Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_non_list_values() {
        // Non-list values should use regular equality
        assert!(Value::Int(42).semantically_equal(&Value::Int(42)));
        assert!(!Value::Int(42).semantically_equal(&Value::Int(43)));
        assert!(
            Value::String("hello".to_string())
                .semantically_equal(&Value::String("hello".to_string()))
        );
        assert!(Value::Bool(true).semantically_equal(&Value::Bool(true)));
    }

    #[test]
    fn semantically_equal_nested_lists() {
        // Lists inside maps are compared order-insensitively via recursive semantically_equal
        let mut map1 = IndexMap::new();
        map1.insert(
            "ports".to_string(),
            Value::List(vec![Value::Int(80), Value::Int(443)]),
        );

        let mut map2 = IndexMap::new();
        map2.insert(
            "ports".to_string(),
            Value::List(vec![Value::Int(443), Value::Int(80)]),
        );

        let a = Value::Map(map1);
        let b = Value::Map(map2);
        assert!(a.semantically_equal(&b));
    }

    #[test]
    fn semantically_equal_maps_different_keys() {
        let mut map1 = IndexMap::new();
        map1.insert("a".to_string(), Value::Int(1));

        let mut map2 = IndexMap::new();
        map2.insert("b".to_string(), Value::Int(1));

        assert!(!Value::Map(map1).semantically_equal(&Value::Map(map2)));
    }

    #[test]
    fn semantically_equal_maps_different_sizes() {
        let mut map1 = IndexMap::new();
        map1.insert("a".to_string(), Value::Int(1));

        let mut map2 = IndexMap::new();
        map2.insert("a".to_string(), Value::Int(1));
        map2.insert("b".to_string(), Value::Int(2));

        assert!(!Value::Map(map1).semantically_equal(&Value::Map(map2)));
    }

    #[test]
    fn merge_with_saved_map_fills_extra_keys() {
        let desired = Value::Map(IndexMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            ("a_record".to_string(), Value::Bool(true)),
        ]));
        let saved = Value::Map(IndexMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            ("a_record".to_string(), Value::Bool(true)),
            ("aaaa_record".to_string(), Value::Bool(false)),
        ]));

        let merged = merge_with_saved(&desired, &saved);
        let expected = Value::Map(IndexMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            ("a_record".to_string(), Value::Bool(true)),
            ("aaaa_record".to_string(), Value::Bool(false)),
        ]));
        assert!(merged.semantically_equal(&expected), "Merged: {:?}", merged);
    }

    #[test]
    fn merge_with_saved_desired_wins() {
        let desired = Value::Map(IndexMap::from([("a".to_string(), Value::Int(10))]));
        let saved = Value::Map(IndexMap::from([
            ("a".to_string(), Value::Int(5)),
            ("b".to_string(), Value::Int(20)),
        ]));

        let merged = merge_with_saved(&desired, &saved);
        let expected = Value::Map(IndexMap::from([
            ("a".to_string(), Value::Int(10)),
            ("b".to_string(), Value::Int(20)),
        ]));
        assert!(merged.semantically_equal(&expected), "Merged: {:?}", merged);
    }

    #[test]
    fn merge_with_saved_list_of_maps() {
        let desired = Value::List(vec![Value::Map(IndexMap::from([(
            "port".to_string(),
            Value::Int(80),
        )]))]);
        let saved = Value::List(vec![Value::Map(IndexMap::from([
            ("port".to_string(), Value::Int(80)),
            ("protocol".to_string(), Value::String("tcp".to_string())),
        ]))]);

        let merged = merge_with_saved(&desired, &saved);
        let expected = Value::List(vec![Value::Map(IndexMap::from([
            ("port".to_string(), Value::Int(80)),
            ("protocol".to_string(), Value::String("tcp".to_string())),
        ]))]);
        assert!(merged.semantically_equal(&expected), "Merged: {:?}", merged);
    }

    #[test]
    fn merge_with_saved_non_map() {
        let desired = Value::String("hello".to_string());
        let saved = Value::String("world".to_string());
        let merged = merge_with_saved(&desired, &saved);
        assert_eq!(merged, Value::String("hello".to_string()));

        let desired = Value::Int(42);
        let saved = Value::Int(99);
        let merged = merge_with_saved(&desired, &saved);
        assert_eq!(merged, Value::Int(42));
    }

    #[test]
    fn lists_equal_large_list_correctness() {
        // Verify correctness with a list larger than HASH_THRESHOLD
        let n = 200;
        let a: Vec<Value> = (0..n).map(Value::Int).collect();
        let b: Vec<Value> = (0..n).rev().map(Value::Int).collect();
        assert!(lists_equal(&a, &b));

        // Different content
        let mut c: Vec<Value> = (0..n).map(Value::Int).collect();
        c[n as usize - 1] = Value::Int(n); // change last element
        assert!(!lists_equal(&a, &c));
    }

    #[test]
    fn lists_equal_large_list_with_duplicates() {
        let n = 100;
        let a: Vec<Value> = (0..n)
            .flat_map(|i| vec![Value::Int(i), Value::Int(i)])
            .collect();
        let b: Vec<Value> = (0..n)
            .rev()
            .flat_map(|i| vec![Value::Int(i), Value::Int(i)])
            .collect();
        assert!(lists_equal(&a, &b));

        // Different multiplicities
        let mut c = a.clone();
        c[0] = Value::Int(999);
        assert!(!lists_equal(&a, &c));
    }

    #[test]
    fn lists_equal_large_list_of_maps() {
        // Simulates security group rules (100+ maps)
        let n = 150;
        let make_rule = |i: i64| {
            Value::Map(IndexMap::from([
                ("port".to_string(), Value::Int(i)),
                ("protocol".to_string(), Value::String("tcp".to_string())),
                (
                    "cidr".to_string(),
                    Value::String(format!("10.0.{}.0/24", i)),
                ),
            ]))
        };

        let a: Vec<Value> = (0..n).map(make_rule).collect();
        let b: Vec<Value> = (0..n).rev().map(make_rule).collect();
        assert!(lists_equal(&a, &b));
    }

    #[test]
    fn lists_equal_performance_large_list() {
        // Benchmark: 1000-element list comparison should complete quickly
        // With O(n^2), 1000 elements = 1M comparisons; with hashing, ~1000.
        let n = 1000;
        let a: Vec<Value> = (0..n).map(Value::Int).collect();
        let b: Vec<Value> = (0..n).rev().map(Value::Int).collect();

        let start = std::time::Instant::now();
        for _ in 0..100 {
            assert!(lists_equal(&a, &b));
        }
        let elapsed = start.elapsed();
        // Should complete well under 1 second for 100 iterations
        assert!(
            elapsed.as_secs() < 5,
            "lists_equal with 1000 elements took {:?} for 100 iterations, expected < 5s",
            elapsed
        );
    }

    #[test]
    fn merge_lists_large_list_correctness() {
        // Verify merge_lists works correctly with large lists
        let n = 50;
        let desired: Vec<Value> = (0..n)
            .map(|i| Value::Map(IndexMap::from([("port".to_string(), Value::Int(i))])))
            .collect();
        let saved: Vec<Value> = (0..n)
            .rev()
            .map(|i| {
                Value::Map(IndexMap::from([
                    ("port".to_string(), Value::Int(i)),
                    ("protocol".to_string(), Value::String("tcp".to_string())),
                ]))
            })
            .collect();

        let merged = merge_lists(&desired, &saved);
        assert_eq!(merged.len(), n as usize);

        // Each merged element should have both port and protocol
        for item in &merged {
            if let Value::Map(map) = item {
                assert!(map.contains_key("port"), "Missing port in merged item");
                assert!(
                    map.contains_key("protocol"),
                    "Missing protocol in merged item"
                );
            } else {
                panic!("Expected Map, got {:?}", item);
            }
        }
    }

    #[test]
    fn canonical_hash_consistency() {
        // Same value should produce same hash
        let v1 = Value::Int(42);
        let v2 = Value::Int(42);
        assert_eq!(v1.canonical_hash(), v2.canonical_hash());

        // Different values should (usually) produce different hashes
        let v3 = Value::Int(43);
        assert_ne!(v1.canonical_hash(), v3.canonical_hash());

        // Maps with same content should hash the same regardless of insertion order
        let m1 = Value::Map(IndexMap::from([
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(2)),
        ]));
        let m2 = Value::Map(IndexMap::from([
            ("b".to_string(), Value::Int(2)),
            ("a".to_string(), Value::Int(1)),
        ]));
        assert_eq!(m1.canonical_hash(), m2.canonical_hash());
    }

    #[test]
    fn resource_typed_binding_field() {
        let resource = Resource::new("s3.Bucket", "my-bucket").with_binding("my_bucket");
        assert_eq!(resource.binding, Some("my_bucket".to_string()));
        // binding should NOT be in attributes
        assert!(!resource.attributes.contains_key("_binding"));
    }

    #[test]
    fn resource_typed_dependency_bindings_field() {
        let resource = Resource::new("ec2.Subnet", "my-subnet")
            .with_dependency_bindings(["vpc".to_string()].into_iter().collect());
        assert!(resource.dependency_bindings.contains("vpc"));
        assert_eq!(resource.dependency_bindings.len(), 1);
        // dependency_bindings should NOT be in attributes
        assert!(!resource.attributes.contains_key("_dependency_bindings"));
    }

    /// Set semantics: assigning the same binding twice yields exactly one
    /// entry (#2228).
    #[test]
    fn resource_dependency_bindings_dedup_on_duplicate_insert() {
        let mut resource = Resource::new("ec2.Subnet", "my-subnet");
        resource.dependency_bindings.insert("vpc".to_string());
        resource.dependency_bindings.insert("vpc".to_string());
        assert_eq!(resource.dependency_bindings.len(), 1);
        assert!(resource.dependency_bindings.contains("vpc"));
    }

    /// Iteration order is deterministic (sorted) regardless of insertion
    /// order (#2228).
    #[test]
    fn resource_dependency_bindings_iteration_is_sorted() {
        let mut resource = Resource::new("ec2.Route", "my-route");
        resource.dependency_bindings.insert("rt".to_string());
        resource
            .dependency_bindings
            .insert("tgw_attach".to_string());
        resource.dependency_bindings.insert("vpc".to_string());
        let order: Vec<&String> = resource.dependency_bindings.iter().collect();
        assert_eq!(order, vec!["rt", "tgw_attach", "vpc"]);
    }

    /// State-struct dependency_bindings has the same Set semantics so
    /// that delete-ordering metadata is also dedup'd and stable (#2228).
    #[test]
    fn state_dependency_bindings_dedup_on_duplicate_insert() {
        let mut state = State::not_found(ResourceId::new("ec2.Subnet", "my-subnet"));
        state.dependency_bindings.insert("vpc".to_string());
        state.dependency_bindings.insert("vpc".to_string());
        assert_eq!(state.dependency_bindings.len(), 1);
    }

    #[test]
    fn resource_typed_virtual_field() {
        let resource = Resource::new("_virtual", "web").with_kind(ResourceKind::Virtual {
            module_name: "web_tier".to_string(),
            instance: "web".to_string(),
        });
        assert!(resource.is_virtual());
        // _virtual should NOT be in attributes
        assert!(!resource.attributes.contains_key("_virtual"));
    }

    #[test]
    fn resource_default_metadata_fields() {
        let resource = Resource::new("s3.Bucket", "my-bucket");
        assert_eq!(resource.binding, None);
        assert!(resource.dependency_bindings.is_empty());
        assert!(!resource.is_virtual());
    }

    #[test]
    fn resource_kind_enum_real_by_default() {
        let resource = Resource::new("s3.Bucket", "my-bucket");
        assert_eq!(resource.kind, ResourceKind::Real);
        assert!(!resource.is_virtual());
        assert!(!resource.is_data_source());
    }

    #[test]
    fn resource_kind_enum_virtual_carries_module_info() {
        let resource = Resource::new("_virtual", "web").with_kind(ResourceKind::Virtual {
            module_name: "web_tier".to_string(),
            instance: "web".to_string(),
        });
        assert!(resource.is_virtual());
        assert!(!resource.is_data_source());
        // Module info is in the kind, not in attributes
        assert!(!resource.attributes.contains_key("_module"));
        assert!(!resource.attributes.contains_key("_module_instance"));
        // Can extract module info from the kind
        match &resource.kind {
            ResourceKind::Virtual {
                module_name,
                instance,
            } => {
                assert_eq!(module_name, "web_tier");
                assert_eq!(instance, "web");
            }
            _ => panic!("Expected Virtual kind"),
        }
    }

    #[test]
    fn resource_kind_enum_data_source() {
        let resource = Resource::new("s3.Bucket", "my-bucket").with_kind(ResourceKind::DataSource);
        assert!(resource.is_data_source());
        assert!(!resource.is_virtual());
    }

    #[test]
    fn expr_wraps_value() {
        let expr = Expr(Value::String("hello".to_string()));
        assert!(matches!(*expr, Value::String(_)));
    }

    #[test]
    fn expr_wraps_resource_ref() {
        let expr = Expr(Value::resource_ref(
            "vpc".to_string(),
            "id".to_string(),
            vec![],
        ));
        assert!(matches!(*expr, Value::ResourceRef { .. }));
        assert!(!expr.is_resolved());
    }

    #[test]
    fn expr_wraps_interpolation() {
        let expr = Expr(Value::Interpolation(vec![
            InterpolationPart::Literal("prefix-".to_string()),
            InterpolationPart::Expr(Value::resource_ref(
                "vpc".to_string(),
                "id".to_string(),
                vec![],
            )),
        ]));
        assert!(matches!(*expr, Value::Interpolation(_)));
        assert!(!expr.is_resolved());
    }

    #[test]
    fn expr_wraps_function_call() {
        let expr = Expr(Value::FunctionCall {
            name: "join".to_string(),
            args: vec![
                Value::String("-".to_string()),
                Value::List(vec![
                    Value::String("a".to_string()),
                    Value::String("b".to_string()),
                ]),
            ],
        });
        assert!(matches!(*expr, Value::FunctionCall { .. }));
    }

    #[test]
    fn resource_attributes_use_expr_type() {
        let resource = Resource::new("s3.Bucket", "test")
            .with_expr_attribute("name", Expr(Value::String("my-bucket".to_string())))
            .with_expr_attribute(
                "vpc_id",
                Expr(Value::resource_ref(
                    "vpc".to_string(),
                    "id".to_string(),
                    vec![],
                )),
            );
        assert!(matches!(resource.get_attr("name"), Some(Value::String(_))));
        assert!(matches!(
            resource.get_attr("vpc_id"),
            Some(Value::ResourceRef { .. })
        ));
    }

    #[test]
    fn expr_serde_round_trip() {
        let exprs = vec![
            Expr(Value::String("hello".to_string())),
            Expr(Value::Int(42)),
            Expr(Value::resource_ref(
                "vpc".to_string(),
                "id".to_string(),
                vec![],
            )),
            Expr(Value::Interpolation(vec![
                InterpolationPart::Literal("prefix-".to_string()),
                InterpolationPart::Expr(Value::resource_ref(
                    "vpc".to_string(),
                    "id".to_string(),
                    vec![],
                )),
            ])),
            Expr(Value::FunctionCall {
                name: "join".to_string(),
                args: vec![
                    Value::String("-".to_string()),
                    Value::List(vec![
                        Value::String("a".to_string()),
                        Value::String("b".to_string()),
                    ]),
                ],
            }),
        ];

        for expr in exprs {
            let json = serde_json::to_string(&expr).unwrap();
            let deserialized: Expr = serde_json::from_str(&json).unwrap();
            assert_eq!(expr, deserialized, "Round-trip failed for {:?}", expr);
        }
    }

    #[test]
    fn expr_is_resolved_for_plain_values() {
        assert!(Expr(Value::String("hello".to_string())).is_resolved());
        assert!(Expr(Value::Int(42)).is_resolved());
        assert!(Expr(Value::Bool(true)).is_resolved());
    }

    #[test]
    fn expr_is_not_resolved_for_refs() {
        assert!(
            !Expr(Value::resource_ref(
                "vpc".to_string(),
                "id".to_string(),
                vec![]
            ))
            .is_resolved()
        );
    }

    #[test]
    fn expr_deref_to_value() {
        let expr = Expr(Value::String("hello".to_string()));
        let val: &Value = &expr;
        assert!(matches!(val, Value::String(s) if s == "hello"));
    }

    #[test]
    fn expr_from_value() {
        let value = Value::Int(42);
        let expr: Expr = value.into();
        assert_eq!(expr.0, Value::Int(42));
    }

    #[test]
    fn expr_resolve_map() {
        let mut attrs = IndexMap::new();
        attrs.insert("name".to_string(), Expr(Value::String("test".to_string())));
        attrs.insert("count".to_string(), Expr(Value::Int(5)));
        let resolved = Expr::resolve_map(&attrs);
        assert_eq!(
            resolved.get("name"),
            Some(&Value::String("test".to_string()))
        );
        assert_eq!(resolved.get("count"), Some(&Value::Int(5)));
    }

    #[test]
    fn resource_module_source_typed_field() {
        // Real resources that belong to modules should use the typed module_source field
        // instead of storing _module/_module_instance as hidden attributes
        let resource =
            Resource::new("ec2.SecurityGroup", "web_sg").with_module_source(ModuleSource::Module {
                name: "web_tier".to_string(),
                instance: "web".to_string(),
            });

        // Module source info should be in the typed field
        assert_eq!(
            resource.module_source,
            Some(ModuleSource::Module {
                name: "web_tier".to_string(),
                instance: "web".to_string(),
            })
        );

        // Module source info should NOT be in attributes
        assert!(!resource.attributes.contains_key("_module"));
        assert!(!resource.attributes.contains_key("_module_instance"));
    }

    #[test]
    fn access_path_from_ref() {
        let path = AccessPath::from_ref("vpc", "id", vec![]);
        assert_eq!(path.binding(), "vpc");
        assert_eq!(path.attribute(), "id");
        assert!(path.field_path().is_empty());
        assert_eq!(path.to_dot_string(), "vpc.id");
    }

    #[test]
    fn access_path_with_field_path() {
        let path = AccessPath::from_ref("web", "network", vec!["vpc_id".to_string()]);
        assert_eq!(path.binding(), "web");
        assert_eq!(path.attribute(), "network");
        assert_eq!(path.field_path(), vec!["vpc_id"]);
        assert_eq!(path.to_dot_string(), "web.network.vpc_id");
    }

    #[test]
    fn resource_ref_serde_roundtrip() {
        let value = Value::resource_ref("vpc", "id", vec![]);
        let json = serde_json::to_string(&value).unwrap();
        let deserialized: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value, deserialized);
    }

    #[test]
    fn resource_ref_serde_with_field_path() {
        let value = Value::resource_ref("web", "network", vec!["vpc_id".to_string()]);
        let json = serde_json::to_string(&value).unwrap();
        let deserialized: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value, deserialized);
    }

    #[test]
    fn value_ref_helpers() {
        let value = Value::resource_ref("vpc", "vpc_id", vec!["nested".to_string()]);
        assert_eq!(value.ref_binding(), Some("vpc"));
        assert_eq!(value.ref_attribute(), Some("vpc_id"));
        assert_eq!(value.ref_field_path(), Some(vec!["nested"]));

        let non_ref = Value::String("hello".to_string());
        assert_eq!(non_ref.ref_binding(), None);
    }

    // Closure tests moved out: `Value::Closure` no longer exists. Closure
    // construction, helper methods, and serde-skip behavior are now
    // properties of `EvalValue`, exercised in `eval_value.rs`.

    #[test]
    fn visit_refs_collects_from_all_nested_variants() {
        let value = Value::List(vec![
            Value::resource_ref("a", "id", vec![]),
            Value::Map(IndexMap::from([(
                "k".to_string(),
                Value::resource_ref("b", "id", vec![]),
            )])),
            Value::Interpolation(vec![
                InterpolationPart::Literal("x".to_string()),
                InterpolationPart::Expr(Value::resource_ref("c", "id", vec![])),
            ]),
            Value::FunctionCall {
                name: "join".to_string(),
                args: vec![Value::resource_ref("d", "id", vec![])],
            },
            Value::Secret(Box::new(Value::resource_ref("e", "id", vec![]))),
            Value::String("plain".to_string()),
        ]);

        let mut collected: Vec<String> = Vec::new();
        value.visit_refs(&mut |path| {
            collected.push(path.binding().to_string());
        });
        collected.sort();
        assert_eq!(collected, vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn visit_refs_on_leaf_variants_calls_nothing() {
        for v in [
            Value::String("s".into()),
            Value::Int(1),
            Value::Float(1.0),
            Value::Bool(true),
        ] {
            let mut count = 0;
            v.visit_refs(&mut |_| count += 1);
            assert_eq!(count, 0);
        }
    }
}
