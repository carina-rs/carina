//! Integration tests for the S3 state backend against an in-process
//! AWS mock (`winterbaume`, library mode).
//!
//! `winterbaume` is injected directly as the `aws-sdk-rust` HTTP client
//! via `MockAws::sdk_config`, so these tests exercise the real
//! `StateBackend` I/O path (`aws-sdk-s3` request construction, response
//! parsing, error classification) with no network I/O and no external
//! process.
//!
//! Conditional-write coverage (#3205): `winterbaume-s3` 0.2.1 enforces
//! `If-None-Match` / `If-Match` and returns `412 PreconditionFailed`
//! on a conflict, so the lock-contention and expired-lock takeover
//! paths are exercised here (in addition to state write/read,
//! bucket auto-create, and single-holder lock acquire/release).

use super::S3Backend;
use crate::{BackendError, StateBackend, StateFile, read_published_exports};
use aws_sdk_s3::Client;
use aws_sdk_s3::primitives::ByteStream;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore, oneshot};
use winterbaume_core::{MockAws, MockRequest, MockResponse, MockService};
use winterbaume_s3::S3Service;

const TEST_REGION: &str = "us-east-1";
const TEST_BUCKET: &str = "carina-state-test-bucket";
const TEST_KEY: &str = "carina.state.json";

/// Build an `S3Backend` over a winterbaume-backed client. `encrypt` is
/// disabled because the mock does not need server-side encryption and
/// leaving it on only adds a header the mock ignores.
async fn mock_backend() -> S3Backend {
    mock_backend_with_client().await.0
}

/// Build an `S3Backend` and return both the backend and a second
/// `aws_sdk_s3::Client` wired to the same in-process mock. The shared
/// client lets a test seed S3 objects directly (e.g. an expired lock
/// file) that the backend will then observe.
async fn mock_backend_with_client() -> (S3Backend, Client) {
    let mock = MockAws::builder().with_service(S3Service::new()).build();
    let sdk_config = mock.sdk_config(TEST_REGION).await;
    let backend = S3Backend::from_client(
        Client::new(&sdk_config),
        TEST_BUCKET.to_string(),
        TEST_KEY.to_string(),
        TEST_REGION.to_string(),
        false, // encrypt
        true,  // auto_create
    );
    let raw_client = Client::new(&sdk_config);
    (backend, raw_client)
}

async fn mock_backend_with_service(service: impl MockService) -> S3Backend {
    let mock = MockAws::builder().with_service(service).build();
    let sdk_config = mock.sdk_config(TEST_REGION).await;
    S3Backend::from_client(
        Client::new(&sdk_config),
        TEST_BUCKET.to_string(),
        TEST_KEY.to_string(),
        TEST_REGION.to_string(),
        false,
        true,
    )
}

async fn create_bucket_and_seed_empty_state(backend: &S3Backend) {
    backend.create_bucket().await.unwrap();
    backend.write_state(&StateFile::new()).await.unwrap();
}

struct FailingCreateBucketService {
    inner: S3Service,
}

impl MockService for FailingCreateBucketService {
    fn service_name(&self) -> &str {
        self.inner.service_name()
    }

    fn url_patterns(&self) -> Vec<&str> {
        self.inner.url_patterns()
    }

    fn handle(
        &self,
        _request: MockRequest,
    ) -> Pin<Box<dyn Future<Output = MockResponse> + Send + '_>> {
        Box::pin(async {
            MockResponse::xml(
                409,
                r#"<Error>
  <Code>BucketAlreadyOwnedByYou</Code>
  <Message>The bucket already exists and is owned by you</Message>
  <RequestId>00000000-0000-0000-0000-000000000000</RequestId>
</Error>"#,
            )
        })
    }
}

fn state_serial(request: &MockRequest) -> Option<u64> {
    (request.method == "PUT")
        .then(|| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
        .flatten()?
        .get("serial")?
        .as_u64()
}

struct DelayedStatePut {
    inner: Arc<S3Service>,
    delayed_serial: u64,
    landed: Arc<Notify>,
    release: Arc<Semaphore>,
    completed: Arc<Notify>,
    state_puts: Arc<AtomicUsize>,
    state_heads: Arc<AtomicUsize>,
}

impl MockService for DelayedStatePut {
    fn service_name(&self) -> &str {
        self.inner.service_name()
    }

    fn url_patterns(&self) -> Vec<&str> {
        self.inner.url_patterns()
    }

    fn handle(
        &self,
        request: MockRequest,
    ) -> Pin<Box<dyn Future<Output = MockResponse> + Send + '_>> {
        if request.method == "HEAD" && request.uri.contains(TEST_KEY) {
            self.state_heads.fetch_add(1, Ordering::SeqCst);
        }
        if state_serial(&request).is_some() {
            self.state_puts.fetch_add(1, Ordering::SeqCst);
        }
        if state_serial(&request) != Some(self.delayed_serial) {
            return self.inner.handle(request);
        }

        let inner = Arc::clone(&self.inner);
        let landed = Arc::clone(&self.landed);
        let release = Arc::clone(&self.release);
        let completed = Arc::clone(&self.completed);
        let (response_tx, response_rx) = oneshot::channel();
        tokio::spawn(async move {
            // Model a request that S3 has committed even though the caller is
            // still waiting for the response and can abandon that future.
            let response = inner.handle(request).await;
            landed.notify_one();
            release
                .acquire()
                .await
                .expect("delayed PUT release semaphore must remain open")
                .forget();
            completed.notify_one();
            let _ = response_tx.send(response);
        });
        Box::pin(async move {
            response_rx.await.unwrap_or_else(|_| {
                MockResponse::error(500, "AbandonedTestResponse", "response receiver dropped")
            })
        })
    }
}

struct RecordingS3Calls {
    inner: Arc<S3Service>,
    enabled: Arc<AtomicBool>,
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

impl MockService for RecordingS3Calls {
    fn service_name(&self) -> &str {
        self.inner.service_name()
    }

    fn url_patterns(&self) -> Vec<&str> {
        self.inner.url_patterns()
    }

    fn handle(
        &self,
        request: MockRequest,
    ) -> Pin<Box<dyn Future<Output = MockResponse> + Send + '_>> {
        if self.enabled.load(Ordering::SeqCst) {
            self.calls
                .lock()
                .unwrap()
                .push((request.method.clone(), request.uri.clone()));
        }
        self.inner.handle(request)
    }
}

struct ConflictingStatePuts {
    inner: Arc<S3Service>,
    enabled: Arc<AtomicBool>,
    conflicts: Arc<AtomicUsize>,
    heads: Arc<AtomicUsize>,
    gets: Arc<AtomicUsize>,
}

struct PauseFirstStatePut {
    inner: Arc<S3Service>,
    paused_serial: u64,
    paused: AtomicBool,
    started: Arc<Notify>,
    release: Arc<Semaphore>,
}

impl MockService for PauseFirstStatePut {
    fn service_name(&self) -> &str {
        self.inner.service_name()
    }

    fn url_patterns(&self) -> Vec<&str> {
        self.inner.url_patterns()
    }

    fn handle(
        &self,
        request: MockRequest,
    ) -> Pin<Box<dyn Future<Output = MockResponse> + Send + '_>> {
        if state_serial(&request) != Some(self.paused_serial)
            || self.paused.swap(true, Ordering::SeqCst)
        {
            return self.inner.handle(request);
        }

        let inner = Arc::clone(&self.inner);
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        Box::pin(async move {
            started.notify_one();
            release
                .acquire()
                .await
                .expect("paused PUT release semaphore must remain open")
                .forget();
            inner.handle(request).await
        })
    }
}

impl MockService for ConflictingStatePuts {
    fn service_name(&self) -> &str {
        self.inner.service_name()
    }

    fn url_patterns(&self) -> Vec<&str> {
        self.inner.url_patterns()
    }

    fn handle(
        &self,
        request: MockRequest,
    ) -> Pin<Box<dyn Future<Output = MockResponse> + Send + '_>> {
        if self.enabled.load(Ordering::SeqCst) {
            match request.method.as_str() {
                "HEAD" => {
                    self.heads.fetch_add(1, Ordering::SeqCst);
                }
                "GET" => {
                    self.gets.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            if state_serial(&request).is_some() {
                let attempt = self.conflicts.fetch_add(1, Ordering::SeqCst) + 1;
                assert!(
                    attempt <= 5,
                    "state write retried beyond the five-attempt limit: {attempt}"
                );
                return Box::pin(async {
                    MockResponse::error(412, "PreconditionFailed", "forced state write conflict")
                });
            }
        }
        self.inner.handle(request)
    }
}

/// S3 key the backend uses for its lock file. Mirrors `S3Backend::lock_key`
/// (which is `pub(crate)`): tests need the same key to plant or replace
/// the lock object out-of-band.
fn lock_key() -> String {
    format!("{TEST_KEY}.lock")
}

#[derive(Serialize)]
struct TestLockFile {
    id: String,
    operation: String,
    who: &'static str,
    created: DateTime<Utc>,
    expires: DateTime<Utc>,
}

impl TestLockFile {
    fn id(&self) -> &str {
        &self.id
    }

    fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        now > self.expires
    }
}

fn test_lock(operation: &str, timeout_secs: i64) -> TestLockFile {
    let now = Utc::now();
    TestLockFile {
        id: uuid::Uuid::new_v4().to_string(),
        operation: operation.to_string(),
        who: "test@localhost",
        created: now,
        expires: now + Duration::seconds(timeout_secs),
    }
}

#[tokio::test]
async fn create_bucket_makes_bucket_exist() {
    let backend = mock_backend().await;

    assert!(
        !backend.bucket_exists().await.unwrap(),
        "bucket should not exist before create_bucket",
    );

    backend.create_bucket().await.unwrap();

    assert!(
        backend.bucket_exists().await.unwrap(),
        "bucket should exist after create_bucket",
    );
}

#[tokio::test]
async fn create_bucket_error_preserves_aws_operation_bucket_and_service_details() {
    let backend = mock_backend_with_service(FailingCreateBucketService {
        inner: S3Service::new(),
    })
    .await;

    let error = backend
        .create_bucket()
        .await
        .expect_err("CreateBucket must return the structured AWS error");

    assert!(matches!(error, BackendError::Aws(_)), "{error:?}");
    let rendered = error.to_string();
    assert!(rendered.contains("s3.CreateBucket"), "{rendered}");
    assert!(rendered.contains(TEST_BUCKET), "{rendered}");
    assert!(rendered.contains("BucketAlreadyOwnedByYou"), "{rendered}");
}

// carina#3336: end-to-end exercise of `--state-url s3://...`'s
// happy path. Seeds an object into the in-process mock S3, reads it
// back through the same `S3Backend::read_state` + LoadedState→StateFile
// path that `load_state_from_url` walks for the `StateUrl::S3` arm. The
// only difference between this test's setup and the production URL
// loader is which `aws_sdk_s3::Client` is in the backend
// (winterbaume-mocked here, SDK-default-chain-built in production —
// the URL parsing + region/SDK resolution are covered by url.rs's
// unit tests). Together they pin the full S3 read path.
#[tokio::test]
async fn url_loader_s3_path_reads_seeded_state() {
    let (backend, raw_client) = mock_backend_with_client().await;
    create_bucket_and_seed_empty_state(&backend).await;

    let mut seeded = StateFile::new();
    seeded.increment_serial();
    seeded.lineage = "test-state-url-s3-loader".to_string();
    let bytes = carina_core::utils::pretty_with_newline_bytes(&seeded).unwrap();

    raw_client
        .put_object()
        .bucket(TEST_BUCKET)
        .key(TEST_KEY)
        .body(ByteStream::from(bytes))
        .send()
        .await
        .unwrap();

    // Replicates the body of `load_state_from_url` for the `StateUrl::S3`
    // arm: read_state, unwrap into_state, error on None.
    let loaded = backend
        .read_state()
        .await
        .unwrap()
        .expect("seeded object should be readable")
        .into_state();
    assert_eq!(loaded.serial, seeded.serial);
    assert_eq!(loaded.lineage, seeded.lineage);
}

#[tokio::test]
async fn write_then_read_state_round_trips() {
    let backend = mock_backend().await;
    create_bucket_and_seed_empty_state(&backend).await;

    // Use a state that carries a resource entry, not just scalar header
    // fields — a serde regression that dropped `resources` would survive
    // a header-only round-trip but is caught here.
    let mut written = StateFile::with_managed_state_bucket(
        "aws",
        "s3.Bucket",
        "aws_s3_bucket_a3f2b1c8",
        carina_core::provider::AppliedIdentifier::new("my-state-bucket").unwrap(),
    )
    .unwrap();
    written.increment_serial();
    written.increment_serial();
    assert_eq!(
        written.resources().len(),
        1,
        "precondition: the written state carries one resource",
    );

    backend.write_state(&written).await.unwrap();

    let read_back = backend
        .read_state()
        .await
        .unwrap()
        .expect("state written above should be readable")
        .into_state();
    // `StateFile` has no `PartialEq`; compare the fields that prove the
    // bytes round-tripped through S3 unchanged. `lineage` is preserved
    // (not regenerated) so it pins identity across the write/read.
    assert_eq!(read_back.serial, written.serial, "serial must round-trip");
    assert_eq!(
        read_back.lineage, written.lineage,
        "lineage must round-trip unchanged",
    );
    assert_eq!(
        read_back.version, written.version,
        "version must round-trip",
    );
    assert_eq!(
        read_back.resources().len(),
        1,
        "the resource entry must survive the write/read round-trip",
    );
    // Assert every non-trivial field of the resource: a serde regression
    // that dropped any one of `attributes` (a JSON map), `identifier`
    // (an `Option`), or `protected` (a `bool`) would survive a
    // name/type-only check but is caught here.
    let written_res = &written.resources()[0];
    let read_res = &read_back.resources()[0];
    assert_eq!(
        read_res.identity, written_res.identity,
        "the round-tripped resource must keep its identity",
    );
    assert_eq!(
        read_res.resource_type, written_res.resource_type,
        "the round-tripped resource must keep its type",
    );
    assert_eq!(
        read_res.identifier(),
        written_res.identifier(),
        "the round-tripped resource must keep its identifier",
    );
    assert_eq!(
        read_res.persisted_attributes(),
        written_res.persisted_attributes(),
        "the round-tripped resource must keep its attributes map",
    );
    assert_eq!(
        read_res.protected, written_res.protected,
        "the round-tripped resource must keep its protected flag",
    );
}

#[tokio::test]
async fn lower_serial_same_lineage_write_replaces_remote_state() {
    let backend = mock_backend().await;
    create_bucket_and_seed_empty_state(&backend).await;

    let mut older = StateFile::new();
    older.increment_serial();
    let mut newer = older.clone();
    newer.increment_serial();

    backend.write_state(&newer).await.unwrap();
    backend.write_state(&older).await.unwrap();

    let persisted = backend.read_state().await.unwrap().unwrap().into_state();
    assert_eq!(persisted.lineage, older.lineage);
    assert_eq!(persisted.serial, older.serial);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_older_put_cannot_overwrite_a_completed_newer_put() {
    let landed = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let completed = Arc::new(Notify::new());
    let state_puts = Arc::new(AtomicUsize::new(0));
    let state_heads = Arc::new(AtomicUsize::new(0));
    let backend = Arc::new(
        mock_backend_with_service(DelayedStatePut {
            inner: Arc::new(S3Service::new()),
            delayed_serial: 1,
            landed: Arc::clone(&landed),
            release: Arc::clone(&release),
            completed: Arc::clone(&completed),
            state_puts: Arc::clone(&state_puts),
            state_heads: Arc::clone(&state_heads),
        })
        .await,
    );
    create_bucket_and_seed_empty_state(&backend).await;

    let initial = backend.read_state().await.unwrap().unwrap().into_state();
    state_puts.store(0, Ordering::SeqCst);
    state_heads.store(0, Ordering::SeqCst);
    let mut older = initial.clone();
    older.increment_serial();
    let mut newer = older.clone();
    newer.increment_serial();

    let older_write = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move { backend.write_state(&older).await })
    };
    landed.notified().await;
    older_write.abort();
    assert!(older_write.await.unwrap_err().is_cancelled());
    release.add_permits(1);
    completed.notified().await;

    backend.write_state(&newer).await.unwrap();

    let persisted = backend.read_state().await.unwrap().unwrap().into_state();
    assert_eq!(persisted.lineage, newer.lineage);
    assert_eq!(persisted.serial, newer.serial);
    assert_eq!(
        state_puts.load(Ordering::SeqCst),
        3,
        "old landed PUT + stale cached-Etag PUT + retry PUT"
    );
    assert_eq!(
        state_heads.load(Ordering::SeqCst),
        1,
        "only the precondition conflict should require HEAD"
    );
}

#[tokio::test]
async fn repeated_state_write_conflicts_are_bounded() {
    let enabled = Arc::new(AtomicBool::new(false));
    let conflicts = Arc::new(AtomicUsize::new(0));
    let heads = Arc::new(AtomicUsize::new(0));
    let gets = Arc::new(AtomicUsize::new(0));
    let backend = mock_backend_with_service(ConflictingStatePuts {
        inner: Arc::new(S3Service::new()),
        enabled: Arc::clone(&enabled),
        conflicts: Arc::clone(&conflicts),
        heads: Arc::clone(&heads),
        gets: Arc::clone(&gets),
    })
    .await;
    create_bucket_and_seed_empty_state(&backend).await;
    let mut state = backend.read_state().await.unwrap().unwrap().into_state();
    state.increment_serial();
    enabled.store(true, Ordering::SeqCst);

    let error = backend
        .write_state(&state)
        .await
        .expect_err("every conditional state PUT should conflict");

    assert_eq!(conflicts.load(Ordering::SeqCst), 5);
    assert_eq!(
        heads.load(Ordering::SeqCst),
        4,
        "the cached ETag avoids the first HEAD; each retry refreshes it"
    );
    assert_eq!(gets.load(Ordering::SeqCst), 0);
    assert!(matches!(
        error,
        BackendError::StateWriteConflict { attempts: 5 }
    ));
}

#[tokio::test]
async fn cleanup_locked_write_with_cached_state_etag_uses_three_s3_calls() {
    let enabled = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mock = MockAws::builder()
        .with_service(RecordingS3Calls {
            inner: Arc::new(S3Service::new()),
            enabled: Arc::clone(&enabled),
            calls: Arc::clone(&calls),
        })
        .build();
    let sdk_config = mock.sdk_config(TEST_REGION).await;

    let seed_backend = S3Backend::from_client(
        Client::new(&sdk_config),
        TEST_BUCKET.to_string(),
        TEST_KEY.to_string(),
        TEST_REGION.to_string(),
        false,
        true,
    );
    create_bucket_and_seed_empty_state(&seed_backend).await;

    // Use a fresh backend so only this read can seed its state ETag cache.
    let backend = S3Backend::from_client(
        Client::new(&sdk_config),
        TEST_BUCKET.to_string(),
        TEST_KEY.to_string(),
        TEST_REGION.to_string(),
        false,
        true,
    );
    let mut state = backend.read_state().await.unwrap().unwrap().into_state();
    let lock = backend.acquire_lock("apply").await.unwrap();
    state.increment_serial();

    enabled.store(true, Ordering::SeqCst);
    backend
        .write_state_locked_for_cleanup(&state, &lock)
        .await
        .unwrap();
    enabled.store(false, Ordering::SeqCst);

    let recorded: Vec<_> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(method, uri)| {
            let object = if uri.contains(&lock_key()) {
                "lock"
            } else if uri.contains(TEST_KEY) {
                "state"
            } else {
                "other"
            };
            format!("{method} {object}")
        })
        .collect();
    assert_eq!(
        recorded,
        vec!["GET lock", "PUT lock", "PUT state"],
        "cached cleanup must fit the 900ms lock GET + lock PUT + state PUT budget"
    );
}

#[tokio::test]
async fn published_exports_read_does_not_seed_the_state_etag_cache() {
    let enabled = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mock = MockAws::builder()
        .with_service(RecordingS3Calls {
            inner: Arc::new(S3Service::new()),
            enabled: Arc::clone(&enabled),
            calls: Arc::clone(&calls),
        })
        .build();
    let sdk_config = mock.sdk_config(TEST_REGION).await;

    let seed_backend = S3Backend::from_client(
        Client::new(&sdk_config),
        TEST_BUCKET.to_string(),
        TEST_KEY.to_string(),
        TEST_REGION.to_string(),
        false,
        true,
    );
    create_bucket_and_seed_empty_state(&seed_backend).await;

    let backend = S3Backend::from_client(
        Client::new(&sdk_config),
        TEST_BUCKET.to_string(),
        TEST_KEY.to_string(),
        TEST_REGION.to_string(),
        false,
        true,
    );
    let mut next_state = StateFile::new();
    next_state.increment_serial();

    enabled.store(true, Ordering::SeqCst);
    read_published_exports(&backend).await.unwrap().unwrap();
    backend.write_state(&next_state).await.unwrap();
    enabled.store(false, Ordering::SeqCst);

    let recorded: Vec<_> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(method, uri)| {
            let object = if uri.contains(TEST_KEY) {
                "state"
            } else {
                "other"
            };
            format!("{method} {object}")
        })
        .collect();
    assert_eq!(
        recorded,
        vec!["GET state", "HEAD state", "PUT state"],
        "an exports-only read must leave the write-CAS cache unknown"
    );
}

#[tokio::test]
async fn cleanup_locked_write_with_known_absent_state_uses_three_s3_calls() {
    let enabled = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mock = MockAws::builder()
        .with_service(RecordingS3Calls {
            inner: Arc::new(S3Service::new()),
            enabled: Arc::clone(&enabled),
            calls: Arc::clone(&calls),
        })
        .build();
    let sdk_config = mock.sdk_config(TEST_REGION).await;
    let backend = S3Backend::from_client(
        Client::new(&sdk_config),
        TEST_BUCKET.to_string(),
        TEST_KEY.to_string(),
        TEST_REGION.to_string(),
        false,
        true,
    );

    backend.create_bucket().await.unwrap();
    assert!(backend.read_state().await.unwrap().is_none());
    let lock = backend.acquire_lock("apply").await.unwrap();
    let mut state = StateFile::new();
    state.increment_serial();

    enabled.store(true, Ordering::SeqCst);
    backend
        .write_state_locked_for_cleanup(&state, &lock)
        .await
        .unwrap();
    enabled.store(false, Ordering::SeqCst);

    let recorded: Vec<_> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(method, uri)| {
            let object = if uri.contains(&lock_key()) {
                "lock"
            } else if uri.contains(TEST_KEY) {
                "state"
            } else {
                "other"
            };
            format!("{method} {object}")
        })
        .collect();
    assert_eq!(
        recorded,
        vec!["GET lock", "PUT lock", "PUT state"],
        "known-absent cleanup must use If-None-Match directly and fit the 900ms budget"
    );
}

#[tokio::test]
async fn read_state_returns_none_when_object_absent() {
    let backend = mock_backend().await;
    // Create the bucket but never write a state object.
    backend.create_bucket().await.unwrap();

    let state = backend.read_state().await.unwrap();
    assert!(
        state.is_none(),
        "read_state on an absent object must return None, not error",
    );
}

#[tokio::test]
async fn acquire_then_release_lock_round_trips() {
    // Covers the single-holder lock mechanics: write the lock object,
    // read it back to verify ownership on release, then delete it. The
    // conditional-write contention path is exercised separately in
    // `acquire_lock_conflicts_with_held_lock` (#3205).
    let backend = mock_backend().await;
    create_bucket_and_seed_empty_state(&backend).await;

    // No lock held: acquire writes the lock object and succeeds.
    let lock = backend.acquire_lock("apply").await.unwrap();

    // Release verifies ownership via a read, then deletes the object.
    backend.release_lock(&lock).await.unwrap();

    // A second release must fail: the lock object was deleted, so the
    // ownership read finds nothing. This is the assertion that proves
    // `release_lock` actually removed the object (it does not depend on
    // the conditional-write header winterbaume ignores).
    let err = backend
        .release_lock(&lock)
        .await
        .expect_err("releasing an already-released lock must fail");
    assert!(
        matches!(err, BackendError::LockNotFound(_)),
        "expected LockNotFound after the lock object was deleted, got: {err:?}",
    );
}

#[tokio::test]
async fn write_state_locked_succeeds_for_held_lock() {
    let backend = mock_backend().await;
    create_bucket_and_seed_empty_state(&backend).await;

    let lock = backend.acquire_lock("apply").await.unwrap();

    let mut state = StateFile::new();
    state.increment_serial();
    backend.write_state_locked(&state, &lock).await.unwrap();

    let read_back = backend.read_state().await.unwrap().unwrap().into_state();
    assert_eq!(
        read_back.lineage, state.lineage,
        "locked write must persist the state we passed in",
    );
    assert_eq!(read_back.serial, state.serial);

    backend.release_lock(&lock).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locked_state_conflict_rechecks_ownership_before_retrying() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let mock = MockAws::builder()
        .with_service(PauseFirstStatePut {
            inner: Arc::new(S3Service::new()),
            paused_serial: 2,
            paused: AtomicBool::new(false),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        })
        .build();
    let sdk_config = mock.sdk_config(TEST_REGION).await;
    let backend = Arc::new(S3Backend::from_client(
        Client::new(&sdk_config),
        TEST_BUCKET.to_string(),
        TEST_KEY.to_string(),
        TEST_REGION.to_string(),
        false,
        true,
    ));
    let raw_client = Client::new(&sdk_config);
    create_bucket_and_seed_empty_state(backend.as_ref()).await;

    let initial = backend.read_state().await.unwrap().unwrap().into_state();
    let held_lock = backend.acquire_lock("apply").await.unwrap();

    // Advance the object behind the backend's cached ETag so its next state
    // PUT reaches the precondition-conflict retry path.
    let mut competing_state = initial;
    competing_state.increment_serial();
    raw_client
        .put_object()
        .bucket(TEST_BUCKET)
        .key(TEST_KEY)
        .body(ByteStream::from(
            carina_core::utils::pretty_with_newline_bytes(&competing_state).unwrap(),
        ))
        .send()
        .await
        .unwrap();

    let mut attempted_state = competing_state.clone();
    attempted_state.increment_serial();
    let attempted_write = {
        let backend = Arc::clone(&backend);
        tokio::spawn(async move {
            backend
                .write_state_locked(&attempted_state, &held_lock)
                .await
        })
    };

    // wall-clock-ok: original hang guard for the controlled in-process S3 conflict handshake.
    tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
        .await
        .expect("locked state PUT must reach the controlled conflict");

    // Replace the lock only after the writer's initial ownership check. A
    // retry based solely on a refreshed state ETag would now overwrite the
    // new owner's state.
    let replacement_lock = test_lock("apply", 3600);
    raw_client
        .put_object()
        .bucket(TEST_BUCKET)
        .key(lock_key())
        .body(ByteStream::from(
            carina_core::utils::pretty_with_newline_bytes(&replacement_lock).unwrap(),
        ))
        .send()
        .await
        .unwrap();
    release.add_permits(1);

    // wall-clock-ok: original hang guard for the writer after releasing its conflict response.
    let error = tokio::time::timeout(std::time::Duration::from_secs(1), attempted_write)
        .await
        .expect("released state writer must return")
        .unwrap()
        .expect_err("a writer that lost its lock must not retry the state PUT");
    assert!(
        matches!(error, BackendError::LockNotHeld(_)),
        "expected LockNotHeld after replacement, got {error:?}"
    );

    let persisted = backend.read_state().await.unwrap().unwrap().into_state();
    assert_eq!(persisted.serial, competing_state.serial);
    assert_eq!(persisted.lineage, competing_state.lineage);
}

#[tokio::test]
async fn cancellation_cleanup_write_and_release_preserve_the_lock_protocol() {
    let backend = mock_backend().await;
    create_bucket_and_seed_empty_state(&backend).await;
    let lock = backend.acquire_lock("apply").await.unwrap();
    let mut state = StateFile::new();
    state.increment_serial();

    backend
        .write_state_locked_for_cleanup(&state, &lock)
        .await
        .unwrap();
    backend.release_lock_for_cleanup(&lock).await.unwrap();

    let persisted = backend.read_state().await.unwrap().unwrap().into_state();
    assert_eq!(persisted.lineage, state.lineage);
    assert_eq!(persisted.serial, state.serial);
    assert!(
        matches!(
            backend.release_lock(&lock).await,
            Err(BackendError::LockNotFound(_))
        ),
        "cleanup release must remove the lock only after the state write succeeds"
    );
}

#[tokio::test]
async fn write_state_locked_rejects_write_when_lock_not_held() {
    // The negative half of the lock gate: once the lock object is gone,
    // `write_state_locked` must refuse to write. Without this case
    // `write_state_locked_succeeds_for_held_lock` would still pass even
    // if the ownership check were bypassed entirely.
    let backend = mock_backend().await;
    create_bucket_and_seed_empty_state(&backend).await;

    let lock = backend.acquire_lock("apply").await.unwrap();
    backend.release_lock(&lock).await.unwrap();

    let mut state = StateFile::new();
    state.increment_serial();
    let err = backend
        .write_state_locked(&state, &lock)
        .await
        .expect_err("write_state_locked must fail when the lock is no longer held");
    assert!(
        matches!(err, BackendError::LockNotHeld(_)),
        "expected LockNotHeld when the lock object is absent, got: {err:?}",
    );
}

#[tokio::test]
async fn acquire_lock_conflicts_with_held_lock() {
    // #3205: a second `acquire_lock` while a non-expired lock is held
    // must return `BackendError::Locked`. The path under test is
    // `write_lock_if_absent` -> `If-None-Match: *` -> 412 ->
    // `is_conditional_write_conflict` -> `read_lock_with_etag` ->
    // `!is_expired` -> `Locked`. Without winterbaume enforcing the
    // conditional header the second PUT would silently overwrite and
    // this test would fail.
    let backend = mock_backend().await;
    create_bucket_and_seed_empty_state(&backend).await;

    let first = backend.acquire_lock("apply").await.unwrap();

    let err = backend
        .acquire_lock("plan")
        .await
        .expect_err("a second acquire while a fresh lock is held must conflict");
    match err {
        BackendError::Locked {
            lock_id, operation, ..
        } => {
            assert_eq!(
                lock_id,
                first.id(),
                "Locked error must report the holder's id"
            );
            assert_eq!(
                operation, "apply",
                "Locked error must report the holder's operation",
            );
        }
        other => panic!("expected BackendError::Locked, got: {other:?}"),
    }

    backend.release_lock(&first).await.unwrap();
}

#[tokio::test]
async fn acquire_lock_takes_over_expired_lock() {
    // #3205: when the holder of the existing lock has expired,
    // `acquire_lock` must replace it via `replace_lock_if_match`
    // (`If-Match: <etag>` -> 200) and return the new lock. This
    // exercises the second branch of the acquire loop, which only
    // works when the mock enforces `If-Match`.
    let (backend, client) = mock_backend_with_client().await;
    create_bucket_and_seed_empty_state(&backend).await;

    // Plant an expired lock under the same key the backend uses.
    let expired = test_lock("apply", -60);
    assert!(
        expired.is_expired_at(expired.created),
        "precondition: planted lock is expired"
    );
    let body = serde_json::to_vec(&expired).unwrap();
    client
        .put_object()
        .bucket(TEST_BUCKET)
        .key(lock_key())
        .body(ByteStream::from(body))
        .content_type("application/json")
        .send()
        .await
        .expect("planting the expired lock must succeed");

    // acquire_lock sees the expired lock and replaces it with a fresh one.
    let taken_over = backend.acquire_lock("plan").await.unwrap();
    assert_ne!(
        taken_over.id(),
        expired.id(),
        "takeover must mint a new lock id, not reuse the expired holder's",
    );
    assert_eq!(taken_over.operation(), "plan");
    assert!(
        !taken_over.is_expired(),
        "the taken-over lock must have a fresh, non-expired TTL",
    );

    backend.release_lock(&taken_over).await.unwrap();
}

#[tokio::test]
async fn renew_lock_rejects_when_lock_was_replaced() {
    // #3205: `renew_lock` must refuse to refresh when the lock object
    // was replaced out from under the holder. The rejection path under
    // test is the id-mismatch check in `renew_lock`; the
    // `If-Match`-conditional `replace_lock_if_match` further guards
    // against TOCTOU between the read and the write.
    let (backend, client) = mock_backend_with_client().await;
    create_bucket_and_seed_empty_state(&backend).await;

    let original = backend.acquire_lock("apply").await.unwrap();

    // Replace the lock object out-of-band with a different lock id.
    let usurper = test_lock("plan", 3600);
    assert_ne!(usurper.id(), original.id());
    let body = serde_json::to_vec(&usurper).unwrap();
    client
        .put_object()
        .bucket(TEST_BUCKET)
        .key(lock_key())
        .body(ByteStream::from(body))
        .content_type("application/json")
        .send()
        .await
        .expect("planting the usurping lock must succeed");

    let err = backend
        .renew_lock(&original)
        .await
        .expect_err("renew_lock must fail when the lock was replaced concurrently");
    assert!(
        matches!(err, BackendError::LockNotHeld(_)),
        "expected LockNotHeld after a concurrent replacement, got: {err:?}",
    );
}

#[tokio::test]
async fn head_bucket_resolves_to_typed_not_found_on_missing_bucket() {
    // winterbaume#3 fix: `HeadBucket` on a missing bucket must return a
    // body-less 404 so `aws-sdk-rust` resolves the response into the
    // typed `HeadBucketError::NotFound` variant. If the mock ever
    // regresses to returning an `<Error>` XML body, the SDK falls back
    // to `Unhandled` and this test fails — flagging that the body-less
    // contract has broken before `bucket_exists`'s 404-status fallback
    // hides it.
    use aws_sdk_s3::error::SdkError;
    use aws_sdk_s3::operation::head_bucket::HeadBucketError;

    let (_backend, client) = mock_backend_with_client().await;

    let err = client
        .head_bucket()
        .bucket(TEST_BUCKET)
        .send()
        .await
        .expect_err("HeadBucket on an absent bucket must fail");
    let service_err = match &err {
        SdkError::ServiceError(svc) => svc.err(),
        other => panic!("expected SdkError::ServiceError, got: {other:?}"),
    };
    assert!(
        matches!(service_err, HeadBucketError::NotFound(_)),
        "HeadBucket must resolve to the typed NotFound variant; got {service_err:?}. \
         An Unhandled variant here means winterbaume regressed and is returning a \
         response body on a 4xx HEAD response (see moriyoshi/winterbaume#3).",
    );
}
