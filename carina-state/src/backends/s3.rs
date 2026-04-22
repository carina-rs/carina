//! S3 backend for state storage

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::operation::head_bucket::HeadBucketError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{
    BucketVersioningStatus, PublicAccessBlockConfiguration, ServerSideEncryption,
    VersioningConfiguration,
};

use carina_core::utils::convert_region_value;

use crate::backend::{BackendConfig, BackendError, BackendResult, StateBackend};
use crate::lock::LockInfo;
use crate::state::{self, StateFile};

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

/// Fallback region used when `region` is not explicitly configured and
/// the bucket does not yet exist (auto_create case).
const DEFAULT_BOOTSTRAP_REGION: &str = "us-east-1";

impl S3Backend {
    /// Create a new S3Backend from configuration.
    ///
    /// The `region` attribute is optional. When omitted, the region is
    /// auto-discovered by calling `GetBucketLocation` on the bucket. If the
    /// bucket does not exist, the backend falls back to `us-east-1` so that
    /// `auto_create` can create the bucket in that region.
    pub async fn from_config(config: &BackendConfig) -> BackendResult<Self> {
        let bucket = config
            .get_string("bucket")
            .ok_or_else(|| BackendError::configuration("Missing required attribute: bucket"))?
            .to_string();

        let key = config
            .get_string("key")
            .ok_or_else(|| BackendError::configuration("Missing required attribute: key"))?
            .to_string();

        let encrypt = config.get_bool_or("encrypt", true);
        let auto_create = config.get_bool_or("auto_create", true);

        let (region, client) = match config.get_string("region") {
            Some(v) => {
                let region = convert_region_value(v);
                let client = build_s3_client(&region).await;
                (region, client)
            }
            None => {
                // Discover via GetBucketLocation, reusing the bootstrap client
                // when the bucket turns out to live in DEFAULT_BOOTSTRAP_REGION.
                let bootstrap_client = build_s3_client(DEFAULT_BOOTSTRAP_REGION).await;
                let discovered = discover_bucket_region(&bootstrap_client, &bucket).await?;
                let client = if discovered == DEFAULT_BOOTSTRAP_REGION {
                    bootstrap_client
                } else {
                    build_s3_client(&discovered).await
                };
                (discovered, client)
            }
        };

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
    async fn read_lock_with_etag(&self) -> BackendResult<Option<(LockInfo, String)>> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.lock_key())
            .send()
            .await;

        match result {
            Ok(output) => {
                let etag = output
                    .e_tag()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| BackendError::Aws("Lock object missing ETag".to_string()))?;
                let body = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                let bytes = body.into_bytes();
                let lock: LockInfo = serde_json::from_slice(&bytes)
                    .map_err(|e| BackendError::Serialization(e.to_string()))?;
                Ok(Some((lock, etag)))
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

    async fn read_lock(&self) -> BackendResult<Option<LockInfo>> {
        Ok(self.read_lock_with_etag().await?.map(|(lock, _)| lock))
    }

    fn lock_body(lock: &LockInfo) -> BackendResult<Vec<u8>> {
        serde_json::to_vec_pretty(lock).map_err(|e| BackendError::Serialization(e.to_string()))
    }

    async fn write_lock_if_absent(&self, lock: &LockInfo) -> BackendResult<bool> {
        self.write_lock(lock, Some("*"), None).await
    }

    async fn replace_lock_if_match(&self, lock: &LockInfo, etag: &str) -> BackendResult<bool> {
        self.write_lock(lock, None, Some(etag)).await
    }

    /// Write a lock file to S3 using conditional headers for atomic acquisition.
    async fn write_lock(
        &self,
        lock: &LockInfo,
        if_none_match: Option<&str>,
        if_match: Option<&str>,
    ) -> BackendResult<bool> {
        let body = Self::lock_body(lock)?;

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

        if let Some(value) = if_none_match {
            request = request.if_none_match(value);
        }

        if let Some(value) = if_match {
            request = request.if_match(value);
        }

        match request.send().await {
            Ok(_) => Ok(true),
            Err(err) if is_conditional_write_conflict(&err) => Ok(false),
            Err(err) => Err(BackendError::Aws(err.to_string())),
        }
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
                let state = state::check_and_migrate_bytes(&bytes)?;
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
        let lock = LockInfo::new(operation);
        loop {
            if self.write_lock_if_absent(&lock).await? {
                return Ok(lock);
            }

            let Some((existing_lock, etag)) = self.read_lock_with_etag().await? else {
                continue;
            };

            if !existing_lock.is_expired() {
                return Err(BackendError::locked(&existing_lock));
            }

            if self.replace_lock_if_match(&lock, &etag).await? {
                return Ok(lock);
            }
        }
    }

    async fn renew_lock(&self, lock: &LockInfo) -> BackendResult<LockInfo> {
        // Read the current lock and its ETag
        let Some((existing_lock, etag)) = self.read_lock_with_etag().await? else {
            return Err(BackendError::LockNotHeld(
                "lock file no longer exists".to_string(),
            ));
        };

        if existing_lock.id != lock.id {
            return Err(BackendError::LockNotHeld(format!(
                "lock {} was replaced by {}",
                lock.id, existing_lock.id
            )));
        }

        // Write a renewed lock, conditioned on the current ETag
        let renewed = lock.renewed();
        if !self.replace_lock_if_match(&renewed, &etag).await? {
            return Err(BackendError::LockNotHeld(
                "lock was modified concurrently during renewal".to_string(),
            ));
        }

        Ok(renewed)
    }

    async fn write_state_locked(&self, state: &StateFile, lock: &LockInfo) -> BackendResult<()> {
        // Verify the lock is still held by us before writing state
        let Some(existing_lock) = self.read_lock().await? else {
            return Err(BackendError::LockNotHeld(
                "lock file no longer exists".to_string(),
            ));
        };

        if existing_lock.id != lock.id {
            return Err(BackendError::LockNotHeld(format!(
                "lock {} was replaced by {}",
                lock.id, existing_lock.id
            )));
        }

        self.write_state(state).await
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
                if is_missing_head_bucket_response(&err) {
                    Ok(false)
                } else {
                    Err(BackendError::Aws(err.to_string()))
                }
            }
        }
    }

    fn provider_name(&self) -> Option<&str> {
        Some("aws")
    }

    fn resource_type(&self) -> Option<&str> {
        Some("s3.Bucket")
    }

    fn resource_definition(&self, name: &str) -> Option<String> {
        Some(format!(
            "\n# Auto-generated by carina (state bucket)\naws.s3.Bucket {{\n    bucket            = \"{}\"\n    versioning_status = Enabled\n}}\n",
            name
        ))
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

/// Check if an S3 error is a "not found" error
fn is_not_found_error<E: std::fmt::Debug>(err: &aws_sdk_s3::error::SdkError<E>) -> bool {
    // Check the raw HTTP response status
    if let Some(raw) = err.raw_response() {
        return raw.status().as_u16() == 404;
    }
    false
}

/// Build an S3 client configured for the given region.
async fn build_s3_client(region: &str) -> Client {
    let aws_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new(region.to_string()))
        .load()
        .await;
    Client::new(&aws_config)
}

/// Auto-discover the AWS region of an existing S3 bucket via `GetBucketLocation`.
///
/// The AWS SDK handles cross-region redirects automatically, so the client used
/// here can be configured for any region. If the bucket does not exist
/// (NoSuchBucket), returns `DEFAULT_BOOTSTRAP_REGION` so that `auto_create` can
/// create the bucket in that region. Other errors are propagated.
async fn discover_bucket_region(client: &Client, bucket: &str) -> BackendResult<String> {
    match client.get_bucket_location().bucket(bucket).send().await {
        Ok(output) => {
            // Empty constraint → us-east-1 (legacy behavior per AWS docs).
            let region = output
                .location_constraint()
                .map(|c| c.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_BOOTSTRAP_REGION);
            Ok(normalize_location_constraint(region).to_string())
        }
        Err(err) => {
            if is_no_such_bucket(&err) {
                Ok(DEFAULT_BOOTSTRAP_REGION.to_string())
            } else {
                Err(BackendError::Aws(format!(
                    "Failed to discover bucket region for '{bucket}': {err}"
                )))
            }
        }
    }
}

/// Normalize legacy `LocationConstraint` values to modern AWS region codes.
///
/// Some historical values don't match the modern region code — notably `"EU"`
/// which maps to `eu-west-1`.
fn normalize_location_constraint(constraint: &str) -> &str {
    match constraint {
        "EU" => "eu-west-1",
        other => other,
    }
}

/// Check whether an SDK error indicates a missing bucket (NoSuchBucket).
fn is_no_such_bucket<E, R>(err: &aws_sdk_s3::error::SdkError<E, R>) -> bool
where
    E: ProvideErrorMetadata,
{
    err.as_service_error()
        .and_then(|e| e.code())
        .is_some_and(|c| c == "NoSuchBucket")
}

fn is_conditional_write_conflict_code(code: Option<&str>) -> bool {
    matches!(
        code,
        Some("ConditionalRequestConflict" | "PreconditionFailed")
    )
}

fn is_conditional_write_conflict<E>(err: &aws_sdk_s3::error::SdkError<E>) -> bool
where
    E: std::fmt::Debug + ProvideErrorMetadata,
{
    if let Some(raw) = err.raw_response() {
        return matches!(raw.status().as_u16(), 409 | 412);
    }

    err.as_service_error()
        .is_some_and(|service_err| is_conditional_write_conflict_code(service_err.code()))
}

fn is_head_bucket_not_found(err: &HeadBucketError) -> bool {
    err.is_not_found()
}

fn is_missing_head_bucket_response<R>(
    err: &aws_sdk_s3::error::SdkError<HeadBucketError, R>,
) -> bool {
    err.as_service_error().is_some_and(is_head_bucket_not_found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::error::{ErrorMetadata, SdkError};

    #[test]
    fn test_convert_region_value() {
        assert_eq!(
            convert_region_value("aws.Region.ap_northeast_1"),
            "ap-northeast-1"
        );
        assert_eq!(convert_region_value("aws.Region.us_west_2"), "us-west-2");
        assert_eq!(
            convert_region_value("awscc.Region.ap_northeast_1"),
            "ap-northeast-1"
        );
        assert_eq!(convert_region_value("awscc.Region.us_west_2"), "us-west-2");
        assert_eq!(convert_region_value("us-east-1"), "us-east-1");
        assert_eq!(convert_region_value("eu-west-1"), "eu-west-1");
    }

    #[test]
    fn test_normalize_location_constraint_legacy_eu() {
        assert_eq!(normalize_location_constraint("EU"), "eu-west-1");
    }

    #[test]
    fn test_normalize_location_constraint_passthrough() {
        assert_eq!(
            normalize_location_constraint("ap-northeast-1"),
            "ap-northeast-1"
        );
        assert_eq!(normalize_location_constraint("us-west-2"), "us-west-2");
    }

    #[test]
    fn test_lock_key() {
        // We can't easily test this without mocking AWS, so just verify the format
        let key = "path/to/state.json";
        let expected_lock_key = "path/to/state.json.lock";
        assert_eq!(format!("{}.lock", key), expected_lock_key);
    }

    #[test]
    fn test_s3_resource_definition_format() {
        // Test the resource definition format directly (same logic as S3Backend::resource_definition)
        let name = "my-state-bucket";
        let resource_def = format!(
            "\n# Auto-generated by carina (state bucket)\naws.s3.Bucket {{\n    bucket            = \"{}\"\n    versioning_status = Enabled\n}}\n",
            name
        );
        assert!(resource_def.contains("aws.s3.Bucket {"));
        assert!(resource_def.contains("my-state-bucket"));
        assert!(resource_def.contains("versioning_status = Enabled"));
        assert!(resource_def.contains("# Auto-generated by carina"));
    }

    #[test]
    fn test_is_head_bucket_not_found_for_not_found_error() {
        let err = HeadBucketError::NotFound(aws_sdk_s3::types::error::NotFound::builder().build());
        assert!(is_head_bucket_not_found(&err));
    }

    #[test]
    fn test_is_head_bucket_not_found_rejects_other_service_errors() {
        let err = HeadBucketError::generic(ErrorMetadata::builder().code("AccessDenied").build());
        assert!(!is_head_bucket_not_found(&err));
    }

    #[test]
    fn test_is_missing_head_bucket_response_for_not_found_service_error() {
        let err = SdkError::service_error(
            HeadBucketError::NotFound(aws_sdk_s3::types::error::NotFound::builder().build()),
            (),
        );
        assert!(is_missing_head_bucket_response(&err));
    }

    #[test]
    fn test_is_missing_head_bucket_response_rejects_non_service_404s() {
        let err = SdkError::response_error("not found response", ());
        assert!(!is_missing_head_bucket_response(&err));
    }

    #[test]
    fn test_is_conditional_write_conflict_code() {
        assert!(is_conditional_write_conflict_code(Some(
            "ConditionalRequestConflict"
        )));
        assert!(is_conditional_write_conflict_code(Some(
            "PreconditionFailed"
        )));
        assert!(!is_conditional_write_conflict_code(Some("AccessDenied")));
        assert!(!is_conditional_write_conflict_code(None));
    }
}
