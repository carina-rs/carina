//! Offline Contents store. Every mutation captures the branch head before
//! yielding, then checks both that head and the target blob precondition.
use super::github::*;
use crate::{
    BackendConfig, BackendError, StateBackend, StateFile, StorageReadiness, lock::LockInfo,
};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

#[derive(Default)]
struct Store {
    missing_repo: bool,
    missing_branch: bool,
    files: BTreeMap<String, Vec<u8>>,
    history: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    head: u64,
    requests: Vec<GitHubRequest>,
    contention: usize,
    replace: Option<Vec<u8>>,
    loser: u16,
    response: Option<GitHubResponse>,
    responses: VecDeque<GitHubResponse>,
    malformed_success: bool,
    two_winners: bool,
    corrupt_inline: bool,
    after_success: Option<Vec<u8>>,
    after_success_pending: Option<(String, Vec<u8>)>,
    remove_before_mutation: bool,
    directory: bool,
    delay: Option<Duration>,
}
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Store>>);

fn blob(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut hash = Sha1::new();
    hash.update(format!("blob {}\0", bytes.len()));
    hash.update(bytes);
    format!("{:x}", hash.finalize())
}

#[test]
fn github_size_error_names_decoded_encoded_sizes_and_cap_without_content() {
    let error = GitHubError::TooLarge {
        size: MAX_BYTES + 1,
        cap: MAX_BYTES,
    };
    let text = error.to_string();
    assert!(text.contains("10000001"));
    assert!(text.contains("10000000"));
    assert!(text.contains("13333336"), "{text}");
}

#[tokio::test]
async fn github_malformed_repository_readiness_flags_fail_closed() {
    for flag in ["archived", "disabled"] {
        let fake = Fake::default();
        let mut repo = json!({"id":1});
        repo[flag] = json!("false");
        fake.0.lock().unwrap().response = Some(reply(200, repo));
        assert!(matches!(
            fake.backend().storage_readiness().await,
            Err(BackendError::GitHub(GitHubError::MalformedResponse))
        ));
    }
}
fn reply(status: u16, body: Value) -> GitHubResponse {
    GitHubResponse {
        status,
        headers: BTreeMap::new(),
        body: serde_json::to_vec(&body).unwrap(),
    }
}
fn conflict(status: u16, message: &str) -> GitHubResponse {
    reply(
        if status == 0 { 409 } else { status },
        json!({"message": message}),
    )
}
impl Store {
    fn commit(&mut self) -> String {
        self.head += 1;
        let commit = format!("{:040x}", self.head);
        self.history.insert(commit.clone(), self.files.clone());
        commit
    }
}
impl Fake {
    fn put(&self, path: &str, bytes: Vec<u8>) {
        let mut store = self.0.lock().unwrap();
        store.files.insert(path.into(), bytes);
        store.commit();
    }
    fn backend(&self) -> GitHubBackend {
        GitHubBackend::with_transport(settings(), Arc::new(self.clone()))
    }
    fn bytes(&self, path: &str) -> Option<Vec<u8>> {
        self.0.lock().unwrap().files.get(path).cloned()
    }
}
fn settings() -> crate::GitHubBackendSettings {
    let parsed = carina_core::parser::parse_file(
        "backend github { repository = 'owner/repo' branch = 'states/prod' path = 'state.json' }",
        std::path::Path::new("backend.crn"),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let BackendConfig::GitHub(settings) =
        BackendConfig::try_from(parsed.backend.as_ref().unwrap()).unwrap()
    else {
        panic!("github settings")
    };
    settings
}

#[test]
fn github_selection_constructs_no_s3_client_and_performs_no_http() {
    // nextest executes each test in an isolated process, before this runtime
    // is created; no other thread in this process reads the environment.
    unsafe {
        std::env::set_var("GITHUB_TOKEN", "offline-test-token");
    }
    let directory = tempfile::tempdir().unwrap();
    let base = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
    let config = BackendConfig::GitHub(settings()).to_parser_config();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let read = crate::BackendSelection::for_read(&base, Some(&config))
            .unwrap()
            .resolve_with_drift_warning(|_| panic!("no drift"))
            .await
            .unwrap();
        assert!(matches!(read.selected_config(), BackendConfig::GitHub(_)));
        let url =
            crate::StateUrl::parse("github://owner/repo/state.json?branch=states%2Fprod").unwrap();
        super::create_explicit_backend(&url).await.unwrap();
    });
    assert_eq!(
        super::s3::S3_CONSTRUCTIONS.load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

#[tokio::test]
async fn github_renew_checks_existing_uuid_and_updates_the_observed_sha() {
    let fake = Fake::default();
    let lock = LockInfo::new("apply");
    fake.put("state.json.lock", serde_json::to_vec(&lock).unwrap());
    let renewed = fake.backend().renew_lock(&lock).await.unwrap();
    assert_eq!(renewed.id(), lock.id());
    assert!(renewed.expires() >= lock.expires());
}

#[tokio::test]
async fn github_release_deletes_only_the_requested_uuid_with_its_sha() {
    let fake = Fake::default();
    let lock = LockInfo::new("apply");
    fake.put("state.json.lock", serde_json::to_vec(&lock).unwrap());
    fake.backend().release_lock(&lock).await.unwrap();
    assert!(fake.bytes("state.json.lock").is_none());
}

#[tokio::test]
async fn github_force_unlock_deletes_only_the_requested_uuid_with_its_sha() {
    let fake = Fake::default();
    let lock = LockInfo::new("apply");
    fake.put("state.json.lock", serde_json::to_vec(&lock).unwrap());
    fake.backend().force_unlock(lock.id()).await.unwrap();
    assert!(fake.bytes("state.json.lock").is_none());
}

#[async_trait]
impl GitHubTransport for Fake {
    async fn execute(
        &self,
        request: GitHubRequest,
        _deadline: Instant,
    ) -> Result<GitHubResponse, GitHubError> {
        let (head, delay) = {
            let mut store = self.0.lock().unwrap();
            store.requests.push(request.clone());
            if request.method == Method::Get
                && let Some((path, bytes)) = store.after_success_pending.take()
            {
                // Our 2xx has returned to the client. Publish the rival's
                // next revision immediately before serving its read-back.
                store.files.insert(path, bytes);
                store.commit();
            }
            if let Some(response) = store.responses.pop_front() {
                return Ok(response);
            }
            if let Some(response) = store.response.take() {
                return Ok(response);
            }
            (store.head, store.delay)
        };
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if request.method != Method::Get {
            tokio::task::yield_now().await;
        }
        let mut store = self.0.lock().unwrap();
        if store.missing_repo {
            return Ok(reply(404, json!({"message":"Not Found"})));
        }
        if request.path == "/repos/owner/repo" {
            return Ok(reply(200, json!({"id":1})));
        }
        if store.missing_branch {
            return Ok(reply(404, json!({"message":"Not Found"})));
        }
        if request.path == "/repos/owner/repo/branches/states%2Fprod" {
            if store.head == 0 {
                store.commit();
            }
            return Ok(reply(
                200,
                json!({"commit":{"sha":format!("{:040x}", store.head)}}),
            ));
        }
        let path = request
            .path
            .strip_prefix("/repos/owner/repo/contents/")
            .expect("Contents route");
        let path = percent_encoding::percent_decode_str(path)
            .decode_utf8()
            .unwrap()
            .into_owned();
        if request.method == Method::Get {
            let reference = request
                .reference
                .as_deref()
                .expect("Contents reads require ref");
            let files = if reference == "states/prod" {
                &store.files
            } else {
                store.history.get(reference).expect("known commit ref")
            };
            let Some(bytes) = files.get(&path) else {
                return Ok(reply(404, json!({"message":"Not Found"})));
            };
            if request.accept == RAW_MEDIA_TYPE {
                return Ok(GitHubResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    body: bytes.clone(),
                });
            }
            assert_eq!(request.accept, JSON_MEDIA_TYPE);
            let content = if bytes.len() <= 1_000_000 {
                let inline = if store.corrupt_inline {
                    vec![b'!'; bytes.len()]
                } else {
                    bytes.clone()
                };
                format!("{}\n", STANDARD.encode(inline))
            } else {
                String::new()
            };
            return Ok(reply(
                200,
                json!({"type": if store.directory {"dir"} else {"file"}, "sha":blob(bytes), "size":bytes.len(), "encoding":if bytes.len() <= 1_000_000 {"base64"} else {"none"}, "content":content}),
            ));
        }
        let body = request.body.as_ref().expect("mutation body");
        assert_eq!(body["branch"], "states/prod");
        assert!(body.get("committer").is_none());
        assert!(body["message"].as_str().unwrap().starts_with("carina: "));
        if store.remove_before_mutation {
            store.remove_before_mutation = false;
            store.files.remove(&path);
            store.commit();
            return Ok(conflict(store.loser, "does not match"));
        }
        if store.contention > 0 {
            store.contention -= 1;
            store.commit();
            return Ok(conflict(store.loser, "is at X but expected Y"));
        }
        if let Some(bytes) = store.replace.take() {
            store.files.insert(path.clone(), bytes);
            store.commit();
        }
        if head != store.head {
            return Ok(conflict(store.loser, "is at X but expected Y"));
        }
        let current = store.files.get(&path).map(|b| blob(b));
        let expected = body.get("sha").and_then(Value::as_str);
        if current.as_deref() != expected {
            return Ok(conflict(store.loser, "does not match"));
        }
        let content = if request.method == Method::Put {
            let bytes = STANDARD.decode(body["content"].as_str().unwrap()).unwrap();
            if bytes.len() > MAX_BYTES {
                return Ok(reply(422, json!({"message":"file too large"})));
            }
            let sha = blob(&bytes);
            store.files.insert(path.clone(), bytes);
            json!({"sha":sha})
        } else {
            assert_eq!(request.method, Method::Delete);
            assert!(expected.is_some());
            store.files.remove(&path);
            Value::Null
        };
        if store.two_winners {
            // A broken acknowledgement: our returned commit itself contains
            // different bytes, rather than a legitimate later publication.
            store.files.insert(path.clone(), b"another writer".to_vec());
        }
        let commit = store.commit();
        if let Some(bytes) = store.after_success.take() {
            store.after_success_pending = Some((path, bytes));
        }
        if store.malformed_success {
            return Ok(reply(200, json!({})));
        }
        Ok(reply(
            if current.is_none() { 201 } else { 200 },
            json!({"content":content,"commit":{"sha":commit}}),
        ))
    }
}

#[tokio::test]
async fn github_read_write_round_trip_and_raw_exports_do_not_replace_expected_sha() {
    let fake = Fake::default();
    let writer = fake.backend();
    assert!(writer.read_state().await.unwrap().is_none());
    let mut state = StateFile::new();
    state.increment_serial();
    writer.write_state(&state).await.unwrap();
    assert_eq!(
        writer
            .read_state()
            .await
            .unwrap()
            .unwrap()
            .into_state()
            .serial,
        state.serial
    );
    let mut other = StateFile::new();
    other.increment_serial();
    other.increment_serial();
    fake.put("state.json", serde_json::to_vec(&other).unwrap());
    assert!(writer.read_state_bytes().await.unwrap().is_some());
    assert!(matches!(
        writer.write_state(&state).await,
        Err(BackendError::StateWriteConflict { .. })
    ));
}

#[tokio::test]
async fn github_readiness_and_absence_distinguish_destination_failures() {
    let fake = Fake::default();
    assert!(matches!(
        fake.backend().storage_readiness().await.unwrap(),
        StorageReadiness::Ready
    ));
    assert!(fake.backend().read_state().await.unwrap().is_none());
    for repository in [false, true] {
        {
            let mut s = fake.0.lock().unwrap();
            s.missing_repo = repository;
            s.missing_branch = !repository;
        }
        assert!(matches!(
            fake.backend().storage_readiness().await.unwrap(),
            StorageReadiness::Unavailable(_)
        ));
        assert!(fake.backend().read_state().await.is_err());
    }
    let fake = Fake::default();
    fake.0.lock().unwrap().response = Some(reply(401, json!({"message":"Bad credentials"})));
    assert!(matches!(
        fake.backend().storage_readiness().await,
        Err(BackendError::GitHub(GitHubError::Api { status: 401, .. }))
    ));
}

#[tokio::test]
async fn github_locks_acquire_renew_release_force_unlock_and_corruption_fail_closed() {
    let fake = Fake::default();
    let backend = fake.backend();
    let lock = backend.acquire_lock("apply").await.unwrap();
    assert!(matches!(
        fake.backend().acquire_lock("destroy").await,
        Err(BackendError::Locked { .. })
    ));
    let renewed = backend.renew_lock(&lock).await.unwrap();
    assert_eq!(lock.id(), renewed.id());
    assert!(renewed.expires() >= lock.expires());
    assert!(matches!(
        backend.force_unlock("wrong-uuid").await,
        Err(BackendError::LockMismatch { .. })
    ));
    backend.release_lock(&renewed).await.unwrap();
    assert!(fake.bytes("state.json.lock").is_none());
    let lock = backend.acquire_lock("plan").await.unwrap();
    backend.force_unlock(lock.id()).await.unwrap();
    assert!(fake.bytes("state.json.lock").is_none());
    fake.put("state.json.lock", b"corrupt lease".to_vec());
    assert!(matches!(
        backend.acquire_lock("apply").await,
        Err(BackendError::GitHub(GitHubError::CorruptLease))
    ));
    assert!(backend.force_unlock(lock.id()).await.is_err());
}

#[tokio::test]
async fn github_expired_lease_is_replaced_without_delete_and_old_owner_cannot_write() {
    let fake = Fake::default();
    let expired = LockInfo::with_timeout("apply", -1);
    fake.put("state.json.lock", serde_json::to_vec(&expired).unwrap());
    let backend = fake.backend();
    let next = backend.acquire_lock("apply").await.unwrap();
    assert_ne!(next.id(), expired.id());
    assert!(
        !fake
            .0
            .lock()
            .unwrap()
            .requests
            .iter()
            .any(|r| r.method == Method::Delete)
    );
    assert!(matches!(
        backend.renew_lock(&expired).await,
        Err(BackendError::LockNotHeld(_))
    ));
    assert!(matches!(
        backend
            .write_state_locked(&StateFile::new(), &expired)
            .await,
        Err(BackendError::LockNotHeld(_))
    ));
}

#[tokio::test]
async fn github_races_have_one_winner_and_typed_loser_for_409_and_422() {
    for loser in [409, 422] {
        let fake = Fake::default();
        fake.0.lock().unwrap().loser = loser;
        let a = fake.backend();
        let b = fake.backend();
        let (a, b) = tokio::join!(a.acquire_lock("a"), b.acquire_lock("b"));
        assert_ne!(a.is_ok(), b.is_ok());
        let err = if a.is_err() {
            a.unwrap_err()
        } else {
            b.unwrap_err()
        };
        assert!(
            matches!(
                err,
                BackendError::Locked { .. }
                    | BackendError::GitHub(GitHubError::Api { status: 422, .. })
            ),
            "{err:?}"
        );
        assert!(fake.bytes("state.json.lock").is_some());
    }
}

#[tokio::test]
async fn github_branch_contention_retries_unchanged_sha_and_never_rebases_stale_content() {
    let fake = Fake::default();
    let backend = fake.backend();
    assert!(backend.read_state().await.unwrap().is_none());
    fake.0.lock().unwrap().contention = 2;
    backend.write_state(&StateFile::new()).await.unwrap();
    let before = fake.bytes("state.json").unwrap();
    fake.0.lock().unwrap().replace = Some(b"winning state".to_vec());
    assert!(matches!(
        backend.write_state(&StateFile::new()).await,
        Err(BackendError::StateWriteConflict { .. })
    ));
    assert_eq!(fake.bytes("state.json").unwrap(), b"winning state");
    assert_ne!(before, b"winning state");
    let fake = Fake::default();
    fake.0.lock().unwrap().contention = 50;
    assert!(matches!(
        fake.backend().write_state(&StateFile::new()).await,
        Err(BackendError::GitHub(GitHubError::Contention { .. }))
    ));
    let puts = fake
        .0
        .lock()
        .unwrap()
        .requests
        .iter()
        .filter(|r| r.method == Method::Put)
        .count();
    assert_eq!(puts, 5);
}

#[tokio::test]
async fn github_stale_release_and_force_unlock_preserve_replacement_lease() {
    for force in [false, true] {
        for loser in [409, 422] {
            let fake = Fake::default();
            let backend = fake.backend();
            let lock = backend.acquire_lock("apply").await.unwrap();
            let other = LockInfo::new("replacement");
            {
                let mut s = fake.0.lock().unwrap();
                s.replace = Some(serde_json::to_vec(&other).unwrap());
                s.loser = loser;
            }
            let result = if force {
                backend.force_unlock(lock.id()).await
            } else {
                backend.release_lock(&lock).await
            };
            assert!(matches!(
                result,
                Err(BackendError::LockNotHeld(_))
                    | Err(BackendError::GitHub(GitHubError::Api { status: 422, .. }))
            ));
            assert_eq!(
                LockInfo::from_json_slice(&fake.bytes("state.json.lock").unwrap())
                    .unwrap()
                    .id(),
                other.id()
            );
        }
    }
}

#[tokio::test]
async fn github_unexpected_success_non_file_and_malformed_responses_stop_operations() {
    for malformed in [false, true] {
        let fake = Fake::default();
        {
            let mut s = fake.0.lock().unwrap();
            s.malformed_success = malformed;
            s.two_winners = !malformed;
        }
        assert!(matches!(
            fake.backend().write_state(&StateFile::new()).await,
            Err(BackendError::GitHub(GitHubError::UnexpectedOutcome { .. }))
        ));
    }
    let fake = Fake::default();
    fake.put("state.json", b"{}".to_vec());
    fake.0.lock().unwrap().directory = true;
    assert!(matches!(
        fake.backend().read_state_bytes().await,
        Err(BackendError::GitHub(GitHubError::NonFile))
    ));
    fake.0.lock().unwrap().response = Some(reply(200, json!({})));
    assert!(matches!(
        fake.backend().read_state_bytes().await,
        Err(BackendError::GitHub(GitHubError::MalformedResponse))
    ));
}

#[tokio::test]
async fn github_raw_reads_enforce_ten_million_byte_cap_at_boundaries() {
    for size in [MAX_BYTES - 1, MAX_BYTES, MAX_BYTES + 1] {
        let fake = Fake::default();
        fake.put("state.json", vec![b'x'; size]);
        let result = fake.backend().read_state_bytes().await;
        if size <= MAX_BYTES {
            assert_eq!(result.unwrap().unwrap().bytes().len(), size);
        } else {
            assert!(
                matches!(result,Err(BackendError::GitHub(GitHubError::TooLarge {size:found,cap:MAX_BYTES})) if found==size)
            );
        }
    }
}

#[tokio::test]
async fn github_rate_limits_carry_retry_hints_and_cleanup_deadlines_are_bounded() {
    for (status, header, message) in [
        (403, "retry-after", "slow"),
        (429, "retry-after", "slow"),
        (403, "x-ratelimit-remaining", "slow"),
        (403, "", "secondary rate limit"),
    ] {
        let fake = Fake::default();
        let mut response = reply(status, json!({"message":message}));
        if !header.is_empty() {
            response.headers.insert(
                header.into(),
                if header == "retry-after" { "120" } else { "0" }.into(),
            );
        }
        fake.0.lock().unwrap().response = Some(response);
        assert!(matches!(
            fake.backend().read_state_bytes().await,
            Err(BackendError::GitHub(GitHubError::RateLimited { .. }))
        ));
    }
    let fake = Fake::default();
    fake.0.lock().unwrap().delay = Some(Duration::from_secs(5));
    let start = Instant::now();
    assert!(matches!(
        fake.backend()
            .write_state_for_cleanup(&StateFile::new())
            .await,
        Err(BackendError::GitHub(GitHubError::Deadline))
    ));
    assert!(start.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn github_write_cap_is_checked_before_any_mutation() {
    for target in [MAX_BYTES - 1, MAX_BYTES, MAX_BYTES + 1] {
        let fake = Fake::default();
        let backend = fake.backend();
        let mut state = StateFile::new();
        state.carina_version.clear();
        let overhead = carina_core::utils::pretty_with_newline_bytes(&state)
            .unwrap()
            .len();
        state.carina_version = "x".repeat(target - overhead);
        assert_eq!(
            carina_core::utils::pretty_with_newline_bytes(&state)
                .unwrap()
                .len(),
            target
        );
        let result = backend.write_state(&state).await;
        if target <= MAX_BYTES {
            result.unwrap();
            assert_eq!(fake.bytes("state.json").unwrap().len(), target);
        } else {
            assert!(
                matches!(result,Err(BackendError::GitHub(GitHubError::TooLarge {size,cap:MAX_BYTES})) if size==target)
            );
            assert!(
                !fake
                    .0
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .any(|r| r.method != Method::Get)
            );
        }
    }
}

#[tokio::test]
async fn github_full_reads_migrate_but_raw_exports_have_independent_version_gate() {
    let fake = Fake::default();
    let mut value = serde_json::to_value(StateFile::new()).unwrap();
    value["version"] = json!(StateFile::CURRENT_VERSION + 1);
    fake.put("state.json", serde_json::to_vec(&value).unwrap());
    assert!(
        matches!(fake.backend().read_state().await,Err(BackendError::StateRead {error,..}) if matches!(*error,BackendError::StateVersionTooNew {..}))
    );
    crate::read_published_exports(&fake.backend())
        .await
        .unwrap();
    value["version"] = json!(StateFile::CURRENT_VERSION - 1);
    let bytes = serde_json::to_vec(&value).unwrap();
    fake.put("state.json", bytes.clone());
    assert!(matches!(
        fake.backend().read_state().await.unwrap(),
        Some(crate::LoadedState::Migrated { .. })
    ));
    assert_eq!(fake.bytes("state.json").unwrap(), bytes);
}

#[tokio::test]
async fn github_same_sha_update_races_and_renew_takeover_races_are_terminal_for_losers() {
    for loser in [409, 422] {
        let fake = Fake::default();
        let seed = fake.backend();
        seed.write_state(&StateFile::new()).await.unwrap();
        let a = fake.backend();
        let b = fake.backend();
        a.read_state().await.unwrap();
        b.read_state().await.unwrap();
        let mut left = StateFile::new();
        left.serial = 11;
        let mut right = StateFile::new();
        right.serial = 22;
        fake.0.lock().unwrap().loser = loser;
        let (a, b) = tokio::join!(a.write_state(&left), b.write_state(&right));
        assert_ne!(a.is_ok(), b.is_ok(), "{a:?} {b:?}");
        let error = if a.is_err() {
            a.unwrap_err()
        } else {
            b.unwrap_err()
        };
        assert!(
            matches!(
                error,
                BackendError::StateWriteConflict { .. }
                    | BackendError::GitHub(GitHubError::Api { status: 422, .. })
            ),
            "{error:?}"
        );
        let serial = fake
            .backend()
            .read_state()
            .await
            .unwrap()
            .unwrap()
            .into_state()
            .serial;
        assert!(serial == 11 || serial == 22);

        let fake = Fake::default();
        let backend = fake.backend();
        let lock = backend.acquire_lock("apply").await.unwrap();
        let replacement = LockInfo::new("takeover");
        {
            let mut s = fake.0.lock().unwrap();
            s.replace = Some(serde_json::to_vec(&replacement).unwrap());
            s.loser = loser;
        }
        let result = backend.renew_lock(&lock).await;
        assert!(matches!(
            result,
            Err(BackendError::LockNotHeld(_))
                | Err(BackendError::GitHub(GitHubError::Api { status: 422, .. }))
        ));
    }
}

#[tokio::test]
async fn github_branch_contention_is_retried_for_every_lease_mutation() {
    let fake = Fake::default();
    let backend = fake.backend();
    fake.0.lock().unwrap().contention = 1;
    let lock = backend.acquire_lock("apply").await.unwrap();
    fake.0.lock().unwrap().contention = 1;
    let lock = backend.renew_lock(&lock).await.unwrap();
    fake.0.lock().unwrap().contention = 1;
    backend.release_lock(&lock).await.unwrap();
    let lock = backend.acquire_lock("apply").await.unwrap();
    fake.0.lock().unwrap().contention = 1;
    backend.force_unlock(lock.id()).await.unwrap();
}

#[tokio::test]
async fn github_aba_uses_blob_content_identity_without_claiming_generation_fencing() {
    let fake = Fake::default();
    let backend = fake.backend();
    let state = StateFile::new();
    backend.write_state(&state).await.unwrap();
    let a = fake.bytes("state.json").unwrap();
    fake.put("state.json", b"B".to_vec());
    fake.put("state.json", a);
    backend.write_state(&state).await.unwrap();
}

#[tokio::test]
async fn github_api_failures_retain_the_response_message_and_request_id() {
    let fake = Fake::default();
    let mut response = reply(
        422,
        json!({"message":"branch policy requires a signed commit"}),
    );
    response
        .headers
        .insert("x-github-request-id".into(), "ABCD:1234".into());
    fake.0.lock().unwrap().response = Some(response);
    let error = fake.backend().storage_readiness().await.unwrap_err();
    let rendered = error.to_string();
    assert!(
        rendered.contains("branch policy requires a signed commit"),
        "{rendered}"
    );
    assert!(rendered.contains("ABCD:1234"), "{rendered}");
}

#[tokio::test]
async fn github_rate_limit_http_date_is_a_retry_hint_without_sleeping() {
    let fake = Fake::default();
    let mut response = reply(429, json!({"message":"slow down"}));
    let retry = (chrono::Utc::now() + chrono::Duration::minutes(2)).to_rfc2822();
    response.headers.insert("retry-after".into(), retry);
    fake.0.lock().unwrap().response = Some(response);
    let error = fake.backend().storage_readiness().await.unwrap_err();
    assert!(
        matches!(error,BackendError::GitHub(GitHubError::RateLimited {retry_after:Some(hint),..}) if hint>=Duration::from_secs(118) && hint<=Duration::from_secs(120))
    );
}

#[tokio::test]
async fn github_different_state_paths_share_branch_contention_but_not_lease_ownership() {
    let fake = Fake::default();
    let a = fake.backend();
    let config = BackendConfig::GitHub(settings()).to_parser_config();
    let mut other = config;
    other.attributes.insert(
        "path",
        carina_core::resource::Value::Concrete(carina_core::resource::ConcreteValue::String(
            "other.json".into(),
        )),
    );
    let BackendConfig::GitHub(settings) = BackendConfig::try_from(&other).unwrap() else {
        panic!("github");
    };
    let b = GitHubBackend::with_transport(settings, Arc::new(fake.clone()));
    let (left, right) = tokio::join!(a.acquire_lock("left"), b.acquire_lock("right"));
    let left = left.unwrap();
    let right = right.unwrap();
    assert_ne!(left.id(), right.id());
    assert!(fake.bytes("state.json.lock").is_some());
    assert!(fake.bytes("other.json.lock").is_some());
    assert_eq!(
        fake.0
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.method == Method::Put)
            .count(),
        3
    );
}

#[tokio::test]
async fn github_cleanup_checks_lease_before_saving_and_releases_within_the_existing_deadline() {
    let fake = Fake::default();
    let lock = LockInfo::new("apply");
    fake.put("state.json.lock", serde_json::to_vec(&lock).unwrap());
    let backend = fake.backend();
    backend
        .write_state_locked_for_cleanup(&StateFile::new(), &lock)
        .await
        .unwrap();
    backend.release_lock_for_cleanup(&lock).await.unwrap();
    let store = fake.0.lock().unwrap();
    let mutations: Vec<_> = store
        .requests
        .iter()
        .filter(|r| r.method != Method::Get)
        .collect();
    assert_eq!(mutations.len(), 2);
    assert!(mutations[0].path.ends_with("state.json"));
    assert_eq!(mutations[1].method, Method::Delete);
}

struct LateMutation {
    fake: Fake,
    started: tokio::sync::Notify,
    resume: Arc<tokio::sync::Notify>,
    sender: Mutex<Option<tokio::sync::oneshot::Sender<GitHubResponse>>>,
}
#[async_trait]
impl GitHubTransport for LateMutation {
    async fn execute(
        &self,
        request: GitHubRequest,
        deadline: Instant,
    ) -> Result<GitHubResponse, GitHubError> {
        let sender = if request.method == Method::Put {
            self.sender.lock().unwrap().take()
        } else {
            None
        };
        if let Some(sender) = sender {
            let fake = self.fake.clone();
            let resume = Arc::clone(&self.resume);
            // Server processing survives cancellation of the client's future.
            tokio::spawn(async move {
                resume.notified().await;
                let response = fake.execute(request, deadline).await.unwrap();
                let _ = sender.send(response);
            });
            self.started.notify_one();
            std::future::pending().await
        } else {
            self.fake.execute(request, deadline).await
        }
    }
}

#[tokio::test]
async fn github_abandoned_and_cleanup_writes_preserve_preconditions_in_both_completion_orders() {
    for old_first in [false, true] {
        let fake = Fake::default();
        fake.backend().write_state(&StateFile::new()).await.unwrap();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let transport = Arc::new(LateMutation {
            fake: fake.clone(),
            started: tokio::sync::Notify::new(),
            resume: Arc::new(tokio::sync::Notify::new()),
            sender: Mutex::new(Some(sender)),
        });
        let backend = Arc::new(GitHubBackend::with_transport(settings(), transport.clone()));
        backend.read_state().await.unwrap();
        let old_backend = Arc::clone(&backend);
        let job = tokio::spawn(async move {
            let mut state = StateFile::new();
            state.serial = 1;
            old_backend.write_state(&state).await
        });
        transport.started.notified().await;
        job.abort();
        assert!(job.await.unwrap_err().is_cancelled());
        let mut latest = StateFile::new();
        latest.serial = 2;
        if old_first {
            transport.resume.notify_one();
            assert_eq!(receiver.await.unwrap().status, 200);
            assert!(matches!(
                backend.write_state_for_cleanup(&latest).await,
                Err(BackendError::StateWriteConflict { .. })
            ));
            assert_eq!(
                fake.backend()
                    .read_state()
                    .await
                    .unwrap()
                    .unwrap()
                    .into_state()
                    .serial,
                1
            );
        } else {
            backend.write_state_for_cleanup(&latest).await.unwrap();
            transport.resume.notify_one();
            assert_eq!(receiver.await.unwrap().status, 409);
            assert_eq!(
                fake.backend()
                    .read_state()
                    .await
                    .unwrap()
                    .unwrap()
                    .into_state()
                    .serial,
                2
            );
        }
    }
}

#[tokio::test]
async fn github_exports_and_state_reads_issue_only_gets_without_leases() {
    let fake = Fake::default();
    fake.put("state.json", serde_json::to_vec(&StateFile::new()).unwrap());
    let backend = fake.backend();
    backend.read_state().await.unwrap();
    backend.read_state_bytes().await.unwrap();
    crate::read_published_exports(&backend).await.unwrap();
    assert!(
        fake.0
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|request| request.method == Method::Get && !request.path.ends_with(".lock"))
    );
}

#[tokio::test]
async fn github_readiness_confirms_archived_storage_is_unavailable_and_transport_failure_is_error()
{
    let fake = Fake::default();
    fake.0.lock().unwrap().response = Some(reply(200, json!({"id":1,"archived":true})));
    assert!(matches!(
        fake.backend().storage_readiness().await.unwrap(),
        StorageReadiness::Unavailable(_)
    ));
    struct Failed;
    #[async_trait]
    impl GitHubTransport for Failed {
        async fn execute(
            &self,
            _: GitHubRequest,
            _: Instant,
        ) -> Result<GitHubResponse, GitHubError> {
            Err(GitHubError::Transport {
                kind: "offline failure",
                source: reqwest::Client::new()
                    .get("http://[invalid")
                    .build()
                    .unwrap_err(),
            })
        }
    }
    let backend = GitHubBackend::with_transport(settings(), Arc::new(Failed));
    assert!(matches!(
        backend.storage_readiness().await,
        Err(BackendError::GitHub(GitHubError::Transport { .. }))
    ));
    assert!(backend.read_state().await.is_err());
}

#[tokio::test]
async fn github_cleanup_protocol_fits_the_flush_budget_with_fifty_ms_transport_latency() {
    let fake = Fake::default();
    fake.put("state.json", serde_json::to_vec(&StateFile::new()).unwrap());
    let backend = fake.backend();
    backend.read_state().await.unwrap();
    let lock = backend.acquire_lock("apply").await.unwrap();
    fake.0.lock().unwrap().delay = Some(Duration::from_millis(50));
    let start = Instant::now();
    backend
        .write_state_locked_for_cleanup(&StateFile::new(), &lock)
        .await
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    backend.release_lock_for_cleanup(&lock).await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn github_round1_acquire_readback_ignores_a_later_client_write() {
    let fake = Fake::default();
    let later = LockInfo::new("later client");
    fake.0.lock().unwrap().after_success = Some(serde_json::to_vec(&later).unwrap());
    let acquired = fake.backend().acquire_lock("apply").await.unwrap();
    assert_ne!(acquired.id(), later.id());
    assert_eq!(
        LockInfo::from_json_slice(&fake.bytes("state.json.lock").unwrap())
            .unwrap()
            .id(),
        later.id()
    );
    assert_pinned_readback(&fake, 1);
}

#[tokio::test]
async fn github_round1_release_readback_ignores_a_new_lease() {
    let fake = Fake::default();
    let backend = fake.backend();
    let lock = backend.acquire_lock("apply").await.unwrap();
    let later = LockInfo::new("later client");
    {
        let mut store = fake.0.lock().unwrap();
        store.requests.clear();
        store.after_success = Some(serde_json::to_vec(&later).unwrap());
    }
    backend.release_lock(&lock).await.unwrap();
    assert_eq!(
        LockInfo::from_json_slice(&fake.bytes("state.json.lock").unwrap())
            .unwrap()
            .id(),
        later.id()
    );
    assert_pinned_readback(&fake, 1);
}

#[tokio::test]
async fn github_round1_state_readback_ignores_a_later_state_write() {
    let fake = Fake::default();
    let later = StateFile::new();
    let later_bytes = serde_json::to_vec(&later).unwrap();
    fake.0.lock().unwrap().after_success = Some(later_bytes.clone());
    let backend = fake.backend();
    backend.write_state(&StateFile::new()).await.unwrap();
    assert_eq!(fake.bytes("state.json").unwrap(), later_bytes);
    assert_pinned_readback(&fake, 1);
    assert!(matches!(
        backend.write_state(&StateFile::new()).await,
        Err(BackendError::StateWriteConflict { .. })
    ));
}

fn assert_pinned_readback(fake: &Fake, count: usize) {
    let store = fake.0.lock().unwrap();
    let mutation = store
        .requests
        .iter()
        .rposition(|r| r.method != Method::Get)
        .unwrap();
    let reads = &store.requests[mutation + 1..];
    assert_eq!(reads.len(), count, "read-back request count");
    assert_eq!(reads[0].accept, JSON_MEDIA_TYPE);
    if count == 2 {
        assert_eq!(reads[1].accept, RAW_MEDIA_TYPE);
    }
    for read in reads {
        assert_eq!(read.method, Method::Get);
        assert!(read.path.contains("/contents/"));
        let reference = read.reference.as_ref().expect("pinned ref");
        assert_eq!(reference.len(), 40);
        assert_eq!(reference, reads[0].reference.as_ref().unwrap());
    }
}

#[tokio::test]
async fn github_round1_readback_uses_inline_content_and_raw_only_for_large_files() {
    for size in [1_000_000, 1_000_001] {
        let fake = Fake::default();
        let mut state = StateFile::new();
        state.carina_version.clear();
        let overhead = carina_core::utils::pretty_with_newline_bytes(&state)
            .unwrap()
            .len();
        state.carina_version = "x".repeat(size - overhead);
        fake.backend().write_state(&state).await.unwrap();
        assert_pinned_readback(&fake, if size == 1_000_000 { 1 } else { 2 });
    }
    let fake = Fake::default();
    fake.backend().write_state(&StateFile::new()).await.unwrap();
    assert_pinned_readback(&fake, 1);
}

#[tokio::test]
async fn github_round1_cleanup_save_has_one_lease_check_no_renewal_and_pinned_readback() {
    let fake = Fake::default();
    let backend = fake.backend();
    backend.read_state().await.unwrap();
    let lock = backend.acquire_lock("apply").await.unwrap();
    fake.0.lock().unwrap().requests.clear();
    backend
        .write_state_locked_for_cleanup(&StateFile::new(), &lock)
        .await
        .unwrap();
    {
        let store = fake.0.lock().unwrap();
        assert_eq!(store.requests.len(), 4);
        assert_eq!(
            store
                .requests
                .iter()
                .filter(|r| r.method == Method::Put)
                .count(),
            1
        );
        assert!(store.requests[2].path.ends_with("state.json"));
    }
    assert_pinned_readback(&fake, 1);
    fake.0.lock().unwrap().requests.clear();
    backend.release_lock_for_cleanup(&lock).await.unwrap();
    assert_eq!(fake.0.lock().unwrap().requests.len(), 4);
    assert_pinned_readback(&fake, 1);
}

#[tokio::test]
async fn github_round1_expired_takeover_retries_create_when_the_old_lease_disappears() {
    let fake = Fake::default();
    let expired = LockInfo::with_timeout("apply", -1);
    fake.put("state.json.lock", serde_json::to_vec(&expired).unwrap());
    fake.0.lock().unwrap().remove_before_mutation = true;
    let lock = fake.backend().acquire_lock("apply").await.unwrap();
    assert_ne!(lock.id(), expired.id());
    let store = fake.0.lock().unwrap();
    let puts: Vec<_> = store
        .requests
        .iter()
        .filter(|r| r.method == Method::Put)
        .collect();
    assert_eq!(puts.len(), 2);
    assert!(puts[0].body.as_ref().unwrap().get("sha").is_some());
    assert!(puts[1].body.as_ref().unwrap().get("sha").is_none());
    assert!(!store.requests.iter().any(|r| r.method == Method::Delete));
}

fn rate_limit(status: u16, header: &str, hint: String) -> GitHubResponse {
    let mut response = reply(status, json!({"message":"rate limited"}));
    response.headers.insert(header.into(), hint);
    if header == "x-ratelimit-reset" {
        response
            .headers
            .insert("x-ratelimit-remaining".into(), "0".into());
    }
    response
}

#[tokio::test(start_paused = true)]
async fn github_round1_rate_limits_retry_short_hints_within_the_deadline() {
    for (status, header, hint) in [
        (403, "retry-after", "1".into()),
        (429, "retry-after", "1".into()),
        (
            403,
            "x-ratelimit-reset",
            (chrono::Utc::now().timestamp() + 2).to_string(),
        ),
    ] {
        let fake = Fake::default();
        fake.0.lock().unwrap().response = Some(rate_limit(status, header, hint));
        let start = Instant::now();
        fake.backend().storage_readiness().await.unwrap();
        assert!(start.elapsed() >= Duration::from_secs(1));
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(fake.0.lock().unwrap().requests.len(), 3);
    }
}

#[tokio::test(start_paused = true)]
async fn github_round1_rate_limits_allow_at_most_two_retries() {
    let fake = Fake::default();
    fake.0.lock().unwrap().responses = (0..3)
        .map(|_| rate_limit(429, "retry-after", "1".into()))
        .collect();
    let start = Instant::now();
    assert!(matches!(
        fake.backend().storage_readiness().await,
        Err(BackendError::GitHub(GitHubError::RateLimited {
            status: 429,
            ..
        }))
    ));
    assert_eq!(fake.0.lock().unwrap().requests.len(), 3);
    assert_eq!(start.elapsed(), Duration::from_secs(2));
}

#[tokio::test(start_paused = true)]
async fn github_round1_rate_limits_do_not_wait_without_a_fitting_hint() {
    for response in [
        rate_limit(429, "retry-after", "30".into()),
        reply(429, json!({"message":"rate limited"})),
        reply(403, json!({"message":"secondary rate limit"})),
    ] {
        let fake = Fake::default();
        fake.0.lock().unwrap().response = Some(response);
        let start = Instant::now();
        assert!(matches!(
            fake.backend().storage_readiness().await,
            Err(BackendError::GitHub(GitHubError::RateLimited { .. }))
        ));
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(fake.0.lock().unwrap().requests.len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn github_round1_cleanup_never_waits_on_rate_limits() {
    for operation in 0..3 {
        let fake = Fake::default();
        let backend = fake.backend();
        let lock = LockInfo::new("apply");
        fake.0.lock().unwrap().response = Some(rate_limit(429, "retry-after", "0".into()));
        let start = Instant::now();
        let result = match operation {
            0 => backend.write_state_for_cleanup(&StateFile::new()).await,
            1 => {
                backend
                    .write_state_locked_for_cleanup(&StateFile::new(), &lock)
                    .await
            }
            _ => backend.release_lock_for_cleanup(&lock).await,
        };
        assert!(matches!(
            result,
            Err(BackendError::GitHub(GitHubError::RateLimited { .. }))
        ));
        assert_eq!(start.elapsed(), Duration::ZERO);
        assert_eq!(fake.0.lock().unwrap().requests.len(), 1);
    }
}

#[tokio::test]
async fn github_round1_readback_rejects_wrong_inline_bytes_even_with_the_expected_sha() {
    let fake = Fake::default();
    fake.0.lock().unwrap().corrupt_inline = true;
    assert!(matches!(
        fake.backend().write_state(&StateFile::new()).await,
        Err(BackendError::GitHub(GitHubError::UnexpectedOutcome { .. }))
    ));
}

struct RateOnEachRequest {
    fake: Fake,
    calls: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl GitHubTransport for RateOnEachRequest {
    async fn execute(
        &self,
        request: GitHubRequest,
        deadline: Instant,
    ) -> Result<GitHubResponse, GitHubError> {
        if self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .is_multiple_of(2)
        {
            Ok(rate_limit(429, "retry-after", "1".into()))
        } else {
            self.fake.execute(request, deadline).await
        }
    }
}
#[tokio::test(start_paused = true)]
async fn github_round1_rate_retry_budget_is_shared_by_the_entire_operation() {
    let transport = Arc::new(RateOnEachRequest {
        fake: Fake::default(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let backend = GitHubBackend::with_transport(settings(), transport.clone());
    let start = Instant::now();
    assert!(matches!(
        backend.acquire_lock("apply").await,
        Err(BackendError::GitHub(GitHubError::RateLimited { .. }))
    ));
    assert_eq!(
        transport.calls.load(std::sync::atomic::Ordering::Relaxed),
        5
    );
    assert_eq!(start.elapsed(), Duration::from_secs(2));
}

struct RateAfterSlowRead {
    fake: Fake,
    calls: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl GitHubTransport for RateAfterSlowRead {
    async fn execute(
        &self,
        request: GitHubRequest,
        deadline: Instant,
    ) -> Result<GitHubResponse, GitHubError> {
        if self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            == 0
        {
            let response = self.fake.execute(request, deadline).await?;
            tokio::time::sleep(Duration::from_secs(20)).await;
            Ok(response)
        } else {
            Ok(rate_limit(429, "retry-after", "11".into()))
        }
    }
}
#[tokio::test(start_paused = true)]
async fn github_round1_rate_hint_must_fit_the_remaining_operation_deadline() {
    let transport = Arc::new(RateAfterSlowRead {
        fake: Fake::default(),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let backend = GitHubBackend::with_transport(settings(), transport.clone());
    let start = Instant::now();
    assert!(matches!(
        backend.storage_readiness().await,
        Err(BackendError::GitHub(GitHubError::RateLimited { .. }))
    ));
    assert_eq!(
        transport.calls.load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    assert_eq!(start.elapsed(), Duration::from_secs(20));
}

#[tokio::test]
async fn github_round1_cleanup_request_counts_cover_cold_and_cached_small_and_large_saves() {
    for cached in [false, true] {
        for locked in [false, true] {
            for large in [false, true] {
                let fake = Fake::default();
                let mut previous = StateFile::new();
                previous.carina_version = "x".repeat(1_000_001);
                fake.put("state.json", serde_json::to_vec(&previous).unwrap());
                let backend = fake.backend();
                if cached {
                    backend.read_state().await.unwrap();
                }
                let lock = if locked {
                    Some(backend.acquire_lock("apply").await.unwrap())
                } else {
                    None
                };
                fake.0.lock().unwrap().requests.clear();
                let mut state = StateFile::new();
                if large {
                    state.carina_version = "x".repeat(1_000_001);
                }
                if let Some(lock) = &lock {
                    backend
                        .write_state_locked_for_cleanup(&state, lock)
                        .await
                        .unwrap();
                } else {
                    backend.write_state_for_cleanup(&state).await.unwrap();
                }
                let expected =
                    2 + usize::from(locked) * 2 + usize::from(!cached) * 2 + usize::from(large);
                let store = fake.0.lock().unwrap();
                assert_eq!(
                    store.requests.len(),
                    expected,
                    "cached={cached}, locked={locked}, large={large}"
                );
                assert_eq!(
                    store
                        .requests
                        .iter()
                        .filter(|r| r.method == Method::Put)
                        .count(),
                    1
                );
                assert_eq!(
                    store
                        .requests
                        .iter()
                        .filter(|r| r.accept == RAW_MEDIA_TYPE)
                        .count(),
                    usize::from(large),
                    "only read-back needs raw bytes; the old state's SHA needs one metadata GET"
                );
            }
        }
    }
}

#[tokio::test]
async fn github_round1_cleanup_never_saves_with_an_expired_or_replaced_lease() {
    for expired in [false, true] {
        let fake = Fake::default();
        let backend = fake.backend();
        backend.read_state().await.unwrap();
        let lock = if expired {
            LockInfo::with_timeout("apply", -1)
        } else {
            LockInfo::new("apply")
        };
        let replacement = LockInfo::new("replacement");
        let current = if expired { &lock } else { &replacement };
        fake.put("state.json.lock", serde_json::to_vec(current).unwrap());
        fake.0.lock().unwrap().requests.clear();
        assert!(matches!(
            backend
                .write_state_locked_for_cleanup(&StateFile::new(), &lock)
                .await,
            Err(BackendError::LockNotHeld(_))
        ));
        assert!(
            fake.0
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|r| r.method == Method::Get)
        );
        assert!(fake.bytes("state.json").is_none());
    }
}
