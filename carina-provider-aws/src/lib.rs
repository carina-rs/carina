//! Carina AWS Provider
//!
//! AWS Provider implementation

mod ec2_security_group_rules;
mod ec2_tags;
mod factory;
pub(crate) mod helpers;
mod normalizer;
mod provider;
pub mod provider_generated;
pub mod schemas;
mod services;
#[cfg(test)]
mod tests;

pub use factory::AwsProviderFactory;
pub use normalizer::AwsNormalizer;

use aws_config::Region;
use aws_sdk_cloudwatchlogs::Client as CloudWatchLogsClient;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_iam::Client as IamClient;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_sts::Client as StsClient;

/// AWS Provider
pub struct AwsProvider {
    s3_client: S3Client,
    ec2_client: Ec2Client,
    iam_client: IamClient,
    logs_client: CloudWatchLogsClient,
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
            iam_client: IamClient::new(&config),
            logs_client: CloudWatchLogsClient::new(&config),
            sts_client: StsClient::new(&config),
            region: region.to_string(),
        }
    }

    /// Create with specific clients (for testing)
    pub fn with_clients(
        s3_client: S3Client,
        ec2_client: Ec2Client,
        iam_client: IamClient,
        logs_client: CloudWatchLogsClient,
        sts_client: StsClient,
        region: String,
    ) -> Self {
        Self {
            s3_client,
            ec2_client,
            iam_client,
            logs_client,
            sts_client,
            region,
        }
    }
}
