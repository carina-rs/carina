//! Carina AWS Provider
//!
//! AWS Provider implementation

use std::collections::HashMap;

use aws_config::Region;
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

/// AWS Provider
pub struct AwsProvider {
    s3_client: S3Client,
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
            region: region.to_string(),
        }
    }

    /// Create with a specific S3 client (for testing)
    pub fn with_client(s3_client: S3Client, region: String) -> Self {
        Self { s3_client, region }
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
                        if rule.id() == Some("auto-expiration") {
                            if let Some(expiration) = rule.expiration() {
                                if let Some(days) = expiration.days {
                                    attributes.insert(
                                        "expiration_days".to_string(),
                                        Value::Int(days as i64),
                                    );
                                }
                            }
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
                    Err(ProviderError::new(format!("Failed to read bucket: {:?}", err))
                        .for_resource(id))
                }
            }
        }
    }

    /// Create an S3 bucket
    async fn create_s3_bucket(&self, resource: Resource) -> ProviderResult<State> {
        let bucket_name = match resource.attributes.get("name") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(ProviderError::new("Bucket name is required")
                    .for_resource(resource.id.clone()));
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
                BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration,
                LifecycleRule, LifecycleRuleFilter,
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
                BucketLifecycleConfiguration, ExpirationStatus, LifecycleExpiration,
                LifecycleRule, LifecycleRuleFilter,
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
}

impl Provider for AwsProvider {
    fn name(&self) -> &'static str {
        "aws"
    }

    fn resource_types(&self) -> Vec<Box<dyn ResourceType>> {
        vec![Box::new(S3BucketType)]
    }

    fn read(&self, id: &ResourceId) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move {
            match id.resource_type.as_str() {
                "s3_bucket" => self.read_s3_bucket(&id.name).await,
                _ => Err(
                    ProviderError::new(format!("Unknown resource type: {}", id.resource_type))
                        .for_resource(id.clone()),
                ),
            }
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let resource = resource.clone();
        Box::pin(async move {
            match resource.id.resource_type.as_str() {
                "s3_bucket" => self.create_s3_bucket(resource).await,
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
                _ => Err(
                    ProviderError::new(format!("Unknown resource type: {}", id.resource_type))
                        .for_resource(id.clone()),
                ),
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
        assert_eq!(
            convert_region_value("aws.Region.us_east_1"),
            "us-east-1"
        );
        assert_eq!(convert_region_value("eu-west-1"), "eu-west-1");
    }

    #[test]
    fn test_s3_bucket_type_name() {
        let bucket_type = S3BucketType;
        assert_eq!(bucket_type.name(), "s3_bucket");
    }
}
