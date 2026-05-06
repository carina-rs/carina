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

// `carina-state/` is the issue #2515 fixture: comments and blank lines
// between repeated `statement {}` siblings inside a `policy {}` map must
// survive `carina fmt`. Earlier the printer attached the leading-comment
// trivia to the previous sibling's closing brace and silently dropped it.
//
// The fixture mirrors the real `carina-rs/infra/aws/management/carina-state/`
// directory shape (main.crn + providers.crn + backend.crn) per the
// "Directory-scoped, never single-file" rule in CLAUDE.md.
#[test]
fn fmt_accepts_carina_state_fixture() {
    let dir = fixture_dir("carina-state");
    let result = format_all_crn_files(&dir);
    assert!(
        result.is_ok(),
        "`carina fmt` must parse every .crn file in {}. Failures:\n{}",
        dir.display(),
        result.unwrap_err()
    );
}

#[test]
fn fmt_idempotent_on_carina_state_fixture() {
    let dir = fixture_dir("carina-state");
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

#[test]
fn fmt_preserves_comments_in_carina_state_fixture() {
    let dir = fixture_dir("carina-state");
    let config = FormatConfig::default();
    let path = dir.join("main.crn");
    let source = fs::read_to_string(&path).expect("read main.crn");
    let formatted = format(&source, &config).expect("format must succeed");

    // The fixture itself must already be canonical so that future drift
    // is caught here rather than masked by an idempotent-but-different
    // round trip.
    assert_eq!(
        source, formatted,
        "fixture has drifted from canonical fmt — re-format and update the fixture"
    );

    // The four-line `# Carina's S3 backend ...` comment block sits
    // between the second and third `statement {}` siblings. Every line
    // must round-trip through `carina fmt`.
    for needle in [
        "# Carina's S3 backend opens any bucket without a configured region by",
        "# calling GetBucketLocation as part of its initialization. Granting it",
        "# cross-account is the only way upstream_state can reach this bucket",
        "# from outside the management account.",
    ] {
        assert!(
            formatted.contains(needle),
            "issue #2515 regression: comment dropped from formatted output:\n  missing: {needle}\n--- formatted ---\n{formatted}"
        );
    }

    let second = format(&formatted, &config).expect("second format must succeed");
    assert_eq!(
        formatted, second,
        "fmt must be idempotent on the carina-state fixture"
    );
}
