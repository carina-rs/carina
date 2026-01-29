//! Carina AWS Cloud Control Provider
//!
//! AWS Cloud Control API Provider implementation

pub mod schemas;

use std::collections::HashMap;
use std::time::Duration;

use aws_config::Region;
use aws_sdk_cloudcontrol::Client as CloudControlClient;
use aws_sdk_cloudcontrol::types::OperationStatus;
use aws_sdk_ec2::Client as Ec2Client;
use carina_core::provider::{
    BoxFuture, Provider, ProviderError, ProviderResult, ResourceSchema, ResourceType,
};
use carina_core::resource::{Resource, ResourceId, State, Value};
use serde_json::json;

/// VPC resource type for Cloud Control
pub struct VpcType;

impl ResourceType for VpcType {
    fn name(&self) -> &'static str {
        "vpc"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// AWS Cloud Control Provider
pub struct AwsccProvider {
    cloudcontrol_client: CloudControlClient,
    ec2_client: Ec2Client,
    region: String,
}

impl AwsccProvider {
    /// Create a new AWS Cloud Control Provider
    pub async fn new(region: &str) -> Self {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(Region::new(region.to_string()))
            .load()
            .await;

        Self {
            cloudcontrol_client: CloudControlClient::new(&config),
            ec2_client: Ec2Client::new(&config),
            region: region.to_string(),
        }
    }

    /// Wait for a Cloud Control operation to complete
    async fn wait_for_operation(&self, request_token: &str) -> ProviderResult<String> {
        let max_attempts = 60;
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
                        // Still in progress, wait and retry
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        Err(ProviderError::new("Operation timed out"))
    }

    /// Find VPC ID by Name tag using EC2 API
    async fn find_vpc_id_by_name(&self, name: &str) -> ProviderResult<Option<String>> {
        use aws_sdk_ec2::types::Filter;

        let filter = Filter::builder().name("tag:Name").values(name).build();

        let result = self
            .ec2_client
            .describe_vpcs()
            .filters(filter)
            .send()
            .await
            .map_err(|e| ProviderError::new(format!("Failed to describe VPCs: {:?}", e)))?;

        Ok(result
            .vpcs()
            .first()
            .and_then(|vpc| vpc.vpc_id().map(String::from)))
    }

    /// Read a VPC using Cloud Control API
    async fn read_vpc(&self, name: &str) -> ProviderResult<State> {
        let id = ResourceId::new("vpc", name);

        // First, find the VPC ID by Name tag using EC2 API
        let vpc_id = match self.find_vpc_id_by_name(name).await? {
            Some(vpc_id) => vpc_id,
            None => return Ok(State::not_found(id)),
        };

        // Then read using Cloud Control API
        let result = self
            .cloudcontrol_client
            .get_resource()
            .type_name("AWS::EC2::VPC")
            .identifier(&vpc_id)
            .send()
            .await;

        match result {
            Ok(response) => {
                if let Some(desc) = response.resource_description()
                    && let Some(props_str) = desc.properties()
                {
                    let props: serde_json::Value =
                        serde_json::from_str(props_str).unwrap_or_default();

                    let mut attributes = HashMap::new();

                    // Carina-specific attributes
                    attributes.insert("name".to_string(), Value::String(name.to_string()));
                    let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
                    attributes.insert("region".to_string(), Value::String(region_dsl));

                    // CloudFormation input properties
                    if let Some(cidr) = props.get("CidrBlock").and_then(|v| v.as_str()) {
                        attributes
                            .insert("cidr_block".to_string(), Value::String(cidr.to_string()));
                    }

                    if let Some(dns_hostnames) =
                        props.get("EnableDnsHostnames").and_then(|v| v.as_bool())
                    {
                        attributes.insert(
                            "enable_dns_hostnames".to_string(),
                            Value::Bool(dns_hostnames),
                        );
                    }

                    if let Some(dns_support) =
                        props.get("EnableDnsSupport").and_then(|v| v.as_bool())
                    {
                        attributes
                            .insert("enable_dns_support".to_string(), Value::Bool(dns_support));
                    }

                    if let Some(tenancy) = props.get("InstanceTenancy").and_then(|v| v.as_str()) {
                        // Convert to DSL format: "dedicated" → "awscc.vpc.InstanceTenancy.dedicated"
                        let tenancy_dsl = format!("awscc.vpc.InstanceTenancy.{}", tenancy);
                        attributes
                            .insert("instance_tenancy".to_string(), Value::String(tenancy_dsl));
                    }

                    if let Some(ipam_pool_id) = props.get("Ipv4IpamPoolId").and_then(|v| v.as_str())
                    {
                        attributes.insert(
                            "ipv4_ipam_pool_id".to_string(),
                            Value::String(ipam_pool_id.to_string()),
                        );
                    }

                    if let Some(netmask_length) =
                        props.get("Ipv4NetmaskLength").and_then(|v| v.as_i64())
                    {
                        attributes.insert(
                            "ipv4_netmask_length".to_string(),
                            Value::Int(netmask_length),
                        );
                    }

                    // Parse Tags (convert CloudFormation format to Terraform-style map)
                    if let Some(tags_array) = props.get("Tags").and_then(|v| v.as_array()) {
                        let mut tags_map = HashMap::new();
                        for tag in tags_array {
                            if let (Some(key), Some(value)) = (
                                tag.get("Key").and_then(|v| v.as_str()),
                                tag.get("Value").and_then(|v| v.as_str()),
                            ) {
                                // Skip the Name tag (handled separately)
                                if key != "Name" {
                                    tags_map
                                        .insert(key.to_string(), Value::String(value.to_string()));
                                }
                            }
                        }
                        if !tags_map.is_empty() {
                            attributes.insert("tags".to_string(), Value::Map(tags_map));
                        }
                    }

                    // CloudFormation return values (read-only)
                    if let Some(vpc_id_val) = props.get("VpcId").and_then(|v| v.as_str()) {
                        attributes
                            .insert("vpc_id".to_string(), Value::String(vpc_id_val.to_string()));
                    }

                    if let Some(associations) = props
                        .get("CidrBlockAssociations")
                        .and_then(|v| v.as_array())
                    {
                        let assoc_list: Vec<Value> = associations
                            .iter()
                            .filter_map(|v| v.as_str().map(|s| Value::String(s.to_string())))
                            .collect();
                        if !assoc_list.is_empty() {
                            attributes.insert(
                                "cidr_block_associations".to_string(),
                                Value::List(assoc_list),
                            );
                        }
                    }

                    if let Some(default_nacl) =
                        props.get("DefaultNetworkAcl").and_then(|v| v.as_str())
                    {
                        attributes.insert(
                            "default_network_acl".to_string(),
                            Value::String(default_nacl.to_string()),
                        );
                    }

                    if let Some(default_sg) =
                        props.get("DefaultSecurityGroup").and_then(|v| v.as_str())
                    {
                        attributes.insert(
                            "default_security_group".to_string(),
                            Value::String(default_sg.to_string()),
                        );
                    }

                    if let Some(ipv6_cidrs) = props.get("Ipv6CidrBlocks").and_then(|v| v.as_array())
                    {
                        let ipv6_list: Vec<Value> = ipv6_cidrs
                            .iter()
                            .filter_map(|v| v.as_str().map(|s| Value::String(s.to_string())))
                            .collect();
                        if !ipv6_list.is_empty() {
                            attributes
                                .insert("ipv6_cidr_blocks".to_string(), Value::List(ipv6_list));
                        }
                    }

                    // Fallback: Get DNS settings from EC2 API if not in Cloud Control response
                    if !attributes.contains_key("enable_dns_support")
                        || !attributes.contains_key("enable_dns_hostnames")
                    {
                        if let Ok(dns_support) = self
                            .ec2_client
                            .describe_vpc_attribute()
                            .vpc_id(&vpc_id)
                            .attribute(aws_sdk_ec2::types::VpcAttributeName::EnableDnsSupport)
                            .send()
                            .await
                            && let Some(attr) = dns_support.enable_dns_support()
                        {
                            attributes.insert(
                                "enable_dns_support".to_string(),
                                Value::Bool(attr.value.unwrap_or(true)),
                            );
                        }

                        if let Ok(dns_hostnames) = self
                            .ec2_client
                            .describe_vpc_attribute()
                            .vpc_id(&vpc_id)
                            .attribute(aws_sdk_ec2::types::VpcAttributeName::EnableDnsHostnames)
                            .send()
                            .await
                            && let Some(attr) = dns_hostnames.enable_dns_hostnames()
                        {
                            attributes.insert(
                                "enable_dns_hostnames".to_string(),
                                Value::Bool(attr.value.unwrap_or(false)),
                            );
                        }
                    }

                    return Ok(State::existing(id, attributes));
                }
                Ok(State::not_found(id))
            }
            Err(e) => {
                // Check if it's a not found error
                let err_str = format!("{:?}", e);
                if err_str.contains("ResourceNotFound") || err_str.contains("NotFound") {
                    Ok(State::not_found(id))
                } else {
                    Err(ProviderError::new(format!("Failed to read VPC: {:?}", e)).for_resource(id))
                }
            }
        }
    }

    /// Create a VPC using Cloud Control API
    async fn create_vpc(&self, resource: Resource) -> ProviderResult<State> {
        let name = match resource.attributes.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("VPC name is required").for_resource(resource.id.clone())
                );
            }
        };

        // Build the desired state JSON for Cloud Control
        let mut desired_state = serde_json::Map::new();

        // CidrBlock (required if not using IPAM)
        if let Some(Value::String(cidr)) = resource.attributes.get("cidr_block") {
            desired_state.insert("CidrBlock".to_string(), json!(cidr));
        }

        // EnableDnsHostnames
        if let Some(Value::Bool(enabled)) = resource.attributes.get("enable_dns_hostnames") {
            desired_state.insert("EnableDnsHostnames".to_string(), json!(enabled));
        }

        // EnableDnsSupport
        if let Some(Value::Bool(enabled)) = resource.attributes.get("enable_dns_support") {
            desired_state.insert("EnableDnsSupport".to_string(), json!(enabled));
        }

        // InstanceTenancy
        if let Some(Value::String(tenancy)) = resource.attributes.get("instance_tenancy") {
            let normalized = schemas::vpc::normalize_instance_tenancy(tenancy);
            desired_state.insert("InstanceTenancy".to_string(), json!(normalized));
        }

        // Ipv4IpamPoolId
        if let Some(Value::String(pool_id)) = resource.attributes.get("ipv4_ipam_pool_id") {
            desired_state.insert("Ipv4IpamPoolId".to_string(), json!(pool_id));
        }

        // Ipv4NetmaskLength
        if let Some(Value::Int(length)) = resource.attributes.get("ipv4_netmask_length") {
            desired_state.insert("Ipv4NetmaskLength".to_string(), json!(length));
        }

        // Tags - always include Name tag, merge with user-provided tags (Terraform-style map)
        let mut tags = vec![json!({"Key": "Name", "Value": name})];

        if let Some(Value::Map(user_tags)) = resource.attributes.get("tags") {
            for (key, value) in user_tags {
                if let Value::String(v) = value {
                    // Skip if it's a Name tag (we already added it)
                    if key != "Name" {
                        tags.push(json!({"Key": key, "Value": v}));
                    }
                }
            }
        }
        desired_state.insert("Tags".to_string(), json!(tags));

        let desired_state_json = serde_json::Value::Object(desired_state);

        let result = self
            .cloudcontrol_client
            .create_resource()
            .type_name("AWS::EC2::VPC")
            .desired_state(desired_state_json.to_string())
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to create VPC: {:?}", e))
                    .for_resource(resource.id.clone())
            })?;

        // Get request token and wait for completion
        let request_token = result
            .progress_event()
            .and_then(|p| p.request_token())
            .ok_or_else(|| {
                ProviderError::new("No request token returned").for_resource(resource.id.clone())
            })?;

        self.wait_for_operation(request_token).await.map_err(|e| {
            ProviderError::new(format!("VPC creation failed: {}", e))
                .for_resource(resource.id.clone())
        })?;

        // Read back the created VPC
        self.read_vpc(&name).await
    }

    /// Update a VPC using Cloud Control API
    async fn update_vpc(&self, id: ResourceId, to: Resource) -> ProviderResult<State> {
        // Find VPC ID by name
        let vpc_id = self
            .find_vpc_id_by_name(&id.name)
            .await?
            .ok_or_else(|| ProviderError::new("VPC not found").for_resource(id.clone()))?;

        // Build patch document for Cloud Control
        // Note: CidrBlock, Ipv4IpamPoolId, Ipv4NetmaskLength cannot be changed after creation
        // Only EnableDnsHostnames, EnableDnsSupport, InstanceTenancy (partially), and Tags can be updated
        let mut patch_ops = Vec::new();

        if let Some(Value::Bool(enabled)) = to.attributes.get("enable_dns_support") {
            patch_ops.push(json!({
                "op": "replace",
                "path": "/EnableDnsSupport",
                "value": enabled
            }));
        }

        if let Some(Value::Bool(enabled)) = to.attributes.get("enable_dns_hostnames") {
            patch_ops.push(json!({
                "op": "replace",
                "path": "/EnableDnsHostnames",
                "value": enabled
            }));
        }

        // InstanceTenancy can only be updated from 'default' to 'dedicated'
        if let Some(Value::String(tenancy)) = to.attributes.get("instance_tenancy") {
            let normalized = schemas::vpc::normalize_instance_tenancy(tenancy);
            patch_ops.push(json!({
                "op": "replace",
                "path": "/InstanceTenancy",
                "value": normalized
            }));
        }

        // Update tags (Terraform-style map)
        if let Some(Value::Map(user_tags)) = to.attributes.get("tags") {
            let name = to
                .attributes
                .get("name")
                .and_then(|v| {
                    if let Value::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| id.name.clone());

            let mut tags = vec![json!({"Key": "Name", "Value": name})];
            for (key, value) in user_tags {
                if let Value::String(v) = value
                    && key != "Name"
                {
                    tags.push(json!({"Key": key, "Value": v}));
                }
            }
            patch_ops.push(json!({
                "op": "replace",
                "path": "/Tags",
                "value": tags
            }));
        }

        if patch_ops.is_empty() {
            // No changes to apply
            return self.read_vpc(&id.name).await;
        }

        let patch_document = serde_json::to_string(&patch_ops).map_err(|e| {
            ProviderError::new(format!("Failed to build patch document: {}", e))
                .for_resource(id.clone())
        })?;

        let result = self
            .cloudcontrol_client
            .update_resource()
            .type_name("AWS::EC2::VPC")
            .identifier(&vpc_id)
            .patch_document(patch_document)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to update VPC: {:?}", e))
                    .for_resource(id.clone())
            })?;

        // Wait for completion
        if let Some(request_token) = result.progress_event().and_then(|p| p.request_token()) {
            self.wait_for_operation(request_token).await.map_err(|e| {
                ProviderError::new(format!("VPC update failed: {}", e)).for_resource(id.clone())
            })?;
        }

        self.read_vpc(&id.name).await
    }

    /// Delete a VPC using Cloud Control API
    async fn delete_vpc(&self, id: ResourceId) -> ProviderResult<()> {
        // Find VPC ID by name
        let vpc_id = self
            .find_vpc_id_by_name(&id.name)
            .await?
            .ok_or_else(|| ProviderError::new("VPC not found").for_resource(id.clone()))?;

        let result = self
            .cloudcontrol_client
            .delete_resource()
            .type_name("AWS::EC2::VPC")
            .identifier(&vpc_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to delete VPC: {:?}", e))
                    .for_resource(id.clone())
            })?;

        // Wait for completion
        if let Some(request_token) = result.progress_event().and_then(|p| p.request_token()) {
            self.wait_for_operation(request_token).await.map_err(|e| {
                ProviderError::new(format!("VPC deletion failed: {}", e)).for_resource(id.clone())
            })?;
        }

        Ok(())
    }
}

impl Provider for AwsccProvider {
    fn name(&self) -> &'static str {
        "awscc"
    }

    fn resource_types(&self) -> Vec<Box<dyn ResourceType>> {
        vec![Box::new(VpcType)]
    }

    fn read(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "vpc" => self.read_vpc(&id.name).await,
                _ => Err(ProviderError::new(format!(
                    "Unknown resource type: {}",
                    id.resource_type
                ))
                .for_resource(id.clone())),
            }
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        Box::pin(async move {
            match resource.id.resource_type.as_str() {
                "vpc" => self.create_vpc(resource).await,
                _ => Err(ProviderError::new(format!(
                    "Unknown resource type: {}",
                    resource.id.resource_type
                ))
                .for_resource(resource.id.clone())),
            }
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let to = to.clone();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "vpc" => self.update_vpc(id, to).await,
                _ => Err(ProviderError::new(format!(
                    "Unknown resource type: {}",
                    id.resource_type
                ))
                .for_resource(id.clone())),
            }
        })
    }

    fn delete(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "vpc" => self.delete_vpc(id).await,
                _ => Err(ProviderError::new(format!(
                    "Unknown resource type: {}",
                    id.resource_type
                ))
                .for_resource(id.clone())),
            }
        })
    }
}

/// Convert DSL enum value (provider.TypeName.value_name) to AWS SDK format (value-name)
pub fn convert_enum_value(value: &str) -> String {
    let parts: Vec<&str> = value.split('.').collect();

    let raw_value = match parts.len() {
        2 => {
            // TypeName.value pattern
            if parts[0].chars().next().is_some_and(|c| c.is_uppercase()) {
                parts[1]
            } else {
                return value.to_string();
            }
        }
        3 => {
            // provider.TypeName.value pattern
            let provider = parts[0];
            let type_name = parts[1];
            if provider.chars().all(|c| c.is_lowercase())
                && type_name.chars().next().is_some_and(|c| c.is_uppercase())
            {
                parts[2]
            } else {
                return value.to_string();
            }
        }
        _ => return value.to_string(),
    };

    raw_value.replace('_', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_enum_value() {
        assert_eq!(
            convert_enum_value("aws.Region.ap_northeast_1"),
            "ap-northeast-1"
        );
        assert_eq!(convert_enum_value("aws.Region.us_east_1"), "us-east-1");
        assert_eq!(
            convert_enum_value("Region.ap_northeast_1"),
            "ap-northeast-1"
        );
        assert_eq!(convert_enum_value("eu-west-1"), "eu-west-1");
    }

    #[test]
    fn test_vpc_type_name() {
        let vpc_type = VpcType;
        assert_eq!(vpc_type.name(), "vpc");
    }
}
