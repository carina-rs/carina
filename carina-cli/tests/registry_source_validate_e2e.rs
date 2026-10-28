use std::fs;
use std::process::Command;

use carina_core::provider_source::ProviderSource;
use carina_provider_resolver::{LockEntry, LockEntryKind, LockFile, cache_path_wasm};

#[test]
fn directory_validate_loads_registry_source_from_lock_and_cache() {
    let project = tempfile::tempdir().expect("create temporary project");
    let source = "carina-rs/awscc";
    let version = "0.5.0";

    fs::write(
        project.path().join("providers.crn"),
        format!("provider awscc {{\n  source = '{source}'\n  version = '{version}'\n}}\n"),
    )
    .expect("write providers.crn");
    fs::write(project.path().join("main.crn"), "").expect("write main.crn");

    let parsed_source: ProviderSource = source.parse().unwrap();
    let ProviderSource::Remote(remote_source) = parsed_source else {
        panic!("registry source must parse as remote")
    };
    let artifact = cache_path_wasm(project.path(), &remote_source.source_key(), version);
    fs::create_dir_all(artifact.parent().unwrap()).expect("create provider cache");
    fs::write(&artifact, b"not a wasm component").expect("write invalid WASM fixture");

    let mut lock = LockFile::default();
    lock.upsert(LockEntry {
        name: "awscc".into(),
        source: source.into(),
        kind: LockEntryKind::Version {
            version: version.into(),
            constraint: Some(version.into()),
        },
        sha256: carina_provider_resolver::sha256_file(&artifact).unwrap(),
        registry: None,
    });
    lock.save(&project.path().join("carina-providers.lock"))
        .expect("write provider lock");

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", "."])
        .current_dir(project.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run carina validate");
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.status.success(),
        "invalid WASM must fail validation: {rendered}"
    );
    assert!(
        rendered.contains("Failed to load WASM component"),
        "{rendered}"
    );
    assert!(rendered.contains("carina-providers.lock"), "{rendered}");
    assert!(!rendered.contains("Invalid source format"), "{rendered}");
}

#[test]
fn directory_validate_rejects_artifact_that_mismatches_lock_sha256() {
    let project = tempfile::tempdir().expect("create temporary project");
    let source = "carina-rs/awscc";
    let version = "0.5.0";
    let expected_sha256 = "pinned-sha256";

    fs::write(
        project.path().join("providers.crn"),
        format!("provider awscc {{\n  source = '{source}'\n  version = '{version}'\n}}\n"),
    )
    .expect("write providers.crn");
    fs::write(project.path().join("main.crn"), "").expect("write main.crn");

    let parsed_source: ProviderSource = source.parse().unwrap();
    let ProviderSource::Remote(remote_source) = parsed_source else {
        panic!("registry source must parse as remote")
    };
    let artifact = cache_path_wasm(project.path(), &remote_source.source_key(), version);
    fs::create_dir_all(artifact.parent().unwrap()).expect("create provider cache");
    fs::write(&artifact, b"mismatched provider bytes").expect("write provider artifact");
    let actual_sha256 = carina_provider_resolver::sha256_file(&artifact).unwrap();

    let mut lock = LockFile::default();
    lock.upsert(LockEntry {
        name: "awscc".into(),
        source: source.into(),
        kind: LockEntryKind::Version {
            version: version.into(),
            constraint: Some(version.into()),
        },
        sha256: expected_sha256.into(),
        registry: None,
    });
    lock.save(&project.path().join("carina-providers.lock"))
        .expect("write provider lock");

    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", "."])
        .current_dir(project.path())
        .env("NO_COLOR", "1")
        .output()
        .expect("run carina validate");
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !output.status.success(),
        "a mismatched provider must fail validation: {rendered}"
    );
    assert!(
        rendered.contains(&format!(
            "Provider 'awscc' installed artifact {source}@{version} does not match carina-providers.lock (expected {expected_sha256}, got {actual_sha256})."
        )),
        "{rendered}"
    );
    assert!(
        rendered.contains(
            "Run `carina init` to discard the mismatched copy, then run it again to download a verified one."
        ),
        "{rendered}"
    );
    assert!(
        artifact.exists(),
        "validate must not remove or replace the mismatched artifact"
    );
}
