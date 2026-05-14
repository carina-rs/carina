//! Integration tests for `carina init` validation.
//!
//! These tests verify that `init` rejects projects whose provider blocks
//! cannot be resolved (e.g. missing `source`), instead of silently claiming
//! success.

use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn carina_init(project_dir: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["init", project_dir.to_str().unwrap()])
        .output()
        .expect("failed to execute carina init")
}

#[test]
fn init_fails_when_provider_has_no_source() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();
    fs::write(
        project.join("main.crn"),
        r#"provider awscc {
  region = awscc.Region.ap_northeast_1
}

awscc.s3.Bucket {
  name = 'test'
}
"#,
    )
    .unwrap();

    let output = carina_init(project);

    assert!(
        !output.status.success(),
        "expected init to fail but it succeeded.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("has no source configured"),
        "expected error about missing source, got stderr:\n{}",
        stderr,
    );

    assert!(
        !project.join(".carina").exists(),
        ".carina/ should not be created on failure",
    );
    assert!(
        !project.join("carina-backend.lock").exists(),
        "carina-backend.lock should not be created on failure",
    );
    assert!(
        !project.join("carina-providers.lock").exists(),
        "carina-providers.lock should not be created on failure",
    );
}

/// carina#3023: `carina init` must not require `source` on a named
/// provider instance. The parser already rejects `source` /
/// `version` / `revision` on `let <name> = provider <kind> { ... }`
/// (those are kind-level properties), so the init source-existence
/// check needs to look at the kind default only.
///
/// Pre-fix behaviour: init walked every `ProviderConfig` and
/// reported "Provider 'aws' has no source configured" for the named
/// instance. The reported message named the *kind*, leaving users
/// with no way to act on it: their default block already had
/// `source` set.
///
/// We assert on the *stderr text* rather than overall exit code:
/// downstream of the pre-check, `carina init` tries to resolve the
/// `file://` plugin we point at (which doesn't exist on the test
/// runner), so init will still fail — but it should fail with a
/// different error (a fetch / find error from
/// `carina_provider_resolver`), not the pre-check string.
#[test]
fn init_does_not_require_source_on_named_provider_instance() {
    let tmp = TempDir::new().unwrap();
    let project = tmp.path();

    // Multi-file fixture mirroring the carina-rs/infra shape from the
    // bug report: default `provider aws` carries `source` on its own
    // (`file://` to a fake path — init will still fail downstream
    // because the plugin doesn't actually resolve, but that's a
    // different error than the pre-check we're testing for), and a
    // `let us = provider aws { region = ... }` named instance sits
    // beside it without `source`.
    fs::write(
        project.join("providers.crn"),
        "provider aws {\n  \
         source = 'file:///nonexistent/fake-provider.wasm'\n  \
         region = aws.Region.ap_northeast_1\n\
         }\n\
         \n\
         let us = provider aws {\n  \
         region = aws.Region.us_east_1\n\
         }\n",
    )
    .unwrap();
    fs::write(
        project.join("main.crn"),
        "aws.s3.Bucket {\n  \
         bucket_name = 'test'\n\
         }\n",
    )
    .unwrap();

    let output = carina_init(project);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Match the exact wording `missing_provider_source_message`
    // produces, not just a loose substring, so a future
    // unrelated error from `carina_provider_resolver` that
    // happens to share words doesn't masquerade as a pass.
    let pre_check_msg = "Provider 'aws' has no source configured. Add `source = 'github.com/...'` to the provider block.";
    assert!(
        !stderr.contains(pre_check_msg) && !stdout.contains(pre_check_msg),
        "init must not surface the kind-level 'no source configured' \
         error when the only entry lacking `source` is the named \
         instance — that's a parser-enforced invariant, not a user \
         mistake. carina#3023.\n--- stdout ---\n{}\n--- stderr ---\n{}",
        stdout,
        stderr,
    );
}
