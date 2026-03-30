use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;
use crate::helpers::require_string_attr;

impl AwsProvider {
    /// Read an EC2 Route (routes are identified by route_table_id + destination)
    pub(crate) async fn read_ec2_route(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        // Parse composite identifier: route_table_id|destination_cidr_block
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        let Some((route_table_id, destination_cidr_block)) = identifier.split_once('|') else {
            return Ok(State::not_found(id.clone()));
        };

        // Describe the route table to get its routes
        let result = self
            .ec2_client
            .describe_route_tables()
            .route_table_ids(route_table_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to describe route table")
                    .with_cause(e)
                    .for_resource(id.clone())
            })?;

        if let Some(rt) = result.route_tables().first() {
            // Find the route matching destination_cidr_block
            for route in rt.routes() {
                if route.destination_cidr_block() == Some(destination_cidr_block) {
                    let mut attributes = HashMap::new();

                    // Auto-generated attribute extraction
                    Self::extract_ec2_route_attributes(route, &mut attributes);

                    // route_table_id is not in the Route struct, add from parameter
                    attributes.insert(
                        "route_table_id".to_string(),
                        Value::String(route_table_id.to_string()),
                    );

                    // Route identifier is route_table_id|destination_cidr_block
                    let composite = format!("{}|{}", route_table_id, destination_cidr_block);
                    return Ok(State::existing(id.clone(), attributes).with_identifier(composite));
                }
            }
        }

        Ok(State::not_found(id.clone()))
    }

    /// Create an EC2 Route
    pub(crate) async fn create_ec2_route(&self, resource: Resource) -> ProviderResult<State> {
        let route_table_id = require_string_attr(&resource, "route_table_id")?;
        let destination_cidr = require_string_attr(&resource, "destination_cidr_block")?;

        let mut req = self
            .ec2_client
            .create_route()
            .route_table_id(&route_table_id)
            .destination_cidr_block(&destination_cidr);

        // Add gateway_id if specified
        if let Some(Value::String(gw_id)) = resource.get_attr("gateway_id") {
            req = req.gateway_id(gw_id);
        }

        // Add nat_gateway_id if specified
        if let Some(Value::String(nat_gw_id)) = resource.get_attr("nat_gateway_id") {
            req = req.nat_gateway_id(nat_gw_id);
        }

        req.send().await.map_err(|e| {
            ProviderError::new("Failed to create route")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        // Route identifier is route_table_id|destination_cidr_block
        let identifier = format!("{}|{}", route_table_id, destination_cidr);
        Ok(
            State::existing(resource.id.clone(), resource.resolved_attributes())
                .with_identifier(identifier),
        )
    }

    /// Update an EC2 Route (replace the route)
    pub(crate) async fn update_ec2_route(
        &self,
        id: ResourceId,
        _identifier: &str,
        to: Resource,
    ) -> ProviderResult<State> {
        let route_table_id = match to.get_attr("route_table_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("route_table_id is required").for_resource(id.clone())
                );
            }
        };

        let destination_cidr = match to.get_attr("destination_cidr_block") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("destination_cidr_block is required")
                    .for_resource(id.clone()));
            }
        };

        let mut req = self
            .ec2_client
            .replace_route()
            .route_table_id(&route_table_id)
            .destination_cidr_block(&destination_cidr);

        // Add gateway_id if specified
        if let Some(Value::String(gw_id)) = to.get_attr("gateway_id") {
            req = req.gateway_id(gw_id);
        }

        // Add nat_gateway_id if specified
        if let Some(Value::String(nat_gw_id)) = to.get_attr("nat_gateway_id") {
            req = req.nat_gateway_id(nat_gw_id);
        }

        req.send().await.map_err(|e| {
            ProviderError::new("Failed to update route")
                .with_cause(e)
                .for_resource(id.clone())
        })?;

        // Route identifier is route_table_id|destination_cidr_block
        let identifier = format!("{}|{}", route_table_id, destination_cidr);
        Ok(State::existing(id, to.resolved_attributes()).with_identifier(identifier))
    }

    /// Delete an EC2 Route
    pub(crate) async fn delete_ec2_route(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        // Parse composite identifier: route_table_id|destination_cidr_block
        let Some((route_table_id, destination_cidr_block)) = identifier.split_once('|') else {
            return Err(
                ProviderError::new(format!("Invalid route identifier: {}", identifier))
                    .for_resource(id),
            );
        };

        self.ec2_client
            .delete_route()
            .route_table_id(route_table_id)
            .destination_cidr_block(destination_cidr_block)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new("Failed to delete route")
                    .with_cause(e)
                    .for_resource(id)
            })?;

        Ok(())
    }
}
