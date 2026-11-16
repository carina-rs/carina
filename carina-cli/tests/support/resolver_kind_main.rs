//! Shared main-only recording guard. A path allowlist is not a code audit:
//! trust is reproducibility from pinned main plus these exact source hashes.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
pub const MAIN: &str = "dedb7bcebd10f6b9a6b2d4a7cd6c8b318f8b2ccd";
pub const TEST_SOURCES: &[&str] = &[
    "carina-cli/tests/spike_4400_directory.rs",
    "carina-cli/tests/support/resolver_kind_main.rs",
    "carina-cli/src/commands/plan/pr1_tests.rs",
    "carina-cli/src/commands/plan.rs",
];
// Literal paths, no directory/prefix wildcard. Includes the one permitted
// existing-source edit: the cfg(test) module declaration in plan.rs.
pub const ALLOWED_FILES: &[&str] = &[
    "carina-cli/tests/spike_4400_directory.rs",
    "carina-cli/tests/support/resolver_kind_main.rs",
    "carina-cli/src/commands/plan/pr1_tests.rs",
    "carina-cli/src/commands/plan.rs",
    "carina-cli/tests/fixtures/resolver_kind_main/4400-cells.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/manifest.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/all_create.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_create_only_rekey.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_if_legacy.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_claimed_legacy.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_deferred_map.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_deferred_map__orgs.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_delete_earlier.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_insert_before.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_module_call.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_module_call__network.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_new_id_exists.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_reorder.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_same_tail.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_loop_sibling_files.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_old_identity_name_override.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/anonymous_old_identity_update.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/compact.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/default_tags.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/default_tags_update_replace.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/default_values.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/deferred_for.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/deferred_for_anonymous.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/deferred_for_create_solo.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/deferred_for_with_dependent_wait.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/deferred_for_with_paired_destroy.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/deferred_for_with_unrelated_delete.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/delete_orphan.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/delete_orphan_list_of_maps.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/depends_on.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/deposed_delete.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/deposed_delete_update_dependency.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/destroy_full.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/destroy_list_struct_child_gutter.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/destroy_orphans.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/dynamic_enum_az_no_diff.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/empty_explicit_children_no_changes.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/enum_display.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/explicit.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_let_chain.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_let_chain__usecase.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_let_chain_5hop.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_let_chain_5hop__usecase.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_let_literal.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_let_literal__usecase.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_module_output.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_module_output__usecase.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_resource_attribute_ref.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_resource_attribute_ref__usecase.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_string_let_attr_access_rejected.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/exports_multifile_string_let_attr_access_rejected__usecase.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/ignore_changes_readable.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/import_deferred_interpolation.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/import_deferred_interpolation__network.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/keyed_partial_update_release.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/legacy_orphan_unknown_destroy_ordering.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/list_diff_added_struct.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/list_diff_added_struct_child_gutter.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/list_diff_best_first_mid_insert.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/list_diff_modified_with_unchanged.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/list_diff_modified_with_unchanged_nested.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/list_diff_paired_all_unchanged_dropped.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/list_diff_removed_struct.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/list_diff_string_list_grew.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/list_struct_child_gutter.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/map_added_from_none.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/map_attribute_removed.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/map_field_string_list_grew.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/map_key_diff.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/mixed_operations.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/module_anonymous_resource.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/module_anonymous_resource__oidc-module.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_claims_precede_heuristics.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_hint_ambiguous.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_hint_let_to_for.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_hint_read_loop.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_hint_read_loop_module.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_hint_read_loop_module__record-module.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_let_to_for_bound.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_let_to_for_map.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_let_to_for_read_loop.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_let_to_for_read_loop_module.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_let_to_for_read_loop_module__record-module.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_let_to_for_unbound.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_let_to_for_unbound_reference.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_prev_keys.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_pure.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/moved_with_changes.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/multi_instance_create.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/multi_instance_module.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/multi_instance_module__reg-module.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/nested_list_of_maps_all_dropped.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/nested_map_diff.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/nested_write_only.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/no_changes.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/no_changes_enum.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/pending_inputs.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/plan_image_anonymous.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/plan_image_module_cascade.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/plan_image_module_cascade__review-module.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/plan_image_reference_layout.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/plan_image_reference_layout__app-module.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/policy_pretty.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/policy_pretty_dynamic_key_list.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/policy_pretty_nested.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/postfix_access.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/pretty_long_string_list.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/pretty_short_string_list.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/provider_prefix.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/read_only_attrs.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/removed_missing_identifier.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/replace_create_only.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/replace_with_non_forcing_diffs.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/required_by_deferred.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/required_by_resources.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/resolved_value_constraint_multifile.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/route53_hosted_zone_name_strip_suffix_no_diff.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/secret_values.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/server_added_list_elements_changed.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/server_default_struct_leaf.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/state_blocks.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/state_only_directive_change.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/string_dot_suffix.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/upstream_state.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/upstream_state__network.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/upstream_state_empty_exports.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/upstream_state_empty_exports__network.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/upstream_state_map_dot_notation.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/upstream_state_map_dot_notation__orgs.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/upstream_state_map_subscript.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/upstream_state_map_subscript__orgs.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/upstream_state_unresolved.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/upstream_state_unresolved__network.json",
    "carina-cli/tests/fixtures/resolver_kind_main/plan-json/wait_cert.json",
];
pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}
pub fn golden_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/resolver_kind_main")
}
fn git(args: &[&str]) -> Vec<u8> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(workspace())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
pub fn recording_header() -> Value {
    assert_eq!(
        String::from_utf8(git(&["rev-parse", "HEAD"]))
            .unwrap()
            .trim(),
        MAIN,
        "export requires the pinned main HEAD"
    );
    for output in [
        git(&["diff", "--name-only", "-z", "HEAD"]),
        git(&["ls-files", "--others", "--exclude-standard", "-z"]),
    ] {
        for path in output
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
        {
            let path = std::str::from_utf8(path).unwrap();
            assert!(
                ALLOWED_FILES.contains(&path),
                "export refuses unlisted change: {path}"
            );
        }
    }
    let hashes = TEST_SOURCES
        .iter()
        .map(|path| {
            let result = std::process::Command::new("shasum")
                .args(["-a", "256", path])
                .current_dir(workspace())
                .output()
                .unwrap();
            assert!(result.status.success(), "sha256 failed for {path}");
            let hash = String::from_utf8(result.stdout)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_owned();
            ((*path).to_owned(), Value::String(hash))
        })
        .collect::<serde_json::Map<_, _>>();
    json!({"main_commit": MAIN, "test_only_files_sha256": hashes,
        "hash_scope": "test source files including the plan.rs module declaration; generated goldens excluded to avoid self hashes",
        "allowed_files": ALLOWED_FILES})
}
pub fn normalize_prefix(value: Value, root: &Path, token: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(root.to_str().unwrap(), token)),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|v| normalize_prefix(v, root, token))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(k, v)| (k, normalize_prefix(v, root, token)))
                .collect(),
        ),
        value => value,
    }
}
pub fn json_difference(expected: &Value, actual: &Value) -> Option<String> {
    fn difference(expected: &Value, actual: &Value, path: &str) -> Option<String> {
        match (expected, actual) {
            (Value::Array(a), Value::Array(b)) if a.len() == b.len() => a
                .iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (a, b))| difference(a, b, &format!("{path}/{i}"))),
            (Value::Object(a), Value::Object(b)) if a.keys().eq(b.keys()) => a
                .iter()
                .find_map(|(k, a)| difference(a, &b[k], &format!("{path}/{k}"))),
            _ if expected == actual => None,
            _ => Some(format!("{path}: expected {expected}, got {actual}")),
        }
    }
    difference(expected, actual, "")
}
pub fn assert_json_eq(expected: &Value, actual: &Value, context: &str) {
    if let Some(diff) = json_difference(expected, actual) {
        panic!("{context}: {diff}");
    }
}
// Failure evidence only, outside the worktree; never an update mode.
pub fn save_actual(label: &str, bytes: &[u8]) -> PathBuf {
    use std::io::Write as _;
    let mut file = tempfile::Builder::new()
        .prefix(&format!("pr1-actual-{}-", label.replace('/', "__")))
        .suffix(".json")
        .tempfile()
        .unwrap();
    file.write_all(bytes).unwrap();
    file.keep().unwrap().1
}
