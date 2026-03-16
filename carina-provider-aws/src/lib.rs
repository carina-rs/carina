//! Carina AWS Provider
//!
//! AWS Provider implementation

pub mod provider_generated;
pub mod schemas;
mod services;

use std::collections::HashMap;

use aws_config::Region;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_sts::Client as StsClient;
use carina_core::provider::{
    BoxFuture, Provider, ProviderError, ProviderFactory, ProviderResult, ResourceType,
};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::utils::convert_enum_value;

/// Factory for creating and configuring the AWS Provider
pub struct AwsProviderFactory;

impl ProviderFactory for AwsProviderFactory {
    fn name(&self) -> &str {
        "aws"
    }

    fn display_name(&self) -> &str {
        "AWS provider"
    }

    fn validate_config(&self, attributes: &HashMap<String, Value>) -> Result<(), String> {
        let region_type = schemas::types::aws_region();
        if let Some(region_value) = attributes.get("region") {
            region_type
                .validate(region_value)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn extract_region(&self, attributes: &HashMap<String, Value>) -> String {
        if let Some(Value::String(region)) = attributes.get("region") {
            if let Some(rest) = region.strip_prefix("aws.Region.") {
                return rest.replace('_', "-");
            }
            return region.clone();
        }
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn Provider>> {
        let region = self.extract_region(attributes);
        Box::pin(async move { Box::new(AwsProvider::new(&region).await) as Box<dyn Provider> })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ResourceSchema> {
        schemas::all_schemas()
    }

    fn identity_attributes(&self) -> Vec<&str> {
        vec!["region"]
    }

    fn region_completions(&self) -> Vec<carina_core::schema::CompletionValue> {
        carina_aws_types::region_completions("aws")
    }
}

/// AWS Provider
pub struct AwsProvider {
    s3_client: S3Client,
    ec2_client: Ec2Client,
    sts_client: StsClient,
    region: String,
}

impl AwsProvider {
    /// Create a new AWS Provider
    pub async fn new(region: &str) -> Self {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(Region::new(region.to_string()))
            .load()
            .await;

        Self {
            s3_client: S3Client::new(&config),
            ec2_client: Ec2Client::new(&config),
            sts_client: StsClient::new(&config),
            region: region.to_string(),
        }
    }

    /// Create with specific clients (for testing)
    pub fn with_clients(
        s3_client: S3Client,
        ec2_client: Ec2Client,
        sts_client: StsClient,
        region: String,
    ) -> Self {
        Self {
            s3_client,
            ec2_client,
            sts_client,
            region,
        }
    }

    /// Extract tags from EC2 tag list into a Value::Map
    pub(crate) fn ec2_tags_to_value(tags: &[aws_sdk_ec2::types::Tag]) -> Option<Value> {
        let mut tag_map = HashMap::new();
        for tag in tags {
            if let (Some(key), Some(value)) = (tag.key(), tag.value()) {
                tag_map.insert(key.to_string(), Value::String(value.to_string()));
            }
        }
        if tag_map.is_empty() {
            None
        } else {
            Some(Value::Map(tag_map))
        }
    }

    /// Build EC2 Tag list from Value::Map
    fn value_to_ec2_tags(value: &Value) -> Vec<aws_sdk_ec2::types::Tag> {
        let mut tags = Vec::new();
        if let Value::Map(map) = value {
            for (key, val) in map {
                if let Value::String(v) = val {
                    tags.push(aws_sdk_ec2::types::Tag::builder().key(key).value(v).build());
                }
            }
        }
        tags
    }

    /// Apply tags to an EC2 resource
    ///
    /// When `from_attributes` is provided, tags that exist in `from` but not in `to`
    /// will be deleted from the resource.
    pub(crate) async fn apply_ec2_tags(
        &self,
        resource_id: &ResourceId,
        ec2_resource_id: &str,
        attributes: &HashMap<String, Value>,
        from_attributes: Option<&HashMap<String, Value>>,
    ) -> ProviderResult<()> {
        // Delete tags that were removed (present in from but not in to)
        if let Some(from_attrs) = from_attributes {
            let old_keys: std::collections::HashSet<&String> =
                if let Some(Value::Map(old_map)) = from_attrs.get("tags") {
                    old_map.keys().collect()
                } else {
                    std::collections::HashSet::new()
                };
            let new_keys: std::collections::HashSet<&String> =
                if let Some(Value::Map(new_map)) = attributes.get("tags") {
                    new_map.keys().collect()
                } else {
                    std::collections::HashSet::new()
                };
            let removed_keys: Vec<&String> = old_keys.difference(&new_keys).copied().collect();
            if !removed_keys.is_empty() {
                let mut req = self.ec2_client.delete_tags().resources(ec2_resource_id);
                for key in removed_keys {
                    req = req.tags(aws_sdk_ec2::types::Tag::builder().key(key.as_str()).build());
                }
                req.send().await.map_err(|e| {
                    ProviderError::new(format!("Failed to delete tags: {:?}", e))
                        .for_resource(resource_id.clone())
                })?;
            }
        }

        // Add/update tags
        if let Some(tag_value) = attributes.get("tags") {
            let tags = Self::value_to_ec2_tags(tag_value);
            if !tags.is_empty() {
                let mut req = self.ec2_client.create_tags().resources(ec2_resource_id);
                for tag in tags {
                    req = req.tags(tag);
                }
                req.send().await.map_err(|e| {
                    ProviderError::new(format!("Failed to tag resource: {:?}", e))
                        .for_resource(resource_id.clone())
                })?;
            }
        }

        Ok(())
    }

    /// Read an EC2 Security Group Rule (shared between ingress and egress)
    async fn read_ec2_security_group_rule(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        // Look up by rule IDs (may be comma-separated)
        let rule_ids: Vec<&str> = identifier.split(',').collect();
        let mut req = self.ec2_client.describe_security_group_rules();
        for rule_id in &rule_ids {
            req = req.security_group_rule_ids(*rule_id);
        }
        let result = req.send().await.map_err(|e| {
            ProviderError::new(format!("Failed to describe security group rules: {:?}", e))
                .for_resource(id.clone())
        })?;
        let rules: Vec<_> = result
            .security_group_rules()
            .iter()
            .filter(|rule| rule.is_egress() == Some(!is_ingress))
            .cloned()
            .collect();

        if rules.is_empty() {
            return Ok(State::not_found(id.clone()));
        }

        // Use the first rule for common attributes
        let first_rule = &rules[0];
        let mut attributes = HashMap::new();

        // Auto-generated attribute extraction (common fields)
        if is_ingress {
            Self::extract_ec2_security_group_ingress_attributes(first_rule, &mut attributes);
        } else {
            Self::extract_ec2_security_group_egress_attributes(first_rule, &mut attributes);
        }

        // Override rule IDs with comma-separated values (multi-rule support)
        let rule_ids: Vec<String> = rules
            .iter()
            .filter_map(|r| r.security_group_rule_id().map(String::from))
            .collect();
        let rule_identifier = if !rule_ids.is_empty() {
            attributes.insert(
                "security_group_rule_id".to_string(),
                Value::String(rule_ids.join(",")),
            );
            Some(rule_ids.join(","))
        } else {
            None
        };

        // IPv4 CIDR (CidrIp in schema maps to CidrIpv4 in SDK)
        if let Some(cidr_ip) = first_rule.cidr_ipv4() {
            attributes.insert("cidr_ip".to_string(), Value::String(cidr_ip.to_string()));
        }

        // Referenced security group ID (nested struct, not auto-extracted)
        if let Some(ref_group) = first_rule.referenced_group_info()
            && let Some(group_id) = ref_group.group_id()
        {
            let attr_name = if is_ingress {
                "source_security_group_id"
            } else {
                "destination_security_group_id"
            };
            attributes.insert(attr_name.to_string(), Value::String(group_id.to_string()));
        }

        let state = State::existing(id.clone(), attributes);
        Ok(if let Some(id_str) = rule_identifier {
            state.with_identifier(id_str)
        } else {
            state
        })
    }

    /// Create an EC2 Security Group Rule (shared between ingress and egress)
    async fn create_ec2_security_group_rule(
        &self,
        resource: Resource,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        let sg_id = match resource.attributes.get("group_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("Security Group ID (group_id) is required")
                        .for_resource(resource.id.clone()),
                );
            }
        };

        let protocol = match resource.attributes.get("ip_protocol") {
            Some(Value::String(s)) => convert_protocol_value(s),
            _ => "-1".to_string(),
        };

        let from_port = match resource.attributes.get("from_port") {
            Some(Value::Int(n)) => *n as i32,
            _ => 0,
        };

        let to_port = match resource.attributes.get("to_port") {
            Some(Value::Int(n)) => *n as i32,
            _ => 0,
        };

        let cidr_ip = match resource.attributes.get("cidr_ip") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let cidr_ipv6 = match resource.attributes.get("cidr_ipv6") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let description = match resource.attributes.get("description") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let prefix_list_attr = if is_ingress {
            "source_prefix_list_id"
        } else {
            "destination_prefix_list_id"
        };
        let prefix_list_id = match resource.attributes.get(prefix_list_attr) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let sg_ref_attr = if is_ingress {
            "source_security_group_id"
        } else {
            "destination_security_group_id"
        };
        let ref_security_group_id = match resource.attributes.get(sg_ref_attr) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };

        let mut permission_builder = aws_sdk_ec2::types::IpPermission::builder()
            .ip_protocol(&protocol)
            .from_port(from_port)
            .to_port(to_port);

        // IPv4 CIDR range
        if let Some(ref cidr) = cidr_ip {
            let mut range_builder = aws_sdk_ec2::types::IpRange::builder().cidr_ip(cidr);
            if let Some(ref desc) = description {
                range_builder = range_builder.description(desc);
            }
            permission_builder = permission_builder.ip_ranges(range_builder.build());
        }

        // IPv6 CIDR range
        if let Some(ref cidr_v6) = cidr_ipv6 {
            let mut range_builder = aws_sdk_ec2::types::Ipv6Range::builder().cidr_ipv6(cidr_v6);
            if let Some(ref desc) = description {
                range_builder = range_builder.description(desc);
            }
            permission_builder = permission_builder.ipv6_ranges(range_builder.build());
        }

        // Prefix list
        if let Some(ref pl_id) = prefix_list_id {
            let mut pl_builder = aws_sdk_ec2::types::PrefixListId::builder().prefix_list_id(pl_id);
            if let Some(ref desc) = description {
                pl_builder = pl_builder.description(desc);
            }
            permission_builder = permission_builder.prefix_list_ids(pl_builder.build());
        }

        // Security group reference
        if let Some(ref ref_sg_id) = ref_security_group_id {
            let mut pair_builder =
                aws_sdk_ec2::types::UserIdGroupPair::builder().group_id(ref_sg_id);
            if let Some(ref desc) = description {
                pair_builder = pair_builder.description(desc);
            }
            permission_builder = permission_builder.user_id_group_pairs(pair_builder.build());
        }

        let permission = permission_builder.build();

        let rule_ids: Vec<String> = if is_ingress {
            let result = self
                .ec2_client
                .authorize_security_group_ingress()
                .group_id(&sg_id)
                .ip_permissions(permission)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to create ingress rule: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;

            result
                .security_group_rules()
                .iter()
                .filter_map(|r| r.security_group_rule_id().map(String::from))
                .collect()
        } else {
            let result = self
                .ec2_client
                .authorize_security_group_egress()
                .group_id(&sg_id)
                .ip_permissions(permission)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to create egress rule: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;

            result
                .security_group_rules()
                .iter()
                .filter_map(|r| r.security_group_rule_id().map(String::from))
                .collect()
        };

        // Read back using rule IDs (reliable identifier)
        let identifier = rule_ids.join(",");
        self.read_ec2_security_group_rule(
            &resource.id,
            if identifier.is_empty() {
                None
            } else {
                Some(&identifier)
            },
            is_ingress,
        )
        .await
    }

    /// Update an EC2 Security Group Rule (rules are immutable, so recreate)
    async fn update_ec2_security_group_rule(
        &self,
        id: ResourceId,
        identifier: &str,
        to: Resource,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        // Security group rules are immutable - delete and recreate
        self.delete_ec2_security_group_rule(id.clone(), identifier, is_ingress)
            .await?;
        self.create_ec2_security_group_rule(to, is_ingress).await
    }

    /// Delete an EC2 Security Group Rule (deletes all rules by identifier)
    async fn delete_ec2_security_group_rule(
        &self,
        id: ResourceId,
        identifier: &str,
        is_ingress: bool,
    ) -> ProviderResult<()> {
        // identifier is comma-separated rule IDs (e.g., "sgr-123,sgr-456")
        let rule_ids: Vec<&str> = identifier.split(',').collect();

        // Look up the rules to get the security group ID
        let mut req = self.ec2_client.describe_security_group_rules();
        for rule_id in &rule_ids {
            req = req.security_group_rule_ids(*rule_id);
        }
        let result = req.send().await.map_err(|e| {
            ProviderError::new(format!("Failed to describe security group rules: {:?}", e))
                .for_resource(id.clone())
        })?;

        let rules = result.security_group_rules();
        if rules.is_empty() {
            return Err(
                ProviderError::new("Security Group Rule not found").for_resource(id.clone())
            );
        }

        let sg_id = rules[0].group_id().ok_or_else(|| {
            ProviderError::new("Rule has no security group ID").for_resource(id.clone())
        })?;

        // Delete all rules at once
        if is_ingress {
            let mut request = self
                .ec2_client
                .revoke_security_group_ingress()
                .group_id(sg_id);
            for rule_id in &rule_ids {
                request = request.security_group_rule_ids(*rule_id);
            }
            request.send().await.map_err(|e| {
                ProviderError::new(format!("Failed to delete ingress rules: {:?}", e))
                    .for_resource(id.clone())
            })?;
        } else {
            let mut request = self
                .ec2_client
                .revoke_security_group_egress()
                .group_id(sg_id);
            for rule_id in &rule_ids {
                request = request.security_group_rule_ids(*rule_id);
            }
            request.send().await.map_err(|e| {
                ProviderError::new(format!("Failed to delete egress rules: {:?}", e))
                    .for_resource(id.clone())
            })?;
        }

        Ok(())
    }
}

impl Provider for AwsProvider {
    fn name(&self) -> &'static str {
        "aws"
    }

    fn resource_types(&self) -> Vec<Box<dyn ResourceType>> {
        provider_generated::resource_types()
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
                "ec2.vpc" => self.read_ec2_vpc(&id, identifier.as_deref()).await,
                "ec2.subnet" => self.read_ec2_subnet(&id, identifier.as_deref()).await,
                "ec2.internet_gateway" => {
                    self.read_ec2_internet_gateway(&id, identifier.as_deref())
                        .await
                }
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
                "ec2.vpc" => self.create_ec2_vpc(resource).await,
                "ec2.subnet" => self.create_ec2_subnet(resource).await,
                "ec2.internet_gateway" => self.create_ec2_internet_gateway(resource).await,
                "ec2.route_table" => self.create_ec2_route_table(resource).await,
                "ec2.route" => self.create_ec2_route(resource).await,
                "ec2.security_group" => self.create_ec2_security_group(resource).await,
                "ec2.security_group_ingress" => {
                    self.create_ec2_security_group_ingress(resource).await
                }
                "ec2.security_group_egress" => {
                    self.create_ec2_security_group_egress(resource).await
                }
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
                "ec2.vpc" => self.update_ec2_vpc(id, &identifier, &from, to).await,
                "ec2.subnet" => self.update_ec2_subnet(id, &identifier, &from, to).await,
                "ec2.internet_gateway" => {
                    self.update_ec2_internet_gateway(id, &identifier, &from, to)
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
                _ => Err(ProviderError::new(format!(
                    "Unknown resource type: {}",
                    id.resource_type
                ))
                .for_resource(id.clone())),
            }
        })
    }

    fn resolve_enum_identifiers(&self, resources: &mut [Resource]) {
        resolve_enum_identifiers_impl(resources);
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        _lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "s3.bucket" => self.delete_s3_bucket(id, &identifier).await,
                "ec2.vpc" => self.delete_ec2_vpc(id, &identifier).await,
                "ec2.subnet" => self.delete_ec2_subnet(id, &identifier).await,
                "ec2.internet_gateway" => self.delete_ec2_internet_gateway(id, &identifier).await,
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
                _ => Err(ProviderError::new(format!(
                    "Unknown resource type: {}",
                    id.resource_type
                ))
                .for_resource(id.clone())),
            }
        })
    }
}

/// Resolve enum identifiers in resources to their fully-qualified DSL format.
///
/// For example, resolves bare `Enabled` or `VersioningStatus.Enabled` into
/// `aws.s3.bucket.VersioningStatus.Enabled` based on schema definitions.
fn resolve_enum_identifiers_impl(resources: &mut [Resource]) {
    let configs = schemas::generated::configs();

    for resource in resources.iter_mut() {
        // Only handle aws resources
        let is_aws = matches!(
            resource.attributes.get("_provider"),
            Some(Value::String(p)) if p == "aws"
        );
        if !is_aws {
            continue;
        }

        // Find the matching schema config
        let config = configs.iter().find(|c| {
            c.schema
                .resource_type
                .strip_prefix("aws.")
                .map(|t| t == resource.id.resource_type)
                .unwrap_or(false)
        });
        let config = match config {
            Some(c) => c,
            None => continue,
        };

        // Resolve enum attributes
        let mut resolved_attrs = HashMap::new();
        for (key, value) in &resource.attributes {
            if let Some(attr_schema) = config.schema.attributes.get(key.as_str())
                && let Some((type_name, ns, to_dsl)) = attr_schema.attr_type.namespaced_enum_parts()
            {
                let resolved = match value {
                    Value::UnresolvedIdent(ident, None) => {
                        // bare identifier: Enabled → aws.s3.bucket.VersioningStatus.Enabled
                        let dsl_val = to_dsl.map_or_else(|| ident.clone(), |f| f(ident));
                        Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                    }
                    Value::UnresolvedIdent(ident, Some(member)) if ident == type_name => {
                        // TypeName.value: VersioningStatus.Enabled → aws.s3.bucket.VersioningStatus.Enabled
                        let dsl_val = to_dsl.map_or_else(|| member.clone(), |f| f(member));
                        Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                    }
                    Value::String(s) if !s.contains('.') => {
                        // plain string without dots: "Enabled" → aws.s3.bucket.VersioningStatus.Enabled
                        let dsl_val = to_dsl.map_or_else(|| s.clone(), |f| f(s));
                        Value::String(format!("{}.{}.{}", ns, type_name, dsl_val))
                    }
                    _ => value.clone(),
                };
                resolved_attrs.insert(key.clone(), resolved);
            }
        }

        for (key, value) in resolved_attrs {
            resource.attributes.insert(key, value);
        }
    }
}

/// Normalize enum values in read-returned state attributes to namespaced DSL format.
///
/// Read methods return plain values like `"Enabled"` from AWS APIs.
/// This converts them to namespaced format like `aws.s3.bucket.VersioningStatus.Enabled`
/// to match the resolved DSL values.
fn normalize_state_enums(resource_type: &str, attributes: &mut HashMap<String, Value>) {
    let configs = schemas::generated::configs();
    let config = configs.iter().find(|c| {
        c.schema
            .resource_type
            .strip_prefix("aws.")
            .map(|t| t == resource_type)
            .unwrap_or(false)
    });
    let config = match config {
        Some(c) => c,
        None => return,
    };

    let mut resolved = HashMap::new();
    for (key, value) in attributes.iter() {
        if let Some(attr_schema) = config.schema.attributes.get(key.as_str())
            && let Some((type_name, ns, to_dsl)) = attr_schema.attr_type.namespaced_enum_parts()
            && let Value::String(s) = value
            && !s.contains('.')
        {
            let dsl_val = to_dsl.map_or_else(|| s.clone(), |f| f(s));
            let namespaced = format!("{}.{}.{}", ns, type_name, dsl_val);
            resolved.insert(key.clone(), Value::String(namespaced));
        }
    }

    for (key, value) in resolved {
        attributes.insert(key, value);
    }
}

/// Convert protocol value from DSL format to AWS format
/// - aws.Protocol.tcp / Protocol.tcp / tcp -> tcp
/// - aws.Protocol.all / Protocol.all / all / -1 -> -1
fn convert_protocol_value(value: &str) -> String {
    // First convert DSL enum format to raw value
    let raw = convert_enum_value(value);

    // Handle special case: "all" means "-1" (all protocols)
    if raw == "all" { "-1".to_string() } else { raw }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_s3_bucket_type_name() {
        let bucket_type = provider_generated::S3BucketType;
        assert_eq!(bucket_type.name(), "s3.bucket");
    }

    #[test]
    fn test_resolve_enum_identifiers_namespaced_value() {
        let mut resource = Resource::new("s3.bucket", "test-bucket");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("aws".to_string()));
        resource.attributes.insert(
            "versioning_status".to_string(),
            Value::String("aws.s3.bucket.VersioningStatus.Enabled".to_string()),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        assert_eq!(
            resources[0].attributes.get("versioning_status"),
            Some(&Value::String(
                "aws.s3.bucket.VersioningStatus.Enabled".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_bare_ident() {
        let mut resource = Resource::new("s3.bucket", "test-bucket");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("aws".to_string()));
        resource.attributes.insert(
            "versioning_status".to_string(),
            Value::UnresolvedIdent("Enabled".to_string(), None),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        assert_eq!(
            resources[0].attributes.get("versioning_status"),
            Some(&Value::String(
                "aws.s3.bucket.VersioningStatus.Enabled".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_typename_value() {
        let mut resource = Resource::new("s3.bucket", "test-bucket");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("aws".to_string()));
        resource.attributes.insert(
            "object_ownership".to_string(),
            Value::UnresolvedIdent(
                "ObjectOwnership".to_string(),
                Some("BucketOwnerEnforced".to_string()),
            ),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        assert_eq!(
            resources[0].attributes.get("object_ownership"),
            Some(&Value::String(
                "aws.s3.bucket.ObjectOwnership.BucketOwnerEnforced".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_plain_string() {
        let mut resource = Resource::new("s3.bucket", "test-bucket");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("aws".to_string()));
        resource.attributes.insert(
            "versioning_status".to_string(),
            Value::String("Enabled".to_string()),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        assert_eq!(
            resources[0].attributes.get("versioning_status"),
            Some(&Value::String(
                "aws.s3.bucket.VersioningStatus.Enabled".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_skips_non_aws() {
        let mut resource = Resource::new("s3.bucket", "test");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("awscc".to_string()));
        resource.attributes.insert(
            "versioning_status".to_string(),
            Value::String("Enabled".to_string()),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        // Should not be modified since provider is "awscc"
        assert_eq!(
            resources[0].attributes.get("versioning_status"),
            Some(&Value::String("Enabled".to_string()))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_with_to_dsl() {
        // ip_protocol has to_dsl that maps "-1" → "all"
        let mut resource = Resource::new("ec2.security_group_ingress", "test-rule");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("aws".to_string()));
        resource
            .attributes
            .insert("ip_protocol".to_string(), Value::String("-1".to_string()));
        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        assert_eq!(
            resources[0].attributes.get("ip_protocol"),
            Some(&Value::String(
                "aws.ec2.security_group_ingress.IpProtocol.all".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums_with_to_dsl() {
        // Read returns "-1" for ip_protocol, should be normalized to "all" via to_dsl
        let mut attributes =
            HashMap::from([("ip_protocol".to_string(), Value::String("-1".to_string()))]);
        normalize_state_enums("ec2.security_group_ingress", &mut attributes);
        assert_eq!(
            attributes.get("ip_protocol"),
            Some(&Value::String(
                "aws.ec2.security_group_ingress.IpProtocol.all".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums() {
        let mut attributes = HashMap::from([
            ("bucket".to_string(), Value::String("my-bucket".to_string())),
            (
                "versioning_status".to_string(),
                Value::String("Enabled".to_string()),
            ),
            (
                "object_ownership".to_string(),
                Value::String("BucketOwnerEnforced".to_string()),
            ),
        ]);
        normalize_state_enums("s3.bucket", &mut attributes);
        assert_eq!(
            attributes.get("versioning_status"),
            Some(&Value::String(
                "aws.s3.bucket.VersioningStatus.Enabled".to_string()
            ))
        );
        assert_eq!(
            attributes.get("object_ownership"),
            Some(&Value::String(
                "aws.s3.bucket.ObjectOwnership.BucketOwnerEnforced".to_string()
            ))
        );
        // Non-enum attributes should not be modified
        assert_eq!(
            attributes.get("bucket"),
            Some(&Value::String("my-bucket".to_string()))
        );
    }

    #[test]
    fn test_normalize_state_enums_already_namespaced() {
        let mut attributes = HashMap::from([(
            "versioning_status".to_string(),
            Value::String("aws.s3.bucket.VersioningStatus.Enabled".to_string()),
        )]);
        normalize_state_enums("s3.bucket", &mut attributes);
        // Already namespaced values (contain dots) should not be modified
        assert_eq!(
            attributes.get("versioning_status"),
            Some(&Value::String(
                "aws.s3.bucket.VersioningStatus.Enabled".to_string()
            ))
        );
    }

    // --- convert_protocol_value tests ---

    #[test]
    fn test_convert_protocol_value_tcp() {
        assert_eq!(convert_protocol_value("tcp"), "tcp");
    }

    #[test]
    fn test_convert_protocol_value_udp() {
        assert_eq!(convert_protocol_value("udp"), "udp");
    }

    #[test]
    fn test_convert_protocol_value_all_keyword() {
        assert_eq!(convert_protocol_value("all"), "-1");
    }

    #[test]
    fn test_convert_protocol_value_minus_one() {
        assert_eq!(convert_protocol_value("-1"), "-1");
    }

    #[test]
    fn test_convert_protocol_value_dsl_format_tcp() {
        assert_eq!(convert_protocol_value("aws.Protocol.tcp"), "tcp");
    }

    #[test]
    fn test_convert_protocol_value_dsl_format_all() {
        assert_eq!(convert_protocol_value("aws.Protocol.all"), "-1");
    }

    #[test]
    fn test_convert_protocol_value_short_dsl_format() {
        assert_eq!(convert_protocol_value("Protocol.tcp"), "tcp");
    }

    // --- ec2_tags_to_value tests ---

    #[test]
    fn test_ec2_tags_to_value_empty() {
        let tags: Vec<aws_sdk_ec2::types::Tag> = vec![];
        assert_eq!(AwsProvider::ec2_tags_to_value(&tags), None);
    }

    #[test]
    fn test_ec2_tags_to_value_single_tag() {
        let tags = vec![
            aws_sdk_ec2::types::Tag::builder()
                .key("Name")
                .value("my-resource")
                .build(),
        ];
        let result = AwsProvider::ec2_tags_to_value(&tags);
        assert!(result.is_some());
        if let Some(Value::Map(map)) = result {
            assert_eq!(
                map.get("Name"),
                Some(&Value::String("my-resource".to_string()))
            );
        } else {
            panic!("Expected Value::Map");
        }
    }

    #[test]
    fn test_ec2_tags_to_value_multiple_tags() {
        let tags = vec![
            aws_sdk_ec2::types::Tag::builder()
                .key("Name")
                .value("test")
                .build(),
            aws_sdk_ec2::types::Tag::builder()
                .key("Environment")
                .value("production")
                .build(),
        ];
        let result = AwsProvider::ec2_tags_to_value(&tags);
        if let Some(Value::Map(map)) = result {
            assert_eq!(map.len(), 2);
            assert_eq!(map.get("Name"), Some(&Value::String("test".to_string())));
            assert_eq!(
                map.get("Environment"),
                Some(&Value::String("production".to_string()))
            );
        } else {
            panic!("Expected Value::Map with 2 entries");
        }
    }

    #[test]
    fn test_ec2_tags_to_value_missing_key_or_value() {
        // Tag with no key set
        let tags = vec![aws_sdk_ec2::types::Tag::builder().build()];
        assert_eq!(AwsProvider::ec2_tags_to_value(&tags), None);
    }

    // --- value_to_ec2_tags tests ---

    #[test]
    fn test_value_to_ec2_tags_from_map() {
        let value = Value::Map(HashMap::from([
            ("Name".to_string(), Value::String("test".to_string())),
            ("Env".to_string(), Value::String("prod".to_string())),
        ]));
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert_eq!(tags.len(), 2);
        // Check both tags exist (order not guaranteed from HashMap)
        let tag_map: HashMap<String, String> = tags
            .iter()
            .map(|t| {
                (
                    t.key().unwrap_or("").to_string(),
                    t.value().unwrap_or("").to_string(),
                )
            })
            .collect();
        assert_eq!(tag_map.get("Name"), Some(&"test".to_string()));
        assert_eq!(tag_map.get("Env"), Some(&"prod".to_string()));
    }

    #[test]
    fn test_value_to_ec2_tags_non_map_value() {
        let value = Value::String("not a map".to_string());
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert!(tags.is_empty());
    }

    #[test]
    fn test_value_to_ec2_tags_non_string_values_skipped() {
        let value = Value::Map(HashMap::from([
            ("Name".to_string(), Value::String("test".to_string())),
            ("Count".to_string(), Value::Int(42)),
        ]));
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].key(), Some("Name"));
        assert_eq!(tags[0].value(), Some("test"));
    }

    #[test]
    fn test_value_to_ec2_tags_empty_map() {
        let value = Value::Map(HashMap::new());
        let tags = AwsProvider::value_to_ec2_tags(&value);
        assert!(tags.is_empty());
    }

    // --- extract_ec2_vpc_attributes tests ---

    #[test]
    fn test_extract_ec2_vpc_attributes() {
        let vpc = aws_sdk_ec2::types::Vpc::builder()
            .vpc_id("vpc-12345678")
            .cidr_block("10.0.0.0/16")
            .instance_tenancy(aws_sdk_ec2::types::Tenancy::Default)
            .build();
        let mut attributes = HashMap::new();
        let identifier = AwsProvider::extract_ec2_vpc_attributes(&vpc, &mut attributes);
        assert_eq!(identifier, Some("vpc-12345678".to_string()));
        assert_eq!(
            attributes.get("vpc_id"),
            Some(&Value::String("vpc-12345678".to_string()))
        );
        assert_eq!(
            attributes.get("cidr_block"),
            Some(&Value::String("10.0.0.0/16".to_string()))
        );
        assert_eq!(
            attributes.get("instance_tenancy"),
            Some(&Value::String("default".to_string()))
        );
    }

    #[test]
    fn test_extract_ec2_vpc_attributes_minimal() {
        let vpc = aws_sdk_ec2::types::Vpc::builder().build();
        let mut attributes = HashMap::new();
        let identifier = AwsProvider::extract_ec2_vpc_attributes(&vpc, &mut attributes);
        assert_eq!(identifier, None);
        assert!(attributes.is_empty());
    }

    // --- extract_ec2_subnet_attributes tests ---

    #[test]
    fn test_extract_ec2_subnet_attributes() {
        let subnet = aws_sdk_ec2::types::Subnet::builder()
            .subnet_id("subnet-12345678")
            .vpc_id("vpc-12345678")
            .cidr_block("10.0.1.0/24")
            .availability_zone("ap-northeast-1a")
            .map_public_ip_on_launch(false)
            .build();
        let mut attributes = HashMap::new();
        let identifier = AwsProvider::extract_ec2_subnet_attributes(&subnet, &mut attributes);
        assert_eq!(identifier, Some("subnet-12345678".to_string()));
        assert_eq!(
            attributes.get("subnet_id"),
            Some(&Value::String("subnet-12345678".to_string()))
        );
        assert_eq!(
            attributes.get("vpc_id"),
            Some(&Value::String("vpc-12345678".to_string()))
        );
        assert_eq!(
            attributes.get("cidr_block"),
            Some(&Value::String("10.0.1.0/24".to_string()))
        );
        assert_eq!(
            attributes.get("availability_zone"),
            Some(&Value::String("ap-northeast-1a".to_string()))
        );
        assert_eq!(
            attributes.get("map_public_ip_on_launch"),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    fn test_extract_ec2_subnet_attributes_minimal() {
        let subnet = aws_sdk_ec2::types::Subnet::builder().build();
        let mut attributes = HashMap::new();
        let identifier = AwsProvider::extract_ec2_subnet_attributes(&subnet, &mut attributes);
        assert_eq!(identifier, None);
    }

    // --- extract_ec2_internet_gateway_attributes tests ---

    #[test]
    fn test_extract_ec2_internet_gateway_attributes() {
        let igw = aws_sdk_ec2::types::InternetGateway::builder()
            .internet_gateway_id("igw-12345678")
            .build();
        let mut attributes = HashMap::new();
        let identifier =
            AwsProvider::extract_ec2_internet_gateway_attributes(&igw, &mut attributes);
        assert_eq!(identifier, Some("igw-12345678".to_string()));
        assert_eq!(
            attributes.get("internet_gateway_id"),
            Some(&Value::String("igw-12345678".to_string()))
        );
    }

    #[test]
    fn test_extract_ec2_internet_gateway_attributes_minimal() {
        let igw = aws_sdk_ec2::types::InternetGateway::builder().build();
        let mut attributes = HashMap::new();
        let identifier =
            AwsProvider::extract_ec2_internet_gateway_attributes(&igw, &mut attributes);
        assert_eq!(identifier, None);
        assert!(attributes.is_empty());
    }

    // --- extract_ec2_route_table_attributes tests ---

    #[test]
    fn test_extract_ec2_route_table_attributes() {
        let rt = aws_sdk_ec2::types::RouteTable::builder()
            .route_table_id("rtb-12345678")
            .vpc_id("vpc-12345678")
            .build();
        let mut attributes = HashMap::new();
        let identifier = AwsProvider::extract_ec2_route_table_attributes(&rt, &mut attributes);
        assert_eq!(identifier, Some("rtb-12345678".to_string()));
        assert_eq!(
            attributes.get("route_table_id"),
            Some(&Value::String("rtb-12345678".to_string()))
        );
        assert_eq!(
            attributes.get("vpc_id"),
            Some(&Value::String("vpc-12345678".to_string()))
        );
    }

    #[test]
    fn test_extract_ec2_route_table_attributes_minimal() {
        let rt = aws_sdk_ec2::types::RouteTable::builder().build();
        let mut attributes = HashMap::new();
        let identifier = AwsProvider::extract_ec2_route_table_attributes(&rt, &mut attributes);
        assert_eq!(identifier, None);
    }

    // --- extract_ec2_route_attributes tests ---

    #[test]
    fn test_extract_ec2_route_attributes() {
        let route = aws_sdk_ec2::types::Route::builder()
            .destination_cidr_block("0.0.0.0/0")
            .gateway_id("igw-12345678")
            .build();
        let mut attributes = HashMap::new();
        let identifier = AwsProvider::extract_ec2_route_attributes(&route, &mut attributes);
        // Route extraction returns None (no single identifier)
        assert_eq!(identifier, None);
        assert_eq!(
            attributes.get("destination_cidr_block"),
            Some(&Value::String("0.0.0.0/0".to_string()))
        );
        assert_eq!(
            attributes.get("gateway_id"),
            Some(&Value::String("igw-12345678".to_string()))
        );
    }

    #[test]
    fn test_extract_ec2_route_attributes_with_nat_gateway() {
        let route = aws_sdk_ec2::types::Route::builder()
            .destination_cidr_block("10.0.0.0/8")
            .nat_gateway_id("nat-12345678")
            .build();
        let mut attributes = HashMap::new();
        AwsProvider::extract_ec2_route_attributes(&route, &mut attributes);
        assert_eq!(
            attributes.get("destination_cidr_block"),
            Some(&Value::String("10.0.0.0/8".to_string()))
        );
        assert_eq!(
            attributes.get("nat_gateway_id"),
            Some(&Value::String("nat-12345678".to_string()))
        );
    }

    #[test]
    fn test_extract_ec2_route_attributes_with_transit_gateway() {
        let route = aws_sdk_ec2::types::Route::builder()
            .destination_cidr_block("172.16.0.0/12")
            .transit_gateway_id("tgw-12345678")
            .build();
        let mut attributes = HashMap::new();
        AwsProvider::extract_ec2_route_attributes(&route, &mut attributes);
        assert_eq!(
            attributes.get("transit_gateway_id"),
            Some(&Value::String("tgw-12345678".to_string()))
        );
    }

    // --- extract_ec2_security_group_attributes tests ---

    #[test]
    fn test_extract_ec2_security_group_attributes() {
        let sg = aws_sdk_ec2::types::SecurityGroup::builder()
            .group_id("sg-12345678")
            .group_name("test-sg")
            .description("Test security group")
            .vpc_id("vpc-12345678")
            .build();
        let mut attributes = HashMap::new();
        let identifier = AwsProvider::extract_ec2_security_group_attributes(&sg, &mut attributes);
        assert_eq!(identifier, Some("sg-12345678".to_string()));
        assert_eq!(
            attributes.get("group_id"),
            Some(&Value::String("sg-12345678".to_string()))
        );
        assert_eq!(
            attributes.get("group_name"),
            Some(&Value::String("test-sg".to_string()))
        );
        assert_eq!(
            attributes.get("description"),
            Some(&Value::String("Test security group".to_string()))
        );
        assert_eq!(
            attributes.get("vpc_id"),
            Some(&Value::String("vpc-12345678".to_string()))
        );
    }

    #[test]
    fn test_extract_ec2_security_group_attributes_minimal() {
        let sg = aws_sdk_ec2::types::SecurityGroup::builder().build();
        let mut attributes = HashMap::new();
        let identifier = AwsProvider::extract_ec2_security_group_attributes(&sg, &mut attributes);
        assert_eq!(identifier, None);
    }

    // --- extract_ec2_security_group_ingress_attributes tests ---

    #[test]
    fn test_extract_ec2_security_group_ingress_attributes() {
        let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-12345678")
            .group_id("sg-12345678")
            .ip_protocol("tcp")
            .from_port(443)
            .to_port(443)
            .description("HTTPS")
            .build();
        let mut attributes = HashMap::new();
        let identifier =
            AwsProvider::extract_ec2_security_group_ingress_attributes(&rule, &mut attributes);
        assert_eq!(identifier, Some("sgr-12345678".to_string()));
        assert_eq!(
            attributes.get("security_group_rule_id"),
            Some(&Value::String("sgr-12345678".to_string()))
        );
        assert_eq!(
            attributes.get("group_id"),
            Some(&Value::String("sg-12345678".to_string()))
        );
        assert_eq!(
            attributes.get("ip_protocol"),
            Some(&Value::String("tcp".to_string()))
        );
        assert_eq!(attributes.get("from_port"), Some(&Value::Int(443)));
        assert_eq!(attributes.get("to_port"), Some(&Value::Int(443)));
        assert_eq!(
            attributes.get("description"),
            Some(&Value::String("HTTPS".to_string()))
        );
    }

    #[test]
    fn test_extract_ec2_security_group_ingress_attributes_with_prefix_list() {
        let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-99999999")
            .group_id("sg-12345678")
            .ip_protocol("tcp")
            .from_port(80)
            .to_port(80)
            .prefix_list_id("pl-12345678")
            .build();
        let mut attributes = HashMap::new();
        AwsProvider::extract_ec2_security_group_ingress_attributes(&rule, &mut attributes);
        assert_eq!(
            attributes.get("source_prefix_list_id"),
            Some(&Value::String("pl-12345678".to_string()))
        );
    }

    // --- extract_ec2_security_group_egress_attributes tests ---

    #[test]
    fn test_extract_ec2_security_group_egress_attributes() {
        let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-87654321")
            .group_id("sg-12345678")
            .ip_protocol("-1")
            .from_port(0)
            .to_port(0)
            .build();
        let mut attributes = HashMap::new();
        let identifier =
            AwsProvider::extract_ec2_security_group_egress_attributes(&rule, &mut attributes);
        assert_eq!(identifier, Some("sgr-87654321".to_string()));
        assert_eq!(
            attributes.get("group_id"),
            Some(&Value::String("sg-12345678".to_string()))
        );
        assert_eq!(
            attributes.get("ip_protocol"),
            Some(&Value::String("-1".to_string()))
        );
        assert_eq!(attributes.get("from_port"), Some(&Value::Int(0)));
        assert_eq!(attributes.get("to_port"), Some(&Value::Int(0)));
    }

    #[test]
    fn test_extract_ec2_security_group_egress_attributes_with_prefix_list() {
        let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-11111111")
            .group_id("sg-12345678")
            .ip_protocol("tcp")
            .from_port(443)
            .to_port(443)
            .prefix_list_id("pl-87654321")
            .build();
        let mut attributes = HashMap::new();
        AwsProvider::extract_ec2_security_group_egress_attributes(&rule, &mut attributes);
        assert_eq!(
            attributes.get("destination_prefix_list_id"),
            Some(&Value::String("pl-87654321".to_string()))
        );
    }

    #[test]
    fn test_extract_ec2_security_group_egress_attributes_with_ipv6() {
        let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-22222222")
            .group_id("sg-12345678")
            .ip_protocol("-1")
            .from_port(0)
            .to_port(0)
            .cidr_ipv6("::/0")
            .build();
        let mut attributes = HashMap::new();
        AwsProvider::extract_ec2_security_group_egress_attributes(&rule, &mut attributes);
        assert_eq!(
            attributes.get("cidr_ipv6"),
            Some(&Value::String("::/0".to_string()))
        );
    }

    // --- resolve_enum_identifiers for EC2 types ---

    #[test]
    fn test_resolve_enum_identifiers_ec2_vpc_instance_tenancy() {
        let mut resource = Resource::new("ec2.vpc", "test-vpc");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("aws".to_string()));
        resource.attributes.insert(
            "instance_tenancy".to_string(),
            Value::UnresolvedIdent("InstanceTenancy".to_string(), Some("dedicated".to_string())),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        assert_eq!(
            resources[0].attributes.get("instance_tenancy"),
            Some(&Value::String(
                "aws.ec2.vpc.InstanceTenancy.dedicated".to_string()
            ))
        );
    }

    #[test]
    fn test_resolve_enum_identifiers_ec2_security_group_ingress_protocol() {
        let mut resource = Resource::new("ec2.security_group_ingress", "test-rule");
        resource
            .attributes
            .insert("_provider".to_string(), Value::String("aws".to_string()));
        resource.attributes.insert(
            "ip_protocol".to_string(),
            Value::UnresolvedIdent("IpProtocol".to_string(), Some("tcp".to_string())),
        );
        let mut resources = vec![resource];
        resolve_enum_identifiers_impl(&mut resources);
        assert_eq!(
            resources[0].attributes.get("ip_protocol"),
            Some(&Value::String(
                "aws.ec2.security_group_ingress.IpProtocol.tcp".to_string()
            ))
        );
    }

    // --- normalize_state_enums for EC2 types ---

    #[test]
    fn test_normalize_state_enums_ec2_vpc_tenancy() {
        let mut attributes = HashMap::from([(
            "instance_tenancy".to_string(),
            Value::String("default".to_string()),
        )]);
        normalize_state_enums("ec2.vpc", &mut attributes);
        assert_eq!(
            attributes.get("instance_tenancy"),
            Some(&Value::String(
                "aws.ec2.vpc.InstanceTenancy.default".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums_ec2_security_group_egress() {
        let mut attributes =
            HashMap::from([("ip_protocol".to_string(), Value::String("-1".to_string()))]);
        normalize_state_enums("ec2.security_group_egress", &mut attributes);
        assert_eq!(
            attributes.get("ip_protocol"),
            Some(&Value::String(
                "aws.ec2.security_group_egress.IpProtocol.all".to_string()
            ))
        );
    }

    #[test]
    fn test_normalize_state_enums_ec2_security_group_egress_tcp() {
        let mut attributes =
            HashMap::from([("ip_protocol".to_string(), Value::String("tcp".to_string()))]);
        normalize_state_enums("ec2.security_group_egress", &mut attributes);
        assert_eq!(
            attributes.get("ip_protocol"),
            Some(&Value::String(
                "aws.ec2.security_group_egress.IpProtocol.tcp".to_string()
            ))
        );
    }

    // --- Route composite identifier parsing tests ---

    #[test]
    fn test_route_identifier_parsing() {
        let identifier = "rtb-12345678|0.0.0.0/0";
        let (route_table_id, destination) = identifier.split_once('|').unwrap();
        assert_eq!(route_table_id, "rtb-12345678");
        assert_eq!(destination, "0.0.0.0/0");
    }

    #[test]
    fn test_route_identifier_parsing_no_separator() {
        let identifier = "rtb-12345678";
        assert_eq!(identifier.split_once('|'), None);
    }

    #[test]
    fn test_route_identifier_parsing_ipv6_destination() {
        let identifier = "rtb-12345678|::/0";
        let (route_table_id, destination) = identifier.split_once('|').unwrap();
        assert_eq!(route_table_id, "rtb-12345678");
        assert_eq!(destination, "::/0");
    }

    // --- EC2 route table route extraction from describe response ---

    #[test]
    fn test_route_table_routes_extraction() {
        // Simulates the route extraction logic in read_ec2_route_table
        let route1 = aws_sdk_ec2::types::Route::builder()
            .destination_cidr_block("10.0.0.0/16")
            .gateway_id("local")
            .build();
        let route2 = aws_sdk_ec2::types::Route::builder()
            .destination_cidr_block("0.0.0.0/0")
            .gateway_id("igw-12345678")
            .build();

        let rt = aws_sdk_ec2::types::RouteTable::builder()
            .route_table_id("rtb-12345678")
            .vpc_id("vpc-12345678")
            .routes(route1)
            .routes(route2)
            .build();

        // Replicate route extraction logic from read_ec2_route_table
        let mut routes_list = Vec::new();
        for route in rt.routes() {
            let mut route_map = HashMap::new();
            if let Some(dest) = route.destination_cidr_block() {
                route_map.insert("destination".to_string(), Value::String(dest.to_string()));
            }
            if let Some(gw) = route.gateway_id() {
                route_map.insert("gateway_id".to_string(), Value::String(gw.to_string()));
            }
            if !route_map.is_empty() {
                routes_list.push(Value::Map(route_map));
            }
        }

        assert_eq!(routes_list.len(), 2);
        if let Value::Map(ref map) = routes_list[0] {
            assert_eq!(
                map.get("destination"),
                Some(&Value::String("10.0.0.0/16".to_string()))
            );
            assert_eq!(
                map.get("gateway_id"),
                Some(&Value::String("local".to_string()))
            );
        }
        if let Value::Map(ref map) = routes_list[1] {
            assert_eq!(
                map.get("destination"),
                Some(&Value::String("0.0.0.0/0".to_string()))
            );
            assert_eq!(
                map.get("gateway_id"),
                Some(&Value::String("igw-12345678".to_string()))
            );
        }
    }

    #[test]
    fn test_route_table_routes_extraction_empty() {
        let rt = aws_sdk_ec2::types::RouteTable::builder()
            .route_table_id("rtb-12345678")
            .build();
        assert!(rt.routes().is_empty());
    }

    // --- Internet Gateway attachment extraction ---

    #[test]
    fn test_internet_gateway_attachment_extraction() {
        // Simulates the vpc_id extraction from IGW attachments
        let attachment = aws_sdk_ec2::types::InternetGatewayAttachment::builder()
            .vpc_id("vpc-12345678")
            .state(aws_sdk_ec2::types::AttachmentStatus::from("available"))
            .build();
        let igw = aws_sdk_ec2::types::InternetGateway::builder()
            .internet_gateway_id("igw-12345678")
            .attachments(attachment)
            .build();

        // Replicate logic from read_ec2_internet_gateway
        let mut attributes = HashMap::new();
        if let Some(att) = igw.attachments().first()
            && let Some(vpc_id) = att.vpc_id()
        {
            attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
        }

        assert_eq!(
            attributes.get("vpc_id"),
            Some(&Value::String("vpc-12345678".to_string()))
        );
    }

    #[test]
    fn test_internet_gateway_no_attachment() {
        let igw = aws_sdk_ec2::types::InternetGateway::builder()
            .internet_gateway_id("igw-12345678")
            .build();

        let mut attributes = HashMap::new();
        if let Some(att) = igw.attachments().first()
            && let Some(vpc_id) = att.vpc_id()
        {
            attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
        }

        assert!(!attributes.contains_key("vpc_id"));
    }

    // --- Subnet availability zone DSL format conversion ---

    #[test]
    fn test_subnet_availability_zone_dsl_format() {
        // Simulates the AZ conversion in read_ec2_subnet
        let az = "ap-northeast-1a";
        let az_dsl = format!("aws.AvailabilityZone.{}", az.replace('-', "_"));
        assert_eq!(az_dsl, "aws.AvailabilityZone.ap_northeast_1a");
    }

    #[test]
    fn test_subnet_availability_zone_dsl_format_us_east() {
        let az = "us-east-1b";
        let az_dsl = format!("aws.AvailabilityZone.{}", az.replace('-', "_"));
        assert_eq!(az_dsl, "aws.AvailabilityZone.us_east_1b");
    }

    // --- Security group rule referenced group extraction ---

    #[test]
    fn test_security_group_rule_referenced_group() {
        let ref_group = aws_sdk_ec2::types::ReferencedSecurityGroup::builder()
            .group_id("sg-ref-12345678")
            .build();
        let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-12345678")
            .group_id("sg-12345678")
            .ip_protocol("tcp")
            .from_port(443)
            .to_port(443)
            .referenced_group_info(ref_group)
            .build();

        // Replicate logic from read_ec2_security_group_rule for ingress
        let mut attributes = HashMap::new();
        if let Some(ref_g) = rule.referenced_group_info()
            && let Some(group_id) = ref_g.group_id()
        {
            attributes.insert(
                "source_security_group_id".to_string(),
                Value::String(group_id.to_string()),
            );
        }

        assert_eq!(
            attributes.get("source_security_group_id"),
            Some(&Value::String("sg-ref-12345678".to_string()))
        );
    }

    #[test]
    fn test_security_group_rule_cidr_ipv4() {
        let rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-12345678")
            .group_id("sg-12345678")
            .ip_protocol("tcp")
            .from_port(80)
            .to_port(80)
            .cidr_ipv4("10.0.0.0/8")
            .build();

        // Replicate logic from read_ec2_security_group_rule
        let mut attributes = HashMap::new();
        if let Some(cidr_ip) = rule.cidr_ipv4() {
            attributes.insert("cidr_ip".to_string(), Value::String(cidr_ip.to_string()));
        }

        assert_eq!(
            attributes.get("cidr_ip"),
            Some(&Value::String("10.0.0.0/8".to_string()))
        );
    }

    // --- Security group rule is_egress filtering ---

    #[test]
    fn test_security_group_rule_is_egress_filtering() {
        let ingress_rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-ingress")
            .is_egress(false)
            .build();
        let egress_rule = aws_sdk_ec2::types::SecurityGroupRule::builder()
            .security_group_rule_id("sgr-egress")
            .is_egress(true)
            .build();

        let rules = [ingress_rule, egress_rule];

        // Filter for ingress (is_ingress=true means is_egress should be false)
        let ingress_filtered: Vec<_> = rules
            .iter()
            .filter(|rule| rule.is_egress() == Some(false))
            .collect();
        assert_eq!(ingress_filtered.len(), 1);
        assert_eq!(
            ingress_filtered[0].security_group_rule_id(),
            Some("sgr-ingress")
        );

        // Filter for egress (is_ingress=false means is_egress should be true)
        let egress_filtered: Vec<_> = rules
            .iter()
            .filter(|rule| rule.is_egress() == Some(true))
            .collect();
        assert_eq!(egress_filtered.len(), 1);
        assert_eq!(
            egress_filtered[0].security_group_rule_id(),
            Some("sgr-egress")
        );
    }

    // --- Security group rule comma-separated identifiers ---

    #[test]
    fn test_security_group_rule_comma_separated_ids() {
        // Tests the comma-separated rule ID pattern used in multi-rule support
        let identifier = "sgr-111,sgr-222,sgr-333";
        let rule_ids: Vec<&str> = identifier.split(',').collect();
        assert_eq!(rule_ids.len(), 3);
        assert_eq!(rule_ids[0], "sgr-111");
        assert_eq!(rule_ids[1], "sgr-222");
        assert_eq!(rule_ids[2], "sgr-333");
    }

    #[test]
    fn test_security_group_rule_single_id() {
        let identifier = "sgr-111";
        let rule_ids: Vec<&str> = identifier.split(',').collect();
        assert_eq!(rule_ids.len(), 1);
        assert_eq!(rule_ids[0], "sgr-111");
    }
}
