//! Provider trait implementation for AWS

use carina_core::provider::{BoxFuture, Provider, ProviderError, ProviderResult};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State};

use crate::AwsProvider;
use crate::normalizer::normalize_state_enums;

impl Provider for AwsProvider {
    fn name(&self) -> &'static str {
        "aws"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.map(String::from);
        Box::pin(async move {
            let mut state = match id.resource_type.as_str() {
                "s3.bucket" => self.read_s3_bucket(&id, identifier.as_deref()).await,
                "ec2.eip" => self.read_ec2_eip(&id, identifier.as_deref()).await,
                "ec2.vpc" => self.read_ec2_vpc(&id, identifier.as_deref()).await,
                "ec2.subnet" => self.read_ec2_subnet(&id, identifier.as_deref()).await,
                "ec2.internet_gateway" => {
                    self.read_ec2_internet_gateway(&id, identifier.as_deref())
                        .await
                }
                "ec2.nat_gateway" => self.read_ec2_nat_gateway(&id, identifier.as_deref()).await,
                "ec2.route_table" => self.read_ec2_route_table(&id, identifier.as_deref()).await,
                "ec2.route" => self.read_ec2_route(&id, identifier.as_deref()).await,
                "ec2.security_group" => {
                    self.read_ec2_security_group(&id, identifier.as_deref())
                        .await
                }
                "ec2.security_group_ingress" => {
                    self.read_ec2_security_group_ingress(&id, identifier.as_deref())
                        .await
                }
                "ec2.security_group_egress" => {
                    self.read_ec2_security_group_egress(&id, identifier.as_deref())
                        .await
                }
                "ec2.subnet_route_table_association" => {
                    self.read_ec2_subnet_route_table_association(&id, identifier.as_deref())
                        .await
                }
                "ec2.flow_log" => self.read_ec2_flow_log(&id, identifier.as_deref()).await,
                "ec2.vpc_endpoint" => self.read_ec2_vpc_endpoint(&id, identifier.as_deref()).await,
                "ec2.vpc_gateway_attachment" => {
                    self.read_ec2_vpc_gateway_attachment(&id, identifier.as_deref())
                        .await
                }
                "ec2.vpn_gateway" => self.read_ec2_vpn_gateway(&id, identifier.as_deref()).await,
                "ec2.transit_gateway" => {
                    self.read_ec2_transit_gateway(&id, identifier.as_deref())
                        .await
                }
                "ec2.transit_gateway_attachment" => {
                    self.read_ec2_transit_gateway_attachment(&id, identifier.as_deref())
                        .await
                }
                "ec2.vpc_peering_connection" => {
                    self.read_ec2_vpc_peering_connection(&id, identifier.as_deref())
                        .await
                }
                "ec2.egress_only_internet_gateway" => {
                    self.read_ec2_egress_only_internet_gateway(&id, identifier.as_deref())
                        .await
                }
                "iam.role" => self.read_iam_role(&id, identifier.as_deref()).await,
                "logs.log_group" => self.read_logs_log_group(&id, identifier.as_deref()).await,
                "sts.caller_identity" => self.read_sts_caller_identity(&id).await,
                _ => Err(ProviderError::new(format!(
                    "Unknown resource type: {}",
                    id.resource_type
                ))
                .for_resource(id.clone())),
            }?;

            // Normalize enum values in read state to namespaced DSL format
            if state.exists {
                normalize_state_enums(&id.resource_type, &mut state.attributes);
            }

            Ok(state)
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        Box::pin(async move {
            match resource.id.resource_type.as_str() {
                "s3.bucket" => self.create_s3_bucket(resource).await,
                "ec2.eip" => self.create_ec2_eip(resource).await,
                "ec2.vpc" => self.create_ec2_vpc(resource).await,
                "ec2.subnet" => self.create_ec2_subnet(resource).await,
                "ec2.internet_gateway" => self.create_ec2_internet_gateway(resource).await,
                "ec2.nat_gateway" => self.create_ec2_nat_gateway(resource).await,
                "ec2.route_table" => self.create_ec2_route_table(resource).await,
                "ec2.route" => self.create_ec2_route(resource).await,
                "ec2.security_group" => self.create_ec2_security_group(resource).await,
                "ec2.security_group_ingress" => {
                    self.create_ec2_security_group_ingress(resource).await
                }
                "ec2.security_group_egress" => {
                    self.create_ec2_security_group_egress(resource).await
                }
                "ec2.subnet_route_table_association" => {
                    self.create_ec2_subnet_route_table_association(resource)
                        .await
                }
                "ec2.flow_log" => self.create_ec2_flow_log(resource).await,
                "ec2.vpc_endpoint" => self.create_ec2_vpc_endpoint(resource).await,
                "ec2.vpc_gateway_attachment" => {
                    self.create_ec2_vpc_gateway_attachment(resource).await
                }
                "ec2.vpn_gateway" => self.create_ec2_vpn_gateway(resource).await,
                "ec2.transit_gateway" => self.create_ec2_transit_gateway(resource).await,
                "ec2.transit_gateway_attachment" => {
                    self.create_ec2_transit_gateway_attachment(resource).await
                }
                "ec2.vpc_peering_connection" => {
                    self.create_ec2_vpc_peering_connection(resource).await
                }
                "ec2.egress_only_internet_gateway" => {
                    self.create_ec2_egress_only_internet_gateway(resource).await
                }
                "iam.role" => self.create_iam_role(resource).await,
                "logs.log_group" => self.create_logs_log_group(resource).await,
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
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let from = from.clone();
        let to = to.clone();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "s3.bucket" => self.update_s3_bucket(id, &identifier, &from, to).await,
                "ec2.eip" => self.update_ec2_eip(id, &identifier, &from, to).await,
                "ec2.vpc" => self.update_ec2_vpc(id, &identifier, &from, to).await,
                "ec2.subnet" => self.update_ec2_subnet(id, &identifier, &from, to).await,
                "ec2.internet_gateway" => {
                    self.update_ec2_internet_gateway(id, &identifier, &from, to)
                        .await
                }
                "ec2.nat_gateway" => {
                    self.update_ec2_nat_gateway(id, &identifier, &from, to)
                        .await
                }
                "ec2.route_table" => {
                    self.update_ec2_route_table(id, &identifier, &from, to)
                        .await
                }
                "ec2.route" => self.update_ec2_route(id, &identifier, to).await,
                "ec2.security_group" => {
                    self.update_ec2_security_group(id, &identifier, &from, to)
                        .await
                }
                "ec2.security_group_ingress" => {
                    self.update_ec2_security_group_ingress(id, &identifier, to)
                        .await
                }
                "ec2.security_group_egress" => {
                    self.update_ec2_security_group_egress(id, &identifier, to)
                        .await
                }
                "ec2.subnet_route_table_association" => {
                    self.update_ec2_subnet_route_table_association(id, &identifier, to)
                        .await
                }
                "ec2.flow_log" => self.update_ec2_flow_log(id, &identifier, &from, to).await,
                "ec2.vpc_endpoint" => {
                    self.update_ec2_vpc_endpoint(id, &identifier, &from, to)
                        .await
                }
                "ec2.vpc_gateway_attachment" => {
                    self.update_ec2_vpc_gateway_attachment(id, &identifier)
                        .await
                }
                "ec2.vpn_gateway" => {
                    self.update_ec2_vpn_gateway(id, &identifier, &from, to)
                        .await
                }
                "ec2.transit_gateway" => {
                    self.update_ec2_transit_gateway(id, &identifier, &from, to)
                        .await
                }
                "ec2.transit_gateway_attachment" => {
                    self.update_ec2_transit_gateway_attachment(id, &identifier, &from, to)
                        .await
                }
                "ec2.vpc_peering_connection" => {
                    self.update_ec2_vpc_peering_connection(id, &identifier, &from, to)
                        .await
                }
                "ec2.egress_only_internet_gateway" => {
                    self.update_ec2_egress_only_internet_gateway(id, &identifier, &from, to)
                        .await
                }
                "iam.role" => self.update_iam_role(id, &identifier, &from, to).await,
                "logs.log_group" => self.update_logs_log_group(id, &identifier, &from, to).await,
                _ => Err(ProviderError::new(format!(
                    "Unknown resource type: {}",
                    id.resource_type
                ))
                .for_resource(id.clone())),
            }
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let lifecycle = lifecycle.clone();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "s3.bucket" => self.delete_s3_bucket(id, &identifier, &lifecycle).await,
                "ec2.eip" => self.delete_ec2_eip(id, &identifier).await,
                "ec2.vpc" => self.delete_ec2_vpc(id, &identifier).await,
                "ec2.subnet" => self.delete_ec2_subnet(id, &identifier).await,
                "ec2.internet_gateway" => self.delete_ec2_internet_gateway(id, &identifier).await,
                "ec2.nat_gateway" => self.delete_ec2_nat_gateway(id, &identifier).await,
                "ec2.route_table" => self.delete_ec2_route_table(id, &identifier).await,
                "ec2.route" => self.delete_ec2_route(id, &identifier).await,
                "ec2.security_group" => self.delete_ec2_security_group(id, &identifier).await,
                "ec2.security_group_ingress" => {
                    self.delete_ec2_security_group_ingress(id, &identifier)
                        .await
                }
                "ec2.security_group_egress" => {
                    self.delete_ec2_security_group_egress(id, &identifier).await
                }
                "ec2.subnet_route_table_association" => {
                    self.delete_ec2_subnet_route_table_association(id, &identifier)
                        .await
                }
                "ec2.flow_log" => self.delete_ec2_flow_log(id, &identifier).await,
                "ec2.vpc_endpoint" => self.delete_ec2_vpc_endpoint(id, &identifier).await,
                "ec2.vpc_gateway_attachment" => {
                    self.delete_ec2_vpc_gateway_attachment(id, &identifier)
                        .await
                }
                "ec2.vpn_gateway" => self.delete_ec2_vpn_gateway(id, &identifier).await,
                "ec2.transit_gateway" => self.delete_ec2_transit_gateway(id, &identifier).await,
                "ec2.transit_gateway_attachment" => {
                    self.delete_ec2_transit_gateway_attachment(id, &identifier)
                        .await
                }
                "ec2.vpc_peering_connection" => {
                    self.delete_ec2_vpc_peering_connection(id, &identifier)
                        .await
                }
                "ec2.egress_only_internet_gateway" => {
                    self.delete_ec2_egress_only_internet_gateway(id, &identifier)
                        .await
                }
                "iam.role" => self.delete_iam_role(id, &identifier).await,
                "logs.log_group" => self.delete_logs_log_group(id, &identifier).await,
                _ => Err(ProviderError::new(format!(
                    "Unknown resource type: {}",
                    id.resource_type
                ))
                .for_resource(id.clone())),
            }
        })
    }
}
