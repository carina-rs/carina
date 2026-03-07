//! Carina AWS Provider
//!
//! AWS Provider implementation

pub mod provider_generated;
pub mod resource_defs;
pub mod schemas;

use std::collections::HashMap;

use aws_config::Region;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_sts::Client as StsClient;
use carina_core::provider::{
    BoxFuture, Provider, ProviderError, ProviderFactory, ProviderResult, ResourceType,
};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::schema::AttributeType;
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

    fn identity_attributes(&self) -> Vec<&str> {
        vec!["region"]
    }

    fn region_completions(&self) -> Vec<carina_core::schema::CompletionValue> {
        aws_region_completions("aws")
    }
}

fn aws_region_completions(prefix: &str) -> Vec<carina_core::schema::CompletionValue> {
    use carina_core::schema::CompletionValue;
    let regions = [
        ("ap_northeast_1", "Asia Pacific (Tokyo)"),
        ("ap_northeast_2", "Asia Pacific (Seoul)"),
        ("ap_northeast_3", "Asia Pacific (Osaka)"),
        ("ap_south_1", "Asia Pacific (Mumbai)"),
        ("ap_southeast_1", "Asia Pacific (Singapore)"),
        ("ap_southeast_2", "Asia Pacific (Sydney)"),
        ("ca_central_1", "Canada (Central)"),
        ("eu_central_1", "Europe (Frankfurt)"),
        ("eu_west_1", "Europe (Ireland)"),
        ("eu_west_2", "Europe (London)"),
        ("eu_west_3", "Europe (Paris)"),
        ("eu_north_1", "Europe (Stockholm)"),
        ("sa_east_1", "South America (Sao Paulo)"),
        ("us_east_1", "US East (N. Virginia)"),
        ("us_east_2", "US East (Ohio)"),
        ("us_west_1", "US West (N. California)"),
        ("us_west_2", "US West (Oregon)"),
    ];
    regions
        .iter()
        .map(|(code, name)| CompletionValue::new(format!("{}.Region.{}", prefix, code), *name))
        .collect()
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
    fn ec2_tags_to_value(tags: &[aws_sdk_ec2::types::Tag]) -> Option<Value> {
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
    async fn apply_ec2_tags(
        &self,
        resource_id: &ResourceId,
        ec2_resource_id: &str,
        attributes: &HashMap<String, Value>,
    ) -> ProviderResult<()> {
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
                attributes.insert("bucket".to_string(), Value::String(name.to_string()));
                // Return region in DSL format
                let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
                attributes.insert("region".to_string(), Value::String(region_dsl));

                // Get versioning status
                self.read_s3_bucket_versioning(name, &mut attributes).await;

                // Get object ownership
                self.read_s3_bucket_ownership_controls(name, &mut attributes)
                    .await;

                // Get Object Lock status
                self.read_s3_bucket_object_lock(name, &mut attributes).await;

                // Get ACL
                self.read_s3_bucket_acl(name, &mut attributes).await;

                // Get tags
                self.read_s3_bucket_tags(name, &mut attributes).await;

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
        let bucket_name = match resource.attributes.get("bucket") {
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

        // Set ObjectLockEnabledForBucket on create
        if let Some(Value::Bool(val)) = resource.attributes.get("object_lock_enabled_for_bucket") {
            req = req.object_lock_enabled_for_bucket(*val);
        }

        // Set ObjectOwnership on create
        if let Some(Value::String(val)) = resource.attributes.get("object_ownership") {
            use aws_sdk_s3::types::ObjectOwnership;
            let normalized = extract_enum_value(val);
            req = req.object_ownership(ObjectOwnership::from(normalized));
        }

        // Set ACL on create (convert_enum_value converts underscores back to hyphens)
        if let Some(Value::String(val)) = resource.attributes.get("acl") {
            use aws_sdk_s3::types::BucketCannedAcl;
            let normalized = convert_enum_value(val);
            req = req.acl(BucketCannedAcl::from(normalized.as_str()));
        }
        if let Some(Value::String(val)) = resource.attributes.get("grant_full_control") {
            req = req.grant_full_control(val);
        }
        if let Some(Value::String(val)) = resource.attributes.get("grant_read") {
            req = req.grant_read(val);
        }
        if let Some(Value::String(val)) = resource.attributes.get("grant_read_acp") {
            req = req.grant_read_acp(val);
        }
        if let Some(Value::String(val)) = resource.attributes.get("grant_write") {
            req = req.grant_write(val);
        }
        if let Some(Value::String(val)) = resource.attributes.get("grant_write_acp") {
            req = req.grant_write_acp(val);
        }

        req.send().await.map_err(|e| {
            ProviderError::new(format!("Failed to create bucket: {:?}", e))
                .for_resource(resource.id.clone())
        })?;

        // Configure versioning
        self.write_s3_bucket_versioning(&resource.id, &bucket_name, &resource.attributes)
            .await?;

        // Set tags
        self.write_s3_bucket_tags(&resource.id, &bucket_name, &resource.attributes)
            .await?;

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

        // Update versioning status
        self.write_s3_bucket_versioning(&id, &bucket_name, &to.attributes)
            .await?;

        // Update object ownership
        self.write_s3_bucket_ownership_controls(&id, &bucket_name, &to.attributes)
            .await?;

        // Update ACL
        self.write_s3_bucket_acl(&id, &bucket_name, &to.attributes)
            .await?;

        // Update tags
        self.write_s3_bucket_tags(&id, &bucket_name, &to.attributes)
            .await?;

        self.read_s3_bucket(&id, Some(&bucket_name)).await
    }

    /// Read S3 bucket ownership controls
    async fn read_s3_bucket_ownership_controls(
        &self,
        identifier: &str,
        attributes: &mut HashMap<String, Value>,
    ) {
        if let Ok(output) = self
            .s3_client
            .get_bucket_ownership_controls()
            .bucket(identifier)
            .send()
            .await
            && let Some(controls) = output.ownership_controls()
            && let Some(rule) = controls.rules().first()
        {
            let value = rule.object_ownership().as_str().to_string();
            attributes.insert("object_ownership".to_string(), Value::String(value));
        }
    }

    /// Write S3 bucket ownership controls
    async fn write_s3_bucket_ownership_controls(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &HashMap<String, Value>,
    ) -> ProviderResult<()> {
        if let Some(Value::String(val)) = attributes.get("object_ownership") {
            use aws_sdk_s3::types::{ObjectOwnership, OwnershipControls, OwnershipControlsRule};
            let normalized = extract_enum_value(val);
            let rule = OwnershipControlsRule::builder()
                .object_ownership(ObjectOwnership::from(normalized))
                .build()
                .map_err(|e| {
                    ProviderError::new(format!("Failed to build ownership controls rule: {}", e))
                        .for_resource(id.clone())
                })?;
            let controls = OwnershipControls::builder()
                .rules(rule)
                .build()
                .map_err(|e| {
                    ProviderError::new(format!("Failed to build ownership controls: {}", e))
                        .for_resource(id.clone())
                })?;
            self.s3_client
                .put_bucket_ownership_controls()
                .bucket(identifier)
                .ownership_controls(controls)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to put bucket ownership controls: {}", e))
                        .for_resource(id.clone())
                })?;
        }
        Ok(())
    }

    /// Read S3 bucket Object Lock status
    async fn read_s3_bucket_object_lock(
        &self,
        identifier: &str,
        attributes: &mut HashMap<String, Value>,
    ) {
        match self
            .s3_client
            .get_object_lock_configuration()
            .bucket(identifier)
            .send()
            .await
        {
            Ok(output) => {
                let enabled = output
                    .object_lock_configuration()
                    .and_then(|config| config.object_lock_enabled())
                    .is_some();
                attributes.insert(
                    "object_lock_enabled_for_bucket".to_string(),
                    Value::Bool(enabled),
                );
            }
            Err(_) => {
                // ObjectLockConfigurationNotFoundError means Object Lock is not enabled
                attributes.insert(
                    "object_lock_enabled_for_bucket".to_string(),
                    Value::Bool(false),
                );
            }
        }
    }

    /// Read S3 bucket ACL
    async fn read_s3_bucket_acl(&self, identifier: &str, attributes: &mut HashMap<String, Value>) {
        let Ok(output) = self
            .s3_client
            .get_bucket_acl()
            .bucket(identifier)
            .send()
            .await
        else {
            return;
        };

        let owner_id = output
            .owner()
            .and_then(|o| o.id())
            .unwrap_or("")
            .to_string();

        let grants = output.grants();

        // Classify grants by permission, collecting grantee strings
        let mut full_control: Vec<String> = Vec::new();
        let mut read: Vec<String> = Vec::new();
        let mut read_acp: Vec<String> = Vec::new();
        let mut write: Vec<String> = Vec::new();
        let mut write_acp: Vec<String> = Vec::new();

        for grant in grants {
            let Some(grantee) = grant.grantee() else {
                continue;
            };
            let Some(permission) = grant.permission() else {
                continue;
            };

            // Build grantee string in header format
            let grantee_str = if let Some(uri) = grantee.uri() {
                format!("uri=\"{}\"", uri)
            } else if let Some(id) = grantee.id() {
                format!("id=\"{}\"", id)
            } else if let Some(email) = grantee.email_address() {
                format!("emailAddress=\"{}\"", email)
            } else {
                continue;
            };

            // Skip owner's FULL_CONTROL (it's implicit)
            let is_owner = grantee.id().is_some_and(|id| id == owner_id);

            use aws_sdk_s3::types::Permission;
            match permission {
                Permission::FullControl => {
                    if !is_owner {
                        full_control.push(grantee_str);
                    }
                }
                Permission::Read => read.push(grantee_str),
                Permission::ReadAcp => read_acp.push(grantee_str),
                Permission::Write => write.push(grantee_str),
                Permission::WriteAcp => write_acp.push(grantee_str),
                _ => {}
            }
        }

        // Try to infer canned ACL
        let canned_acl = infer_canned_acl(&full_control, &read, &read_acp, &write, &write_acp);

        if let Some(acl) = canned_acl {
            // When a canned ACL is inferred, only set `acl` — the grant fields
            // are the expansion of the canned ACL and would cause false diffs.
            attributes.insert("acl".to_string(), Value::String(acl.to_string()));
        } else {
            // No canned ACL matched — set individual grant fields
            if !full_control.is_empty() {
                attributes.insert(
                    "grant_full_control".to_string(),
                    Value::String(full_control.join(", ")),
                );
            }
            if !read.is_empty() {
                attributes.insert("grant_read".to_string(), Value::String(read.join(", ")));
            }
            if !read_acp.is_empty() {
                attributes.insert(
                    "grant_read_acp".to_string(),
                    Value::String(read_acp.join(", ")),
                );
            }
            if !write.is_empty() {
                attributes.insert("grant_write".to_string(), Value::String(write.join(", ")));
            }
            if !write_acp.is_empty() {
                attributes.insert(
                    "grant_write_acp".to_string(),
                    Value::String(write_acp.join(", ")),
                );
            }
        }
    }

    /// Write S3 bucket ACL
    async fn write_s3_bucket_acl(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &HashMap<String, Value>,
    ) -> ProviderResult<()> {
        let acl = extract_string_attr(attributes, "acl");
        let grant_full_control = extract_string_attr(attributes, "grant_full_control");
        let grant_read = extract_string_attr(attributes, "grant_read");
        let grant_read_acp = extract_string_attr(attributes, "grant_read_acp");
        let grant_write = extract_string_attr(attributes, "grant_write");
        let grant_write_acp = extract_string_attr(attributes, "grant_write_acp");

        let has_acl = acl.is_some()
            || grant_full_control.is_some()
            || grant_read.is_some()
            || grant_read_acp.is_some()
            || grant_write.is_some()
            || grant_write_acp.is_some();

        if !has_acl {
            return Ok(());
        }

        use aws_sdk_s3::types::BucketCannedAcl;
        let mut req = self.s3_client.put_bucket_acl().bucket(identifier);

        if let Some(val) = acl {
            let normalized = convert_enum_value(val);
            req = req.acl(BucketCannedAcl::from(normalized.as_str()));
        }
        if let Some(val) = grant_full_control {
            req = req.grant_full_control(val);
        }
        if let Some(val) = grant_read {
            req = req.grant_read(val);
        }
        if let Some(val) = grant_read_acp {
            req = req.grant_read_acp(val);
        }
        if let Some(val) = grant_write {
            req = req.grant_write(val);
        }
        if let Some(val) = grant_write_acp {
            req = req.grant_write_acp(val);
        }

        req.send().await.map_err(|e| {
            ProviderError::new(format!("Failed to put bucket ACL: {}", e)).for_resource(id.clone())
        })?;

        Ok(())
    }

    /// Read S3 bucket tags
    async fn read_s3_bucket_tags(&self, identifier: &str, attributes: &mut HashMap<String, Value>) {
        if let Ok(output) = self
            .s3_client
            .get_bucket_tagging()
            .bucket(identifier)
            .send()
            .await
        {
            let mut tag_map = HashMap::new();
            for tag in output.tag_set() {
                tag_map.insert(
                    tag.key().to_string(),
                    Value::String(tag.value().to_string()),
                );
            }
            if !tag_map.is_empty() {
                attributes.insert("tags".to_string(), Value::Map(tag_map));
            }
        }
    }

    /// Write S3 bucket tags
    async fn write_s3_bucket_tags(
        &self,
        id: &ResourceId,
        identifier: &str,
        attributes: &HashMap<String, Value>,
    ) -> ProviderResult<()> {
        if let Some(Value::Map(tag_map)) = attributes.get("tags") {
            use aws_sdk_s3::types::{Tag, Tagging};
            let tags: Vec<Tag> = tag_map
                .iter()
                .filter_map(|(k, v)| {
                    if let Value::String(val) = v {
                        Some(Tag::builder().key(k).value(val).build().ok()?)
                    } else {
                        None
                    }
                })
                .collect();

            let tagging = Tagging::builder()
                .set_tag_set(Some(tags))
                .build()
                .map_err(|e| {
                    ProviderError::new(format!("Failed to build tagging: {:?}", e))
                        .for_resource(id.clone())
                })?;

            self.s3_client
                .put_bucket_tagging()
                .bucket(identifier)
                .tagging(tagging)
                .send()
                .await
                .map_err(|e| {
                    ProviderError::new(format!("Failed to put bucket tags: {:?}", e))
                        .for_resource(id.clone())
                })?;
        }

        Ok(())
    }

    // ========== EC2 VPC Operations ==========

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

        let filter = Filter::builder().name("vpc-id").values(identifier).build();

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

            // Instance tenancy - return plain value, normalize_state_enums handles namespacing
            if let Some(tenancy) = vpc.instance_tenancy() {
                attributes.insert(
                    "instance_tenancy".to_string(),
                    Value::String(tenancy.as_str().to_string()),
                );
            }

            // Extract user-defined tags (excluding Name)
            if let Some(tags_value) = Self::ec2_tags_to_value(vpc.tags()) {
                attributes.insert("tags".to_string(), tags_value);
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

        // Apply tags
        self.apply_ec2_tags(&resource.id, vpc_id, &resource.attributes)
            .await?;

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

        // Update tags
        self.apply_ec2_tags(&id, &vpc_id, &to.attributes).await?;

        self.read_ec2_vpc(&id, Some(identifier)).await
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

        let filter = Filter::builder()
            .name("subnet-id")
            .values(identifier)
            .build();

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

        // Apply tags
        self.apply_ec2_tags(&resource.id, subnet_id, &resource.attributes)
            .await?;

        // Read back using subnet ID (reliable identifier)
        self.read_ec2_subnet(&resource.id, Some(subnet_id)).await
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

        let filter = Filter::builder()
            .name("internet-gateway-id")
            .values(identifier)
            .build();

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

        // Apply tags
        self.apply_ec2_tags(&resource.id, igw_id, &resource.attributes)
            .await?;

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

        let filter = Filter::builder()
            .name("route-table-id")
            .values(identifier)
            .build();

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

        // Apply tags
        self.apply_ec2_tags(&resource.id, rt_id, &resource.attributes)
            .await?;

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

    // ========== EC2 Route Operations ==========

    /// Read an EC2 Route (routes are identified by route_table_id + destination)
    async fn read_ec2_route(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
    ) -> ProviderResult<State> {
        // Routes are identified by route_table_id + destination_cidr_block
        // For read, we return not_found since we can't look up by identifier alone
        Ok(State::not_found(id.clone()))
    }

    /// Read an EC2 Route by route_table_id and destination_cidr_block
    pub async fn read_ec2_route_by_key(
        &self,
        name: &str,
        route_table_id: &str,
        destination_cidr_block: &str,
    ) -> ProviderResult<State> {
        let id = ResourceId::with_provider("aws", "ec2.route", name);

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

        // Route identifier is route_table_id|destination_cidr_block
        let identifier = format!("{}|{}", route_table_id, destination_cidr);
        Ok(State::existing(resource.id, resource.attributes).with_identifier(identifier))
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

            let region_dsl = format!("aws.Region.{}", self.region.replace('-', "_"));
            attributes.insert("region".to_string(), Value::String(region_dsl));

            if let Some(group_name) = sg.group_name() {
                attributes.insert(
                    "group_name".to_string(),
                    Value::String(group_name.to_string()),
                );
            }

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
        self.apply_ec2_tags(&resource.id, sg_id, &resource.attributes)
            .await?;

        // Read back using security group ID (reliable identifier)
        self.read_ec2_security_group(&resource.id, Some(sg_id))
            .await
    }

    // ========== STS Operations ==========

    /// Read STS caller identity (data source)
    ///
    /// Calls STS GetCallerIdentity and returns account_id, arn, user_id.
    /// Always succeeds regardless of identifier (STS doesn't need one).
    async fn read_sts_caller_identity(&self, id: &ResourceId) -> ProviderResult<State> {
        let response = self
            .sts_client
            .get_caller_identity()
            .send()
            .await
            .map_err(|e| {
                ProviderError::new(format!("Failed to get STS caller identity: {:?}", e))
                    .for_resource(id.clone())
            })?;

        let mut attributes = HashMap::new();
        if let Some(account) = response.account() {
            attributes.insert("account_id".to_string(), Value::String(account.to_string()));
        }
        if let Some(arn) = response.arn() {
            attributes.insert("arn".to_string(), Value::String(arn.to_string()));
        }
        if let Some(user_id) = response.user_id() {
            attributes.insert("user_id".to_string(), Value::String(user_id.to_string()));
        }

        Ok(State::existing(id.clone(), attributes))
    }

    // ========== EC2 Security Group Rule Operations ==========

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
            attributes.insert("group_id".to_string(), Value::String(sg_id.to_string()));
        }

        if let Some(protocol) = first_rule.ip_protocol() {
            // Keep protocol as raw string for comparison (tcp, udp, icmp, -1)
            attributes.insert(
                "ip_protocol".to_string(),
                Value::String(protocol.to_string()),
            );
        }

        if let Some(from_port) = first_rule.from_port() {
            attributes.insert("from_port".to_string(), Value::Int(from_port as i64));
        }

        if let Some(to_port) = first_rule.to_port() {
            attributes.insert("to_port".to_string(), Value::Int(to_port as i64));
        }

        // IPv4 CIDR
        if let Some(cidr_ip) = first_rule.cidr_ipv4() {
            attributes.insert("cidr_ip".to_string(), Value::String(cidr_ip.to_string()));
        }

        // IPv6 CIDR
        if let Some(cidr_ipv6) = first_rule.cidr_ipv6() {
            attributes.insert(
                "cidr_ipv6".to_string(),
                Value::String(cidr_ipv6.to_string()),
            );
        }

        // Description
        if let Some(description) = first_rule.description() {
            attributes.insert(
                "description".to_string(),
                Value::String(description.to_string()),
            );
        }

        // Prefix list ID (source for ingress, destination for egress)
        if let Some(prefix_list_id) = first_rule.prefix_list_id() {
            let attr_name = if is_ingress {
                "source_prefix_list_id"
            } else {
                "destination_prefix_list_id"
            };
            attributes.insert(
                attr_name.to_string(),
                Value::String(prefix_list_id.to_string()),
            );
        }

        // Referenced security group ID (source for ingress, destination for egress)
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

    /// Create an EC2 Security Group Rule
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
                    self.read_ec2_security_group_rule(&id, identifier.as_deref(), true)
                        .await
                }
                "ec2.security_group_egress" => {
                    self.read_ec2_security_group_rule(&id, identifier.as_deref(), false)
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
                    self.create_ec2_security_group_rule(resource, true).await
                }
                "ec2.security_group_egress" => {
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
                "s3.bucket" => self.update_s3_bucket(id, &identifier, to).await,
                "ec2.vpc" => self.update_ec2_vpc(id, &identifier, to).await,
                "ec2.subnet" => self.update_ec2_subnet(id, &identifier, to).await,
                "ec2.internet_gateway" => {
                    self.update_ec2_internet_gateway(id, &identifier, to).await
                }
                "ec2.route_table" => self.update_ec2_route_table(id, &identifier, to).await,
                "ec2.route" => self.update_ec2_route(id, &identifier, to).await,
                "ec2.security_group" => self.update_ec2_security_group(id, &identifier, to).await,
                "ec2.security_group_ingress" => {
                    self.update_ec2_security_group_rule(id, &identifier, to, true)
                        .await
                }
                "ec2.security_group_egress" => {
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
                "ec2.route" => {
                    // Route deletion requires route_table_id and destination_cidr_block
                    // which are not available from ResourceId alone.
                    // Routes are typically deleted when the route table is deleted.
                    Ok(())
                }
                "ec2.security_group" => self.delete_ec2_security_group(id, &identifier).await,
                "ec2.security_group_ingress" => {
                    self.delete_ec2_security_group_rule(id, &identifier, true)
                        .await
                }
                "ec2.security_group_egress" => {
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
                && let AttributeType::Custom {
                    name: type_name,
                    namespace: Some(ns),
                    to_dsl,
                    ..
                } = &attr_schema.attr_type
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
            && let AttributeType::Custom {
                name: type_name,
                namespace: Some(ns),
                to_dsl,
                ..
            } = &attr_schema.attr_type
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

fn extract_string_attr<'a>(attributes: &'a HashMap<String, Value>, key: &str) -> Option<&'a str> {
    match attributes.get(key) {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

const ALL_USERS_URI: &str = "http://acs.amazonaws.com/groups/global/AllUsers";
const AUTH_USERS_URI: &str = "http://acs.amazonaws.com/groups/global/AuthenticatedUsers";

/// Infer a canned ACL from the grant lists.
/// Returns None if the grants don't match any known canned ACL pattern.
fn infer_canned_acl(
    full_control: &[String],
    read: &[String],
    read_acp: &[String],
    write: &[String],
    write_acp: &[String],
) -> Option<&'static str> {
    let all_users_read = format!("uri=\"{}\"", ALL_USERS_URI);
    let all_users_write = format!("uri=\"{}\"", ALL_USERS_URI);
    let auth_users_read = format!("uri=\"{}\"", AUTH_USERS_URI);

    // private: no non-owner grants
    if full_control.is_empty()
        && read.is_empty()
        && read_acp.is_empty()
        && write.is_empty()
        && write_acp.is_empty()
    {
        return Some("private");
    }

    // public-read: AllUsers READ, nothing else
    if full_control.is_empty()
        && read.len() == 1
        && read[0] == all_users_read
        && read_acp.is_empty()
        && write.is_empty()
        && write_acp.is_empty()
    {
        return Some("public-read");
    }

    // public-read-write: AllUsers READ + WRITE, nothing else
    if full_control.is_empty()
        && read.len() == 1
        && read[0] == all_users_read
        && read_acp.is_empty()
        && write.len() == 1
        && write[0] == all_users_write
        && write_acp.is_empty()
    {
        return Some("public-read-write");
    }

    // authenticated-read: AuthenticatedUsers READ, nothing else
    if full_control.is_empty()
        && read.len() == 1
        && read[0] == auth_users_read
        && read_acp.is_empty()
        && write.is_empty()
        && write_acp.is_empty()
    {
        return Some("authenticated-read");
    }

    None
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

    #[test]
    fn test_infer_canned_acl_private() {
        let result = infer_canned_acl(&[], &[], &[], &[], &[]);
        assert_eq!(result, Some("private"));
    }

    #[test]
    fn test_infer_canned_acl_public_read() {
        let all_users_read = format!("uri=\"{}\"", ALL_USERS_URI);
        let result = infer_canned_acl(&[], &[all_users_read], &[], &[], &[]);
        assert_eq!(result, Some("public-read"));
    }

    #[test]
    fn test_infer_canned_acl_public_read_write() {
        let all_users_read = format!("uri=\"{}\"", ALL_USERS_URI);
        let all_users_write = format!("uri=\"{}\"", ALL_USERS_URI);
        let result = infer_canned_acl(&[], &[all_users_read], &[], &[all_users_write], &[]);
        assert_eq!(result, Some("public-read-write"));
    }

    #[test]
    fn test_infer_canned_acl_authenticated_read() {
        let auth_users_read = format!("uri=\"{}\"", AUTH_USERS_URI);
        let result = infer_canned_acl(&[], &[auth_users_read], &[], &[], &[]);
        assert_eq!(result, Some("authenticated-read"));
    }

    #[test]
    fn test_infer_canned_acl_unknown() {
        let custom = vec!["id=\"abc123\"".to_string()];
        let result = infer_canned_acl(&custom, &[], &[], &[], &[]);
        assert_eq!(result, None);
    }
}
