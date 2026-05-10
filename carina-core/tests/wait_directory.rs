//! Directory-scoped acceptance test for the `wait` construct (#2825).
//!
//! Per the CLAUDE.md "Directory-scoped, never single-file" rule, every
//! feature that reads DSL source must be exercised against a multi-file
//! fixture. This test fails if `wait` declared in one `.crn` file
//! cannot resolve to a target / depends_on binding declared in a
//! sibling file.

use std::path::PathBuf;

use carina_core::config_loader::parse_directory;
use carina_core::parser::ProviderContext;
use carina_core::resource::Value;

#[test]
fn wait_resolves_target_and_depends_on_across_sibling_files() {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("tests/fixtures/wait/cert_issued");

    let parsed = parse_directory(&dir, &ProviderContext::default())
        .expect("parse_directory should succeed for valid fixture");

    // The wait binding lives in wait.crn; cert / validation_record are
    // in main.crn. Cross-file visibility is the load-bearing assertion.
    assert_eq!(
        parsed.wait_bindings.len(),
        1,
        "expected exactly one wait binding, got {}",
        parsed.wait_bindings.len()
    );
    let wait = &parsed.wait_bindings[0];
    assert_eq!(wait.binding, "cert_issued");
    assert_eq!(wait.target, "cert");
    assert_eq!(
        wait.until_raw,
        "cert.status == aws.acm.Certificate.Status.Issued"
    );
    assert_eq!(wait.timeout_secs, Some(75 * 60));
    assert_eq!(wait.depends_on, vec!["validation_record"]);
    assert_eq!(wait.until_predicate.lhs_segments, vec!["cert", "status"]);
    assert_eq!(
        wait.until_predicate.rhs,
        Value::String("aws.acm.Certificate.Status.Issued".to_string())
    );

    // Sanity: the target binding really lives in a sibling file.
    let cert = parsed
        .resources
        .iter()
        .find(|r| r.id.name.as_str() == "cert")
        .expect("cert binding should be present from main.crn");
    assert_eq!(cert.id.resource_type, "acm.Certificate");
}
