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

const PROVIDER: &str = "registry.carina-rs.dev/carina-rs/aws";
const HOST: &str = "registry.carina-rs.dev";
const FIXTURE_VERSION: &str = "0.5.0";
const FIXTURE_ARTIFACT_URL: &str = "https://downloads.example.test/aws.wasm";
const FIXTURE_ARTIFACT: &[u8] = b"registry sequence notice fixture";

fn write_protected_lock(base_dir: &Path) {
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
signature_state = "required_current"
source_oidc_issuer = "issuer-a"
source_repository_owner_id = "owner-a"
source_repository_id = "identity-a"
stable_workflow = "release.yml"
transparency_log_present = true
"#,
    )
    .unwrap();
}

fn write_pending_discovery_lock(base_dir: &Path) {
    write_protected_lock(base_dir);
    let path = base_dir.join("carina-providers.lock");
    let lock = fs::read_to_string(&path).unwrap();
    let pinned = r#"[registry_host."registry.carina-rs.dev"]
discovery_pin_present = true

[registry_host."registry.carina-rs.dev".discovery_values]
"providers.v1" = "https://registry.carina-rs.dev/v1/providers/"
"#;
    let pending = r#"[registry_host."registry.carina-rs.dev"]
discovery_pin_present = false

[[registry_host."registry.carina-rs.dev".migration_pending_discovery_pins]]
api_base_url = "https://registry.carina-rs.dev/v1/providers/"
discovery_sha256 = "legacy-discovery-sha256"
"#;
    assert_eq!(lock.matches(pinned).count(), 1);
    fs::write(path, lock.replacen(pinned, pending, 1)).unwrap();
}

fn write_two_pin_same_host_pending_lock(base_dir: &Path) {
    write_pending_discovery_lock(base_dir);
    let path = base_dir.join("carina-providers.lock");
    let lock = fs::read_to_string(&path).unwrap();
    let first = r#"[[registry_host."registry.carina-rs.dev".migration_pending_discovery_pins]]
api_base_url = "https://registry.carina-rs.dev/v1/providers/"
discovery_sha256 = "legacy-discovery-sha256"
"#;
    let second = r#"
[[registry_host."registry.carina-rs.dev".migration_pending_discovery_pins]]
api_base_url = "https://registry.carina-rs.dev/v1/providers/"
discovery_sha256 = "newer-discovery-sha256"
"#;
    assert_eq!(lock.matches(first).count(), 1);
    let mut lock = lock.replacen(first, &format!("{first}{second}"), 1);
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
sequence_present = false
sequence_anchor_established = false
valid_until_present = false
signature_state = "not_required"
transparency_log_present = false
"#,
    );
    fs::write(path, lock).unwrap();
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
        unresolved_attributes: Default::default(),
        binding: None,
        attributes: Default::default(),
        default_tags: Default::default(),
    }
}

fn fixture_artifact_sha256() -> String {
    let artifact = tempfile::NamedTempFile::new().unwrap();
    fs::write(artifact.path(), FIXTURE_ARTIFACT).unwrap();
    sha256_file(&carina_core::paths::ResolvedPath::new(artifact.path()).unwrap()).unwrap()
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
    let install_lock =
        ProviderInstallLock::acquire(&carina_core::paths::ResolvedPath::new(base_dir).unwrap())
            .unwrap();
    resolve_all_with_fixture_registry(
        user_dirs,
        &install_lock,
        &[registry_provider_config()],
        LockMode::Normal,
        http,
        &mut |_| {},
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
fn pending_discovery_repin_previews_and_clears_every_pending_pin_for_the_host() {
    let dir = tempfile::tempdir().unwrap();
    write_two_pin_same_host_pending_lock(dir.path());

    let output = carina_target(dir.path(), "repin-discovery", HOST, true, "");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("legacy-discovery-sha256"), "{stderr}");
    assert!(stderr.contains("newer-discovery-sha256"), "{stderr}");
    assert_eq!(
        stderr
            .matches("Discarding migrated legacy discovery document SHA-256")
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
fn pending_discovery_repin_preserves_other_hosts_pending_pins() {
    const OTHER_HOST: &str = "other.registry.example.test";
    let dir = tempfile::tempdir().unwrap();
    write_pending_discovery_lock(dir.path());
    let path = dir.path().join("carina-providers.lock");
    let mut fixture = fs::read_to_string(&path).unwrap();
    fixture.push_str(
        r#"
[registry_host."other.registry.example.test"]
discovery_pin_present = false

[[registry_host."other.registry.example.test".migration_pending_discovery_pins]]
api_base_url = "https://other.registry.example.test/v1/providers/"
discovery_sha256 = "other-host-pending-sha256"
"#,
    );
    fs::write(&path, &fixture).unwrap();
    let before: toml::Value = toml::from_str(&fixture).unwrap();
    let output = carina_target(dir.path(), "repin-discovery", HOST, true, "");
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("legacy-discovery-sha256"), "{stderr}");
    assert!(!stderr.contains("other-host-pending-sha256"), "{stderr}");
    carina_provider_resolver::LockFile::load(
        &carina_core::paths::ResolvedPath::new(dir.path()).unwrap(),
    )
    .unwrap()
    .unwrap();
    let saved: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        saved["registry_host"][OTHER_HOST],
        before["registry_host"][OTHER_HOST]
    );
    assert!(
        saved["registry_host"][HOST]
            .get("migration_pending_discovery_pins")
            .is_none()
    );
    assert_eq!(
        saved["registry_host"][HOST]["discovery_pin_present"].as_bool(),
        Some(false)
    );
    assert_eq!(saved["provider"], before["provider"]);
}

#[test]
fn provider_recovery_on_pending_host_keeps_discovery_blocked() {
    for operation in ["repin-identity", "re-bootstrap"] {
        let dir = tempfile::tempdir().unwrap();
        write_pending_discovery_lock(dir.path());
        let path = dir.path().join("carina-providers.lock");
        let before: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let output = carina(dir.path(), operation, true, "");
        assert!(output.status.success(), "{operation}: {output:?}");
        let saved: toml::Value = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["version"].as_integer(), Some(5));
        assert_eq!(saved["registry_host"], before["registry_host"]);
        assert_eq!(
            saved["registry_host"][HOST]["migration_pending_discovery_pins"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let mut expected = before["provider"].clone();
        let registry = expected[0]["registry"].as_table_mut().unwrap();
        match operation {
            "repin-identity" => {
                registry.insert("signature_state".into(), "required_unpinned".into());
                for key in [
                    "source_oidc_issuer",
                    "source_repository_owner_id",
                    "source_repository_id",
                    "stable_workflow",
                ] {
                    registry.remove(key);
                }
            }
            "re-bootstrap" => {
                registry.insert("sequence_present".into(), false.into());
                registry.insert("sequence_anchor_established".into(), false.into());
                registry.insert("sequence_anchor_reset_pending".into(), true.into());
                registry.remove("sequence");
                registry.remove("sequence_anchor");
            }
            _ => unreachable!(),
        }
        assert_eq!(saved["provider"], expected, "{operation}");
        let install_lock = ProviderInstallLock::acquire(
            &carina_core::paths::ResolvedPath::new(dir.path()).unwrap(),
        )
        .unwrap();
        let http = FixtureRegistryHttp::default();
        let error = resolve_all_with_fixture_registry(
            &UserDirs::under(&dir.path().join("test-user")),
            &install_lock,
            &[registry_provider_config()],
            LockMode::Normal,
            &http,
            &mut |_| {},
        )
        .expect_err("provider-scoped recovery must leave discovery blocked");
        assert!(
            error
                .to_string()
                .contains("carina providers repin-discovery registry.carina-rs.dev"),
            "{operation}: {error}"
        );
        assert!(
            http.requested_urls().is_empty(),
            "{operation}: discovery must require authorization before HTTP"
        );
        assert_eq!(
            toml::from_str::<toml::Value>(&fs::read_to_string(&path).unwrap()).unwrap(),
            saved
        );
    }
}

#[test]
fn declining_migrated_discovery_repin_preserves_pin_and_resolution_block() {
    let dir = tempfile::tempdir().unwrap();
    write_pending_discovery_lock(dir.path());
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
        "declining must leave the pending pin intact on disk"
    );

    let install_lock =
        ProviderInstallLock::acquire(&carina_core::paths::ResolvedPath::new(dir.path()).unwrap())
            .unwrap();
    let error = resolve_all(
        &carina_core::UserDirs::under(&dir.path().join("test-user")),
        &install_lock,
        &[registry_provider_config()],
        LockMode::Normal,
        &mut |_| {},
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
    carina_provider_resolver::LockFile::load(
        &carina_core::paths::ResolvedPath::new(dir.path()).unwrap(),
    )
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
        saved.contains("source_repository_id = \"identity-a\""),
        "{saved}"
    );
    assert!(
        saved.contains("source_oidc_issuer = \"issuer-a\""),
        "{saved}"
    );
    assert!(
        saved.contains("source_repository_owner_id = \"owner-a\""),
        "{saved}"
    );
    assert!(
        saved.contains("stable_workflow = \"release.yml\""),
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
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stdout.contains("Discarding"), "{stdout}");
    assert!(!stdout.contains("Enter provider source"), "{stdout}");
    carina_provider_resolver::LockFile::load(
        &carina_core::paths::ResolvedPath::new(dir.path()).unwrap(),
    )
    .expect("repinned lock must remain parseable")
    .expect("repinned lock must remain present");
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
    assert!(saved.contains("sha256 = \"pinned-shasum\""), "{saved}");
    assert!(saved.contains("sequence_anchor = 5"), "{saved}");
    assert!(saved.contains("valid_until_present = true"), "{saved}");
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
    assert!(
        stdout.contains(&format!(
            "Registry freshness reset. Run `{}` to establish a new reference point (or record that the registry serves no sequence).",
            ProjectCommand::new("init", dir.path())
        )),
        "{stdout}"
    );

    let lock_path = dir.path().join("carina-providers.lock");
    carina_provider_resolver::LockFile::load(
        &carina_core::paths::ResolvedPath::new(dir.path()).unwrap(),
    )
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
        saved.contains("signature_state = \"required_current\""),
        "{saved}"
    );
    assert!(
        saved.contains("source_repository_id = \"identity-a\""),
        "{saved}"
    );
    assert!(
        saved.contains("source_oidc_issuer = \"issuer-a\""),
        "{saved}"
    );
    assert!(
        saved.contains("source_repository_owner_id = \"owner-a\""),
        "{saved}"
    );
    assert!(
        saved.contains("stable_workflow = \"release.yml\""),
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
