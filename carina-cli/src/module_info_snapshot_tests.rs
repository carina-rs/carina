//! Module info snapshot tests.
//!
//! Loads a fixture module directory and snapshots the `module info` display output.

use std::path::PathBuf;

use carina_core::module::FileSignature;
use carina_core::module_resolver;

/// Strip ANSI escape codes from a string for snapshot readability.
fn strip_ansi(s: &str) -> String {
    let re = regex_lite::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(s, "").to_string()
}

#[test]
fn snapshot_module_info() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let module_dir = PathBuf::from(format!("{}/tests/fixtures/module_info", manifest_dir));

    let parsed = module_resolver::load_module_from_directory(&module_dir)
        .expect("failed to load fixture module");
    let module_name = module_resolver::derive_module_name(&module_dir);
    let signature = FileSignature::from_parsed_file_with_name(&parsed, &module_name);
    let display = strip_ansi(&signature.display());

    insta::assert_snapshot!(display);
}
