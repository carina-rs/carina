//! AWS Cloud Control Provider implementation
//!
//! This module contains the main provider implementation that communicates
//! with AWS Cloud Control API to manage resources.

use std::collections::HashMap;
use std::time::Duration;

use aws_config::Region;
use aws_sdk_cloudcontrol::Client as CloudControlClient;
use aws_sdk_cloudcontrol::types::OperationStatus;
use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use heck::{ToPascalCase, ToSnakeCase};
use serde_json::json;

use carina_core::schema::AttributeType;

use crate::schemas::generated::AwsccSchemaConfig;
use crate::utils::{convert_enum_value, normalize_availability_zone, normalize_instance_tenancy};

/// Get the AwsccSchemaConfig for a resource type
fn get_schema_config(resource_type: &str) -> Option<AwsccSchemaConfig> {
    crate::schemas::generated::configs().into_iter().find(|c| {
        // Match by schema resource_type: "awscc.ec2_vpc" -> "ec2_vpc"
        c.schema
            .resource_type
            .strip_prefix("awscc.")
            .map(|t| t == resource_type)
            .unwrap_or(false)
    })
}

/// AWS Cloud Control Provider
pub struct AwsccProvider {
    cloudcontrol_client: CloudControlClient,
    region: String,
}

impl AwsccProvider {
    /// Create a new AwsccProvider for the specified region
    pub async fn new(region: &str) -> Self {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(Region::new(region.to_string()))
            .load()
            .await;

        Self {
            cloudcontrol_client: CloudControlClient::new(&config),
            region: region.to_string(),
        }
    }

    // =========================================================================
    // Cloud Control API Methods
    // =========================================================================

    /// Get a resource by identifier using Cloud Control API
    pub async fn cc_get_resource(
        &self,
        type_name: &str,
        identifier: &str,
    ) -> ProviderResult<Option<serde_json::Value>> {
        let result = self
            .cloudcontrol_client
            .get_resource()
            .type_name(type_name)
            .identifier(identifier)
            .send()
            .await;

        match result {
            Ok(response) => {
                if let Some(desc) = response.resource_description()
                    && let Some(props_str) = desc.properties()
                {
                    let props: serde_json::Value =
                        serde_json::from_str(props_str).unwrap_or_default();
                    Ok(Some(props))
                } else {
                    Ok(None)
                }
            }
            Err(e) => {
                let err_str = format!("{:?}", e);
                if err_str.contains("ResourceNotFound") || err_str.contains("NotFound") {
                    Ok(None)
                } else {
                    Err(ProviderError::new(format!(
                        "Failed to get resource: {:?}",
                        e
                    )))
                }
            }
        }
    }

    /// Create a resource using Cloud Control API
    pub async fn cc_create_resource(
        &self,
        type_name: &str,
        desired_state: serde_json::Value,
    ) -> ProviderResult<String> {
        let result = self
            .cloudcontrol_client
            .create_resource()
            .type_name(type_name)
            .desired_state(desired_state.to_string())
            .send()
            .await
            .map_err(|e| ProviderError::new(format!("Failed to create resource: {:?}", e)))?;

        let request_token = result
            .progress_event()
            .and_then(|p| p.request_token())
            .ok_or_else(|| ProviderError::new("No request token returned"))?;

        self.wait_for_operation(request_token).await
    }

    /// Update a resource using Cloud Control API
    pub async fn cc_update_resource(
        &self,
        type_name: &str,
        identifier: &str,
        patch_ops: Vec<serde_json::Value>,
    ) -> ProviderResult<()> {
        if patch_ops.is_empty() {
            return Ok(());
        }

        let patch_document = serde_json::to_string(&patch_ops)
            .map_err(|e| ProviderError::new(format!("Failed to build patch: {}", e)))?;

        let result = self
            .cloudcontrol_client
            .update_resource()
            .type_name(type_name)
            .identifier(identifier)
            .patch_document(patch_document)
            .send()
            .await
            .map_err(|e| ProviderError::new(format!("Failed to update resource: {:?}", e)))?;

        if let Some(request_token) = result.progress_event().and_then(|p| p.request_token()) {
            self.wait_for_operation(request_token).await?;
        }

        Ok(())
    }

    /// Delete a resource using Cloud Control API
    pub async fn cc_delete_resource(
        &self,
        type_name: &str,
        identifier: &str,
    ) -> ProviderResult<()> {
        let result = self
            .cloudcontrol_client
            .delete_resource()
            .type_name(type_name)
            .identifier(identifier)
            .send()
            .await
            .map_err(|e| ProviderError::new(format!("Failed to delete resource: {:?}", e)))?;

        if let Some(request_token) = result.progress_event().and_then(|p| p.request_token()) {
            self.wait_for_operation(request_token).await?;
        }

        Ok(())
    }

    /// Wait for a Cloud Control operation to complete
    async fn wait_for_operation(&self, request_token: &str) -> ProviderResult<String> {
        let max_attempts = 120;
        let delay = Duration::from_secs(5);

        for _ in 0..max_attempts {
            let status = self
                .cloudcontrol_client
                .get_resource_request_status()
                .request_token(request_token)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to get operation status: {:?}", e))
                })?;

            if let Some(progress) = status.progress_event() {
                match progress.operation_status() {
                    Some(OperationStatus::Success) => {
                        return Ok(progress.identifier().unwrap_or("").to_string());
                    }
                    Some(OperationStatus::Failed) => {
                        let msg = progress.status_message().unwrap_or("Unknown error");
                        return Err(ProviderError::new(format!("Operation failed: {}", msg)));
                    }
                    Some(OperationStatus::CancelComplete) => {
                        return Err(ProviderError::new("Operation was cancelled"));
                    }
                    _ => {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(ProviderError::new("Operation timed out").timeout())
    }

    // =========================================================================
    // Resource Operations
    // =========================================================================

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

        // Add region for VPC
        if resource_type == "ec2_vpc" {
            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));
        }

        // Map AWS attributes to DSL attributes using provider_name
        for (dsl_name, attr_schema) in &config.schema.attributes {
            // Skip tags - handled separately below
            if dsl_name == "tags" {
                continue;
            }
            if let Some(aws_name) = &attr_schema.provider_name
                && let Some(value) = props.get(aws_name.as_str())
            {
                let dsl_value = self.aws_value_to_dsl(dsl_name, value);
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
                let aws_value = self.dsl_value_to_aws(dsl_name, value, &attr_schema.attr_type);
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

        self.read_resource(
            &resource.id.resource_type,
            &resource.id.name,
            Some(&identifier),
        )
        .await
    }

    /// Update a resource
    pub async fn update_resource(
        &self,
        id: ResourceId,
        identifier: &str,
        to: Resource,
    ) -> ProviderResult<State> {
        let config = get_schema_config(&id.resource_type).ok_or_else(|| {
            ProviderError::new(format!("Unknown resource type: {}", id.resource_type))
                .for_resource(id.clone())
        })?;

        // Only VPC supports in-place updates currently
        if id.resource_type != "ec2_vpc" {
            return Err(ProviderError::new(format!(
                "Update not supported for {}, delete and recreate",
                id.resource_type
            ))
            .for_resource(id));
        }

        let mut patch_ops = Vec::new();

        // Build patch operations for changed attributes using provider_name
        for (dsl_name, attr_schema) in &config.schema.attributes {
            // Skip tags - handled separately below
            if dsl_name == "tags" {
                continue;
            }
            if let Some(aws_name) = &attr_schema.provider_name
                && let Some(value) = to.attributes.get(dsl_name.as_str())
                && let Some(aws_value) =
                    self.dsl_value_to_aws(dsl_name, value, &attr_schema.attr_type)
            {
                patch_ops.push(json!({
                    "op": "replace",
                    "path": format!("/{}", aws_name),
                    "value": aws_value
                }));
            }
        }

        // Handle tags update
        if config.has_tags
            && let Some(Value::Map(user_tags)) = to.attributes.get("tags")
        {
            let mut tags = Vec::new();
            for (key, value) in user_tags {
                if let Value::String(v) = value {
                    tags.push(json!({"Key": key, "Value": v}));
                }
            }
            if !tags.is_empty() {
                patch_ops.push(json!({"op": "replace", "path": "/Tags", "value": tags}));
            }
        }

        self.cc_update_resource(config.aws_type_name, identifier, patch_ops)
            .await
            .map_err(|e| e.for_resource(id.clone()))?;

        self.read_resource(&id.resource_type, &id.name, Some(identifier))
            .await
    }

    /// Delete a resource
    pub async fn delete_resource(&self, id: &ResourceId, identifier: &str) -> ProviderResult<()> {
        let config = get_schema_config(&id.resource_type).ok_or_else(|| {
            ProviderError::new(format!("Unknown resource type: {}", id.resource_type))
                .for_resource(id.clone())
        })?;

        // Handle special pre-delete operations
        self.pre_delete_operations(id, &config, identifier).await?;

        self.cc_delete_resource(config.aws_type_name, identifier)
            .await
            .map_err(|e| e.for_resource(id.clone()))
    }

    // =========================================================================
    // Value Conversion Helpers
    // =========================================================================

    /// Convert AWS value to DSL value
    fn aws_value_to_dsl(&self, dsl_name: &str, value: &serde_json::Value) -> Option<Value> {
        match dsl_name {
            "availability_zone" => {
                // Convert ap-northeast-1a to aws.AvailabilityZone.ap_northeast_1a
                value.as_str().map(|s| {
                    let az_dsl = format!("aws.AvailabilityZone.{}", s.replace('-', "_"));
                    Value::String(az_dsl)
                })
            }
            _ => self.json_to_value(value),
        }
    }

    /// Convert JSON value to DSL Value
    fn json_to_value(&self, value: &serde_json::Value) -> Option<Value> {
        match value {
            serde_json::Value::String(s) => Some(Value::String(s.clone())),
            serde_json::Value::Bool(b) => Some(Value::Bool(*b)),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Some(Value::Int(i))
                } else {
                    n.as_f64().map(|f| Value::Int(f as i64))
                }
            }
            serde_json::Value::Array(arr) => {
                let items: Vec<Value> = arr.iter().filter_map(|v| self.json_to_value(v)).collect();
                Some(Value::List(items))
            }
            serde_json::Value::Object(obj) => {
                let map: HashMap<String, Value> = obj
                    .iter()
                    .filter_map(|(k, v)| self.json_to_value(v).map(|val| (k.to_snake_case(), val)))
                    .collect();
                Some(Value::Map(map))
            }
            _ => None,
        }
    }

    /// Convert DSL value to AWS JSON value
    fn dsl_value_to_aws(
        &self,
        dsl_name: &str,
        value: &Value,
        attr_type: &AttributeType,
    ) -> Option<serde_json::Value> {
        match dsl_name {
            "availability_zone" => {
                if let Value::String(s) = value {
                    Some(json!(normalize_availability_zone(s)))
                } else {
                    None
                }
            }
            "instance_tenancy" => {
                if let Value::String(s) = value {
                    Some(json!(normalize_instance_tenancy(s)))
                } else {
                    None
                }
            }
            _ => {
                // For Custom (enum) types, convert enum values
                if matches!(attr_type, AttributeType::Custom { .. }) {
                    match value {
                        Value::String(s) => Some(json!(convert_enum_value(s))),
                        Value::UnresolvedIdent(ident, member) => {
                            let raw = if let Some(m) = member {
                                m.clone()
                            } else {
                                ident.clone()
                            };
                            Some(json!(raw.replace('_', "-")))
                        }
                        _ => self.value_to_json(value),
                    }
                } else {
                    self.value_to_json(value)
                }
            }
        }
    }

    /// Convert DSL Value to JSON value
    fn value_to_json(&self, value: &Value) -> Option<serde_json::Value> {
        match value {
            Value::String(s) => Some(json!(s)),
            Value::Bool(b) => Some(json!(b)),
            Value::Int(i) => Some(json!(i)),
            Value::List(items) => {
                let arr: Vec<serde_json::Value> =
                    items.iter().filter_map(|v| self.value_to_json(v)).collect();
                Some(serde_json::Value::Array(arr))
            }
            Value::Map(map) => {
                let obj: serde_json::Map<String, serde_json::Value> = map
                    .iter()
                    .filter_map(|(k, v)| self.value_to_json(v).map(|val| (k.to_pascal_case(), val)))
                    .collect();
                Some(serde_json::Value::Object(obj))
            }
            _ => None,
        }
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
            "ec2_internet_gateway" => {
                // Get VPC attachment
                if let Some(attachments) = props.get("Attachments").and_then(|v| v.as_array())
                    && let Some(first) = attachments.first()
                    && let Some(vpc_id) = first.get("VpcId").and_then(|v| v.as_str())
                {
                    attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
                }
            }
            "ec2_vpc_endpoint" => {
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
        if resource_type == "ec2_eip" && !desired_state.contains_key("Domain") {
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
        if id.resource_type == "ec2_internet_gateway" {
            // Detach from VPC first
            if let Some(props) = self
                .cc_get_resource(config.aws_type_name, identifier)
                .await?
                && let Some(attachments) = props.get("Attachments").and_then(|v| v.as_array())
                && !attachments.is_empty()
            {
                let patch_ops = vec![json!({"op": "remove", "path": "/Attachments"})];
                let _ = self
                    .cc_update_resource(config.aws_type_name, identifier, patch_ops)
                    .await;
            }
        }
        Ok(())
    }

    // =========================================================================
    // Tag Helpers
    // =========================================================================

    /// Build tags array for CloudFormation format
    fn build_tags(&self, user_tags: Option<&Value>) -> Vec<serde_json::Value> {
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
    fn parse_tags(&self, tags_array: &[serde_json::Value]) -> HashMap<String, Value> {
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
}
