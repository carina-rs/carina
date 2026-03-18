//! Resource-type-specific special case handlers.
//!
//! Some AWS resource types require non-standard attribute handling that falls
//! outside the generic schema-driven mapping. This module centralizes those
//! special cases for read, create, and delete operations.

use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, Value};
use serde_json::json;

use super::AwsccProvider;
use crate::schemas::generated::AwsccSchemaConfig;

impl AwsccProvider {
    /// Handle special attributes that don't follow standard mapping
    pub(crate) fn read_special_attributes(
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
    pub(crate) fn create_special_attributes(
        &self,
        _resource: &Resource,
        _desired_state: &mut serde_json::Map<String, serde_json::Value>,
    ) {
    }

    /// Set default values for create
    pub(crate) fn set_default_values(
        &self,
        resource_type: &str,
        desired_state: &mut serde_json::Map<String, serde_json::Value>,
    ) {
        if resource_type == "ec2.eip" && !desired_state.contains_key("Domain") {
            desired_state.insert("Domain".to_string(), json!("vpc"));
        }
    }

    /// Handle pre-delete operations (e.g., detach IGW from VPC)
    pub(crate) async fn pre_delete_operations(
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
}
