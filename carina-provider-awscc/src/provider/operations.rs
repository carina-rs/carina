//! High-level resource operations (read, create, update, delete).
//!
//! This module implements the main resource lifecycle operations that bridge
//! between DSL resources and the Cloud Control API. It handles attribute mapping,
//! tags, special cases, and default values.

use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use serde_json::json;

use super::conversion::{aws_value_to_dsl, dsl_value_to_aws};
use super::update::build_update_patches;
use super::{AwsccProvider, get_schema_config};

impl AwsccProvider {
    /// Read a resource using its configuration
    pub async fn read_resource(
        &self,
        resource_type: &str,
        name: &str,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let id = ResourceId::with_provider("awscc", resource_type, name);

        let config = get_schema_config(resource_type).ok_or_else(|| {
            ProviderError::new(format!("Unknown resource type: {}", resource_type))
                .for_resource(id.clone())
        })?;

        let identifier = match identifier {
            Some(id) => id,
            None => return Ok(State::not_found(id)),
        };

        let props = match self
            .cc_get_resource(config.aws_type_name, identifier)
            .await?
        {
            Some(props) => props,
            None => return Ok(State::not_found(id)),
        };

        let mut attributes = HashMap::new();

        // Map AWS attributes to DSL attributes using provider_name
        for (dsl_name, attr_schema) in &config.schema.attributes {
            // Skip tags - handled separately below
            if dsl_name == "tags" {
                continue;
            }
            if let Some(aws_name) = &attr_schema.provider_name
                && let Some(value) = props.get(aws_name.as_str())
            {
                let dsl_value =
                    aws_value_to_dsl(dsl_name, value, &attr_schema.attr_type, resource_type);
                if let Some(v) = dsl_value {
                    attributes.insert(dsl_name.to_string(), v);
                }
            }
        }

        // Handle tags
        if config.has_tags
            && let Some(tags_array) = props.get("Tags").and_then(|v| v.as_array())
        {
            let tags_map = self.parse_tags(tags_array);
            if !tags_map.is_empty() {
                attributes.insert("tags".to_string(), Value::Map(tags_map));
            }
        }

        // Handle special cases
        self.read_special_attributes(resource_type, &props, &mut attributes);

        Ok(State::existing(id, attributes).with_identifier(identifier))
    }

    /// Create a resource using its configuration
    pub async fn create_resource(&self, resource: Resource) -> ProviderResult<State> {
        let config = get_schema_config(&resource.id.resource_type).ok_or_else(|| {
            ProviderError::new(format!(
                "Unknown resource type: {}",
                resource.id.resource_type
            ))
            .for_resource(resource.id.clone())
        })?;

        let mut desired_state = serde_json::Map::new();

        // Map DSL attributes to AWS attributes using provider_name
        for (dsl_name, attr_schema) in &config.schema.attributes {
            // Skip tags - handled separately below
            if dsl_name == "tags" {
                continue;
            }
            if let Some(aws_name) = &attr_schema.provider_name
                && let Some(value) = resource.attributes.get(dsl_name.as_str())
            {
                let aws_value = dsl_value_to_aws(
                    value,
                    &attr_schema.attr_type,
                    &resource.id.resource_type,
                    dsl_name,
                );
                if let Some(v) = aws_value {
                    desired_state.insert(aws_name.to_string(), v);
                }
            }
        }

        // Handle special cases for create
        self.create_special_attributes(&resource, &mut desired_state);

        // Handle tags
        if config.has_tags {
            let tags = self.build_tags(resource.attributes.get("tags"));
            if !tags.is_empty() {
                desired_state.insert("Tags".to_string(), json!(tags));
            }
        }

        // Set default values
        self.set_default_values(&resource.id.resource_type, &mut desired_state);

        let identifier = self
            .cc_create_resource(
                config.aws_type_name,
                serde_json::Value::Object(desired_state),
            )
            .await
            .map_err(|e| e.for_resource(resource.id.clone()))?;

        let mut state = self
            .read_resource(
                &resource.id.resource_type,
                &resource.id.name,
                Some(&identifier),
            )
            .await?;

        // Preserve desired attributes not returned by CloudControl API.
        // CloudControl doesn't always return all properties in GetResource responses
        // (create-only properties, and some normal properties like `description`).
        // Carry them forward from the desired state.
        for dsl_name in config.schema.attributes.keys() {
            if !state.attributes.contains_key(dsl_name)
                && let Some(value) = resource.attributes.get(dsl_name.as_str())
            {
                state.attributes.insert(dsl_name.to_string(), value.clone());
            }
        }

        Ok(state)
    }

    /// Update a resource
    pub async fn update_resource(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        let config = get_schema_config(&id.resource_type).ok_or_else(|| {
            ProviderError::new(format!("Unknown resource type: {}", id.resource_type))
                .for_resource(id.clone())
        })?;

        // Reject updates for resource types marked as force_replace in the schema
        if config.schema.force_replace {
            return Err(ProviderError::new(format!(
                "Update not supported for {}, delete and recreate",
                id.resource_type
            ))
            .for_resource(id));
        }

        let patch_ops = build_update_patches(&config, from, &to);

        self.cc_update_resource(config.aws_type_name, identifier, patch_ops)
            .await
            .map_err(|e| e.for_resource(id.clone()))?;

        let mut state = self
            .read_resource(&id.resource_type, &id.name, Some(identifier))
            .await?;

        // Preserve desired attributes not returned by CloudControl API.
        // Same logic as create_resource: carry forward attributes that were accepted
        // by the API but aren't included in the read response.
        for dsl_name in config.schema.attributes.keys() {
            if !state.attributes.contains_key(dsl_name)
                && let Some(value) = to.attributes.get(dsl_name.as_str())
            {
                state.attributes.insert(dsl_name.to_string(), value.clone());
            }
        }

        Ok(state)
    }

    /// Delete a resource
    pub async fn delete_resource(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> ProviderResult<()> {
        let config = get_schema_config(&id.resource_type).ok_or_else(|| {
            ProviderError::new(format!("Unknown resource type: {}", id.resource_type))
                .for_resource(id.clone())
        })?;

        // Handle special pre-delete operations
        self.pre_delete_operations(id, &config, identifier).await?;

        // Handle force_delete for S3 buckets: empty the bucket before deletion
        if lifecycle.force_delete && id.resource_type == "s3.bucket" {
            self.empty_s3_bucket(identifier).await.map_err(|e| {
                ProviderError::new("Failed to empty S3 bucket before deletion")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        }

        self.cc_delete_resource(config.aws_type_name, identifier)
            .await
            .map_err(|e| e.for_resource(id.clone()))
    }
}
