use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 Transit Gateway VPC Attachment
    pub(crate) async fn read_ec2_transit_gateway_attachment(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let result = self
            .ec2_client
            .describe_transit_gateway_vpc_attachments()
            .transit_gateway_attachment_ids(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe transit gateway VPC attachments")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(att) = result.transit_gateway_vpc_attachments().first() {
            // Skip deleted attachments
            if att.state().map(|s| s.as_str()) == Some("deleted") {
                return Ok(State::not_found(id.clone()));
            }

            let mut attributes = HashMap::new();

            let identifier_value =
                Self::extract_ec2_transit_gateway_attachment_attributes(att, &mut attributes);

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(att.tags()) {
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

    /// Create an EC2 Transit Gateway VPC Attachment
    pub(crate) async fn create_ec2_transit_gateway_attachment(
        &self,
        resource: Resource,
    ) -> ProviderResult<State> {
        let transit_gateway_id = match resource.attributes.get("transit_gateway_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("transit_gateway_id is required")
                    .for_resource(resource.id.clone()));
            }
        };

        let vpc_id = match resource.attributes.get("vpc_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("vpc_id is required").for_resource(resource.id.clone())
                );
            }
        };

        let subnet_ids = match resource.attributes.get("subnet_ids") {
            Some(Value::List(ids)) => {
                let mut result = Vec::new();
                for id_val in ids {
                    if let Value::String(s) = id_val {
                        result.push(s.clone());
                    }
                }
                if result.is_empty() {
                    return Err(ProviderError::new("subnet_ids must not be empty")
                        .for_resource(resource.id.clone()));
                }
                result
            }
            _ => {
                return Err(
                    ProviderError::new("subnet_ids is required").for_resource(resource.id.clone())
                );
            }
        };

        let mut req = self
            .ec2_client
            .create_transit_gateway_vpc_attachment()
            .transit_gateway_id(&transit_gateway_id)
            .vpc_id(&vpc_id);

        for subnet_id in &subnet_ids {
            req = req.subnet_ids(subnet_id);
        }

        // Apply tags via TagSpecifications
        if let Some(Value::Map(tags)) = resource.attributes.get("tags") {
            use aws_sdk_ec2::types::{Tag, TagSpecification};
            let mut tag_spec = TagSpecification::builder()
                .resource_type(aws_sdk_ec2::types::ResourceType::TransitGatewayAttachment);
            for (key, val) in tags {
                if let Value::String(v) = val {
                    tag_spec = tag_spec.tags(Tag::builder().key(key).value(v).build());
                }
            }
            req = req.tag_specifications(tag_spec.build());
        }

        let result = req.send().await.map_err(|e| {
            ProviderError::new("Failed to create transit gateway VPC attachment")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        let att_id = result
            .transit_gateway_vpc_attachment()
            .and_then(|att| att.transit_gateway_attachment_id())
            .ok_or_else(|| {
                ProviderError::new("Transit Gateway Attachment created but no ID returned")
                    .for_resource(resource.id.clone())
            })?;

        // Wait for attachment to become available
        self.wait_for_transit_gateway_attachment_available(&resource.id, att_id)
            .await?;

        // Read back
        self.read_ec2_transit_gateway_attachment(&resource.id, Some(att_id))
            .await
    }

    /// Update an EC2 Transit Gateway VPC Attachment (tags only)
    pub(crate) async fn update_ec2_transit_gateway_attachment(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        self.apply_ec2_tags(&id, identifier, &to.attributes, Some(&from.attributes))
            .await?;
        self.read_ec2_transit_gateway_attachment(&id, Some(identifier))
            .await
    }

    /// Delete an EC2 Transit Gateway VPC Attachment
    pub(crate) async fn delete_ec2_transit_gateway_attachment(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .delete_transit_gateway_vpc_attachment()
            .transit_gateway_attachment_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete transit gateway VPC attachment")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        // Wait for attachment to be deleted
        self.wait_for_transit_gateway_attachment_deleted(&id, identifier)
            .await?;

        Ok(())
    }

    /// Wait for a transit gateway attachment to reach the "available" state
    async fn wait_for_transit_gateway_attachment_available(
        &self,
        id: &ResourceId,
        attachment_id: &str,
    ) -> ProviderResult<()> {
        use std::time::Duration;
        use tokio::time::sleep;

        for _ in 0..60 {
            let result = self
                .ec2_client
                .describe_transit_gateway_vpc_attachments()
                .transit_gateway_attachment_ids(attachment_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to describe transit gateway VPC attachment")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;

            if let Some(att) = result.transit_gateway_vpc_attachments().first()
                && let Some(state) = att.state()
            {
                if state.as_str() == "available" {
                    return Ok(());
                }
                if state.as_str() == "failed" || state.as_str() == "deleted" {
                    return Err(
                        ProviderError::new("Transit gateway attachment creation failed")
                            .for_resource(id.clone()),
                    );
                }
            }

            sleep(Duration::from_secs(5)).await;
        }

        Err(ProviderError::new(
            "Timeout waiting for transit gateway attachment to become available",
        )
        .for_resource(id.clone()))
    }

    /// Wait for a transit gateway attachment to be deleted
    async fn wait_for_transit_gateway_attachment_deleted(
        &self,
        id: &ResourceId,
        attachment_id: &str,
    ) -> ProviderResult<()> {
        use std::time::Duration;
        use tokio::time::sleep;

        for _ in 0..60 {
            let result = self
                .ec2_client
                .describe_transit_gateway_vpc_attachments()
                .transit_gateway_attachment_ids(attachment_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new("Failed to describe transit gateway VPC attachment")
                        .with_cause(e)
                        .for_resource(id.clone())
                })?;

            if let Some(att) = result.transit_gateway_vpc_attachments().first() {
                if att.state().map(|s| s.as_str()) == Some("deleted") {
                    return Ok(());
                }
            } else {
                return Ok(());
            }

            sleep(Duration::from_secs(5)).await;
        }

        Err(
            ProviderError::new("Timeout waiting for transit gateway attachment to be deleted")
                .for_resource(id.clone()),
        )
    }
}
