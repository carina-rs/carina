//! Integration tests for negative/boundary .crn test files.
//!
//! These tests verify that invalid .crn files produce appropriate validation errors
//! when run through `carina validate`.

use std::process::Command;

fn carina_validate(fixture: &str) -> std::process::Output {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixture_path = format!("{}/tests/fixtures/negative/{}", manifest_dir, fixture);

    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", &fixture_path])
        .output()
        .expect("failed to execute carina")
}

fn assert_validate_fails(fixture: &str, expected_substring: &str) {
    let output = carina_validate(fixture);
    assert!(
        !output.status.success(),
        "{}: expected validation to fail but it succeeded.\nstdout: {}\nstderr: {}",
        fixture,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_substring),
        "{}: expected error containing '{}' but got:\n{}",
        fixture,
        expected_substring,
        stderr,
    );
}

#[test]
#[ignore = "requires provider binary for schema-based validation"]
fn invalid_enum_value() {
    assert_validate_fails("invalid_enum_value.crn", "Invalid enum variant");
}

#[test]
#[ignore = "requires provider binary for schema-based validation"]
fn invalid_cidr_format() {
    assert_validate_fails("invalid_cidr.crn", "Invalid CIDR format");
}

#[test]
#[ignore = "requires provider binary for schema-based validation"]
fn invalid_cidr_prefix_out_of_range() {
    assert_validate_fails("invalid_cidr_prefix.crn", "Invalid prefix length");
}

#[test]
#[ignore = "requires provider binary for schema-based validation"]
fn invalid_cidr_octet_out_of_range() {
    assert_validate_fails("invalid_cidr_octet.crn", "Invalid octet");
}

#[test]
#[ignore = "requires provider binary for schema-based validation"]
fn type_mismatch_bool_gets_string() {
    assert_validate_fails("type_mismatch_bool.crn", "expected Bool, got String");
}

#[test]
#[ignore = "requires provider binary for schema-based validation"]
fn type_mismatch_int_gets_string() {
    assert_validate_fails("type_mismatch_int.crn", "Expected integer");
}

#[test]
#[ignore = "requires provider binary for schema-based validation"]
fn out_of_range_integer() {
    assert_validate_fails("out_of_range_int.crn", "out of range");
}

#[test]
#[ignore = "requires provider binary for resource type validation"]
fn unknown_resource_type() {
    assert_validate_fails("unknown_resource_type.crn", "Unknown resource type");
}

#[test]
#[ignore = "requires provider binary for schema-based validation"]
fn missing_required_attribute() {
    assert_validate_fails("missing_required_attr.crn", "group_description");
}

#[test]
#[ignore = "requires provider binary for schema-based validation"]
fn invalid_region() {
    assert_validate_fails("invalid_region.crn", "region");
}

#[test]
fn missing_provider_plugin() {
    assert_validate_fails("missing_provider_plugin", "has no source configured");
}

#[test]
fn arguments_block_rejected_in_root() {
    // Issue #2198. An `arguments` block belongs on the module side of a
    // module boundary; placing it in a root configuration silently turns
    // its `default` values into a de-facto root variable. Reject it with
    // a clear diagnostic instead.
    assert_validate_fails(
        "arguments_in_root",
        "arguments blocks are only valid inside module definitions",
    );
}

#[test]
fn validate_reports_multiple_static_errors_in_one_pass() {
    // Regression for #2102. Independent static errors in the same project
    // must all surface in a single `carina validate` run so the user fixes
    // them in one pass.
    //
    // The fixture declares `let orgs = upstream_state { ... }` (source
    // path intentionally missing) plus two for-expressions with typo'd
    // iterable bindings. Three distinct diagnostics are expected in the
    // combined output: the missing upstream source and both undefined
    // identifiers.
    let output = carina_validate("multiple_errors");
    assert!(
        !output.status.success(),
        "expected validation to fail, got:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("`org`"),
        "missing `org` identifier diagnostic: {stderr}"
    );
    assert!(
        stderr.contains("`missing`"),
        "missing `missing` identifier diagnostic: {stderr}"
    );
    assert!(
        stderr.contains("nonexistent_sibling"),
        "missing upstream_state source diagnostic: {stderr}"
    );
}
