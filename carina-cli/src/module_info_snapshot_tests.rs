//! Module info snapshot tests.
//!
//! Loads fixture module directories and snapshots the `module info` display output.
//! Each test corresponds to a subdirectory under `tests/fixtures/module_info/`.

use std::path::PathBuf;

use carina_core::module::FileSignature;
use carina_core::module_resolver;

/// Strip ANSI escape codes from a string for snapshot readability.
fn strip_ansi(s: &str) -> String {
    let re = regex_lite::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(s, "").to_string()
}

/// Helper: load a module info fixture directory and return the stripped display output.
fn module_info_output(fixture_name: &str) -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let module_dir = PathBuf::from(format!(
        "{}/tests/fixtures/module_info/{}",
        manifest_dir, fixture_name
    ));

    let parsed = module_resolver::load_module_from_directory(&module_dir)
        .expect("failed to load fixture module");
    let module_name = module_resolver::derive_module_name(&module_dir);
    let signature = FileSignature::from_parsed_file_with_name(&parsed, &module_name);
    strip_ansi(&signature.display())
}

/// Helper: load a single .crn file fixture and return the stripped display output.
fn module_info_file_output(fixture_name: &str) -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let file_path = PathBuf::from(format!(
        "{}/tests/fixtures/module_info/{}/main.crn",
        manifest_dir, fixture_name
    ));

    let parsed = module_resolver::get_parsed_file(&file_path).expect("failed to load fixture file");
    let module_name = module_resolver::derive_module_name(&file_path);
    let signature = FileSignature::from_parsed_file_with_name(&parsed, &module_name);
    strip_ansi(&signature.display())
}

/// Full module with arguments, resources (with dependencies), and attributes.
#[test]
fn snapshot_module_info() {
    let display = module_info_output(".");
    insta::assert_snapshot!(display);
}

/// Module with attributes but no arguments block.
/// Verifies the ARGUMENTS section shows "(none)".
#[test]
fn snapshot_module_info_no_args() {
    let display = module_info_output("no_args");
    insta::assert_snapshot!(display);
}

/// Module with resources that have no inter-resource dependencies.
/// Verifies resources are listed without a tree structure.
#[test]
fn snapshot_module_info_no_deps() {
    let display = module_info_output("no_deps");
    insta::assert_snapshot!(display);
}

/// Module with arguments and empty attributes, no resources.
/// Verifies CREATES and ATTRIBUTES sections show "(none)".
#[test]
fn snapshot_module_info_empty_module() {
    let display = module_info_output("empty_module");
    insta::assert_snapshot!(display);
}

/// Root config file (not a module directory) with resources and dependencies.
/// Verifies the RootConfig display format (File: header, IMPORTS section, dependency tree).
#[test]
fn snapshot_module_info_root_config() {
    let display = module_info_file_output("root_config");
    insta::assert_snapshot!(display);
}
