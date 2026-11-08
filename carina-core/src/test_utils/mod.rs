//! Fixtures available to downstream tests with the `test-utils` feature.

#[cfg(not(target_arch = "wasm32"))]
pub mod loopback;

// Host boundary fixtures must not become guest display authority through
// Cargo feature unification in a provider's wasm build.
#[cfg(not(target_arch = "wasm32"))]
pub mod provider_text;

/// Run classified resource fixtures through production validation admission.
pub fn admit_resource_attribute_report<
    P: crate::resource::PlaceholderKind,
    B: crate::resource::BindingAddress,
>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    attributes: &std::collections::HashMap<
        String,
        crate::resource::knowledge::Classification<'_, P, B>,
    >,
    is_string_literal: &dyn Fn(&str) -> bool,
    custom_types: &crate::parser::ProviderCustomTypes,
) -> crate::schema::ResourceValueValidation {
    let catalog = crate::parser::ProviderContext {
        custom_types: custom_types.clone(),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    };
    let id = crate::resource::ResourceId::pending_with_provider(
        schema.owner(),
        schema.schema().parts().resource_type,
        None,
    );
    let quoted = attributes
        .keys()
        .filter(|name| is_string_literal(name))
        .cloned()
        .collect();
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        crate::validation::admission::attributes::admit_validation_attributes::<
            crate::validation::admission::kind::ResourceAttribute,
            P,
            B,
        >(schema, &id, attributes, &quoted, None, session, None, None)
    })
}

/// Validate a resource's authored attributes through production admission.
///
/// Available with the `test-utils` feature for provider test dev-dependencies.
/// The schema walk checks required attributes and field groups; each authored
/// value enters `admit()`, including local and provider custom-type validation
/// from `custom_types`. Use the schema and custom types from the same catalog.
///
/// A VPC gateway attachment must specify exactly one gateway. Supplying both
/// is rejected even though each attribute is individually a valid String:
///
/// ```
/// use std::collections::HashMap;
/// use carina_core::{
///     parser::ProviderCustomTypes,
///     resource::{ConcreteValue, Value, knowledge::KnowledgeContext},
///     schema::{AttributeSchema, AttributeType, FieldGroup, ResourceSchema, SchemaKind, SchemaRegistry},
///     test_utils::admit_resource_attributes,
/// };
///
/// let schema = ResourceSchema::new("ec2.vpc_gateway_attachment")
///     .attribute(AttributeSchema::required("vpc_id", AttributeType::string()).build())
///     .attribute(AttributeSchema::optional("internet_gateway_id", AttributeType::string()).build())
///     .attribute(AttributeSchema::optional("vpn_gateway_id", AttributeType::string()).build())
///     .field_group(FieldGroup::exactly_one(["internet_gateway_id", "vpn_gateway_id"]).unwrap())
///     .validate().unwrap();
/// let mut schemas = SchemaRegistry::new();
/// schemas.insert("awscc", schema);
/// let schema = schemas.get("awscc", "ec2.vpc_gateway_attachment", SchemaKind::Resource).unwrap();
/// let values: HashMap<String, Value> = HashMap::from([
///     ("vpc_id".into(), Value::Concrete(ConcreteValue::String("vpc-12345678".into()))),
///     ("internet_gateway_id".into(), Value::Concrete(ConcreteValue::String("igw-12345678".into()))),
///     ("vpn_gateway_id".into(), Value::Concrete(ConcreteValue::String("vgw-12345678".into()))),
/// ]);
/// let attributes = values.iter().map(|(name, value)| {
///     (name.clone(), value.knowledge(&KnowledgeContext::validation()).unwrap().into_classification())
/// }).collect();
/// let errors = admit_resource_attributes(
///     schema, &attributes, &|_| true, &ProviderCustomTypes::default(),
/// ).unwrap_err();
/// assert_eq!(errors.len(), 1);
/// assert!(errors[0].to_string().contains(
///     "exactly one of [internet_gateway_id, vpn_gateway_id] must be set"
/// ));
/// ```
pub fn admit_resource_attributes<
    P: crate::resource::PlaceholderKind,
    B: crate::resource::BindingAddress,
>(
    schema: crate::schema::OwnerBoundResourceSchema<'_>,
    attributes: &std::collections::HashMap<
        String,
        crate::resource::knowledge::Classification<'_, P, B>,
    >,
    is_string_literal: &dyn Fn(&str) -> bool,
    custom_types: &crate::parser::ProviderCustomTypes,
) -> Result<(), Vec<crate::schema::TypeError>> {
    let report =
        admit_resource_attribute_report(schema, attributes, is_string_literal, custom_types);
    let errors: Vec<_> = report.errors().cloned().collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// A registered, empty schema for boundary fixtures that exercise wire checks
/// rather than provider declarations. Its attributes remain unschematized.
pub fn preparation_schemas(
    id: &crate::resource::ResourceId,
    kind: crate::schema::SchemaKind,
) -> crate::schema::SchemaRegistry {
    let mut registry = crate::schema::SchemaRegistry::new();
    let schema = crate::schema::ResourceSchema::new(&id.resource_type);
    let schema = match kind {
        crate::schema::SchemaKind::Resource => schema,
        crate::schema::SchemaKind::DataSource => schema.as_data_source(),
    };
    registry.insert(&id.provider, schema.validate().unwrap());
    registry
}
