//! AWS Cloud Control Provider implementation
//!
//! This module contains the main provider implementation that communicates
//! with AWS Cloud Control API to manage resources.

use std::collections::HashMap;
use std::time::Duration;

use aws_config::Region;
use aws_sdk_cloudcontrol::Client as CloudControlClient;
use aws_sdk_cloudcontrol::types::OperationStatus;
use aws_smithy_runtime_api::client::result::SdkError;
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use serde_json::json;

use carina_core::schema::{AttributeType, StructField};

use crate::schemas::generated::{
    AwsccSchemaConfig, canonicalize_enum_value, get_enum_alias_reverse,
};
use carina_core::utils::{convert_enum_value, extract_enum_value_with_values};

/// Get the AwsccSchemaConfig for a resource type
fn get_schema_config(resource_type: &str) -> Option<AwsccSchemaConfig> {
    crate::schemas::generated::configs().into_iter().find(|c| {
        // Match by schema resource_type: "awscc.ec2.vpc" -> "ec2.vpc"
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

/// Maximum number of retry attempts for retryable delete errors
const DELETE_RETRY_MAX_ATTEMPTS: u32 = 12;

/// Initial delay in seconds before retrying a failed delete operation
const DELETE_RETRY_INITIAL_DELAY_SECS: u64 = 10;

/// Maximum delay in seconds between delete retry attempts
const DELETE_RETRY_MAX_DELAY_SECS: u64 = 120;

/// AWS Cloud Control Provider
pub struct AwsccProvider {
    cloudcontrol_client: CloudControlClient,
    aws_config: aws_config::SdkConfig,
}

// =========================================================================
// Value Conversion Helpers
// =========================================================================

/// Convert AWS value to DSL value
fn aws_value_to_dsl(
    dsl_name: &str,
    value: &serde_json::Value,
    attr_type: &AttributeType,
    resource_type: &str,
) -> Option<Value> {
    // For schema-level string enums with namespace, convert to DSL namespaced format.
    if let Some((type_name, ns, to_dsl)) = attr_type.namespaced_enum_parts()
        && let Some(s) = value.as_str()
    {
        let canonical = if let Some((_, values, _, _)) = attr_type.string_enum_parts() {
            let valid_values: Vec<&str> = values.iter().map(String::as_str).collect();
            canonicalize_enum_value(s, &valid_values)
        } else {
            use crate::schemas::generated::get_enum_valid_values;
            if let Some(valid_values) = get_enum_valid_values(resource_type, dsl_name) {
                canonicalize_enum_value(s, valid_values)
            } else {
                s.to_string()
            }
        };
        // Apply to_dsl transformation if present (e.g., hyphens → underscores for AZs)
        let dsl_val = to_dsl.map_or_else(|| canonical.clone(), |f| f(&canonical));
        let namespaced = format!("{}.{}.{}", ns, type_name, dsl_val);
        return Some(Value::String(namespaced));
    }

    // For List types, recurse into each item with the inner type for type-aware conversion
    if let AttributeType::List(inner) = attr_type
        && let Some(arr) = value.as_array()
    {
        let items: Vec<Value> = arr
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let result = aws_value_to_dsl(dsl_name, item, inner, resource_type);
                if result.is_none() {
                    log::warn!(
                        "aws_value_to_dsl: dropping unconvertible array item at index {} for attribute '{}' in resource '{}': {:?}",
                        i, dsl_name, resource_type, item
                    );
                }
                result
            })
            .collect();
        return Some(Value::List(items));
    }

    // For Union types, try each member type and use the first that produces a type-aware result
    if let AttributeType::Union(members) = attr_type {
        for member in members {
            if let Some(result) = aws_value_to_dsl(dsl_name, value, member, resource_type) {
                return Some(result);
            }
        }
        return json_to_value(value);
    }

    // For bare Struct{fields}, recurse into fields
    if let AttributeType::Struct { fields, .. } = attr_type
        && let Some(obj) = value.as_object()
    {
        let map: HashMap<String, Value> = fields
            .iter()
            .filter_map(|field| {
                let provider_key = field.provider_name.as_deref().unwrap_or(&field.name);
                let json_val = obj.get(provider_key)?;
                let dsl_val =
                    aws_value_to_dsl(&field.name, json_val, &field.field_type, resource_type);
                dsl_val.map(|v| (field.name.clone(), v))
            })
            .collect();
        if !map.is_empty() {
            return Some(Value::Map(map));
        }
    }

    // For Map types, preserve keys as-is (user-defined) and recurse into values
    if let AttributeType::Map(inner) = attr_type
        && let Some(obj) = value.as_object()
    {
        let map: HashMap<String, Value> = obj
            .iter()
            .filter_map(|(k, v)| {
                let result = aws_value_to_dsl(dsl_name, v, inner, resource_type);
                if result.is_none() {
                    log::warn!(
                        "aws_value_to_dsl: dropping unconvertible map entry '{}' for attribute '{}' in resource '{}': {:?}",
                        k, dsl_name, resource_type, v
                    );
                }
                result.map(|val| (k.clone(), val))
            })
            .collect();
        return Some(Value::Map(map));
    }

    json_to_value(value)
}

/// Convert JSON value to DSL Value
fn json_to_value(value: &serde_json::Value) -> Option<Value> {
    match value {
        serde_json::Value::String(s) => Some(Value::String(s.clone())),
        serde_json::Value::Bool(b) => Some(Value::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(Value::Int(i))
            } else {
                n.as_f64().map(Value::Float)
            }
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<Value> = arr
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let result = json_to_value(item);
                    if result.is_none() {
                        log::warn!(
                            "json_to_value: dropping unconvertible array item at index {}: {:?}",
                            i,
                            item
                        );
                    }
                    result
                })
                .collect();
            Some(Value::List(items))
        }
        serde_json::Value::Object(obj) => {
            let map: HashMap<String, Value> = obj
                .iter()
                .filter_map(|(k, v)| {
                    let result = json_to_value(v);
                    if result.is_none() {
                        log::warn!(
                            "json_to_value: dropping unconvertible map entry '{}': {:?}",
                            k,
                            v
                        );
                    }
                    result.map(|val| (k.clone(), val))
                })
                .collect();
            Some(Value::Map(map))
        }
        _ => None,
    }
}

/// Convert DSL value to AWS JSON value
fn dsl_value_to_aws(
    value: &Value,
    attr_type: &AttributeType,
    resource_type: &str,
    attr_name: &str,
) -> Option<serde_json::Value> {
    // For schema-level string enums, convert namespaced DSL values back to provider values.
    if attr_type.namespaced_enum_parts().is_some() {
        match value {
            Value::String(s) => {
                // Extract the raw enum value from the namespaced identifier, using
                // known valid values for disambiguation when enum values contain dots
                // (e.g., "ipsec.1" in "awscc.ec2.vpn_gateway.Type.ipsec.1").
                let raw = if let Some((_, values, _, _)) = attr_type.string_enum_parts() {
                    let valid: Vec<&str> = values.iter().map(String::as_str).collect();
                    let raw_extracted = extract_enum_value_with_values(s, &valid);
                    canonicalize_enum_value(raw_extracted, &valid)
                } else {
                    convert_enum_value(s)
                };
                // Apply alias reverse mapping (e.g., "all" -> "-1")
                let resolved = match get_enum_alias_reverse(resource_type, attr_name, &raw) {
                    Some(canonical) => canonical.to_string(),
                    None => raw,
                };
                Some(json!(resolved))
            }
            Value::UnresolvedIdent(ident, member) => {
                let raw = if let Some(m) = member {
                    m.clone()
                } else {
                    ident.clone()
                };
                // Use valid values when available for correct resolution
                let converted = if let Some((_, values, _, _)) = attr_type.string_enum_parts() {
                    let valid: Vec<&str> = values.iter().map(String::as_str).collect();
                    canonicalize_enum_value(&raw, &valid)
                } else {
                    raw.replace('_', "-")
                };
                // Apply alias reverse mapping (e.g., "all" -> "-1")
                let resolved = match get_enum_alias_reverse(resource_type, attr_name, &converted) {
                    Some(canonical) => canonical.to_string(),
                    None => converted,
                };
                Some(json!(resolved))
            }
            _ => value_to_json(value),
        }
    } else if let AttributeType::List(inner) = attr_type
        && let Value::List(items) = value
    {
        // Recurse into list items with inner type for type-aware conversion
        let arr: Vec<serde_json::Value> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let result = dsl_value_to_aws(item, inner, resource_type, attr_name);
                if result.is_none() {
                    log::warn!(
                        "dsl_value_to_aws: dropping unconvertible list item at index {} for attribute '{}' in resource '{}': {:?}",
                        i, attr_name, resource_type, item
                    );
                }
                result
            })
            .collect();
        Some(serde_json::Value::Array(arr))
    } else if let AttributeType::Union(members) = attr_type {
        // Try each member type; use the first that produces a type-aware result
        for member in members {
            if let Some(result) = dsl_value_to_aws(value, member, resource_type, attr_name) {
                return Some(result);
            }
        }
        value_to_json(value)
    } else if let AttributeType::Struct { fields, .. } = attr_type
        && let Value::Map(map) = value
    {
        // Recurse into bare struct fields for type-aware conversion (map assignment syntax)
        let obj: serde_json::Map<String, serde_json::Value> = fields
            .iter()
            .filter_map(|field| {
                let dsl_val = map.get(&field.name)?;
                let provider_key = field
                    .provider_name
                    .as_deref()
                    .unwrap_or(&field.name)
                    .to_string();
                let json_val =
                    dsl_value_to_aws(dsl_val, &field.field_type, resource_type, &field.name);
                json_val.map(|v| (provider_key, v))
            })
            .collect();
        Some(serde_json::Value::Object(obj))
    } else if let AttributeType::Struct { fields, .. } = attr_type
        && let Value::List(items) = value
        && items.len() == 1
        && let Value::Map(map) = &items[0]
    {
        // Recurse into bare struct fields for type-aware conversion (block syntax)
        let obj: serde_json::Map<String, serde_json::Value> = fields
            .iter()
            .filter_map(|field| {
                let dsl_val = map.get(&field.name)?;
                let provider_key = field
                    .provider_name
                    .as_deref()
                    .unwrap_or(&field.name)
                    .to_string();
                let json_val =
                    dsl_value_to_aws(dsl_val, &field.field_type, resource_type, &field.name);
                json_val.map(|v| (provider_key, v))
            })
            .collect();
        Some(serde_json::Value::Object(obj))
    } else if let AttributeType::Map(inner) = attr_type
        && let Value::Map(map) = value
    {
        // Map type: preserve keys as-is (user-defined), recurse into values with inner type
        let obj: serde_json::Map<String, serde_json::Value> = map
            .iter()
            .filter_map(|(k, v)| {
                let result = dsl_value_to_aws(v, inner, resource_type, attr_name);
                if result.is_none() {
                    log::warn!(
                        "dsl_value_to_aws: dropping unconvertible map entry '{}' for attribute '{}' in resource '{}': {:?}",
                        k, attr_name, resource_type, v
                    );
                }
                result.map(|val| (k.clone(), val))
            })
            .collect();
        Some(serde_json::Value::Object(obj))
    } else {
        value_to_json(value)
    }
}

/// Convert DSL Value to JSON value
fn value_to_json(value: &Value) -> Option<serde_json::Value> {
    match value {
        Value::String(s) => Some(json!(s)),
        Value::Bool(b) => Some(json!(b)),
        Value::Int(i) => Some(json!(i)),
        Value::Float(f) if f.is_finite() => Some(json!(f)),
        Value::Float(_) => None,
        Value::List(items) => {
            let arr: Vec<serde_json::Value> = items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let result = value_to_json(item);
                    if result.is_none() {
                        log::warn!(
                            "value_to_json: dropping unconvertible list item at index {}: {:?}",
                            i,
                            item
                        );
                    }
                    result
                })
                .collect();
            Some(serde_json::Value::Array(arr))
        }
        Value::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .filter_map(|(k, v)| {
                    let result = value_to_json(v);
                    if result.is_none() {
                        log::warn!(
                            "value_to_json: dropping unconvertible map entry '{}': {:?}",
                            k,
                            v
                        );
                    }
                    result.map(|val| (k.clone(), val))
                })
                .collect();
            Some(serde_json::Value::Object(obj))
        }
        _ => None,
    }
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
                    let props = parse_resource_properties(props_str)?;
                    Ok(Some(props))
                } else {
                    Ok(None)
                }
            }
            Err(e) => {
                if Self::is_not_found_error(&e) {
                    Ok(None)
                } else {
                    Err(ProviderError::new("Failed to get resource").with_cause(e))
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
                            if Self::is_retryable_status_message(&e.message)
                                && attempt < CREATE_RETRY_MAX_ATTEMPTS =>
                        {
                            log::warn!(
                                "Retryable error creating {} (attempt {}/{}): {}. Retrying in {}s...",
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
                    if Self::is_retryable_sdk_error(&e) && attempt < CREATE_RETRY_MAX_ATTEMPTS {
                        log::warn!(
                            "Retryable error creating {} (attempt {}/{}): {:?}. Retrying in {}s...",
                            type_name,
                            attempt + 1,
                            CREATE_RETRY_MAX_ATTEMPTS,
                            e,
                            delay_secs,
                        );
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                        delay_secs = (delay_secs * 2).min(CREATE_RETRY_MAX_DELAY_SECS);
                        continue;
                    }
                    return Err(ProviderError::new("Failed to create resource").with_cause(e));
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
            .map_err(|e| ProviderError::new("Failed to build patch").with_cause(e))?;

        let result = self
            .cloudcontrol_client
            .update_resource()
            .type_name(type_name)
            .identifier(identifier)
            .patch_document(patch_document)
            .send()
            .await
            .map_err(|e| ProviderError::new("Failed to update resource").with_cause(e))?;

        if let Some(request_token) = result.progress_event().and_then(|p| p.request_token()) {
            self.wait_for_operation(request_token).await?;
        }

        Ok(())
    }

    /// Delete a resource using Cloud Control API, with retry logic for retryable errors.
    ///
    /// Uses resource-type-specific polling timeouts. IPAM-related resources
    /// get a longer timeout since their deletion via CloudControl API can
    /// take 15-30 minutes. Retries with exponential backoff on transient errors
    /// such as throttling or service unavailability.
    pub async fn cc_delete_resource(
        &self,
        type_name: &str,
        identifier: &str,
    ) -> ProviderResult<()> {
        let mut delay_secs = DELETE_RETRY_INITIAL_DELAY_SECS;
        let max_polling_attempts = Self::max_polling_attempts(type_name, "delete");

        for attempt in 0..=DELETE_RETRY_MAX_ATTEMPTS {
            let result = self
                .cloudcontrol_client
                .delete_resource()
                .type_name(type_name)
                .identifier(identifier)
                .send()
                .await;

            match result {
                Ok(response) => {
                    if let Some(request_token) =
                        response.progress_event().and_then(|p| p.request_token())
                    {
                        match self
                            .wait_for_operation_with_attempts(request_token, max_polling_attempts)
                            .await
                        {
                            Ok(_) => return Ok(()),
                            Err(e)
                                if Self::is_retryable_status_message(&e.message)
                                    && attempt < DELETE_RETRY_MAX_ATTEMPTS =>
                            {
                                log::warn!(
                                    "Retryable error deleting {} (attempt {}/{}): {}. Retrying in {}s...",
                                    type_name,
                                    attempt + 1,
                                    DELETE_RETRY_MAX_ATTEMPTS,
                                    e.message,
                                    delay_secs,
                                );
                                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                                delay_secs = (delay_secs * 2).min(DELETE_RETRY_MAX_DELAY_SECS);
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    return Ok(());
                }
                Err(e) => {
                    if Self::is_retryable_sdk_error(&e) && attempt < DELETE_RETRY_MAX_ATTEMPTS {
                        log::warn!(
                            "Retryable error deleting {} (attempt {}/{}): {:?}. Retrying in {}s...",
                            type_name,
                            attempt + 1,
                            DELETE_RETRY_MAX_ATTEMPTS,
                            e,
                            delay_secs,
                        );
                        tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                        delay_secs = (delay_secs * 2).min(DELETE_RETRY_MAX_DELAY_SECS);
                        continue;
                    }
                    return Err(ProviderError::new("Failed to delete resource").with_cause(e));
                }
            }
        }

        Err(ProviderError::new(format!(
            "Failed to delete resource {} after {} retry attempts",
            type_name, DELETE_RETRY_MAX_ATTEMPTS
        )))
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

    /// Returns true if the SDK error represents a "not found" condition.
    ///
    /// Uses structured error metadata (`ProvideErrorMetadata::code()`) instead of
    /// fragile string matching against Debug-formatted output.
    ///
    /// Not-found error codes:
    /// - `ResourceNotFoundException`: The resource does not exist
    /// - `HandlerNotFoundException`: The resource handler was not found
    fn is_not_found_error<E, R>(error: &SdkError<E, R>) -> bool
    where
        E: ProvideErrorMetadata,
    {
        const NOT_FOUND_ERROR_CODES: &[&str] =
            &["ResourceNotFoundException", "HandlerNotFoundException"];

        match error {
            SdkError::ServiceError(service_error) => {
                let err = service_error.err();
                if let Some(code) = err.code() {
                    NOT_FOUND_ERROR_CODES.contains(&code)
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Returns true if an AWS SDK error represents a retryable condition.
    ///
    /// Uses structured error types from the AWS SDK rather than string matching.
    /// This detects retryable conditions based on the error variant or error code,
    /// which are part of the AWS API contract and more stable than error messages.
    ///
    /// Retryable error types:
    /// - `ThrottlingException`: Request rate exceeded (covers "Throttling", "Rate exceeded")
    /// - `ServiceInternalErrorException`: AWS internal server error
    /// - `HandlerFailureException`: Resource handler failed (may be transient)
    /// - `HandlerInternalFailureException`: Internal handler error
    /// - `NetworkFailureException`: Network connectivity issues
    /// - `ConcurrentOperationException`: Another operation is in progress
    /// - `NotStabilizedException`: Resource not yet stabilized
    /// - `SdkError::TimeoutError`: Connection timeout
    /// - `SdkError::DispatchFailure`: HTTP dispatch failure
    fn is_retryable_sdk_error<E, R>(error: &SdkError<E, R>) -> bool
    where
        E: ProvideErrorMetadata,
    {
        /// Error codes from the CloudControl API that indicate retryable conditions.
        const RETRYABLE_ERROR_CODES: &[&str] = &[
            "ThrottlingException",
            "ServiceInternalErrorException",
            "HandlerFailureException",
            "HandlerInternalFailureException",
            "NetworkFailureException",
            "ConcurrentOperationException",
            "NotStabilizedException",
        ];

        match error {
            SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) => true,
            SdkError::ServiceError(service_error) => {
                let err = service_error.err();
                if let Some(code) = err.code() {
                    RETRYABLE_ERROR_CODES.contains(&code)
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Returns true if a CloudControl operation status message indicates a retryable condition.
    ///
    /// When a CloudControl operation (create/delete) succeeds at the API level but the
    /// async operation fails, the error details come as a plain-text status message from
    /// `progress_event.status_message()`. These messages don't have structured error codes,
    /// so string pattern matching is the only option.
    ///
    /// **Fragility note**: These patterns depend on AWS error message wording. If AWS
    /// changes the message format, retries may silently stop working. The patterns below
    /// are based on observed CloudControl API behavior as of 2025:
    ///
    /// - `"missing a source resource"`: IPAM Pool CIDR propagation delay causes subnet
    ///   creation to fail transiently while the pool is still provisioning.
    /// - `"Throttling"` / `"Rate exceeded"` / `"RequestLimitExceeded"`: Downstream service
    ///   throttling reported through CloudControl operation status.
    /// - `"ServiceUnavailable"` / `"InternalError"`: Transient downstream service errors
    ///   reported through CloudControl operation status.
    fn is_retryable_status_message(status_message: &str) -> bool {
        const RETRYABLE_STATUS_PATTERNS: &[&str] = &[
            "missing a source resource",
            "Throttling",
            "Rate exceeded",
            "RequestLimitExceeded",
            "ServiceUnavailable",
            "InternalError",
        ];
        RETRYABLE_STATUS_PATTERNS
            .iter()
            .any(|pattern| status_message.contains(pattern))
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
                .map_err(|e| ProviderError::new("Failed to get operation status").with_cause(e))?;

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
/// into fully-qualified namespaced strings (e.g., `awscc.ec2.ipam.Tier.advanced`).
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
                && let Some((type_name, ns, to_dsl)) = attr_schema.attr_type.namespaced_enum_parts()
            {
                let resolved = match value {
                    Value::UnresolvedIdent(ident, None) => {
                        // bare identifier: advanced → awscc.ec2.ipam.Tier.advanced
                        let dsl_val = to_dsl.map_or_else(|| ident.clone(), |f| f(ident));
                        Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                    }
                    Value::UnresolvedIdent(ident, Some(member)) if ident == type_name => {
                        // TypeName.value: Tier.advanced → awscc.ec2.ipam.Tier.advanced
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

            // Handle struct fields containing schema-level string enums.
            if let Some(attr_schema) = config.schema.attributes.get(key.as_str()) {
                let struct_fields = match &attr_schema.attr_type {
                    AttributeType::List(inner) => {
                        if let AttributeType::Struct { fields, .. } = inner.as_ref() {
                            Some(fields)
                        } else {
                            None
                        }
                    }
                    AttributeType::Struct { fields, .. } => Some(fields),
                    _ => None,
                };

                if let Some(fields) = struct_fields {
                    let resolved = resolve_struct_enum_values(value, fields);
                    resolved_attrs.insert(key.clone(), resolved);
                    continue;
                }
            }

            resolved_attrs.insert(key.clone(), value.clone());
        }
        resource.attributes = resolved_attrs;
    }
}

/// Resolve enum identifiers within struct field values.
/// Recurses into List and Map values, resolving UnresolvedIdent values
/// for struct fields that have StringEnum type with namespace.
fn resolve_struct_enum_values(value: &Value, fields: &[StructField]) -> Value {
    match value {
        Value::List(items) => {
            let resolved_items: Vec<Value> = items
                .iter()
                .map(|item| resolve_struct_enum_values(item, fields))
                .collect();
            Value::List(resolved_items)
        }
        Value::Map(map) => {
            let mut resolved_map = HashMap::new();
            for (field_key, field_value) in map {
                if let Some(field) = fields.iter().find(|f| f.name == *field_key)
                    && let Some((type_name, ns, to_dsl)) = field.field_type.namespaced_enum_parts()
                {
                    let resolved = match field_value {
                        Value::UnresolvedIdent(ident, None) => {
                            let dsl_val = to_dsl.map_or_else(|| ident.clone(), |f| f(ident));
                            Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                        }
                        Value::UnresolvedIdent(ident, Some(member)) if ident == type_name => {
                            let dsl_val = to_dsl.map_or_else(|| member.clone(), |f| f(member));
                            Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                        }
                        Value::String(s) if !s.contains('.') => {
                            let dsl_val = to_dsl.map_or_else(|| s.clone(), |f| f(s));
                            Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                        }
                        _ => field_value.clone(),
                    };
                    resolved_map.insert(field_key.clone(), resolved);
                    continue;
                }
                resolved_map.insert(field_key.clone(), field_value.clone());
            }
            Value::Map(resolved_map)
        }
        _ => value.clone(),
    }
}

/// Restore unreturned attributes from saved state into current read states.
///
/// CloudControl API doesn't always return all properties in GetResource responses
/// (create-only properties, and some normal properties like `description`).
/// We carry them forward from the previously saved attribute values.
pub fn restore_unreturned_attrs_impl(
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
        for dsl_name in config.schema.attributes.keys() {
            if !state.attributes.contains_key(dsl_name)
                && let Some(value) = saved.get(dsl_name)
            {
                state.attributes.insert(dsl_name.clone(), value.clone());
            }
        }
    }
}

/// Parse a JSON string from CloudControl API response into a `serde_json::Value`.
///
/// Returns an error instead of silently returning an empty object when the JSON is malformed.
fn parse_resource_properties(props_str: &str) -> ProviderResult<serde_json::Value> {
    serde_json::from_str(props_str)
        .map_err(|e| ProviderError::new("Failed to parse resource properties").with_cause(e))
}

/// Build JSON Patch operations for updating a resource.
///
/// Compares `from` (current state) and `to` (desired state) to generate:
/// - `"replace"` operations for attributes present in `to`
/// - `"remove"` operations for attributes present in `from` but absent in `to`
///   (only for non-required, non-create-only attributes with a provider_name)
fn build_update_patches(
    config: &AwsccSchemaConfig,
    from: &State,
    to: &Resource,
) -> Vec<serde_json::Value> {
    let mut patch_ops = Vec::new();
    let resource_type = &to.id.resource_type;

    // Build replace operations for attributes that changed between `from` and `to`
    for (dsl_name, attr_schema) in &config.schema.attributes {
        // Skip tags - handled separately below
        if dsl_name == "tags" {
            continue;
        }
        if let Some(aws_name) = &attr_schema.provider_name
            && let Some(value) = to.attributes.get(dsl_name.as_str())
            && let Some(aws_value) =
                dsl_value_to_aws(value, &attr_schema.attr_type, resource_type, dsl_name)
        {
            // Skip if the value is unchanged from the current state
            if let Some(from_value) = from.attributes.get(dsl_name)
                && from_value == value
            {
                continue;
            }
            patch_ops.push(json!({
                "op": "replace",
                "path": format!("/{}", aws_name),
                "value": aws_value
            }));
        }
    }

    // Build remove operations for attributes present in `from` but absent in `to`
    for (dsl_name, attr_schema) in &config.schema.attributes {
        if dsl_name == "tags" {
            continue;
        }
        // Only generate remove for attributes that:
        // 1. Have a provider_name (so we know the AWS path)
        // 2. Are not required (required attributes cannot be removed)
        // 3. Are not create-only (create-only attributes cannot be changed after creation)
        // 4. Exist in from but not in to
        if let Some(aws_name) = &attr_schema.provider_name
            && !attr_schema.required
            && !attr_schema.create_only
            && from.attributes.contains_key(dsl_name)
            && !to.attributes.contains_key(dsl_name)
        {
            patch_ops.push(json!({
                "op": "remove",
                "path": format!("/{}", aws_name)
            }));
        }
    }

    // Handle tags
    if config.has_tags {
        if let Some(Value::Map(user_tags)) = to.attributes.get("tags") {
            // Skip if tags are unchanged from the current state
            let tags_unchanged = matches!(from.attributes.get("tags"), Some(Value::Map(from_tags)) if from_tags == user_tags);
            if !tags_unchanged {
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
        } else if from.attributes.contains_key("tags") {
            // Tags existed in from but removed in to: generate remove operation
            patch_ops.push(json!({"op": "remove", "path": "/Tags"}));
        }
    }

    patch_ops
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // is_retryable_status_message tests (for CloudControl operation status strings)
    // =========================================================================

    #[test]
    fn test_is_retryable_status_message_ipam_source_resource() {
        assert!(AwsccProvider::is_retryable_status_message(
            "Operation failed: IpamPool 'ipam-pool-xxx' is missing a source resource"
        ));
    }

    #[test]
    fn test_is_retryable_status_message_throttling() {
        assert!(AwsccProvider::is_retryable_status_message(
            "Throttling: Rate exceeded"
        ));
    }

    #[test]
    fn test_is_retryable_status_message_request_limit() {
        assert!(AwsccProvider::is_retryable_status_message(
            "RequestLimitExceeded: too many requests"
        ));
    }

    #[test]
    fn test_is_retryable_status_message_service_unavailable() {
        assert!(AwsccProvider::is_retryable_status_message(
            "ServiceUnavailable: try again later"
        ));
    }

    #[test]
    fn test_is_retryable_status_message_internal_error() {
        assert!(AwsccProvider::is_retryable_status_message(
            "InternalError: something went wrong"
        ));
    }

    #[test]
    fn test_is_not_retryable_status_message() {
        assert!(!AwsccProvider::is_retryable_status_message(
            "InvalidParameterValue: invalid CIDR"
        ));
        assert!(!AwsccProvider::is_retryable_status_message(
            "ResourceNotFoundException: not found"
        ));
        assert!(!AwsccProvider::is_retryable_status_message(
            "AccessDeniedException: not authorized"
        ));
    }

    // =========================================================================
    // is_retryable_sdk_error tests (for structured AWS SDK error types)
    // =========================================================================

    /// Helper to build an ErrorMetadata with a given error code.
    fn error_meta(code: &str) -> aws_smithy_types::error::ErrorMetadata {
        aws_smithy_types::error::ErrorMetadata::builder()
            .code(code)
            .build()
    }

    #[test]
    fn test_is_retryable_sdk_error_throttling() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::ThrottlingException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::ThrottlingException(
            ThrottlingException::builder()
                .message("Rate exceeded")
                .meta(error_meta("ThrottlingException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_service_internal() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::ServiceInternalErrorException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::ServiceInternalErrorException(
            ServiceInternalErrorException::builder()
                .message("Internal error")
                .meta(error_meta("ServiceInternalErrorException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_handler_failure() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::HandlerFailureException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::HandlerFailureException(
            HandlerFailureException::builder()
                .message("Handler failed")
                .meta(error_meta("HandlerFailureException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_handler_internal_failure() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::HandlerInternalFailureException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::HandlerInternalFailureException(
            HandlerInternalFailureException::builder()
                .message("Internal failure")
                .meta(error_meta("HandlerInternalFailureException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_network_failure() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::NetworkFailureException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::NetworkFailureException(
            NetworkFailureException::builder()
                .message("Network error")
                .meta(error_meta("NetworkFailureException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_concurrent_operation() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::ConcurrentOperationException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::ConcurrentOperationException(
            ConcurrentOperationException::builder()
                .message("Concurrent operation")
                .meta(error_meta("ConcurrentOperationException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_not_stabilized() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::NotStabilizedException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::NotStabilizedException(
            NotStabilizedException::builder()
                .message("Not stabilized")
                .meta(error_meta("NotStabilizedException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_not_retryable_sdk_error_invalid_request() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::InvalidRequestException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::InvalidRequestException(
            InvalidRequestException::builder()
                .message("Invalid request")
                .meta(error_meta("InvalidRequestException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(!AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_not_retryable_sdk_error_already_exists() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_sdk_cloudcontrol::types::error::AlreadyExistsException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = CreateResourceError::AlreadyExistsException(
            AlreadyExistsException::builder()
                .message("Already exists")
                .meta(error_meta("AlreadyExistsException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(!AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_timeout() {
        use aws_sdk_cloudcontrol::operation::create_resource::CreateResourceError;
        use aws_smithy_runtime_api::client::result::SdkError;

        let sdk_err: SdkError<CreateResourceError, http::Response<&str>> =
            SdkError::timeout_error("connection timed out");
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_retryable_sdk_error_delete_throttling() {
        use aws_sdk_cloudcontrol::operation::delete_resource::DeleteResourceError;
        use aws_sdk_cloudcontrol::types::error::ThrottlingException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = DeleteResourceError::ThrottlingException(
            ThrottlingException::builder()
                .message("Rate exceeded")
                .meta(error_meta("ThrottlingException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    #[test]
    fn test_is_not_retryable_sdk_error_delete_type_not_found() {
        use aws_sdk_cloudcontrol::operation::delete_resource::DeleteResourceError;
        use aws_sdk_cloudcontrol::types::error::TypeNotFoundException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = DeleteResourceError::TypeNotFoundException(
            TypeNotFoundException::builder()
                .message("Type not found")
                .meta(error_meta("TypeNotFoundException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(!AwsccProvider::is_retryable_sdk_error(&sdk_err));
    }

    // =========================================================================
    // is_not_found_error tests (for structured "not found" error detection)
    // =========================================================================

    #[test]
    fn test_is_not_found_error_resource_not_found() {
        use aws_sdk_cloudcontrol::operation::get_resource::GetResourceError;
        use aws_sdk_cloudcontrol::types::error::ResourceNotFoundException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = GetResourceError::ResourceNotFoundException(
            ResourceNotFoundException::builder()
                .message("Resource not found")
                .meta(error_meta("ResourceNotFoundException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_not_found_error(&sdk_err));
    }

    #[test]
    fn test_is_not_found_error_handler_not_found_code() {
        use aws_sdk_cloudcontrol::operation::get_resource::GetResourceError;
        use aws_smithy_runtime_api::client::result::SdkError;

        // HandlerNotFoundException is not a typed variant, but may appear as an error code
        let err = GetResourceError::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .code("HandlerNotFoundException")
                .message("Handler not found")
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(AwsccProvider::is_not_found_error(&sdk_err));
    }

    #[test]
    fn test_is_not_found_error_false_for_throttling() {
        use aws_sdk_cloudcontrol::operation::get_resource::GetResourceError;
        use aws_sdk_cloudcontrol::types::error::ThrottlingException;
        use aws_smithy_runtime_api::client::result::SdkError;

        let err = GetResourceError::ThrottlingException(
            ThrottlingException::builder()
                .message("Rate exceeded")
                .meta(error_meta("ThrottlingException"))
                .build(),
        );
        let sdk_err = SdkError::service_error(err, http::Response::new(""));
        assert!(!AwsccProvider::is_not_found_error(&sdk_err));
    }

    #[test]
    fn test_is_not_found_error_false_for_timeout() {
        use aws_sdk_cloudcontrol::operation::get_resource::GetResourceError;
        use aws_smithy_runtime_api::client::result::SdkError;

        let sdk_err: SdkError<GetResourceError, http::Response<&str>> =
            SdkError::timeout_error("connection timed out");
        assert!(!AwsccProvider::is_not_found_error(&sdk_err));
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
        let mut resource = Resource::new("ec2.vpc", "test");
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
        let mut resource = Resource::new("ec2.vpc", "test");
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
        let mut resource = Resource::new("s3.bucket", "test");
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
        let mut resource = Resource::new("ec2.flow_log", "test");
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
                    s, "awscc.ec2.flow_log.LogDestinationType.cloud_watch_logs",
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
        let mut resource = Resource::new("ec2.flow_log", "test");
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
                    s, "awscc.ec2.flow_log.LogDestinationType.cloud_watch_logs",
                    "Hyphenated string should be converted to underscore form, got: {}",
                    s
                );
            }
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_restore_unreturned_attrs_impl_create_only() {
        // Create a state that's missing a create-only attribute
        let id = ResourceId::with_provider("awscc", "ec2.nat_gateway", "test");
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

        restore_unreturned_attrs_impl(&mut current_states, &saved_attrs);

        // subnet_id is create-only on nat_gateway, so it should be restored
        assert_eq!(
            current_states[&id].attributes.get("subnet_id"),
            Some(&Value::String("subnet-abc".to_string()))
        );
    }

    #[test]
    fn test_restore_unreturned_attrs_skips_non_awscc() {
        let id = ResourceId::with_provider("aws", "s3.bucket", "test");
        let state = State::existing(id.clone(), HashMap::new());

        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        let mut saved = HashMap::new();
        saved.insert("some_attr".to_string(), Value::String("value".to_string()));
        let mut saved_attrs = HashMap::new();
        saved_attrs.insert(id.clone(), saved);

        restore_unreturned_attrs_impl(&mut current_states, &saved_attrs);

        // Should not have added anything since provider is "aws"
        assert!(!current_states[&id].attributes.contains_key("some_attr"));
    }

    #[test]
    fn test_restore_unreturned_attrs_skips_already_present() {
        let id = ResourceId::with_provider("awscc", "ec2.nat_gateway", "test");
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

        restore_unreturned_attrs_impl(&mut current_states, &saved_attrs);

        // Should keep the current value, not overwrite with saved
        assert_eq!(
            current_states[&id].attributes.get("subnet_id"),
            Some(&Value::String("subnet-current".to_string()))
        );
    }

    #[test]
    fn test_restore_unreturned_attrs_impl_non_create_only() {
        // Test that non-create-only attributes (like description on security_group_egress)
        // are also restored when CloudControl doesn't return them
        let id = ResourceId::with_provider("awscc", "ec2.security_group_egress", "test");
        let mut state = State::existing(id.clone(), HashMap::new());
        state.attributes.insert(
            "ip_protocol".to_string(),
            Value::String("awscc.ec2.security_group_egress.IpProtocol.all".to_string()),
        );

        let mut current_states = HashMap::new();
        current_states.insert(id.clone(), state);

        // description is NOT create-only but CloudControl doesn't return it
        let mut saved = HashMap::new();
        saved.insert(
            "description".to_string(),
            Value::String("Allow all outbound".to_string()),
        );
        let mut saved_attrs = HashMap::new();
        saved_attrs.insert(id.clone(), saved);

        restore_unreturned_attrs_impl(&mut current_states, &saved_attrs);

        // description should be restored even though it's not create-only
        assert_eq!(
            current_states[&id].attributes.get("description"),
            Some(&Value::String("Allow all outbound".to_string()))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_ip_protocol_all_alias() {
        // Test that bare "all" identifier resolves to namespaced form for IpProtocol
        let mut resource = Resource::new("ec2.security_group_egress", "test");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("all".to_string(), None),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["ip_protocol"] {
            Value::String(s) => {
                assert_eq!(
                    s, "awscc.ec2.security_group_egress.IpProtocol.all",
                    "Expected namespaced IpProtocol.all, got: {}",
                    s
                );
            }
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    #[test]
    fn test_resolve_enum_identifiers_ip_protocol_tcp() {
        // Test that bare "tcp" identifier resolves correctly
        let mut resource = Resource::new("ec2.security_group_egress", "test");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("tcp".to_string(), None),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        match &resources[0].attributes["ip_protocol"] {
            Value::String(s) => {
                assert_eq!(
                    s, "awscc.ec2.security_group_egress.IpProtocol.tcp",
                    "Expected namespaced IpProtocol.tcp, got: {}",
                    s
                );
            }
            other => panic!("Expected String, got: {:?}", other),
        }
    }

    /// Helper to create struct fields with a Custom enum type for testing
    fn test_ip_protocol_fields() -> Vec<StructField> {
        vec![
            StructField::new(
                "ip_protocol",
                AttributeType::Custom {
                    name: "IpProtocol".to_string(),
                    base: Box::new(AttributeType::String),
                    validate: |_| Ok(()),
                    namespace: Some("awscc.ec2.security_group".to_string()),
                    to_dsl: Some(|s: &str| match s {
                        "-1" => "all".to_string(),
                        _ => s.to_string(),
                    }),
                },
            )
            .with_provider_name("IpProtocol"),
            StructField::new("from_port", AttributeType::Int).with_provider_name("FromPort"),
            StructField::new("cidr_ip", AttributeType::String).with_provider_name("CidrIp"),
        ]
    }

    #[test]
    fn test_resolve_struct_enum_values_bare_ident() {
        let fields = test_ip_protocol_fields();
        let mut map = HashMap::new();
        map.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("all".to_string(), None),
        );
        map.insert("from_port".to_string(), Value::Int(443));
        let value = Value::List(vec![Value::Map(map)]);

        let resolved = resolve_struct_enum_values(&value, &fields);
        if let Value::List(items) = resolved {
            if let Value::Map(m) = &items[0] {
                match &m["ip_protocol"] {
                    Value::String(s) => {
                        assert_eq!(s, "awscc.ec2.security_group.IpProtocol.all");
                    }
                    other => panic!("Expected String, got: {:?}", other),
                }
                // Non-enum field should be unchanged
                assert_eq!(m["from_port"], Value::Int(443));
            } else {
                panic!("Expected Map");
            }
        } else {
            panic!("Expected List");
        }
    }

    #[test]
    fn test_resolve_struct_enum_values_typename_dot_value() {
        let fields = test_ip_protocol_fields();
        let mut map = HashMap::new();
        map.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("IpProtocol".to_string(), Some("tcp".to_string())),
        );
        let value = Value::List(vec![Value::Map(map)]);

        let resolved = resolve_struct_enum_values(&value, &fields);
        if let Value::List(items) = resolved {
            if let Value::Map(m) = &items[0] {
                match &m["ip_protocol"] {
                    Value::String(s) => {
                        assert_eq!(s, "awscc.ec2.security_group.IpProtocol.tcp");
                    }
                    other => panic!("Expected String, got: {:?}", other),
                }
            } else {
                panic!("Expected Map");
            }
        } else {
            panic!("Expected List");
        }
    }

    #[test]
    fn test_resolve_struct_enum_values_string_passthrough() {
        let fields = test_ip_protocol_fields();
        let mut map = HashMap::new();
        // Already-resolved string with dots should pass through unchanged
        map.insert(
            "ip_protocol".to_string(),
            Value::String("awscc.ec2.security_group.IpProtocol.tcp".to_string()),
        );
        let value = Value::List(vec![Value::Map(map)]);

        let resolved = resolve_struct_enum_values(&value, &fields);
        if let Value::List(items) = resolved {
            if let Value::Map(m) = &items[0] {
                match &m["ip_protocol"] {
                    Value::String(s) => {
                        assert_eq!(s, "awscc.ec2.security_group.IpProtocol.tcp");
                    }
                    other => panic!("Expected String, got: {:?}", other),
                }
            } else {
                panic!("Expected Map");
            }
        } else {
            panic!("Expected List");
        }
    }

    #[test]
    fn test_aws_value_to_dsl_bare_struct_returns_map() {
        let fields = vec![
            StructField::new("status", AttributeType::String).with_provider_name("Status"),
            StructField::new("mfa_delete", AttributeType::String).with_provider_name("MfaDelete"),
        ];
        let attr_type = AttributeType::Struct {
            name: "VersioningConfiguration".to_string(),
            fields,
        };
        let json_val = serde_json::json!({
            "Status": "Enabled",
        });

        let result = aws_value_to_dsl(
            "versioning_configuration",
            &json_val,
            &attr_type,
            "AWS::S3::Bucket",
        );
        let result = result.expect("Should return Some");

        // Must be Value::Map(...) to match parser output for map assignment syntax
        if let Value::Map(map) = &result {
            assert_eq!(
                map.get("status"),
                Some(&Value::String("Enabled".to_string()))
            );
        } else {
            panic!("Expected Value::Map, got: {:?}", result);
        }
    }

    #[test]
    fn test_dsl_value_to_aws_map_for_bare_struct() {
        let fields = vec![
            StructField::new("status", AttributeType::String).with_provider_name("Status"),
            StructField::new("mfa_delete", AttributeType::String).with_provider_name("MfaDelete"),
        ];
        let attr_type = AttributeType::Struct {
            name: "VersioningConfiguration".to_string(),
            fields,
        };

        // Parser produces Value::Map(...) for map assignment syntax (= { ... })
        let mut map = HashMap::new();
        map.insert("status".to_string(), Value::String("Enabled".to_string()));
        let dsl_value = Value::Map(map);

        let result = dsl_value_to_aws(
            &dsl_value,
            &attr_type,
            "AWS::S3::Bucket",
            "versioning_configuration",
        );
        let result = result.expect("Should return Some");

        // Must produce a JSON object (not array)
        if let serde_json::Value::Object(obj) = &result {
            assert_eq!(obj.get("Status"), Some(&serde_json::json!("Enabled")));
        } else {
            panic!("Expected JSON Object, got: {:?}", result);
        }
    }

    #[test]
    fn test_dsl_value_to_aws_list_for_bare_struct() {
        let fields = vec![
            StructField::new("status", AttributeType::String).with_provider_name("Status"),
            StructField::new("mfa_delete", AttributeType::String).with_provider_name("MfaDelete"),
        ];
        let attr_type = AttributeType::Struct {
            name: "VersioningConfiguration".to_string(),
            fields,
        };

        // Parser produces Value::List(vec![Value::Map(...)]) for block syntax (name { ... })
        let mut map = HashMap::new();
        map.insert("status".to_string(), Value::String("Enabled".to_string()));
        let dsl_value = Value::List(vec![Value::Map(map)]);

        let result = dsl_value_to_aws(
            &dsl_value,
            &attr_type,
            "AWS::S3::Bucket",
            "versioning_configuration",
        );
        let result = result.expect("Should return Some");

        // Must produce a JSON object (not array)
        if let serde_json::Value::Object(obj) = &result {
            assert_eq!(obj.get("Status"), Some(&serde_json::json!("Enabled")));
        } else {
            panic!("Expected JSON Object, got: {:?}", result);
        }
    }

    #[test]
    fn test_bare_struct_roundtrip_no_spurious_diff() {
        let fields =
            vec![StructField::new("status", AttributeType::String).with_provider_name("Status")];
        let attr_type = AttributeType::Struct {
            name: "VersioningConfiguration".to_string(),
            fields,
        };

        // Simulate AWS API response (JSON object)
        let aws_json = serde_json::json!({ "Status": "Enabled" });

        // Read path: convert AWS JSON to DSL value
        let dsl_value = aws_value_to_dsl(
            "versioning_configuration",
            &aws_json,
            &attr_type,
            "AWS::S3::Bucket",
        )
        .expect("read should succeed");

        // Simulate parser output (what the user wrote in .crn with map assignment syntax)
        let mut parser_map = HashMap::new();
        parser_map.insert("status".to_string(), Value::String("Enabled".to_string()));
        let parser_value = Value::Map(parser_map);

        // The read value and parser value must be equal (no spurious diff)
        assert_eq!(
            dsl_value, parser_value,
            "Read value should match parser value — no spurious diff"
        );

        // Write path: convert DSL value back to AWS JSON
        let written_json = dsl_value_to_aws(
            &dsl_value,
            &attr_type,
            "AWS::S3::Bucket",
            "versioning_configuration",
        )
        .expect("write should succeed");

        assert_eq!(
            written_json, aws_json,
            "Round-trip should produce original AWS JSON"
        );
    }

    #[test]
    fn test_vpc_endpoint_type_roundtrip_no_false_diff() {
        // Issue #175: vpc_endpoint_type shows false diff after apply
        // DSL uses bare `Gateway`, AWS returns "Gateway" as string.
        // Both must normalize to the same namespaced value.

        let config = crate::schemas::generated::ec2::vpc_endpoint::ec2_vpc_endpoint_config();
        let attr_schema = config.schema.attributes.get("vpc_endpoint_type").unwrap();

        // 1. DSL side: resolve_enum_identifiers_impl converts bare `Gateway` ident
        let mut resource = Resource::new("ec2.vpc_endpoint", "test");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource
            .attributes
            .insert("vpc_id".to_string(), Value::String("vpc-123".to_string()));
        resource.attributes.insert(
            "vpc_endpoint_type".to_string(),
            Value::UnresolvedIdent("Gateway".to_string(), None),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);

        let dsl_resolved = &resources[0].attributes["vpc_endpoint_type"];
        assert_eq!(
            dsl_resolved,
            &Value::String("awscc.ec2.vpc_endpoint.VpcEndpointType.Gateway".to_string()),
            "DSL bare ident `Gateway` should resolve to namespaced form"
        );

        // 2. AWS read-back side: aws_value_to_dsl converts "Gateway" string
        let aws_json = serde_json::json!("Gateway");
        let aws_dsl = aws_value_to_dsl(
            "vpc_endpoint_type",
            &aws_json,
            &attr_schema.attr_type,
            "ec2.vpc_endpoint",
        )
        .expect("aws_value_to_dsl should return Some");

        assert_eq!(
            aws_dsl,
            Value::String("awscc.ec2.vpc_endpoint.VpcEndpointType.Gateway".to_string()),
            "AWS read-back 'Gateway' should normalize to namespaced form"
        );

        // 3. Both must be equal (no false diff)
        assert_eq!(
            dsl_resolved, &aws_dsl,
            "DSL resolved value and AWS read-back value must match — no false diff"
        );
    }

    #[test]
    fn test_delete_retry_constants() {
        assert_eq!(DELETE_RETRY_MAX_ATTEMPTS, 12);
        assert_eq!(DELETE_RETRY_INITIAL_DELAY_SECS, 10);
        assert_eq!(DELETE_RETRY_MAX_DELAY_SECS, 120);
    }

    #[test]
    fn test_delete_retry_constants_match_create() {
        // Delete retry should use the same strategy as create retry
        assert_eq!(DELETE_RETRY_MAX_ATTEMPTS, CREATE_RETRY_MAX_ATTEMPTS);
        assert_eq!(
            DELETE_RETRY_INITIAL_DELAY_SECS,
            CREATE_RETRY_INITIAL_DELAY_SECS
        );
        assert_eq!(DELETE_RETRY_MAX_DELAY_SECS, CREATE_RETRY_MAX_DELAY_SECS);
    }

    #[test]
    fn test_resolve_enum_identifiers_impl_struct_field() {
        // Test that resolve_enum_identifiers_impl handles struct field enums
        // in ec2_security_group
        let mut resource = Resource::new("ec2.security_group", "test-sg");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "group_description".to_string(),
            Value::String("test".to_string()),
        );
        let mut egress_map = HashMap::new();
        egress_map.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("all".to_string(), None),
        );
        egress_map.insert(
            "cidr_ip".to_string(),
            Value::String("0.0.0.0/0".to_string()),
        );
        resource.attributes.insert(
            "security_group_egress".to_string(),
            Value::List(vec![Value::Map(egress_map)]),
        );

        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);

        // Check that the struct field enum was resolved
        if let Value::List(items) = &resources[0].attributes["security_group_egress"] {
            if let Value::Map(m) = &items[0] {
                match &m["ip_protocol"] {
                    Value::String(s) => {
                        assert_eq!(
                            s, "awscc.ec2.security_group.IpProtocol.all",
                            "Expected namespaced IpProtocol.all in struct field, got: {}",
                            s
                        );
                    }
                    other => panic!("Expected String for ip_protocol, got: {:?}", other),
                }
                // Non-enum field should be unchanged
                match &m["cidr_ip"] {
                    Value::String(s) => assert_eq!(s, "0.0.0.0/0"),
                    other => panic!("Expected String for cidr_ip, got: {:?}", other),
                }
            } else {
                panic!("Expected Map in egress list");
            }
        } else {
            panic!("Expected List for security_group_egress");
        }
    }

    #[test]
    fn test_dsl_value_to_aws_preserves_underscores_in_enum_values() {
        // Enum values like INFREQUENT_ACCESS should NOT have underscores
        // converted to hyphens (issue #516)
        let attr_type = AttributeType::StringEnum {
            name: "LogGroupClass".to_string(),
            values: vec![
                "STANDARD".to_string(),
                "INFREQUENT_ACCESS".to_string(),
                "DELIVERY".to_string(),
            ],
            namespace: Some("awscc.logs.log_group".to_string()),
            to_dsl: None,
        };
        let value =
            Value::String("awscc.logs.log_group.LogGroupClass.INFREQUENT_ACCESS".to_string());
        let result = dsl_value_to_aws(&value, &attr_type, "logs.log_group", "log_group_class");
        assert_eq!(result, Some(json!("INFREQUENT_ACCESS")));
    }

    #[test]
    fn test_dsl_value_to_aws_converts_underscores_for_region() {
        // Region values like ap_northeast_1 SHOULD have underscores converted
        // to hyphens since Region is a Custom type without valid values list
        let attr_type = AttributeType::Custom {
            name: "Region".to_string(),
            base: Box::new(AttributeType::String),
            validate: |_| Ok(()),
            namespace: Some("awscc".to_string()),
            to_dsl: None,
        };
        let value = Value::String("awscc.Region.ap_northeast_1".to_string());
        let result = dsl_value_to_aws(&value, &attr_type, "logs.log_group", "region");
        assert_eq!(result, Some(json!("ap-northeast-1")));
    }

    #[test]
    fn test_dsl_value_to_aws_list_string_enum() {
        // List(StringEnum) items should have enum conversion applied
        let inner = AttributeType::StringEnum {
            name: "AllowedMethod".to_string(),
            values: vec!["GET".to_string(), "PUT".to_string(), "DELETE".to_string()],
            namespace: Some("awscc.s3.bucket".to_string()),
            to_dsl: None,
        };
        let attr_type = AttributeType::List(Box::new(inner));
        let value = Value::List(vec![
            Value::String("awscc.s3.bucket.AllowedMethod.GET".to_string()),
            Value::String("awscc.s3.bucket.AllowedMethod.PUT".to_string()),
        ]);
        let result = dsl_value_to_aws(&value, &attr_type, "s3.bucket", "allowed_methods");
        assert_eq!(result, Some(json!(["GET", "PUT"])));
    }

    #[test]
    fn test_aws_value_to_dsl_list_string_enum() {
        // List(StringEnum) read-back should namespace each item
        let inner = AttributeType::StringEnum {
            name: "AllowedMethod".to_string(),
            values: vec!["GET".to_string(), "PUT".to_string(), "DELETE".to_string()],
            namespace: Some("awscc.s3.bucket".to_string()),
            to_dsl: None,
        };
        let attr_type = AttributeType::List(Box::new(inner));
        let json_val = json!(["GET", "PUT"]);
        let result = aws_value_to_dsl("allowed_methods", &json_val, &attr_type, "s3.bucket");
        assert_eq!(
            result,
            Some(Value::List(vec![
                Value::String("awscc.s3.bucket.AllowedMethod.GET".to_string()),
                Value::String("awscc.s3.bucket.AllowedMethod.PUT".to_string()),
            ]))
        );
    }

    #[test]
    fn test_dsl_value_to_aws_list_string_enum_roundtrip() {
        // Verify List(StringEnum) round-trips correctly
        let inner = AttributeType::StringEnum {
            name: "AllowedMethod".to_string(),
            values: vec!["GET".to_string(), "PUT".to_string()],
            namespace: Some("awscc.s3.bucket".to_string()),
            to_dsl: None,
        };
        let attr_type = AttributeType::List(Box::new(inner));

        let aws_json = json!(["GET", "PUT"]);
        let dsl = aws_value_to_dsl("allowed_methods", &aws_json, &attr_type, "s3.bucket")
            .expect("read should succeed");
        let written = dsl_value_to_aws(&dsl, &attr_type, "s3.bucket", "allowed_methods")
            .expect("write should succeed");
        assert_eq!(written, aws_json, "Round-trip should produce original JSON");
    }

    #[test]
    fn test_dsl_value_to_aws_union_with_string_enum() {
        // Union member that is a namespaced StringEnum should be converted.
        // More specific types (StringEnum) should come before generic ones (String).
        let attr_type = AttributeType::Union(vec![
            AttributeType::StringEnum {
                name: "Protocol".to_string(),
                values: vec!["tcp".to_string(), "udp".to_string()],
                namespace: Some("awscc.ec2.sg".to_string()),
                to_dsl: None,
            },
            AttributeType::String,
        ]);
        let value = Value::String("awscc.ec2.sg.Protocol.tcp".to_string());
        let result = dsl_value_to_aws(&value, &attr_type, "ec2.sg", "protocol");
        assert_eq!(result, Some(json!("tcp")));
    }

    #[test]
    fn test_dsl_value_to_aws_map_preserves_user_keys() {
        // Map(String) should preserve user-defined keys as-is, not PascalCase them
        let attr_type = AttributeType::Map(Box::new(AttributeType::String));

        let mut map = HashMap::new();
        map.insert(
            "my_custom_key".to_string(),
            Value::String("value1".to_string()),
        );
        map.insert(
            "another-key".to_string(),
            Value::String("value2".to_string()),
        );
        let dsl_value = Value::Map(map);

        let result = dsl_value_to_aws(&dsl_value, &attr_type, "s3.bucket", "tags");
        let result = result.expect("Should return Some");

        if let serde_json::Value::Object(obj) = &result {
            // Keys must be preserved exactly as written, not PascalCased
            assert_eq!(obj.get("my_custom_key"), Some(&json!("value1")));
            assert_eq!(obj.get("another-key"), Some(&json!("value2")));
            // Verify PascalCased versions do NOT exist
            assert!(obj.get("MyCustomKey").is_none());
            assert!(obj.get("AnotherKey").is_none());
        } else {
            panic!("Expected JSON Object, got: {:?}", result);
        }
    }

    #[test]
    fn test_dsl_value_to_aws_map_recurses_into_values() {
        // Map with enum values should recurse for type-aware conversion
        let inner_type = AttributeType::StringEnum {
            name: "Status".to_string(),
            values: vec!["Active".to_string(), "Inactive".to_string()],
            namespace: Some("awscc.test.resource".to_string()),
            to_dsl: None,
        };
        let attr_type = AttributeType::Map(Box::new(inner_type));

        let mut map = HashMap::new();
        map.insert(
            "item_one".to_string(),
            Value::String("awscc.test.resource.Status.Active".to_string()),
        );
        let dsl_value = Value::Map(map);

        let result = dsl_value_to_aws(&dsl_value, &attr_type, "test.resource", "status_map");
        let result = result.expect("Should return Some");

        if let serde_json::Value::Object(obj) = &result {
            // Key preserved, value converted from namespaced enum to raw value
            assert_eq!(obj.get("item_one"), Some(&json!("Active")));
        } else {
            panic!("Expected JSON Object, got: {:?}", result);
        }
    }

    #[test]
    fn test_aws_value_to_dsl_map_preserves_user_keys() {
        // Map(String) read path should preserve keys as-is, not snake_case them
        let attr_type = AttributeType::Map(Box::new(AttributeType::String));

        let aws_json = json!({
            "MyCustomKey": "value1",
            "another-key": "value2"
        });

        let result = aws_value_to_dsl("tags", &aws_json, &attr_type, "s3.bucket");
        let result = result.expect("Should return Some");

        if let Value::Map(map) = &result {
            assert_eq!(
                map.get("MyCustomKey"),
                Some(&Value::String("value1".to_string()))
            );
            assert_eq!(
                map.get("another-key"),
                Some(&Value::String("value2".to_string()))
            );
            // Verify snake_cased versions do NOT exist
            assert!(map.get("my_custom_key").is_none());
        } else {
            panic!("Expected Value::Map, got: {:?}", result);
        }
    }

    #[test]
    fn test_aws_value_to_dsl_union_with_string_enum() {
        // Union: read-back should try members and pick the namespaced one
        let attr_type = AttributeType::Union(vec![
            AttributeType::StringEnum {
                name: "Protocol".to_string(),
                values: vec!["tcp".to_string(), "udp".to_string()],
                namespace: Some("awscc.ec2.sg".to_string()),
                to_dsl: None,
            },
            AttributeType::String,
        ]);
        let json_val = json!("tcp");
        let result = aws_value_to_dsl("protocol", &json_val, &attr_type, "ec2.sg");
        assert_eq!(
            result,
            Some(Value::String("awscc.ec2.sg.Protocol.tcp".to_string()))
        );
    }

    #[test]
    fn test_aws_value_to_dsl_union_fallback() {
        // Union: when no member produces type-aware result, fall back to generic
        let attr_type = AttributeType::Union(vec![
            AttributeType::StringEnum {
                name: "Protocol".to_string(),
                values: vec!["tcp".to_string(), "udp".to_string()],
                namespace: Some("awscc.ec2.sg".to_string()),
                to_dsl: None,
            },
            AttributeType::Int,
        ]);
        let json_val = json!(42);
        let result = aws_value_to_dsl("protocol", &json_val, &attr_type, "ec2.sg");
        assert_eq!(result, Some(Value::Int(42)));
    }

    #[test]
    fn test_dsl_value_to_aws_iam_policy_document_uses_pascal_case() {
        // IAM policy documents must use PascalCase keys (Version, Statement, Effect, etc.)
        // when sent to AWS. The DSL uses snake_case (version, statement, effect, etc.).
        use carina_aws_types::iam_policy_document;

        let attr_type = iam_policy_document();
        let value = Value::Map(
            vec![
                (
                    "version".to_string(),
                    Value::String("2012-10-17".to_string()),
                ),
                (
                    "statement".to_string(),
                    Value::List(vec![Value::Map(
                        vec![
                            ("effect".to_string(), Value::String("Allow".to_string())),
                            (
                                "action".to_string(),
                                Value::String("sts:AssumeRole".to_string()),
                            ),
                            (
                                "principal".to_string(),
                                Value::Map(
                                    vec![(
                                        "service".to_string(),
                                        Value::String("lambda.amazonaws.com".to_string()),
                                    )]
                                    .into_iter()
                                    .collect(),
                                ),
                            ),
                        ]
                        .into_iter()
                        .collect(),
                    )]),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let result = dsl_value_to_aws(
            &value,
            &attr_type,
            "iam.role",
            "assume_role_policy_document",
        );
        let result = result.expect("Should return Some");

        // Top-level keys must be PascalCase
        let obj = result.as_object().expect("Expected JSON Object");
        assert_eq!(obj.get("Version"), Some(&json!("2012-10-17")));
        assert!(
            obj.get("version").is_none(),
            "snake_case 'version' should not exist"
        );

        // Statement array
        let statements = obj.get("Statement").expect("Should have Statement");
        assert!(
            obj.get("statement").is_none(),
            "snake_case 'statement' should not exist"
        );
        let stmt = statements.as_array().unwrap().first().unwrap();
        let stmt_obj = stmt.as_object().unwrap();

        // Statement fields must be PascalCase
        assert_eq!(stmt_obj.get("Effect"), Some(&json!("Allow")));
        assert!(stmt_obj.get("effect").is_none());
        assert_eq!(stmt_obj.get("Action"), Some(&json!("sts:AssumeRole")));
        assert!(stmt_obj.get("action").is_none());

        // Principal nested map must have PascalCase keys
        let principal = stmt_obj.get("Principal").expect("Should have Principal");
        assert!(stmt_obj.get("principal").is_none());
        let principal_obj = principal.as_object().unwrap();
        assert_eq!(
            principal_obj.get("Service"),
            Some(&json!("lambda.amazonaws.com"))
        );
        assert!(principal_obj.get("service").is_none());
    }

    #[test]
    fn test_aws_value_to_dsl_iam_policy_document_uses_snake_case() {
        // Reading IAM policy documents from AWS should convert PascalCase to snake_case
        use carina_aws_types::iam_policy_document;

        let attr_type = iam_policy_document();
        let aws_json = json!({
            "Version": "2012-10-17",
            "Statement": [{
                "Effect": "Allow",
                "Action": "sts:AssumeRole",
                "Principal": {
                    "Service": "lambda.amazonaws.com"
                }
            }]
        });

        let result = aws_value_to_dsl(
            "assume_role_policy_document",
            &aws_json,
            &attr_type,
            "iam.role",
        );
        let result = result.expect("Should return Some");

        if let Value::Map(map) = &result {
            assert_eq!(
                map.get("version"),
                Some(&Value::String("2012-10-17".to_string()))
            );
            assert!(
                map.get("Version").is_none(),
                "PascalCase 'Version' should not exist"
            );

            if let Some(Value::List(stmts)) = map.get("statement") {
                if let Some(Value::Map(stmt)) = stmts.first() {
                    assert_eq!(
                        stmt.get("effect"),
                        Some(&Value::String("Allow".to_string()))
                    );
                    assert_eq!(
                        stmt.get("action"),
                        Some(&Value::String("sts:AssumeRole".to_string()))
                    );
                    if let Some(Value::Map(principal)) = stmt.get("principal") {
                        assert_eq!(
                            principal.get("service"),
                            Some(&Value::String("lambda.amazonaws.com".to_string()))
                        );
                    } else {
                        panic!("Expected principal to be a Map");
                    }
                } else {
                    panic!("Expected statement to contain a Map");
                }
            } else {
                panic!("Expected statement to be a List");
            }
        } else {
            panic!("Expected Value::Map, got: {:?}", result);
        }
    }

    #[test]
    fn test_aws_value_to_dsl_region_in_struct_uses_underscores() {
        // Region values inside struct fields should use underscore format (ap_northeast_1),
        // not hyphen format (ap-northeast-1), to match DSL conventions.
        // This ensures idempotency for resources like ec2.ipam operating_regions.
        use crate::schemas::awscc_types::awscc_region;

        let fields = vec![
            StructField::new("region_name", awscc_region())
                .required()
                .with_provider_name("RegionName"),
        ];
        let attr_type = AttributeType::List(Box::new(AttributeType::Struct {
            name: "IpamOperatingRegion".to_string(),
            fields,
        }));
        let json_val = json!([{"RegionName": "ap-northeast-1"}]);

        let result = aws_value_to_dsl("operating_regions", &json_val, &attr_type, "ec2.ipam");
        let expected = Value::List(vec![Value::Map(HashMap::from([(
            "region_name".to_string(),
            Value::String("awscc.Region.ap_northeast_1".to_string()),
        )]))]);
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_aws_value_to_dsl_enum_value_with_dot() {
        // When an enum value itself contains a dot (e.g., "ipsec.1" for VPN Gateway type),
        // aws_value_to_dsl should produce a valid namespaced identifier.
        let attr_type = AttributeType::StringEnum {
            name: "Type".to_string(),
            values: vec!["ipsec.1".to_string()],
            namespace: Some("awscc.ec2.vpn_gateway".to_string()),
            to_dsl: None,
        };
        let json_val = json!("ipsec.1");

        let result = aws_value_to_dsl("type", &json_val, &attr_type, "ec2.vpn_gateway");
        assert_eq!(
            result,
            Some(Value::String(
                "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
            ))
        );
    }

    #[test]
    fn test_dsl_value_to_aws_enum_value_with_dot() {
        // When given a namespaced identifier with a dotted enum value,
        // dsl_value_to_aws should correctly extract the value using known valid values.
        let attr_type = AttributeType::StringEnum {
            name: "Type".to_string(),
            values: vec!["ipsec.1".to_string()],
            namespace: Some("awscc.ec2.vpn_gateway".to_string()),
            to_dsl: None,
        };
        let value = Value::String("awscc.ec2.vpn_gateway.Type.ipsec.1".to_string());

        let result = dsl_value_to_aws(&value, &attr_type, "ec2.vpn_gateway", "type");
        assert_eq!(result, Some(json!("ipsec.1")));
    }

    #[test]
    fn test_dsl_value_to_aws_enum_plain_dot_value() {
        // Plain string "ipsec.1" (not namespaced) should also work
        let attr_type = AttributeType::StringEnum {
            name: "Type".to_string(),
            values: vec!["ipsec.1".to_string()],
            namespace: Some("awscc.ec2.vpn_gateway".to_string()),
            to_dsl: None,
        };
        let value = Value::String("ipsec.1".to_string());

        let result = dsl_value_to_aws(&value, &attr_type, "ec2.vpn_gateway", "type");
        assert_eq!(result, Some(json!("ipsec.1")));
    }

    #[test]
    fn test_enum_round_trip_with_dotted_value() {
        // Full round-trip: AWS value -> DSL -> back to AWS
        let attr_type = AttributeType::StringEnum {
            name: "Type".to_string(),
            values: vec!["ipsec.1".to_string()],
            namespace: Some("awscc.ec2.vpn_gateway".to_string()),
            to_dsl: None,
        };

        // AWS -> DSL (read path)
        let aws_val = json!("ipsec.1");
        let dsl_val = aws_value_to_dsl("type", &aws_val, &attr_type, "ec2.vpn_gateway");
        assert_eq!(
            dsl_val,
            Some(Value::String(
                "awscc.ec2.vpn_gateway.Type.ipsec.1".to_string()
            ))
        );

        // DSL -> AWS (write path)
        let back_to_aws =
            dsl_value_to_aws(&dsl_val.unwrap(), &attr_type, "ec2.vpn_gateway", "type");
        assert_eq!(back_to_aws, Some(json!("ipsec.1")));
    }

    // =========================================================================
    // Retry logging tests (verify log framework is used instead of eprintln!)
    // =========================================================================

    #[test]
    fn test_retry_logging_uses_log_framework() {
        // Verify that retry messages go through the log framework.
        // The source code should use log::warn! instead of eprintln!.
        // Count lines that START with eprintln (after trimming), which are actual macro calls,
        // excluding this test code itself.
        let source = include_str!("provider.rs");
        let actual_eprintln_calls = source
            .lines()
            .filter(|line| line.trim().starts_with("eprintln!"))
            .count();
        assert_eq!(
            actual_eprintln_calls, 0,
            "Found {} eprintln! macro calls in provider.rs; \
             all should be replaced with log::warn!",
            actual_eprintln_calls
        );
    }

    // =========================================================================
    // value_to_json tests
    // =========================================================================

    #[test]
    fn test_value_to_json_nan_returns_none() {
        let value = Value::Float(f64::NAN);
        assert_eq!(value_to_json(&value), None);
    }

    #[test]
    fn test_value_to_json_infinity_returns_none() {
        let value = Value::Float(f64::INFINITY);
        assert_eq!(value_to_json(&value), None);
    }

    #[test]
    fn test_value_to_json_neg_infinity_returns_none() {
        let value = Value::Float(f64::NEG_INFINITY);
        assert_eq!(value_to_json(&value), None);
    }

    #[test]
    fn test_value_to_json_finite_float() {
        let value = Value::Float(1.5);
        let result = value_to_json(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), serde_json::json!(1.5));
    }

    // =========================================================================
    // Silent array/map drop warning tests
    // =========================================================================

    #[test]
    fn test_json_to_value_array_with_null_logs_warning() {
        // json_to_value should warn when array items are dropped (e.g., null values)
        // Verify the source code contains log::warn! in json_to_value's Array branch
        let source = include_str!("provider.rs");

        // Find the json_to_value function and check it warns on dropped array items
        let in_json_to_value = source
            .split("fn json_to_value")
            .nth(1)
            .expect("json_to_value function not found");
        let fn_body = &in_json_to_value[..in_json_to_value
            .find("\nfn ")
            .unwrap_or(in_json_to_value.len())];

        assert!(
            fn_body.contains("log::warn!"),
            "json_to_value should log a warning when array items or map entries are dropped"
        );
    }

    #[test]
    fn test_aws_value_to_dsl_list_logs_warning_on_dropped_items() {
        // aws_value_to_dsl should warn when List items are dropped
        let source = include_str!("provider.rs");

        let in_fn = source
            .split("fn aws_value_to_dsl")
            .nth(1)
            .expect("aws_value_to_dsl function not found");
        let fn_body = &in_fn[..in_fn.find("\nfn ").unwrap_or(in_fn.len())];

        assert!(
            fn_body.contains("log::warn!"),
            "aws_value_to_dsl should log a warning when list items or map entries are dropped"
        );
    }

    #[test]
    fn test_dsl_value_to_aws_list_logs_warning_on_dropped_items() {
        // dsl_value_to_aws should warn when List items are dropped
        let source = include_str!("provider.rs");

        let in_fn = source
            .split("fn dsl_value_to_aws")
            .nth(1)
            .expect("dsl_value_to_aws function not found");
        let fn_body = &in_fn[..in_fn.find("\nfn ").unwrap_or(in_fn.len())];

        assert!(
            fn_body.contains("log::warn!"),
            "dsl_value_to_aws should log a warning when list items or map entries are dropped"
        );
    }

    #[test]
    fn test_value_to_json_list_logs_warning_on_dropped_items() {
        // value_to_json should warn when List items are dropped
        let source = include_str!("provider.rs");

        let in_fn = source
            .split("fn value_to_json")
            .nth(1)
            .expect("value_to_json function not found");
        let fn_body = &in_fn[..in_fn.find("\nfn ").unwrap_or(in_fn.len())];

        assert!(
            fn_body.contains("log::warn!"),
            "value_to_json should log a warning when list items or map entries are dropped"
        );
    }

    // =========================================================================
    // Behavioral tests for array/map value conversion with unconvertible items
    // =========================================================================

    #[test]
    fn test_json_to_value_array_with_null_drops_null_items() {
        // JSON null cannot be represented as a DSL Value, so it is dropped.
        // This test documents the behavior and ensures warnings are logged.
        let json = serde_json::json!(["a", null, "b"]);
        let result = json_to_value(&json);
        let expected = Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]);
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_json_to_value_map_with_null_value_drops_entry() {
        let json = serde_json::json!({"key1": "val1", "key2": null});
        let result = json_to_value(&json);
        match result {
            Some(Value::Map(map)) => {
                assert_eq!(map.len(), 1);
                assert_eq!(map.get("key1"), Some(&Value::String("val1".to_string())));
                assert!(!map.contains_key("key2"));
            }
            other => panic!("Expected Some(Value::Map), got {:?}", other),
        }
    }

    #[test]
    fn test_aws_value_to_dsl_list_with_null_drops_null_items() {
        let json = serde_json::json!(["a", null, "b"]);
        let attr_type = AttributeType::List(Box::new(AttributeType::String));
        let result = aws_value_to_dsl("test_attr", &json, &attr_type, "test.resource");
        let expected = Value::List(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
        ]);
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_value_to_json_list_with_nan_drops_nan_items() {
        // NaN floats cannot be represented in JSON, so they are dropped.
        let value = Value::List(vec![
            Value::Float(1.0),
            Value::Float(f64::NAN),
            Value::Float(2.0),
        ]);
        let result = value_to_json(&value);
        let expected = serde_json::json!([1.0, 2.0]);
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_dsl_value_to_aws_list_with_nan_drops_nan_items() {
        let value = Value::List(vec![
            Value::Float(1.0),
            Value::Float(f64::NAN),
            Value::Float(2.0),
        ]);
        let attr_type = AttributeType::List(Box::new(AttributeType::Float));
        let result = dsl_value_to_aws(&value, &attr_type, "test.resource", "test_attr");
        let expected = serde_json::json!([1.0, 2.0]);
        assert_eq!(result, Some(expected));
    }

    // =========================================================================
    // parse_resource_properties tests
    // =========================================================================

    #[test]
    fn test_parse_resource_properties_valid_json() {
        let json_str = r#"{"VpcId": "vpc-123", "CidrBlock": "10.0.0.0/16"}"#;
        let result = parse_resource_properties(json_str);
        assert!(result.is_ok());
        let value = result.unwrap();
        assert_eq!(value["VpcId"], "vpc-123");
        assert_eq!(value["CidrBlock"], "10.0.0.0/16");
    }

    #[test]
    fn test_parse_resource_properties_malformed_json_returns_error() {
        let malformed = r#"{"VpcId": "vpc-123", invalid"#;
        let result = parse_resource_properties(malformed);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.message.contains("Failed to parse resource properties"),
            "Expected error message about parsing, got: {}",
            err.message
        );
    }

    #[test]
    fn test_parse_resource_properties_empty_string_returns_error() {
        let result = parse_resource_properties("");
        assert!(result.is_err());
    }

    // =========================================================================
    // build_update_patches tests
    // =========================================================================

    /// Helper to get an AwsccSchemaConfig for ec2.vpc (a real resource type with tags)
    fn get_vpc_config() -> AwsccSchemaConfig {
        get_schema_config("ec2.vpc").expect("ec2.vpc schema should exist")
    }

    #[test]
    fn test_build_update_patches_remove_attribute_absent_in_to() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        // from state has instance_tenancy
        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        from_attrs.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.default".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        // to resource only has cidr_block, instance_tenancy removed
        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        // Should have a replace for cidr_block and a remove for instance_tenancy
        let has_remove_instance_tenancy = patches.iter().any(|p| {
            p.get("op").and_then(|v| v.as_str()) == Some("remove")
                && p.get("path").and_then(|v| v.as_str()) == Some("/InstanceTenancy")
        });
        assert!(
            has_remove_instance_tenancy,
            "Expected remove patch for /InstanceTenancy, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_remove_tags_absent_in_to() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        // from state has tags
        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let mut tags = HashMap::new();
        tags.insert("Name".to_string(), Value::String("my-vpc".to_string()));
        from_attrs.insert("tags".to_string(), Value::Map(tags));
        let from = State::existing(id.clone(), from_attrs);

        // to resource has no tags
        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        let has_remove_tags = patches.iter().any(|p| {
            p.get("op").and_then(|v| v.as_str()) == Some("remove")
                && p.get("path").and_then(|v| v.as_str()) == Some("/Tags")
        });
        assert!(
            has_remove_tags,
            "Expected remove patch for /Tags, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_no_remove_for_required_attribute() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        // from state has cidr_block (which is required)
        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        // to resource has no cidr_block (shouldn't happen in practice, but test the guard)
        let to = Resource::new("ec2.vpc", "test");

        let patches = build_update_patches(&config, &from, &to);

        // Should NOT have a remove for cidr_block since it's required
        let has_remove_cidr = patches.iter().any(|p| {
            p.get("op").and_then(|v| v.as_str()) == Some("remove")
                && p.get("path").and_then(|v| v.as_str()) == Some("/CidrBlock")
        });
        assert!(
            !has_remove_cidr,
            "Should not remove required attribute CidrBlock, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_replace_only_when_both_present() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        // Both from and to have the same attributes
        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        from_attrs.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.default".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        to.attributes.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.dedicated".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        // Should only have replace operations, no remove
        let has_remove = patches
            .iter()
            .any(|p| p.get("op").and_then(|v| v.as_str()) == Some("remove"));
        assert!(
            !has_remove,
            "Should not have remove operations when attribute is present in both from and to, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_skip_unchanged_attributes() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        // from and to have identical values for cidr_block, different for instance_tenancy
        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        from_attrs.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.default".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        to.attributes.insert(
            "instance_tenancy".to_string(),
            Value::String("awscc.ec2.vpc.InstanceTenancy.dedicated".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        // cidr_block is unchanged, so no patch should be generated for it
        let has_cidr_replace = patches.iter().any(|p| {
            p.get("op").and_then(|v| v.as_str()) == Some("replace")
                && p.get("path").and_then(|v| v.as_str()) == Some("/CidrBlock")
        });
        assert!(
            !has_cidr_replace,
            "Should not generate replace patch for unchanged attribute /CidrBlock, got: {:?}",
            patches
        );

        // instance_tenancy changed, so a replace patch should be generated
        let has_tenancy_replace = patches.iter().any(|p| {
            p.get("op").and_then(|v| v.as_str()) == Some("replace")
                && p.get("path").and_then(|v| v.as_str()) == Some("/InstanceTenancy")
        });
        assert!(
            has_tenancy_replace,
            "Should generate replace patch for changed attribute /InstanceTenancy, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_no_patches_when_identical() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        // from and to are completely identical
        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );

        let patches = build_update_patches(&config, &from, &to);

        assert!(
            patches.is_empty(),
            "Should generate no patches when from and to are identical, got: {:?}",
            patches
        );
    }

    #[test]
    fn test_build_update_patches_skip_unchanged_tags() {
        let config = get_vpc_config();
        let id = ResourceId::with_provider("awscc", "ec2.vpc", "test");

        // from and to have identical tags
        let mut from_attrs = HashMap::new();
        from_attrs.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        let mut tags = HashMap::new();
        tags.insert("Name".to_string(), Value::String("my-vpc".to_string()));
        from_attrs.insert("tags".to_string(), Value::Map(tags.clone()));
        let from = State::existing(id.clone(), from_attrs);

        let mut to = Resource::new("ec2.vpc", "test");
        to.attributes.insert(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        );
        to.attributes.insert("tags".to_string(), Value::Map(tags));

        let patches = build_update_patches(&config, &from, &to);

        // No patches should be generated since everything is identical
        assert!(
            patches.is_empty(),
            "Should generate no patches when tags are unchanged, got: {:?}",
            patches
        );
    }
}
