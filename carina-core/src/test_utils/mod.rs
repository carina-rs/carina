//! Fixtures available to downstream tests with the `test-utils` feature.

// Host boundary fixtures must not become guest display authority through
// Cargo feature unification in a provider's wasm build.
#[cfg(not(target_arch = "wasm32"))]
pub mod provider_text;

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
