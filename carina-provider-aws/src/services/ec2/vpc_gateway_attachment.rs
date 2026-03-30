use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;
use crate::helpers::require_string_attr;

impl AwsProvider {
    /// Read an EC2 VPC Gateway Attachment
    ///
    /// The identifier is a composite: `vpc_id|igw_id` or `vpc_id|vgw_id`
    pub(crate) async fn read_ec2_vpc_gateway_attachment(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        // Parse composite identifier: vpc_id|gateway_id
        let Some((vpc_id, gateway_id)) = identifier.split_once('|') else {
            return Ok(State::not_found(id.clone()));
        };

        if gateway_id.starts_with("igw-") {
            // Internet Gateway attachment
            self.read_igw_attachment(id, vpc_id, gateway_id, identifier)
                .await
        } else if gateway_id.starts_with("vgw-") {
            // VPN Gateway attachment
            self.read_vgw_attachment(id, vpc_id, gateway_id, identifier)
                .await
        } else {
            Ok(State::not_found(id.clone()))
        }
    }

    async fn read_igw_attachment(
        &self,
        id: &ResourceId,
        vpc_id: &str,
        igw_id: &str,
        composite: &str,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let filter = Filter::builder()
            .name("internet-gateway-id")
            .values(igw_id)
            .build();

        let result = self
            .ec2_client
            .describe_internet_gateways()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe internet gateways")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(igw) = result.internet_gateways().first() {
            // Check if the IGW is attached to the specified VPC
            for attachment in igw.attachments() {
                if attachment.vpc_id() == Some(vpc_id) {
                    let mut attributes = HashMap::new();
                    attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
                    attributes.insert(
                        "internet_gateway_id".to_string(),
                        Value::String(igw_id.to_string()),
                    );

                    return Ok(State::existing(id.clone(), attributes)
                        .with_identifier(composite.to_string()));
                }
            }
        }

        Ok(State::not_found(id.clone()))
    }

    async fn read_vgw_attachment(
        &self,
        id: &ResourceId,
        vpc_id: &str,
        vgw_id: &str,
        composite: &str,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let filter = Filter::builder()
            .name("vpn-gateway-id")
            .values(vgw_id)
            .build();

        let result = self
            .ec2_client
            .describe_vpn_gateways()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe VPN gateways")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(vgw) = result.vpn_gateways().first() {
            for attachment in vgw.vpc_attachments() {
                if attachment.vpc_id() == Some(vpc_id) {
                    let state_str = attachment.state().map(|s| s.as_str());
                    if state_str == Some("attached") {
                        let mut attributes = HashMap::new();
                        attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
                        attributes.insert(
                            "vpn_gateway_id".to_string(),
                            Value::String(vgw_id.to_string()),
                        );

                        return Ok(State::existing(id.clone(), attributes)
                            .with_identifier(composite.to_string()));
                    }
                }
            }
        }

        Ok(State::not_found(id.clone()))
    }

    /// Create an EC2 VPC Gateway Attachment
    pub(crate) async fn create_ec2_vpc_gateway_attachment(
        &self,
        resource: Resource,
    ) -> ProviderResult<State> {
        let vpc_id = require_string_attr(&resource, "vpc_id")?;

        if let Some(Value::String(igw_id)) = resource.get_attr("internet_gateway_id") {
            // Attach Internet Gateway
            self.ec2_client
                .attach_internet_gateway()
                .internet_gateway_id(igw_id)
                .vpc_id(&vpc_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to attach internet gateway")
                        .with_cause(e)
                        .for_resource(resource.id.clone())
                })?;

            let composite = format!("{}|{}", vpc_id, igw_id);
            self.read_ec2_vpc_gateway_attachment(&resource.id, Some(&composite))
                .await
        } else if let Some(Value::String(vgw_id)) = resource.get_attr("vpn_gateway_id") {
            // Attach VPN Gateway
            self.ec2_client
                .attach_vpn_gateway()
                .vpn_gateway_id(vgw_id)
                .vpc_id(&vpc_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to attach VPN gateway")
                        .with_cause(e)
                        .for_resource(resource.id.clone())
                })?;

            let composite = format!("{}|{}", vpc_id, vgw_id);
            self.read_ec2_vpc_gateway_attachment(&resource.id, Some(&composite))
                .await
        } else {
            Err(
                ProviderError::new("Either internet_gateway_id or vpn_gateway_id is required")
                    .for_resource(resource.id.clone()),
            )
        }
    }

    /// Update an EC2 VPC Gateway Attachment (not supported - create_only)
    pub(crate) async fn update_ec2_vpc_gateway_attachment(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<State> {
        // VPC Gateway Attachment attributes are all create_only,
        // so updates should not occur. Read back the current state.
        self.read_ec2_vpc_gateway_attachment(&id, Some(identifier))
            .await
    }

    /// Delete an EC2 VPC Gateway Attachment
    pub(crate) async fn delete_ec2_vpc_gateway_attachment(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        let Some((vpc_id, gateway_id)) = identifier.split_once('|') else {
            return Err(ProviderError::new(format!(
                "Invalid gateway attachment identifier: {}",
                identifier
            ))
            .for_resource(id));
        };

        if gateway_id.starts_with("igw-") {
            self.ec2_client
                .detach_internet_gateway()
                .internet_gateway_id(gateway_id)
                .vpc_id(vpc_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to detach internet gateway")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        } else if gateway_id.starts_with("vgw-") {
            self.ec2_client
                .detach_vpn_gateway()
                .vpn_gateway_id(gateway_id)
                .vpc_id(vpc_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to detach VPN gateway")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;
        } else {
            return Err(ProviderError::new(format!(
                "Unknown gateway type in identifier: {}",
                identifier
            ))
            .for_resource(id));
        }

        Ok(())
    }
}
