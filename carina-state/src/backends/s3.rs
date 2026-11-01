//! S3 backend for state storage

use async_trait::async_trait;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::retry::RetryConfig;
use aws_sdk_s3::config::timeout::TimeoutConfig;
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::operation::head_bucket::HeadBucketError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{PublicAccessBlockConfiguration, ServerSideEncryption};

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::backend::{AwsError, BackendError, BackendResult, RawState, StateBackend};
use crate::backend_config::S3BackendSettings;
use crate::lock::LockInfo;
use crate::state::{self, LoadedState, MigrationInfo, StateFile, log_state_migration_once};

use super::{S3_BACKEND_PROVIDER_NAME, S3_BACKEND_RESOURCE_TYPE};

const REGION_UNRESOLVED_MESSAGE: &str = "S3 backend has no region: set `region` in the backend block, or configure AWS_REGION / a profile region";

// GitHub Actions cancellation is SIGINT -> 7.5s -> SIGTERM -> 2.5s ->
// SIGKILL. Cleanup has a 2s hard deadline and a 1s state-flush budget. A
// full-state read resolves the ETag cache to either present or absent, so the
// steady-state locked flush performs lock GET + lock PUT + state PUT: 3 *
// 300ms = 900ms.
// This also holds for a known-absent first state, whose PUT uses If-None-Match
// directly. Lock release performs GET + DELETE, so all five calls take at most
// 1.5s and leave 500ms for scheduling and serialization. A conflict is an
// expected cleanup case when an aborted checkpoint PUT commits after the final
// flush starts: lock GET + lock PUT + state PUT (412) + HEAD + lock GET + state
// PUT is 6 * 300ms = 1.8s, beyond the 1s state-flush budget. Cleanup may abandon
// that retry, but the late checkpoint that caused the conflict is already
// durable, and every later state PUT remains conditional. Normal operations
// retain SDK retries.
const CLEANUP_S3_OPERATION_TIMEOUT: Duration = Duration::from_millis(300);
const STATE_WRITE_MAX_ATTEMPTS: usize = 5;

fn cleanup_operation_config() -> aws_sdk_s3::config::Builder {
    aws_sdk_s3::Config::builder()
        .retry_config(RetryConfig::standard().with_max_attempts(1))
        .timeout_config(
            TimeoutConfig::builder()
                .operation_timeout(CLEANUP_S3_OPERATION_TIMEOUT)
                .operation_attempt_timeout(CLEANUP_S3_OPERATION_TIMEOUT)
                .build(),
        )
}

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
    /// Tracks the first in-memory state-schema migration observed by
    /// `read_state` on this backend instance (carina#3283). See the
    /// equivalent field on `LocalBackend` for the rationale.
    migration_logged: OnceLock<MigrationInfo>,
    /// Best-known state of the state object's ETag. Successful full-state reads
    /// (including a 404) and writes resolve this cache so the steady-state
    /// conditional write needs only one PUT. Exports-only reads deliberately do
    /// not change it. A stale value is safe: S3 rejects it and the writer
    /// refreshes via HEAD.
    state_etag: Mutex<CachedStateEtag>,
}

#[derive(Clone)]
enum CachedStateEtag {
    Unknown,
    Absent,
    Etag(String),
}

#[derive(Clone, Copy)]
enum StateWriteOperation {
    Normal,
    Cleanup,
}

#[derive(Clone, Copy)]
enum StateWriteLock<'a> {
    Unlocked,
    Locked(&'a LockInfo),
}

enum StatePutOutcome {
    Written(String),
    Conflict,
}

impl S3Backend {
    /// Create a new S3Backend from configuration.
    ///
    /// The `region` attribute is optional. When omitted, the region is
    /// resolved through the AWS SDK's standard chain (env vars, shared config
    /// profile, etc.). If neither the backend block nor the SDK chain yields
    /// a region, configuration fails — the backend never silently picks a
    /// default endpoint. The S3 SDK transparently follows cross-region
    /// redirects, so any region from the chain reaches buckets in other
    /// regions.
    pub(crate) async fn from_config(config: &S3BackendSettings) -> BackendResult<Self> {
        let bucket = config.bucket().to_string();
        let key = config.key().to_string();
        let encrypt = config.encrypt();
        let auto_create = config.auto_create();

        let sdk_region = sdk_chain_region().await;
        let region = resolve_region(config.region(), sdk_region.as_deref())?;
        let client = build_s3_client(&region).await;

        Ok(Self::from_client(
            client,
            bucket,
            key,
            region,
            encrypt,
            auto_create,
        ))
    }

    /// Construct an `S3Backend` from a bucket + key pair, resolving the
    /// region through the AWS SDK default chain.
    ///
    /// This is the URL-addressed entry point (carina#3336): it goes
    /// through the same region resolution and SDK client construction
    /// as [`Self::from_config`] but skips the `BackendConfig` round-trip
    /// that callers without a DSL `backend { ... }` block would otherwise
    /// have to fabricate. `auto_create` is forced off — a read-only URL
    /// lookup must never create infrastructure as a side effect.
    /// `encrypt` is left at the production default (`true`) so any future
    /// fallback write would match `from_config`'s behavior.
    pub(crate) async fn from_url_parts(bucket: String, key: String) -> BackendResult<Self> {
        let sdk_region = sdk_chain_region().await;
        let region = resolve_region(None, sdk_region.as_deref())?;
        let client = build_s3_client(&region).await;
        Ok(Self::from_client(client, bucket, key, region, true, false))
    }

    /// Construct an `S3Backend` over a caller-supplied `aws_sdk_s3::Client`.
    ///
    /// `from_config` is the production entry point — it resolves the
    /// region and builds the SDK client itself. This constructor exists
    /// so a test can inject a client wired to an in-process AWS mock
    /// (`winterbaume`, library mode) and exercise the `StateBackend` I/O
    /// path with no real AWS and no external process (#3203). It does
    /// not touch the AWS SDK config chain; the `region` argument is used
    /// verbatim (it still drives the `create_bucket` location
    /// constraint).
    pub(crate) fn from_client(
        client: Client,
        bucket: String,
        key: String,
        region: String,
        encrypt: bool,
        auto_create: bool,
    ) -> Self {
        Self {
            client,
            bucket,
            key,
            region,
            encrypt,
            auto_create,
            migration_logged: OnceLock::new(),
            state_etag: Mutex::new(CachedStateEtag::Unknown),
        }
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
                let etag = output.e_tag().map(ToOwned::to_owned).ok_or_else(|| {
                    BackendError::Configuration(format!(
                        "Lock object {}/{} missing ETag",
                        self.bucket,
                        self.lock_key()
                    ))
                })?;
                let body = output
                    .body
                    .collect()
                    .await
                    .map_err(|e| BackendError::Io(e.to_string()))?;
                let bytes = body.into_bytes();
                let lock = LockInfo::from_json_slice(&bytes)
                    .map_err(|e| BackendError::Serialization(e.to_string()))?;
                Ok(Some((lock, etag)))
            }
            Err(err) => {
                // Check if it's a NoSuchKey error
                if is_not_found_error(&err) {
                    Ok(None)
                } else {
                    Err(BackendError::Aws(Box::new(
                        AwsError::from_sdk_error("s3.GetObject", err)
                            .bucket(&self.bucket)
                            .key(self.lock_key()),
                    )))
                }
            }
        }
    }

    async fn read_lock(&self) -> BackendResult<Option<LockInfo>> {
        Ok(self.read_lock_with_etag().await?.map(|(lock, _)| lock))
    }

    async fn read_lock_with_etag_for_cleanup(&self) -> BackendResult<Option<(LockInfo, String)>> {
        self.with_cleanup_operation_timeout("s3.GetObject", async {
            let result = self
                .client
                .get_object()
                .bucket(&self.bucket)
                .key(self.lock_key())
                .customize()
                .config_override(cleanup_operation_config())
                .send()
                .await;

            match result {
                Ok(output) => {
                    let etag = output.e_tag().map(ToOwned::to_owned).ok_or_else(|| {
                        BackendError::Configuration(format!(
                            "Lock object {}/{} missing ETag",
                            self.bucket,
                            self.lock_key()
                        ))
                    })?;
                    let body = output
                        .body
                        .collect()
                        .await
                        .map_err(|e| BackendError::Io(e.to_string()))?;
                    let lock = LockInfo::from_json_slice(&body.into_bytes())
                        .map_err(|e| BackendError::Serialization(e.to_string()))?;
                    Ok(Some((lock, etag)))
                }
                Err(err) if is_not_found_error(&err) => Ok(None),
                Err(err) => Err(BackendError::Aws(Box::new(
                    AwsError::from_sdk_error("s3.GetObject", err)
                        .bucket(&self.bucket)
                        .key(self.lock_key()),
                ))),
            }
        })
        .await
    }

    async fn with_cleanup_operation_timeout<T>(
        &self,
        operation: &'static str,
        future: impl std::future::Future<Output = BackendResult<T>>,
    ) -> BackendResult<T> {
        tokio::time::timeout(CLEANUP_S3_OPERATION_TIMEOUT, future)
            .await
            .map_err(|_| {
                BackendError::Io(format!(
                    "Cancellation cleanup {operation} timed out after {}ms",
                    CLEANUP_S3_OPERATION_TIMEOUT.as_millis()
                ))
            })?
    }

    fn lock_body(lock: &LockInfo) -> BackendResult<Vec<u8>> {
        carina_core::utils::pretty_with_newline_bytes(lock)
            .map_err(|e| BackendError::Serialization(e.to_string()))
    }

    fn state_body(state: &StateFile) -> BackendResult<Vec<u8>> {
        state
            .validate_unique_identities()
            .map_err(BackendError::InvalidState)?;
        carina_core::utils::pretty_with_newline_bytes(state)
            .map_err(|e| BackendError::Serialization(e.to_string()))
    }

    fn cached_state_etag(&self) -> CachedStateEtag {
        self.state_etag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_cached_state_etag(&self, etag: CachedStateEtag) {
        *self
            .state_etag
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = etag;
    }

    fn state_location(&self) -> String {
        format!("s3://{}/{}", self.bucket, self.key.trim_start_matches('/'))
    }

    async fn get_state_object(&self) -> BackendResult<Option<(Vec<u8>, Option<String>)>> {
        let result = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&self.key)
            .send()
            .await;

        match result {
            Ok(output) => {
                let etag = output.e_tag().map(ToOwned::to_owned);
                let body = output
                    .body
                    .collect()
                    .await
                    .map_err(|error| BackendError::Io(error.to_string()))?;
                Ok(Some((body.into_bytes().to_vec(), etag)))
            }
            Err(error) if is_not_found_error(&error) => Ok(None),
            Err(error) => Err(BackendError::Aws(Box::new(
                AwsError::from_sdk_error("s3.GetObject", error)
                    .bucket(&self.bucket)
                    .key(&self.key),
            ))),
        }
    }

    async fn head_state_etag(
        &self,
        operation: StateWriteOperation,
    ) -> BackendResult<CachedStateEtag> {
        let read = async {
            let request = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&self.key);
            let result = match operation {
                StateWriteOperation::Normal => request.send().await,
                StateWriteOperation::Cleanup => {
                    request
                        .customize()
                        .config_override(cleanup_operation_config())
                        .send()
                        .await
                }
            };

            match result {
                Ok(output) => {
                    let etag = output.e_tag().map(ToOwned::to_owned).ok_or_else(|| {
                        BackendError::Configuration(format!(
                            "State object {}/{} missing ETag",
                            self.bucket, self.key
                        ))
                    })?;
                    Ok(CachedStateEtag::Etag(etag))
                }
                Err(error) if is_not_found_error(&error) => Ok(CachedStateEtag::Absent),
                Err(error) => Err(BackendError::Aws(Box::new(
                    AwsError::from_sdk_error("s3.HeadObject", error)
                        .bucket(&self.bucket)
                        .key(&self.key),
                ))),
            }
        };

        match operation {
            StateWriteOperation::Normal => read.await,
            StateWriteOperation::Cleanup => {
                self.with_cleanup_operation_timeout("s3.HeadObject", read)
                    .await
            }
        }
    }

    async fn put_state_conditionally(
        &self,
        body: Vec<u8>,
        current_etag: Option<&str>,
        operation: StateWriteOperation,
    ) -> BackendResult<StatePutOutcome> {
        let write = async {
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
            request = match current_etag {
                Some(etag) => request.if_match(etag),
                None => request.if_none_match("*"),
            };

            let result = match operation {
                StateWriteOperation::Normal => request.send().await,
                StateWriteOperation::Cleanup => {
                    request
                        .customize()
                        .config_override(cleanup_operation_config())
                        .send()
                        .await
                }
            };
            match result {
                Ok(output) => output
                    .e_tag()
                    .map(ToOwned::to_owned)
                    .map(StatePutOutcome::Written)
                    .ok_or_else(|| {
                        BackendError::Configuration(format!(
                            "State object {}/{} PUT response missing ETag",
                            self.bucket, self.key
                        ))
                    }),
                Err(error) if is_conditional_write_conflict(&error) => {
                    Ok(StatePutOutcome::Conflict)
                }
                Err(error) => Err(BackendError::Aws(Box::new(
                    AwsError::from_sdk_error("s3.PutObject", error)
                        .bucket(&self.bucket)
                        .key(&self.key),
                ))),
            }
        };

        match operation {
            StateWriteOperation::Normal => write.await,
            StateWriteOperation::Cleanup => {
                self.with_cleanup_operation_timeout("s3.PutObject", write)
                    .await
            }
        }
    }

    async fn write_state_conditionally(
        &self,
        state: &StateFile,
        operation: StateWriteOperation,
        write_lock: StateWriteLock<'_>,
    ) -> BackendResult<()> {
        let body = Self::state_body(state)?;
        let mut cached_etag = self.cached_state_etag();
        if matches!(cached_etag, CachedStateEtag::Unknown) {
            cached_etag = self.head_state_etag(operation).await?;
            self.set_cached_state_etag(cached_etag.clone());
        }

        // Dropping an AWS SDK future cannot prove that S3 did not already
        // accept its PUT. Compare-and-swap the object ETag so an abandoned
        // older request either commits first (and this live writer retries)
        // or fails its precondition after this newer write commits.
        for attempt in 1..=STATE_WRITE_MAX_ATTEMPTS {
            if attempt > 1
                && let StateWriteLock::Locked(lock) = write_lock
            {
                self.verify_state_write_lock(lock, operation).await?;
            }
            let current_etag = match &cached_etag {
                CachedStateEtag::Absent => None,
                CachedStateEtag::Etag(etag) => Some(etag.as_str()),
                CachedStateEtag::Unknown => {
                    unreachable!("unknown state ETag must be resolved before a conditional PUT")
                }
            };
            match self
                .put_state_conditionally(body.clone(), current_etag, operation)
                .await?
            {
                StatePutOutcome::Written(etag) => {
                    self.set_cached_state_etag(CachedStateEtag::Etag(etag));
                    return Ok(());
                }
                StatePutOutcome::Conflict if attempt < STATE_WRITE_MAX_ATTEMPTS => {
                    cached_etag = self.head_state_etag(operation).await?;
                    self.set_cached_state_etag(cached_etag.clone());
                }
                StatePutOutcome::Conflict => break,
            }
        }

        Err(BackendError::StateWriteConflict {
            attempts: STATE_WRITE_MAX_ATTEMPTS,
        })
    }

    async fn verify_state_write_lock(
        &self,
        lock: &LockInfo,
        operation: StateWriteOperation,
    ) -> BackendResult<()> {
        let existing_lock = match operation {
            StateWriteOperation::Normal => self.read_lock().await?,
            StateWriteOperation::Cleanup => self
                .read_lock_with_etag_for_cleanup()
                .await?
                .map(|(existing_lock, _)| existing_lock),
        };
        let Some(existing_lock) = existing_lock else {
            return Err(BackendError::LockNotHeld(
                "lock file no longer exists".to_string(),
            ));
        };

        if existing_lock.id() != lock.id() {
            return Err(BackendError::LockNotHeld(format!(
                "lock {} was replaced by {}",
                lock.id(),
                existing_lock.id()
            )));
        }

        Ok(())
    }

    async fn write_lock_if_absent(&self, lock: &LockInfo) -> BackendResult<bool> {
        self.write_lock(lock, Some("*"), None).await
    }

    async fn replace_lock_if_match(&self, lock: &LockInfo, etag: &str) -> BackendResult<bool> {
        self.write_lock(lock, None, Some(etag)).await
    }

    async fn replace_lock_if_match_for_cleanup(
        &self,
        lock: &LockInfo,
        etag: &str,
    ) -> BackendResult<bool> {
        let body = Self::lock_body(lock)?;
        self.with_cleanup_operation_timeout("s3.PutObject", async {
            let mut request = self
                .client
                .put_object()
                .bucket(&self.bucket)
                .key(self.lock_key())
                .body(ByteStream::from(body))
                .content_type("application/json")
                .if_match(etag);

            if self.encrypt {
                request = request.server_side_encryption(ServerSideEncryption::Aes256);
            }

            match request
                .customize()
                .config_override(cleanup_operation_config())
                .send()
                .await
            {
                Ok(_) => Ok(true),
                Err(err) if is_conditional_write_conflict(&err) => Ok(false),
                Err(err) => Err(BackendError::Aws(Box::new(
                    AwsError::from_sdk_error("s3.PutObject", err)
                        .bucket(&self.bucket)
                        .key(self.lock_key()),
                ))),
            }
        })
        .await
    }

    async fn write_state_for_cleanup_operation(
        &self,
        state: &StateFile,
        write_lock: StateWriteLock<'_>,
    ) -> BackendResult<()> {
        self.write_state_conditionally(state, StateWriteOperation::Cleanup, write_lock)
            .await
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
            Err(err) => Err(BackendError::Aws(Box::new(
                AwsError::from_sdk_error("s3.PutObject", err)
                    .bucket(&self.bucket)
                    .key(self.lock_key()),
            ))),
        }
    }

    /// Delete the lock file from S3
    async fn delete_lock(&self) -> BackendResult<()> {
        let lock_key = self.lock_key();
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&lock_key)
            .send()
            .await
            .map_err(|e| {
                BackendError::Aws(Box::new(
                    AwsError::from_sdk_error("s3.DeleteObject", e)
                        .bucket(&self.bucket)
                        .key(&lock_key),
                ))
            })?;

        Ok(())
    }

    async fn delete_lock_for_cleanup(&self) -> BackendResult<()> {
        let lock_key = self.lock_key();
        self.with_cleanup_operation_timeout("s3.DeleteObject", async {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(&lock_key)
                .customize()
                .config_override(cleanup_operation_config())
                .send()
                .await
                .map_err(|err| {
                    BackendError::Aws(Box::new(
                        AwsError::from_sdk_error("s3.DeleteObject", err)
                            .bucket(&self.bucket)
                            .key(&lock_key),
                    ))
                })?;
            Ok(())
        })
        .await
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
    async fn read_state(&self) -> BackendResult<Option<LoadedState>> {
        let state_location = self.state_location();
        match self.get_state_object().await? {
            Some((bytes, etag)) => {
                let outcome = state::check_and_migrate_bytes(&bytes)
                    .map_err(|error| error.with_state_read_location(state_location.clone()))?;
                let (state, migration) = outcome.into_parts();
                let loaded_state = if let Some(info) = migration {
                    log_state_migration_once(&self.migration_logged, info, &state_location);
                    LoadedState::Migrated { state, info }
                } else {
                    LoadedState::Pristine(state)
                };
                self.set_cached_state_etag(
                    etag.map_or(CachedStateEtag::Unknown, CachedStateEtag::Etag),
                );
                Ok(Some(loaded_state))
            }
            None => {
                self.set_cached_state_etag(CachedStateEtag::Absent);
                Ok(None)
            }
        }
    }

    async fn read_state_bytes(&self) -> BackendResult<Option<RawState>> {
        let state_location = self.state_location();
        Ok(self
            .get_state_object()
            .await?
            .map(|(bytes, _)| RawState::new(bytes, state_location)))
    }

    async fn write_state(&self, state: &StateFile) -> BackendResult<()> {
        self.write_state_conditionally(state, StateWriteOperation::Normal, StateWriteLock::Unlocked)
            .await
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

        if existing_lock.id() != lock.id() {
            return Err(BackendError::LockNotHeld(format!(
                "lock {} was replaced by {}",
                lock.id(),
                existing_lock.id()
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
        self.verify_state_write_lock(lock, StateWriteOperation::Normal)
            .await?;
        self.write_state_conditionally(
            state,
            StateWriteOperation::Normal,
            StateWriteLock::Locked(lock),
        )
        .await
    }

    async fn write_state_for_cleanup(&self, state: &StateFile) -> BackendResult<()> {
        self.write_state_for_cleanup_operation(state, StateWriteLock::Unlocked)
            .await
    }

    async fn write_state_locked_for_cleanup(
        &self,
        state: &StateFile,
        lock: &LockInfo,
    ) -> BackendResult<()> {
        // Releasing first, or racing release against this write, is unsafe:
        // another process could acquire the now-free lock after our ownership
        // check but before the state PUT. Keep state-before-release.
        // We also cannot skip renewal because a long apply may be near its TTL;
        // the conditional renewal both proves ownership and establishes a fresh
        // exclusive lease. Reusing that proof for the state PUT removes the
        // second lock GET from the normal renew + write_state_locked sequence.
        let Some((existing_lock, etag)) = self.read_lock_with_etag_for_cleanup().await? else {
            return Err(BackendError::LockNotHeld(
                "lock file no longer exists".to_string(),
            ));
        };

        if existing_lock.id() != lock.id() {
            return Err(BackendError::LockNotHeld(format!(
                "lock {} was replaced by {}",
                lock.id(),
                existing_lock.id()
            )));
        }

        let renewed = lock.renewed();
        if !self
            .replace_lock_if_match_for_cleanup(&renewed, &etag)
            .await?
        {
            return Err(BackendError::LockNotHeld(
                "lock was modified concurrently during cancellation cleanup".to_string(),
            ));
        }

        self.write_state_for_cleanup_operation(state, StateWriteLock::Locked(&renewed))
            .await
    }

    async fn release_lock(&self, lock: &LockInfo) -> BackendResult<()> {
        // Verify the lock exists and matches
        if let Some(existing_lock) = self.read_lock().await? {
            if existing_lock.id() != lock.id() {
                return Err(BackendError::LockMismatch {
                    expected: lock.id().to_owned(),
                    actual: existing_lock.id().to_owned(),
                });
            }
        } else {
            return Err(BackendError::LockNotFound(lock.id().to_owned()));
        }

        self.delete_lock().await
    }

    async fn release_lock_for_cleanup(&self, lock: &LockInfo) -> BackendResult<()> {
        let Some((existing_lock, _etag)) = self.read_lock_with_etag_for_cleanup().await? else {
            return Err(BackendError::LockNotFound(lock.id().to_owned()));
        };

        if existing_lock.id() != lock.id() {
            return Err(BackendError::LockMismatch {
                expected: lock.id().to_owned(),
                actual: existing_lock.id().to_owned(),
            });
        }

        self.delete_lock_for_cleanup().await
    }

    async fn force_unlock(&self, lock_id: &str) -> BackendResult<()> {
        // Verify a lock exists
        if let Some(existing_lock) = self.read_lock().await? {
            if existing_lock.id() != lock_id {
                return Err(BackendError::LockMismatch {
                    expected: lock_id.to_string(),
                    actual: existing_lock.id().to_owned(),
                });
            }
        } else {
            return Err(BackendError::LockNotFound(lock_id.to_string()));
        }

        self.delete_lock().await
    }

    async fn bucket_exists(&self) -> BackendResult<bool> {
        let result = self.client.head_bucket().bucket(&self.bucket).send().await;

        match result {
            Ok(_) => Ok(true),
            Err(err) => {
                if is_missing_head_bucket_response(&err) {
                    Ok(false)
                } else {
                    Err(BackendError::Aws(Box::new(
                        AwsError::from_sdk_error("s3.HeadBucket", err).bucket(&self.bucket),
                    )))
                }
            }
        }
    }

    fn provider_name(&self) -> Option<&str> {
        Some(S3_BACKEND_PROVIDER_NAME)
    }

    fn resource_type(&self) -> Option<&str> {
        Some(S3_BACKEND_RESOURCE_TYPE)
    }

    fn resource_definition(&self, name: &str) -> Option<String> {
        Some(format_state_bucket_definition(name))
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

        create_request.send().await.map_err(|e| {
            BackendError::Aws(Box::new(
                AwsError::from_sdk_error("s3.CreateBucket", e).bucket(&self.bucket),
            ))
        })?;

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
            .map_err(|e| {
                BackendError::Aws(Box::new(
                    AwsError::from_sdk_error("s3.PutPublicAccessBlock", e).bucket(&self.bucket),
                ))
            })?;

        Ok(())
    }
}

/// Render the auto-generated `aws.s3.Bucket` block that `carina apply`
/// appends to the user's `.crn` during state-bucket bootstrap. The shape
/// must stay in sync with the current `aws.s3.Bucket` schema, otherwise
/// later `carina apply` runs reject the auto-generated definition.
fn format_state_bucket_definition(name: &str) -> String {
    format!(
        "\n# Auto-generated by carina (state bucket)\naws.s3.Bucket {{\n    bucket = \"{name}\"\n}}\n"
    )
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

/// Resolve the region the SDK's standard chain (env vars, shared config
/// profile, IMDS, etc.) provides, if any. Returns `None` when the chain
/// has nothing configured.
async fn sdk_chain_region() -> Option<String> {
    aws_config::defaults(aws_config::BehaviorVersion::latest())
        .load()
        .await
        .region()
        .map(|r| r.as_ref().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve the region the S3 backend should use.
///
/// Priority: explicit schema-resolved `region` from the backend block > region
/// from the AWS SDK chain. If neither yields a region, return a configuration
/// error rather than silently picking a default endpoint.
fn resolve_region(explicit: Option<&str>, sdk_chain: Option<&str>) -> BackendResult<String> {
    if let Some(value) = explicit {
        return Ok(value.to_string());
    }
    if let Some(value) = sdk_chain {
        return Ok(value.to_string());
    }
    Err(BackendError::configuration(REGION_UNRESOLVED_MESSAGE))
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

/// Returns `true` if a `HeadBucket` error means the bucket is absent.
///
/// `HeadBucket` is an HTTP `HEAD` request, so an absent bucket is a 404
/// with no response body — the SDK cannot always resolve that into the
/// typed `HeadBucketError::NotFound` variant and may leave it
/// `Unhandled`. The HTTP status is therefore the reliable signal: a 404
/// is a missing bucket regardless of how the SDK classified it. This is
/// the same approach as `is_not_found_error` (used for `GetObject`),
/// which also keys off `raw_response().status()`.
///
/// When no raw response is attached at all — a connection/dispatch
/// failure rather than a service response — there is no evidence the
/// bucket was reported absent, so this returns `false` and `bucket_exists`
/// surfaces the underlying error.
fn is_missing_head_bucket_response(err: &aws_sdk_s3::error::SdkError<HeadBucketError>) -> bool {
    err.raw_response()
        .is_some_and(|raw| raw.status().as_u16() == 404)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ResourceState;
    use aws_sdk_s3::error::{ErrorMetadata, SdkError};

    #[test]
    fn test_resolve_region_prefers_explicit_over_sdk_chain() {
        let resolved = resolve_region(Some("ap-northeast-1"), Some("us-east-1")).unwrap();
        assert_eq!(resolved, "ap-northeast-1");
    }

    #[test]
    fn test_resolve_region_falls_back_to_sdk_chain() {
        let resolved = resolve_region(None, Some("ap-northeast-1")).unwrap();
        assert_eq!(resolved, "ap-northeast-1");
    }

    #[test]
    fn test_resolve_region_errors_when_unresolved() {
        let err = resolve_region(None, None).unwrap_err();
        match err {
            BackendError::Configuration(msg) => {
                assert!(
                    msg.contains("region"),
                    "expected message to mention region, got: {msg}"
                );
                assert!(
                    msg.contains("AWS_REGION") || msg.contains("profile"),
                    "expected message to mention SDK chain hints, got: {msg}"
                );
            }
            other => panic!("expected Configuration error, got: {other:?}"),
        }
    }

    // Pin the byte-level shape so the S3-stored lock body matches
    // the trailing-newline convention used by the state body (#2754)
    // and the local backend (#2721, #2583).
    #[test]
    fn lock_body_ends_with_trailing_newline() {
        let lock = LockInfo::new("apply");
        let bytes = S3Backend::lock_body(&lock).unwrap();
        assert_eq!(
            bytes.last().copied(),
            Some(b'\n'),
            "S3 lock body must end with a trailing newline; got {:?}",
            bytes.last().map(|b| *b as char),
        );
    }

    // Pin the byte-level shape so the S3-stored carina.state.json
    // body matches the trailing-newline convention used by the local
    // backend (#2721).
    #[test]
    fn state_body_ends_with_trailing_newline() {
        let mut state = StateFile::new();
        state.increment_serial();
        let bytes = S3Backend::state_body(&state).unwrap();
        assert_eq!(
            bytes.last().copied(),
            Some(b'\n'),
            "S3 state body must end with a trailing newline; got {:?}",
            bytes.last().map(|b| *b as char),
        );
    }

    #[test]
    fn state_body_reports_duplicate_identity_as_invalid_state() {
        let resource = ResourceState::new("ec2.Vpc", "network", "aws");
        let state = StateFile::from_unchecked_resources_for_test(vec![resource.clone(), resource]);

        let error = S3Backend::state_body(&state)
            .expect_err("duplicate identity must be rejected before serialization");

        assert!(
            matches!(&error, BackendError::InvalidState(message) if message.contains("duplicate resource identity")
                && message.contains("aws")
                && message.contains("ec2.Vpc")
                && message.contains("network")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn cancellation_cleanup_s3_operations_have_a_bounded_single_attempt_budget() {
        let config = cleanup_operation_config().build();
        let retry = config
            .retry_config()
            .expect("cleanup operation must override the SDK retry policy");
        let timeout = config
            .timeout_config()
            .expect("cleanup operation must override the SDK timeout policy");

        assert_eq!(
            retry.max_attempts(),
            1,
            "cleanup cannot spend time retrying"
        );
        assert_eq!(
            timeout.operation_timeout(),
            Some(CLEANUP_S3_OPERATION_TIMEOUT)
        );
        assert!(
            CLEANUP_S3_OPERATION_TIMEOUT * 5 < carina_core::shutdown::CLEANUP_DEADLINE,
            "three state calls plus two lock-release calls must fit inside the 2s cleanup deadline"
        );
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
        // Including a removed attribute here makes subsequent `carina apply`
        // runs fail with `Unknown attribute '...'` — pin the schema-aligned shape.
        let definition = format_state_bucket_definition("my-state-bucket");
        assert!(definition.contains("aws.s3.Bucket {"));
        assert!(definition.contains("bucket = \"my-state-bucket\""));
        assert!(definition.contains("# Auto-generated by carina"));
        assert!(
            !definition.contains("versioning_status"),
            "auto-definition must not reference the removed versioning_status attribute, got: {definition}"
        );
    }

    /// Build a `HeadBucket` raw HTTP response with the given status, so
    /// the HTTP-status-based classifier can be exercised without a live
    /// SDK call.
    fn head_bucket_http_response(status: u16) -> aws_smithy_runtime_api::http::Response {
        aws_smithy_runtime_api::http::Response::new(
            status.try_into().unwrap(),
            aws_smithy_types::body::SdkBody::empty(),
        )
    }

    #[test]
    fn test_is_missing_head_bucket_response_for_typed_not_found_with_404() {
        // The SDK resolved the 404 into the typed NotFound variant.
        let err = SdkError::service_error(
            HeadBucketError::NotFound(aws_sdk_s3::types::error::NotFound::builder().build()),
            head_bucket_http_response(404),
        );
        assert!(is_missing_head_bucket_response(&err));
    }

    #[test]
    fn test_is_missing_head_bucket_response_for_unhandled_404() {
        // The SDK left the 404 as an Unhandled error — e.g. when the
        // response carries an error body the typed parser does not
        // expect (this is exactly what an in-process S3 mock can emit).
        // The HTTP status must still classify it as a missing bucket.
        let err = SdkError::service_error(
            HeadBucketError::generic(ErrorMetadata::builder().code("NoSuchBucket").build()),
            head_bucket_http_response(404),
        );
        assert!(is_missing_head_bucket_response(&err));
    }

    #[test]
    fn test_is_missing_head_bucket_response_rejects_non_404_status() {
        // A 403 (e.g. AccessDenied) is not a missing bucket.
        let err = SdkError::service_error(
            HeadBucketError::generic(ErrorMetadata::builder().code("AccessDenied").build()),
            head_bucket_http_response(403),
        );
        assert!(!is_missing_head_bucket_response(&err));
    }

    #[test]
    fn test_is_missing_head_bucket_response_rejects_error_without_raw_response() {
        // A construction/dispatch failure carries no HTTP response, so
        // there is no evidence the bucket is absent — this must not be
        // classified as a missing bucket.
        let err: SdkError<HeadBucketError> =
            SdkError::construction_failure("no raw response attached");
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
