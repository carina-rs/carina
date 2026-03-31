use clap::Parser;

use crate::Cli;

#[test]
fn cli_version_flag_succeeds() {
    // --version should cause clap to print version and exit (Err with DisplayVersion)
    let result = Cli::try_parse_from(["carina", "--version"]);
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("Expected --version to produce an error (DisplayVersion), got Ok"),
    };
    assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
}

#[test]
fn cli_version_contains_package_version() {
    let result = Cli::try_parse_from(["carina", "--version"]);
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("Expected --version to produce an error (DisplayVersion), got Ok"),
    };
    let output = err.to_string();
    assert!(
        output.contains(env!("CARGO_PKG_VERSION")),
        "Expected version output to contain '{}', got: {}",
        env!("CARGO_PKG_VERSION"),
        output
    );
}
