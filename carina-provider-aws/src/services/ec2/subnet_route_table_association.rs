use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;
use crate::helpers::require_string_attr;

impl AwsProvider {
    /// Read an EC2 Subnet Route Table Association
    pub(crate) async fn read_ec2_subnet_route_table_association(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        // Parse composite identifier: association_id|subnet_id
        let Some((association_id, subnet_id)) = identifier.split_once('|') else {
            return Ok(State::not_found(id.clone()));
        };

        // Describe route tables filtered by association ID
        use aws_sdk_ec2::types::Filter;

        let filter = Filter::builder()
            .name("association.route-table-association-id")
            .values(association_id)
            .build();

        let result = self
            .ec2_client
            .describe_route_tables()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe route tables")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(rt) = result.route_tables().first() {
            // Find the association matching the association_id
            for assoc in rt.associations() {
                if assoc.route_table_association_id() == Some(association_id) {
                    let mut attributes = HashMap::new();

                    attributes.insert(
                        "association_id".to_string(),
                        Value::String(association_id.to_string()),
                    );

                    if let Some(rt_id) = assoc.route_table_id() {
                        attributes.insert(
                            "route_table_id".to_string(),
                            Value::String(rt_id.to_string()),
                        );
                    }

                    if let Some(sid) = assoc.subnet_id() {
                        attributes.insert("subnet_id".to_string(), Value::String(sid.to_string()));
                    }

                    let composite = format!("{}|{}", association_id, subnet_id);
                    return Ok(State::existing(id.clone(), attributes).with_identifier(composite));
                }
            }
        }

        Ok(State::not_found(id.clone()))
    }

    /// Create an EC2 Subnet Route Table Association
    pub(crate) async fn create_ec2_subnet_route_table_association(
        &self,
        resource: Resource,
    ) -> ProviderResult<State> {
        let route_table_id = require_string_attr(&resource, "route_table_id")?;
        let subnet_id = require_string_attr(&resource, "subnet_id")?;

        let result = self
            .ec2_client
            .associate_route_table()
            .route_table_id(&route_table_id)
            .subnet_id(&subnet_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to associate route table")
                    .with_cause(e)
                    .for_resource(resource.id.clone())
            })?;

        let assoc_id = result.association_id().ok_or_else(|| {
            ProviderError::new("Route table association created but no association ID returned")
                .for_resource(resource.id.clone())
        })?;

        // Composite identifier: association_id|subnet_id
        let composite = format!("{}|{}", assoc_id, subnet_id);
        self.read_ec2_subnet_route_table_association(&resource.id, Some(&composite))
            .await
    }

    /// Update an EC2 Subnet Route Table Association (replace association)
    pub(crate) async fn update_ec2_subnet_route_table_association(
        &self,
        id: ResourceId,
        identifier: &str,
        to: Resource,
    ) -> ProviderResult<State> {
        // Parse composite identifier: association_id|subnet_id
        let Some((association_id, subnet_id)) = identifier.split_once('|') else {
            return Err(ProviderError::new(format!(
                "Invalid association identifier: {}",
                identifier
            ))
            .for_resource(id.clone()));
        };

        let route_table_id = match to.get_attr("route_table_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("route_table_id is required").for_resource(id.clone())
                );
            }
        };

        // Replace the association with a new route table
        let result = self
            .ec2_client
            .replace_route_table_association()
            .association_id(association_id)
            .route_table_id(&route_table_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to replace route table association")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        let new_assoc_id = result.new_association_id().ok_or_else(|| {
            ProviderError::new(
                "Route table association replaced but no new association ID returned",
            )
            .for_resource(id.clone())
        })?;

        let composite = format!("{}|{}", new_assoc_id, subnet_id);
        self.read_ec2_subnet_route_table_association(&id, Some(&composite))
            .await
    }

    /// Delete an EC2 Subnet Route Table Association
    pub(crate) async fn delete_ec2_subnet_route_table_association(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        // Parse composite identifier: association_id|subnet_id
        let Some((association_id, _subnet_id)) = identifier.split_once('|') else {
            return Err(ProviderError::new(format!(
                "Invalid association identifier: {}",
                identifier
            ))
            .for_resource(id));
        };

        self.ec2_client
            .disassociate_route_table()
            .association_id(association_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to disassociate route table")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        Ok(())
    }
}
