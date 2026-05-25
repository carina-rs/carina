//! Integration tests for `carina-cli`'s KMS decryptor against an
//! in-process AWS mock (`winterbaume`, library mode).
//!
//! The decryptor is the closure `create_provider_context` builds for
//! `ProviderContext.decryptor` — it powers the DSL `decrypt("…")`
//! built-in by calling AWS KMS. `carina-core`'s
//! `parse_decrypt_uses_config_decryptor` already proves the parser
//! invokes whatever decryptor it is given, but the decryptor *that
//! carina-cli actually ships* (the one that talks to KMS) has had no
//! coverage. These tests fill that gap by injecting a winterbaume-backed
//! `aws_sdk_kms::Client` into `build_kms_decryptor` and exercising the
//! real base64 → `KMS:Decrypt` → UTF-8 path with no real AWS and no
//! external process (#3227).
//!
//! KeyId coverage: `decrypt_one`'s `Some(key)` branch (the optional
//! `KeyId` argument) is exercised in
//! `decryptor_rejects_wrong_key_id_with_incorrect_key_exception` —
//! `winterbaume-kms` 0.2.1 validates the request-side `KeyId` against
//! the ciphertext header and surfaces `IncorrectKeyException`, matching
//! the documented AWS behaviour.

use aws_sdk_kms::Client;
use aws_sdk_kms::primitives::Blob;
use base64::Engine;
use carina_cli::kms::build_kms_decryptor;
use winterbaume_core::MockAws;
use winterbaume_kms::KmsService;

const TEST_REGION: &str = "us-east-1";
const TEST_PLAINTEXT: &[u8] = b"hello carina kms decryptor";

/// Build an `aws_sdk_kms::Client` wired to a fresh in-process winterbaume
/// KMS service. Each call gets an isolated mock with empty state.
async fn mock_kms_client() -> Client {
    let mock = MockAws::builder().with_service(KmsService::new()).build();
    let sdk_config = mock.sdk_config(TEST_REGION).await;
    Client::new(&sdk_config)
}

/// Create a key in the mock and encrypt `plaintext` with it, returning
/// the base64-encoded ciphertext blob the way carina's `decrypt(...)`
/// built-in receives it from the user (`.crn` literals are already
/// base64-encoded).
async fn seed_ciphertext_base64(client: &Client, plaintext: &[u8]) -> String {
    let key = client
        .create_key()
        .send()
        .await
        .expect("create_key should succeed");
    let key_id = key.key_metadata().unwrap().key_id();

    let resp = client
        .encrypt()
        .key_id(key_id)
        .plaintext(Blob::new(plaintext.to_vec()))
        .send()
        .await
        .expect("encrypt should succeed");
    let ciphertext = resp
        .ciphertext_blob()
        .expect("encrypt response should carry a ciphertext blob");

    base64::engine::general_purpose::STANDARD.encode(ciphertext.as_ref())
}

#[tokio::test(flavor = "multi_thread")]
async fn decryptor_round_trips_base64_ciphertext_through_kms() {
    let client = mock_kms_client().await;
    let ciphertext_b64 = seed_ciphertext_base64(&client, TEST_PLAINTEXT).await;

    let decryptor = build_kms_decryptor(client);

    let plaintext = decryptor(&ciphertext_b64, None).expect("decryptor must succeed");
    assert_eq!(
        plaintext.as_bytes(),
        TEST_PLAINTEXT,
        "decryptor must return the original plaintext bytes",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn decryptor_round_trips_with_matching_key_id() {
    // `Some(key)` branch, matching ciphertext key: real KMS accepts the
    // request and `decrypt_one` returns the plaintext. Without this case
    // a no-op `Some` branch (e.g. one that silently ignored the argument)
    // would not be caught by the `None` happy-path test.
    let client = mock_kms_client().await;
    let key = client.create_key().send().await.expect("create_key");
    let key_id = key.key_metadata().unwrap().key_id().to_string();

    let enc = client
        .encrypt()
        .key_id(&key_id)
        .plaintext(Blob::new(TEST_PLAINTEXT.to_vec()))
        .send()
        .await
        .expect("encrypt");
    let ciphertext_b64 =
        base64::engine::general_purpose::STANDARD.encode(enc.ciphertext_blob().unwrap().as_ref());

    let decryptor = build_kms_decryptor(client);

    let plaintext = decryptor(&ciphertext_b64, Some(&key_id))
        .expect("decryptor must succeed when KeyId matches the ciphertext");
    assert_eq!(plaintext.as_bytes(), TEST_PLAINTEXT);
}

#[tokio::test(flavor = "multi_thread")]
async fn decryptor_rejects_wrong_key_id_with_incorrect_key_exception() {
    // `Some(key)` branch, mismatched ciphertext key: real KMS returns
    // `IncorrectKeyException` per the `KMS:Decrypt` API docs. winterbaume
    // 0.2.1 enforces the same. The decryptor must propagate this as a
    // `decrypt(): KMS decrypt failed:` error rather than silently
    // returning the wrong-key plaintext.
    let client = mock_kms_client().await;
    let key_a = client.create_key().send().await.expect("create_key a");
    let key_a_id = key_a.key_metadata().unwrap().key_id().to_string();
    let key_b = client.create_key().send().await.expect("create_key b");
    let key_b_id = key_b.key_metadata().unwrap().key_id().to_string();

    let enc = client
        .encrypt()
        .key_id(&key_a_id)
        .plaintext(Blob::new(TEST_PLAINTEXT.to_vec()))
        .send()
        .await
        .expect("encrypt under key_a");
    let ciphertext_b64 =
        base64::engine::general_purpose::STANDARD.encode(enc.ciphertext_blob().unwrap().as_ref());

    let decryptor = build_kms_decryptor(client);

    let err = decryptor(&ciphertext_b64, Some(&key_b_id))
        .expect_err("decrypt with a KeyId that does not match the ciphertext must fail");
    assert!(
        err.starts_with("decrypt(): KMS decrypt failed:"),
        "wrong-KeyId failure must be surfaced via the KMS error label, got: {err}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn decryptor_rejects_invalid_base64_with_named_error() {
    // Base64 decoding happens before any KMS call, so an invalid blob
    // surfaces as a base64-flavoured error string. This asserts the
    // error shape (`decrypt():` prefix + `base64` mention); it does not
    // independently observe that the KMS client was never invoked —
    // that follows from `decrypt_one`'s straight-line code (decode → send).
    let client = mock_kms_client().await;
    let decryptor = build_kms_decryptor(client);

    let err = decryptor("not%%%base64!", None)
        .expect_err("invalid base64 must surface as a decryptor error");
    assert!(
        err.starts_with("decrypt():"),
        "error message must be prefixed with the `decrypt():` builtin label, got: {err}",
    );
    assert!(
        err.contains("base64"),
        "error message must mention base64, got: {err}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn decryptor_surfaces_kms_failure_for_valid_base64_garbage() {
    // A blob that is syntactically valid base64 but is not a real KMS
    // ciphertext reaches the KMS:Decrypt call and is rejected there.
    // This is the path the happy-path test cannot isolate — it proves
    // the closure actually invokes the SDK rather than short-circuiting.
    let client = mock_kms_client().await;
    let garbage_b64 = base64::engine::general_purpose::STANDARD.encode(b"not a real ciphertext");
    let decryptor = build_kms_decryptor(client);

    let err = decryptor(&garbage_b64, None)
        .expect_err("KMS must reject a non-ciphertext blob even though base64 parses");
    assert!(
        err.starts_with("decrypt(): KMS decrypt failed:"),
        "error message must be prefixed with the `decrypt(): KMS decrypt failed:` label, got: {err}",
    );
}
