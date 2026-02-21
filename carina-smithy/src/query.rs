use crate::ast::*;

impl SmithyModel {
    /// Look up a shape by its full shape ID (e.g. `com.amazonaws.ec2#Vpc`).
    pub fn get_shape(&self, id: &str) -> Option<&Shape> {
        self.shapes.get(id)
    }

    /// Get a structure shape by ID. Returns `None` if the shape doesn't exist
    /// or isn't a structure.
    pub fn get_structure(&self, id: &str) -> Option<&StructureShape> {
        match self.get_shape(id)? {
            Shape::Structure(s) => Some(s),
            _ => None,
        }
    }

    /// Get an operation shape by ID.
    pub fn get_operation(&self, id: &str) -> Option<&OperationShape> {
        match self.get_shape(id)? {
            Shape::Operation(op) => Some(op),
            _ => None,
        }
    }

    /// Get an enum shape by ID.
    pub fn get_enum(&self, id: &str) -> Option<&EnumShape> {
        match self.get_shape(id)? {
            Shape::Enum(e) => Some(e),
            _ => None,
        }
    }

    /// Get the service shape by ID.
    pub fn get_service(&self, id: &str) -> Option<&ServiceShape> {
        match self.get_shape(id)? {
            Shape::Service(s) => Some(s),
            _ => None,
        }
    }

    /// Get the input structure for an operation.
    pub fn operation_input(&self, op_id: &str) -> Option<&StructureShape> {
        let op = self.get_operation(op_id)?;
        let input_ref = op.input.as_ref()?;
        self.get_structure(&input_ref.target)
    }

    /// Get the output structure for an operation.
    pub fn operation_output(&self, op_id: &str) -> Option<&StructureShape> {
        let op = self.get_operation(op_id)?;
        let output_ref = op.output.as_ref()?;
        // smithy.api#Unit means no output
        if output_ref.target == "smithy.api#Unit" {
            return None;
        }
        self.get_structure(&output_ref.target)
    }

    /// Get the input shape ID for an operation.
    pub fn operation_input_id(&self, op_id: &str) -> Option<&str> {
        let op = self.get_operation(op_id)?;
        Some(op.input.as_ref()?.target.as_str())
    }

    /// Get the output shape ID for an operation.
    pub fn operation_output_id(&self, op_id: &str) -> Option<&str> {
        let op = self.get_operation(op_id)?;
        let output_ref = op.output.as_ref()?;
        if output_ref.target == "smithy.api#Unit" {
            return None;
        }
        Some(output_ref.target.as_str())
    }

    /// Extract enum string values from an enum shape.
    /// Returns a list of (member_name, enum_value) pairs.
    pub fn enum_values(&self, id: &str) -> Option<Vec<(String, String)>> {
        let enum_shape = self.get_enum(id)?;
        let mut values = Vec::new();
        for (name, member) in &enum_shape.members {
            if let Some(val) = member.traits.get(TRAIT_ENUM_VALUE)
                && let Some(s) = val.as_str()
            {
                values.push((name.clone(), s.to_string()));
            }
        }
        Some(values)
    }

    /// List all operation shape IDs for a service.
    pub fn service_operations(&self, service_id: &str) -> Option<Vec<&str>> {
        let service = self.get_service(service_id)?;
        Some(
            service
                .operations
                .iter()
                .map(|r| r.target.as_str())
                .collect(),
        )
    }

    /// Find the service shape ID in this model. Returns the first service found.
    pub fn find_service(&self) -> Option<(&str, &ServiceShape)> {
        for (id, shape) in &self.shapes {
            if let Shape::Service(s) = shape {
                return Some((id.as_str(), s));
            }
        }
        None
    }

    /// Check if a member has the `smithy.api#required` trait.
    pub fn is_required(member: &ShapeRef) -> bool {
        member.traits.contains_key(TRAIT_REQUIRED)
    }

    /// Extract documentation from traits.
    pub fn documentation(traits: &Traits) -> Option<&str> {
        traits.get(TRAIT_DOCUMENTATION)?.as_str()
    }

    /// Check if a structure has the `smithy.api#input` trait.
    pub fn is_input(structure: &StructureShape) -> bool {
        structure.traits.contains_key(TRAIT_INPUT)
    }

    /// Check if a structure has the `smithy.api#output` trait.
    pub fn is_output(structure: &StructureShape) -> bool {
        structure.traits.contains_key(TRAIT_OUTPUT)
    }

    /// Resolve the base type name from a shape ID.
    /// e.g. `com.amazonaws.ec2#String` -> `String`
    /// e.g. `com.amazonaws.ec2#Tenancy` -> `Tenancy`
    pub fn shape_name(id: &str) -> &str {
        id.rsplit_once('#').map(|(_, name)| name).unwrap_or(id)
    }

    /// Get the namespace from a shape ID.
    /// e.g. `com.amazonaws.ec2#String` -> `com.amazonaws.ec2`
    pub fn shape_namespace(id: &str) -> &str {
        id.split_once('#').map(|(ns, _)| ns).unwrap_or(id)
    }

    /// Determine the kind of a shape (for type mapping purposes).
    pub fn shape_kind(&self, id: &str) -> Option<ShapeKind> {
        // Handle well-known Smithy prelude types
        match id {
            "smithy.api#String" => return Some(ShapeKind::String),
            "smithy.api#Boolean" | "smithy.api#PrimitiveBoolean" => {
                return Some(ShapeKind::Boolean);
            }
            "smithy.api#Integer" | "smithy.api#PrimitiveInteger" => {
                return Some(ShapeKind::Integer);
            }
            "smithy.api#Long" | "smithy.api#PrimitiveLong" => return Some(ShapeKind::Long),
            "smithy.api#Float" | "smithy.api#PrimitiveFloat" => return Some(ShapeKind::Float),
            "smithy.api#Double" | "smithy.api#PrimitiveDouble" => return Some(ShapeKind::Double),
            "smithy.api#Blob" => return Some(ShapeKind::Blob),
            "smithy.api#Timestamp" => return Some(ShapeKind::Timestamp),
            "smithy.api#Unit" => return Some(ShapeKind::Unit),
            _ => {}
        }

        match self.get_shape(id)? {
            Shape::Service(_) => Some(ShapeKind::Service),
            Shape::Operation(_) => Some(ShapeKind::Operation),
            Shape::Structure(_) => Some(ShapeKind::Structure),
            Shape::Union(_) => Some(ShapeKind::Union),
            Shape::Enum(_) => Some(ShapeKind::Enum),
            Shape::IntEnum(_) => Some(ShapeKind::IntEnum),
            Shape::List(_) => Some(ShapeKind::List),
            Shape::Map(_) => Some(ShapeKind::Map),
            Shape::String(_) => Some(ShapeKind::String),
            Shape::Boolean(_) => Some(ShapeKind::Boolean),
            Shape::Integer(_) => Some(ShapeKind::Integer),
            Shape::Long(_) => Some(ShapeKind::Long),
            Shape::Float(_) => Some(ShapeKind::Float),
            Shape::Double(_) => Some(ShapeKind::Double),
            Shape::Blob(_) => Some(ShapeKind::Blob),
            Shape::Timestamp(_) => Some(ShapeKind::Timestamp),
            Shape::Resource(_) => Some(ShapeKind::Resource),
        }
    }
}

/// Classification of shape types for type mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    Service,
    Operation,
    Structure,
    Union,
    Enum,
    IntEnum,
    List,
    Map,
    String,
    Boolean,
    Integer,
    Long,
    Float,
    Double,
    Blob,
    Timestamp,
    Resource,
    Unit,
}
