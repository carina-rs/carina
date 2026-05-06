//! Smoke test for `carina fmt` against real-world `.crn` files.
//!
//! #2117 repro lives in `fixtures/fmt_smoke/identity-center/` — verbatim
//! copies of `carina-rs/infra/aws/management/identity-center/`. If this
//! test fails, the formatter grammar has drifted away from something the
//! main parser accepts. Fix the formatter, don't modify the fixture.
//!
//! This test was added after a round of fixes that passed synthetic unit
//! tests but silently missed additional grammar gaps in the same file.
//! Running the formatter end-to-end over the actual source guards the
//! acceptance condition the original issue called out.

use std::fs;
use std::path::PathBuf;

use carina_core::formatter::{FormatConfig, format};

fn fixture_dir(name: &str) -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("tests/fixtures/fmt_smoke").join(name)
}

fn format_all_crn_files(dir: &PathBuf) -> Result<(), String> {
    let config = FormatConfig::default();
    let entries = fs::read_dir(dir).map_err(|e| format!("read_dir {}: {}", dir.display(), e))?;
    let mut failures: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "crn") {
            let source =
                fs::read_to_string(&path).map_err(|e| format!("read {}: {}", path.display(), e))?;
            if let Err(e) = format(&source, &config) {
                failures.push(format!(
                    "{}: {}",
                    path.file_name().unwrap().to_string_lossy(),
                    e
                ));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

#[test]
fn fmt_accepts_identity_center_fixture() {
    let dir = fixture_dir("identity-center");
    let result = format_all_crn_files(&dir);
    assert!(
        result.is_ok(),
        "`carina fmt` must parse every .crn file in {}. Failures:\n{}\n\nAdd the missing construct to `carina-core/src/formatter/carina_fmt.pest` rather than modifying the fixture.",
        dir.display(),
        result.unwrap_err()
    );
}

#[test]
fn fmt_idempotent_on_identity_center_fixture() {
    let dir = fixture_dir("identity-center");
    let config = FormatConfig::default();
    let entries = fs::read_dir(&dir).expect("read_dir");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "crn") {
            let source = fs::read_to_string(&path).expect("read file");
            let first = format(&source, &config)
                .unwrap_or_else(|e| panic!("{}: format failed: {}", path.display(), e));
            let second = format(&first, &config)
                .unwrap_or_else(|e| panic!("{}: second format failed: {}", path.display(), e));
            assert_eq!(
                first,
                second,
                "format must be idempotent on {}",
                path.display()
            );
        }
    }
}

// `github-oidc/` mirrors `carina-rs/infra/aws/management/github-oidc/` from
// issue #2504: `let X = module_call { ... }` is accepted by validate/plan
// but rejected by the formatter parser, so any infra layout that names a
// module instantiation (to expose its outputs via `exports.crn`) blocks
// CI fmt enforcement.
#[test]
fn fmt_accepts_github_oidc_fixture() {
    let dir = fixture_dir("github-oidc");
    let result = format_all_crn_files(&dir);
    assert!(
        result.is_ok(),
        "`carina fmt` must parse every .crn file in {}. Failures:\n{}",
        dir.display(),
        result.unwrap_err()
    );
}

#[test]
fn fmt_idempotent_on_github_oidc_fixture() {
    let dir = fixture_dir("github-oidc");
    let config = FormatConfig::default();
    let entries = fs::read_dir(&dir).expect("read_dir");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "crn") {
            let source = fs::read_to_string(&path).expect("read file");
            let first = format(&source, &config)
                .unwrap_or_else(|e| panic!("{}: format failed: {}", path.display(), e));
            let second = format(&first, &config)
                .unwrap_or_else(|e| panic!("{}: second format failed: {}", path.display(), e));
            assert_eq!(
                first,
                second,
                "format must be idempotent on {}",
                path.display()
            );
        }
    }
}
