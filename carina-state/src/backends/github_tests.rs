//! Offline Contents store. Every mutation captures the branch head before
//! yielding, then checks the head and GitHub's measured blob preconditions.
use super::*;
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
    contents_status: Option<u16>,
    root_status: Option<u16>,
    mutation_response: Option<GitHubResponse>,
    mutation_responses: VecDeque<GitHubResponse>,
    requires_pull_request: bool,
    files: BTreeMap<String, Vec<u8>>,
    history: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    parents: BTreeMap<String, Option<String>>,
    head: u64,
    next_commit: u64,
    requests: Vec<GitHubRequest>,
    contention: usize,
    replace: Option<Vec<u8>>,
    loser: u16,
    response: Option<GitHubResponse>,
    responses: VecDeque<GitHubResponse>,
    malformed_success: bool,
    wrong_blob_sha: bool,
    two_winners: bool,
    compare_response: Option<Value>,
    fail_read_after_commit: bool,
    after_commit_error: Option<GitHubError>,
    after_commit_response: Option<GitHubResponse>,
    after_success: Option<Vec<u8>>,
    after_success_pending: Option<(String, Vec<u8>)>,
    remove_before_mutation: bool,
    directory: bool,
    delay: Option<Duration>,
}
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<Store>>);

/// Model a real deadline: optionally commit, then lose the reply until after
/// the caller's absolute deadline. Cancellation must preserve the ambiguity.
struct LostReply {
    fake: Fake,
    method: Method,
    commit: bool,
    path_fragment: &'static str,
    armed: std::sync::atomic::AtomicBool,
}
impl LostReply {
    fn new(fake: Fake, method: Method, commit: bool) -> Arc<Self> {
        Arc::new(Self {
            fake,
            method,
            commit,
            path_fragment: "",
            armed: std::sync::atomic::AtomicBool::new(false),
        })
    }
    fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}
#[async_trait]
impl GitHubTransport for LostReply {
    async fn execute(
        &self,
        request: GitHubRequest,
        deadline: Instant,
    ) -> Result<GitHubResponse, GitHubError> {
        if request.method == self.method
            && request.path.contains(self.path_fragment)
            && self.armed.swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            if self.commit {
                self.fake.execute(request, deadline).await?;
            }
            tokio::time::sleep_until(deadline + Duration::from_secs(1)).await;
            return Err(GitHubError::Deadline);
        }
        self.fake.execute(request, deadline).await
    }
}

#[tokio::test(start_paused = true)]
async fn github_round3_state_timeout_is_resolved_by_one_metadata_read_on_the_next_save() {
    for committed in [false, true] {
        for replacing in [false, true] {
            let fake = Fake::default();
            let transport = LostReply::new(fake.clone(), Method::Put, committed);
            let backend = GitHubBackend::with_transport(settings(), transport.clone());
            let mut state = StateFile::new();
            if replacing {
                backend.write_state(&state).await.unwrap();
            }
            backend.read_state().await.unwrap();
            transport.arm();
            state.serial = 1;
            let start = Instant::now();
            assert!(matches!(
                backend.write_state(&state).await,
                Err(BackendError::GitHub(GitHubError::MutationDeadline))
            ));
            assert_eq!(start.elapsed(), Duration::from_secs(30));
            fake.0.lock().unwrap().requests.clear();
            state.serial = 2;
            backend.write_state(&state).await.unwrap();
            let store = fake.0.lock().unwrap();
            assert_eq!(
                store
                    .requests
                    .iter()
                    .filter(|r| r.method == Method::Get && r.path.ends_with("/contents/state.json"))
                    .count(),
                1
            );
            assert!(store.requests.iter().all(|r| r.accept != RAW_MEDIA_TYPE));
            assert_eq!(
                store.files["state.json"],
                carina_core::utils::pretty_with_newline_bytes(&state).unwrap()
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn github_round3_ambiguous_state_write_never_overwrites_a_different_winner() {
    let fake = Fake::default();
    let transport = LostReply::new(fake.clone(), Method::Put, true);
    let backend = GitHubBackend::with_transport(settings(), transport.clone());
    backend.read_state().await.unwrap();
    transport.arm();
    assert!(backend.write_state(&StateFile::new()).await.is_err());
    fake.put("state.json", b"another writer".to_vec());
    fake.0.lock().unwrap().requests.clear();
    assert!(matches!(
        backend.write_state(&StateFile::new()).await,
        Err(BackendError::StateWriteConflict { .. })
    ));
    assert!(
        fake.0
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|r| r.method == Method::Get)
    );
    assert_eq!(fake.bytes("state.json").unwrap(), b"another writer");
}

#[tokio::test(start_paused = true)]
async fn github_round3_acquire_timeout_recovers_our_uuid_on_the_next_acquire() {
    for takeover in [false, true] {
        for committed in [false, true] {
            let fake = Fake::default();
            if takeover {
                let expired = LockInfo::with_timeout("expired", -1);
                fake.put("state.json.lock", serde_json::to_vec(&expired).unwrap());
            }
            let transport = LostReply::new(fake.clone(), Method::Put, committed);
            let backend = GitHubBackend::with_transport(settings(), transport.clone());
            transport.arm();
            let start = Instant::now();
            assert!(matches!(
                backend.acquire_lock("apply").await,
                Err(BackendError::GitHub(GitHubError::MutationDeadline))
            ));
            assert_eq!(start.elapsed(), Duration::from_secs(30));
            let sent = fake.bytes("state.json.lock");
            fake.0.lock().unwrap().requests.clear();
            let acquired = backend.acquire_lock("apply").await.unwrap();
            if committed {
                assert_eq!(serde_json::to_vec(&acquired).unwrap(), sent.unwrap());
                assert!(
                    fake.0
                        .lock()
                        .unwrap()
                        .requests
                        .iter()
                        .all(|r| r.method == Method::Get)
                );
            }
            backend.release_lock(&acquired).await.unwrap();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn github_round3_renew_timeout_recovers_on_the_next_acquire_or_renew() {
    for acquire in [false, true] {
        let fake = Fake::default();
        let transport = LostReply::new(fake.clone(), Method::Put, true);
        let backend = GitHubBackend::with_transport(settings(), transport.clone());
        let lock = backend.acquire_lock("apply").await.unwrap();
        transport.arm();
        assert!(matches!(
            backend.renew_lock(&lock).await,
            Err(BackendError::GitHub(GitHubError::MutationDeadline))
        ));
        let recovered = if acquire {
            backend.acquire_lock("apply").await.unwrap()
        } else {
            backend.renew_lock(&lock).await.unwrap()
        };
        assert_eq!(recovered.id(), lock.id());
        backend.release_lock(&recovered).await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn github_round3_compare_timeout_does_not_strand_acquire_or_release() {
    for releasing in [false, true] {
        let fake = Fake::default();
        let transport = Arc::new(LostReply {
            fake: fake.clone(),
            method: Method::Get,
            commit: true,
            path_fragment: "/compare/",
            armed: std::sync::atomic::AtomicBool::new(false),
        });
        let backend = GitHubBackend::with_transport(settings(), transport.clone());
        let lock = if releasing {
            Some(backend.acquire_lock("apply").await.unwrap())
        } else {
            None
        };
        transport.arm();
        let result = if let Some(lock) = &lock {
            backend.release_lock(lock).await
        } else {
            backend.acquire_lock("apply").await.map(|_| ())
        };
        assert!(matches!(
            result,
            Err(BackendError::GitHub(GitHubError::MutationDeadline))
        ));
        fake.0.lock().unwrap().requests.clear();
        if let Some(lock) = &lock {
            backend.release_lock(lock).await.unwrap();
        } else {
            let lock = backend.acquire_lock("apply").await.unwrap();
            assert_eq!(
                fake.bytes("state.json.lock").unwrap(),
                serde_json::to_vec(&lock).unwrap()
            );
        }
        assert!(
            fake.0
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|r| r.method == Method::Get)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn github_round3_known_201_renewal_is_not_recovered_as_ours_after_compare_timeout() {
    let fake = Fake::default();
    let transport = Arc::new(LostReply {
        fake: fake.clone(),
        method: Method::Get,
        commit: true,
        path_fragment: "/compare/",
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let backend = GitHubBackend::with_transport(settings(), transport.clone());
    let lock = backend.acquire_lock("apply").await.unwrap();
    fake.0.lock().unwrap().remove_before_mutation = true;
    transport.arm();
    assert!(matches!(
        backend.renew_lock(&lock).await,
        Err(BackendError::LockNotHeld(_))
            | Err(BackendError::GitHub(GitHubError::MutationDeadline))
    ));
    fake.0.lock().unwrap().requests.clear();
    assert!(matches!(
        backend.write_state_locked(&StateFile::new(), &lock).await,
        Err(BackendError::LockNotHeld(_))
    ));
    assert!(matches!(
        backend.renew_lock(&lock).await,
        Err(BackendError::LockNotHeld(_))
    ));
    assert!(matches!(
        backend.acquire_lock("apply").await,
        Err(BackendError::Locked { .. })
    ));
    assert!(
        fake.0
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|r| r.method == Method::Get)
    );
    // As with any failed rollback, the known recreated lease remains available
    // for explicit cleanup; it must never become a recovered acquisition.
    backend.force_unlock(lock.id()).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn github_round3_lost_reply_resurrection_keeps_the_existing_matching_bytes_policy() {
    let fake = Fake::default();
    let transport = LostReply::new(fake.clone(), Method::Put, true);
    let backend = GitHubBackend::with_transport(settings(), transport.clone());
    backend.write_state(&StateFile::new()).await.unwrap();
    let lock = backend.acquire_lock("apply").await.unwrap();
    for lease in [false, true] {
        fake.0.lock().unwrap().remove_before_mutation = true;
        transport.arm();
        let result = if lease {
            backend.renew_lock(&lock).await.map(|_| ())
        } else {
            backend.write_state(&StateFile::new()).await
        };
        assert!(matches!(
            result,
            Err(BackendError::GitHub(GitHubError::MutationDeadline))
        ));
        // Accepted limitation: a lost 201 is recovered by exact bytes,
        // even if the path was recreated.
        if lease {
            backend.renew_lock(&lock).await.unwrap();
        } else {
            backend.write_state(&StateFile::new()).await.unwrap();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn github_round3_lost_delete_reply_is_recovered_without_deleting_a_successor() {
    for force in [false, true] {
        for successor in [false, true] {
            for committed in [false, true] {
                let fake = Fake::default();
                let transport = LostReply::new(fake.clone(), Method::Delete, committed);
                let backend = GitHubBackend::with_transport(settings(), transport.clone());
                let lock = backend.acquire_lock("apply").await.unwrap();
                transport.arm();
                let result = if force {
                    backend.force_unlock(lock.id()).await
                } else {
                    backend.release_lock(&lock).await
                };
                assert!(matches!(
                    result,
                    Err(BackendError::GitHub(GitHubError::MutationDeadline))
                ));
                let replacement = LockInfo::new("successor");
                if successor {
                    fake.put("state.json.lock", serde_json::to_vec(&replacement).unwrap());
                }
                fake.0.lock().unwrap().requests.clear();
                if force {
                    backend.force_unlock(lock.id()).await.unwrap();
                } else {
                    backend.release_lock(&lock).await.unwrap();
                }
                if committed || successor {
                    assert!(
                        fake.0
                            .lock()
                            .unwrap()
                            .requests
                            .iter()
                            .all(|r| r.method == Method::Get)
                    );
                }
                assert_eq!(
                    fake.bytes("state.json.lock"),
                    successor.then(|| serde_json::to_vec(&replacement).unwrap())
                );
            }
        }
    }
}

#[tokio::test]
async fn github_round3_lost_delete_transport_reply_recovers_in_the_same_call() {
    for force in [false, true] {
        for successor in [false, true] {
            let fake = Fake::default();
            let backend = fake.backend();
            let lock = backend.acquire_lock("apply").await.unwrap();
            let replacement = LockInfo::new("successor");
            {
                let mut store = fake.0.lock().unwrap();
                store.after_commit_error = Some(GitHubError::Transport {
                    kind: "lost delete reply",
                    error: reqwest::Client::new()
                        .get("http://[invalid")
                        .build()
                        .unwrap_err(),
                });
                if successor {
                    store.after_success = Some(serde_json::to_vec(&replacement).unwrap());
                }
            }
            if force {
                backend.force_unlock(lock.id()).await.unwrap();
            } else {
                backend.release_lock(&lock).await.unwrap();
            }
            assert_eq!(
                fake.bytes("state.json.lock"),
                successor.then(|| serde_json::to_vec(&replacement).unwrap())
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn github_round3_committed_delete_rate_retry_accepts_absence_or_a_successor() {
    for force in [false, true] {
        for successor in [false, true] {
            let fake = Fake::default();
            let backend = fake.backend();
            let lock = backend.acquire_lock("apply").await.unwrap();
            let replacement = LockInfo::new("successor");
            {
                let mut store = fake.0.lock().unwrap();
                store.after_commit_response = Some(rate_limit(429, "retry-after", "0".into()));
                if successor {
                    store.after_success = Some(serde_json::to_vec(&replacement).unwrap());
                }
            }
            if force {
                backend.force_unlock(lock.id()).await.unwrap();
            } else {
                backend.release_lock(&lock).await.unwrap();
            }
            assert_eq!(
                fake.bytes("state.json.lock"),
                successor.then(|| serde_json::to_vec(&replacement).unwrap())
            );
        }
    }
}

#[tokio::test]
async fn github_round3_missing_sha_422_state_creator_race_is_typed() {
    let fake = Fake::default();
    let backend = fake.backend();
    backend.read_state().await.unwrap();
    fake.put("state.json", b"winning creator".to_vec());
    assert!(matches!(
        backend.write_state(&StateFile::new()).await,
        Err(BackendError::StateWriteConflict { .. })
    ));
    assert_eq!(fake.bytes("state.json").unwrap(), b"winning creator");
}

#[tokio::test]
async fn github_round3_other_422_validation_failures_remain_terminal_api_errors() {
    for replacing in [false, true] {
        let fake = Fake::default();
        let backend = fake.backend();
        if replacing {
            backend.write_state(&StateFile::new()).await.unwrap();
        }
        backend.read_state().await.unwrap();
        // Force a validation failure before the transport can commit anything.
        let mut responses = VecDeque::new();
        responses.push_back(reply(422, json!({"message":"file too large"})));
        fake.0.lock().unwrap().responses = responses;
        assert!(matches!(
            backend.write_state(&StateFile::new()).await,
            Err(BackendError::GitHub(GitHubError::Api { status: 422, .. }))
        ));
    }
}

#[tokio::test]
async fn github_round3_compare_has_an_independent_one_mib_limit_and_committed_error() {
    for overflow in [false, true] {
        let fake = Fake::default();
        let backend = fake.backend();
        let mut comparison = json!({"status":"identical","ahead_by":0,"behind_by":0,"base_commit":null,"files":[{"patch":""}]});
        let padding =
            1_048_576 + usize::from(overflow) - serde_json::to_vec(&comparison).unwrap().len();
        comparison["files"][0]["patch"] = json!("x".repeat(padding));
        fake.0.lock().unwrap().compare_response = Some(comparison);
        let result = backend.write_state(&StateFile::new()).await;
        if overflow {
            let error = result.unwrap_err();
            assert!(matches!(
                &error,
                BackendError::GitHub(GitHubError::CompareTooLarge {
                    size: 1_048_577,
                    cap: MAX_COMPARE_BYTES
                })
            ));
            let message = error.to_string();
            assert!(
                message.contains("committed") && message.contains("could not be verified"),
                "{error:?}"
            );
            // The acknowledged SHA is still usable after verification fails.
            let mut next = StateFile::new();
            next.serial = 1;
            backend.write_state(&next).await.unwrap();
        } else {
            result.unwrap();
        }
    }
}

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
        let parent = (self.head != 0).then(|| format!("{:040x}", self.head));
        self.next_commit += 1;
        self.head = self.next_commit;
        let commit = format!("{:040x}", self.head);
        self.history.insert(commit.clone(), self.files.clone());
        self.parents.insert(commit.clone(), parent);
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
    fn published_backend(&self) -> crate::PublishedBackend {
        crate::PublishedBackend::for_test(
            Arc::new(self.backend()),
            &carina_core::shutdown::ShutdownToken::running(),
        )
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
        super::super::create_explicit_backend(&url).await.unwrap();
    });
    assert_eq!(
        super::super::s3::S3_CONSTRUCTIONS.load(std::sync::atomic::Ordering::Relaxed),
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
            if request.method != Method::Get
                && let Some(response) = store
                    .mutation_responses
                    .pop_front()
                    .or_else(|| store.mutation_response.take())
            {
                return Ok(response);
            }
            if request.method == Method::Get
                && let Some((path, bytes)) = store.after_success_pending.take()
            {
                // Our 2xx has returned to the client. Publish the rival's
                // next revision immediately before verifying reachability.
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
        if let Some(comparison) = request.path.strip_prefix("/repos/owner/repo/compare/") {
            assert_eq!(request.method, Method::Get);
            assert!(request.reference.is_none());
            let base = comparison
                .strip_suffix("...states%2Fprod")
                .expect("encoded compare branch");
            if let Some(mut response) = store.compare_response.take() {
                // Isolate status failures from unrelated metadata failures.
                if response.get("base_commit").is_none() {
                    response["base_commit"] = json!({"sha":base, "parents":store.parents[base].iter().map(|sha| json!({"sha":sha})).collect::<Vec<_>>()});
                }
                return Ok(reply(200, response));
            }
            let mut cursor = Some(format!("{:040x}", store.head));
            let mut ahead = 0;
            while let Some(commit) = cursor {
                if commit == base {
                    return Ok(reply(
                        200,
                        json!({
                            "status": if ahead == 0 { "identical" } else { "ahead" },
                            "ahead_by": ahead, "behind_by": 0,
                            "base_commit": {"sha": base, "parents": store.parents[base].iter().map(|sha| json!({"sha": sha})).collect::<Vec<_>>()}
                        }),
                    ));
                }
                cursor = store.parents.get(&commit).cloned().flatten();
                ahead += 1;
            }
            return Ok(reply(
                200,
                json!({"status":"diverged","ahead_by":1,"behind_by":1}),
            ));
        }
        if request.path == "/repos/owner/repo/contents" {
            assert_eq!(request.method, Method::Get);
            let files = &store.history[request.reference.as_ref().unwrap()];
            // M5: even an empty-tree branch has a readable root, returning [].
            let entries: Vec<_> = files
                .keys()
                .map(|path| json!({"type":"file", "name":path}))
                .collect();
            return Ok(reply(store.root_status.unwrap_or(200), json!(entries)));
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
            if let Some(status) = store.contents_status {
                return Ok(reply(
                    status,
                    json!({"message":"Resource not accessible by integration"}),
                ));
            }
            let reference = request
                .reference
                .as_deref()
                .expect("Contents reads require ref");
            // The inline cleanup probe sends the qualified head ref. Reject a
            // short ref so tests catch regressions to an ambiguous tag/name.
            let files = if reference == "refs/heads/states/prod" {
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
                format!("{}\n", STANDARD.encode(bytes))
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
        if store.requires_pull_request {
            // M6: Contents creates and updates both return 409 under this rule.
            return Ok(reply(
                409,
                json!({"message":"Repository rule violations found\n\nChanges must be made through a pull request.\n\n"}),
            ));
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
        if store.remove_before_mutation {
            store.remove_before_mutation = false;
            store.files.remove(&path);
            store.commit();
        }
        let current = store.files.get(&path).map(|b| blob(b));
        let expected = body.get("sha").and_then(Value::as_str);
        if request.method == Method::Put && current.is_some() && expected.is_none() {
            // M2: the missing-SHA validation error is HTTP 422, even without a race.
            return Ok(reply(422, json!({"message":"\"sha\" wasn't supplied."})));
        }
        if request.method == Method::Delete && current.is_none() {
            return Ok(reply(404, json!({"message":"Not Found"})));
        }
        // M1: PUT ignores a supplied SHA if the path has disappeared.
        if current.is_some() && current.as_deref() != expected {
            return Ok(conflict(store.loser, "does not match"));
        }
        let content = if request.method == Method::Put {
            let bytes = STANDARD.decode(body["content"].as_str().unwrap()).unwrap();
            if bytes.len() > MAX_BYTES {
                return Ok(reply(422, json!({"message":"file too large"})));
            }
            let sha = blob(&bytes);
            store.files.insert(path.clone(), bytes);
            json!({"sha": if store.wrong_blob_sha {blob(b"different acknowledgement")} else {sha}})
        } else {
            assert_eq!(request.method, Method::Delete);
            assert!(expected.is_some());
            store.files.remove(&path);
            Value::Null
        };
        let parent = format!("{:040x}", store.head);
        let commit = store.commit();
        if store.two_winners {
            // Our acknowledged commit exists with the exact requested bytes,
            // but the branch selects a competing child of the same parent.
            store.head = u64::from_str_radix(&parent, 16).unwrap();
            store.files = store.history[&parent].clone();
            store.files.insert(path.clone(), b"another writer".to_vec());
            store.commit();
        }
        if let Some(bytes) = store.after_success.take() {
            store.after_success_pending = Some((path, bytes));
        }
        if store.malformed_success {
            return Ok(reply(200, json!({})));
        }
        if std::mem::take(&mut store.fail_read_after_commit) {
            store.response = Some(reply(503, json!({"message":"verification unavailable"})));
        }
        if let Some(error) = store.after_commit_error.take() {
            return Err(error);
        }
        if let Some(response) = store.after_commit_response.take() {
            return Ok(response);
        }
        Ok(reply(
            if current.is_none() { 201 } else { 200 },
            json!({"content":content,"commit":{"sha":commit,"parents":[{"sha":parent}]}}),
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
        Err(BackendError::GitHub(GitHubError::Credentials(_)))
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
        assert!(matches!(err, BackendError::Locked { .. }), "{err:?}");
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

#[tokio::test(start_paused = true)]
async fn github_rate_limits_carry_retry_hints_and_cleanup_deadlines_are_bounded() {
    for (status, header, message) in [
        (403, "retry-after", "slow"),
        (429, "retry-after", "slow"),
        (403, "x-ratelimit-remaining", "API rate limit exceeded"),
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
    fake.0.lock().unwrap().delay = Some(Duration::from_secs(10));
    let backend = fake.published_backend();
    let budget = backend.cleanup_budget();
    assert_eq!(budget.state_flush(), Duration::from_secs(5));
    assert_eq!(budget.total(), Duration::from_secs(8));
    // Decision B1 intentionally exceeds GitHub Actions' 2.5s SIGTERM-to-SIGKILL window.
    assert!(budget.total() > Duration::from_millis(2500));
    for release in [false, true] {
        let start = Instant::now();
        let result = if release {
            backend
                .release_lock_for_cleanup(&LockInfo::for_test("apply"))
                .await
        } else {
            backend.write_state_for_cleanup(&StateFile::new()).await
        };
        assert!(matches!(
            result,
            Err(BackendError::GitHub(GitHubError::Deadline))
        ));
        let bound = if release {
            budget.total() - budget.state_flush()
        } else {
            budget.state_flush() - Duration::from_millis(500)
        };
        assert_eq!(start.elapsed(), bound);
    }
}

#[tokio::test(start_paused = true)]
async fn github_cleanup_release_after_flush_cutoff_leaves_supervisor_margin() {
    let fake = Fake::default();
    let backend = fake.backend();
    let lock = backend.acquire_lock("apply").await.unwrap();
    let (trigger, shutdown) = carina_core::shutdown::testing::shutdown_channel();
    let backend = crate::PublishedBackend::for_test(Arc::new(backend), &shutdown);
    let budget = backend.cleanup_budget();
    trigger.prioritize_cleanup();
    let start = Instant::now();

    // An unresponsive flush consumes its entire cutoff before release starts.
    tokio::select! {
        _ = std::future::pending::<()>() => unreachable!(),
        _ = shutdown.state_flush_budget_expired(budget) => {}
    }
    assert_eq!(start.elapsed(), budget.state_flush());
    fake.0.lock().unwrap().delay = Some(Duration::from_secs(1));
    let result = backend.release_lock_for_cleanup(&lock).await;
    assert_eq!(start.elapsed(), budget.total() - CLEANUP_OPERATION_MARGIN);
    assert!(
        matches!(
            result,
            Err(BackendError::GitHub(GitHubError::MutationDeadline))
        ),
        "{result:?}"
    );
    assert!(start.elapsed() < budget.total());
    // DELETE committed at 7s; compare would only complete at the 8s hard exit.
    assert!(fake.bytes("state.json.lock").is_none());
}

#[tokio::test(start_paused = true)]
async fn github_late_cleanup_saves_observe_absolute_supervisor_margin() {
    for locked in [false, true] {
        for before_save in [
            Duration::from_secs(4),
            Duration::from_secs(5),
            Duration::from_millis(7500),
            Duration::from_millis(8100),
        ] {
            let fake = Fake::default();
            let backend = fake.backend();
            backend.read_state().await.unwrap();
            let lock = backend.acquire_lock("apply").await.unwrap();
            let (trigger, shutdown) = carina_core::shutdown::testing::shutdown_channel();
            let backend = crate::PublishedBackend::for_test(Arc::new(backend), &shutdown);
            trigger.prioritize_cleanup();
            tokio::time::advance(before_save).await;
            fake.0.lock().unwrap().delay = Some(Duration::from_secs(10));
            let start = Instant::now();
            let result = if locked {
                backend
                    .write_state_locked_for_cleanup(&StateFile::new(), &lock)
                    .await
            } else {
                backend.write_state_for_cleanup(&StateFile::new()).await
            };
            let remaining = (backend.cleanup_budget().total() - CLEANUP_OPERATION_MARGIN)
                .saturating_sub(before_save);
            assert_eq!(
                start.elapsed(),
                remaining,
                "locked={locked}, before_save={before_save:?}"
            );
            if locked || remaining.is_zero() {
                assert!(
                    matches!(result, Err(BackendError::GitHub(GitHubError::Deadline))),
                    "{result:?}"
                );
            } else {
                assert!(
                    matches!(
                        result,
                        Err(BackendError::GitHub(GitHubError::MutationDeadline))
                    ),
                    "{result:?}"
                );
            }
            assert!(fake.bytes("state.json").is_none());
        }
    }
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
    let backend = fake.published_backend();
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
        let backend = crate::PublishedBackend::for_test(
            backend,
            &carina_core::shutdown::ShutdownToken::running(),
        );
        let mut latest = StateFile::new();
        latest.serial = 2;
        if old_first {
            transport.resume.notify_one();
            assert_eq!(receiver.await.unwrap().status, 200);
            // The abandoned PUT's exact bytes are ours under decision (b)'s
            // current policy. Cleanup resolves that intent before its save.
            backend.write_state_for_cleanup(&latest).await.unwrap();
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
                error: reqwest::Client::new()
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

#[tokio::test(start_paused = true)]
async fn github_cleanup_protocol_fits_the_flush_budget_with_fifty_ms_transport_latency() {
    let fake = Fake::default();
    fake.put("state.json", serde_json::to_vec(&StateFile::new()).unwrap());
    let backend = fake.published_backend();
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
async fn github_round2_acquire_compare_accepts_a_later_client_commit() {
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
    assert_compare_only(&fake);
}

#[tokio::test]
async fn github_round2_release_compare_accepts_a_later_acquisition() {
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
    assert_compare_only(&fake);
}

#[tokio::test]
async fn github_round2_state_compare_accepts_a_later_state_commit() {
    let fake = Fake::default();
    let later = StateFile::new();
    let later_bytes = serde_json::to_vec(&later).unwrap();
    fake.0.lock().unwrap().after_success = Some(later_bytes.clone());
    let backend = fake.backend();
    backend.write_state(&StateFile::new()).await.unwrap();
    assert_eq!(fake.bytes("state.json").unwrap(), later_bytes);
    assert_compare_only(&fake);
    assert!(matches!(
        backend.write_state(&StateFile::new()).await,
        Err(BackendError::StateWriteConflict { .. })
    ));
}

fn assert_compare_only(fake: &Fake) {
    let store = fake.0.lock().unwrap();
    let mutation = store
        .requests
        .iter()
        .rposition(|r| r.method != Method::Get)
        .unwrap();
    let reads = &store.requests[mutation + 1..];
    assert_eq!(
        reads.len(),
        1,
        "one reachability request regardless of content size"
    );
    assert_eq!(reads[0].method, Method::Get);
    assert_eq!(reads[0].accept, JSON_MEDIA_TYPE);
    assert!(reads[0].reference.is_none());
    let comparison = reads[0]
        .path
        .strip_prefix("/repos/owner/repo/compare/")
        .expect("compare route");
    let commit = comparison.strip_suffix("...states%2Fprod").unwrap();
    assert_eq!(commit.len(), 40);
    assert!(store.history.contains_key(commit));
}

#[tokio::test]
async fn github_round2_compare_costs_one_request_for_small_and_large_files() {
    for size in [1_000_000, 1_000_001] {
        let fake = Fake::default();
        let mut state = StateFile::new();
        state.carina_version.clear();
        let overhead = carina_core::utils::pretty_with_newline_bytes(&state)
            .unwrap()
            .len();
        state.carina_version = "x".repeat(size - overhead);
        fake.backend().write_state(&state).await.unwrap();
        assert_compare_only(&fake);
    }
    let fake = Fake::default();
    fake.backend().write_state(&StateFile::new()).await.unwrap();
    assert_compare_only(&fake);
}

#[tokio::test]
async fn github_cleanup_inline_lease_save_and_release_each_use_three_requests() {
    let fake = Fake::default();
    let backend = fake.published_backend();
    backend.read_state().await.unwrap();
    let lock = backend.acquire_lock("apply").await.unwrap();
    fake.0.lock().unwrap().requests.clear();
    backend
        .write_state_locked_for_cleanup(&StateFile::new(), &lock)
        .await
        .unwrap();
    {
        let store = fake.0.lock().unwrap();
        assert_eq!(store.requests.len(), 3);
        assert!(store.requests[0].path.ends_with("state.json.lock"));
        assert_eq!(
            store.requests[0].reference.as_deref(),
            Some("refs/heads/states/prod")
        );
        assert_eq!(
            store
                .requests
                .iter()
                .filter(|r| r.method == Method::Put)
                .count(),
            1
        );
        assert!(store.requests[1].path.ends_with("state.json"));
    }
    assert_compare_only(&fake);
    fake.0.lock().unwrap().requests.clear();
    backend.release_lock_for_cleanup(&lock).await.unwrap();
    assert_eq!(fake.0.lock().unwrap().requests.len(), 3);
    assert_compare_only(&fake);
}

#[tokio::test]
async fn github_cleanup_split_lease_reads_stay_at_one_immutable_commit() {
    struct SplitLease(Fake);
    #[async_trait]
    impl GitHubTransport for SplitLease {
        async fn execute(
            &self,
            request: GitHubRequest,
            deadline: Instant,
        ) -> Result<GitHubResponse, GitHubError> {
            let lease_metadata = request.method == Method::Get
                && request.path.ends_with("state.json.lock")
                && request.accept == JSON_MEDIA_TYPE;
            let mut response = self.0.execute(request, deadline).await?;
            if lease_metadata && response.status == 200 {
                let mut metadata: Value = serde_json::from_slice(&response.body).unwrap();
                metadata["encoding"] = json!("none");
                metadata["content"] = json!("");
                response.body = serde_json::to_vec(&metadata).unwrap();
            }
            Ok(response)
        }
    }
    let fake = Fake::default();
    let lock = LockInfo::new("apply");
    fake.put("state.json.lock", serde_json::to_vec(&lock).unwrap());
    let backend = GitHubBackend::with_transport(settings(), Arc::new(SplitLease(fake.clone())));
    let backend = crate::PublishedBackend::for_test(
        Arc::new(backend),
        &carina_core::shutdown::ShutdownToken::running(),
    );
    backend.read_state().await.unwrap();
    for release in [false, true] {
        fake.0.lock().unwrap().requests.clear();
        if release {
            backend.release_lock_for_cleanup(&lock).await.unwrap();
        } else {
            backend
                .write_state_locked_for_cleanup(&StateFile::new(), &lock)
                .await
                .unwrap();
        }
        let store = fake.0.lock().unwrap();
        assert_eq!(store.requests.len(), 6);
        assert_eq!(
            store.requests[0].reference.as_deref(),
            Some("refs/heads/states/prod")
        );
        assert!(store.requests[1].path.contains("/branches/"));
        assert_eq!(store.requests[2].reference, store.requests[3].reference);
        assert_ne!(store.requests[2].reference, store.requests[0].reference);
        assert_eq!(store.requests[3].accept, RAW_MEDIA_TYPE);
        assert!(
            store.requests[4]
                .body
                .as_ref()
                .unwrap()
                .get("sha")
                .is_some()
                || !release
        );
        assert!(store.requests[5].path.contains("/compare/"));
    }
}

#[tokio::test]
async fn github_round2_expired_takeover_accepts_created_when_the_old_lease_disappears() {
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
    assert_eq!(puts.len(), 1);
    assert!(puts[0].body.as_ref().unwrap().get("sha").is_some());
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
        let backend = fake.published_backend();
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
    for existing in [false, true] {
        let fake = Fake::default();
        if existing {
            let expired = LockInfo::with_timeout("expired", -1);
            fake.put("state.json.lock", serde_json::to_vec(&expired).unwrap());
        }
        let transport = Arc::new(RateOnEachRequest {
            fake,
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
            // An absent lease needs a root proof, which exhausts the shared
            // retry budget before dispatch. An existing lease reaches the
            // mutation and two reconciliation reads, with no fresh budget.
            if existing { 7 } else { 5 }
        );
        assert_eq!(start.elapsed(), Duration::from_secs(2));
    }
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
async fn github_round2_cleanup_request_counts_cover_cold_and_cached_small_and_large_saves() {
    for cached in [false, true] {
        for locked in [false, true] {
            for large in [false, true] {
                let fake = Fake::default();
                let mut previous = StateFile::new();
                previous.carina_version = "x".repeat(1_000_001);
                let backend = fake.published_backend();
                if cached {
                    fake.put("state.json", serde_json::to_vec(&previous).unwrap());
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
                // Cold saves only create absent state, including one root
                // Contents read proving that the absence is visible to this token.
                let expected = 2 + usize::from(locked) + usize::from(!cached) * 3;
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
                    0,
                    "neither reachability nor the old state precondition needs raw bytes"
                );
            }
        }
    }
}

#[tokio::test]
async fn github_round1_cleanup_never_saves_with_an_expired_or_replaced_lease() {
    for expired in [false, true] {
        let fake = Fake::default();
        let backend = fake.published_backend();
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

#[tokio::test]
async fn github_round2_success_caches_sha_before_a_failed_verification() {
    let fake = Fake::default();
    let backend = fake.backend();
    let mut state = StateFile::new();
    backend.write_state(&state).await.unwrap();
    fake.0.lock().unwrap().fail_read_after_commit = true;
    state.serial = 1;
    assert!(matches!(
        backend.write_state(&state).await,
        Err(BackendError::GitHub(GitHubError::Api { status: 503, .. }))
    ));
    state.serial = 2;
    backend.write_state(&state).await.unwrap();
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

#[tokio::test(start_paused = true)]
async fn github_round2_committed_rate_limit_retry_conflict_is_already_applied() {
    let fake = Fake::default();
    let backend = fake.backend();
    let mut state = StateFile::new();
    backend.write_state(&state).await.unwrap();
    fake.0.lock().unwrap().requests.clear();
    fake.0.lock().unwrap().after_commit_response = Some(rate_limit(429, "retry-after", "0".into()));
    state.serial = 1;
    backend.write_state(&state).await.unwrap();
    assert_eq!(
        fake.0
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.method == Method::Put)
            .count(),
        2
    );
    state.serial = 2;
    backend.write_state(&state).await.unwrap();
}

#[tokio::test]
async fn github_round2_lost_acquire_response_recovers_our_exact_lease() {
    let error = GitHubError::Transport {
        kind: "lost response",
        error: reqwest::Client::new()
            .get("http://[invalid")
            .build()
            .unwrap_err(),
    };
    let fake = Fake::default();
    fake.0.lock().unwrap().after_commit_error = Some(error);
    let backend = fake.backend();
    let acquired = backend.acquire_lock("apply").await.unwrap();
    assert_eq!(
        fake.bytes("state.json.lock").unwrap(),
        serde_json::to_vec(&acquired).unwrap()
    );
    backend.release_lock(&acquired).await.unwrap();
}

#[tokio::test]
async fn github_round2_committed_rate_limit_without_a_retry_hint_recovers_state() {
    let fake = Fake::default();
    let backend = fake.backend();
    backend.read_state().await.unwrap();
    fake.0.lock().unwrap().after_commit_response =
        Some(reply(429, json!({"message":"rate limited"})));
    let mut state = StateFile::new();
    backend.write_state(&state).await.unwrap();
    state.serial = 1;
    backend.write_state(&state).await.unwrap();
}

#[tokio::test]
async fn github_round2_compare_rejects_a_detached_acknowledgement_with_matching_bytes() {
    let fake = Fake::default();
    fake.0.lock().unwrap().two_winners = true;
    assert!(matches!(
        fake.backend().write_state(&StateFile::new()).await,
        Err(BackendError::GitHub(GitHubError::UnexpectedOutcome { .. }))
    ));
}

#[tokio::test]
async fn github_round2_compare_rejects_non_reachable_statuses() {
    for (status, behind) in [
        ("behind", 1),
        ("diverged", 1),
        ("ahead", 1),
        ("identical", 1),
        ("unknown", 0),
    ] {
        let fake = Fake::default();
        fake.0.lock().unwrap().compare_response =
            Some(json!({"status":status,"ahead_by":1,"behind_by":behind}));
        assert!(
            matches!(
                fake.backend().write_state(&StateFile::new()).await,
                Err(BackendError::GitHub(GitHubError::UnexpectedOutcome { .. }))
            ),
            "{status}, behind={behind}"
        );
    }
}

#[tokio::test]
async fn github_round2_deleted_lease_renewal_is_rejected_and_its_resurrection_removed() {
    let fake = Fake::default();
    let backend = fake.backend();
    let lock = backend.acquire_lock("apply").await.unwrap();
    fake.0.lock().unwrap().requests.clear();
    fake.0.lock().unwrap().remove_before_mutation = true;
    assert!(
        matches!(backend.renew_lock(&lock).await, Err(BackendError::LockNotHeld(id)) if id == lock.id())
    );
    assert!(fake.bytes("state.json.lock").is_none());
    let store = fake.0.lock().unwrap();
    let deletion = store
        .requests
        .iter()
        .find(|r| r.method == Method::Delete)
        .expect("resurrected lease cleanup");
    assert!(deletion.body.as_ref().unwrap().get("sha").is_some());
    assert!(store.requests.iter().any(|r| r.path.contains("/compare/")));
}

#[tokio::test]
async fn github_round2_deleted_state_update_reports_conflict_and_keeps_created_state() {
    let fake = Fake::default();
    let backend = fake.backend();
    let mut state = StateFile::new();
    backend.write_state(&state).await.unwrap();
    fake.0.lock().unwrap().remove_before_mutation = true;
    state.serial = 1;
    let error = backend.write_state(&state).await.unwrap_err();
    assert!(matches!(
        &error,
        BackendError::GitHub(GitHubError::StateDeletedConcurrently)
    ));
    assert!(
        error
            .to_string()
            .contains("state file had been deleted concurrently"),
        "{error:?}"
    );
    assert_eq!(
        fake.bytes("state.json").unwrap(),
        carina_core::utils::pretty_with_newline_bytes(&state).unwrap()
    );
    state.serial = 2;
    backend.write_state(&state).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn github_round2_fake_matches_deleted_sha_create_and_existing_missing_sha_validation() {
    let fake = Fake::default();
    fake.put("state.json", b"old".to_vec());
    let request = |sha: Option<&str>| GitHubRequest {
        method: Method::Put,
        path: "/repos/owner/repo/contents/state.json".into(),
        reference: None,
        accept: JSON_MEDIA_TYPE,
        body_limit: BodyLimit::Contents,
        body: Some({
            let mut body = json!({"message":"carina: test","branch":"states/prod","content": STANDARD.encode(b"new")});
            if let Some(sha) = sha {
                body["sha"] = json!(sha);
            }
            body
        }),
    };
    let deadline = Instant::now() + Duration::from_secs(1);
    let response = fake.execute(request(None), deadline).await.unwrap();
    assert_eq!(response.status, 422);
    assert_eq!(
        serde_json::from_slice::<Value>(&response.body).unwrap()["message"],
        "\"sha\" wasn't supplied."
    );
    fake.0.lock().unwrap().remove_before_mutation = true;
    assert_eq!(
        fake.execute(request(Some(&blob(b"old"))), deadline)
            .await
            .unwrap()
            .status,
        201
    );
    assert_eq!(fake.bytes("state.json").unwrap(), b"new");
}

#[tokio::test]
async fn github_round2_locked_saves_require_more_than_sixty_seconds_plus_skew() {
    for cleanup in [false, true] {
        // The 5-second skew allowance is part of the documented ownership margin.
        for (ttl, allowed) in [(1, false), (60, false), (65, false), (66, true)] {
            let fake = Fake::default();
            let backend = fake.published_backend();
            backend.read_state().await.unwrap();
            let lock = LockInfo::with_timeout("apply", ttl);
            fake.put("state.json.lock", serde_json::to_vec(&lock).unwrap());
            fake.0.lock().unwrap().requests.clear();
            let result = if cleanup {
                backend
                    .write_state_locked_for_cleanup(&StateFile::new(), &lock)
                    .await
            } else {
                backend.write_state_locked(&StateFile::new(), &lock).await
            };
            if allowed {
                result.unwrap();
            } else {
                assert!(
                    matches!(result, Err(BackendError::LockNotHeld(_))),
                    "cleanup={cleanup}, ttl={ttl}: {result:?}"
                );
                assert!(
                    fake.0
                        .lock()
                        .unwrap()
                        .requests
                        .iter()
                        .all(|r| r.method == Method::Get)
                );
            }
        }
    }
}

#[tokio::test]
async fn github_round2_forbidden_with_zero_remaining_requires_a_rate_limit_message() {
    let fake = Fake::default();
    let mut response = reply(
        403,
        json!({"message":"Resource not accessible by integration"}),
    );
    response
        .headers
        .insert("x-ratelimit-remaining".into(), "0".into());
    fake.0.lock().unwrap().response = Some(response);
    let error = fake.backend().storage_readiness().await.unwrap_err();
    assert!(
        matches!(
            &error,
            BackendError::GitHub(GitHubError::Api { status: 403, .. })
        ),
        "{error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("Resource not accessible by integration")
    );
}

#[tokio::test]
async fn github_round2_diagnostics_redact_queries_in_every_response_url_field() {
    capture_backend_logs();
    for status in [403, 422, 429, 500, 502, 503, 504, 409] {
        let fake = Fake::default();
        let url = "https://raw.githubusercontent.com/owner/repo/state.json?token=github-issued-raw-secret&other=private";
        let message = if status == 409 {
            format!("Repository rule violations found; denied at {url}")
        } else {
            format!("denied at {url}")
        };
        let mut response = reply(
            status,
            json!({
                "message":message, "download_url": url, "git_url":url,
                "url":url,"other":{"arbitrary":[url,"https://example.com/path?unrelated=also-private"]}
            }),
        );
        response
            .headers
            .insert("x-github-request-id".into(), url.into());
        let error = if status == 409 {
            fake.0.lock().unwrap().mutation_response = Some(response);
            fake.backend()
                .write_state(&StateFile::new())
                .await
                .unwrap_err()
        } else {
            fake.0.lock().unwrap().response = Some(response);
            fake.backend().storage_readiness().await.unwrap_err()
        };
        assert_safe_response_diagnostics(&error, &["github-issued-raw-secret", "also-private"]);
        if status == 409 {
            assert!(matches!(
                &error,
                BackendError::GitHub(GitHubError::RuleViolation { .. })
            ));
        }
        if status != 429 {
            let rendered = error.to_string();
            assert!(
                rendered
                    .contains("denied at https://raw.githubusercontent.com/owner/repo/state.json")
            );
        }
    }

    // Exercise the backend's warning as well as otherwise silent API failures.
    struct RejectCleanupDelete(Fake);
    #[async_trait]
    impl GitHubTransport for RejectCleanupDelete {
        async fn execute(
            &self,
            request: GitHubRequest,
            deadline: Instant,
        ) -> Result<GitHubResponse, GitHubError> {
            if request.method == Method::Delete {
                return Ok(reply(
                    503,
                    json!({
                        "message": "cleanup failed",
                        "download_url": "https://raw.githubusercontent.com/owner/repo/state.json?token=cleanup-raw-secret",
                    }),
                ));
            }
            self.0.execute(request, deadline).await
        }
    }
    let fake = Fake::default();
    let backend =
        GitHubBackend::with_transport(settings(), Arc::new(RejectCleanupDelete(fake.clone())));
    let lock = backend.acquire_lock("apply").await.unwrap();
    fake.0.lock().unwrap().remove_before_mutation = true;
    let error = backend.renew_lock(&lock).await.unwrap_err();
    assert!(matches!(&error, BackendError::LockNotHeld(_)));
    assert!(BACKEND_LOGS.0.lock().unwrap().iter().any(|line| {
        line.contains("GitHub recreated lease cleanup could not be confirmed")
            && line.matches("cleanup failed").count() == 1
            && line.contains("HTTP 503")
    }));
    assert_safe_response_diagnostics(&error, &["cleanup-raw-secret"]);
}

#[tokio::test]
async fn github_round2_acknowledged_blob_sha_must_match_the_exact_sent_bytes() {
    let fake = Fake::default();
    let backend = fake.backend();
    fake.0.lock().unwrap().wrong_blob_sha = true;
    assert!(matches!(
        backend.write_state(&StateFile::new()).await,
        Err(BackendError::GitHub(GitHubError::UnexpectedOutcome { .. }))
    ));
    fake.0.lock().unwrap().wrong_blob_sha = false;
    fake.0.lock().unwrap().requests.clear();
    let mut state = StateFile::new();
    state.serial = 1;
    backend.write_state(&state).await.unwrap();
    assert!(
        fake.0.lock().unwrap().requests.iter().any(|request| {
            request.method == Method::Get && request.path.ends_with("/contents/state.json")
        }),
        "a rejected acknowledgement must force a fresh precondition read"
    );
}

#[tokio::test]
async fn github_round2_known_created_renewal_still_cleans_up_if_compare_fails() {
    let fake = Fake::default();
    let backend = fake.backend();
    let lock = backend.acquire_lock("apply").await.unwrap();
    {
        let mut store = fake.0.lock().unwrap();
        store.remove_before_mutation = true;
        store.fail_read_after_commit = true;
    }
    assert!(matches!(
        backend.renew_lock(&lock).await,
        Err(BackendError::LockNotHeld(_))
    ));
    assert!(fake.bytes("state.json.lock").is_none());
}

#[tokio::test]
async fn github_round2_resurrection_cleanup_preserves_a_successor_lease() {
    let fake = Fake::default();
    let backend = fake.backend();
    let lock = backend.acquire_lock("apply").await.unwrap();
    let successor = LockInfo::new("successor");
    {
        let mut store = fake.0.lock().unwrap();
        store.remove_before_mutation = true;
        store.after_success = Some(serde_json::to_vec(&successor).unwrap());
    }
    assert!(matches!(
        backend.renew_lock(&lock).await,
        Err(BackendError::LockNotHeld(_))
    ));
    assert_eq!(
        fake.bytes("state.json.lock").unwrap(),
        serde_json::to_vec(&successor).unwrap()
    );
}

#[tokio::test(start_paused = true)]
async fn github_round2_committed_acquire_rate_retry_recovers_after_missing_sha_422() {
    let fake = Fake::default();
    fake.0.lock().unwrap().after_commit_response = Some(rate_limit(429, "retry-after", "0".into()));
    let acquired = fake.backend().acquire_lock("apply").await.unwrap();
    assert_eq!(
        fake.bytes("state.json.lock").unwrap(),
        serde_json::to_vec(&acquired).unwrap()
    );
}

#[tokio::test]
async fn github_round2_known_created_state_conflict_survives_compare_failure() {
    let fake = Fake::default();
    let backend = fake.backend();
    let mut state = StateFile::new();
    backend.write_state(&state).await.unwrap();
    {
        let mut store = fake.0.lock().unwrap();
        store.remove_before_mutation = true;
        store.fail_read_after_commit = true;
    }
    state.serial = 1;
    let error = backend.write_state(&state).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("state file had been deleted concurrently"),
        "{error:?}"
    );
    assert_eq!(
        fake.bytes("state.json").unwrap(),
        carina_core::utils::pretty_with_newline_bytes(&state).unwrap()
    );
}

#[tokio::test]
async fn github_round3_compare_reads_only_the_three_reachability_fields() {
    let fake = Fake::default();
    fake.0.lock().unwrap().compare_response = Some(json!({
        "status":"identical", "ahead_by":0, "behind_by":0,
        "base_commit":{"sha":format!("{:040x}", 2), "parents":[{"sha":format!("{:040x}", 99)}]}
    }));
    fake.backend().write_state(&StateFile::new()).await.unwrap();
}

#[tokio::test]
async fn github_round2_unstructured_errors_and_request_ids_redact_url_queries() {
    capture_backend_logs();
    for body in [
        b"denied at https://example.com/state?token=raw-token".to_vec(),
        br#"<a href="https://example.com/state?token=raw-token">denied</a>"#.to_vec(),
        br#"{"broken":"https:\/\/example.com\/state?token=raw-token"#.to_vec(),
        br#"{"message":"https:\/\/example.com\/state?\u0074oken=raw-token"}"#.to_vec(),
    ] {
        let fake = Fake::default();
        fake.0.lock().unwrap().response = Some(GitHubResponse {
            status: 503,
            headers: BTreeMap::from([(
                "x-github-request-id".into(),
                "https://example.com/id?token=header-token".into(),
            )]),
            body,
        });
        let error = fake.backend().storage_readiness().await.unwrap_err();
        assert_safe_response_diagnostics(&error, &["raw-token", "header-token"]);
        assert!(error.to_string().contains("https://example.com/state"));
    }
}

// The backend uses log::warn! for recreated-lease cleanup failures and emits
// no tracing events. Capture its log facade at the emission boundary.
struct BackendLogs(Mutex<Vec<String>>);
static BACKEND_LOGS: BackendLogs = BackendLogs(Mutex::new(Vec::new()));
static BACKEND_LOG_INIT: std::sync::Once = std::sync::Once::new();

impl log::Log for BackendLogs {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata
            .target()
            .starts_with("carina_state::backends::github")
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            self.0.lock().unwrap().push(record.args().to_string());
        }
    }

    fn flush(&self) {}
}

fn capture_backend_logs() {
    BACKEND_LOG_INIT.call_once(|| {
        log::set_logger(&BACKEND_LOGS).unwrap();
        log::set_max_level(log::LevelFilter::Trace);
    });
    BACKEND_LOGS.0.lock().unwrap().clear();
}

fn assert_safe_response_diagnostics(error: &BackendError, secrets: &[&str]) {
    let mut diagnostics = vec![error.to_string(), format!("{error:?}")];
    if let BackendError::GitHub(inner) = error {
        diagnostics.extend([inner.to_string(), format!("{inner:?}")]);
    }
    diagnostics.extend(BACKEND_LOGS.0.lock().unwrap().iter().cloned());
    for diagnostic in diagnostics {
        for secret in std::iter::once("token=").chain(secrets.iter().copied()) {
            assert!(!diagnostic.contains(secret), "{diagnostic}");
        }
    }
}

#[tokio::test]
async fn github_round4_mismatched_ack_never_adopts_a_third_writers_state() {
    for replacing in [false, true] {
        let fake = Fake::default();
        let backend = fake.backend();
        if replacing {
            fake.put("state.json", serde_json::to_vec(&StateFile::new()).unwrap());
        }
        backend.read_state().await.unwrap();
        fake.0.lock().unwrap().wrong_blob_sha = true;
        let mut state = StateFile::new();
        state.serial = 1;
        assert!(matches!(
            backend.write_state(&state).await,
            Err(BackendError::GitHub(GitHubError::UnexpectedOutcome { .. }))
        ));
        fake.0.lock().unwrap().wrong_blob_sha = false;
        let mut third = StateFile::new();
        third.serial = 3;
        let third_bytes = serde_json::to_vec(&third).unwrap();
        fake.put("state.json", third_bytes.clone());
        fake.0.lock().unwrap().requests.clear();
        for _ in 0..2 {
            state.serial = 2;
            assert!(matches!(
                backend.write_state(&state).await,
                Err(BackendError::StateWriteConflict { .. })
            ));
        }
        assert_eq!(fake.bytes("state.json").unwrap(), third_bytes);
        assert!(
            fake.0
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|r| r.method == Method::Get)
        );
    }
}

#[tokio::test]
async fn github_round4_unread_state_cannot_be_adopted_by_a_write_or_raw_export() {
    for raw_export in [false, true] {
        let fake = Fake::default();
        let backend = fake.backend();
        let state = StateFile::new();
        fake.put("state.json", serde_json::to_vec(&state).unwrap());
        if raw_export {
            backend.read_state_bytes().await.unwrap();
        }
        let mut third = StateFile::new();
        third.serial = 3;
        let third_bytes = serde_json::to_vec(&third).unwrap();
        fake.put("state.json", third_bytes.clone());
        fake.0.lock().unwrap().requests.clear();
        assert!(matches!(
            backend.write_state(&state).await,
            Err(BackendError::StateWriteConflict { .. })
        ));
        assert_eq!(fake.bytes("state.json").unwrap(), third_bytes);
        assert!(
            fake.0
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|r| r.method == Method::Get)
        );
        backend.read_state().await.unwrap();
        backend.write_state(&state).await.unwrap();
    }
}

fn assert_located_message(error: &BackendError, fragments: &[&str]) {
    assert!(matches!(error, BackendError::GitHub(_)), "{error:?}");
    let message = error.to_string();
    assert!(message.contains("owner/repo@states/prod"), "{message}");
    for fragment in fragments {
        assert!(
            message.contains(fragment),
            "missing {fragment:?}: {message}"
        );
    }
}

#[tokio::test]
async fn github_round4_repository_missing_or_inaccessible_has_actionable_location() {
    let fake = Fake::default();
    fake.0.lock().unwrap().missing_repo = true;
    let StorageReadiness::Unavailable(unavailable) =
        fake.backend().storage_readiness().await.unwrap()
    else {
        panic!("missing repository");
    };
    let error = unavailable.into_error();
    assert!(matches!(
        error,
        BackendError::GitHub(GitHubError::RepositoryNotFoundOrInaccessible { .. })
    ));
    assert_located_message(
        &error,
        &[
            "missing",
            "token",
            "access",
            "Contents",
            "fine-grained",
            "repository selection",
        ],
    );
}

#[tokio::test]
async fn github_round4_branch_missing_has_actionable_location() {
    let fake = Fake::default();
    fake.0.lock().unwrap().missing_branch = true;
    let error = fake.backend().read_state().await.unwrap_err();
    assert!(matches!(
        error,
        BackendError::GitHub(GitHubError::BranchNotFound { .. })
    ));
    assert_located_message(&error, &["branch", "not found"]);
}

#[tokio::test]
async fn github_round4_archived_or_disabled_repository_has_actionable_location() {
    for flag in ["archived", "disabled"] {
        let fake = Fake::default();
        fake.0.lock().unwrap().response = Some(reply(200, json!({"id":1,flag:true})));
        let StorageReadiness::Unavailable(unavailable) =
            fake.backend().storage_readiness().await.unwrap()
        else {
            panic!("inactive repository");
        };
        let error = unavailable.into_error();
        assert!(matches!(
            error,
            BackendError::GitHub(GitHubError::RepositoryArchivedOrDisabled { .. })
        ));
        assert_located_message(&error, &["archived", "disabled"]);
    }
}

#[tokio::test]
async fn github_round4_unauthorized_names_both_credential_sources() {
    let fake = Fake::default();
    fake.0.lock().unwrap().response = Some(reply(401, json!({"message":"Bad credentials"})));
    let error = fake.backend().read_state().await.unwrap_err();
    assert!(matches!(
        error,
        BackendError::GitHub(GitHubError::Credentials(_))
    ));
    let message = error.to_string();
    assert!(
        message.contains("GITHUB_TOKEN") && message.contains("gh auth login"),
        "{message}"
    );
}

#[tokio::test]
async fn github_round4_put_and_delete_access_errors_name_contents_write() {
    for method in [Method::Put, Method::Delete] {
        for status in [403, 404] {
            let fake = Fake::default();
            let backend = fake.backend();
            backend.read_state().await.unwrap();
            let lock = backend.acquire_lock("apply").await.unwrap();
            let mut denied = reply(
                status,
                json!({"message":"Resource not accessible by integration"}),
            );
            denied
                .headers
                .insert("x-ratelimit-remaining".into(), "0".into());
            fake.0.lock().unwrap().mutation_response = Some(denied);
            let error = if method == Method::Put {
                backend.write_state(&StateFile::new()).await.unwrap_err()
            } else {
                backend.release_lock(&lock).await.unwrap_err()
            };
            assert_located_message(&error, &["token", "Contents write"]);
            assert!(matches!(
                error,
                BackendError::GitHub(GitHubError::ContentsWriteDenied { .. })
            ));
            assert_eq!(
                fake.bytes("state.json.lock").unwrap(),
                serde_json::to_vec(&lock).unwrap()
            );
        }
    }
}

#[tokio::test]
async fn github_round4_delete_404_is_typed_for_release_and_force_unlock() {
    for force in [false, true] {
        let fake = Fake::default();
        let backend = fake.backend();
        let lock = backend.acquire_lock("apply").await.unwrap();
        fake.0.lock().unwrap().remove_before_mutation = true;
        let result = if force {
            backend.force_unlock(lock.id()).await
        } else {
            backend.release_lock(&lock).await
        };
        if force {
            assert!(
                matches!(result, Err(BackendError::LockNotFound(ref id)) if id == lock.id()),
                "{result:?}"
            );
        } else {
            result.unwrap();
        }
        assert!(fake.bytes("state.json.lock").is_none());
    }
}

#[tokio::test]
async fn github_round4_absent_lease_is_released_but_force_unlock_is_not_found() {
    let fake = Fake::default();
    let backend = fake.backend();
    let lock = LockInfo::new("apply");
    backend.release_lock(&lock).await.unwrap();
    assert!(matches!(backend.force_unlock(lock.id()).await,
        Err(BackendError::LockNotFound(ref id)) if id == lock.id()));
    assert!(
        fake.0
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|r| r.method == Method::Get)
    );
}

#[tokio::test(start_paused = true)]
async fn github_round4_deadline_wording_only_warns_after_mutation_dispatch() {
    let fake = Fake::default();
    let transport = LostReply::new(fake.clone(), Method::Put, true);
    let backend = GitHubBackend::with_transport(settings(), transport.clone());
    backend.read_state().await.unwrap();
    transport.arm();
    let message = backend
        .write_state(&StateFile::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("a dispatched mutation may have completed"),
        "{message}"
    );
    fake.0.lock().unwrap().delay = Some(Duration::from_secs(60));
    let message = backend.read_state().await.unwrap_err().to_string();
    assert!(message.contains("deadline"), "{message}");
    assert!(!message.contains("mutation"), "{message}");
    let message = fake
        .published_backend()
        .write_state_for_cleanup(&StateFile::new())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        !message.contains("mutation"),
        "no PUT dispatched: {message}"
    );
}

#[test]
fn github_round4_rate_limit_display_is_actionable_without_option_debug() {
    for retry_after in [
        None,
        Some(Duration::from_secs(60)),
        Some(Duration::from_millis(500)),
    ] {
        let message = GitHubError::RateLimited {
            status: 403,
            retry_after,
            request_id: None,
        }
        .to_string();
        assert!(message.contains("403"), "{message}");
        assert!(
            !message.contains("Some(") && !message.contains("None"),
            "{message}"
        );
        if let Some(hint) = retry_after {
            assert!(
                message.contains(&hint.as_secs_f64().to_string()),
                "{message}"
            );
        }
    }
}

#[tokio::test]
async fn github_round4_absence_requires_readable_root_at_the_same_commit() {
    for raw in [false, true] {
        let fake = Fake::default();
        let backend = fake.backend();
        if raw {
            assert!(backend.read_state_bytes().await.unwrap().is_none());
        } else {
            assert!(backend.read_state().await.unwrap().is_none());
        }
        let store = fake.0.lock().unwrap();
        assert_eq!(
            store.requests.len(),
            3,
            "branch, state metadata, root proof"
        );
        assert_eq!(store.requests[2].path, "/repos/owner/repo/contents");
        assert_eq!(store.requests[1].reference, store.requests[2].reference);
    }
}

#[tokio::test]
async fn github_round4_hidden_contents_fail_closed_without_a_state_put() {
    for contents_status in [403, 404] {
        for root_status in [301, 403, 404, 500, 502, 503, 504] {
            for writing in [false, true] {
                let fake = Fake::default();
                let backend = fake.backend();
                {
                    let mut store = fake.0.lock().unwrap();
                    store.contents_status = Some(contents_status);
                    store.root_status = Some(root_status);
                }
                let error = if writing {
                    backend.write_state(&StateFile::new()).await.unwrap_err()
                } else {
                    backend.read_state().await.unwrap_err()
                };
                if contents_status == 403 || matches!(root_status, 403 | 404) {
                    assert_located_message(&error, &["token", "Contents read"]);
                    assert!(matches!(
                        error,
                        BackendError::GitHub(GitHubError::ContentsReadDenied { .. })
                    ));
                } else {
                    assert!(
                        matches!(
                            error,
                            BackendError::GitHub(GitHubError::Api { status, .. }) if status == root_status
                        ),
                        "{error:?}"
                    );
                }
                let store = fake.0.lock().unwrap();
                assert!(store.requests.iter().all(|r| r.method == Method::Get));
                assert_eq!(
                    store
                        .requests
                        .iter()
                        .filter(|r| r.path == "/repos/owner/repo/contents")
                        .count(),
                    usize::from(contents_status == 404)
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn github_round5_rules_requiring_pull_requests_fail_without_retrying() {
    for operation in [
        "create state",
        "replace state",
        "create lease",
        "delete lease",
    ] {
        let fake = Fake::default();
        let backend = fake.backend();
        let state = StateFile::new();
        if operation == "replace state" {
            backend.write_state(&state).await.unwrap();
        }
        backend.read_state().await.unwrap();
        let lock = if operation == "delete lease" {
            Some(backend.acquire_lock("apply").await.unwrap())
        } else {
            None
        };
        let before = fake.0.lock().unwrap().files.clone();
        {
            let mut store = fake.0.lock().unwrap();
            store.requires_pull_request = true;
            store.requests.clear();
        }
        let error = match operation {
            "create lease" => backend.acquire_lock("apply").await.unwrap_err(),
            "delete lease" => backend
                .release_lock(lock.as_ref().unwrap())
                .await
                .unwrap_err(),
            _ => backend.write_state(&state).await.unwrap_err(),
        };
        let store = fake.0.lock().unwrap();
        assert_eq!(store.files, before);
        assert_eq!(
            store
                .requests
                .iter()
                .filter(|r| r.method != Method::Get)
                .count(),
            1,
            "{operation}: {error:?}"
        );
        assert!(
            store.requests.last().unwrap().method != Method::Get,
            "a rule rejection needs no reconciliation read"
        );
        assert_located_message(
            &error,
            &[
                "Repository rule violations found",
                "token must be allowed to push directly to the state branch",
            ],
        );
        assert!(
            matches!(&error,
            BackendError::GitHub(GitHubError::RuleViolation { location, message })
                if location == "owner/repo@states/prod"
                    && message.contains("Changes must be made through a pull request.")),
            "{error:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn github_round5_contention_keeps_the_last_sanitized_response_evidence() {
    capture_backend_logs();
    // Preserve the recorded 409 rule, including unrecognised 409 messages.
    for message in ["is at X but expected Y", "another 409 reason"] {
        let fake = Fake::default();
        let backend = fake.backend();
        backend.read_state().await.unwrap();
        let mut expected_body = String::new();
        let mut expected_request_id = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let mut response = reply(
                409,
                json!({
                    "message": format!("{message}; attempt {attempt}; https://example.invalid/details?token=body-secret"),
                    "documentation_url": "https://example.invalid/docs?token=docs-secret",
                    "download_url": "https://raw.githubusercontent.com/owner/repo/state.json?token=raw-secret",
                }),
            );
            response.headers.insert(
                "x-github-request-id".into(),
                format!("request-{attempt} https://example.invalid/id?token=id-secret"),
            );
            let BackendError::GitHub(GitHubError::Api {
                body, request_id, ..
            }) = GitHubBackend::api_error(&response)
            else {
                panic!("API evidence")
            };
            expected_body = body;
            expected_request_id = request_id;
            fake.0
                .lock()
                .unwrap()
                .mutation_responses
                .push_back(response);
        }
        let error = backend.write_state(&StateFile::new()).await.unwrap_err();
        assert!(
            matches!(&error, BackendError::GitHub(GitHubError::Contention {
            attempts: MAX_ATTEMPTS, message, request_id,
        }) if message == &expected_body && request_id == &expected_request_id),
            "{error:?}"
        );
        let display = error.to_string();
        assert!(display.contains(&expected_body), "{display}");
        assert!(
            display.contains(expected_request_id.as_ref().unwrap()),
            "{display}"
        );
        assert_safe_response_diagnostics(
            &error,
            &["body-secret", "docs-secret", "id-secret", "raw-secret"],
        );
        let store = fake.0.lock().unwrap();
        assert_eq!(
            store
                .requests
                .iter()
                .filter(|r| r.method == Method::Put)
                .count(),
            MAX_ATTEMPTS
        );
        assert!(store.files.is_empty());
    }
}

#[tokio::test]
async fn github_round5_committed_server_error_preserves_state_intent_for_the_next_save() {
    for status in [500, 502, 503, 504] {
        for replacing in [false, true] {
            let fake = Fake::default();
            let backend = fake.backend();
            let mut state = StateFile::new();
            if replacing {
                backend.write_state(&state).await.unwrap();
            }
            backend.read_state().await.unwrap();
            {
                let mut store = fake.0.lock().unwrap();
                store.after_commit_response = Some(reply(status, json!({"message":"lost reply"})));
                store.fail_read_after_commit = true;
            }
            state.serial = 1;
            assert!(matches!(backend.write_state(&state).await,
                Err(BackendError::GitHub(GitHubError::Api { status: actual, .. })) if actual == status));
            assert_eq!(
                fake.bytes("state.json").unwrap(),
                carina_core::utils::pretty_with_newline_bytes(&state).unwrap()
            );
            {
                let mut store = fake.0.lock().unwrap();
                // End the read outage even if the implementation skipped reconciliation.
                store.response = None;
                store.requests.clear();
            }
            state.serial = 2;
            backend.write_state(&state).await.unwrap();
            assert_eq!(
                fake.bytes("state.json").unwrap(),
                carina_core::utils::pretty_with_newline_bytes(&state).unwrap()
            );
            assert_eq!(
                fake.0
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .filter(|r| r.method == Method::Put)
                    .count(),
                1
            );
        }
    }
}

#[tokio::test]
async fn github_round5_committed_server_error_preserves_our_uuid_for_the_next_acquire() {
    for status in [500, 502, 503, 504] {
        let fake = Fake::default();
        let backend = fake.backend();
        {
            let mut store = fake.0.lock().unwrap();
            store.after_commit_response = Some(reply(status, json!({"message":"lost reply"})));
            store.fail_read_after_commit = true;
        }
        assert!(matches!(backend.acquire_lock("apply").await,
            Err(BackendError::GitHub(GitHubError::Api { status: actual, .. })) if actual == status));
        let sent = fake.bytes("state.json.lock").unwrap();
        {
            let mut store = fake.0.lock().unwrap();
            store.response = None;
            store.requests.clear();
        }
        let recovered = backend.acquire_lock("apply").await.unwrap();
        assert_eq!(serde_json::to_vec(&recovered).unwrap(), sent);
        assert!(
            fake.0
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|r| r.method == Method::Get)
        );
    }
}

#[tokio::test]
async fn github_round5_committed_server_errors_reconcile_put_and_delete_in_the_same_call() {
    for status in [500, 502, 503, 504] {
        for deleting in [false, true] {
            let fake = Fake::default();
            let backend = fake.backend();
            let lock = backend.acquire_lock("apply").await.unwrap();
            backend.read_state().await.unwrap();
            {
                let mut store = fake.0.lock().unwrap();
                store.after_commit_response = Some(reply(status, json!({"message":"lost reply"})));
                store.requests.clear();
            }
            if deleting {
                backend.release_lock(&lock).await.unwrap();
                assert!(fake.bytes("state.json.lock").is_none());
            } else {
                let state = StateFile::new();
                backend.write_state(&state).await.unwrap();
                assert_eq!(
                    fake.bytes("state.json").unwrap(),
                    carina_core::utils::pretty_with_newline_bytes(&state).unwrap()
                );
            }
            assert_eq!(
                fake.0
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .filter(|r| r.method != Method::Get)
                    .count(),
                1
            );
        }
    }
}

#[tokio::test]
async fn github_round5_committed_delete_server_error_keeps_pending_release_until_readable() {
    for force in [false, true] {
        let fake = Fake::default();
        let backend = fake.backend();
        let lock = backend.acquire_lock("apply").await.unwrap();
        {
            let mut store = fake.0.lock().unwrap();
            store.after_commit_response = Some(reply(502, json!({"message":"lost reply"})));
            store.fail_read_after_commit = true;
        }
        let result = if force {
            backend.force_unlock(lock.id()).await
        } else {
            backend.release_lock(&lock).await
        };
        assert!(matches!(
            result,
            Err(BackendError::GitHub(GitHubError::Api { status: 502, .. }))
        ));
        {
            let mut store = fake.0.lock().unwrap();
            store.response = None;
            store.requests.clear();
        }
        if force {
            backend.force_unlock(lock.id()).await.unwrap();
        } else {
            backend.release_lock(&lock).await.unwrap();
        }
        assert!(
            fake.0
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|r| r.method == Method::Get)
        );
        assert!(fake.bytes("state.json.lock").is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn github_round5_empty_tree_root_proves_absence_before_the_first_write() {
    for raw in [false, true] {
        let fake = Fake::default();
        let commit = CommitSha::parse(&fake.0.lock().unwrap().commit()).unwrap();
        let response = fake
            .execute(
                GitHubRequest::get(
                    "/repos/owner/repo/contents".into(),
                    Some(&commit),
                    JSON_MEDIA_TYPE,
                ),
                Instant::now() + OPERATION_BUDGET,
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(decode(&response.body).unwrap(), json!([]));
        fake.0.lock().unwrap().requests.clear();
        let backend = fake.backend();
        if raw {
            assert!(backend.read_state_bytes().await.unwrap().is_none());
        } else {
            assert!(backend.read_state().await.unwrap().is_none());
        }
        {
            let store = fake.0.lock().unwrap();
            assert_eq!(store.requests.len(), 3);
            assert_eq!(
                store.requests[1].reference.as_deref(),
                Some(commit.as_str())
            );
            assert_eq!(store.requests[2].reference, store.requests[1].reference);
        }
        backend.write_state(&StateFile::new()).await.unwrap();
        assert!(fake.bytes("state.json").is_some());
    }
}

#[tokio::test]
async fn github_round5_release_cannot_treat_lost_contents_access_as_lease_absence() {
    for operation in ["release", "cleanup", "force unlock"] {
        for root_status in [403, 404] {
            let fake = Fake::default();
            let backend = fake.published_backend();
            let lock = backend.acquire_lock("apply").await.unwrap();
            let before = fake.bytes("state.json.lock").unwrap();
            {
                let mut store = fake.0.lock().unwrap();
                store.contents_status = Some(404);
                store.root_status = Some(root_status);
                store.requests.clear();
            }
            let result = match operation {
                "cleanup" => backend.release_lock_for_cleanup(&lock).await,
                "force unlock" => backend.force_unlock(lock.id()).await,
                _ => backend.release_lock(&lock).await,
            };
            assert!(
                matches!(
                    &result,
                    Err(BackendError::GitHub(GitHubError::ContentsReadDenied { .. }))
                ),
                "{operation}: {result:?}"
            );
            assert_located_message(&result.unwrap_err(), &["token", "Contents read"]);
            assert_eq!(fake.bytes("state.json.lock").unwrap(), before);
            let store = fake.0.lock().unwrap();
            assert!(store.requests.iter().all(|r| r.method == Method::Get));
            assert_eq!(
                store.requests.len(),
                if operation == "cleanup" { 4 } else { 3 }
            );
            let n = store.requests.len();
            assert_eq!(
                store.requests[n - 2].reference,
                store.requests[n - 1].reference
            );
        }
    }
}

#[tokio::test]
async fn github_round4_existing_state_needs_no_root_probe() {
    let fake = Fake::default();
    fake.put("state.json", serde_json::to_vec(&StateFile::new()).unwrap());
    fake.backend()
        .read_state()
        .await
        .unwrap()
        .unwrap()
        .into_state();
    assert_eq!(fake.0.lock().unwrap().requests.len(), 2);
}

#[tokio::test]
async fn github_round4_delete_404_without_readable_absence_reports_contents_write() {
    struct HiddenAfterDelete {
        fake: Fake,
        status: u16,
    }
    #[async_trait]
    impl GitHubTransport for HiddenAfterDelete {
        async fn execute(
            &self,
            request: GitHubRequest,
            deadline: Instant,
        ) -> Result<GitHubResponse, GitHubError> {
            if request.method == Method::Delete {
                let mut store = self.fake.0.lock().unwrap();
                store.requests.push(request);
                store.contents_status = Some(self.status);
                store.root_status = Some(403);
                return Ok(reply(404, json!({"message":"Not Found"})));
            }
            self.fake.execute(request, deadline).await
        }
    }
    for status in [403, 404] {
        let fake = Fake::default();
        let backend = GitHubBackend::with_transport(
            settings(),
            Arc::new(HiddenAfterDelete {
                fake: fake.clone(),
                status,
            }),
        );
        let lock = backend.acquire_lock("apply").await.unwrap();
        let error = backend.release_lock(&lock).await.unwrap_err();
        assert!(
            matches!(
                error,
                BackendError::GitHub(GitHubError::ContentsWriteDenied { .. })
            ),
            "{error}"
        );
        assert_located_message(&error, &["token", "Contents write"]);
        assert!(fake.bytes("state.json.lock").is_some());
    }
}

#[tokio::test]
async fn github_round4_mutation_rate_limits_keep_the_round2_classification() {
    for delete in [false, true] {
        for (status, message, header) in [
            (
                403,
                "API rate limit exceeded",
                Some(("x-ratelimit-remaining", "0")),
            ),
            (403, "secondary rate limit", None),
            (403, "slow down", Some(("retry-after", "120"))),
            (429, "slow down", None),
        ] {
            let fake = Fake::default();
            let backend = fake.backend();
            backend.read_state().await.unwrap();
            let lock = backend.acquire_lock("apply").await.unwrap();
            let mut response = reply(status, json!({"message": message}));
            if let Some((key, value)) = header {
                response.headers.insert(key.into(), value.into());
            }
            fake.0.lock().unwrap().mutation_response = Some(response);
            let error = if delete {
                backend.release_lock(&lock).await.unwrap_err()
            } else {
                backend.write_state(&StateFile::new()).await.unwrap_err()
            };
            assert!(
                matches!(error, BackendError::GitHub(GitHubError::RateLimited { status: found, .. }) if found == status)
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn github_round10_large_cleanup_reconciliation_never_downloads_raw_state() {
    for status in [409, 422, 503] {
        for committed in [false, true] {
            for locked in [false, true] {
                let fake = Fake::default();
                let mut state = StateFile::new();
                state.carina_version = "x".repeat(1_000_001);
                fake.put("state.json", serde_json::to_vec(&state).unwrap());
                let backend = fake.published_backend();
                backend.read_state().await.unwrap();
                let lock = if locked {
                    Some(backend.acquire_lock("apply").await.unwrap())
                } else {
                    None
                };
                {
                    let mut store = fake.0.lock().unwrap();
                    store.requests.clear();
                    let response = reply(status, json!({"message":"temporary failure"}));
                    if committed {
                        store.after_commit_response = Some(response);
                    } else {
                        store.mutation_response = Some(response);
                    }
                }
                state.serial = 1;
                let result = if let Some(lock) = &lock {
                    backend.write_state_locked_for_cleanup(&state, lock).await
                } else {
                    backend.write_state_for_cleanup(&state).await
                };
                if committed || status == 409 {
                    result.unwrap();
                    assert_eq!(
                        fake.bytes("state.json").unwrap(),
                        carina_core::utils::pretty_with_newline_bytes(&state).unwrap()
                    );
                } else {
                    assert!(matches!(
                        result,
                        Err(BackendError::GitHub(GitHubError::Api { status: code, .. }))
                            if code == status
                    ));
                }
                let store = fake.0.lock().unwrap();
                assert!(
                    store.requests.iter().all(|r| r.accept != RAW_MEDIA_TYPE),
                    "status={status}, committed={committed}, locked={locked}"
                );
                assert!(store.requests.iter().any(|r| {
                    r.method == Method::Get
                        && r.path.ends_with("/contents/state.json")
                        && r.reference
                            .as_ref()
                            .is_some_and(|sha| store.history.contains_key(sha))
                }));
            }
        }
    }
}

#[tokio::test]
async fn github_round10_mismatched_lease_ack_retains_uncertainty_and_recovers_exact_bytes() {
    for operation in ["create", "takeover", "renew"] {
        for successor in [false, true] {
            let fake = Fake::default();
            let backend = fake.backend();
            if operation == "takeover" {
                let expired = LockInfo::with_timeout("expired", -1);
                fake.put("state.json.lock", serde_json::to_vec(&expired).unwrap());
            }
            let held = if operation == "renew" {
                Some(backend.acquire_lock("apply").await.unwrap())
            } else {
                None
            };
            fake.0.lock().unwrap().wrong_blob_sha = true;
            let result = if let Some(lock) = &held {
                backend.renew_lock(lock).await
            } else {
                backend.acquire_lock("apply").await
            };
            assert!(matches!(
                result,
                Err(BackendError::GitHub(GitHubError::UnexpectedOutcome { .. }))
            ));
            let sent = fake.bytes("state.json.lock").unwrap();
            assert!(
                backend.pending_lease.lock().unwrap().is_some(),
                "{operation}"
            );
            fake.0.lock().unwrap().wrong_blob_sha = false;
            if successor {
                let other = LockInfo::new("successor");
                fake.put("state.json.lock", serde_json::to_vec(&other).unwrap());
            }
            fake.0.lock().unwrap().requests.clear();
            let recovered = backend.acquire_lock("apply").await;
            if successor {
                assert!(matches!(recovered, Err(BackendError::Locked { .. })));
                assert!(backend.pending_lease.lock().unwrap().is_some());
            } else {
                assert_eq!(serde_json::to_vec(&recovered.unwrap()).unwrap(), sent);
                assert!(backend.pending_lease.lock().unwrap().is_none());
            }
            assert!(
                fake.0
                    .lock()
                    .unwrap()
                    .requests
                    .iter()
                    .all(|r| r.method == Method::Get)
            );
        }
    }
}

#[test]
fn github_round10_request_ids_render_without_option_debug() {
    for request_id in [None, Some("request-123".to_string())] {
        for error in [
            GitHubError::Api {
                status: 503,
                body: "unavailable".into(),
                request_id: request_id.clone(),
            },
            GitHubError::Contention {
                attempts: 5,
                message: "branch moved".into(),
                request_id: request_id.clone(),
            },
        ] {
            let display = error.to_string();
            assert!(
                !display.contains("Some(") && !display.contains("None"),
                "{display}"
            );
            if let Some(id) = &request_id {
                assert!(
                    display.ends_with(&format!("(request ID: {id})")),
                    "{display}"
                );
            } else {
                assert!(!display.contains("request ID"), "{display}");
            }
        }
    }
}
