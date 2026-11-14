use std::path::Path;
use std::process::Command;

const SHARDS: usize = 4;

fn check_suggestion_fixture(name: &str) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("carina-core/tests/fixtures/diagnostic_order")
        .join(name);
    for _ in 0..8 {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .arg("validate")
            .arg(&directory)
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Did you mean `bat`?"), "{stderr}");
    }
}

#[test]
fn undefined_identifier_suggestion_ties_are_stable() {
    check_suggestion_fixture("suggestion_identifier");
}

#[test]
fn upstream_export_suggestion_ties_are_stable() {
    check_suggestion_fixture("suggestion_upstream");
}

#[test]
fn duplicate_sibling_functions_are_rejected() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("carina-core/tests/fixtures/diagnostic_order/functions/duplicates");
    let output = Command::new(env!("CARGO_BIN_EXE_carina"))
        .arg("validate")
        .arg(directory)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["errors"].as_array().unwrap().len(), 1);
    assert_eq!(
        result["errors"][0]["message"],
        "duplicate function 'zebra': declared in a_functions.crn and z_functions.crn"
    );
}

fn check_shard(index: usize) {
    with_evidence(index, |evidence| check_shard_in(index, evidence));
}

fn with_evidence(index: usize, check: impl FnOnce(&Path)) {
    let evidence = tempfile::Builder::new()
        .prefix(&format!("validate-parity-shard-{index}-"))
        .tempdir()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(evidence.path())));
    if let Err(panic) = result {
        eprintln!(
            "failed diagnostic-order sweep evidence: {}",
            evidence.keep().display()
        );
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn sweep_evidence_is_deleted_on_success_and_retained_on_failure() {
    let success = std::cell::RefCell::new(std::path::PathBuf::new());
    with_evidence(0, |directory| *success.borrow_mut() = directory.to_owned());
    assert!(
        !success.borrow().exists(),
        "successful sweep left {}",
        success.borrow().display()
    );
    let failure = std::cell::RefCell::new(std::path::PathBuf::new());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        with_evidence(0, |directory| {
            *failure.borrow_mut() = directory.to_owned();
            panic!("failed sweep");
        });
    }));
    assert!(result.is_err());
    assert!(failure.borrow().exists());
    std::fs::remove_dir_all(&*failure.borrow()).unwrap();
}

fn check_shard_in(index: usize, evidence: &Path) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let script = repo.join("scripts/validate-parity.py");
    let command = || {
        let mut command = Command::new("python3");
        command
            .arg(&script)
            .args([
                "--binary",
                env!("CARGO_BIN_EXE_carina"),
                "--repeat",
                "8",
                "--repo",
            ])
            .arg(repo)
            .arg("--evidence")
            .arg(evidence);
        command
    };
    // Python owns enumeration and validates the complete manifest and shard coverage.
    let listed = command().arg("--list-only").output().unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let output = command()
        .arg("--directory-manifest")
        .arg(evidence.join("directories.json"))
        .arg("--shard")
        .arg(format!("{index}/{SHARDS}"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "diagnostic order shard {index}/{SHARDS}; evidence: {}\nstdout:\n{}\nstderr:\n{}",
        evidence.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let summary: serde_json::Value =
        serde_json::from_slice(&std::fs::read(evidence.join("summary.json")).unwrap()).unwrap();
    // Every shard must exercise validation, excluding stable provider/setup failures.
    assert!(
        summary["validation_directories"].as_u64().unwrap() > 0,
        "shard {index}/{SHARDS} reached no validation: {summary}"
    );
    let validated = summary["validated_directory_names"].as_array().unwrap();
    for fixture in summary["diagnostic_order_directory_names"]
        .as_array()
        .unwrap()
    {
        assert!(
            validated.contains(fixture),
            "focused fixture {fixture} did not reach validation: {summary}"
        );
    }
    eprintln!(
        "shard {index}/{SHARDS}; {}",
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .last()
            .unwrap_or_default(),
    );
}

macro_rules! diagnostic_order_shards {
    ($($name:ident: $index:literal),+ $(,)?) => {
        const _: () = {
            let indices = [$($index),+];
            assert!(indices.len() == SHARDS);
            let mut index = 0;
            while index < SHARDS {
                assert!(indices[index] == index);
                index += 1;
            }
        };
        $(#[test] fn $name() { check_shard($index); })+
    };
}

diagnostic_order_shards! {
    validate_diagnostic_order_shard_0: 0,
    validate_diagnostic_order_shard_1: 1,
    validate_diagnostic_order_shard_2: 2,
    validate_diagnostic_order_shard_3: 3,
}

#[test]
fn validation_sweep_coverage_accounting() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("scripts/test-validate-parity.py");
    let output = Command::new("python3").arg(script).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn duplicate_function_signatures_keep_the_first_declaration() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("carina-core/tests/fixtures/diagnostic_order/functions/duplicate_signatures");
    for _ in 0..16 {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .arg("validate")
            .arg(&directory)
            .arg("--json")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stderr.is_empty(), "{output:?}");
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            result,
            serde_json::json!({"status": "error", "resource_count": 0, "resources": [], "errors": [{
                "message": "duplicate function 'zebra': declared in a_functions.crn and z_functions.crn",
                "severity": "error", "code": null, "location": null
            }]})
        );
    }
}
