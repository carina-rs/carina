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
use crate::schemas::generated::AwsccSchemaConfig;

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

    // =========================================================================
    // Special Case Handlers
    // =========================================================================

    /// Handle special attributes that don't follow standard mapping
    fn read_special_attributes(
        &self,
        resource_type: &str,
        props: &serde_json::Value,
        attributes: &mut HashMap<String, Value>,
    ) {
        match resource_type {
            "ec2.internet_gateway" => {
                // Get VPC attachment
                if let Some(attachments) = props.get("Attachments").and_then(|v| v.as_array())
                    && let Some(first) = attachments.first()
                    && let Some(vpc_id) = first.get("VpcId").and_then(|v| v.as_str())
                {
                    attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
                }
            }
            "ec2.vpc_endpoint" => {
                // Handle route_table_ids list
                if let Some(rt_ids) = props.get("RouteTableIds").and_then(|v| v.as_array()) {
                    let ids: Vec<Value> = rt_ids
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| Value::String(s.to_string())))
                        .collect();
                    if !ids.is_empty() {
                        attributes.insert("route_table_ids".to_string(), Value::List(ids));
                    }
                }
            }
            _ => {}
        }
    }

    /// Handle special attributes for create
    fn create_special_attributes(
        &self,
        _resource: &Resource,
        _desired_state: &mut serde_json::Map<String, serde_json::Value>,
    ) {
    }

    /// Set default values for create
    fn set_default_values(
        &self,
        resource_type: &str,
        desired_state: &mut serde_json::Map<String, serde_json::Value>,
    ) {
        if resource_type == "ec2.eip" && !desired_state.contains_key("Domain") {
            desired_state.insert("Domain".to_string(), json!("vpc"));
        }
    }

    /// Handle pre-delete operations (e.g., detach IGW from VPC)
    async fn pre_delete_operations(
        &self,
        id: &ResourceId,
        config: &AwsccSchemaConfig,
        identifier: &str,
    ) -> ProviderResult<()> {
        if id.resource_type == "ec2.internet_gateway" {
            // Detach from VPC first
            if let Some(props) = self
                .cc_get_resource(config.aws_type_name, identifier)
                .await?
                && let Some(attachments) = props.get("Attachments").and_then(|v| v.as_array())
                && !attachments.is_empty()
            {
                let patch_ops = vec![json!({"op": "remove", "path": "/Attachments"})];
                self.cc_update_resource(config.aws_type_name, identifier, patch_ops)
                    .await
                    .map_err(|e| {
                        ProviderError::new(
                            "Failed to detach Internet Gateway from VPC before deletion",
                        )
                        .with_cause(e)
                        .for_resource(id.clone())
                    })?;
            }
        }
        Ok(())
    }

    // =========================================================================
    // Tag Helpers
    // =========================================================================

    /// Build tags array for CloudFormation format
    pub(crate) fn build_tags(&self, user_tags: Option<&Value>) -> Vec<serde_json::Value> {
        let mut tags = Vec::new();
        if let Some(Value::Map(user_tags)) = user_tags {
            for (key, value) in user_tags {
                if let Value::String(v) = value {
                    tags.push(json!({"Key": key, "Value": v}));
                }
            }
        }
        tags
    }

    /// Parse tags from CloudFormation format to map
    pub(crate) fn parse_tags(&self, tags_array: &[serde_json::Value]) -> HashMap<String, Value> {
        let mut tags_map = HashMap::new();
        for tag in tags_array {
            if let (Some(key), Some(value)) = (
                tag.get("Key").and_then(|v| v.as_str()),
                tag.get("Value").and_then(|v| v.as_str()),
            ) {
                tags_map.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
        tags_map
    }

    /// Create an S3 client from the stored config
    fn s3_client(&self) -> aws_sdk_s3::Client {
        aws_sdk_s3::Client::new(&self.aws_config)
    }

    /// Empty an S3 bucket by deleting all objects and versions
    async fn empty_s3_bucket(&self, bucket_name: &str) -> ProviderResult<()> {
        let s3 = self.s3_client();

        // Delete all object versions (handles versioned and non-versioned buckets)
        let mut key_marker: Option<String> = None;
        let mut version_id_marker: Option<String> = None;

        loop {
            let mut req = s3.list_object_versions().bucket(bucket_name).max_keys(1000);
            if let Some(ref km) = key_marker {
                req = req.key_marker(km);
            }
            if let Some(ref vim) = version_id_marker {
                req = req.version_id_marker(vim);
            }

            let response = req
                .send()
                .await
                .map_err(|e| ProviderError::new("Failed to list object versions").with_cause(e))?;

            let mut objects_to_delete = Vec::new();

            // Collect versions
            for version in response.versions() {
                if let Some(key) = version.key() {
                    let mut id = aws_sdk_s3::types::ObjectIdentifier::builder().key(key);
                    if let Some(vid) = version.version_id() {
                        id = id.version_id(vid);
                    }
                    objects_to_delete.push(id.build().map_err(|e| {
                        ProviderError::new("Failed to build ObjectIdentifier").with_cause(e)
                    })?);
                }
            }

            // Collect delete markers
            for marker in response.delete_markers() {
                if let Some(key) = marker.key() {
                    let mut id = aws_sdk_s3::types::ObjectIdentifier::builder().key(key);
                    if let Some(vid) = marker.version_id() {
                        id = id.version_id(vid);
                    }
                    objects_to_delete.push(id.build().map_err(|e| {
                        ProviderError::new("Failed to build ObjectIdentifier").with_cause(e)
                    })?);
                }
            }

            // Batch delete (max 1000 per request)
            if !objects_to_delete.is_empty() {
                let delete = aws_sdk_s3::types::Delete::builder()
                    .set_objects(Some(objects_to_delete))
                    .quiet(true)
                    .build()
                    .map_err(|e| {
                        ProviderError::new("Failed to build Delete request").with_cause(e)
                    })?;

                s3.delete_objects()
                    .bucket(bucket_name)
                    .delete(delete)
                    .send()
                    .await
                    .map_err(|e| ProviderError::new("Failed to delete objects").with_cause(e))?;
            }

            if response.is_truncated() == Some(true) {
                key_marker = response.next_key_marker().map(|s| s.to_string());
                version_id_marker = response.next_version_id_marker().map(|s| s.to_string());
            } else {
                break;
            }
        }

        Ok(())
    }
}
