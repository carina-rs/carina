use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use carina_core::UserDirs;
use carina_core::hint::ProjectCommand;
use carina_core::parser::ProviderConfig;
use carina_provider_resolver::{
    FixtureRegistryHttp, LockMode, ProviderInstallLock, resolve_all,
    resolve_all_with_fixture_registry, sha256_file,
};
use indexmap::IndexMap;

const PROVIDER: &str = "registry.carina-rs.dev/carina-rs/aws";
const HOST: &str = "registry.carina-rs.dev";
const FIXTURE_VERSION: &str = "0.5.0";
const FIXTURE_ARTIFACT_URL: &str = "https://downloads.example.test/aws.wasm";
const FIXTURE_ARTIFACT: &[u8] = b"registry sequence notice fixture";

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

fn write_v1_protected_lock(base_dir: &Path) {
    fs::write(
        base_dir.join("carina-providers.lock"),
        r#"version = 1

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
constraint = "^0.5"
sha256 = "pinned-shasum"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
api_base_url = "https://registry.carina-rs.dev/v1/providers/"
discovery_sha256 = "legacy-discovery-sha256"
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

fn write_two_pin_same_host_v1_lock(base_dir: &Path) {
    write_v1_protected_lock(base_dir);
    let path = base_dir.join("carina-providers.lock");
    let mut lock = fs::read_to_string(&path).unwrap();
    lock.push_str(
        r#"
[[provider]]
name = "random"
source = "carina-rs/random"
mode = "version"
version = "1.0.0"
sha256 = "other-pinned-provider-sha256"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
api_base_url = "https://registry.carina-rs.dev/v1/providers/"
discovery_sha256 = "newer-discovery-sha256"
sequence_present = false
sequence_anchor_established = false
valid_until_present = false
signature_present = false
transparency_log_present = false
"#,
    );
    fs::write(path, lock).unwrap();
}

fn write_two_host_v1_lock(base_dir: &Path) {
    fs::write(
        base_dir.join("carina-providers.lock"),
        r#"version = 1

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.5.0"
sha256 = "aws-sha256"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
api_base_url = "https://registry.carina-rs.dev/v1/providers/"
discovery_sha256 = "default-host-discovery-sha256"
sequence_present = false
sequence_anchor_established = false
valid_until_present = false
signature_present = false
transparency_log_present = false

[[provider]]
name = "random"
source = "registry.example.test/carina-rs/random"
mode = "version"
version = "1.0.0"
sha256 = "random-sha256"

[provider.registry]
resolved_hostname = "registry.example.test"
api_base_url = "https://registry.example.test/v1/providers/"
discovery_sha256 = "other-host-discovery-sha256"
sequence_present = false
sequence_anchor_established = false
valid_until_present = false
signature_present = false
transparency_log_present = false
"#,
    )
    .unwrap();
}

fn write_current_signer_lock(base_dir: &Path) {
    fs::write(
        base_dir.join("carina-providers.lock"),
        r#"version = 5

[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true

[registry_host."registry.carina-rs.dev".discovery_values]
"providers.v1" = "https://registry.carina-rs.dev/v1/providers/"

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "0.7.0"
sha256 = "pinned-shasum"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
sequence_present = true
sequence = 7
sequence_anchor_established = true
sequence_anchor = 5
valid_until_present = true
yanked_versions = ["0.4.0"]
signature_state = "required_current"
source_oidc_issuer = "https://token.actions.githubusercontent.com"
source_repository_owner_id = "260157389"
source_repository_id = "1199024834"
stable_workflow = "https://example.test/release.yml"
prerelease_workflow = "https://example.test/ci.yml"
transparency_log_present = true
"#,
    )
    .unwrap();
}

fn registry_provider_config() -> ProviderConfig {
    ProviderConfig {
        name: "aws".into(),
        source: Some("carina-rs/aws".parse().unwrap()),
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
    }
}

fn fixture_artifact_sha256() -> String {
    let artifact = tempfile::NamedTempFile::new().unwrap();
    fs::write(artifact.path(), FIXTURE_ARTIFACT).unwrap();
    sha256_file(artifact.path()).unwrap()
}

fn registry_fixture(sequence: Option<u64>) -> FixtureRegistryHttp {
    registry_fixture_with_observations(sequence, None, false)
}

fn registry_fixture_with_observations(
    sequence: Option<u64>,
    valid_until: Option<&str>,
    yanked: bool,
) -> FixtureRegistryHttp {
    let mut listing = serde_json::json!({
        "versions": [{"version": FIXTURE_VERSION, "protocols": ["1"]}]
    });
    if yanked {
        listing["versions"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "version": "0.4.0",
                "protocols": ["1"],
                "yanked": true
            }));
    }
    if let Some(sequence) = sequence {
        listing["sequence"] = serde_json::json!(sequence);
    }
    if let Some(valid_until) = valid_until {
        listing["valid_until"] = serde_json::json!(valid_until);
    }
    let shasum = fixture_artifact_sha256();
    FixtureRegistryHttp::default()
        .json(
            "https://registry.carina-rs.dev/.well-known/carina.json",
            r#"{"providers.v1":"/v1/providers/"}"#,
        )
        .json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/versions",
            serde_json::to_vec(&listing).unwrap(),
        )
        .json(
            "https://registry.carina-rs.dev/v1/providers/carina-rs/aws/0.5.0/download",
            format!(
                r#"{{"protocols":["1"],"filename":"aws.wasm","download_url":"{FIXTURE_ARTIFACT_URL}","shasum":"{shasum}","shasum_authored_by":"registry"}}"#
            ),
        )
        .downloadable_bytes(FIXTURE_ARTIFACT_URL, FIXTURE_ARTIFACT.to_vec())
}

fn write_unsigned_anchored_lock(base_dir: &Path, sequence: u64) {
    let shasum = fixture_artifact_sha256();
    fs::write(
        base_dir.join("carina-providers.lock"),
        format!(
            r#"version = 5

[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true

[registry_host."registry.carina-rs.dev".discovery_values]
"providers.v1" = "https://registry.carina-rs.dev/v1/providers/"

[[provider]]
name = "aws"
source = "carina-rs/aws"
mode = "version"
version = "{FIXTURE_VERSION}"
sha256 = "{shasum}"

[provider.registry]
resolved_hostname = "registry.carina-rs.dev"
sequence_present = true
sequence = {sequence}
sequence_anchor_established = true
sequence_anchor = {sequence}
valid_until_present = false
signature_state = "not_required"
transparency_log_present = false
"#
        ),
    )
    .unwrap();
}

fn resolve_with_fixture(base_dir: &Path, user_dirs: &UserDirs, http: &FixtureRegistryHttp) {
    let install_lock = ProviderInstallLock::acquire(base_dir).unwrap();
    resolve_all_with_fixture_registry(
        user_dirs,
        &install_lock,
        &[registry_provider_config()],
        LockMode::Normal,
        http,
    )
    .unwrap();
}

fn run_child_test(test_name: &str, child_env: &str) -> Output {
    Command::new(std::env::current_exe().unwrap())
        .args([test_name, "--exact", "--nocapture"])
        .env(child_env, "1")
        .output()
        .unwrap()
}

fn carina(base_dir: &Path, operation: &str, force: bool, input: &str) -> Output {
    carina_target(base_dir, operation, PROVIDER, force, input)
}

fn carina_init(base_dir: &Path) -> Output {
    let cache = tempfile::tempdir().expect("create isolated plugin cache");
    let trust = tempfile::tempdir().expect("create isolated trust-record directory");
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["init", base_dir.to_str().unwrap()])
        .env("NO_COLOR", "1")
        .env("CARINA_PLUGIN_CACHE_DIR", cache.path())
        .env("CARINA_TRUST_RECORD_DIR", trust.path())
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
fn recovery_without_lock_reports_init_and_does_not_create_project() {
    let parent = tempfile::tempdir().unwrap();
    let project = parent.path().join("missing");

    let output = carina(&project, "re-bootstrap", true, "");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!(
            "Provider lock file {} does not exist; run `{}` first",
            project.join("carina-providers.lock").display(),
            ProjectCommand::new("init", &project)
        )),
        "{stderr}"
    );
    assert!(!project.exists());
}

#[test]
fn recovery_with_file_as_project_reports_missing_lock() {
    let parent = tempfile::tempdir().unwrap();
    let project = parent.path().join("not-a-directory");
    fs::write(&project, b"unchanged").unwrap();

    let output = carina(&project, "re-bootstrap", true, "");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(&format!(
            "Provider lock file {} does not exist; run `{}` first",
            project.join("carina-providers.lock").display(),
            ProjectCommand::new("init", &project)
        )),
        "{stderr}"
    );
    assert_eq!(fs::read(project).unwrap(), b"unchanged");
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
            stderr.contains("Provider lock format migration: version 2 -> version 5"),
            "{operation}: {stderr}"
        );
        assert_eq!(
            stderr
                .matches("Provider lock format migration: version 2 -> version 5")
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
        assert!(saved.starts_with("version = 5\n"), "{operation}: {saved}");
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
                assert!(saved.contains("signature_state = \"required_legacy\""));
                assert!(saved.contains("legacy_certificate_identity = \"identity-a\""));
            }
            "repin-identity" => {
                assert!(saved.contains("migration_pending_discovery_pins"));
                assert!(saved.contains("api_base_url"), "{saved}");
                assert!(saved.contains("discovery_sha256"), "{saved}");
                assert!(saved.contains("sequence = 7"), "{saved}");
                assert!(saved.contains("sequence_anchor = 5"), "{saved}");
                assert!(!saved.contains("legacy_certificate_identity"), "{saved}");
                assert!(
                    saved.contains("signature_state = \"required_unpinned\""),
                    "{saved}"
                );
            }
            "re-bootstrap" => {
                assert!(saved.contains("migration_pending_discovery_pins"));
                assert!(saved.contains("api_base_url"), "{saved}");
                assert!(saved.contains("discovery_sha256"), "{saved}");
                assert!(saved.contains("sequence_present = false"), "{saved}");
                assert!(
                    saved.contains("sequence_anchor_established = false"),
                    "{saved}"
                );
                assert!(
                    saved.contains("sequence_anchor_reset_pending = true"),
                    "{saved}"
                );
                assert!(saved.contains("signature_state = \"required_legacy\""));
                assert!(saved.contains("legacy_certificate_identity = \"identity-a\""));
            }
            _ => unreachable!(),
        }

        if operation != "repin-discovery" {
            let install_lock = ProviderInstallLock::acquire(dir.path()).unwrap();
            let error = resolve_all(
                &carina_core::UserDirs::under(&dir.path().join("test-user")),
                &install_lock,
                &[registry_provider_config()],
                LockMode::Normal,
            )
            .expect_err("provider-level recovery must not authorize discovery migration");
            assert!(
                error
                    .to_string()
                    .contains("lock-format migration requires operator authorization"),
                "{operation}: {error}"
            );
            assert!(
                error
                    .to_string()
                    .contains("carina providers repin-discovery registry.carina-rs.dev"),
                "{operation}: {error}"
            );
        }
    }
}

#[test]
fn v1_lock_migration_keeps_discovery_repin_reachable() {
    let dir = tempfile::tempdir().unwrap();
    write_v1_protected_lock(dir.path());

    let output = carina_target(dir.path(), "repin-discovery", HOST, true, "");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let header = "Provider lock format migration: version 1 -> version 5";
    assert_eq!(stderr.matches(header).count(), 1, "{stderr}");
    assert!(
        stderr.contains("Discarding v1 pinned discovery API base"),
        "{stderr}"
    );

    let saved = fs::read_to_string(dir.path().join("carina-providers.lock")).unwrap();
    assert!(saved.starts_with("version = 5\n"), "{saved}");
    assert!(!saved.contains("migration_pending_discovery_pin"));
    assert!(saved.contains("sequence = 7"), "{saved}");
    assert!(saved.contains("signature_state = \"required_legacy\""));
}

#[test]
fn v1_discovery_repin_previews_and_clears_every_pending_pin_for_the_host() {
    let dir = tempfile::tempdir().unwrap();
    write_two_pin_same_host_v1_lock(dir.path());

    let output = carina_target(dir.path(), "repin-discovery", HOST, true, "");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr
            .matches("Provider lock format migration: version 1 -> version 5")
            .count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("legacy-discovery-sha256"), "{stderr}");
    assert!(stderr.contains("newer-discovery-sha256"), "{stderr}");
    assert_eq!(
        stderr
            .matches("Discarding v1 discovery document SHA-256")
            .count(),
        2,
        "{stderr}"
    );

    let saved = fs::read_to_string(dir.path().join("carina-providers.lock")).unwrap();
    assert!(saved.starts_with("version = 5\n"), "{saved}");
    assert!(
        !saved.contains("migration_pending_discovery_pin"),
        "{saved}"
    );
    assert!(saved.contains("source = \"carina-rs/aws\""), "{saved}");
    assert!(saved.contains("source = \"carina-rs/random\""), "{saved}");
}

#[test]
fn v1_discovery_repin_preview_folds_the_complete_migration_report() {
    let dir = tempfile::tempdir().unwrap();
    write_two_host_v1_lock(dir.path());

    let output = carina_target(dir.path(), "repin-discovery", HOST, true, "");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let header = "Provider lock format migration: version 1 -> version 5";
    assert_eq!(stderr.matches(header).count(), 1, "{stderr}");
    assert!(
        stderr.contains(
            "Discarding v1 pinned discovery API base: https://registry.carina-rs.dev/v1/providers/"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "Retaining v1 pinned discovery API base pending authorization: https://registry.example.test/v1/providers/"
        ),
        "the folded report must retain the other host's pending state: {stderr}"
    );
    assert!(
        !stderr.contains(
            "Retaining v1 pinned discovery API base pending authorization: https://registry.carina-rs.dev/v1/providers/"
        ),
        "the target must be described only as discarded: {stderr}"
    );
    assert!(
        stderr.contains(
            "registry discovery for host registry.example.test is blocked because its legacy lock-format migration requires operator authorization"
        ),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Registry resolution remains blocked for each other migrated host"),
        "{stderr}"
    );

    let saved = fs::read_to_string(dir.path().join("carina-providers.lock")).unwrap();
    assert_eq!(saved.matches("migration_pending_discovery_pins").count(), 1);
    assert!(saved.contains("other-host-discovery-sha256"), "{saved}");
    assert!(!saved.contains("default-host-discovery-sha256"), "{saved}");
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
    let header = "Provider lock format migration: version 2 -> version 5";
    assert!(stderr.contains(header), "{stderr}");
    let repin_command =
        ProjectCommand::new("providers repin-discovery", dir.path()).with_argument(HOST);
    assert!(stderr.contains(&format!("`{repin_command}`")), "{stderr}");
    let lock_path = dir.path().join("carina-providers.lock");
    assert!(
        stderr.contains(&format!("Lock file: {}", lock_path.display())),
        "{stderr}"
    );
    assert_eq!(
        stderr.matches(header).count(),
        1,
        "carina init must report migration exactly once: {stderr}"
    );
    assert_eq!(
        stderr
            .matches("lock-format migration requires operator authorization")
            .count(),
        2,
        "the migration report and the resolve-time refusal must each name the authorization gate: {stderr}"
    );
}

#[test]
fn init_reports_v1_lock_migration_once_before_authorization_error() {
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
    write_v1_protected_lock(dir.path());

    let output = carina_init(dir.path());
    assert!(
        !output.status.success(),
        "migration-pending discovery must remain blocked"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    let header = "Provider lock format migration: version 1 -> version 5";
    assert!(stderr.contains(header), "{stderr}");
    assert_eq!(
        stderr.matches(header).count(),
        1,
        "carina init must report migration exactly once: {stderr}"
    );
    assert_eq!(
        stderr
            .matches("lock-format migration requires operator authorization")
            .count(),
        2,
        "the migration report and the resolve-time refusal must each name the authorization gate: {stderr}"
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

    let install_lock = ProviderInstallLock::acquire(dir.path()).unwrap();
    let error = resolve_all(
        &carina_core::UserDirs::under(&dir.path().join("test-user")),
        &install_lock,
        &[registry_provider_config()],
        LockMode::Normal,
    )
    .expect_err("declining must leave migrated discovery resolution blocked");
    assert!(
        error
            .to_string()
            .contains("lock-format migration requires operator authorization"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains("carina providers repin-discovery registry.carina-rs.dev"),
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
    carina_provider_resolver::LockFile::load(dir.path())
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
        saved.contains("legacy_certificate_identity = \"identity-a\""),
        "{saved}"
    );
    assert!(
        saved.contains("legacy_certificate_oidc_issuer = \"issuer-a\""),
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
        stderr.contains("Discarding legacy certificate identity: identity-a"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Discarding legacy OIDC issuer: issuer-a"),
        "{stderr}"
    );
    assert!(!stdout.contains("Discarding"), "{stdout}");
    assert!(!stdout.contains("Enter provider source"), "{stdout}");

    let lock_path = dir.path().join("carina-providers.lock");
    carina_provider_resolver::LockFile::load(dir.path())
        .expect("repinned lock must remain parseable")
        .expect("repinned lock must remain present");
    let saved = fs::read_to_string(lock_path).unwrap();
    assert!(saved.contains("sha256 = \"pinned-shasum\""), "{saved}");
    assert!(saved.contains("sequence = 7"), "{saved}");
    assert!(saved.contains("sequence_anchor = 5"), "{saved}");
    assert!(saved.contains("yanked_versions = [\"0.4.0\"]"), "{saved}");
    assert!(
        saved.contains("signature_state = \"required_unpinned\""),
        "{saved}"
    );
    assert!(!saved.contains("legacy_certificate_identity"), "{saved}");
    assert!(!saved.contains("legacy_certificate_oidc_issuer"), "{saved}");
    assert!(saved.contains("transparency_log_present = true"), "{saved}");
}

#[test]
fn repin_identity_prints_and_clears_every_current_signer_pin() {
    let dir = tempfile::tempdir().unwrap();
    write_current_signer_lock(dir.path());

    let output = carina(dir.path(), "repin-identity", true, "");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    for expected in [
        "Discarding source OIDC issuer: https://token.actions.githubusercontent.com",
        "Discarding source repository owner ID: 260157389",
        "Discarding source repository ID: 1199024834",
        "Discarding stable workflow: https://example.test/release.yml",
        "Discarding pre-release workflow: https://example.test/ci.yml",
    ] {
        assert!(stderr.contains(expected), "{stderr}");
    }

    let saved = fs::read_to_string(dir.path().join("carina-providers.lock")).unwrap();
    assert!(
        saved.contains("signature_state = \"required_unpinned\""),
        "{saved}"
    );
    for removed in [
        "source_oidc_issuer",
        "source_repository_owner_id",
        "source_repository_id",
        "stable_workflow",
        "prerelease_workflow",
    ] {
        assert!(!saved.contains(removed), "{removed} survived:\n{saved}");
    }
    assert!(saved.contains("sequence = 7"), "{saved}");
    assert!(saved.contains("yanked_versions = [\"0.4.0\"]"), "{saved}");
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
    carina_provider_resolver::LockFile::load(dir.path())
        .expect("re-bootstrapped lock must remain parseable")
        .expect("re-bootstrapped lock must remain present");
    let saved = fs::read_to_string(lock_path).unwrap();
    assert!(saved.contains("sha256 = \"pinned-shasum\""), "{saved}");
    assert!(saved.contains("sequence_present = false"), "{saved}");
    assert!(
        saved.contains("sequence_anchor_established = false"),
        "{saved}"
    );
    assert!(
        saved.contains("sequence_anchor_reset_pending = true"),
        "{saved}"
    );
    assert!(!saved.contains("sequence = "), "{saved}");
    assert!(!saved.contains("sequence_anchor = "), "{saved}");
    assert!(saved.contains("yanked_versions = [\"0.4.0\"]"), "{saved}");
    assert!(
        saved.contains("signature_state = \"required_legacy\""),
        "{saved}"
    );
    assert!(
        saved.contains("legacy_certificate_identity = \"identity-a\""),
        "{saved}"
    );
    assert!(
        saved.contains("legacy_certificate_oidc_issuer = \"issuer-a\""),
        "{saved}"
    );
    assert!(saved.contains("transparency_log_present = true"), "{saved}");
}

#[test]
fn first_contact_prints_registry_sequence_anchor_notice_once() {
    const CHILD_ENV: &str = "CARINA_TEST_FIRST_CONTACT_SEQUENCE_ANCHOR_NOTICE";
    const TEST_NAME: &str = "first_contact_prints_registry_sequence_anchor_notice_once";

    if std::env::var_os(CHILD_ENV).is_some() {
        let project = tempfile::tempdir().unwrap();
        let user_root = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(user_root.path());
        let http = registry_fixture(Some(11));

        resolve_with_fixture(project.path(), &user_dirs, &http);
        resolve_with_fixture(project.path(), &user_dirs, &http);

        let saved = fs::read_to_string(project.path().join("carina-providers.lock")).unwrap();
        assert!(saved.contains("sequence_anchor = 11"), "{saved}");
        return;
    }

    let output = run_child_test(TEST_NAME, CHILD_ENV);
    assert!(output.status.success(), "child test failed: {output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let notice = "carina: established registry sequence anchor for carina-rs/aws: 11";
    assert_eq!(stderr.matches(notice).count(), 1, "{stderr}");
}

#[test]
fn rebootstrap_next_resolution_prints_registry_sequence_anchor_notice_once() {
    const CHILD_ENV: &str = "CARINA_TEST_REBOOTSTRAP_SEQUENCE_ANCHOR_NOTICE";
    const TEST_NAME: &str =
        "rebootstrap_next_resolution_prints_registry_sequence_anchor_notice_once";

    if std::env::var_os(CHILD_ENV).is_some() {
        let project = tempfile::tempdir().unwrap();
        write_unsigned_anchored_lock(project.path(), 5);
        let recovery = carina(project.path(), "re-bootstrap", true, "");
        assert!(
            recovery.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&recovery.stdout),
            String::from_utf8_lossy(&recovery.stderr)
        );

        let user_root = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(user_root.path());
        let http = registry_fixture(Some(11));
        resolve_with_fixture(project.path(), &user_dirs, &http);
        resolve_with_fixture(project.path(), &user_dirs, &http);

        let saved = fs::read_to_string(project.path().join("carina-providers.lock")).unwrap();
        assert!(saved.contains("sequence_anchor = 11"), "{saved}");
        return;
    }

    let output = run_child_test(TEST_NAME, CHILD_ENV);
    assert!(output.status.success(), "child test failed: {output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let notice = "carina: established registry sequence anchor for carina-rs/aws: 11";
    assert_eq!(stderr.matches(notice).count(), 1, "{stderr}");
}

#[test]
fn rebootstrap_sequence_less_listing_prints_no_reference_point_notice() {
    const CHILD_ENV: &str = "CARINA_TEST_REBOOTSTRAP_NO_SEQUENCE_NOTICE";
    const TEST_NAME: &str = "rebootstrap_sequence_less_listing_prints_no_reference_point_notice";

    if std::env::var_os(CHILD_ENV).is_some() {
        let project = tempfile::tempdir().unwrap();
        write_unsigned_anchored_lock(project.path(), 5);
        let recovery = carina(project.path(), "re-bootstrap", true, "");
        assert!(
            recovery.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&recovery.stdout),
            String::from_utf8_lossy(&recovery.stderr)
        );

        let user_root = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(user_root.path());
        let http = registry_fixture(None);
        resolve_with_fixture(project.path(), &user_dirs, &http);
        resolve_with_fixture(project.path(), &user_dirs, &http);

        let saved = fs::read_to_string(project.path().join("carina-providers.lock")).unwrap();
        assert!(
            saved.contains("sequence_anchor_established = false"),
            "{saved}"
        );
        return;
    }

    let output = run_child_test(TEST_NAME, CHILD_ENV);
    assert!(output.status.success(), "child test failed: {output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let notice = "carina: registry listing for carina-rs/aws has no sequence; no reference point established";
    assert_eq!(stderr.matches(notice).count(), 1, "{stderr}");
    assert!(
        !stderr.contains("carina: established registry sequence anchor"),
        "{stderr}"
    );
}

#[test]
fn sequence_less_registry_first_contact_prints_no_reference_point_notice_once() {
    const CHILD_ENV: &str = "CARINA_TEST_FIRST_CONTACT_NO_SEQUENCE_NOTICE";
    const TEST_NAME: &str =
        "sequence_less_registry_first_contact_prints_no_reference_point_notice_once";

    if std::env::var_os(CHILD_ENV).is_some() {
        let project = tempfile::tempdir().unwrap();
        let user_root = tempfile::tempdir().unwrap();
        let user_dirs = UserDirs::under(user_root.path());
        let http = registry_fixture(None);

        for _ in 0..3 {
            resolve_with_fixture(project.path(), &user_dirs, &http);
        }

        let saved = fs::read_to_string(project.path().join("carina-providers.lock")).unwrap();
        assert!(
            saved.contains("sequence_anchor_established = false"),
            "{saved}"
        );
        assert!(!saved.contains("sequence_anchor_reset_pending"), "{saved}");
        return;
    }

    let output = run_child_test(TEST_NAME, CHILD_ENV);
    assert!(output.status.success(), "child test failed: {output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let notice = "carina: registry listing for carina-rs/aws has no sequence; no reference point established";
    assert_eq!(stderr.matches(notice).count(), 1, "{stderr}");
    assert!(
        !stderr.contains("carina: established registry sequence anchor"),
        "{stderr}"
    );
}

#[test]
fn sequence_less_first_contact_with_persisted_observations_prints_notice_once() {
    const CHILD_ENV: &str = "CARINA_TEST_FIRST_CONTACT_OBSERVATION_NOTICE";
    const TEST_NAME: &str =
        "sequence_less_first_contact_with_persisted_observations_prints_notice_once";

    if std::env::var_os(CHILD_ENV).is_some() {
        for http in [
            registry_fixture_with_observations(None, Some("2999-01-01T00:00:00Z"), false),
            registry_fixture_with_observations(None, None, true),
        ] {
            let project = tempfile::tempdir().unwrap();
            let user_root = tempfile::tempdir().unwrap();
            let user_dirs = UserDirs::under(user_root.path());

            // No version constraint makes resolution validate the listing
            // during selection and again before pinning the artifact.
            resolve_with_fixture(project.path(), &user_dirs, &http);
            assert_eq!(
                http.requested_urls()
                    .iter()
                    .filter(|url| url.ends_with("/carina-rs/aws/versions"))
                    .count(),
                2,
                "first contact must exercise the double-fetch path"
            );
            resolve_with_fixture(project.path(), &user_dirs, &http);
        }
        return;
    }

    let output = run_child_test(TEST_NAME, CHILD_ENV);
    assert!(output.status.success(), "child test failed: {output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let notice = "carina: registry listing for carina-rs/aws has no sequence; no reference point established";
    assert_eq!(
        stderr.matches(notice).count(),
        2,
        "valid_until and yanked first contacts must each announce exactly once: {stderr}"
    );
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
            "repin-identity" => assert!(
                saved.contains("signature_state = \"required_unpinned\""),
                "{saved}"
            ),
            "re-bootstrap" => assert!(
                saved.contains("sequence_anchor_established = false"),
                "{saved}"
            ),
            _ => unreachable!(),
        }
    }
}
