use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::utils::extract_enum_value;

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 Elastic IP
    pub(crate) async fn read_ec2_eip(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let filter = Filter::builder()
            .name("allocation-id")
            .values(identifier)
            .build();

        let result = self
            .ec2_client
            .describe_addresses()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe addresses")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(addr) = result.addresses().first() {
            let mut attributes = HashMap::new();

            // Extract attributes
            let identifier_value = Self::extract_ec2_eip_attributes(addr, &mut attributes);

            // Extract user-defined tags
            if let Some(tags_value) = Self::ec2_tags_to_value(addr.tags()) {
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

    /// Create an EC2 Elastic IP
    pub(crate) async fn create_ec2_eip(&self, resource: Resource) -> ProviderResult<State> {
        let mut req = self.ec2_client.allocate_address();

        if let Some(Value::String(domain)) = resource.get_attr("domain") {
            use aws_sdk_ec2::types::DomainType;
            let domain_type = DomainType::from(extract_enum_value(domain));
            req = req.domain(domain_type);
        } else {
            // Default to VPC
            req = req.domain(aws_sdk_ec2::types::DomainType::Vpc);
        }

        let result = req.send().await.map_err(|e| {
            ProviderError::new("Failed to allocate address")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        let alloc_id = result.allocation_id().ok_or_else(|| {
            ProviderError::new("EIP allocated but no allocation ID returned")
                .for_resource(resource.id.clone())
        })?;

        // Apply tags
        self.apply_ec2_tags(
            &resource.id,
            alloc_id,
            &resource.resolved_attributes(),
            None,
        )
        .await?;

        // Read back using allocation ID
        self.read_ec2_eip(&resource.id, Some(alloc_id)).await
    }

    /// Update an EC2 Elastic IP (tags only)
    pub(crate) async fn update_ec2_eip(
        &self,
        id: ResourceId,
        identifier: &str,
        from: &State,
        to: Resource,
    ) -> ProviderResult<State> {
        self.apply_ec2_tags(
            &id,
            identifier,
            &to.resolved_attributes(),
            Some(&from.attributes),
        )
        .await?;
        self.read_ec2_eip(&id, Some(identifier)).await
    }

    /// Delete an EC2 Elastic IP
    pub(crate) async fn delete_ec2_eip(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .release_address()
            .allocation_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to release address")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;
        Ok(())
    }
}
