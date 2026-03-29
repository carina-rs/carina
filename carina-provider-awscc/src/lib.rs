//! Carina AWS Cloud Control Provider
//!
//! AWS Cloud Control API Provider implementation.
//!
//! ## Module Structure
//!
//! - `resources` - Schema configuration tests
//! - `provider` - AwsccProvider implementation
//! - `schemas` - Auto-generated resource schemas

pub mod provider;
pub mod resources;
pub mod schemas;

// Re-export main types
pub use provider::AwsccProvider;

use std::collections::HashMap;

use carina_core::provider::{
    BoxFuture, Provider, ProviderFactory, ProviderNormalizer, ProviderResult, SavedAttrs,
};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;

/// Schema extension for the AWSCC provider.
///
/// Handles plan-time normalization of enum identifiers and hydration of
/// unreturned attributes from saved state.
pub struct AwsccNormalizer;

impl ProviderNormalizer for AwsccNormalizer {
    fn normalize_desired(&self, resources: &mut [Resource]) {
        crate::provider::resolve_enum_identifiers_impl(resources);
    }

    fn normalize_state(&self, current_states: &mut HashMap<ResourceId, State>) {
        crate::provider::normalize_state_enums_impl(current_states);
    }

    fn hydrate_read_state(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &SavedAttrs,
    ) {
        crate::provider::restore_unreturned_attrs_impl(current_states, saved_attrs);
    }

    fn merge_default_tags(
        &self,
        resources: &mut [Resource],
        default_tags: &HashMap<String, Value>,
        schemas: &HashMap<String, ResourceSchema>,
    ) {
        if default_tags.is_empty() {
            return;
        }

        for resource in resources.iter_mut() {
            if resource.id.provider != "awscc" {
                continue;
            }

            // Check if the resource schema has a `tags` attribute
            let schema_key = format!("awscc.{}", resource.id.resource_type);
            let has_tags = schemas
                .get(&schema_key)
                .is_some_and(|s| s.attributes.contains_key("tags"));

            if !has_tags {
                continue;
            }

            // Merge default_tags into the resource's tags
            let mut default_tag_keys: Vec<String> = Vec::new();
            match resource.get_attr_mut("tags") {
                Some(Value::Map(existing_tags)) => {
                    // Resource-level tags take precedence: only insert defaults for missing keys
                    for (key, value) in default_tags {
                        if !existing_tags.contains_key(key) {
                            existing_tags.insert(key.clone(), value.clone());
                            default_tag_keys.push(key.clone());
                        }
                    }
                }
                None => {
                    // No tags on the resource: use default_tags as-is
                    default_tag_keys = default_tags.keys().cloned().collect();
                    resource.set_attr("tags".to_string(), Value::Map(default_tags.clone()));
                }
                _ => {
                    // tags is some other value type (unexpected), skip
                    continue;
                }
            }

            // Store which tag keys came from default_tags as internal metadata
            if !default_tag_keys.is_empty() {
                default_tag_keys.sort();
                resource.set_attr(
                    "_default_tag_keys".to_string(),
                    Value::List(default_tag_keys.into_iter().map(Value::String).collect()),
                );
            }
        }
    }
}

/// Factory for creating and configuring the AWSCC Provider
pub struct AwsccProviderFactory;

impl ProviderFactory for AwsccProviderFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWS Cloud Control provider"
    }

    fn validate_config(&self, attributes: &HashMap<String, Value>) -> Result<(), String> {
        let region_type = schemas::awscc_types::awscc_region();
        if let Some(region_value) = attributes.get("region") {
            region_type
                .validate(region_value)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn extract_region(&self, attributes: &HashMap<String, Value>) -> String {
        if let Some(Value::String(region)) = attributes.get("region") {
            if let Some(rest) = region.strip_prefix("awscc.Region.") {
                return rest.replace('_', "-");
            }
            if let Some(rest) = region.strip_prefix("aws.Region.") {
                return rest.replace('_', "-");
            }
            return region.clone();
        }
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn Provider>> {
        let region = self.extract_region(attributes);
        Box::pin(async move { Box::new(AwsccProvider::new(&region).await) as Box<dyn Provider> })
    }

    fn create_normalizer(
        &self,
        _attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Option<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Some(Box::new(AwsccNormalizer) as Box<dyn ProviderNormalizer>) })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ResourceSchema> {
        schemas::all_schemas()
    }

    fn identity_attributes(&self) -> Vec<&str> {
        vec!["region"]
    }

    fn region_completions(&self) -> Vec<carina_core::schema::CompletionValue> {
        carina_aws_types::region_completions("awscc")
    }

    fn get_enum_alias_reverse(
        &self,
        resource_type: &str,
        attr_name: &str,
        value: &str,
    ) -> Option<&'static str> {
        schemas::generated::get_enum_alias_reverse(resource_type, attr_name, value)
    }
}

// =============================================================================
// Provider Trait Implementation
// =============================================================================

impl Provider for AwsccProvider {
    fn name(&self) -> &'static str {
        "awscc"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.map(|s| s.to_string());
        Box::pin(async move {
            self.read_resource(&id.resource_type, &id.name, identifier.as_deref())
                .await
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        Box::pin(async move { self.create_resource(resource).await })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let from = from.clone();
        let to = to.clone();
        Box::pin(async move { self.update_resource(id, &identifier, &from, to).await })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let lifecycle = lifecycle.clone();
        Box::pin(async move { self.delete_resource(&id, &identifier, &lifecycle).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_schemas() -> HashMap<String, ResourceSchema> {
        let factory = AwsccProviderFactory;
        let mut schemas = HashMap::new();
        for schema in factory.schemas() {
            schemas.insert(schema.resource_type.clone(), schema);
        }
        schemas
    }

    #[test]
    fn test_merge_default_tags_resource_tags_win() {
        let schemas = build_schemas();
        let normalizer = AwsccNormalizer;

        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "test-vpc");
        resource.set_attr(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let mut resource_tags = HashMap::new();
        resource_tags.insert("Name".to_string(), Value::String("my-vpc".to_string()));
        resource_tags.insert(
            "Environment".to_string(),
            Value::String("staging".to_string()),
        );
        resource.set_attr("tags".to_string(), Value::Map(resource_tags));

        let mut default_tags = HashMap::new();
        default_tags.insert(
            "Environment".to_string(),
            Value::String("production".to_string()),
        );
        default_tags.insert("Team".to_string(), Value::String("platform".to_string()));

        let mut resources = vec![resource];
        normalizer.merge_default_tags(&mut resources, &default_tags, &schemas);

        if let Some(Value::Map(tags)) = resources[0].get_attr("tags") {
            assert_eq!(
                tags.get("Environment"),
                Some(&Value::String("staging".to_string()))
            );
            assert_eq!(tags.get("Name"), Some(&Value::String("my-vpc".to_string())));
            assert_eq!(
                tags.get("Team"),
                Some(&Value::String("platform".to_string()))
            );
        } else {
            panic!("Expected tags to be a Map");
        }

        if let Some(Value::List(keys)) = resources[0].get_attr("_default_tag_keys") {
            let key_strs: Vec<&str> = keys
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(key_strs, vec!["Team"]);
        } else {
            panic!("Expected _default_tag_keys to be set");
        }
    }

    #[test]
    fn test_merge_default_tags_no_explicit_tags() {
        let schemas = build_schemas();
        let normalizer = AwsccNormalizer;

        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "test-vpc");
        resource.set_attr(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let mut default_tags = HashMap::new();
        default_tags.insert(
            "Environment".to_string(),
            Value::String("production".to_string()),
        );

        let mut resources = vec![resource];
        normalizer.merge_default_tags(&mut resources, &default_tags, &schemas);

        if let Some(Value::Map(tags)) = resources[0].get_attr("tags") {
            assert_eq!(
                tags.get("Environment"),
                Some(&Value::String("production".to_string()))
            );
        } else {
            panic!("Expected tags to be set from default_tags");
        }

        if let Some(Value::List(keys)) = resources[0].get_attr("_default_tag_keys") {
            let key_strs: Vec<&str> = keys
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(key_strs, vec!["Environment"]);
        } else {
            panic!("Expected _default_tag_keys to be set");
        }
    }

    #[test]
    fn test_merge_default_tags_skips_no_tag_schema() {
        let schemas = build_schemas();
        let normalizer = AwsccNormalizer;

        let mut resource = Resource::with_provider("awscc", "ec2.route", "test-route");
        resource.set_attr(
            "route_table_id".to_string(),
            Value::String("rtb-123".to_string()),
        );

        let mut default_tags = HashMap::new();
        default_tags.insert(
            "Environment".to_string(),
            Value::String("production".to_string()),
        );

        let mut resources = vec![resource];
        normalizer.merge_default_tags(&mut resources, &default_tags, &schemas);

        assert!(!resources[0].attributes.contains_key("tags"));
        assert!(!resources[0].attributes.contains_key("_default_tag_keys"));
    }

    #[test]
    fn test_merge_default_tags_no_default_tags() {
        let schemas = build_schemas();
        let normalizer = AwsccNormalizer;

        let mut resource = Resource::with_provider("awscc", "ec2.vpc", "test-vpc");
        resource.set_attr(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let mut resource_tags = HashMap::new();
        resource_tags.insert("Name".to_string(), Value::String("my-vpc".to_string()));
        resource.set_attr("tags".to_string(), Value::Map(resource_tags));

        let default_tags = HashMap::new();

        let mut resources = vec![resource];
        normalizer.merge_default_tags(&mut resources, &default_tags, &schemas);

        if let Some(Value::Map(tags)) = resources[0].get_attr("tags") {
            assert_eq!(tags.len(), 1);
            assert_eq!(tags.get("Name"), Some(&Value::String("my-vpc".to_string())));
        } else {
            panic!("Expected tags to be unchanged");
        }
        assert!(!resources[0].attributes.contains_key("_default_tag_keys"));
    }
}
