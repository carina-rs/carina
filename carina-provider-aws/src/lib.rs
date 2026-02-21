//! Carina AWS Provider
//!
//! AWS Provider implementation

pub mod schemas;

use std::collections::HashMap;

use aws_config::Region;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_s3::Client as S3Client;
use carina_core::provider::{
    BoxFuture, Provider, ProviderError, ProviderFactory, ProviderResult, ResourceSchema,
    ResourceType,
};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::utils::{convert_enum_value, extract_enum_value};

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

    fn extract_region_dsl(&self, attributes: &HashMap<String, Value>) -> Option<String> {
        if let Some(Value::String(region)) = attributes.get("region") {
            Some(region.clone())
        } else {
            None
        }
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
}

/// S3 Bucket resource type
pub struct S3BucketType;

impl ResourceType for S3BucketType {
    fn name(&self) -> &'static str {
        "s3_bucket"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// VPC resource type
pub struct VpcType;

impl ResourceType for VpcType {
    fn name(&self) -> &'static str {
        "ec2_vpc"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Subnet resource type
pub struct SubnetType;

impl ResourceType for SubnetType {
    fn name(&self) -> &'static str {
        "ec2_subnet"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Internet Gateway resource type
pub struct InternetGatewayType;

impl ResourceType for InternetGatewayType {
    fn name(&self) -> &'static str {
        "ec2_internet_gateway"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Route Table resource type
pub struct RouteTableType;

impl ResourceType for RouteTableType {
    fn name(&self) -> &'static str {
        "ec2_route_table"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Route resource type
pub struct RouteType;

impl ResourceType for RouteType {
    fn name(&self) -> &'static str {
        "ec2_route"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Security Group resource type
pub struct SecurityGroupType;

impl ResourceType for SecurityGroupType {
    fn name(&self) -> &'static str {
        "ec2_security_group"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Security Group Ingress Rule resource type
pub struct SecurityGroupIngressRuleType;

impl ResourceType for SecurityGroupIngressRuleType {
    fn name(&self) -> &'static str {
        "ec2_security_group_ingress"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Security Group Egress Rule resource type
pub struct SecurityGroupEgressRuleType;

impl ResourceType for SecurityGroupEgressRuleType {
    fn name(&self) -> &'static str {
        "ec2_security_group_egress"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// AWS Provider
pub struct AwsProvider {
    s3_client: S3Client,
    ec2_client: Ec2Client,
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
            region: region.to_string(),
        }
    }

    /// Create with specific clients (for testing)
    pub fn with_clients(s3_client: S3Client, ec2_client: Ec2Client, region: String) -> Self {
        Self {
            s3_client,
            ec2_client,
            region,
        }
    }

    /// Read an S3 bucket
    async fn read_s3_bucket(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        let Some(name) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        match self.s3_client.head_bucket().bucket(name).send().await {
            Ok(_) => {
                let mut attributes = HashMap::new();
                attributes.insert("name".to_string(), Value::String(name.to_string()));
                // Return region in DSL format
                let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
                attributes.insert("region".to_string(), Value::String(region_dsl));

                // Get versioning status
                if let Ok(versioning) = self
                    .s3_client
                    .get_bucket_versioning()
                    .bucket(name)
                    .send()
                    .await
                {
                    let status = versioning
                        .status()
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_else(|| "Suspended".to_string());
                    attributes.insert("versioning".to_string(), Value::String(status));
                }

                // Get lifecycle configuration
                if let Ok(lifecycle) = self
                    .s3_client
                    .get_bucket_lifecycle_configuration()
                    .bucket(name)
                    .send()
                    .await
                {
                    for rule in lifecycle.rules() {
                        if rule.id() == Some("auto-expiration")
                            && let Some(expiration) = rule.expiration()
                            && let Some(days) = expiration.days
                        {
                            attributes
                                .insert("expiration_days".to_string(), Value::Int(days as i64));
                        }
                    }
                }

                // S3 bucket identifier is the bucket name
                Ok(State::existing(id.clone(), attributes).with_identifier(name))
            }
            Err(err) => {
                // Handle bucket not found
                use aws_sdk_s3::error::SdkError;

                let is_not_found = match &err {
                    SdkError::ServiceError(service_err) => {
                        // NotFound error or 301/403/404 status codes
                        // 403 is returned when bucket doesn't exist or is owned by another account
                        let status = service_err.raw().status().as_u16();
                        service_err.err().is_not_found()
                            || status == 301
                            || status == 403
                            || status == 404
                    }
                    _ => false,
                };

                if is_not_found {
                    Ok(State::not_found(id.clone()))
                } else {
                    Err(
                        ProviderError::new(format!("Failed to read bucket: {:?}", err))
                            .for_resource(id.clone()),
                    )
                }
            }
        }
    }

    /// Create an S3 bucket
    async fn create_s3_bucket(&self, resource: Resource) -> ProviderResult<State> {
        let bucket_name = match resource.attributes.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("Bucket name is required").for_resource(resource.id.clone())
                );
            }
        };

        // Get region (use Provider's region if not specified)
        let region = match resource.attributes.get("region") {
            Some(Value::String(s)) => {
                // Convert from aws.Region.ap_northeast_1 format to ap-northeast-1 format
                convert_enum_value(s)
            }
            _ => self.region.clone(),
        };

        // Create bucket
        let mut req = self.s3_client.create_bucket().bucket(&bucket_name);

        // Specify LocationConstraint for regions other than us-east-1
        if region != "us-east-1" {
            use aws_sdk_s3::types::{BucketLocationConstraint, CreateBucketConfiguration};
            let constraint = BucketLocationConstraint::from(region.as_str());
            let config = CreateBucketConfiguration::builder()
                .location_constraint(constraint)
                .build();
            req = req.create_bucket_configuration(config);
        }

        req.send().await.map_err(|e| {
            ProviderError::new(format!("Failed to create bucket: {:?}", e))
                .for_resource(resource.id.clone())
        })?;

        // Configure versioning
        if let Some(Value::String(status)) = resource.attributes.get("versioning") {
            use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};
            let normalized = extract_enum_value(status);
            let versioning_status = if normalized == "Enabled" {
                BucketVersioningStatus::Enabled
            } else {
                BucketVersioningStatus::Suspended
            };
            let config = VersioningConfiguration::builder()
                .status(versioning_status)
                .build();
            self.s3_client
                .put_bucket_versioning()
                .bucket(&bucket_name)
                .versioning_configuration(config)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to configure versioning: {}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        // Configure lifecycle rule (expiration_days)
        if let Some(Value::Int(days)) = resource.attributes.get("expiration_days") {
            use aws_sdk_s3::types::{
                BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration, LifecycleRule,
                LifecycleRuleFilter,
            };
            let expiration = LifecycleExpiration::builder().days(*days as i32).build();
            let filter = LifecycleRuleFilter::builder().prefix("").build();
            let rule = LifecycleRule::builder()
                .id("auto-expiration")
                .status(ExpirationStatus::Enabled)
                .filter(filter)
                .expiration(expiration)
                .build()
                .map_err(|e| {
                    ProviderError::new(format!("Failed to build lifecycle rule: {}", e))
                        .for_resource(resource.id.clone())
                })?;

            let config = BucketLifecycleConfiguration::builder()
                .rules(rule)
                .build()
                .map_err(|e| {
                    ProviderError::new(format!("Failed to build lifecycle config: {}", e))
                        .for_resource(resource.id.clone())
                })?;

            self.s3_client
                .put_bucket_lifecycle_configuration()
                .bucket(&bucket_name)
                .lifecycle_configuration(config)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to set lifecycle: {}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        // Return state after creation
        self.read_s3_bucket(&resource.id, Some(&bucket_name)).await
    }

    /// Update an S3 bucket
    async fn update_s3_bucket(
        &self,
        id: ResourceId,
        identifier: &str,
        to: Resource,
    ) -> ProviderResult<State> {
        let bucket_name = identifier.to_string();

        // Update versioning configuration
        if let Some(Value::String(status)) = to.attributes.get("versioning") {
            use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};
            let normalized = extract_enum_value(status);
            let versioning_status = if normalized == "Enabled" {
                BucketVersioningStatus::Enabled
            } else {
                BucketVersioningStatus::Suspended
            };
            let config = VersioningConfiguration::builder()
                .status(versioning_status)
                .build();
            self.s3_client
                .put_bucket_versioning()
                .bucket(&bucket_name)
                .versioning_configuration(config)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to update versioning: {}", e))
                        .for_resource(id.clone())
                })?;
        }

        // Update lifecycle rule (expiration_days)
        if let Some(Value::Int(days)) = to.attributes.get("expiration_days") {
            use aws_sdk_s3::types::{
                BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration, LifecycleRule,
                LifecycleRuleFilter,
            };
            let expiration = LifecycleExpiration::builder().days(*days as i32).build();
            let filter = LifecycleRuleFilter::builder().prefix("").build();
            let rule = LifecycleRule::builder()
                .id("auto-expiration")
                .status(ExpirationStatus::Enabled)
                .filter(filter)
                .expiration(expiration)
                .build()
                .map_err(|e| {
                    ProviderError::new(format!("Failed to build lifecycle rule: {}", e))
                        .for_resource(id.clone())
                })?;

            let config = BucketLifecycleConfiguration::builder()
                .rules(rule)
                .build()
                .map_err(|e| {
                    ProviderError::new(format!("Failed to build lifecycle config: {}", e))
                        .for_resource(id.clone())
                })?;

            self.s3_client
                .put_bucket_lifecycle_configuration()
                .bucket(&bucket_name)
                .lifecycle_configuration(config)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to set lifecycle: {}", e))
                        .for_resource(id.clone())
                })?;
        }

        self.read_s3_bucket(&id, Some(&bucket_name)).await
    }

    /// Delete an S3 bucket
    async fn delete_s3_bucket(&self, id: ResourceId, identifier: &str) -> ProviderResult<()> {
        self.s3_client
            .delete_bucket()
            .bucket(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to delete bucket: {}", e))
                    .for_resource(id.clone())
            })?;

        Ok(())
    }

    // ========== EC2 VPC Operations ==========

    /// Find VPC ID by Name tag
    /// Read an EC2 VPC
    async fn read_ec2_vpc(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        // If identifier starts with vpc-, look up by VPC ID; otherwise by Name tag
        let filter = if identifier.starts_with("vpc-") {
            Filter::builder().name("vpc-id").values(identifier).build()
        } else {
            Filter::builder()
                .name("tag:Name")
                .values(identifier)
                .build()
        };

        let result = self
            .ec2_client
            .describe_vpcs()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to describe VPCs: {:?}", e))
                    .for_resource(id.clone())
            })?;

        if let Some(vpc) = result.vpcs().first() {
            let mut attributes = HashMap::new();

            // Extract Name tag from VPC
            if let Some(name_tag) = vpc
                .tags()
                .iter()
                .find(|t| t.key() == Some("Name"))
                .and_then(|t| t.value())
            {
                attributes.insert("name".to_string(), Value::String(name_tag.to_string()));
            }

            // Return region in DSL format
            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            if let Some(cidr) = vpc.cidr_block() {
                attributes.insert("cidr_block".to_string(), Value::String(cidr.to_string()));
            }

            // Store VPC ID as public attribute and as identifier
            let vpc_id_str = vpc.vpc_id().map(String::from);
            if let Some(ref vpc_id) = vpc_id_str {
                attributes.insert("id".to_string(), Value::String(vpc_id.clone()));
            }

            // Instance tenancy - convert to DSL format
            if let Some(tenancy) = vpc.instance_tenancy() {
                let tenancy_str = match tenancy {
                    aws_sdk_ec2::types::Tenancy::Default => "aws.vpc.InstanceTenancy.default",
                    aws_sdk_ec2::types::Tenancy::Dedicated => "aws.vpc.InstanceTenancy.dedicated",
                    aws_sdk_ec2::types::Tenancy::Host => "aws.vpc.InstanceTenancy.host",
                    _ => "aws.vpc.InstanceTenancy.default",
                };
                attributes.insert(
                    "instance_tenancy".to_string(),
                    Value::String(tenancy_str.to_string()),
                );
            }

            // Get VPC attributes for DNS settings
            if let Some(vpc_id) = vpc.vpc_id() {
                if let Ok(dns_support) = self
                    .ec2_client
                    .describe_vpc_attribute()
                    .vpc_id(vpc_id)
                    .attribute(aws_sdk_ec2::types::VpcAttributeName::EnableDnsSupport)
                    .send()
                    .await
                    && let Some(attr) = dns_support.enable_dns_support()
                {
                    attributes.insert(
                        "enable_dns_support".to_string(),
                        Value::Bool(attr.value.unwrap_or(false)),
                    );
                }

                if let Ok(dns_hostnames) = self
                    .ec2_client
                    .describe_vpc_attribute()
                    .vpc_id(vpc_id)
                    .attribute(aws_sdk_ec2::types::VpcAttributeName::EnableDnsHostnames)
                    .send()
                    .await
                    && let Some(attr) = dns_hostnames.enable_dns_hostnames()
                {
                    attributes.insert(
                        "enable_dns_hostnames".to_string(),
                        Value::Bool(attr.value.unwrap_or(false)),
                    );
                }
            }

            let state = State::existing(id.clone(), attributes);
            Ok(if let Some(vpc_id) = vpc_id_str {
                state.with_identifier(vpc_id)
            } else {
                state
            })
        } else {
            Ok(State::not_found(id.clone()))
        }
    }

    /// Create an EC2 VPC
    async fn create_ec2_vpc(&self, resource: Resource) -> ProviderResult<State> {
        let name = resource.attributes.get("name").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });

        let cidr_block = match resource.attributes.get("cidr_block") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("CIDR block is required").for_resource(resource.id.clone())
                );
            }
        };

        // Create VPC with optional instance_tenancy
        let mut create_vpc_builder = self.ec2_client.create_vpc().cidr_block(&cidr_block);

        // Handle instance_tenancy if specified
        if let Some(Value::String(tenancy)) = resource.attributes.get("instance_tenancy") {
            // Convert DSL format (aws.vpc.InstanceTenancy.dedicated) to API value (dedicated)
            let tenancy_value = extract_enum_value(tenancy);

            let tenancy_enum = match tenancy_value {
                "dedicated" => aws_sdk_ec2::types::Tenancy::Dedicated,
                "host" => aws_sdk_ec2::types::Tenancy::Host,
                _ => aws_sdk_ec2::types::Tenancy::Default,
            };
            create_vpc_builder = create_vpc_builder.instance_tenancy(tenancy_enum);
        }

        let result = create_vpc_builder.send().await.map_err(|e| {
            ProviderError::new(format!("Failed to create VPC: {:?}", e))
                .for_resource(resource.id.clone())
        })?;

        let vpc_id = result.vpc().and_then(|v| v.vpc_id()).ok_or_else(|| {
            ProviderError::new("VPC created but no ID returned").for_resource(resource.id.clone())
        })?;

        // Tag with Name (if provided)
        if let Some(ref name) = name {
            self.ec2_client
                .create_tags()
                .resources(vpc_id)
                .tags(
                    aws_sdk_ec2::types::Tag::builder()
                        .key("Name")
                        .value(name)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to tag VPC: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        // Configure DNS support
        if let Some(Value::Bool(enabled)) = resource.attributes.get("enable_dns_support") {
            self.ec2_client
                .modify_vpc_attribute()
                .vpc_id(vpc_id)
                .enable_dns_support(
                    aws_sdk_ec2::types::AttributeBooleanValue::builder()
                        .value(*enabled)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to set DNS support: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        // Configure DNS hostnames
        if let Some(Value::Bool(enabled)) = resource.attributes.get("enable_dns_hostnames") {
            self.ec2_client
                .modify_vpc_attribute()
                .vpc_id(vpc_id)
                .enable_dns_hostnames(
                    aws_sdk_ec2::types::AttributeBooleanValue::builder()
                        .value(*enabled)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to set DNS hostnames: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        // Read back using VPC ID (reliable identifier)
        self.read_ec2_vpc(&resource.id, Some(vpc_id)).await
    }

    /// Update an EC2 VPC
    async fn update_ec2_vpc(
        &self,
        id: ResourceId,
        identifier: &str,
        to: Resource,
    ) -> ProviderResult<State> {
        // identifier is the VPC ID (e.g., vpc-12345678)
        let vpc_id = identifier.to_string();

        // Update DNS support
        if let Some(Value::Bool(enabled)) = to.attributes.get("enable_dns_support") {
            self.ec2_client
                .modify_vpc_attribute()
                .vpc_id(&vpc_id)
                .enable_dns_support(
                    aws_sdk_ec2::types::AttributeBooleanValue::builder()
                        .value(*enabled)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to update DNS support: {:?}", e))
                        .for_resource(id.clone())
                })?;
        }

        // Update DNS hostnames
        if let Some(Value::Bool(enabled)) = to.attributes.get("enable_dns_hostnames") {
            self.ec2_client
                .modify_vpc_attribute()
                .vpc_id(&vpc_id)
                .enable_dns_hostnames(
                    aws_sdk_ec2::types::AttributeBooleanValue::builder()
                        .value(*enabled)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to update DNS hostnames: {:?}", e))
                        .for_resource(id.clone())
                })?;
        }

        self.read_ec2_vpc(&id, Some(identifier)).await
    }

    /// Delete an EC2 VPC
    async fn delete_ec2_vpc(&self, id: ResourceId, identifier: &str) -> ProviderResult<()> {
        // identifier is the VPC ID
        self.ec2_client
            .delete_vpc()
            .vpc_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to delete VPC: {:?}", e))
                    .for_resource(id.clone())
            })?;

        Ok(())
    }

    // ========== EC2 Subnet Operations ==========

    /// Read an EC2 Subnet
    async fn read_ec2_subnet(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        // If identifier starts with subnet-, look up by subnet ID; otherwise by Name tag
        let filter = if identifier.starts_with("subnet-") {
            Filter::builder()
                .name("subnet-id")
                .values(identifier)
                .build()
        } else {
            Filter::builder()
                .name("tag:Name")
                .values(identifier)
                .build()
        };

        let result = self
            .ec2_client
            .describe_subnets()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to describe subnets: {:?}", e))
                    .for_resource(id.clone())
            })?;

        if let Some(subnet) = result.subnets().first() {
            let mut attributes = HashMap::new();

            // Extract Name tag from subnet
            if let Some(name_tag) = subnet
                .tags()
                .iter()
                .find(|t| t.key() == Some("Name"))
                .and_then(|t| t.value())
            {
                attributes.insert("name".to_string(), Value::String(name_tag.to_string()));
            }

            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            if let Some(cidr) = subnet.cidr_block() {
                attributes.insert("cidr_block".to_string(), Value::String(cidr.to_string()));
            }

            if let Some(az) = subnet.availability_zone() {
                // Return availability_zone in DSL format
                let az_dsl = format!("aws.AvailabilityZone.{}", az.replace('-', "_"));
                attributes.insert("availability_zone".to_string(), Value::String(az_dsl));
            }

            // Store subnet ID
            let subnet_id_str = subnet.subnet_id().map(String::from);
            if let Some(ref subnet_id) = subnet_id_str {
                attributes.insert("id".to_string(), Value::String(subnet_id.clone()));
            }

            // Store VPC ID
            if let Some(vpc_id) = subnet.vpc_id() {
                attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
            }

            let state = State::existing(id.clone(), attributes);
            Ok(if let Some(subnet_id) = subnet_id_str {
                state.with_identifier(subnet_id)
            } else {
                state
            })
        } else {
            Ok(State::not_found(id.clone()))
        }
    }

    /// Create an EC2 Subnet
    async fn create_ec2_subnet(&self, resource: Resource) -> ProviderResult<State> {
        let name = resource.attributes.get("name").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });

        let cidr_block = match resource.attributes.get("cidr_block") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("CIDR block is required").for_resource(resource.id.clone())
                );
            }
        };

        let vpc_id = match resource.attributes.get("vpc_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("VPC ID is required").for_resource(resource.id.clone())
                );
            }
        };

        let mut req = self
            .ec2_client
            .create_subnet()
            .vpc_id(&vpc_id)
            .cidr_block(&cidr_block);

        if let Some(Value::String(az)) = resource.attributes.get("availability_zone") {
            req = req.availability_zone(convert_enum_value(az));
        }

        let result = req.send().await.map_err(|e| {
            ProviderError::new(format!("Failed to create subnet: {:?}", e))
                .for_resource(resource.id.clone())
        })?;

        let subnet_id = result.subnet().and_then(|s| s.subnet_id()).ok_or_else(|| {
            ProviderError::new("Subnet created but no ID returned")
                .for_resource(resource.id.clone())
        })?;

        // Tag with Name (if provided)
        if let Some(ref name) = name {
            self.ec2_client
                .create_tags()
                .resources(subnet_id)
                .tags(
                    aws_sdk_ec2::types::Tag::builder()
                        .key("Name")
                        .value(name)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to tag subnet: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        // Read back using subnet ID (reliable identifier)
        self.read_ec2_subnet(&resource.id, Some(subnet_id)).await
    }

    /// Update an EC2 Subnet (limited - most attributes are immutable)
    async fn update_ec2_subnet(
        &self,
        id: ResourceId,
        identifier: &str,
        _to: Resource,
    ) -> ProviderResult<State> {
        // Subnet attributes (cidr_block, vpc, availability_zone) are immutable
        // Only tags can be updated
        self.read_ec2_subnet(&id, Some(identifier)).await
    }

    /// Delete an EC2 Subnet
    async fn delete_ec2_subnet(&self, id: ResourceId, identifier: &str) -> ProviderResult<()> {
        self.ec2_client
            .delete_subnet()
            .subnet_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to delete subnet: {:?}", e))
                    .for_resource(id.clone())
            })?;

        Ok(())
    }

    // ========== EC2 Internet Gateway Operations ==========

    /// Read an EC2 Internet Gateway
    async fn read_ec2_internet_gateway(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        // If identifier starts with igw-, look up by IGW ID; otherwise by Name tag
        let filter = if identifier.starts_with("igw-") {
            Filter::builder()
                .name("internet-gateway-id")
                .values(identifier)
                .build()
        } else {
            Filter::builder()
                .name("tag:Name")
                .values(identifier)
                .build()
        };

        let result = self
            .ec2_client
            .describe_internet_gateways()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to describe internet gateways: {:?}", e))
                    .for_resource(id.clone())
            })?;

        if let Some(igw) = result.internet_gateways().first() {
            let mut attributes = HashMap::new();

            // Extract Name tag from IGW
            if let Some(name_tag) = igw
                .tags()
                .iter()
                .find(|t| t.key() == Some("Name"))
                .and_then(|t| t.value())
            {
                attributes.insert("name".to_string(), Value::String(name_tag.to_string()));
            }

            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            // Store IGW ID
            let igw_id_str = igw.internet_gateway_id().map(String::from);
            if let Some(ref igw_id) = igw_id_str {
                attributes.insert("id".to_string(), Value::String(igw_id.clone()));
            }

            // Store attached VPC ID
            if let Some(attachment) = igw.attachments().first()
                && let Some(vpc_id) = attachment.vpc_id()
            {
                attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
            }

            let state = State::existing(id.clone(), attributes);
            Ok(if let Some(igw_id) = igw_id_str {
                state.with_identifier(igw_id)
            } else {
                state
            })
        } else {
            Ok(State::not_found(id.clone()))
        }
    }

    /// Create an EC2 Internet Gateway
    async fn create_ec2_internet_gateway(&self, resource: Resource) -> ProviderResult<State> {
        let name = resource.attributes.get("name").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });

        // Create Internet Gateway
        let result = self
            .ec2_client
            .create_internet_gateway()
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to create internet gateway: {:?}", e))
                    .for_resource(resource.id.clone())
            })?;

        let igw_id = result
            .internet_gateway()
            .and_then(|igw| igw.internet_gateway_id())
            .ok_or_else(|| {
                ProviderError::new("Internet Gateway created but no ID returned")
                    .for_resource(resource.id.clone())
            })?;

        // Tag with Name (if provided)
        if let Some(ref name) = name {
            self.ec2_client
                .create_tags()
                .resources(igw_id)
                .tags(
                    aws_sdk_ec2::types::Tag::builder()
                        .key("Name")
                        .value(name)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to tag internet gateway: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        // Attach to VPC if specified
        if let Some(Value::String(vpc_id)) = resource.attributes.get("vpc_id") {
            self.ec2_client
                .attach_internet_gateway()
                .internet_gateway_id(igw_id)
                .vpc_id(vpc_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to attach internet gateway: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        // Read back using IGW ID (reliable identifier)
        self.read_ec2_internet_gateway(&resource.id, Some(igw_id))
            .await
    }

    /// Update an EC2 Internet Gateway
    async fn update_ec2_internet_gateway(
        &self,
        id: ResourceId,
        identifier: &str,
        _to: Resource,
    ) -> ProviderResult<State> {
        // Internet Gateway attributes are mostly immutable
        // VPC attachment changes would require detach/attach
        self.read_ec2_internet_gateway(&id, Some(identifier)).await
    }

    /// Delete an EC2 Internet Gateway
    async fn delete_ec2_internet_gateway(
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
                ProviderError::new(format!("Failed to describe internet gateway: {:?}", e))
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
                        ProviderError::new(format!("Failed to detach internet gateway: {:?}", e))
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
                ProviderError::new(format!("Failed to delete internet gateway: {:?}", e))
                    .for_resource(id.clone())
            })?;

        Ok(())
    }

    // ========== EC2 Route Table Operations ==========

    /// Read an EC2 Route Table
    async fn read_ec2_route_table(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        // If identifier starts with rtb-, look up by route table ID; otherwise by Name tag
        let filter = if identifier.starts_with("rtb-") {
            Filter::builder()
                .name("route-table-id")
                .values(identifier)
                .build()
        } else {
            Filter::builder()
                .name("tag:Name")
                .values(identifier)
                .build()
        };

        let result = self
            .ec2_client
            .describe_route_tables()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to describe route tables: {:?}", e))
                    .for_resource(id.clone())
            })?;

        if let Some(rt) = result.route_tables().first() {
            let mut attributes = HashMap::new();

            // Extract Name tag from route table
            if let Some(name_tag) = rt
                .tags()
                .iter()
                .find(|t| t.key() == Some("Name"))
                .and_then(|t| t.value())
            {
                attributes.insert("name".to_string(), Value::String(name_tag.to_string()));
            }

            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            // Store route table ID
            let rt_id_str = rt.route_table_id().map(String::from);
            if let Some(ref rt_id) = rt_id_str {
                attributes.insert("id".to_string(), Value::String(rt_id.clone()));
            }

            // Store VPC ID
            if let Some(vpc_id) = rt.vpc_id() {
                attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
            }

            // Convert routes to list
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
            if !routes_list.is_empty() {
                attributes.insert("routes".to_string(), Value::List(routes_list));
            }

            let state = State::existing(id.clone(), attributes);
            Ok(if let Some(rt_id) = rt_id_str {
                state.with_identifier(rt_id)
            } else {
                state
            })
        } else {
            Ok(State::not_found(id.clone()))
        }
    }

    /// Create an EC2 Route Table
    async fn create_ec2_route_table(&self, resource: Resource) -> ProviderResult<State> {
        let name = resource.attributes.get("name").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });

        let vpc_id = match resource.attributes.get("vpc_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("VPC ID is required").for_resource(resource.id.clone())
                );
            }
        };

        // Create Route Table
        let result = self
            .ec2_client
            .create_route_table()
            .vpc_id(&vpc_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to create route table: {:?}", e))
                    .for_resource(resource.id.clone())
            })?;

        let rt_id = result
            .route_table()
            .and_then(|rt| rt.route_table_id())
            .ok_or_else(|| {
                ProviderError::new("Route Table created but no ID returned")
                    .for_resource(resource.id.clone())
            })?;

        // Tag with Name (if provided)
        if let Some(ref name) = name {
            self.ec2_client
                .create_tags()
                .resources(rt_id)
                .tags(
                    aws_sdk_ec2::types::Tag::builder()
                        .key("Name")
                        .value(name)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to tag route table: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        // Add routes
        if let Some(Value::List(routes)) = resource.attributes.get("routes") {
            for route in routes {
                if let Value::Map(route_map) = route {
                    let destination = route_map.get("destination").and_then(|v| {
                        if let Value::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    });
                    let gateway_id = route_map.get("gateway_id").and_then(|v| {
                        if let Value::String(s) = v {
                            Some(s)
                        } else {
                            None
                        }
                    });

                    if let (Some(dest), Some(gw_id)) = (destination, gateway_id) {
                        self.ec2_client
                            .create_route()
                            .route_table_id(rt_id)
                            .destination_cidr_block(dest)
                            .gateway_id(gw_id)
                            .send()
                            .await
                            .map_err(|e| {
                                ProviderError::new(format!("Failed to create route: {:?}", e))
                                    .for_resource(resource.id.clone())
                            })?;
                    }
                }
            }
        }

        // Read back using route table ID (reliable identifier)
        self.read_ec2_route_table(&resource.id, Some(rt_id)).await
    }

    /// Update an EC2 Route Table
    async fn update_ec2_route_table(
        &self,
        id: ResourceId,
        identifier: &str,
        _to: Resource,
    ) -> ProviderResult<State> {
        // Route updates would require deleting and recreating routes
        // For now, just return current state
        self.read_ec2_route_table(&id, Some(identifier)).await
    }

    /// Delete an EC2 Route Table
    async fn delete_ec2_route_table(&self, id: ResourceId, identifier: &str) -> ProviderResult<()> {
        self.ec2_client
            .delete_route_table()
            .route_table_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to delete route table: {:?}", e))
                    .for_resource(id.clone())
            })?;

        Ok(())
    }

    // ========== EC2 Route Operations ==========

    /// Read an EC2 Route (routes are identified by route_table_id + destination)
    async fn read_ec2_route(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
    ) -> ProviderResult<State> {
        // Routes don't have a "name" in AWS - we use the name for identification
        // The actual route is identified by route_table_id + destination_cidr_block
        // For read, we return not_found since we can't look up by name alone
        Ok(State::not_found(id.clone()))
    }

    /// Read an EC2 Route by route_table_id and destination_cidr_block
    pub async fn read_ec2_route_by_key(
        &self,
        name: &str,
        route_table_id: &str,
        destination_cidr_block: &str,
    ) -> ProviderResult<State> {
        let id = ResourceId::with_provider("aws", "ec2_route", name);

        // Describe the route table to get its routes
        let result = self
            .ec2_client
            .describe_route_tables()
            .route_table_ids(route_table_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to describe route table: {:?}", e))
                    .for_resource(id.clone())
            })?;

        if let Some(rt) = result.route_tables().first() {
            // Find the route matching destination_cidr_block
            for route in rt.routes() {
                if route.destination_cidr_block() == Some(destination_cidr_block) {
                    let mut attributes = HashMap::new();
                    attributes.insert("name".to_string(), Value::String(name.to_string()));
                    attributes.insert(
                        "route_table_id".to_string(),
                        Value::String(route_table_id.to_string()),
                    );
                    attributes.insert(
                        "destination_cidr_block".to_string(),
                        Value::String(destination_cidr_block.to_string()),
                    );

                    let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
                    attributes.insert("region".to_string(), Value::String(region_dsl));

                    if let Some(gw_id) = route.gateway_id() {
                        attributes
                            .insert("gateway_id".to_string(), Value::String(gw_id.to_string()));
                    }
                    if let Some(nat_gw_id) = route.nat_gateway_id() {
                        attributes.insert(
                            "nat_gateway_id".to_string(),
                            Value::String(nat_gw_id.to_string()),
                        );
                    }

                    // Route identifier is route_table_id|destination_cidr_block
                    let identifier = format!("{}|{}", route_table_id, destination_cidr_block);
                    return Ok(State::existing(id, attributes).with_identifier(identifier));
                }
            }
        }

        Ok(State::not_found(id))
    }

    /// Create an EC2 Route
    async fn create_ec2_route(&self, resource: Resource) -> ProviderResult<State> {
        let name = match resource.attributes.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("Route name is required").for_resource(resource.id.clone())
                );
            }
        };

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
            ProviderError::new(format!("Failed to create route: {:?}", e))
                .for_resource(resource.id.clone())
        })?;

        // Return state with the route attributes
        let mut attributes = resource.attributes.clone();
        attributes.insert("name".to_string(), Value::String(name));

        // Route identifier is route_table_id|destination_cidr_block
        let identifier = format!("{}|{}", route_table_id, destination_cidr);
        Ok(State::existing(resource.id, attributes).with_identifier(identifier))
    }

    /// Update an EC2 Route (replace the route)
    async fn update_ec2_route(
        &self,
        id: ResourceId,
        _identifier: &str,
        to: Resource,
    ) -> ProviderResult<State> {
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
            ProviderError::new(format!("Failed to update route: {:?}", e)).for_resource(id.clone())
        })?;

        // Route identifier is route_table_id|destination_cidr_block
        let identifier = format!("{}|{}", route_table_id, destination_cidr);
        Ok(State::existing(id, to.attributes.clone()).with_identifier(identifier))
    }

    // ========== EC2 Security Group Operations ==========

    /// Read an EC2 Security Group
    async fn read_ec2_security_group(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        // If identifier starts with sg-, look up by security group ID; otherwise by Name tag
        let filter = if identifier.starts_with("sg-") {
            Filter::builder()
                .name("group-id")
                .values(identifier)
                .build()
        } else {
            Filter::builder()
                .name("tag:Name")
                .values(identifier)
                .build()
        };

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

            // Extract Name tag from security group
            if let Some(name_tag) = sg
                .tags()
                .iter()
                .find(|t| t.key() == Some("Name"))
                .and_then(|t| t.value())
            {
                attributes.insert("name".to_string(), Value::String(name_tag.to_string()));
            }

            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            if let Some(desc) = sg.description() {
                attributes.insert("description".to_string(), Value::String(desc.to_string()));
            }

            // Store security group ID
            let sg_id_str = sg.group_id().map(String::from);
            if let Some(ref sg_id) = sg_id_str {
                attributes.insert("id".to_string(), Value::String(sg_id.clone()));
            }

            // Store VPC ID
            if let Some(vpc_id) = sg.vpc_id() {
                attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
            }

            let state = State::existing(id.clone(), attributes);
            Ok(if let Some(sg_id) = sg_id_str {
                state.with_identifier(sg_id)
            } else {
                state
            })
        } else {
            Ok(State::not_found(id.clone()))
        }
    }

    /// Create an EC2 Security Group
    async fn create_ec2_security_group(&self, resource: Resource) -> ProviderResult<State> {
        let name = resource.attributes.get("name").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });

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
            _ => name.clone().unwrap_or_default(), // Use name as description if not specified
        };

        // group_name is required for CreateSecurityGroup API
        let group_name = name.clone().unwrap_or_else(|| resource.id.name.clone());

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

        // Tag with Name (if provided)
        if let Some(ref name) = name {
            self.ec2_client
                .create_tags()
                .resources(sg_id)
                .tags(
                    aws_sdk_ec2::types::Tag::builder()
                        .key("Name")
                        .value(name)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to tag security group: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        // Read back using security group ID (reliable identifier)
        self.read_ec2_security_group(&resource.id, Some(sg_id))
            .await
    }

    /// Update an EC2 Security Group
    async fn update_ec2_security_group(
        &self,
        id: ResourceId,
        identifier: &str,
        _to: Resource,
    ) -> ProviderResult<State> {
        // Security group rule updates would require revoking and re-adding rules
        // For now, just return current state
        self.read_ec2_security_group(&id, Some(identifier)).await
    }

    /// Delete an EC2 Security Group
    async fn delete_ec2_security_group(
        &self,
        id: ResourceId,
        identifier: &str,
    ) -> ProviderResult<()> {
        self.ec2_client
            .delete_security_group()
            .group_id(identifier)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to delete security group: {:?}", e))
                    .for_resource(id.clone())
            })?;

        Ok(())
    }

    // ========== EC2 Security Group Rule Operations ==========

    /// Find all Security Group Rules by Name tag (returns all rules with the same name)
    async fn find_security_group_rules_by_name(
        &self,
        name: &str,
        is_ingress: bool,
    ) -> ProviderResult<Vec<aws_sdk_ec2::types::SecurityGroupRule>> {
        use aws_sdk_ec2::types::Filter;

        let filter = Filter::builder().name("tag:Name").values(name).build();

        let result = self
            .ec2_client
            .describe_security_group_rules()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to describe security group rules: {:?}", e))
            })?;

        // Filter by ingress/egress and collect all matching rules
        let rules: Vec<_> = result
            .security_group_rules()
            .iter()
            .filter(|rule| rule.is_egress() == Some(!is_ingress))
            .cloned()
            .collect();

        Ok(rules)
    }

    /// Read an EC2 Security Group Rule
    async fn read_ec2_security_group_rule(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        let Some(identifier) = identifier else {
            return Ok(State::not_found(id.clone()));
        };

        // If identifier starts with sgr-, look up by rule ID; otherwise by Name tag
        let rules = if identifier.starts_with("sgr-") {
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
            result
                .security_group_rules()
                .iter()
                .filter(|rule| rule.is_egress() == Some(!is_ingress))
                .cloned()
                .collect::<Vec<_>>()
        } else {
            self.find_security_group_rules_by_name(identifier, is_ingress)
                .await?
        };

        if rules.is_empty() {
            return Ok(State::not_found(id.clone()));
        }

        // Use the first rule for common attributes
        let first_rule = &rules[0];
        let mut attributes = HashMap::new();

        // Extract Name tag from rules
        if let Some(name_tag) = first_rule
            .tags()
            .iter()
            .find(|t| t.key() == Some("Name"))
            .and_then(|t| t.value())
        {
            attributes.insert("name".to_string(), Value::String(name_tag.to_string()));
        }

        let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
        attributes.insert("region".to_string(), Value::String(region_dsl));

        // Store rule IDs (comma-separated if multiple)
        let rule_ids: Vec<String> = rules
            .iter()
            .filter_map(|r| r.security_group_rule_id().map(String::from))
            .collect();
        let rule_identifier = if !rule_ids.is_empty() {
            attributes.insert("id".to_string(), Value::String(rule_ids.join(",")));
            Some(rule_ids.join(","))
        } else {
            None
        };

        // Store security group ID
        if let Some(sg_id) = first_rule.group_id() {
            attributes.insert(
                "security_group_id".to_string(),
                Value::String(sg_id.to_string()),
            );
        }

        if let Some(protocol) = first_rule.ip_protocol() {
            // Keep protocol as raw string for comparison (tcp, udp, icmp, -1)
            attributes.insert("protocol".to_string(), Value::String(protocol.to_string()));
        }

        if let Some(from_port) = first_rule.from_port() {
            attributes.insert("from_port".to_string(), Value::Int(from_port as i64));
        }

        if let Some(to_port) = first_rule.to_port() {
            attributes.insert("to_port".to_string(), Value::Int(to_port as i64));
        }

        // Aggregate cidr_blocks from all rules with the same name
        let cidr_blocks: Vec<Value> = rules
            .iter()
            .filter_map(|r| r.cidr_ipv4().map(|c| Value::String(c.to_string())))
            .collect();
        if !cidr_blocks.is_empty() {
            attributes.insert("cidr_blocks".to_string(), Value::List(cidr_blocks));
        }

        let state = State::existing(id.clone(), attributes);
        Ok(if let Some(id_str) = rule_identifier {
            state.with_identifier(id_str)
        } else {
            state
        })
    }

    /// Create an EC2 Security Group Rule
    async fn create_ec2_security_group_rule(
        &self,
        resource: Resource,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        let name = resource.attributes.get("name").and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });

        let sg_id = match resource.attributes.get("security_group_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("Security Group ID is required")
                    .for_resource(resource.id.clone()));
            }
        };

        let protocol = match resource.attributes.get("protocol") {
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

        // Support both cidr_blocks (list) and cidr (single value) for backwards compatibility
        let cidrs: Vec<String> = match resource.attributes.get("cidr_blocks") {
            Some(Value::List(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            _ => match resource.attributes.get("cidr") {
                Some(Value::String(s)) => vec![s.clone()],
                _ => vec!["0.0.0.0/0".to_string()],
            },
        };

        let mut permission_builder = aws_sdk_ec2::types::IpPermission::builder()
            .ip_protocol(&protocol)
            .from_port(from_port)
            .to_port(to_port);

        for cidr in &cidrs {
            permission_builder = permission_builder
                .ip_ranges(aws_sdk_ec2::types::IpRange::builder().cidr_ip(cidr).build());
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

        // Tag ALL rules with the same Name (if name provided)
        if let Some(ref name) = name
            && !rule_ids.is_empty()
        {
            let mut tag_request = self.ec2_client.create_tags();
            for rule_id in &rule_ids {
                tag_request = tag_request.resources(rule_id);
            }
            tag_request
                .tags(
                    aws_sdk_ec2::types::Tag::builder()
                        .key("Name")
                        .value(name)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to tag security group rules: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

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
        vec![
            Box::new(S3BucketType),
            Box::new(VpcType),
            Box::new(SubnetType),
            Box::new(InternetGatewayType),
            Box::new(RouteTableType),
            Box::new(RouteType),
            Box::new(SecurityGroupType),
            Box::new(SecurityGroupIngressRuleType),
            Box::new(SecurityGroupEgressRuleType),
        ]
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.map(String::from);
        Box::pin(async move {
            match id.resource_type.as_str() {
                "s3_bucket" => self.read_s3_bucket(&id, identifier.as_deref()).await,
                "ec2_vpc" => self.read_ec2_vpc(&id, identifier.as_deref()).await,
                "ec2_subnet" => self.read_ec2_subnet(&id, identifier.as_deref()).await,
                "ec2_internet_gateway" => {
                    self.read_ec2_internet_gateway(&id, identifier.as_deref())
                        .await
                }
                "ec2_route_table" => self.read_ec2_route_table(&id, identifier.as_deref()).await,
                "ec2_route" => self.read_ec2_route(&id, identifier.as_deref()).await,
                "ec2_security_group" => {
                    self.read_ec2_security_group(&id, identifier.as_deref())
                        .await
                }
                "ec2_security_group_ingress" => {
                    self.read_ec2_security_group_rule(&id, identifier.as_deref(), true)
                        .await
                }
                "ec2_security_group_egress" => {
                    self.read_ec2_security_group_rule(&id, identifier.as_deref(), false)
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

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        Box::pin(async move {
            match resource.id.resource_type.as_str() {
                "s3_bucket" => self.create_s3_bucket(resource).await,
                "ec2_vpc" => self.create_ec2_vpc(resource).await,
                "ec2_subnet" => self.create_ec2_subnet(resource).await,
                "ec2_internet_gateway" => self.create_ec2_internet_gateway(resource).await,
                "ec2_route_table" => self.create_ec2_route_table(resource).await,
                "ec2_route" => self.create_ec2_route(resource).await,
                "ec2_security_group" => self.create_ec2_security_group(resource).await,
                "ec2_security_group_ingress" => {
                    self.create_ec2_security_group_rule(resource, true).await
                }
                "ec2_security_group_egress" => {
                    self.create_ec2_security_group_rule(resource, false).await
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
        _from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let to = to.clone();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "s3_bucket" => self.update_s3_bucket(id, &identifier, to).await,
                "ec2_vpc" => self.update_ec2_vpc(id, &identifier, to).await,
                "ec2_subnet" => self.update_ec2_subnet(id, &identifier, to).await,
                "ec2_internet_gateway" => {
                    self.update_ec2_internet_gateway(id, &identifier, to).await
                }
                "ec2_route_table" => self.update_ec2_route_table(id, &identifier, to).await,
                "ec2_route" => self.update_ec2_route(id, &identifier, to).await,
                "ec2_security_group" => self.update_ec2_security_group(id, &identifier, to).await,
                "ec2_security_group_ingress" => {
                    self.update_ec2_security_group_rule(id, &identifier, to, true)
                        .await
                }
                "ec2_security_group_egress" => {
                    self.update_ec2_security_group_rule(id, &identifier, to, false)
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
                "s3_bucket" => self.delete_s3_bucket(id, &identifier).await,
                "ec2_vpc" => self.delete_ec2_vpc(id, &identifier).await,
                "ec2_subnet" => self.delete_ec2_subnet(id, &identifier).await,
                "ec2_internet_gateway" => self.delete_ec2_internet_gateway(id, &identifier).await,
                "ec2_route_table" => self.delete_ec2_route_table(id, &identifier).await,
                "ec2_route" => {
                    // Route deletion requires route_table_id and destination_cidr_block
                    // which are not available from ResourceId alone.
                    // Routes are typically deleted when the route table is deleted.
                    Ok(())
                }
                "ec2_security_group" => self.delete_ec2_security_group(id, &identifier).await,
                "ec2_security_group_ingress" => {
                    self.delete_ec2_security_group_rule(id, &identifier, true)
                        .await
                }
                "ec2_security_group_egress" => {
                    self.delete_ec2_security_group_rule(id, &identifier, false)
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
        let bucket_type = S3BucketType;
        assert_eq!(bucket_type.name(), "s3_bucket");
    }
}
