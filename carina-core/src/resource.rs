//! Resource - Representing resources and their state

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};

/// Unique identifier for a resource
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceId {
    /// Provider name (e.g., "aws", "awscc")
    pub provider: String,
    /// Resource type (e.g., "s3.bucket", "ec2.instance")
    pub resource_type: String,
    /// Resource name (identifier specified in DSL)
    pub name: String,
}

impl ResourceId {
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            provider: String::new(),
            resource_type: resource_type.into(),
            name: name.into(),
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
            name: name.into(),
        }
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

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.provider.is_empty() {
            write!(f, "{}.{}", self.resource_type, self.name)
        } else {
            write!(f, "{}.{}.{}", self.provider, self.resource_type, self.name)
        }
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
    Map(HashMap<String, Value>),
    /// Reference to another resource's attribute
    ResourceRef {
        /// Binding name of the referenced resource (e.g., "vpc", "web_sg")
        binding_name: String,
        /// Attribute name being referenced (e.g., "id", "name")
        attribute_name: String,
    },
    /// Unresolved identifier that will be resolved during schema validation
    /// This allows shorthand enum values like `dedicated` to be resolved to
    /// `aws.vpc.InstanceTenancy.dedicated` based on schema context.
    /// The tuple contains (identifier, optional_member) for forms like:
    /// - `dedicated` -> ("dedicated", None)
    /// - `InstanceTenancy.dedicated` -> ("InstanceTenancy", Some("dedicated"))
    UnresolvedIdent(String, Option<String>),
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
            Value::ResourceRef {
                binding_name,
                attribute_name,
            } => {
                binding_name.hash(hasher);
                attribute_name.hash(hasher);
            }
            Value::UnresolvedIdent(name, member) => {
                name.hash(hasher);
                member.hash(hasher);
            }
        }
    }
}

/// Compare two maps using semantic equality for their values.
/// This ensures nested lists within maps are compared order-insensitively.
fn maps_semantically_equal(a: &HashMap<String, Value>, b: &HashMap<String, Value>) -> bool {
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
    /// If true, force-delete the resource (e.g., empty S3 bucket before deletion)
    #[serde(default)]
    pub force_delete: bool,
    /// If true, create the new resource before destroying the old one during replacement
    #[serde(default)]
    pub create_before_destroy: bool,
}

/// Desired state declared in DSL
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    pub attributes: HashMap<String, Value>,
    /// If true, this is a data source (read-only) that won't be modified
    pub read_only: bool,
    /// Lifecycle meta-argument configuration
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
    /// Attribute prefixes: maps attribute name -> prefix string
    /// e.g., {"bucket_name": "my-app-"} from `bucket_name_prefix = "my-app-"`
    #[serde(default)]
    pub prefixes: HashMap<String, String>,
}

impl Resource {
    pub fn new(resource_type: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: ResourceId::new(resource_type, name),
            attributes: HashMap::new(),
            read_only: false,
            lifecycle: LifecycleConfig::default(),
            prefixes: HashMap::new(),
        }
    }

    pub fn with_provider(
        provider: impl Into<String>,
        resource_type: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            id: ResourceId::with_provider(provider, resource_type, name),
            attributes: HashMap::new(),
            read_only: false,
            lifecycle: LifecycleConfig::default(),
            prefixes: HashMap::new(),
        }
    }

    pub fn with_attribute(mut self, key: impl Into<String>, value: Value) -> Self {
        self.attributes.insert(key.into(), value);
        self
    }

    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// Returns true if this resource is a data source (read-only)
    pub fn is_data_source(&self) -> bool {
        self.read_only
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
}

impl State {
    pub fn not_found(id: ResourceId) -> Self {
        Self {
            id,
            identifier: None,
            attributes: HashMap::new(),
            exists: false,
        }
    }

    pub fn existing(id: ResourceId, attributes: HashMap<String, Value>) -> Self {
        Self {
            id,
            identifier: None,
            attributes,
            exists: true,
        }
    }

    pub fn with_identifier(mut self, identifier: impl Into<String>) -> Self {
        self.identifier = Some(identifier.into());
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
            Value::Map(HashMap::from([
                ("key".to_string(), Value::String("val".to_string())),
                ("num".to_string(), Value::Int(10)),
            ])),
            Value::ResourceRef {
                binding_name: "vpc".to_string(),
                attribute_name: "id".to_string(),
            },
            Value::ResourceRef {
                binding_name: "web_sg".to_string(),
                attribute_name: "id".to_string(),
            },
            Value::ResourceRef {
                binding_name: "bucket".to_string(),
                attribute_name: "arn".to_string(),
            },
            Value::UnresolvedIdent("dedicated".to_string(), None),
            Value::UnresolvedIdent("InstanceTenancy".to_string(), Some("dedicated".to_string())),
        ];

        for value in values {
            let json = serde_json::to_string(&value).unwrap();
            let deserialized: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value, deserialized, "Round-trip failed for {:?}", value);
        }
    }

    #[test]
    fn resource_id_serde_round_trip() {
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "main-vpc");
        let json = serde_json::to_string(&id).unwrap();
        let deserialized: ResourceId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, deserialized);
    }

    #[test]
    fn state_serde_round_trip() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my-bucket".to_string()));
        attrs.insert("versioning".to_string(), Value::Bool(true));

        let state = State::existing(
            ResourceId::with_provider("aws", "s3.bucket", "my-bucket"),
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
            force_delete: false,
            create_before_destroy: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: LifecycleConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
        assert!(deserialized.create_before_destroy);
    }

    #[test]
    fn lifecycle_config_backward_compatible_deserialize() {
        // Old JSON without create_before_destroy field should deserialize with default (false)
        let json = r#"{"force_delete":true}"#;
        let config: LifecycleConfig = serde_json::from_str(json).unwrap();
        assert!(config.force_delete);
        assert!(!config.create_before_destroy);
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
        let mut map1 = HashMap::new();
        map1.insert("port".to_string(), Value::Int(80));
        map1.insert("protocol".to_string(), Value::String("tcp".to_string()));

        let mut map2 = HashMap::new();
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
        let mut map1 = HashMap::new();
        map1.insert(
            "ports".to_string(),
            Value::List(vec![Value::Int(80), Value::Int(443)]),
        );

        let mut map2 = HashMap::new();
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
        let mut map1 = HashMap::new();
        map1.insert("a".to_string(), Value::Int(1));

        let mut map2 = HashMap::new();
        map2.insert("b".to_string(), Value::Int(1));

        assert!(!Value::Map(map1).semantically_equal(&Value::Map(map2)));
    }

    #[test]
    fn semantically_equal_maps_different_sizes() {
        let mut map1 = HashMap::new();
        map1.insert("a".to_string(), Value::Int(1));

        let mut map2 = HashMap::new();
        map2.insert("a".to_string(), Value::Int(1));
        map2.insert("b".to_string(), Value::Int(2));

        assert!(!Value::Map(map1).semantically_equal(&Value::Map(map2)));
    }

    #[test]
    fn merge_with_saved_map_fills_extra_keys() {
        let desired = Value::Map(HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            ("a_record".to_string(), Value::Bool(true)),
        ]));
        let saved = Value::Map(HashMap::from([
            (
                "hostname_type".to_string(),
                Value::String("ip-name".to_string()),
            ),
            ("a_record".to_string(), Value::Bool(true)),
            ("aaaa_record".to_string(), Value::Bool(false)),
        ]));

        let merged = merge_with_saved(&desired, &saved);
        let expected = Value::Map(HashMap::from([
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
        let desired = Value::Map(HashMap::from([("a".to_string(), Value::Int(10))]));
        let saved = Value::Map(HashMap::from([
            ("a".to_string(), Value::Int(5)),
            ("b".to_string(), Value::Int(20)),
        ]));

        let merged = merge_with_saved(&desired, &saved);
        let expected = Value::Map(HashMap::from([
            ("a".to_string(), Value::Int(10)),
            ("b".to_string(), Value::Int(20)),
        ]));
        assert!(merged.semantically_equal(&expected), "Merged: {:?}", merged);
    }

    #[test]
    fn merge_with_saved_list_of_maps() {
        let desired = Value::List(vec![Value::Map(HashMap::from([(
            "port".to_string(),
            Value::Int(80),
        )]))]);
        let saved = Value::List(vec![Value::Map(HashMap::from([
            ("port".to_string(), Value::Int(80)),
            ("protocol".to_string(), Value::String("tcp".to_string())),
        ]))]);

        let merged = merge_with_saved(&desired, &saved);
        let expected = Value::List(vec![Value::Map(HashMap::from([
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
            Value::Map(HashMap::from([
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
            .map(|i| Value::Map(HashMap::from([("port".to_string(), Value::Int(i))])))
            .collect();
        let saved: Vec<Value> = (0..n)
            .rev()
            .map(|i| {
                Value::Map(HashMap::from([
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
        let m1 = Value::Map(HashMap::from([
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(2)),
        ]));
        let m2 = Value::Map(HashMap::from([
            ("b".to_string(), Value::Int(2)),
            ("a".to_string(), Value::Int(1)),
        ]));
        assert_eq!(m1.canonical_hash(), m2.canonical_hash());
    }
}
