//! Integration tests for fixture-based plan rendering.
//!
//! These exercise the same library path the `plan-fixture` example binary
//! uses (`carina_cli::fixture_plan` + `carina_cli::display`), without
//! spawning a nested `cargo run --example plan-fixture` subprocess. The
//! subprocess form serialized eight concurrent cargo invocations on the
//! target lock and pushed several cases past nextest's 60s SLOW threshold,
//! dominating the CI Test job (refs #3084). Calling the library directly
//! keeps the same behavior assertion (rendered output is non-empty) while
//! running sub-second.

use carina_cli::DetailLevel;
use carina_cli::display::{format_destroy_plan, format_plan};
use carina_cli::fixture_plan::{
    build_plan_from_fixture_name, delete_attributes_from_plan, delete_attributes_from_states,
};

/// Render a fixture the way the `plan-fixture` example does for a normal
/// (non-destroy) plan and return the formatted output string.
fn render_plan(fixture: &str, detail: DetailLevel) -> String {
    let fp = build_plan_from_fixture_name(fixture);
    let delete_attributes = delete_attributes_from_plan(&fp.plan, &fp.current_states);
    format_plan(
        &fp.plan,
        detail,
        &delete_attributes,
        Some(&fp.schemas),
        &fp.moved_origins,
        &[],
        &fp.deferred_for_expressions,
        Some(&fp.prev_explicit),
    )
}

/// Render a fixture as a destroy plan, mirroring `plan-fixture --destroy`.
fn render_destroy_plan(fixture: &str, detail: DetailLevel) -> String {
    let fp = build_plan_from_fixture_name(fixture);
    let delete_attributes = delete_attributes_from_states(&fp.current_states);
    format_destroy_plan(&fp.plan, detail, &delete_attributes)
}

fn assert_non_empty(output: &str, fixture: &str) {
    assert!(
        !output.is_empty(),
        "plan-fixture for '{}' produced empty output",
        fixture,
    );
}

#[test]
fn plan_fixture_all_create() {
    let output = render_plan("all_create", DetailLevel::Full);
    assert_non_empty(&output, "all_create");
}

#[test]
fn plan_fixture_mixed_operations() {
    let output = render_plan("mixed_operations", DetailLevel::Full);
    assert_non_empty(&output, "mixed_operations");
}

#[test]
fn plan_fixture_delete_orphan() {
    let output = render_plan("delete_orphan", DetailLevel::Full);
    assert_non_empty(&output, "delete_orphan");
}

#[test]
fn plan_fixture_compact_detail_none() {
    let output = render_plan("compact", DetailLevel::None);
    assert_non_empty(&output, "compact");
}

#[test]
fn plan_fixture_destroy_full() {
    let output = render_destroy_plan("destroy_full", DetailLevel::Full);
    assert_non_empty(&output, "destroy_full");
}

#[test]
fn plan_fixture_upstream_state() {
    let output = render_plan("upstream_state", DetailLevel::Full);
    assert_non_empty(&output, "upstream_state");
}

#[test]
fn plan_fixture_upstream_state_map_subscript() {
    let output = render_plan("upstream_state_map_subscript", DetailLevel::Full);
    assert_non_empty(&output, "upstream_state_map_subscript");
}

#[test]
fn plan_fixture_upstream_state_map_dot_notation() {
    let output = render_plan("upstream_state_map_dot_notation", DetailLevel::Full);
    assert_non_empty(&output, "upstream_state_map_dot_notation");
}
