//! Integration tests for the S3 state backend against an in-process
//! AWS mock (`winterbaume`, library mode).
//!
//! `winterbaume` is injected directly as the `aws-sdk-rust` HTTP client
//! via `MockAws::sdk_config`, so these tests exercise the real
//! `StateBackend` I/O path (`aws-sdk-s3` request construction, response
//! parsing, error classification) with no network I/O and no external
//! process.
//!
//! Scope note (#3203): `winterbaume-s3` 0.2 parses the S3 conditional
//! write headers (`If-None-Match` / `If-Match`) but does not enforce
//! them — there is no `PreconditionFailed` path — so the lock-contention
//! and conditional-write-conflict paths cannot be covered here. See the
//! issue and #3205 for the follow-up. These tests cover what the mock
//! honors faithfully: state write/read, bucket auto-create, and
//! single-holder lock acquire/release.

use aws_sdk_s3::Client;
use carina_state::backends::S3Backend;
use carina_state::{StateBackend, StateFile};
use winterbaume_core::MockAws;
use winterbaume_s3::S3Service;

const TEST_REGION: &str = "us-east-1";
const TEST_BUCKET: &str = "carina-state-test-bucket";
const TEST_KEY: &str = "carina.state.json";

/// Build an `aws_sdk_s3::Client` wired to a fresh in-process winterbaume
/// S3 service. Each call gets an isolated mock with empty state.
async fn mock_s3_client() -> Client {
    let mock = MockAws::builder().with_service(S3Service::new()).build();
    let sdk_config = mock.sdk_config(TEST_REGION).await;
    Client::new(&sdk_config)
}

/// Build an `S3Backend` over a winterbaume-backed client. `encrypt` is
/// disabled because the mock does not need server-side encryption and
/// leaving it on only adds a header the mock ignores.
async fn mock_backend() -> S3Backend {
    S3Backend::from_client(
        mock_s3_client().await,
        TEST_BUCKET.to_string(),
        TEST_KEY.to_string(),
        TEST_REGION.to_string(),
        false, // encrypt
        true,  // auto_create
    )
}

#[tokio::test]
async fn init_auto_creates_bucket_and_seeds_empty_state() {
    let backend = mock_backend().await;

    // The bucket does not exist yet.
    assert!(
        !backend.bucket_exists().await.unwrap(),
        "bucket should not exist before init",
    );

    // init() auto-creates the bucket and seeds an empty state file.
    backend.init().await.unwrap();

    assert!(
        backend.bucket_exists().await.unwrap(),
        "bucket should exist after init",
    );

    let state = backend
        .read_state()
        .await
        .unwrap()
        .expect("init should seed a state file");
    assert_eq!(
        state.version,
        StateFile::CURRENT_VERSION,
        "seeded state should be the current format version",
    );
    assert_eq!(state.serial, 0, "seeded state should start at serial 0");
    assert!(
        state.resources.is_empty(),
        "seeded state should have no resources",
    );
}

#[tokio::test]
async fn write_then_read_state_round_trips() {
    let backend = mock_backend().await;
    backend.init().await.unwrap();

    // Use a state that carries a resource entry, not just scalar header
    // fields — a serde regression that dropped `resources` would survive
    // a header-only round-trip but is caught here.
    let mut written = StateFile::with_managed_state_bucket(
        "aws",
        "s3.Bucket",
        "aws_s3_bucket_a3f2b1c8",
        "my-state-bucket",
    );
    written.increment_serial();
    written.increment_serial();
    assert_eq!(
        written.resources.len(),
        1,
        "precondition: the written state carries one resource",
    );

    backend.write_state(&written).await.unwrap();

    let read_back = backend
        .read_state()
        .await
        .unwrap()
        .expect("state written above should be readable");
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
        read_back.resources.len(),
        1,
        "the resource entry must survive the write/read round-trip",
    );
    // Assert every non-trivial field of the resource: a serde regression
    // that dropped any one of `attributes` (a JSON map), `identifier`
    // (an `Option`), or `protected` (a `bool`) would survive a
    // name/type-only check but is caught here.
    let written_res = &written.resources[0];
    let read_res = &read_back.resources[0];
    assert_eq!(
        read_res.name, written_res.name,
        "the round-tripped resource must keep its name",
    );
    assert_eq!(
        read_res.resource_type, written_res.resource_type,
        "the round-tripped resource must keep its type",
    );
    assert_eq!(
        read_res.identifier, written_res.identifier,
        "the round-tripped resource must keep its identifier",
    );
    assert_eq!(
        read_res.attributes, written_res.attributes,
        "the round-tripped resource must keep its attributes map",
    );
    assert_eq!(
        read_res.protected, written_res.protected,
        "the round-tripped resource must keep its protected flag",
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
    // read it back to verify ownership on release, then delete it. It
    // does *not* cover contention — `winterbaume-s3` 0.2 does not enforce
    // the `If-None-Match` conditional header, so a second `acquire_lock`
    // would succeed instead of conflicting. The contention path is
    // tracked separately (#3203 Notes / follow-up #3205).
    let backend = mock_backend().await;
    backend.init().await.unwrap();

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
        matches!(err, carina_state::BackendError::LockNotFound(_)),
        "expected LockNotFound after the lock object was deleted, got: {err:?}",
    );
}

#[tokio::test]
async fn write_state_locked_succeeds_for_held_lock() {
    let backend = mock_backend().await;
    backend.init().await.unwrap();

    let lock = backend.acquire_lock("apply").await.unwrap();

    let mut state = StateFile::new();
    state.increment_serial();
    backend.write_state_locked(&state, &lock).await.unwrap();

    let read_back = backend.read_state().await.unwrap().unwrap();
    assert_eq!(
        read_back.lineage, state.lineage,
        "locked write must persist the state we passed in",
    );
    assert_eq!(read_back.serial, state.serial);

    backend.release_lock(&lock).await.unwrap();
}

#[tokio::test]
async fn write_state_locked_rejects_write_when_lock_not_held() {
    // The negative half of the lock gate: once the lock object is gone,
    // `write_state_locked` must refuse to write. Without this case
    // `write_state_locked_succeeds_for_held_lock` would still pass even
    // if the ownership check were bypassed entirely.
    let backend = mock_backend().await;
    backend.init().await.unwrap();

    let lock = backend.acquire_lock("apply").await.unwrap();
    backend.release_lock(&lock).await.unwrap();

    let mut state = StateFile::new();
    state.increment_serial();
    let err = backend
        .write_state_locked(&state, &lock)
        .await
        .expect_err("write_state_locked must fail when the lock is no longer held");
    assert!(
        matches!(err, carina_state::BackendError::LockNotHeld(_)),
        "expected LockNotHeld when the lock object is absent, got: {err:?}",
    );
}

#[tokio::test]
async fn init_without_auto_create_errors_on_missing_bucket() {
    let backend = S3Backend::from_client(
        mock_s3_client().await,
        TEST_BUCKET.to_string(),
        TEST_KEY.to_string(),
        TEST_REGION.to_string(),
        false, // encrypt
        false, // auto_create disabled
    );

    let err = backend
        .init()
        .await
        .expect_err("init must fail when the bucket is absent and auto_create is off");
    assert!(
        matches!(&err, carina_state::BackendError::BucketNotFound(b) if b == TEST_BUCKET),
        "expected BucketNotFound({TEST_BUCKET}), got: {err:?}",
    );
}
