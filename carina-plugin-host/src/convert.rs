//! Conversions between carina-core types and carina-provider-protocol types.

use std::collections::HashMap;

use carina_core::resource::{
    LifecycleConfig as CoreLifecycle, Resource as CoreResource, ResourceId as CoreResourceId,
    State as CoreState, Value as CoreValue,
};
use carina_core::schema::{
    AttributeSchema as CoreAttributeSchema, AttributeType as CoreAttributeType,
    ResourceSchema as CoreResourceSchema, StructField as CoreStructField,
};
use carina_provider_protocol::types::{
    AttributeSchema as ProtoAttributeSchema, AttributeType as ProtoAttributeType,
    LifecycleConfig as ProtoLifecycle, Resource as ProtoResource, ResourceId as ProtoResourceId,
    ResourceSchema as ProtoResourceSchema, State as ProtoState, StructField as ProtoStructField,
    Value as ProtoValue,
};

// -- ResourceId --

pub fn core_to_proto_resource_id(id: &CoreResourceId) -> ProtoResourceId {
    ProtoResourceId {
        provider: id.provider.clone(),
        resource_type: id.resource_type.clone(),
        name: id.name.clone(),
    }
}

pub fn proto_to_core_resource_id(id: &ProtoResourceId) -> CoreResourceId {
    CoreResourceId::with_provider(&id.provider, &id.resource_type, &id.name)
}

// -- Value --

pub fn core_to_proto_value(v: &CoreValue) -> ProtoValue {
    match v {
        CoreValue::String(s) => ProtoValue::String(s.clone()),
        CoreValue::Int(i) => ProtoValue::Int(*i),
        CoreValue::Float(f) => ProtoValue::Float(*f),
        CoreValue::Bool(b) => ProtoValue::Bool(*b),
        CoreValue::List(l) => ProtoValue::List(l.iter().map(core_to_proto_value).collect()),
        CoreValue::Map(m) => ProtoValue::Map(
            m.iter()
                .map(|(k, v)| (k.clone(), core_to_proto_value(v)))
                .collect(),
        ),
        // ResourceRef, Interpolation, FunctionCall, Closure, Secret
        // should be resolved before reaching the provider.
        _ => ProtoValue::String(format!("{v:?}")),
    }
}

pub fn proto_to_core_value(v: &ProtoValue) -> CoreValue {
    match v {
        ProtoValue::String(s) => CoreValue::String(s.clone()),
        ProtoValue::Int(i) => CoreValue::Int(*i),
        ProtoValue::Float(f) => CoreValue::Float(*f),
        ProtoValue::Bool(b) => CoreValue::Bool(*b),
        ProtoValue::List(l) => CoreValue::List(l.iter().map(proto_to_core_value).collect()),
        ProtoValue::Map(m) => CoreValue::Map(
            m.iter()
                .map(|(k, v)| (k.clone(), proto_to_core_value(v)))
                .collect(),
        ),
    }
}

pub fn core_to_proto_value_map(m: &HashMap<String, CoreValue>) -> HashMap<String, ProtoValue> {
    m.iter()
        .map(|(k, v)| (k.clone(), core_to_proto_value(v)))
        .collect()
}

pub fn proto_to_core_value_map(m: &HashMap<String, ProtoValue>) -> HashMap<String, CoreValue> {
    m.iter()
        .map(|(k, v)| (k.clone(), proto_to_core_value(v)))
        .collect()
}

// -- State --

pub fn core_to_proto_state(s: &CoreState) -> ProtoState {
    ProtoState {
        id: core_to_proto_resource_id(&s.id),
        identifier: s.identifier.clone(),
        attributes: core_to_proto_value_map(&s.attributes),
        exists: s.exists,
    }
}

pub fn proto_to_core_state(s: &ProtoState) -> CoreState {
    let id = proto_to_core_resource_id(&s.id);
    if s.exists {
        let mut state = CoreState::existing(id, proto_to_core_value_map(&s.attributes));
        if let Some(ref ident) = s.identifier {
            state = state.with_identifier(ident);
        }
        state
    } else {
        CoreState::not_found(id)
    }
}

// -- Resource --

pub fn core_to_proto_resource(r: &CoreResource) -> ProtoResource {
    ProtoResource {
        id: core_to_proto_resource_id(&r.id),
        attributes: core_to_proto_value_map(&r.resolved_attributes()),
        lifecycle: core_to_proto_lifecycle(&r.lifecycle),
    }
}

// -- LifecycleConfig --

pub fn core_to_proto_lifecycle(l: &CoreLifecycle) -> ProtoLifecycle {
    ProtoLifecycle {
        force_delete: l.force_delete,
        create_before_destroy: l.create_before_destroy,
    }
}

// -- proto_to_core_resource (reverse of core_to_proto_resource) --

pub fn proto_to_core_resource(r: &ProtoResource) -> CoreResource {
    use carina_core::resource::Expr;
    let mut resource = CoreResource::with_provider(&r.id.provider, &r.id.resource_type, &r.id.name);
    resource.attributes = r
        .attributes
        .iter()
        .map(|(k, v)| (k.clone(), Expr(proto_to_core_value(v))))
        .collect();
    resource.lifecycle = CoreLifecycle {
        force_delete: r.lifecycle.force_delete,
        create_before_destroy: r.lifecycle.create_before_destroy,
    };
    resource
}

// -- AttributeType --

fn proto_to_core_attribute_type(t: &ProtoAttributeType) -> CoreAttributeType {
    match t {
        ProtoAttributeType::String => CoreAttributeType::String,
        ProtoAttributeType::Int => CoreAttributeType::Int,
        ProtoAttributeType::Float => CoreAttributeType::Float,
        ProtoAttributeType::Bool => CoreAttributeType::Bool,
        ProtoAttributeType::StringEnum { values } => CoreAttributeType::StringEnum {
            name: String::new(),
            values: values.clone(),
            namespace: None,
            to_dsl: None,
        },
        ProtoAttributeType::List { inner } => CoreAttributeType::List {
            inner: Box::new(proto_to_core_attribute_type(inner)),
            ordered: true,
        },
        ProtoAttributeType::Map { inner } => {
            CoreAttributeType::Map(Box::new(proto_to_core_attribute_type(inner)))
        }
        ProtoAttributeType::Struct { name, fields } => CoreAttributeType::Struct {
            name: name.clone(),
            fields: fields.iter().map(proto_to_core_struct_field).collect(),
        },
        ProtoAttributeType::Union { members } => {
            CoreAttributeType::Union(members.iter().map(proto_to_core_attribute_type).collect())
        }
    }
}

fn proto_to_core_struct_field(f: &ProtoStructField) -> CoreStructField {
    CoreStructField {
        name: f.name.clone(),
        field_type: proto_to_core_attribute_type(&f.field_type),
        required: f.required,
        description: f.description.clone(),
        provider_name: None,
        block_name: f.block_name.clone(),
    }
}

fn proto_to_core_attribute_schema(a: &ProtoAttributeSchema) -> CoreAttributeSchema {
    CoreAttributeSchema {
        name: a.name.clone(),
        attr_type: proto_to_core_attribute_type(&a.attr_type),
        required: a.required,
        default: a.default.as_ref().map(proto_to_core_value),
        description: a.description.clone(),
        completions: None,
        provider_name: None,
        create_only: a.create_only,
        read_only: a.read_only,
        removable: None,
        block_name: a.block_name.clone(),
        write_only: a.write_only,
    }
}

pub fn proto_to_core_schema(s: &ProtoResourceSchema) -> CoreResourceSchema {
    CoreResourceSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), proto_to_core_attribute_schema(v)))
            .collect(),
        description: s.description.clone(),
        validator: None,
        data_source: s.data_source,
        name_attribute: s.name_attribute.clone(),
        force_replace: s.force_replace,
    }
}

fn core_to_proto_attribute_type(t: &CoreAttributeType) -> ProtoAttributeType {
    match t {
        CoreAttributeType::String => ProtoAttributeType::String,
        CoreAttributeType::Int => ProtoAttributeType::Int,
        CoreAttributeType::Float => ProtoAttributeType::Float,
        CoreAttributeType::Bool => ProtoAttributeType::Bool,
        CoreAttributeType::StringEnum { values, .. } => ProtoAttributeType::StringEnum {
            values: values.clone(),
        },
        CoreAttributeType::List { inner, .. } => ProtoAttributeType::List {
            inner: Box::new(core_to_proto_attribute_type(inner)),
        },
        CoreAttributeType::Map(inner) => ProtoAttributeType::Map {
            inner: Box::new(core_to_proto_attribute_type(inner)),
        },
        CoreAttributeType::Struct { name, fields } => ProtoAttributeType::Struct {
            name: name.clone(),
            fields: fields.iter().map(core_to_proto_struct_field).collect(),
        },
        // Custom → base type: function pointers can't cross process boundary
        CoreAttributeType::Custom { base, .. } => core_to_proto_attribute_type(base),
        CoreAttributeType::Union(members) => ProtoAttributeType::Union {
            members: members.iter().map(core_to_proto_attribute_type).collect(),
        },
    }
}

fn core_to_proto_struct_field(f: &CoreStructField) -> ProtoStructField {
    ProtoStructField {
        name: f.name.clone(),
        field_type: core_to_proto_attribute_type(&f.field_type),
        required: f.required,
        description: f.description.clone(),
        block_name: f.block_name.clone(),
    }
}

fn core_to_proto_attribute_schema(a: &CoreAttributeSchema) -> ProtoAttributeSchema {
    ProtoAttributeSchema {
        name: a.name.clone(),
        attr_type: core_to_proto_attribute_type(&a.attr_type),
        required: a.required,
        default: a.default.as_ref().map(core_to_proto_value),
        description: a.description.clone(),
        create_only: a.create_only,
        read_only: a.read_only,
        write_only: a.write_only,
        block_name: a.block_name.clone(),
    }
}

pub fn core_to_proto_schema(s: &CoreResourceSchema) -> ProtoResourceSchema {
    ProtoResourceSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), core_to_proto_attribute_schema(v)))
            .collect(),
        description: s.description.clone(),
        data_source: s.data_source,
        name_attribute: s.name_attribute.clone(),
        force_replace: s.force_replace,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_schema_roundtrip() {
        let core_schema = CoreResourceSchema {
            resource_type: "ec2.vpc".into(),
            attributes: HashMap::from([(
                "cidr_block".into(),
                CoreAttributeSchema {
                    name: "cidr_block".into(),
                    attr_type: CoreAttributeType::String,
                    required: true,
                    default: None,
                    description: Some("CIDR block".into()),
                    completions: None,
                    provider_name: None,
                    create_only: true,
                    read_only: false,
                    removable: None,
                    block_name: None,
                    write_only: false,
                },
            )]),
            description: Some("VPC".into()),
            validator: None,
            data_source: false,
            name_attribute: None,
            force_replace: false,
        };

        let proto = core_to_proto_schema(&core_schema);
        let back = proto_to_core_schema(&proto);

        assert_eq!(back.resource_type, "ec2.vpc");
        assert_eq!(back.attributes.len(), 1);
        let attr = &back.attributes["cidr_block"];
        assert_eq!(attr.name, "cidr_block");
        assert!(attr.required);
        assert!(attr.create_only);
        assert_eq!(attr.description, Some("CIDR block".into()));
    }

    #[test]
    fn test_union_type_roundtrip() {
        // Union types (e.g., IAM principal: Struct | String) must survive
        // the core→proto→core roundtrip. Previously, Union was downgraded
        // to String, causing "Type mismatch: expected String, got Map" errors
        // when validating principal = { service = "ec2.amazonaws.com" }.
        let core_type = CoreAttributeType::Union(vec![
            CoreAttributeType::Struct {
                name: "IamPolicyPrincipal".into(),
                fields: vec![CoreStructField {
                    name: "service".into(),
                    field_type: CoreAttributeType::String,
                    required: false,
                    description: None,
                    provider_name: None,
                    block_name: None,
                }],
            },
            CoreAttributeType::String,
        ]);

        let proto = core_to_proto_attribute_type(&core_type);
        let back = proto_to_core_attribute_type(&proto);

        // The roundtripped type must accept both String and Map values
        let string_val = CoreValue::String("*".to_string());
        let map_val = CoreValue::Map(
            vec![(
                "service".to_string(),
                CoreValue::String("ec2.amazonaws.com".to_string()),
            )]
            .into_iter()
            .collect(),
        );

        assert!(
            back.validate(&string_val).is_ok(),
            "Union roundtrip should accept String value, got: {:?}",
            back.validate(&string_val)
        );
        assert!(
            back.validate(&map_val).is_ok(),
            "Union roundtrip should accept Map value (struct), got: {:?}",
            back.validate(&map_val)
        );
    }

    #[test]
    fn test_struct_type_roundtrip() {
        let core_type = CoreAttributeType::Struct {
            name: "Tag".into(),
            fields: vec![CoreStructField {
                name: "key".into(),
                field_type: CoreAttributeType::String,
                required: true,
                description: None,
                provider_name: None,
                block_name: None,
            }],
        };

        let proto = core_to_proto_attribute_type(&core_type);
        let back = proto_to_core_attribute_type(&proto);

        if let CoreAttributeType::Struct { name, fields } = back {
            assert_eq!(name, "Tag");
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "key");
        } else {
            panic!("Expected Struct type");
        }
    }

    #[test]
    fn test_block_name_roundtrip_struct_field() {
        // block_name on struct fields must survive core->proto->core roundtrip.
        // Without this, block syntax (e.g., `rule { ... }` for `rules` field)
        // fails with "Required attribute 'rules' is missing" because the CLI
        // cannot resolve block names.
        let core_schema = CoreResourceSchema {
            resource_type: "s3.bucket".into(),
            attributes: HashMap::from([(
                "lifecycle_configuration".into(),
                CoreAttributeSchema {
                    name: "lifecycle_configuration".into(),
                    attr_type: CoreAttributeType::Struct {
                        name: "LifecycleConfiguration".into(),
                        fields: vec![CoreStructField {
                            name: "rules".into(),
                            field_type: CoreAttributeType::list(CoreAttributeType::Struct {
                                name: "Rule".into(),
                                fields: vec![],
                            }),
                            required: true,
                            description: None,
                            provider_name: None,
                            block_name: Some("rule".into()),
                        }],
                    },
                    required: false,
                    default: None,
                    description: None,
                    completions: None,
                    provider_name: None,
                    create_only: false,
                    read_only: false,
                    removable: None,
                    block_name: Some("lifecycle_configuration".into()),
                    write_only: false,
                },
            )]),
            description: None,
            validator: None,
            data_source: false,
            name_attribute: None,
            force_replace: false,
        };

        let proto = core_to_proto_schema(&core_schema);
        let back = proto_to_core_schema(&proto);

        // Attribute-level block_name
        let attr = &back.attributes["lifecycle_configuration"];
        assert_eq!(
            attr.block_name,
            Some("lifecycle_configuration".into()),
            "attribute-level block_name must survive roundtrip"
        );

        // Struct field-level block_name
        if let CoreAttributeType::Struct { fields, .. } = &attr.attr_type {
            assert_eq!(
                fields[0].block_name,
                Some("rule".into()),
                "struct field block_name must survive roundtrip"
            );
        } else {
            panic!("Expected Struct type");
        }
    }
}
