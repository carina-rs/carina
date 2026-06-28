//! Directory-scoped acceptance test for `directives.depends_on` (#2823).
//!
//! Per the CLAUDE.md "Directory-scoped, never single-file" rule, every
//! feature that reads DSL source must be exercised against a multi-file
//! fixture. This test fails if `directives.depends_on` in one `.crn`
//! file cannot resolve to a binding declared in a sibling file.

use std::path::PathBuf;

use carina_core::config_loader::parse_directory;
use carina_core::deps::get_resource_dependencies;
use carina_core::parser::ProviderContext;

#[test]
fn depends_on_resolves_across_sibling_files_in_directory() {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("tests/fixtures/depends_on/basic");

    let parsed = parse_directory(&dir, &ProviderContext::default())
        .expect("parse_directory should succeed for valid fixture");

    let bucket = parsed
        .resources
        .iter()
        .find(|r| r.id.identity_or_empty() == "bucket")
        .expect("bucket binding should be present");

    let deps = get_resource_dependencies(bucket);
    assert!(
        deps.contains("role"),
        "depends_on entry from bucket.crn should resolve to 'role' declared in main.crn; \
         got dependencies = {:?}",
        deps
    );
}
