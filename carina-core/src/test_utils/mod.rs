//! Fixtures available to downstream tests with the `test-utils` feature.

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

/// Collect fixture diagnostics from production validation admission.
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
