use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 Security Group
    pub(crate) async fn read_ec2_security_group(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let filter = Filter::builder()
            .name("group-id")
            .values(identifier)
            .build();

        let result = self
            .ec2_client
            .describe_security_groups()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to describe security groups: {:?}", e))
                    .for_resource(id.clone())
            })?;

        if let Some(sg) = result.security_groups().first() {
            let mut attributes = HashMap::new();

            // Auto-generated attribute extraction
            let identifier_value = Self::extract_ec2_security_group_attributes(sg, &mut attributes);

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(sg.tags()) {
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

    /// Create an EC2 Security Group
    pub(crate) async fn create_ec2_security_group(
        &self,
        resource: Resource,
    ) -> ProviderResult<State> {
        let vpc_id = match resource.attributes.get("vpc_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("VPC ID is required").for_resource(resource.id.clone())
                );
            }
        };

        let description = match resource.attributes.get("description") {
            Some(Value::String(s)) => s.clone(),
            _ => String::new(),
        };

        // group_name is required for CreateSecurityGroup API
        let group_name = match resource.attributes.get("group_name") {
            Some(Value::String(s)) => s.clone(),
            _ => resource.id.name.clone(),
        };

        // Create Security Group
        let result = self
            .ec2_client
            .create_security_group()
            .group_name(&group_name)
            .description(&description)
            .vpc_id(&vpc_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to create security group: {:?}", e))
                    .for_resource(resource.id.clone())
            })?;

        let sg_id = result.group_id().ok_or_else(|| {
            ProviderError::new("Security Group created but no ID returned")
                .for_resource(resource.id.clone())
        })?;

        // Apply tags
        self.apply_ec2_tags(&resource.id, sg_id, &resource.attributes, None)
            .await?;

        // Read back using security group ID (reliable identifier)
        self.read_ec2_security_group(&resource.id, Some(sg_id))
            .await
    }
}
