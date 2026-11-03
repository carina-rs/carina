use std::sync::Arc;

use carina_core::provider::{
    BoxFuture, CanonicalProviderConfigAttributes, InitializeOutcomeMint, ProviderError,
    ProviderFactory, ProviderInitialization, ProviderReadyConfig, ProviderResult,
};
use carina_core::schema::{
    AssignabilityEdge, AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry,
    TypeIdentity, ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
    legacy_validator,
};

pub fn identity(kind: &str) -> TypeIdentity {
    TypeIdentity::from_schema_type("mock", "Bucket", kind)
}

pub fn string(kind: &str) -> AttributeType {
    AttributeType::refined_string(Some(identity(kind)), None, None, None)
}

pub struct EdgeFactory {
    pub owner: &'static str,
    pub edges: bool,
}

impl ProviderFactory for EdgeFactory {
    fn name(&self) -> &str {
        self.owner
    }
    fn display_name(&self) -> &str {
        self.owner
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(&self, _: &CanonicalProviderConfigAttributes) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: Option<&str>,
        _: &ProviderReadyConfig,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { Err(ProviderError::internal("schema-only identity edge fixture")) })
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let name = AttributeType::refined_string_with_validator(
            identity("Name"),
            None,
            None,
            legacy_validator(|value| {
                if let carina_core::resource::Value::Concrete(
                    carina_core::resource::ConcreteValue::String(text),
                ) = value
                    && text.expose_plaintext().starts_with("good-")
                {
                    Ok(())
                } else {
                    Err(format!(
                        "Name must start with good- (got {})",
                        value.type_name()
                    ))
                }
            }),
            None,
        );
        let schema = ResourceSchema::new("Bucket")
            .attribute(AttributeSchema::optional("bucket", name).build())
            .attribute(AttributeSchema::optional("new_bucket", string("NewName")).build())
            .attribute(AttributeSchema::provider_populated("new_name", string("NewName")).build())
            .attribute(AttributeSchema::provider_populated("old_name", string("Name")).build())
            .attribute(AttributeSchema::provider_populated("unrelated", string("Other")).build());
        let schema = ["A", "B", "C"].into_iter().fold(schema, |schema, kind| {
            schema.attribute(
                AttributeSchema::provider_populated(kind.to_lowercase(), string(kind)).build(),
            )
        });
        let schema = [("NewCount", "new_count"), ("Count", "count")]
            .into_iter()
            .fold(schema, |schema, (kind, attr)| {
                schema.attribute(
                    AttributeSchema::provider_populated(
                        attr,
                        AttributeType::refined_int(Some(identity(kind)), None),
                    )
                    .build(),
                )
            });
        let schema = [("NewRatio", "new_ratio"), ("Ratio", "ratio")]
            .into_iter()
            .fold(schema, |schema, (kind, attr)| {
                schema.attribute(
                    AttributeSchema::provider_populated(
                        attr,
                        AttributeType::refined_float(Some(identity(kind)), None),
                    )
                    .build(),
                )
            });
        vec![
            schema.validate().unwrap(),
            ResourceSchema::new("Lookup")
                .as_data_source()
                .attribute(AttributeSchema::required("bucket", string("Name")).build())
                .attribute(AttributeSchema::optional("new_bucket", string("NewName")).build())
                .validate()
                .unwrap(),
        ]
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            if self.edges {
                [
                    ("NewName", "Name"),
                    ("A", "B"),
                    ("B", "C"),
                    ("NewCount", "Count"),
                    ("NewRatio", "Ratio"),
                ]
                .into_iter()
                .map(|(source, sink)| {
                    AssignabilityEdge::new(identity(source), identity(sink)).unwrap()
                })
                .collect()
            } else {
                Vec::new()
            },
        )
        .unwrap()
    }
}

pub fn snapshot(owners: &[(&'static str, bool)]) -> carina_core::provider::LoadedProviderSnapshot {
    let factories = owners
        .iter()
        .map(|&(owner, edges)| Box::new(EdgeFactory { owner, edges }) as Box<dyn ProviderFactory>)
        .collect();
    carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    )
}
