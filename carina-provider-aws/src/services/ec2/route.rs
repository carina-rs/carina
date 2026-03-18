use std::collections::HashMap;

use carina_core::provider::{ProviderError, ProviderResult};
use carina_core::resource::{Resource, ResourceId, State, Value};

use crate::AwsProvider;

/// Unsupported destination attribute names for ec2.route.
/// The aws provider only supports destination_cidr_block (IPv4 CIDR).
const UNSUPPORTED_DESTINATIONS: &[&str] =
    &["destination_ipv6_cidr_block", "destination_prefix_list_id"];

/// Unsupported target attribute names for ec2.route.
/// The aws provider only supports gateway_id and nat_gateway_id.
const UNSUPPORTED_TARGETS: &[&str] = &[
    "carrier_gateway_id",
    "core_network_arn",
    "egress_only_internet_gateway_id",
    "instance_id",
    "local_gateway_id",
    "network_interface_id",
    "transit_gateway_id",
    "vpc_endpoint_id",
    "vpc_peering_connection_id",
];

/// Validate that a route resource only uses supported destination and target types.
/// Returns an error if any unsupported attributes are set.
pub(crate) fn validate_ec2_route_attributes(
    attributes: &HashMap<String, Value>,
    resource_id: &ResourceId,
) -> ProviderResult<()> {
    let unsupported_dest: Vec<&str> = UNSUPPORTED_DESTINATIONS
        .iter()
        .filter(|attr| attributes.contains_key(**attr))
        .copied()
        .collect();

    if !unsupported_dest.is_empty() {
        return Err(ProviderError::new(format!(
            "aws.ec2.route does not support destination type: {}. \
             Only destination_cidr_block (IPv4 CIDR) is supported. \
             Use awscc.ec2.route for other destination types.",
            unsupported_dest.join(", ")
        ))
        .for_resource(resource_id.clone()));
    }

    let unsupported_tgt: Vec<&str> = UNSUPPORTED_TARGETS
        .iter()
        .filter(|attr| attributes.contains_key(**attr))
        .copied()
        .collect();

    if !unsupported_tgt.is_empty() {
        return Err(ProviderError::new(format!(
            "aws.ec2.route does not support target type: {}. \
             Only gateway_id and nat_gateway_id are supported. \
             Use awscc.ec2.route for other target types.",
            unsupported_tgt.join(", ")
        ))
        .for_resource(resource_id.clone()));
    }

    Ok(())
}

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
        validate_ec2_route_attributes(&resource.attributes, &resource.id)?;

        let route_table_id = match resource.attributes.get("route_table_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("route_table_id is required")
                    .for_resource(resource.id.clone()));
            }
        };

        let destination_cidr = match resource.attributes.get("destination_cidr_block") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("destination_cidr_block is required")
                    .for_resource(resource.id.clone()));
            }
        };

        let mut req = self
            .ec2_client
            .create_route()
            .route_table_id(&route_table_id)
            .destination_cidr_block(&destination_cidr);

        // Add gateway_id if specified
        if let Some(Value::String(gw_id)) = resource.attributes.get("gateway_id") {
            req = req.gateway_id(gw_id);
        }

        // Add nat_gateway_id if specified
        if let Some(Value::String(nat_gw_id)) = resource.attributes.get("nat_gateway_id") {
            req = req.nat_gateway_id(nat_gw_id);
        }

        req.send().await.map_err(|e| {
            ProviderError::new("Failed to create route")
                .with_cause(e)
                .for_resource(resource.id.clone())
        })?;

        // Route identifier is route_table_id|destination_cidr_block
        let identifier = format!("{}|{}", route_table_id, destination_cidr);
        Ok(State::existing(resource.id, resource.attributes).with_identifier(identifier))
    }

    /// Update an EC2 Route (replace the route)
    pub(crate) async fn update_ec2_route(
        &self,
        id: ResourceId,
        _identifier: &str,
        to: Resource,
    ) -> ProviderResult<State> {
        validate_ec2_route_attributes(&to.attributes, &id)?;

        let route_table_id = match to.attributes.get("route_table_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("route_table_id is required").for_resource(id.clone())
                );
            }
        };

        let destination_cidr = match to.attributes.get("destination_cidr_block") {
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
        if let Some(Value::String(gw_id)) = to.attributes.get("gateway_id") {
            req = req.gateway_id(gw_id);
        }

        // Add nat_gateway_id if specified
        if let Some(Value::String(nat_gw_id)) = to.attributes.get("nat_gateway_id") {
            req = req.nat_gateway_id(nat_gw_id);
        }

        req.send().await.map_err(|e| {
            ProviderError::new("Failed to update route")
                .with_cause(e)
                .for_resource(id.clone())
        })?;

        // Route identifier is route_table_id|destination_cidr_block
        let identifier = format!("{}|{}", route_table_id, destination_cidr);
        Ok(State::existing(id, to.attributes.clone()).with_identifier(identifier))
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
