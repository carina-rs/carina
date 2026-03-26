//! Snapshot tests for the `module list` subcommand.

use std::path::PathBuf;

use carina_core::config_loader;

use crate::commands::module::format_module_list;

#[test]
fn snapshot_module_list_with_imports() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixture_dir = PathBuf::from(format!("{}/tests/fixtures/module_list", manifest_dir));

    let config = config_loader::load_configuration(&fixture_dir).unwrap();
    let output = format_module_list(&config.parsed.imports);

    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_module_list_empty() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixture_dir = PathBuf::from(format!("{}/tests/fixtures/module_info", manifest_dir));

    let config = config_loader::load_configuration(&fixture_dir).unwrap();
    let output = format_module_list(&config.parsed.imports);

    insta::assert_snapshot!(output);
}
