//! Serializable protocol types for host-guest communication.
//!
//! These mirror carina-core types but are JSON-serializable across the process boundary.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_true() -> bool {
    true
}

/// Mirrors `carina_core::resource::ResourceId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceId {
    pub provider: String,
    pub resource_type: String,
    pub name: String,
}

/// Structured identity of a provider-defined custom type.
///
/// Mirrors `carina_core::schema::TypeIdentity` and the WIT
/// `type-identity` record. Keyed on discrete `provider + segments +
/// kind` axes so two providers' same-named custom types stay distinct.
/// An empty `provider` string denotes a provider-agnostic type.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TypeIdentity {
    pub provider: String,
    pub segments: Vec<String>,
    pub kind: String,
}

/// Mirrors `carina_core::resource::Value`.
///
/// Only includes variants that can cross the process boundary.
/// `ResourceRef`, `Interpolation`, `FunctionCall`, `Closure` are resolved
/// before reaching the provider, so they are excluded.
///
/// Custom Serialize/Deserialize to produce untagged JSON (strings as `"hello"`,
/// ints as `42`, etc.) without hitting serde's recursive monomorphization limit.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Map(HashMap<String, Value>),
}

impl Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Int(i) => serializer.serialize_i64(*i),
            Value::Float(f) => serializer.serialize_f64(*f),
            Value::String(s) => serializer.serialize_str(s),
            Value::List(l) => {
                use serde::ser::SerializeSeq;
                let mut seq = serializer.serialize_seq(Some(l.len()))?;
                for v in l {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            Value::Map(m) => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(m.len()))?;
                for (k, v) in m {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let json = serde_json::Value::deserialize(deserializer)?;
        Ok(Value::from_json(json))
    }
}

impl Value {
    fn from_json(json: serde_json::Value) -> Self {
        match json {
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else {
                    Value::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => Value::String(s),
            serde_json::Value::Array(a) => {
                Value::List(a.into_iter().map(Value::from_json).collect())
            }
            serde_json::Value::Object(m) => Value::Map(
                m.into_iter()
                    .map(|(k, v)| (k, Value::from_json(v)))
                    .collect(),
            ),
            serde_json::Value::Null => Value::String(String::new()),
        }
    }
}

/// Mirrors `carina_core::resource::State`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub id: ResourceId,
    pub identifier: Option<String>,
    pub attributes: HashMap<String, Value>,
    pub exists: bool,
}

/// Kind of a single [`PatchOp`]. Mirrors `patch-op-kind` in
/// `wit/types.wit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchOpKind {
    Add,
    Replace,
    Remove,
}

/// One operation inside an [`UpdatePatch`]. Mirrors `patch-op` in
/// `wit/types.wit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchOp {
    pub kind: PatchOpKind,
    pub key: String,
    /// `Some(_)` for `Add`/`Replace`; `None` for `Remove`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// Structured description of the user's intended change to a resource.
/// Mirrors `update-patch` in `wit/types.wit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdatePatch {
    #[serde(default)]
    pub ops: Vec<PatchOp>,
}

/// Per-operation request record for `read`. Mirrors `read-request`
/// in `wit/types.wit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReadRequest;

/// Per-operation request record for `create`. Mirrors
/// `create-request` in `wit/types.wit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRequest {
    pub resource: Resource,
}

/// Per-operation request record for `update`. Mirrors
/// `update-request` in `wit/types.wit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateRequest {
    pub from: State,
    pub patch: UpdatePatch,
}

/// Per-operation request record for `delete`. Mirrors
/// `delete-request` in `wit/types.wit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeleteRequest {
    #[serde(default)]
    pub directives: Directives,
}

/// Carina-side directives for a resource. Mirrors `directives` in
/// `wit/types.wit`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Directives {
    #[serde(default)]
    pub force_delete: bool,
    #[serde(default)]
    pub create_before_destroy: bool,
    #[serde(default)]
    pub prevent_destroy: bool,
}

/// Simplified resource for the process boundary.
/// Attributes are pre-resolved `Value`s, not `Expr`s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    pub attributes: HashMap<String, Value>,
    #[serde(default)]
    pub directives: Directives,
}

/// Provider metadata returned by `provider_info`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub version: String,
}

/// Wire envelope for info(): carries the provider's metadata plus the
/// PROTOCOL_VERSION the SDK was compiled against, so the host can
/// reject a provider built against an older protocol (carina#3365).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfoEnvelope {
    #[serde(flatten)]
    pub info: ProviderInfo,
    /// Defaults to 0 for providers predating the envelope, so the host
    /// detects them as below any minimum.
    #[serde(default)]
    pub protocol_version: u32,
}

/// Completion value for LSP completions, serializable for WIT transport.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionValue {
    pub value: String,
    pub description: String,
}

/// Variant tag of [`ProviderError`].
///
/// Mirrors `provider-error` in `wit/types.wit`. Carried as a separate
/// `kind` field so the wire format stays a flat record (easier to
/// inspect than a serde-tagged enum, and matches the WIT variant
/// directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    InvalidInput,
    ApiError,
    NotFound,
    Timeout,
    #[default]
    Internal,
}

/// Provider error returned from operations.
///
/// Mirrors `(provider-error, error-detail)` in `wit/types.wit`. The
/// variant lives in [`ProviderErrorKind`]; the metadata fields mirror
/// `error-detail`.
///
/// The `operation` / `status` / `code` / `request_id` quartet carries
/// cloud-API metadata in a cloud-agnostic shape so the host can render
/// a multi-line, labeled error display without providers doing ad-hoc
/// string formatting. All four are optional — providers leave them
/// empty when the error doesn't come from an HTTP cloud API call, and
/// the host falls back to the legacy chain-walking render in that
/// case. See carina-rs/carina#3242 for the full design.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderError {
    #[serde(default)]
    pub kind: ProviderErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<ResourceId>,
    /// Flattened representation of the underlying cause chain, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// Provider name (e.g. `"aws"`, `"awscc"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    /// Service-qualified cloud-API operation that failed
    /// (e.g. `"iam.ListRoles"`, `"s3.HeadBucket"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// HTTP status code from the cloud-API response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Application-level error code (e.g. `"AccessDenied"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Correlation id from the cloud-API response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Serializable validator types that can cross the WASM boundary.
///
/// Function-pointer validators (`ResourceSchema.validator`) are lost during
/// WASM serialization. This enum allows providers to declare validators as
/// data, which the host reconstructs into actual validator functions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorType {
    /// Check that a `tags` map does not use Key/Value pair list structure.
    TagsKeyValueCheck,
}

/// Classification of a schema in the provider protocol: managed (full CRUD
/// lifecycle) vs data source (read-only lookup of existing infrastructure).
///
/// Mirror of `carina_core::schema::SchemaKind`. Defined here independently to
/// keep `carina-provider-protocol` free of a dependency on `carina-core`
/// (the protocol crate is the lightweight contract layer that providers
/// link against).
///
/// `Default` is derived to back `#[serde(default)]` on `ResourceSchema::kind`,
/// so JSON payloads from older or minimal producers omit the field and
/// fall back to `Managed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaKind {
    #[default]
    Managed,
    DataSource,
}

/// Schema types for resource validation and completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSchema {
    pub resource_type: String,
    pub attributes: HashMap<String, AttributeSchema>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub kind: SchemaKind,
    #[serde(default)]
    pub name_attribute: Option<String>,
    #[serde(default)]
    pub force_replace: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_config: Option<OperationConfig>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validators: Vec<ValidatorType>,
    /// Declarative "exactly one of" groups. Each inner vec is a group of
    /// attribute names where exactly one must be specified. Survives
    /// serialization across the WASM plugin boundary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclusive_required: Vec<Vec<String>>,
    /// Named definitions reachable via [`AttributeType::Ref`] from
    /// this resource's attribute types. Empty for resources whose
    /// attribute graph contains no cycles (the common case). Mirror
    /// of [`carina_core::schema::ResourceSchema::defs`] — carries the
    /// cyclic CFN struct map across the JSON wire form so the host
    /// can resolve `Ref` targets at the walk-sites that need them
    /// (carina#3340).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub defs: std::collections::BTreeMap<String, AttributeType>,
}

/// Per-resource operational configuration for timeouts and retries.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OperationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_max_retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create_max_retries: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributeSchema {
    pub name: String,
    pub attr_type: AttributeType,
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub create_only: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub write_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_name: Option<String>,
    /// Provider-side property name (e.g., "VpcId" for AWS Cloud Control)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    /// Override for removability detection.
    /// `None` = auto-detect, `Some(false)` = explicitly non-removable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removable: Option<bool>,
    /// Whether this attribute contributes to anonymous resource identity hashing.
    #[serde(default)]
    pub identity: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AttributeType {
    String,
    Int,
    Float,
    Bool,
    /// Time duration. Authored in the DSL as a literal like `30min`,
    /// `1h`, or `15s` (`duration_literal` in the grammar); reaches the
    /// provider across the WIT boundary as integer seconds. Lets
    /// providers declare Duration-typed schema attributes so the
    /// host-side type checker accepts the literal form (carina#3166).
    Duration,
    #[serde(rename = "string_enum")]
    StringEnum {
        values: Vec<String>,
        #[serde(default)]
        name: String,
        /// DSL namespace prefix for enum validation (e.g., `"awscc"`).
        /// When present, values may be written as
        /// `{namespace}.{name}.{value}` in the DSL.
        ///
        /// Carried as a flat dotted string over the JSON wire form;
        /// the host reconstructs the structured
        /// [`carina_core::schema::TypeIdentity`] via
        /// `string_enum_identity(name, namespace.as_deref())` when
        /// projecting back into the core schema. Pre-#3222 the core
        /// `StringEnum` carried the same flat string; with #3222 the
        /// core form carries a structured identity but the wire form
        /// stays flat — splitting the boundary cost in one place.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        /// API-canonical -> DSL spelling pairs, for every value where the
        /// two differ. The host-side validator accepts both spellings.
        /// Empty means the API spelling IS the DSL spelling for every
        /// value, which is also the value used by older provider
        /// components that predate this field (carina#2831).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dsl_aliases: Vec<(String, String)>,
    },
    #[serde(rename = "list")]
    List {
        inner: Box<AttributeType>,
        /// Whether list elements are ordered (positional comparison).
        /// When false, elements are compared as multisets (order-insensitive).
        #[serde(default = "default_true")]
        ordered: bool,
    },
    #[serde(rename = "map")]
    Map {
        inner: Box<AttributeType>,
        /// Key type constraint for map keys.
        key: Box<AttributeType>,
    },
    #[serde(rename = "struct")]
    Struct {
        name: String,
        fields: Vec<StructField>,
    },
    #[serde(rename = "union")]
    Union {
        members: Vec<AttributeType>,
    },
    /// Structurally-validated custom type: values carry their own
    /// format (`arn:aws:s3:::bucket-name`, `vpc-12345678`) and reach
    /// the host-side validator verbatim. Sibling of
    /// [`AttributeType::CustomEnum`].
    ///
    /// The pre-#3222 wire shape carried a single `Custom` variant
    /// with a runtime `namespace: Option<String>` flag distinguishing
    /// enum-shaped vs structural; the variant split makes that a
    /// type-level fact (see the corresponding split in
    /// `carina_core::schema::AttributeType`).
    #[serde(rename = "custom")]
    Custom {
        name: String,
        base: Box<AttributeType>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        length: Option<(Option<u64>, Option<u64>)>,
    },
    /// Enum-shaped custom type: values are written as namespaced
    /// shorthand and expanded host-side via `expand_enum_shorthand`
    /// before the validator runs. `namespace` is the dotted prefix
    /// (`"aws"`, `"awscc.s3.Bucket"`) — the host reconstructs the
    /// structured [`carina_core::schema::TypeIdentity`] from
    /// `(name, namespace)`.
    #[serde(rename = "custom_enum")]
    CustomEnum {
        name: String,
        base: Box<AttributeType>,
        namespace: String,
        /// Data-driven API-to-DSL transform. The host applies the
        /// carried operation directly, so provider plugins do not need
        /// to register host-process callbacks across the WASM boundary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dsl_transform: Option<DslTransform>,
    },
    /// Named reference into the enclosing [`ResourceSchema::defs`]
    /// map. Used to express cyclic CFN definition graphs (WAFv2
    /// `WebACL.Statement`, AppSync `GraphQLApi`, ...) so the codegen
    /// emission step does not stack-overflow on recursive struct
    /// definitions (`Statement -> AndStatement -> List<Statement>`).
    ///
    /// Introduced in carina#3340. The host-side
    /// [`carina_core::schema::AttributeType::Ref`] is the structural
    /// counterpart; conversion in `carina-plugin-host::wasm_convert`
    /// passes the variant through unchanged.
    #[serde(rename = "ref")]
    Ref {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum DslTransform {
    Identity,
    HyphenToUnderscore,
    StripSuffix(String),
    ReplaceTable(Vec<(String, String)>),
    Unknown(serde_json::Value),
}

impl DslTransform {
    pub fn apply(&self, s: &str) -> String {
        match self {
            Self::Identity | Self::Unknown(_) => s.to_string(),
            Self::HyphenToUnderscore => s.replace('-', "_"),
            Self::StripSuffix(suffix) => s.strip_suffix(suffix.as_str()).unwrap_or(s).to_string(),
            Self::ReplaceTable(table) => table
                .iter()
                .find(|(k, _)| k == s)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| s.to_string()),
        }
    }
}

impl Serialize for DslTransform {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = match self {
            Self::Identity => serde_json::json!({ "type": "Identity" }),
            Self::HyphenToUnderscore => serde_json::json!({ "type": "HyphenToUnderscore" }),
            Self::StripSuffix(suffix) => {
                serde_json::json!({ "type": "StripSuffix", "value": suffix })
            }
            Self::ReplaceTable(table) => {
                serde_json::json!({ "type": "ReplaceTable", "value": table })
            }
            Self::Unknown(raw) => raw.clone(),
        };
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DslTransform {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        let tag = raw
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| serde::de::Error::custom("DslTransform missing string `type` tag"))?;

        match tag {
            "Identity" => Ok(Self::Identity),
            "HyphenToUnderscore" => Ok(Self::HyphenToUnderscore),
            "StripSuffix" => {
                let value = raw
                    .get("value")
                    .cloned()
                    .ok_or_else(|| serde::de::Error::custom("StripSuffix missing `value`"))?;
                serde_json::from_value(value)
                    .map(Self::StripSuffix)
                    .map_err(serde::de::Error::custom)
            }
            "ReplaceTable" => {
                let value = raw
                    .get("value")
                    .cloned()
                    .ok_or_else(|| serde::de::Error::custom("ReplaceTable missing `value`"))?;
                serde_json::from_value(value)
                    .map(Self::ReplaceTable)
                    .map_err(serde::de::Error::custom)
            }
            _ => Ok(Self::Unknown(raw)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructField {
    pub name: String,
    pub field_type: AttributeType,
    pub required: bool,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_name: Option<String>,
    /// Provider-side property name (e.g., "IpProtocol" for AWS Cloud Control)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_roundtrip() {
        let values = vec![
            Value::String("hello".into()),
            Value::Int(42),
            Value::Float(2.71),
            Value::Bool(true),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::Map(HashMap::from([("key".into(), Value::String("val".into()))])),
        ];

        for value in values {
            let json = serde_json::to_string(&value).unwrap();
            let back: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value, back);
        }
    }

    #[test]
    fn test_state_roundtrip() {
        let state = State {
            id: ResourceId {
                provider: "mock".into(),
                resource_type: "test.resource".into(),
                name: "my-resource".into(),
            },
            identifier: Some("mock-id".into()),
            attributes: HashMap::from([("name".into(), Value::String("test".into()))]),
            exists: true,
        };

        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(state.id, back.id);
        assert_eq!(state.identifier, back.identifier);
        assert_eq!(state.exists, back.exists);
    }

    #[test]
    fn test_attribute_type_roundtrip() {
        let attr = AttributeType::Struct {
            name: "Config".into(),
            fields: vec![StructField {
                name: "enabled".into(),
                field_type: AttributeType::Bool,
                required: true,
                description: None,
                block_name: None,
                provider_name: None,
            }],
        };

        let json = serde_json::to_string(&attr).unwrap();
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        assert_eq!(json, serde_json::to_string(&back).unwrap());
    }

    #[test]
    fn ref_attribute_type_roundtrip() {
        // carina#3340: cyclic CFN schemas emit `Ref { name }` at the
        // recursion point; the wire form must round-trip through
        // serde so the host can rebuild the structural Ref variant.
        let attr = AttributeType::Ref {
            name: "Statement".into(),
        };
        let json = serde_json::to_string(&attr).unwrap();
        assert_eq!(json, r#"{"type":"ref","name":"Statement"}"#);
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        match back {
            AttributeType::Ref { name } => assert_eq!(name, "Statement"),
            other => panic!("expected Ref, got {:?}", other),
        }
    }

    #[test]
    fn custom_pattern_and_length_round_trip() {
        let attr = AttributeType::Custom {
            name: "awscc.wafv2.WebACL.EntityDescription".to_string(),
            base: Box::new(AttributeType::String),
            pattern: Some("^[a-z]+$".to_string()),
            length: Some((Some(1), Some(9))),
        };

        let json = serde_json::to_string(&attr).unwrap();
        assert!(json.contains("pattern"));
        assert!(json.contains("length"));
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        match back {
            AttributeType::Custom {
                pattern, length, ..
            } => {
                assert_eq!(pattern.as_deref(), Some("^[a-z]+$"));
                assert_eq!(length, Some((Some(1), Some(9))));
            }
            other => panic!("expected Custom, got {:?}", other),
        }
    }

    #[test]
    fn resource_schema_defs_roundtrip() {
        // carina#3340: the `defs` map carries cyclic struct definitions
        // (`Statement` -> `AndStatement` -> `List<Statement>` ...).
        // Pin the wire shape so a host-side regression cannot silently
        // lose recursion-target schema information.
        let schema = ResourceSchema {
            resource_type: "awscc.wafv2.WebACL".into(),
            attributes: HashMap::new(),
            description: None,
            kind: SchemaKind::Managed,
            name_attribute: None,
            force_replace: false,
            operation_config: None,
            validators: vec![],
            exclusive_required: vec![],
            defs: std::collections::BTreeMap::from([(
                "Statement".to_string(),
                AttributeType::Struct {
                    name: "Statement".into(),
                    fields: vec![StructField {
                        name: "and_statement".into(),
                        field_type: AttributeType::List {
                            inner: Box::new(AttributeType::Ref {
                                name: "Statement".into(),
                            }),
                            ordered: true,
                        },
                        required: false,
                        description: None,
                        block_name: None,
                        provider_name: None,
                    }],
                },
            )]),
        };
        let json = serde_json::to_string(&schema).unwrap();
        let back: ResourceSchema = serde_json::from_str(&json).unwrap();
        assert_eq!(back.defs.len(), 1, "defs map lost during round-trip");
        assert!(back.defs.contains_key("Statement"));
    }

    #[test]
    fn duration_attribute_type_serializes_as_duration_tag() {
        // Providers serialize this via serde_json across the WIT
        // boundary; the host must reconstruct it from the
        // `{"type": "Duration"}` form. Pinning the wire shape is the
        // only thing that keeps the cross-boundary contract stable
        // (carina#3166).
        let attr = AttributeType::Duration;
        let json = serde_json::to_string(&attr).unwrap();
        assert_eq!(json, r#"{"type":"Duration"}"#);
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AttributeType::Duration));
    }

    #[test]
    fn test_provider_info_with_capabilities() {
        let info = ProviderInfo {
            name: "test".into(),
            display_name: "Test Provider".into(),
            capabilities: vec!["normalize_desired".into(), "normalize_state".into()],
            version: "1.2.3".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: ProviderInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.capabilities,
            vec!["normalize_desired", "normalize_state"]
        );
    }

    #[test]
    fn test_provider_info_without_capabilities_defaults_to_empty() {
        // Simulates deserializing a response from a plugin that doesn't send capabilities
        let json = r#"{"name":"old","display_name":"Old Provider","version":"1.0.0"}"#;
        let info: ProviderInfo = serde_json::from_str(json).unwrap();
        assert!(info.capabilities.is_empty());
    }

    #[test]
    fn test_provider_info_envelope_flat_roundtrip_and_default_protocol_version() {
        let envelope = ProviderInfoEnvelope {
            info: ProviderInfo {
                name: "test".into(),
                display_name: "Test Provider".into(),
                capabilities: vec!["normalize_desired".into()],
                version: "1.2.3".into(),
            },
            protocol_version: crate::PROTOCOL_VERSION,
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["name"], "test");
        assert_eq!(value["display_name"], "Test Provider");
        assert_eq!(value["capabilities"][0], "normalize_desired");
        assert_eq!(value["version"], "1.2.3");
        assert_eq!(value["protocol_version"], crate::PROTOCOL_VERSION);

        let back: ProviderInfoEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.info.name, "test");
        assert_eq!(back.info.display_name, "Test Provider");
        assert_eq!(back.info.capabilities, vec!["normalize_desired"]);
        assert_eq!(back.info.version, "1.2.3");
        assert_eq!(back.protocol_version, crate::PROTOCOL_VERSION);

        let old_json = r#"{"name":"old","display_name":"Old Provider","version":"1.0.0"}"#;
        let old: ProviderInfoEnvelope = serde_json::from_str(old_json).unwrap();
        assert_eq!(old.info.name, "old");
        assert_eq!(old.protocol_version, 0);
    }

    #[test]
    fn test_union_type_roundtrip() {
        let attr = AttributeType::Union {
            members: vec![
                AttributeType::Struct {
                    name: "IamPolicyPrincipal".into(),
                    fields: vec![StructField {
                        name: "service".into(),
                        field_type: AttributeType::String,
                        required: false,
                        description: None,
                        block_name: None,
                        provider_name: None,
                    }],
                },
                AttributeType::String,
            ],
        };

        let json = serde_json::to_string(&attr).unwrap();
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        assert_eq!(json, serde_json::to_string(&back).unwrap());
    }

    #[test]
    fn test_operation_config_serialization() {
        let config = OperationConfig {
            delete_timeout_secs: Some(1800),
            delete_max_retries: Some(24),
            create_timeout_secs: None,
            create_max_retries: None,
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"delete_timeout_secs\":1800"));
        assert!(!json.contains("create_timeout_secs"));

        let back: OperationConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.delete_timeout_secs, Some(1800));
        assert_eq!(back.create_timeout_secs, None);
    }

    #[test]
    fn test_resource_schema_with_operation_config() {
        let json = r#"{"resource_type":"ec2.tgw","attributes":{},"operation_config":{"delete_timeout_secs":1800}}"#;
        let schema: ResourceSchema = serde_json::from_str(json).unwrap();
        assert_eq!(
            schema.operation_config.unwrap().delete_timeout_secs,
            Some(1800)
        );
    }

    #[test]
    fn test_resource_schema_without_operation_config() {
        let json = r#"{"resource_type":"ec2.Vpc","attributes":{}}"#;
        let schema: ResourceSchema = serde_json::from_str(json).unwrap();
        assert!(schema.operation_config.is_none());
    }

    /// carina#2831: `StringEnum.dsl_aliases` carries the API->DSL spelling
    /// map that the host-side validator consults. Round-trips through
    /// JSON so a WASM provider's serialized schema reaches the host with
    /// the field intact.
    #[test]
    fn string_enum_dsl_aliases_round_trip() {
        let attr = AttributeType::StringEnum {
            name: "ObjectOwnership".to_string(),
            values: vec![
                "ObjectWriter".to_string(),
                "BucketOwnerEnforced".to_string(),
            ],
            namespace: Some("awscc.s3.Bucket".to_string()),
            dsl_aliases: vec![
                ("ObjectWriter".to_string(), "object_writer".to_string()),
                (
                    "BucketOwnerEnforced".to_string(),
                    "bucket_owner_enforced".to_string(),
                ),
            ],
        };
        let json = serde_json::to_string(&attr).unwrap();
        assert!(json.contains("dsl_aliases"));
        assert!(json.contains("bucket_owner_enforced"));
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        match back {
            AttributeType::StringEnum {
                dsl_aliases: aliases,
                ..
            } => {
                assert_eq!(aliases.len(), 2);
                assert!(
                    aliases
                        .iter()
                        .any(|(a, d)| a == "BucketOwnerEnforced" && d == "bucket_owner_enforced"),
                    "alias pair lost in round trip: {aliases:?}"
                );
            }
            other => panic!("expected StringEnum, got {other:?}"),
        }
    }

    /// `dsl_aliases` defaults to empty so older WASM providers that
    /// predate this field (carina#2831 stage 1) still parse — they
    /// emit no `dsl_aliases` key and the host treats every value as
    /// already in DSL form.
    #[test]
    fn string_enum_without_dsl_aliases_defaults_empty() {
        let json = r#"{"type":"string_enum","values":["A","B"],"name":"X"}"#;
        let attr: AttributeType = serde_json::from_str(json).unwrap();
        match attr {
            AttributeType::StringEnum {
                dsl_aliases: aliases,
                ..
            } => {
                assert!(
                    aliases.is_empty(),
                    "missing dsl_aliases must deserialize as empty vec"
                );
            }
            other => panic!("expected StringEnum, got {other:?}"),
        }
    }

    /// Empty `dsl_aliases` skips serialization so the wire format
    /// stays compact for the common case (no aliases) and older host
    /// builds (which do not know the field) round-trip unchanged.
    #[test]
    fn string_enum_empty_dsl_aliases_skipped_on_serialize() {
        let attr = AttributeType::StringEnum {
            name: "X".to_string(),
            values: vec!["A".to_string()],
            namespace: None,
            dsl_aliases: vec![],
        };
        let json = serde_json::to_string(&attr).unwrap();
        assert!(
            !json.contains("dsl_aliases"),
            "empty dsl_aliases must be omitted, got: {json}"
        );
    }

    #[test]
    fn custom_enum_dsl_transform_round_trip() {
        let transforms = [
            DslTransform::Identity,
            DslTransform::HyphenToUnderscore,
            DslTransform::StripSuffix(".".to_string()),
            DslTransform::ReplaceTable(vec![("all".to_string(), "-1".to_string())]),
        ];

        for transform in transforms {
            let attr = AttributeType::CustomEnum {
                name: "ZoneName".to_string(),
                base: Box::new(AttributeType::String),
                namespace: "aws.AvailabilityZone".to_string(),
                dsl_transform: Some(transform.clone()),
            };
            let json = serde_json::to_string(&attr).unwrap();
            let back: AttributeType = serde_json::from_str(&json).unwrap();
            match back {
                AttributeType::CustomEnum { dsl_transform, .. } => {
                    assert_eq!(dsl_transform, Some(transform));
                }
                other => panic!("expected CustomEnum, got {other:?}"),
            }
        }
    }

    #[test]
    fn dsl_transform_replace_table_applies_first_match() {
        let transform = DslTransform::ReplaceTable(vec![
            ("-1".to_string(), "all".to_string()),
            ("tcp".to_string(), "tcp".to_string()),
        ]);
        assert_eq!(transform.apply("-1"), "all");
        assert_eq!(transform.apply("udp"), "udp");
    }

    #[test]
    fn dsl_transform_unknown_unit_deserializes_as_identity() {
        let json = r#"{"type":"FutureTransform"}"#;
        let transform: DslTransform = serde_json::from_str(json).unwrap();
        assert_eq!(
            transform,
            DslTransform::Unknown(serde_json::json!({"type":"FutureTransform"}))
        );
        assert_eq!(transform.apply("foo.bar."), "foo.bar.");
    }

    #[test]
    fn dsl_transform_unknown_sequence_payload_deserializes_as_identity() {
        let json = r#"{"type":"FutureTransform","value":[["a","b"]]}"#;
        let transform: DslTransform = serde_json::from_str(json).unwrap();
        assert_eq!(
            transform,
            DslTransform::Unknown(serde_json::json!({
                "type": "FutureTransform",
                "value": [["a", "b"]]
            }))
        );
        assert_eq!(transform.apply("foo.bar."), "foo.bar.");
    }

    #[test]
    fn custom_enum_unknown_sequence_payload_deserializes_without_schema_failure() {
        let json = r#"{
            "type":"custom_enum",
            "name":"ZoneName",
            "base":{"type":"String"},
            "namespace":"aws.AvailabilityZone",
            "dsl_transform":{"type":"FutureTransform","value":[["a","b"]]}
        }"#;
        let attr: AttributeType = serde_json::from_str(json).unwrap();
        match attr {
            AttributeType::CustomEnum { dsl_transform, .. } => {
                assert!(matches!(
                    dsl_transform,
                    Some(DslTransform::Unknown(raw))
                        if raw == serde_json::json!({
                            "type": "FutureTransform",
                            "value": [["a", "b"]]
                        })
                ));
            }
            other => panic!("expected CustomEnum, got {other:?}"),
        }
    }

    #[test]
    fn custom_enum_unknown_struct_payload_deserializes_without_schema_failure() {
        let json = r#"{
            "type":"custom_enum",
            "name":"ZoneName",
            "base":{"type":"String"},
            "namespace":"aws.AvailabilityZone",
            "dsl_transform":{"type":"Other","value":{"x":1}}
        }"#;
        let attr: AttributeType = serde_json::from_str(json).unwrap();
        match attr {
            AttributeType::CustomEnum { dsl_transform, .. } => {
                assert!(matches!(
                    dsl_transform,
                    Some(DslTransform::Unknown(raw))
                        if raw == serde_json::json!({
                            "type": "Other",
                            "value": {"x": 1}
                        })
                ));
            }
            other => panic!("expected CustomEnum, got {other:?}"),
        }
    }

    #[test]
    fn dsl_transform_unknown_serializes_losslessly() {
        let raw = serde_json::json!({
            "type": "FutureTransform",
            "value": [["a", "b"]]
        });
        let transform = DslTransform::Unknown(raw.clone());
        let json = serde_json::to_string(&transform).unwrap();
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(back, raw);
    }
}
