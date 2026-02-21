use serde::Deserialize;
use std::collections::BTreeMap;

/// A Smithy 2.0 JSON AST model.
#[derive(Debug, Deserialize)]
pub struct SmithyModel {
    pub smithy: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub shapes: BTreeMap<String, Shape>,
}

/// A Smithy shape, tagged by the `type` field.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Shape {
    Service(ServiceShape),
    Operation(OperationShape),
    Structure(StructureShape),
    Union(UnionShape),
    Enum(EnumShape),
    IntEnum(IntEnumShape),
    List(ListShape),
    Map(MapShape),
    String(TraitOnly),
    Boolean(TraitOnly),
    Integer(TraitOnly),
    Long(TraitOnly),
    Float(TraitOnly),
    Double(TraitOnly),
    Blob(TraitOnly),
    Timestamp(TraitOnly),
    // Resource shape (Smithy resource, not used in AWS API models but part of spec)
    Resource(ResourceShape),
}

/// Traits are represented as a map from trait ID to arbitrary JSON value.
pub type Traits = BTreeMap<String, serde_json::Value>;

/// A shape that only carries traits (primitives like string, boolean, etc.).
#[derive(Debug, Deserialize)]
pub struct TraitOnly {
    #[serde(default)]
    pub traits: Traits,
}

/// A reference to another shape (used in operation input/output, list member, etc.).
#[derive(Debug, Deserialize)]
pub struct ShapeRef {
    pub target: String,
    #[serde(default)]
    pub traits: Traits,
}

/// Service shape.
#[derive(Debug, Deserialize)]
pub struct ServiceShape {
    pub version: String,
    #[serde(default)]
    pub operations: Vec<ShapeRef>,
    #[serde(default)]
    pub resources: Vec<ShapeRef>,
    #[serde(default)]
    pub traits: Traits,
}

/// Operation shape.
#[derive(Debug, Deserialize)]
pub struct OperationShape {
    #[serde(default)]
    pub input: Option<ShapeRef>,
    #[serde(default)]
    pub output: Option<ShapeRef>,
    #[serde(default)]
    pub errors: Vec<ShapeRef>,
    #[serde(default)]
    pub traits: Traits,
}

/// Structure shape.
#[derive(Debug, Deserialize)]
pub struct StructureShape {
    #[serde(default)]
    pub members: BTreeMap<String, ShapeRef>,
    #[serde(default)]
    pub traits: Traits,
}

/// Union shape (tagged union / sum type).
#[derive(Debug, Deserialize)]
pub struct UnionShape {
    #[serde(default)]
    pub members: BTreeMap<String, ShapeRef>,
    #[serde(default)]
    pub traits: Traits,
}

/// Enum shape (string enum in Smithy 2.0).
#[derive(Debug, Deserialize)]
pub struct EnumShape {
    #[serde(default)]
    pub members: BTreeMap<String, ShapeRef>,
    #[serde(default)]
    pub traits: Traits,
}

/// Integer enum shape.
#[derive(Debug, Deserialize)]
pub struct IntEnumShape {
    #[serde(default)]
    pub members: BTreeMap<String, ShapeRef>,
    #[serde(default)]
    pub traits: Traits,
}

/// List shape.
#[derive(Debug, Deserialize)]
pub struct ListShape {
    pub member: ShapeRef,
    #[serde(default)]
    pub traits: Traits,
}

/// Map shape.
#[derive(Debug, Deserialize)]
pub struct MapShape {
    pub key: ShapeRef,
    pub value: ShapeRef,
    #[serde(default)]
    pub traits: Traits,
}

/// Resource shape (part of Smithy spec, rarely used in AWS API models).
#[derive(Debug, Deserialize)]
pub struct ResourceShape {
    #[serde(default)]
    pub identifiers: BTreeMap<String, ShapeRef>,
    #[serde(default)]
    pub traits: Traits,
}

// ── Trait constants ──

pub const TRAIT_REQUIRED: &str = "smithy.api#required";
pub const TRAIT_DOCUMENTATION: &str = "smithy.api#documentation";
pub const TRAIT_ENUM_VALUE: &str = "smithy.api#enumValue";
pub const TRAIT_INPUT: &str = "smithy.api#input";
pub const TRAIT_OUTPUT: &str = "smithy.api#output";
pub const TRAIT_PAGINATED: &str = "smithy.api#paginated";
pub const TRAIT_TITLE: &str = "smithy.api#title";
