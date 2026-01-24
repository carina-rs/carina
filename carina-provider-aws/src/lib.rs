//! Carina AWS Provider
//!
//! AWS Provider implementation

use std::collections::HashMap;

use aws_config::Region;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_s3::Client as S3Client;
use carina_core::provider::{
    BoxFuture, Provider, ProviderError, ProviderResult, ResourceSchema, ResourceType,
};
use carina_core::resource::{Resource, ResourceId, State, Value};

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
        "vpc"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Subnet resource type
pub struct SubnetType;

impl ResourceType for SubnetType {
    fn name(&self) -> &'static str {
        "subnet"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Internet Gateway resource type
pub struct InternetGatewayType;

impl ResourceType for InternetGatewayType {
    fn name(&self) -> &'static str {
        "internet_gateway"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Route Table resource type
pub struct RouteTableType;

impl ResourceType for RouteTableType {
    fn name(&self) -> &'static str {
        "route_table"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Security Group resource type
pub struct SecurityGroupType;

impl ResourceType for SecurityGroupType {
    fn name(&self) -> &'static str {
        "security_group"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Security Group Ingress Rule resource type
pub struct SecurityGroupIngressRuleType;

impl ResourceType for SecurityGroupIngressRuleType {
    fn name(&self) -> &'static str {
        "security_group.ingress_rule"
    }

    fn schema(&self) -> ResourceSchema {
        ResourceSchema::default()
    }
}

/// Security Group Egress Rule resource type
pub struct SecurityGroupEgressRuleType;

impl ResourceType for SecurityGroupEgressRuleType {
    fn name(&self) -> &'static str {
        "security_group.egress_rule"
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
    async fn read_s3_bucket(&self, name: &str) -> ProviderResult<State> {
        let id = ResourceId::new("s3_bucket", name);

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
                    let enabled = versioning
                        .status()
                        .map(|s| s.as_str() == "Enabled")
                        .unwrap_or(false);
                    attributes.insert("versioning".to_string(), Value::Bool(enabled));
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

                Ok(State::existing(id, attributes))
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
                    Ok(State::not_found(id))
                } else {
                    Err(
                        ProviderError::new(format!("Failed to read bucket: {:?}", err))
                            .for_resource(id),
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
                convert_region_value(s)
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
        if let Some(Value::Bool(true)) = resource.attributes.get("versioning") {
            use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};
            let config = VersioningConfiguration::builder()
                .status(BucketVersioningStatus::Enabled)
                .build();
            self.s3_client
                .put_bucket_versioning()
                .bucket(&bucket_name)
                .versioning_configuration(config)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to enable versioning: {}", e))
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
        self.read_s3_bucket(&bucket_name).await
    }

    /// Update an S3 bucket
    async fn update_s3_bucket(&self, id: ResourceId, to: Resource) -> ProviderResult<State> {
        let bucket_name = id.name.clone();

        // Update versioning configuration
        if let Some(Value::Bool(enabled)) = to.attributes.get("versioning") {
            use aws_sdk_s3::types::{BucketVersioningStatus, VersioningConfiguration};
            let status = if *enabled {
                BucketVersioningStatus::Enabled
            } else {
                BucketVersioningStatus::Suspended
            };
            let config = VersioningConfiguration::builder().status(status).build();
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

        self.read_s3_bucket(&bucket_name).await
    }

    /// Delete an S3 bucket
    async fn delete_s3_bucket(&self, id: ResourceId) -> ProviderResult<()> {
        self.s3_client
            .delete_bucket()
            .bucket(&id.name)
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
    async fn find_vpc_id_by_name(&self, name: &str) -> ProviderResult<Option<String>> {
        use aws_sdk_ec2::types::Filter;

        let filter = Filter::builder().name("tag:Name").values(name).build();

        let result = self
            .ec2_client
            .describe_vpcs()
            .filters(filter)
            .send()
            .await
            .map_err(|e| ProviderError::new(format!("Failed to describe VPCs: {:?}", e)))?;

        Ok(result
            .vpcs()
            .first()
            .and_then(|vpc| vpc.vpc_id().map(String::from)))
    }

    /// Read an EC2 VPC
    async fn read_ec2_vpc(&self, name: &str) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let id = ResourceId::new("vpc", name);

        let filter = Filter::builder().name("tag:Name").values(name).build();

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
            attributes.insert("name".to_string(), Value::String(name.to_string()));

            // Return region in DSL format
            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            if let Some(cidr) = vpc.cidr_block() {
                attributes.insert("cidr_block".to_string(), Value::String(cidr.to_string()));
            }

            // Store VPC ID as public attribute
            if let Some(vpc_id) = vpc.vpc_id() {
                attributes.insert("id".to_string(), Value::String(vpc_id.to_string()));
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

            Ok(State::existing(id, attributes))
        } else {
            Ok(State::not_found(id))
        }
    }

    /// Create an EC2 VPC
    async fn create_ec2_vpc(&self, resource: Resource) -> ProviderResult<State> {
        let name = match resource.attributes.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("VPC name is required").for_resource(resource.id.clone())
                );
            }
        };

        let cidr_block = match resource.attributes.get("cidr_block") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("CIDR block is required").for_resource(resource.id.clone())
                );
            }
        };

        // Create VPC
        let result = self
            .ec2_client
            .create_vpc()
            .cidr_block(&cidr_block)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to create VPC: {:?}", e))
                    .for_resource(resource.id.clone())
            })?;

        let vpc_id = result.vpc().and_then(|v| v.vpc_id()).ok_or_else(|| {
            ProviderError::new("VPC created but no ID returned").for_resource(resource.id.clone())
        })?;

        // Tag with Name
        self.ec2_client
            .create_tags()
            .resources(vpc_id)
            .tags(
                aws_sdk_ec2::types::Tag::builder()
                    .key("Name")
                    .value(&name)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to tag VPC: {:?}", e))
                    .for_resource(resource.id.clone())
            })?;

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

        self.read_ec2_vpc(&name).await
    }

    /// Update an EC2 VPC
    async fn update_ec2_vpc(&self, id: ResourceId, to: Resource) -> ProviderResult<State> {
        let vpc_id = self
            .find_vpc_id_by_name(&id.name)
            .await?
            .ok_or_else(|| ProviderError::new("VPC not found").for_resource(id.clone()))?;

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

        self.read_ec2_vpc(&id.name).await
    }

    /// Delete an EC2 VPC
    async fn delete_ec2_vpc(&self, id: ResourceId) -> ProviderResult<()> {
        let vpc_id = self
            .find_vpc_id_by_name(&id.name)
            .await?
            .ok_or_else(|| ProviderError::new("VPC not found").for_resource(id.clone()))?;

        self.ec2_client
            .delete_vpc()
            .vpc_id(&vpc_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to delete VPC: {:?}", e))
                    .for_resource(id.clone())
            })?;

        Ok(())
    }

    // ========== EC2 Subnet Operations ==========

    /// Find Subnet ID by Name tag
    async fn find_subnet_id_by_name(&self, name: &str) -> ProviderResult<Option<String>> {
        use aws_sdk_ec2::types::Filter;

        let filter = Filter::builder().name("tag:Name").values(name).build();

        let result = self
            .ec2_client
            .describe_subnets()
            .filters(filter)
            .send()
            .await
            .map_err(|e| ProviderError::new(format!("Failed to describe subnets: {:?}", e)))?;

        Ok(result
            .subnets()
            .first()
            .and_then(|s| s.subnet_id().map(String::from)))
    }

    /// Read an EC2 Subnet
    async fn read_ec2_subnet(&self, name: &str) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let id = ResourceId::new("subnet", name);

        let filter = Filter::builder().name("tag:Name").values(name).build();

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
            attributes.insert("name".to_string(), Value::String(name.to_string()));

            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            if let Some(cidr) = subnet.cidr_block() {
                attributes.insert("cidr_block".to_string(), Value::String(cidr.to_string()));
            }

            if let Some(az) = subnet.availability_zone() {
                attributes.insert(
                    "availability_zone".to_string(),
                    Value::String(az.to_string()),
                );
            }

            // Store subnet ID
            if let Some(subnet_id) = subnet.subnet_id() {
                attributes.insert("id".to_string(), Value::String(subnet_id.to_string()));
            }

            // Store VPC ID
            if let Some(vpc_id) = subnet.vpc_id() {
                attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
            }

            Ok(State::existing(id, attributes))
        } else {
            Ok(State::not_found(id))
        }
    }

    /// Create an EC2 Subnet
    async fn create_ec2_subnet(&self, resource: Resource) -> ProviderResult<State> {
        let name = match resource.attributes.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("Subnet name is required").for_resource(resource.id.clone())
                );
            }
        };

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
            req = req.availability_zone(az);
        }

        let result = req.send().await.map_err(|e| {
            ProviderError::new(format!("Failed to create subnet: {:?}", e))
                .for_resource(resource.id.clone())
        })?;

        let subnet_id = result.subnet().and_then(|s| s.subnet_id()).ok_or_else(|| {
            ProviderError::new("Subnet created but no ID returned")
                .for_resource(resource.id.clone())
        })?;

        // Tag with Name
        self.ec2_client
            .create_tags()
            .resources(subnet_id)
            .tags(
                aws_sdk_ec2::types::Tag::builder()
                    .key("Name")
                    .value(&name)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to tag subnet: {:?}", e))
                    .for_resource(resource.id.clone())
            })?;

        self.read_ec2_subnet(&name).await
    }

    /// Update an EC2 Subnet (limited - most attributes are immutable)
    async fn update_ec2_subnet(&self, id: ResourceId, _to: Resource) -> ProviderResult<State> {
        // Subnet attributes (cidr_block, vpc, availability_zone) are immutable
        // Only tags can be updated
        self.read_ec2_subnet(&id.name).await
    }

    /// Delete an EC2 Subnet
    async fn delete_ec2_subnet(&self, id: ResourceId) -> ProviderResult<()> {
        let subnet_id = self
            .find_subnet_id_by_name(&id.name)
            .await?
            .ok_or_else(|| ProviderError::new("Subnet not found").for_resource(id.clone()))?;

        self.ec2_client
            .delete_subnet()
            .subnet_id(&subnet_id)
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
    async fn read_ec2_internet_gateway(&self, name: &str) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let id = ResourceId::new("internet_gateway", name);

        let filter = Filter::builder().name("tag:Name").values(name).build();

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
            attributes.insert("name".to_string(), Value::String(name.to_string()));

            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            // Store IGW ID
            if let Some(igw_id) = igw.internet_gateway_id() {
                attributes.insert("id".to_string(), Value::String(igw_id.to_string()));
            }

            // Store attached VPC ID
            if let Some(attachment) = igw.attachments().first()
                && let Some(vpc_id) = attachment.vpc_id()
            {
                attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
            }

            Ok(State::existing(id, attributes))
        } else {
            Ok(State::not_found(id))
        }
    }

    /// Create an EC2 Internet Gateway
    async fn create_ec2_internet_gateway(&self, resource: Resource) -> ProviderResult<State> {
        let name = match resource.attributes.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("Internet Gateway name is required")
                    .for_resource(resource.id.clone()));
            }
        };

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

        // Tag with Name
        self.ec2_client
            .create_tags()
            .resources(igw_id)
            .tags(
                aws_sdk_ec2::types::Tag::builder()
                    .key("Name")
                    .value(&name)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to tag internet gateway: {:?}", e))
                    .for_resource(resource.id.clone())
            })?;

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

        self.read_ec2_internet_gateway(&name).await
    }

    /// Update an EC2 Internet Gateway
    async fn update_ec2_internet_gateway(
        &self,
        id: ResourceId,
        _to: Resource,
    ) -> ProviderResult<State> {
        // Internet Gateway attributes are mostly immutable
        // VPC attachment changes would require detach/attach
        self.read_ec2_internet_gateway(&id.name).await
    }

    /// Delete an EC2 Internet Gateway
    async fn delete_ec2_internet_gateway(&self, id: ResourceId) -> ProviderResult<()> {
        use aws_sdk_ec2::types::Filter;

        let filter = Filter::builder().name("tag:Name").values(&id.name).build();

        let result = self
            .ec2_client
            .describe_internet_gateways()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to describe internet gateway: {:?}", e))
                    .for_resource(id.clone())
            })?;

        let igw = result.internet_gateways().first().ok_or_else(|| {
            ProviderError::new("Internet Gateway not found").for_resource(id.clone())
        })?;

        let igw_id = igw
            .internet_gateway_id()
            .ok_or_else(|| ProviderError::new("No IGW ID").for_resource(id.clone()))?;

        // Detach from VPC first
        if let Some(attachment) = igw.attachments().first()
            && let Some(vpc_id) = attachment.vpc_id()
        {
            self.ec2_client
                .detach_internet_gateway()
                .internet_gateway_id(igw_id)
                .vpc_id(vpc_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to detach internet gateway: {:?}", e))
                        .for_resource(id.clone())
                })?;
        }

        // Delete Internet Gateway
        self.ec2_client
            .delete_internet_gateway()
            .internet_gateway_id(igw_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to delete internet gateway: {:?}", e))
                    .for_resource(id.clone())
            })?;

        Ok(())
    }

    // ========== EC2 Route Table Operations ==========

    /// Find Route Table ID by Name tag
    async fn find_route_table_id_by_name(&self, name: &str) -> ProviderResult<Option<String>> {
        use aws_sdk_ec2::types::Filter;

        let filter = Filter::builder().name("tag:Name").values(name).build();

        let result = self
            .ec2_client
            .describe_route_tables()
            .filters(filter)
            .send()
            .await
            .map_err(|e| ProviderError::new(format!("Failed to describe route tables: {:?}", e)))?;

        Ok(result
            .route_tables()
            .first()
            .and_then(|rt| rt.route_table_id().map(String::from)))
    }

    /// Read an EC2 Route Table
    async fn read_ec2_route_table(&self, name: &str) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let id = ResourceId::new("route_table", name);

        let filter = Filter::builder().name("tag:Name").values(name).build();

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
            attributes.insert("name".to_string(), Value::String(name.to_string()));

            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            // Store route table ID
            if let Some(rt_id) = rt.route_table_id() {
                attributes.insert("id".to_string(), Value::String(rt_id.to_string()));
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

            Ok(State::existing(id, attributes))
        } else {
            Ok(State::not_found(id))
        }
    }

    /// Create an EC2 Route Table
    async fn create_ec2_route_table(&self, resource: Resource) -> ProviderResult<State> {
        let name = match resource.attributes.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("Route Table name is required")
                    .for_resource(resource.id.clone()));
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

        // Tag with Name
        self.ec2_client
            .create_tags()
            .resources(rt_id)
            .tags(
                aws_sdk_ec2::types::Tag::builder()
                    .key("Name")
                    .value(&name)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to tag route table: {:?}", e))
                    .for_resource(resource.id.clone())
            })?;

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

        self.read_ec2_route_table(&name).await
    }

    /// Update an EC2 Route Table
    async fn update_ec2_route_table(&self, id: ResourceId, _to: Resource) -> ProviderResult<State> {
        // Route updates would require deleting and recreating routes
        // For now, just return current state
        self.read_ec2_route_table(&id.name).await
    }

    /// Delete an EC2 Route Table
    async fn delete_ec2_route_table(&self, id: ResourceId) -> ProviderResult<()> {
        let rt_id = self
            .find_route_table_id_by_name(&id.name)
            .await?
            .ok_or_else(|| ProviderError::new("Route Table not found").for_resource(id.clone()))?;

        self.ec2_client
            .delete_route_table()
            .route_table_id(&rt_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to delete route table: {:?}", e))
                    .for_resource(id.clone())
            })?;

        Ok(())
    }

    // ========== EC2 Security Group Operations ==========

    /// Find Security Group ID by Name tag (not group-name)
    async fn find_security_group_id_by_name(&self, name: &str) -> ProviderResult<Option<String>> {
        use aws_sdk_ec2::types::Filter;

        let filter = Filter::builder().name("tag:Name").values(name).build();

        let result = self
            .ec2_client
            .describe_security_groups()
            .filters(filter)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to describe security groups: {:?}", e))
            })?;

        Ok(result
            .security_groups()
            .first()
            .and_then(|sg| sg.group_id().map(String::from)))
    }

    /// Read an EC2 Security Group
    async fn read_ec2_security_group(&self, name: &str) -> ProviderResult<State> {
        use aws_sdk_ec2::types::Filter;

        let id = ResourceId::new("security_group", name);

        let filter = Filter::builder().name("tag:Name").values(name).build();

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
            attributes.insert("name".to_string(), Value::String(name.to_string()));

            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            if let Some(desc) = sg.description() {
                attributes.insert("description".to_string(), Value::String(desc.to_string()));
            }

            // Store security group ID
            if let Some(sg_id) = sg.group_id() {
                attributes.insert("id".to_string(), Value::String(sg_id.to_string()));
            }

            // Store VPC ID
            if let Some(vpc_id) = sg.vpc_id() {
                attributes.insert("vpc_id".to_string(), Value::String(vpc_id.to_string()));
            }

            Ok(State::existing(id, attributes))
        } else {
            Ok(State::not_found(id))
        }
    }

    /// Create an EC2 Security Group
    async fn create_ec2_security_group(&self, resource: Resource) -> ProviderResult<State> {
        let name = match resource.attributes.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("Security Group name is required")
                    .for_resource(resource.id.clone()));
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

        let description = match resource.attributes.get("description") {
            Some(Value::String(s)) => s.clone(),
            _ => name.clone(), // Use name as description if not specified
        };

        // Create Security Group
        let result = self
            .ec2_client
            .create_security_group()
            .group_name(&name)
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

        // Tag with Name
        self.ec2_client
            .create_tags()
            .resources(sg_id)
            .tags(
                aws_sdk_ec2::types::Tag::builder()
                    .key("Name")
                    .value(&name)
                    .build(),
            )
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to tag security group: {:?}", e))
                    .for_resource(resource.id.clone())
            })?;

        self.read_ec2_security_group(&name).await
    }

    /// Update an EC2 Security Group
    async fn update_ec2_security_group(
        &self,
        id: ResourceId,
        _to: Resource,
    ) -> ProviderResult<State> {
        // Security group rule updates would require revoking and re-adding rules
        // For now, just return current state
        self.read_ec2_security_group(&id.name).await
    }

    /// Delete an EC2 Security Group
    async fn delete_ec2_security_group(&self, id: ResourceId) -> ProviderResult<()> {
        let sg_id = self
            .find_security_group_id_by_name(&id.name)
            .await?
            .ok_or_else(|| {
                ProviderError::new("Security Group not found").for_resource(id.clone())
            })?;

        self.ec2_client
            .delete_security_group()
            .group_id(&sg_id)
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to delete security group: {:?}", e))
                    .for_resource(id.clone())
            })?;

        Ok(())
    }

    // ========== EC2 Security Group Rule Operations ==========

    /// Find Security Group Rule by Name tag
    async fn find_security_group_rule_by_name(
        &self,
        name: &str,
        is_ingress: bool,
    ) -> ProviderResult<Option<aws_sdk_ec2::types::SecurityGroupRule>> {
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

        // Filter by ingress/egress
        for rule in result.security_group_rules() {
            if rule.is_egress() == Some(!is_ingress) {
                return Ok(Some(rule.clone()));
            }
        }

        Ok(None)
    }

    /// Read an EC2 Security Group Rule
    async fn read_ec2_security_group_rule(
        &self,
        name: &str,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        let resource_type = if is_ingress {
            "security_group.ingress_rule"
        } else {
            "security_group.egress_rule"
        };
        let id = ResourceId::new(resource_type, name);

        let rule = self
            .find_security_group_rule_by_name(name, is_ingress)
            .await?;

        if let Some(rule) = rule {
            let mut attributes = HashMap::new();
            attributes.insert("name".to_string(), Value::String(name.to_string()));

            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            // Store rule ID
            if let Some(rule_id) = rule.security_group_rule_id() {
                attributes.insert("id".to_string(), Value::String(rule_id.to_string()));
            }

            // Store security group ID
            if let Some(sg_id) = rule.group_id() {
                attributes.insert(
                    "security_group_id".to_string(),
                    Value::String(sg_id.to_string()),
                );
            }

            if let Some(protocol) = rule.ip_protocol() {
                attributes.insert("protocol".to_string(), Value::String(protocol.to_string()));
            }

            if let Some(from_port) = rule.from_port() {
                attributes.insert("from_port".to_string(), Value::Int(from_port as i64));
            }

            if let Some(to_port) = rule.to_port() {
                attributes.insert("to_port".to_string(), Value::Int(to_port as i64));
            }

            if let Some(cidr) = rule.cidr_ipv4() {
                attributes.insert("cidr".to_string(), Value::String(cidr.to_string()));
            }

            Ok(State::existing(id, attributes))
        } else {
            Ok(State::not_found(id))
        }
    }

    /// Create an EC2 Security Group Rule
    async fn create_ec2_security_group_rule(
        &self,
        resource: Resource,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        let name = match resource.attributes.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(
                    ProviderError::new("Rule name is required").for_resource(resource.id.clone())
                );
            }
        };

        let sg_id = match resource.attributes.get("security_group_id") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("Security Group ID is required")
                    .for_resource(resource.id.clone()));
            }
        };

        let protocol = match resource.attributes.get("protocol") {
            Some(Value::String(s)) => s.clone(),
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

        let cidr = match resource.attributes.get("cidr") {
            Some(Value::String(s)) => s.clone(),
            _ => "0.0.0.0/0".to_string(),
        };

        let permission = aws_sdk_ec2::types::IpPermission::builder()
            .ip_protocol(&protocol)
            .from_port(from_port)
            .to_port(to_port)
            .ip_ranges(
                aws_sdk_ec2::types::IpRange::builder()
                    .cidr_ip(&cidr)
                    .build(),
            )
            .build();

        let rule_id = if is_ingress {
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
                .first()
                .and_then(|r| r.security_group_rule_id())
                .map(String::from)
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
                .first()
                .and_then(|r| r.security_group_rule_id())
                .map(String::from)
        };

        // Tag the rule with Name
        if let Some(rule_id) = rule_id {
            self.ec2_client
                .create_tags()
                .resources(&rule_id)
                .tags(
                    aws_sdk_ec2::types::Tag::builder()
                        .key("Name")
                        .value(&name)
                        .build(),
                )
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to tag security group rule: {:?}", e))
                        .for_resource(resource.id.clone())
                })?;
        }

        self.read_ec2_security_group_rule(&name, is_ingress).await
    }

    /// Update an EC2 Security Group Rule (rules are immutable, so recreate)
    async fn update_ec2_security_group_rule(
        &self,
        id: ResourceId,
        _to: Resource,
        is_ingress: bool,
    ) -> ProviderResult<State> {
        // Security group rules are immutable - changes require delete and recreate
        self.read_ec2_security_group_rule(&id.name, is_ingress)
            .await
    }

    /// Delete an EC2 Security Group Rule
    async fn delete_ec2_security_group_rule(
        &self,
        id: ResourceId,
        is_ingress: bool,
    ) -> ProviderResult<()> {
        let rule = self
            .find_security_group_rule_by_name(&id.name, is_ingress)
            .await?
            .ok_or_else(|| {
                ProviderError::new("Security Group Rule not found").for_resource(id.clone())
            })?;

        let rule_id = rule
            .security_group_rule_id()
            .ok_or_else(|| ProviderError::new("Rule has no ID").for_resource(id.clone()))?;

        let sg_id = rule.group_id().ok_or_else(|| {
            ProviderError::new("Rule has no security group ID").for_resource(id.clone())
        })?;

        if is_ingress {
            self.ec2_client
                .revoke_security_group_ingress()
                .group_id(sg_id)
                .security_group_rule_ids(rule_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to delete ingress rule: {:?}", e))
                        .for_resource(id.clone())
                })?;
        } else {
            self.ec2_client
                .revoke_security_group_egress()
                .group_id(sg_id)
                .security_group_rule_ids(rule_id)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to delete egress rule: {:?}", e))
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
            Box::new(SecurityGroupType),
            Box::new(SecurityGroupIngressRuleType),
            Box::new(SecurityGroupEgressRuleType),
        ]
    }

    fn read(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "s3_bucket" => self.read_s3_bucket(&id.name).await,
                "vpc" => self.read_ec2_vpc(&id.name).await,
                "subnet" => self.read_ec2_subnet(&id.name).await,
                "internet_gateway" => self.read_ec2_internet_gateway(&id.name).await,
                "route_table" => self.read_ec2_route_table(&id.name).await,
                "security_group" => self.read_ec2_security_group(&id.name).await,
                "security_group.ingress_rule" => {
                    self.read_ec2_security_group_rule(&id.name, true).await
                }
                "security_group.egress_rule" => {
                    self.read_ec2_security_group_rule(&id.name, false).await
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
                "vpc" => self.create_ec2_vpc(resource).await,
                "subnet" => self.create_ec2_subnet(resource).await,
                "internet_gateway" => self.create_ec2_internet_gateway(resource).await,
                "route_table" => self.create_ec2_route_table(resource).await,
                "security_group" => self.create_ec2_security_group(resource).await,
                "security_group.ingress_rule" => {
                    self.create_ec2_security_group_rule(resource, true).await
                }
                "security_group.egress_rule" => {
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
        _from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        let to = to.clone();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "s3_bucket" => self.update_s3_bucket(id, to).await,
                "vpc" => self.update_ec2_vpc(id, to).await,
                "subnet" => self.update_ec2_subnet(id, to).await,
                "internet_gateway" => self.update_ec2_internet_gateway(id, to).await,
                "route_table" => self.update_ec2_route_table(id, to).await,
                "security_group" => self.update_ec2_security_group(id, to).await,
                "security_group.ingress_rule" => {
                    self.update_ec2_security_group_rule(id, to, true).await
                }
                "security_group.egress_rule" => {
                    self.update_ec2_security_group_rule(id, to, false).await
                }
                _ => Err(ProviderError::new(format!(
                    "Unknown resource type: {}",
                    id.resource_type
                ))
                .for_resource(id.clone())),
            }
        })
    }

    fn delete(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "s3_bucket" => self.delete_s3_bucket(id).await,
                "vpc" => self.delete_ec2_vpc(id).await,
                "subnet" => self.delete_ec2_subnet(id).await,
                "internet_gateway" => self.delete_ec2_internet_gateway(id).await,
                "route_table" => self.delete_ec2_route_table(id).await,
                "security_group" => self.delete_ec2_security_group(id).await,
                "security_group.ingress_rule" => {
                    self.delete_ec2_security_group_rule(id, true).await
                }
                "security_group.egress_rule" => {
                    self.delete_ec2_security_group_rule(id, false).await
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

/// Convert DSL region value (aws.Region.ap_northeast_1) to AWS SDK format (ap-northeast-1)
fn convert_region_value(value: &str) -> String {
    if value.starts_with("aws.Region.") {
        value
            .strip_prefix("aws.Region.")
            .unwrap_or(value)
            .replace('_', "-")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_region_value() {
        assert_eq!(
            convert_region_value("aws.Region.ap_northeast_1"),
            "ap-northeast-1"
        );
        assert_eq!(convert_region_value("aws.Region.us_east_1"), "us-east-1");
        assert_eq!(convert_region_value("eu-west-1"), "eu-west-1");
    }

    #[test]
    fn test_s3_bucket_type_name() {
        let bucket_type = S3BucketType;
        assert_eq!(bucket_type.name(), "s3_bucket");
    }
}
