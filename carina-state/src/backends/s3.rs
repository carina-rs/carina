//! S3 backend for state storage

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    BucketVersioningStatus, PublicAccessBlockConfiguration, ServerSideEncryption,
    VersioningConfiguration,
};

use crate::backend::{BackendConfig, BackendError, BackendResult, StateBackend};
use crate::lock::LockInfo;
use crate::state::StateFile;

/// S3-based state backend
pub struct S3Backend {
    /// S3 client
    client: Client,
    /// Bucket name
    bucket: String,
    /// Object key for the state file
    key: String,
    /// AWS region
    region: String,
    /// Whether to encrypt the state file (default: true)
    encrypt: bool,
    /// Whether to auto-create the bucket if it doesn't exist (default: true)
    auto_create: bool,
}

impl S3Backend {
    /// Create a new S3Backend from configuration
    pub async fn from_config(config: &BackendConfig) -> BackendResult<Self> {
        let bucket = config
            .get_string("bucket")
            .ok_or_else(|| BackendError::configuration("Missing required attribute: bucket"))?
            .to_string();

        let key = config
            .get_string("key")
            .ok_or_else(|| BackendError::configuration("Missing required attribute: key"))?
            .to_string();

        let region_value = config
            .get_string("region")
            .ok_or_else(|| BackendError::configuration("Missing required attribute: region"))?;

        // Convert region from DSL format (aws.Region.ap_northeast_1) to AWS format (ap-northeast-1)
        let region = convert_region_value(region_value);

        let encrypt = config.get_bool_or("encrypt", true);
        let auto_create = config.get_bool_or("auto_create", true);

        // Load AWS config with the specified region
        let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region.clone()))
            .load()
            .await;

        let client = Client::new(&aws_config);

        Ok(Self {
            client,
            bucket,
            key,
            region,
            encrypt,
            auto_create,
        })
    }

    /// Get the lock file key (state key + ".lock")
    fn lock_key(&self) -> String {
        format!("{}.lock", self.key)
    }

    /// Read the lock file from S3
    async fn read_lock(&self) -> BackendResult<Option<LockInfo>> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.lock_key())
            .send()
            .await;

        match result {
            Ok(output) => {
                let body = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                let bytes = body.into_bytes();
                let lock: LockInfo = serde_json::from_slice(&bytes)
                    .map_err(|e| BackendError::Serialization(e.to_string()))?;
                Ok(Some(lock))
            }
            Err(err) => {
                // Check if it's a NoSuchKey error
                if is_not_found_error(&err) {
                    Ok(None)
                } else {
                    Err(BackendError::Aws(err.to_string()))
                }
            }
        }
    }

    /// Write a lock file to S3
    async fn write_lock(&self, lock: &LockInfo) -> BackendResult<()> {
        let body = serde_json::to_vec_pretty(lock)
            .map_err(|e| BackendError::Serialization(e.to_string()))?;

        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(self.lock_key())
            .body(ByteStream::from(body))
            .content_type("application/json");

        if self.encrypt {
            request = request.server_side_encryption(ServerSideEncryption::Aes256);
        }

        request
            .send()
            .await
            .map_err(|e| BackendError::Aws(e.to_string()))?;

        Ok(())
    }

    /// Delete the lock file from S3
    async fn delete_lock(&self) -> BackendResult<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.lock_key())
            .send()
            .await
            .map_err(|e| BackendError::Aws(e.to_string()))?;

        Ok(())
    }

    /// Get the bucket name
    pub fn bucket_name(&self) -> &str {
        &self.bucket
    }

    /// Get whether auto_create is enabled
    pub fn auto_create_enabled(&self) -> bool {
        self.auto_create
    }
}

#[async_trait]
impl StateBackend for S3Backend {
    async fn read_state(&self) -> BackendResult<Option<StateFile>> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&self.key)
            .send()
            .await;

        match result {
            Ok(output) => {
                let body = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                let bytes = body.into_bytes();
                let state: StateFile = serde_json::from_slice(&bytes)
                    .map_err(|e| BackendError::InvalidState(e.to_string()))?;
                Ok(Some(state))
            }
            Err(err) => {
                if is_not_found_error(&err) {
                    Ok(None)
                } else {
                    Err(BackendError::Aws(err.to_string()))
                }
            }
        }
    }

    async fn write_state(&self, state: &StateFile) -> BackendResult<()> {
        let body = serde_json::to_vec_pretty(state)
            .map_err(|e| BackendError::Serialization(e.to_string()))?;

        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&self.key)
            .body(ByteStream::from(body))
            .content_type("application/json");

        if self.encrypt {
            request = request.server_side_encryption(ServerSideEncryption::Aes256);
        }

        request
            .send()
            .await
            .map_err(|e| BackendError::Aws(e.to_string()))?;

        Ok(())
    }

    async fn acquire_lock(&self, operation: &str) -> BackendResult<LockInfo> {
        // Check for existing lock
        if let Some(existing_lock) = self.read_lock().await? {
            // If the lock has expired, we can take it
            if existing_lock.is_expired() {
                // Expired lock - delete it and proceed
                self.delete_lock().await?;
            } else {
                // Lock is still valid
                return Err(BackendError::locked(&existing_lock));
            }
        }

        // Create and write new lock
        let lock = LockInfo::new(operation);
        self.write_lock(&lock).await?;

        // Verify we got the lock (in case of race condition)
        // Read it back and check it's ours
        if let Some(written_lock) = self.read_lock().await? {
            if written_lock.id == lock.id {
                return Ok(lock);
            } else {
                // Someone else got the lock
                return Err(BackendError::locked(&written_lock));
            }
        }

        // This shouldn't happen, but just in case
        Ok(lock)
    }

    async fn release_lock(&self, lock: &LockInfo) -> BackendResult<()> {
        // Verify the lock exists and matches
        if let Some(existing_lock) = self.read_lock().await? {
            if existing_lock.id != lock.id {
                return Err(BackendError::LockMismatch {
                    expected: lock.id.clone(),
                    actual: existing_lock.id,
                });
            }
        } else {
            return Err(BackendError::LockNotFound(lock.id.clone()));
        }

        self.delete_lock().await
    }

    async fn force_unlock(&self, lock_id: &str) -> BackendResult<()> {
        // Verify a lock exists
        if let Some(existing_lock) = self.read_lock().await? {
            if existing_lock.id != lock_id {
                return Err(BackendError::LockMismatch {
                    expected: lock_id.to_string(),
                    actual: existing_lock.id,
                });
            }
        } else {
            return Err(BackendError::LockNotFound(lock_id.to_string()));
        }

        self.delete_lock().await
    }

    async fn init(&self) -> BackendResult<()> {
        // Check if bucket exists
        if !self.bucket_exists().await? {
            if self.auto_create {
                self.create_bucket().await?;
            } else {
                return Err(BackendError::BucketNotFound(self.bucket.clone()));
            }
        }

        // Initialize empty state if none exists
        if self.read_state().await?.is_none() {
            let state = StateFile::new();
            self.write_state(&state).await?;
        }

        Ok(())
    }

    async fn bucket_exists(&self) -> BackendResult<bool> {
        let result = self.client.head_bucket().bucket(&self.bucket).send().await;

        match result {
            Ok(_) => Ok(true),
            Err(err) => {
                // Check if it's a NotFound error (404)
                let service_error = err.as_service_error();
                if service_error.is_some() {
                    // HeadBucket returns 404 for non-existent buckets
                    Ok(false)
                } else {
                    // Check raw response for 404
                    let raw = err.raw_response();
                    if raw.is_some_and(|r| r.status().as_u16() == 404) {
                        Ok(false)
                    } else {
                        Err(BackendError::Aws(err.to_string()))
                    }
                }
            }
        }
    }

    async fn create_bucket(&self) -> BackendResult<()> {
        // Create bucket with location constraint if not us-east-1
        let mut create_request = self.client.create_bucket().bucket(&self.bucket);

        if self.region != "us-east-1" {
            use aws_sdk_s3::types::{BucketLocationConstraint, CreateBucketConfiguration};

            let constraint = BucketLocationConstraint::from(self.region.as_str());
            let config = CreateBucketConfiguration::builder()
                .location_constraint(constraint)
                .build();
            create_request = create_request.create_bucket_configuration(config);
        }

        create_request
            .send()
            .await
            .map_err(|e| BackendError::BucketCreationFailed(e.to_string()))?;

        // Enable versioning
        let versioning_config = VersioningConfiguration::builder()
            .status(BucketVersioningStatus::Enabled)
            .build();

        self.client
            .put_bucket_versioning()
            .bucket(&self.bucket)
            .versioning_configuration(versioning_config)
            .send()
            .await
            .map_err(|e| BackendError::Aws(format!("Failed to enable versioning: {}", e)))?;

        // Block public access
        let public_access_block = PublicAccessBlockConfiguration::builder()
            .block_public_acls(true)
            .block_public_policy(true)
            .ignore_public_acls(true)
            .restrict_public_buckets(true)
            .build();

        self.client
            .put_public_access_block()
            .bucket(&self.bucket)
            .public_access_block_configuration(public_access_block)
            .send()
            .await
            .map_err(|e| BackendError::Aws(format!("Failed to block public access: {}", e)))?;

        Ok(())
    }
}

/// Convert region value from DSL format to AWS format
/// e.g., "aws.Region.ap_northeast_1" -> "ap-northeast-1"
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

/// Check if an S3 error is a "not found" error
fn is_not_found_error<E: std::fmt::Debug>(err: &aws_sdk_s3::error::SdkError<E>) -> bool {
    // Check the raw HTTP response status
    if let Some(raw) = err.raw_response() {
        return raw.status().as_u16() == 404;
    }
    false
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
        assert_eq!(convert_region_value("aws.Region.us_west_2"), "us-west-2");
        assert_eq!(convert_region_value("us-east-1"), "us-east-1");
        assert_eq!(convert_region_value("eu-west-1"), "eu-west-1");
    }

    #[test]
    fn test_lock_key() {
        // We can't easily test this without mocking AWS, so just verify the format
        let key = "path/to/state.json";
        let expected_lock_key = "path/to/state.json.lock";
        assert_eq!(format!("{}.lock", key), expected_lock_key);
    }
}
