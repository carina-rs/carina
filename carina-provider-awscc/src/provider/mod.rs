//! AWS Cloud Control Provider implementation
//!
//! This module contains the main provider implementation that communicates
//! with AWS Cloud Control API to manage resources.
//!
//! ## Sub-modules
//!
//! - `cloudcontrol` - Low-level Cloud Control API methods and retry/error handling
//! - `conversion` - Value conversion between DSL and AWS JSON formats
//! - `normalizer` - Plan-time enum resolution and state hydration
//! - `operations` - High-level resource operations (read, create, update, delete)
//! - `s3` - S3-specific operations (empty bucket for force_delete)
//! - `special_cases` - Resource-type-specific attribute handling
//! - `tags` - Tag conversion between DSL and CloudFormation formats
//! - `update` - Update patch building and resource property parsing

mod cloudcontrol;
pub(crate) mod conversion;
mod normalizer;
mod operations;
mod s3;
mod special_cases;
mod tags;
pub(crate) mod update;

use aws_config::Region;
use aws_sdk_cloudcontrol::Client as CloudControlClient;

use crate::schemas::generated::AwsccSchemaConfig;

// Re-export public API
pub use normalizer::{resolve_enum_identifiers_impl, restore_unreturned_attrs_impl};
pub(crate) use update::parse_resource_properties;

/// Maximum number of retry attempts for retryable create errors
const CREATE_RETRY_MAX_ATTEMPTS: u32 = 12;

/// Initial delay in seconds before retrying a failed create operation
const CREATE_RETRY_INITIAL_DELAY_SECS: u64 = 10;

/// Maximum delay in seconds between create retry attempts
const CREATE_RETRY_MAX_DELAY_SECS: u64 = 120;

/// Maximum number of retry attempts for retryable delete errors
const DELETE_RETRY_MAX_ATTEMPTS: u32 = 12;

/// Initial delay in seconds before retrying a failed delete operation
const DELETE_RETRY_INITIAL_DELAY_SECS: u64 = 10;

/// Maximum delay in seconds between delete retry attempts
const DELETE_RETRY_MAX_DELAY_SECS: u64 = 120;

/// AWS Cloud Control Provider
pub struct AwsccProvider {
    cloudcontrol_client: CloudControlClient,
    aws_config: aws_config::SdkConfig,
}

impl AwsccProvider {
    /// Create a new AwsccProvider for the specified region
    pub async fn new(region: &str) -> Self {
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(Region::new(region.to_string()))
            .load()
            .await;

        Self {
            cloudcontrol_client: CloudControlClient::new(&config),
            aws_config: config,
        }
    }
}

/// Get the AwsccSchemaConfig for a resource type
fn get_schema_config(resource_type: &str) -> Option<AwsccSchemaConfig> {
    crate::schemas::generated::configs().into_iter().find(|c| {
        // Match by schema resource_type: "awscc.ec2.vpc" -> "ec2.vpc"
        c.schema
            .resource_type
            .strip_prefix("awscc.")
            .map(|t| t == resource_type)
            .unwrap_or(false)
    })
}
