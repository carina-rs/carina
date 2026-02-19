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
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use heck::{ToPascalCase, ToSnakeCase};
use serde_json::json;

use carina_core::schema::AttributeType;

use crate::schemas::generated::{
    AwsccSchemaConfig, canonicalize_enum_value, get_enum_valid_values,
};
use carina_core::utils::convert_enum_value;

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

/// Maximum number of retry attempts for retryable create errors
const CREATE_RETRY_MAX_ATTEMPTS: u32 = 12;

/// Initial delay in seconds before retrying a failed create operation
const CREATE_RETRY_INITIAL_DELAY_SECS: u64 = 10;

/// Maximum delay in seconds between create retry attempts
const CREATE_RETRY_MAX_DELAY_SECS: u64 = 120;

/// AWS Cloud Control Provider
pub struct AwsccProvider {
    cloudcontrol_client: CloudControlClient,
    aws_config: aws_config::SdkConfig,
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
            aws_config: config,
            region: region.to_string(),
        }
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

            let response = req.send().await.map_err(|e| {
                ProviderError::new(format!("Failed to list object versions: {:?}", e))
            })?;

            let mut objects_to_delete = Vec::new();

            // Collect versions
            for version in response.versions() {
                if let Some(key) = version.key() {
                    let mut id = aws_sdk_s3::types::ObjectIdentifier::builder().key(key);
                    if let Some(vid) = version.version_id() {
                        id = id.version_id(vid);
                    }
                    objects_to_delete.push(id.build().unwrap());
                }
            }

            // Collect delete markers
            for marker in response.delete_markers() {
                if let Some(key) = marker.key() {
                    let mut id = aws_sdk_s3::types::ObjectIdentifier::builder().key(key);
                    if let Some(vid) = marker.version_id() {
                        id = id.version_id(vid);
                    }
                    objects_to_delete.push(id.build().unwrap());
                }
            }

            // Batch delete (max 1000 per request)
            if !objects_to_delete.is_empty() {
                let delete = aws_sdk_s3::types::Delete::builder()
                    .set_objects(Some(objects_to_delete))
                    .quiet(true)
                    .build()
                    .unwrap();

                s3.delete_objects()
                    .bucket(bucket_name)
                    .delete(delete)
                    .send()
                    .await
                    .map_err(|e| {
                        ProviderError::new(format!("Failed to delete objects: {:?}", e))
                    })?;
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

    /// Create a resource using Cloud Control API, with retry logic for retryable errors.
    ///
    /// Some operations fail transiently due to eventual consistency in AWS
    /// (e.g., IPAM Pool CIDR propagation delays cause "missing a source resource"
    /// errors when creating subnets). This method retries with exponential backoff
    /// for such errors.
    pub async fn cc_create_resource(
        &self,
        type_name: &str,
        desired_state: serde_json::Value,
    ) -> ProviderResult<String> {
        let mut delay_secs = CREATE_RETRY_INITIAL_DELAY_SECS;

        for attempt in 0..=CREATE_RETRY_MAX_ATTEMPTS {
            let result = self
                .cloudcontrol_client
                .create_resource()
                .type_name(type_name)
                .desired_state(desired_state.to_string())
                .send()
                .await;

            match result {
                Ok(response) => {
                    let request_token =
                        response
                            .progress_event()
                            .and_then(|p| p.request_token())
                            .ok_or_else(|| ProviderError::new("No request token returned"))?;

                    match self.wait_for_operation(request_token).await {
                        Ok(identifier) => return Ok(identifier),
                        Err(e)
                            if Self::is_retryable_error(&e.message)
                                && attempt < CREATE_RETRY_MAX_ATTEMPTS =>
                        {
                            eprintln!(
                                "  Retryable error creating {} (attempt {}/{}): {}. Retrying in {}s...",
                                type_name,
                                attempt + 1,
                                CREATE_RETRY_MAX_ATTEMPTS,
                                e.message,
                                delay_secs,
                            );
                            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                            delay_secs = (delay_secs * 2).min(CREATE_RETRY_MAX_DELAY_SECS);
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => {
                    let err_str = format!("{:?}", e);
                    if Self::is_retryable_error(&err_str) && attempt < CREATE_RETRY_MAX_ATTEMPTS {
                        eprintln!(
                            "  Retryable error creating {} (attempt {}/{}): {}. Retrying in {}s...",
                            type_name,
                            attempt + 1,
                            CREATE_RETRY_MAX_ATTEMPTS,
                            err_str,
                            delay_secs,
                        );
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                        delay_secs = (delay_secs * 2).min(CREATE_RETRY_MAX_DELAY_SECS);
                        continue;
                    }
                    return Err(ProviderError::new(format!(
                        "Failed to create resource: {:?}",
                        e
                    )));
                }
            }
        }

        Err(ProviderError::new(format!(
            "Failed to create resource {} after {} retry attempts",
            type_name, CREATE_RETRY_MAX_ATTEMPTS
        )))
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

    /// Delete a resource using Cloud Control API.
    ///
    /// Uses resource-type-specific polling timeouts. IPAM-related resources
    /// get a longer timeout since their deletion via CloudControl API can
    /// take 15-30 minutes.
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
            let max_attempts = Self::max_polling_attempts(type_name, "delete");
            self.wait_for_operation_with_attempts(request_token, max_attempts)
                .await?;
        }

        Ok(())
    }

    /// Returns the max polling attempts for a given resource type and operation.
    ///
    /// Some resource types (e.g., IPAM Pool) take significantly longer to delete
    /// via the CloudControl API than the default timeout allows.
    fn max_polling_attempts(type_name: &str, operation: &str) -> u32 {
        // IPAM Pool deletions can take 15-30 minutes via CloudControl API
        if operation == "delete" && (type_name.contains("IPAMPool") || type_name.contains("IPAM")) {
            return 360; // 30 minutes (360 * 5s)
        }
        120 // Default: 10 minutes (120 * 5s)
    }

    /// Returns true if the error message indicates a retryable condition.
    ///
    /// Some operations fail transiently, e.g., IPAM Pool CIDR propagation
    /// delays cause "missing a source resource" errors for subnet creation.
    fn is_retryable_error(error_message: &str) -> bool {
        let retryable_patterns = [
            "missing a source resource",
            "Throttling",
            "Rate exceeded",
            "RequestLimitExceeded",
            "ServiceUnavailable",
            "InternalError",
        ];
        retryable_patterns
            .iter()
            .any(|pattern| error_message.contains(pattern))
    }

    /// Wait for a Cloud Control operation to complete
    async fn wait_for_operation(&self, request_token: &str) -> ProviderResult<String> {
        self.wait_for_operation_with_attempts(request_token, 120)
            .await
    }

    /// Wait for a Cloud Control operation to complete with a configurable number of attempts
    async fn wait_for_operation_with_attempts(
        &self,
        request_token: &str,
        max_attempts: u32,
    ) -> ProviderResult<String> {
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
            let region_dsl = format!("awscc.Region.{}", self.region.replace('-', "_"));
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
                let dsl_value =
                    self.aws_value_to_dsl(dsl_name, value, &attr_schema.attr_type, resource_type);
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
                let aws_value = self.dsl_value_to_aws(value, &attr_schema.attr_type);
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

        // Preserve create-only attributes from the desired resource.
        // CloudControl API doesn't return create-only properties in GetResource responses,
        // so we need to carry them forward from the desired state.
        for (dsl_name, attr_schema) in &config.schema.attributes {
            if attr_schema.create_only
                && !state.attributes.contains_key(dsl_name)
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
                && let Some(aws_value) = self.dsl_value_to_aws(value, &attr_schema.attr_type)
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
        if lifecycle.force_delete && id.resource_type == "s3_bucket" {
            self.empty_s3_bucket(identifier).await.map_err(|e| {
                ProviderError::new(format!("Failed to empty S3 bucket before deletion: {}", e))
                    .for_resource(id.clone())
            })?;
        }

        self.cc_delete_resource(config.aws_type_name, identifier)
            .await
            .map_err(|e| e.for_resource(id.clone()))
    }

    // =========================================================================
    // Value Conversion Helpers
    // =========================================================================

    /// Convert AWS value to DSL value
    fn aws_value_to_dsl(
        &self,
        dsl_name: &str,
        value: &serde_json::Value,
        attr_type: &AttributeType,
        resource_type: &str,
    ) -> Option<Value> {
        // For Custom enum types with namespace, convert to DSL namespaced format
        if let AttributeType::Custom {
            name: type_name,
            namespace: Some(ns),
            to_dsl,
            ..
        } = attr_type
            && let Some(s) = value.as_str()
        {
            // Canonicalize case using valid values registry
            let canonical =
                if let Some(valid_values) = get_enum_valid_values(resource_type, dsl_name) {
                    canonicalize_enum_value(s, valid_values)
                } else {
                    s.to_string()
                };
            // Apply to_dsl transformation if present (e.g., hyphens → underscores for AZs)
            let dsl_val = to_dsl.map_or_else(|| canonical.clone(), |f| f(&canonical));
            let namespaced = format!("{}.{}.{}", ns, type_name, dsl_val);
            return Some(Value::String(namespaced));
        }
        self.json_to_value(value)
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
        value: &Value,
        attr_type: &AttributeType,
    ) -> Option<serde_json::Value> {
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
                self.cc_update_resource(config.aws_type_name, identifier, patch_ops)
                    .await
                    .map_err(|e| {
                        ProviderError::new(format!(
                            "Failed to detach Internet Gateway from VPC before deletion: {}",
                            e
                        ))
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

// =============================================================================
// Standalone functions for Provider trait methods
// =============================================================================

/// Resolve enum identifiers in resources to their fully-qualified DSL format.
///
/// For each awscc resource, looks up the schema and resolves bare identifiers
/// (e.g., `advanced`) or TypeName.value identifiers (e.g., `Tier.advanced`)
/// into fully-qualified namespaced strings (e.g., `awscc.ec2_ipam.Tier.advanced`).
pub fn resolve_enum_identifiers_impl(resources: &mut [Resource]) {
    let awscc_configs = crate::schemas::generated::configs();

    for resource in resources.iter_mut() {
        // Only handle awscc resources
        let is_awscc = matches!(
            resource.attributes.get("_provider"),
            Some(Value::String(p)) if p == "awscc"
        );
        if !is_awscc {
            continue;
        }

        // Find the matching schema config
        let config = awscc_configs.iter().find(|c| {
            c.schema
                .resource_type
                .strip_prefix("awscc.")
                .map(|t| t == resource.id.resource_type)
                .unwrap_or(false)
        });
        let config = match config {
            Some(c) => c,
            None => continue,
        };

        // Resolve enum attributes
        let mut resolved_attrs = HashMap::new();
        for (key, value) in &resource.attributes {
            if let Some(attr_schema) = config.schema.attributes.get(key.as_str())
                && let AttributeType::Custom {
                    name: type_name,
                    namespace: Some(ns),
                    to_dsl,
                    ..
                } = &attr_schema.attr_type
            {
                let resolved = match value {
                    Value::UnresolvedIdent(ident, None) => {
                        // bare identifier: advanced → awscc.ec2_ipam.Tier.advanced
                        let dsl_val = to_dsl.map_or_else(|| ident.clone(), |f| f(ident));
                        Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                    }
                    Value::UnresolvedIdent(ident, Some(member)) if ident == type_name => {
                        // TypeName.value: Tier.advanced → awscc.ec2_ipam.Tier.advanced
                        let dsl_val = to_dsl.map_or_else(|| member.clone(), |f| f(member));
                        Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                    }
                    Value::String(s) if !s.contains('.') => {
                        // plain string: "ap-northeast-1a" → awscc.AvailabilityZone.ap_northeast_1a
                        let dsl_val = to_dsl.map_or_else(|| s.clone(), |f| f(s));
                        Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                    }
                    _ => value.clone(),
                };
                resolved_attrs.insert(key.clone(), resolved);
                continue;
            }
            resolved_attrs.insert(key.clone(), value.clone());
        }
        resource.attributes = resolved_attrs;
    }
}

/// Restore create-only attributes from saved state into current read states.
///
/// CloudControl API doesn't return create-only properties in GetResource responses,
/// so we carry them forward from the previously saved attribute values.
pub fn restore_create_only_attrs_impl(
    current_states: &mut HashMap<ResourceId, State>,
    saved_attrs: &HashMap<ResourceId, HashMap<String, Value>>,
) {
    let awscc_configs = crate::schemas::generated::configs();

    for (resource_id, state) in current_states.iter_mut() {
        if !state.exists || resource_id.provider != "awscc" {
            continue;
        }
        let config = awscc_configs
            .iter()
            .find(|c| c.resource_type_name == resource_id.resource_type);
        let config = match config {
            Some(c) => c,
            None => continue,
        };
        let saved = match saved_attrs.get(resource_id) {
            Some(attrs) => attrs,
            None => continue,
        };
        for (dsl_name, attr_schema) in &config.schema.attributes {
            if attr_schema.create_only
                && !state.attributes.contains_key(dsl_name)
                && let Some(value) = saved.get(dsl_name)
            {
                state.attributes.insert(dsl_name.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_retryable_error_ipam_source_resource() {
        assert!(AwsccProvider::is_retryable_error(
            "Operation failed: IpamPool 'ipam-pool-xxx' is missing a source resource"
        ));
    }

    #[test]
    fn test_is_retryable_error_throttling() {
        assert!(AwsccProvider::is_retryable_error(
            "Throttling: Rate exceeded"
        ));
    }

    #[test]
    fn test_is_retryable_error_request_limit() {
        assert!(AwsccProvider::is_retryable_error(
            "RequestLimitExceeded: too many requests"
        ));
    }

    #[test]
    fn test_is_retryable_error_service_unavailable() {
        assert!(AwsccProvider::is_retryable_error(
            "ServiceUnavailable: try again later"
        ));
    }

    #[test]
    fn test_is_retryable_error_internal_error() {
        assert!(AwsccProvider::is_retryable_error(
            "InternalError: something went wrong"
        ));
    }

    #[test]
    fn test_is_not_retryable_error() {
        assert!(!AwsccProvider::is_retryable_error(
            "InvalidParameterValue: invalid CIDR"
        ));
        assert!(!AwsccProvider::is_retryable_error(
            "ResourceNotFoundException: not found"
        ));
        assert!(!AwsccProvider::is_retryable_error(
            "AccessDeniedException: not authorized"
        ));
    }

    #[test]
    fn test_max_polling_attempts_ipam_pool_delete() {
        assert_eq!(
            AwsccProvider::max_polling_attempts("AWS::EC2::IPAMPool", "delete"),
            360
        );
    }

    #[test]
    fn test_max_polling_attempts_ipam_delete() {
        assert_eq!(
            AwsccProvider::max_polling_attempts("AWS::EC2::IPAM", "delete"),
            360
        );
    }

    #[test]
    fn test_max_polling_attempts_default_delete() {
        assert_eq!(
            AwsccProvider::max_polling_attempts("AWS::EC2::VPC", "delete"),
            120
        );
    }

    #[test]
    fn test_max_polling_attempts_ipam_create() {
        // IPAM create should use default timeout
        assert_eq!(
            AwsccProvider::max_polling_attempts("AWS::EC2::IPAMPool", "create"),
            120
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_bare_ident() {
        let mut resource = Resource::new("ec2_vpc", "test");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "instance_tenancy".to_string(),
            Value::UnresolvedIdent("dedicated".to_string(), None),
        );

        // After resolution, the bare ident should be fully qualified
        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["instance_tenancy"] {
            Value::String(s) => assert!(
                s.contains("InstanceTenancy") && s.contains("dedicated"),
                "Expected namespaced enum, got: {}",
                s
            ),
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_enum_identifiers_typename_value() {
        let mut resource = Resource::new("ec2_vpc", "test");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "instance_tenancy".to_string(),
            Value::UnresolvedIdent("InstanceTenancy".to_string(), Some("dedicated".to_string())),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["instance_tenancy"] {
            Value::String(s) => assert!(
                s.contains("InstanceTenancy") && s.contains("dedicated"),
                "Expected namespaced enum, got: {}",
                s
            ),
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_enum_identifiers_skips_non_awscc() {
        let mut resource = Resource::new("s3_bucket", "test");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("aws".to_string()));
        resource.attributes.insert(
            "instance_tenancy".to_string(),
            Value::UnresolvedIdent("dedicated".to_string(), None),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        // Should remain unchanged
        assert!(matches!(
            &resources[0].attributes["instance_tenancy"],
            Value::UnresolvedIdent(_, _)
        ));
    }

    #[test]
    fn test_resolve_enum_identifiers_hyphen_to_underscore() {
        // Test that flow log's log_destination_type with hyphens gets converted to underscores
        let mut resource = Resource::new("ec2_flow_log", "test");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "log_destination_type".to_string(),
            Value::UnresolvedIdent("cloud_watch_logs".to_string(), None),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["log_destination_type"] {
            Value::String(s) => {
                assert_eq!(
                    s, "awscc.ec2_flow_log.LogDestinationType.cloud_watch_logs",
                    "Expected underscored namespaced enum, got: {}",
                    s
                );
            }
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_enum_identifiers_hyphen_string_to_underscore() {
        // Test that a plain string with hyphens is converted to underscores via to_dsl
        let mut resource = Resource::new("ec2_flow_log", "test");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "log_destination_type".to_string(),
            Value::String("cloud-watch-logs".to_string()),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["log_destination_type"] {
            Value::String(s) => {
                assert_eq!(
                    s, "awscc.ec2_flow_log.LogDestinationType.cloud_watch_logs",
                    "Hyphenated string should be converted to underscore form, got: {}",
                    s
                );
            }
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_restore_create_only_attrs_impl_basic() {
        // Create a state that's missing a create-only attribute
        let id = ResourceId::with_provider("awscc", "ec2_nat_gateway", "test");
        let mut state = State::existing(id.clone(), HashMap::new());
        state.attributes.insert(
            "nat_gateway_id".to_string(),
            Value::String("nat-123".to_string()),
        );

        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        // Build saved attrs with a create-only attribute (subnet_id is create-only for nat_gateway)
        let mut saved = HashMap::new();
        saved.insert(
            "subnet_id".to_string(),
            Value::String("subnet-abc".to_string()),
        );
        let mut saved_attrs = HashMap::new();
        saved_attrs.insert(id.clone(), saved);

        restore_create_only_attrs_impl(&mut current_states, &saved_attrs);

        // subnet_id is create-only on nat_gateway, so it should be restored
        assert_eq!(
            current_states[&id].attributes.get("subnet_id"),
            Some(&Value::String("subnet-abc".to_string()))
        );
    }

    #[test]
    fn test_restore_create_only_attrs_skips_non_awscc() {
        let id = ResourceId::with_provider("aws", "s3_bucket", "test");
        let state = State::existing(id.clone(), HashMap::new());

        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        let mut saved = HashMap::new();
        saved.insert("some_attr".to_string(), Value::String("value".to_string()));
        let mut saved_attrs = HashMap::new();
        saved_attrs.insert(id.clone(), saved);

        restore_create_only_attrs_impl(&mut current_states, &saved_attrs);

        // Should not have added anything since provider is "aws"
        assert!(!current_states[&id].attributes.contains_key("some_attr"));
    }

    #[test]
    fn test_restore_create_only_attrs_skips_already_present() {
        let id = ResourceId::with_provider("awscc", "ec2_nat_gateway", "test");
        let mut attrs = HashMap::new();
        attrs.insert(
            "subnet_id".to_string(),
            Value::String("subnet-current".to_string()),
        );
        let state = State::existing(id.clone(), attrs);

        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        let mut saved = HashMap::new();
        saved.insert(
            "subnet_id".to_string(),
            Value::String("subnet-saved".to_string()),
        );
        let mut saved_attrs = HashMap::new();
        saved_attrs.insert(id.clone(), saved);

        restore_create_only_attrs_impl(&mut current_states, &saved_attrs);

        // Should keep the current value, not overwrite with saved
        assert_eq!(
            current_states[&id].attributes.get("subnet_id"),
            Some(&Value::String("subnet-current".to_string()))
        );
    }
}
