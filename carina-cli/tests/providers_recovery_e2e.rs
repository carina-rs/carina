use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use carina_core::parser::ProviderConfig;
use carina_provider_resolver::{LockMode, resolve_all};
use indexmap::IndexMap;

const PROVIDER: &str = "registry.carina-rs.dev/carina-rs/aws";
const HOST: &str = "registry.carina-rs.dev";

fn write_protected_lock(base_dir: &Path) {
    fs::write(
        base_dir.join("carina-providers.lock"),
        r#"version = 3

[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true

[registry_host."registry.carina-rs.dev".discovery_values]
"providers.v1" = "https://registry.carina-rs.dev/v1/providers/"

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
constraint = "^0.5"
sha256 = "pinned-shasum"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
sequence_present = true
sequence = 7
sequence_anchor_established = true
sequence_anchor = 5
valid_until_present = true
yanked_versions = ["0.4.0"]
signature_present = true
certificate_identity = "identity-a"
certificate_oidc_issuer = "issuer-a"
transparency_log_present = true
"#,
    )
    .unwrap();
}

fn write_v2_protected_lock(base_dir: &Path) {
    fs::write(
        base_dir.join("carina-providers.lock"),
        r#"version = 2

[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true
api_base_url = "https://registry.carina-rs.dev/v1/providers/"
discovery_sha256 = "legacy-discovery-sha256"

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
constraint = "^0.5"
sha256 = "pinned-shasum"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
sequence_present = true
sequence = 7
sequence_anchor_established = true
sequence_anchor = 5
valid_until_present = true
yanked_versions = ["0.4.0"]
signature_present = true
certificate_identity = "identity-a"
certificate_oidc_issuer = "issuer-a"
transparency_log_present = true
"#,
    )
    .unwrap();
}

fn registry_provider_config() -> ProviderConfig {
    ProviderConfig {
        name: "aws".into(),
        source: Some("carina-rs/aws".into()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        is_default: true,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    }
}

fn carina(base_dir: &Path, operation: &str, force: bool, input: &str) -> Output {
    carina_target(base_dir, operation, PROVIDER, force, input)
}

fn carina_init(base_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["init", base_dir.to_str().unwrap()])
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to execute carina init")
}

fn carina_target(
    base_dir: &Path,
    operation: &str,
    target: &str,
    force: bool,
    input: &str,
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command.args(["providers", operation, target]);
    if force {
        command.arg("--force");
    }
    let mut child = command
        .arg(base_dir)
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn v2_lock_migration_keeps_all_recovery_commands_reachable_without_authorizing_discovery() {
    for (operation, target) in [
        ("repin-discovery", HOST),
        ("repin-identity", PROVIDER),
        ("re-bootstrap", PROVIDER),
    ] {
        let dir = tempfile::tempdir().unwrap();
        write_v2_protected_lock(dir.path());

        let output = carina_target(dir.path(), operation, target, true, "");
        assert!(
            output.status.success(),
            "{operation} must remain reachable through v2 migration\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("Provider lock format migration: version 2 -> version 3"),
            "{operation}: {stderr}"
        );
        assert_eq!(
            stderr
                .matches("Provider lock format migration: version 2 -> version 3")
                .count(),
            1,
            "{operation} must report migration exactly once: {stderr}"
        );
        assert!(
            stderr.contains("Registry host: registry.carina-rs.dev"),
            "{operation}: {stderr}"
        );
        let migration_action = if operation == "repin-discovery" {
            "Discarding"
        } else {
            "Retaining"
        };
        assert!(
            stderr.contains(&format!("{migration_action} v2 pinned discovery API base")),
            "{operation}: {stderr}"
        );
        assert!(
            stderr.contains(&format!("{migration_action} v2 discovery document SHA-256")),
            "{operation}: {stderr}"
        );
        assert!(
            stderr.contains("All provider entries and provider security state will be retained."),
            "{operation}: {stderr}"
        );

        let saved = fs::read_to_string(dir.path().join("carina-providers.lock")).unwrap();
        assert!(saved.starts_with("version = 3\n"), "{operation}: {saved}");
        assert!(
            saved.contains("discovery_pin_present = false"),
            "{operation}: {saved}"
        );
        assert!(
            saved.contains("sha256 = \"pinned-shasum\""),
            "{operation}: {saved}"
        );
        assert!(
            saved.contains("yanked_versions = [\"0.4.0\"]"),
            "{operation}: {saved}"
        );
        assert!(
            saved.contains("valid_until_present = true"),
            "{operation}: {saved}"
        );
        assert!(
            saved.contains("transparency_log_present = true"),
            "{operation}: {saved}"
        );

        match operation {
            "repin-discovery" => {
                assert!(!saved.contains("migration_pending_discovery_pin"));
                assert!(!saved.contains("api_base_url"), "{saved}");
                assert!(!saved.contains("discovery_sha256"), "{saved}");
                assert!(saved.contains("sequence = 7"), "{saved}");
                assert!(saved.contains("sequence_anchor = 5"), "{saved}");
                assert!(saved.contains("certificate_identity = \"identity-a\""));
            }
            "repin-identity" => {
                assert!(saved.contains("migration_pending_discovery_pin"));
                assert!(saved.contains("api_base_url"), "{saved}");
                assert!(saved.contains("discovery_sha256"), "{saved}");
                assert!(saved.contains("sequence = 7"), "{saved}");
                assert!(saved.contains("sequence_anchor = 5"), "{saved}");
                assert!(!saved.contains("certificate_identity"), "{saved}");
                assert!(saved.contains("signature_present = true"), "{saved}");
            }
            "re-bootstrap" => {
                assert!(saved.contains("migration_pending_discovery_pin"));
                assert!(saved.contains("api_base_url"), "{saved}");
                assert!(saved.contains("discovery_sha256"), "{saved}");
                assert!(saved.contains("sequence_present = false"), "{saved}");
                assert!(
                    saved.contains("sequence_anchor_established = false"),
                    "{saved}"
                );
                assert!(saved.contains("certificate_identity = \"identity-a\""));
            }
            _ => unreachable!(),
        }

        if operation != "repin-discovery" {
            let error = resolve_all(dir.path(), &[registry_provider_config()], LockMode::Normal)
                .expect_err("provider-level recovery must not authorize discovery migration");
            assert!(
                error.contains("lock-format migration requires operator authorization"),
                "{operation}: {error}"
            );
            assert!(
                error.contains("carina providers repin-discovery registry.carina-rs.dev"),
                "{operation}: {error}"
            );
        }
    }
}

#[test]
fn init_reports_v2_lock_migration_once_before_authorization_error() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("providers.crn"),
        r#"provider aws {
  source = 'carina-rs/aws'
  version = '^0.5'
}
"#,
    )
    .unwrap();
    write_v2_protected_lock(dir.path());

    let output = carina_init(dir.path());
    assert!(
        !output.status.success(),
        "migration-pending discovery must remain blocked"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let header = "Provider lock format migration: version 2 -> version 3";
    assert!(stderr.contains(header), "{stderr}");
    assert_eq!(
        stderr.matches(header).count(),
        1,
        "carina init must report migration exactly once: {stderr}"
    );
    assert!(
        stderr.contains("lock-format migration requires operator authorization"),
        "{stderr}"
    );
}

#[test]
fn declining_migrated_discovery_repin_preserves_pin_and_resolution_block() {
    let dir = tempfile::tempdir().unwrap();
    write_v2_protected_lock(dir.path());
    let lock_path = dir.path().join("carina-providers.lock");
    let before = fs::read(&lock_path).unwrap();

    let cancelled = carina_target(
        dir.path(),
        "repin-discovery",
        HOST,
        false,
        &format!("{PROVIDER}\n"),
    );
    assert!(
        cancelled.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&cancelled.stdout),
        String::from_utf8_lossy(&cancelled.stderr)
    );
    let stdout = String::from_utf8(cancelled.stdout).unwrap();
    assert!(stdout.contains("Enter registry host"), "{stdout}");
    assert!(stdout.contains("Recovery cancelled."), "{stdout}");
    assert_eq!(
        fs::read(&lock_path).unwrap(),
        before,
        "declining must leave the v2 pin intact on disk"
    );

    let error = resolve_all(dir.path(), &[registry_provider_config()], LockMode::Normal)
        .expect_err("declining must leave migrated discovery resolution blocked");
    assert!(
        error.contains("lock-format migration requires operator authorization"),
        "{error}"
    );
    assert!(
        error.contains("carina providers repin-discovery registry.carina-rs.dev"),
        "{error}"
    );
    assert_eq!(fs::read(lock_path).unwrap(), before);
}

#[test]
fn repin_discovery_prints_and_clears_consumed_host_values_only() {
    let dir = tempfile::tempdir().unwrap();
    write_protected_lock(dir.path());

    let output = carina_target(dir.path(), "repin-discovery", HOST, true, "");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Registry host: registry.carina-rs.dev"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "Discarding pinned discovery value providers.v1: https://registry.carina-rs.dev/v1/providers/"
        ),
        "{stderr}"
    );
    assert!(!stdout.contains("Discarding"), "{stdout}");
    assert!(!stdout.contains("Enter registry host"), "{stdout}");

    let lock_path = dir.path().join("carina-providers.lock");
    carina_provider_resolver::LockFile::load(&lock_path)
        .expect("re-pinned lock must remain parseable")
        .expect("re-pinned lock must remain present");
    let saved = fs::read_to_string(lock_path).unwrap();
    assert!(saved.contains("discovery_pin_present = false"), "{saved}");
    assert!(!saved.contains("discovery_values"), "{saved}");
    assert!(
        saved.contains("resolved_hostname = \"registry.carina-rs.dev\""),
        "{saved}"
    );
    assert!(saved.contains("sha256 = \"pinned-shasum\""), "{saved}");
    assert!(saved.contains("sequence = 7"), "{saved}");
    assert!(saved.contains("sequence_anchor = 5"), "{saved}");
    assert!(saved.contains("yanked_versions = [\"0.4.0\"]"), "{saved}");
    assert!(
        saved.contains("certificate_identity = \"identity-a\""),
        "{saved}"
    );
    assert!(
        saved.contains("certificate_oidc_issuer = \"issuer-a\""),
        "{saved}"
    );
    assert!(saved.contains("transparency_log_present = true"), "{saved}");
}

#[test]
fn repin_discovery_requires_the_exact_host_confirmation() {
    let cancelled_dir = tempfile::tempdir().unwrap();
    write_protected_lock(cancelled_dir.path());
    let cancelled_path = cancelled_dir.path().join("carina-providers.lock");
    let before = fs::read(&cancelled_path).unwrap();

    let cancelled = carina_target(
        cancelled_dir.path(),
        "repin-discovery",
        HOST,
        false,
        &format!("{PROVIDER}\n"),
    );
    assert!(cancelled.status.success());
    let cancelled_stdout = String::from_utf8(cancelled.stdout).unwrap();
    assert!(
        cancelled_stdout.contains("Enter registry host"),
        "{cancelled_stdout}"
    );
    assert!(
        cancelled_stdout.contains("Recovery cancelled."),
        "{cancelled_stdout}"
    );
    assert_eq!(fs::read(cancelled_path).unwrap(), before);

    let confirmed_dir = tempfile::tempdir().unwrap();
    write_protected_lock(confirmed_dir.path());
    let confirmed = carina_target(
        confirmed_dir.path(),
        "repin-discovery",
        HOST,
        false,
        &format!("{HOST}\n"),
    );
    assert!(
        confirmed.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&confirmed.stdout),
        String::from_utf8_lossy(&confirmed.stderr)
    );
    let confirmed_stdout = String::from_utf8(confirmed.stdout).unwrap();
    assert!(
        confirmed_stdout.contains("Enter registry host"),
        "{confirmed_stdout}"
    );
    assert!(!confirmed_stdout.contains("Recovery cancelled."));
    assert!(
        fs::read_to_string(confirmed_dir.path().join("carina-providers.lock"))
            .unwrap()
            .contains("discovery_pin_present = false")
    );
}

#[test]
fn repin_identity_prints_discarded_pin_and_preserves_other_lock_state() {
    let dir = tempfile::tempdir().unwrap();
    write_protected_lock(dir.path());

    let output = carina(dir.path(), "repin-identity", true, "");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Discarding certificate identity: identity-a"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Discarding OIDC issuer: issuer-a"),
        "{stderr}"
    );
    assert!(!stdout.contains("Discarding"), "{stdout}");
    assert!(!stdout.contains("Enter provider source"), "{stdout}");

    let lock_path = dir.path().join("carina-providers.lock");
    carina_provider_resolver::LockFile::load(&lock_path)
        .expect("repinned lock must remain parseable")
        .expect("repinned lock must remain present");
    let saved = fs::read_to_string(lock_path).unwrap();
    assert!(saved.contains("sha256 = \"pinned-shasum\""), "{saved}");
    assert!(saved.contains("sequence = 7"), "{saved}");
    assert!(saved.contains("sequence_anchor = 5"), "{saved}");
    assert!(saved.contains("yanked_versions = [\"0.4.0\"]"), "{saved}");
    assert!(saved.contains("signature_present = true"), "{saved}");
    assert!(!saved.contains("certificate_identity"), "{saved}");
    assert!(!saved.contains("certificate_oidc_issuer"), "{saved}");
    assert!(saved.contains("transparency_log_present = true"), "{saved}");
}

#[test]
fn rebootstrap_prints_and_clears_both_freshness_values_only() {
    let dir = tempfile::tempdir().unwrap();
    write_protected_lock(dir.path());

    let output = carina(dir.path(), "re-bootstrap", true, "");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Discarding sequence observation: 7"),
        "{stderr}"
    );
    assert!(stderr.contains("Discarding sequence anchor: 5"), "{stderr}");
    assert!(!stdout.contains("Discarding"), "{stdout}");
    assert!(!stdout.contains("Enter provider source"), "{stdout}");

    let lock_path = dir.path().join("carina-providers.lock");
    carina_provider_resolver::LockFile::load(&lock_path)
        .expect("re-bootstrapped lock must remain parseable")
        .expect("re-bootstrapped lock must remain present");
    let saved = fs::read_to_string(lock_path).unwrap();
    assert!(saved.contains("sha256 = \"pinned-shasum\""), "{saved}");
    assert!(saved.contains("sequence_present = false"), "{saved}");
    assert!(
        saved.contains("sequence_anchor_established = false"),
        "{saved}"
    );
    assert!(!saved.contains("sequence = "), "{saved}");
    assert!(!saved.contains("sequence_anchor = "), "{saved}");
    assert!(saved.contains("yanked_versions = [\"0.4.0\"]"), "{saved}");
    assert!(saved.contains("signature_present = true"), "{saved}");
    assert!(
        saved.contains("certificate_identity = \"identity-a\""),
        "{saved}"
    );
    assert!(
        saved.contains("certificate_oidc_issuer = \"issuer-a\""),
        "{saved}"
    );
    assert!(saved.contains("transparency_log_present = true"), "{saved}");
}

#[test]
fn recovery_without_force_cancels_on_mismatch_and_preserves_the_lock() {
    for operation in ["repin-identity", "re-bootstrap"] {
        let dir = tempfile::tempdir().unwrap();
        write_protected_lock(dir.path());
        let lock_path = dir.path().join("carina-providers.lock");
        let before = fs::read(&lock_path).unwrap();

        let output = carina(dir.path(), operation, false, "carina-rs/not-aws\n");

        assert!(
            output.status.success(),
            "{operation} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stdout.contains("Enter provider source"), "{stdout}");
        assert!(stdout.contains("Recovery cancelled."), "{stdout}");
        assert!(stderr.contains("Discarding"), "{stderr}");
        assert_eq!(
            fs::read(lock_path).unwrap(),
            before,
            "{operation} mutated the lock"
        );
    }
}

#[test]
fn recovery_without_force_proceeds_after_exact_provider_confirmation() {
    for operation in ["repin-identity", "re-bootstrap"] {
        let dir = tempfile::tempdir().unwrap();
        write_protected_lock(dir.path());
        let lock_path = dir.path().join("carina-providers.lock");

        let output = carina(dir.path(), operation, false, &format!("{PROVIDER}\n"));

        assert!(
            output.status.success(),
            "{operation} failed\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("Enter provider source"), "{stdout}");
        assert!(!stdout.contains("Recovery cancelled."), "{stdout}");
        let saved = fs::read_to_string(lock_path).unwrap();
        match operation {
            "repin-identity" => assert!(!saved.contains("certificate_identity"), "{saved}"),
            "re-bootstrap" => assert!(
                saved.contains("sequence_anchor_established = false"),
                "{saved}"
            ),
            _ => unreachable!(),
        }
    }
}
