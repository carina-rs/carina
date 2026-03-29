use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 Internet Gateway
    pub(crate) async fn read_ec2_internet_gateway(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let filter = Filter::builder()
            .name("internet-gateway-id")
            .values(identifier)
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
            let mut attributes = HashMap::new();

            // Auto-generated attribute extraction
            let identifier_value =
                Self::extract_ec2_internet_gateway_attributes(igw, &mut attributes);

            // Store attached VPC ID (from Attachments, not a direct member)
            if let Some(attachment) = igw.attachments().first()
                && let Some(vpc_id) = attachment.vpc_id()
            {
                attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
            }

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(igw.tags()) {
                attributes.insert("tags".to_string(), tags_value);
            }

            let state = State::existing(id.clone(), attributes);
            Ok(if let Some(id_val) = identifier_value {
                state.with_identifier(id_val)
            } else {
                state
            })
        } else {
            Ok(State::not_found(id.clone()))
        }
    }

    /// Create an EC2 Internet Gateway
    pub(crate) async fn create_ec2_internet_gateway(
        &self,
        resource: Resource,
    ) -> ProviderResult<State> {
        // Create Internet Gateway
        let result = self
            .ec2_client
            .create_internet_gateway()
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to create internet gateway")
                    .with_cause(e)
                    .for_resource(resource.id.clone())
            })?;

        let igw_id = result
            .internet_gateway()
            .and_then(|igw| igw.internet_gateway_id())
            .ok_or_else(|| {
                ProviderError::new("Internet Gateway created but no ID returned")
                    .for_resource(resource.id.clone())
            })?;

        // Apply tags
        self.apply_ec2_tags(&resource.id, igw_id, &resource.resolved_attributes(), None)
            .await?;

        // Attach to VPC if specified
        if let Some(Value::String(vpc_id)) = resource.get_attr("vpc_id") {
            self.ec2_client
                .attach_internet_gateway()
                .internet_gateway_id(igw_id)
                .vpc_id(vpc_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to attach internet gateway")
                        .with_cause(e)
                        .for_resource(resource.id.clone())
                })?;
        }

        // Read back using IGW ID (reliable identifier)
        self.read_ec2_internet_gateway(&resource.id, Some(igw_id))
            .await
    }

    /// Delete an EC2 Internet Gateway
    pub(crate) async fn delete_ec2_internet_gateway(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        // Look up the IGW to check for VPC attachments before deleting
        let result = self
            .ec2_client
            .describe_internet_gateways()
            .internet_gateway_ids(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe internet gateway")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(igw) = result.internet_gateways().first() {
            // Detach from VPC first
            if let Some(attachment) = igw.attachments().first()
                && let Some(vpc_id) = attachment.vpc_id()
            {
                self.ec2_client
                    .detach_internet_gateway()
                    .internet_gateway_id(identifier)
                    .vpc_id(vpc_id)
                    .send()
                    .await
                    .map_err(|e| {
                        ProviderError::new("Failed to detach internet gateway")
                            .with_cause(e)
                            .for_resource(id.clone())
                    })?;
            }
        }

        // Delete Internet Gateway
        self.ec2_client
            .delete_internet_gateway()
            .internet_gateway_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete internet gateway")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        Ok(())
    }
}
