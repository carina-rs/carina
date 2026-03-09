use carina_core::provider::ProviderResult;
use carina_core::resource::{Resource, ResourceId, State};

use crate::AwsProvider;

impl AwsProvider {
    /// Read an EC2 Security Group Ingress Rule
    pub(crate) async fn read_ec2_security_group_ingress(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        self.read_ec2_security_group_rule(id, identifier, true)
            .await
    }

    /// Create an EC2 Security Group Ingress Rule
    pub(crate) async fn create_ec2_security_group_ingress(
        &self,
        resource: Resource,
    ) -> ProviderResult<State> {
        self.create_ec2_security_group_rule(resource, true).await
    }

    /// Update an EC2 Security Group Ingress Rule
    pub(crate) async fn update_ec2_security_group_ingress(
        &self,
        id: ResourceId,
        identifier: &str,
        to: Resource,
    ) -> ProviderResult<State> {
        self.update_ec2_security_group_rule(id, identifier, to, true)
            .await
    }

    /// Delete an EC2 Security Group Ingress Rule
    pub(crate) async fn delete_ec2_security_group_ingress(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.delete_ec2_security_group_rule(id, identifier, true)
            .await
    }
}
