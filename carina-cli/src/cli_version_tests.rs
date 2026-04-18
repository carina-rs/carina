use clap::Parser;

use crate::Cli;

fn version_output() -> String {
    let err = match Cli::try_parse_from(["carina", "--version"]) {
        Err(e) => e,
        Ok(_) => panic!("Expected --version to produce an error (DisplayVersion), got Ok"),
    };
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    err.to_string()
}

#[test]
fn cli_version_flag_succeeds() {
    // --version should cause clap to print version and exit (Err with DisplayVersion)
    let err = match Cli::try_parse_from(["carina", "--version"]) {
        Err(e) => e,
        Ok(_) => panic!("Expected --version to produce an error (DisplayVersion), got Ok"),
    };
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
}

#[test]
fn cli_version_contains_package_version() {
    let output = version_output();
    assert!(
        output.contains(env!("CARGO_PKG_VERSION")),
        "Expected version output to contain '{}', got: {}",
        env!("CARGO_PKG_VERSION"),
        output
    );
}

#[test]
fn cli_version_contains_build_date() {
    // build.rs always sets CARINA_BUILD_DATE; it must appear in the
    // output when the build has git context (the normal dev case).
    let date = env!("CARINA_BUILD_DATE");
    let git_hash = env!("CARINA_GIT_HASH");
    if git_hash.is_empty() {
        // Non-git build (e.g. `cargo install` from crates.io): version
        // string is the bare package version. Nothing extra to assert.
        return;
    }
    let output = version_output();
    assert!(
        output.contains(date),
        "Expected version output to contain build date '{}', got: {}",
        date,
        output
    );
    assert!(
        output.contains(git_hash),
        "Expected version output to contain git hash '{}', got: {}",
        git_hash,
        output
    );
}

#[test]
fn cli_version_format_is_paren_wrapped_when_git_context_available() {
    // `carina 0.4.0 (abcdefg 2026-04-18)` or
    // `carina 0.4.0 (abcdefg-dirty 2026-04-18)`.
    let git_hash = env!("CARINA_GIT_HASH");
    if git_hash.is_empty() {
        return;
    }
    let output = version_output();
    assert!(
        output.contains('(') && output.contains(')'),
        "Expected parenthesized metadata, got: {}",
        output
    );
}
