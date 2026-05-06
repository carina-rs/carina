//! Plan display snapshot tests.
//!
//! Each test loads a .crn fixture (and optionally a state file), builds a plan
//! using the same logic as `--refresh=false`, formats the plan output, strips
//! ANSI color codes, and asserts the result against an `insta` snapshot.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use carina_core::config_loader::load_configuration;
use carina_core::resource::{ResourceId, State};
use carina_core::schema::SchemaRegistry;

use crate::DetailLevel;
use crate::display::{format_destroy_plan, format_plan};
use crate::fixture_plan::build_plan_from_fixture_name;

/// Strip ANSI escape codes from a string for snapshot readability.
fn strip_ansi(s: &str) -> String {
    let re = regex_lite::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(s, "").to_string()
}

/// Build a plan from a .crn fixture and optional state file, mimicking `--refresh=false`.
fn build_plan_from_fixture(
    fixture_dir: &str,
) -> (
    carina_core::plan::Plan,
    SchemaRegistry,
    HashMap<ResourceId, ResourceId>,
) {
    let fp = build_plan_from_fixture_name(fixture_dir);
    (fp.plan, fp.schemas, fp.moved_origins)
}

#[allow(clippy::type_complexity)]
fn build_plan_and_states_from_fixture(
    fixture_dir: &str,
) -> (
    carina_core::plan::Plan,
    HashMap<ResourceId, State>,
    SchemaRegistry,
    HashMap<ResourceId, ResourceId>,
) {
    let fp = build_plan_from_fixture_name(fixture_dir);
    (fp.plan, fp.current_states, fp.schemas, fp.moved_origins)
}

#[test]
fn snapshot_all_create() {
    let (plan, schemas, _moved) = build_plan_from_fixture("all_create");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_policy_pretty() {
    let (plan, schemas, _moved) = build_plan_from_fixture("policy_pretty");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_pretty_long_string_list() {
    let (plan, schemas, _moved) = build_plan_from_fixture("pretty_long_string_list");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_pretty_short_string_list() {
    let (plan, schemas, _moved) = build_plan_from_fixture("pretty_short_string_list");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_no_changes() {
    let (plan, _schemas, _moved) = build_plan_from_fixture("no_changes");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_mixed_operations() {
    let (plan, schemas, _moved) = build_plan_from_fixture("mixed_operations");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_delete_orphan() {
    use carina_core::resource::Value;
    let (plan, current_states, schemas, _moved) =
        build_plan_and_states_from_fixture("delete_orphan");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let carina_core::effect::Effect::Delete { id, .. } = e {
                current_states
                    .get(id)
                    .map(|s| (id.clone(), s.attributes.clone()))
            } else {
                None
            }
        })
        .collect();
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_compact() {
    let (plan, _schemas, _moved) = build_plan_from_fixture("compact");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_map_key_diff() {
    let (plan, schemas, _moved) = build_plan_from_fixture("map_key_diff");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_enum_display() {
    let (plan, schemas, _moved) = build_plan_from_fixture("enum_display");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_no_changes_enum() {
    let (plan, _schemas, _moved) = build_plan_from_fixture("no_changes_enum");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_destroy_full() {
    use carina_core::resource::Value;
    let (plan, current_states, _schemas, _moved) =
        build_plan_and_states_from_fixture("destroy_full");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = current_states
        .into_iter()
        .filter(|(_, state)| state.exists)
        .map(|(id, state)| (id, state.attributes))
        .collect();
    let output = strip_ansi(&format_destroy_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_destroy_orphans() {
    use carina_core::resource::Value;
    let (plan, current_states, _schemas, _moved) =
        build_plan_and_states_from_fixture("destroy_orphans");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = current_states
        .into_iter()
        .filter(|(_, state)| state.exists)
        .map(|(id, state)| (id, state.attributes))
        .collect();
    let output = strip_ansi(&format_destroy_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_default_values() {
    let (plan, schemas, _moved) = build_plan_from_fixture("default_values");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_read_only_attrs() {
    let (plan, schemas, _moved) = build_plan_from_fixture("read_only_attrs");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_explicit() {
    let (plan, schemas, _moved) = build_plan_from_fixture("explicit");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Explicit,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_default_tags() {
    let (plan, schemas, _moved) = build_plan_from_fixture("default_tags");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_state_blocks() {
    let (plan, schemas, _moved) = build_plan_from_fixture("state_blocks");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_secret_values() {
    use carina_core::resource::Value;
    let (plan, current_states, schemas, _moved) =
        build_plan_and_states_from_fixture("secret_values");
    let delete_attributes: HashMap<ResourceId, HashMap<String, Value>> = plan
        .effects()
        .iter()
        .filter_map(|e| {
            if let carina_core::effect::Effect::Delete { id, .. } = e {
                current_states
                    .get(id)
                    .map(|s| (id.clone(), s.attributes.clone()))
            } else {
                None
            }
        })
        .collect();
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &delete_attributes,
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_moved_with_changes() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("moved_with_changes");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

/// Moved block with attribute removal: prev_desired_keys must transfer to new name.
///
/// State has "old_vpc" with desired_keys=["cidr_block", "tags"].
/// After move to "new_vpc", tags are removed from the DSL.
/// Plan should detect the removal via prev_desired_keys under the new name.
#[test]
fn snapshot_moved_prev_keys() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("moved_prev_keys");
    // The plan must contain an Update effect to remove the "tags" attribute.
    let has_update = plan
        .effects()
        .iter()
        .any(|e| matches!(e, carina_core::effect::Effect::Update { .. }));
    assert!(
        has_update,
        "Plan should detect tag removal via prev_desired_keys transfer, but no Update effect found"
    );
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

/// Pure move: Move effect with no attribute changes.
///
/// State has "old_vpc" with cidr_block=10.0.0.0/16.
/// After move to "new_vpc", attributes are identical -> Move only, no Update.
/// The Move line must be visible in the plan tree.
#[test]
fn snapshot_moved_pure() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("moved_pure");
    // Pure move should NOT have an Update effect
    let has_update = plan
        .effects()
        .iter()
        .any(|e| matches!(e, carina_core::effect::Effect::Update { .. }));
    assert!(
        !has_update,
        "Pure move fixture should not produce an Update effect"
    );
    // But should have a Move effect
    let has_move = plan
        .effects()
        .iter()
        .any(|e| matches!(e, carina_core::effect::Effect::Move { .. }));
    assert!(has_move, "Pure move fixture should produce a Move effect");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

/// Compact (`DetailLevel::None`) rendering of a moved Update effect.
///
/// Locks in the `(moved from: <name>)` annotation form on the
/// compact path of `display::TreeRenderer::render_node` (#2470). The
/// detailed branch is already covered by `snapshot_moved_with_changes`;
/// this fixture-reusing test guards the compact branch separately so a
/// regression to the redundant `<provider>.<type>.<name>` form would
/// surface here instead of going unnoticed.
#[test]
fn snapshot_moved_with_changes_compact() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("moved_with_changes");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

/// Collect unused `let` bindings across every fixture subdirectory of
/// `fixtures_root`. A fixture is any immediate subdirectory containing at
/// least one `.crn` file (the file need not be named `main.crn` — sibling
/// layouts like `resources.crn` + `exports.crn` are covered).
fn collect_unused_let_bindings_in_fixtures(
    fixtures_root: &std::path::Path,
) -> Vec<(String, Vec<String>)> {
    let mut failures: Vec<(String, Vec<String>)> = Vec::new();

    for entry in std::fs::read_dir(fixtures_root).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_dir() {
            continue;
        }
        let fixture_dir = entry.path();
        let has_crn = std::fs::read_dir(&fixture_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.path().extension().is_some_and(|ext| ext == "crn"));
        if !has_crn {
            continue;
        }
        let fixture_name = entry.file_name().to_string_lossy().to_string();

        let loaded = load_configuration(&fixture_dir).unwrap();
        let unused = crate::wiring::check_unused_bindings(&loaded.unresolved_parsed);
        if unused.is_empty() {
            continue;
        }
        // Moved block targets are structurally required bindings
        let move_targets: HashSet<String> = loaded
            .unresolved_parsed
            .state_blocks
            .iter()
            .filter_map(|sb| {
                if let carina_core::parser::StateBlock::Moved { to, .. } = sb {
                    Some(to.name_str().to_string())
                } else {
                    None
                }
            })
            .collect();
        let truly_unused: Vec<String> = unused
            .into_iter()
            .filter(|b| !move_targets.contains(b))
            .collect();
        if !truly_unused.is_empty() {
            failures.push((fixture_name, truly_unused));
        }
    }

    failures
}

/// Ensure no fixture .crn file has unused `let` bindings.
///
/// `let` should only be used when a binding is referenced by another resource.
/// This test prevents regressions where unnecessary `let` bindings are added
/// to fixture files.
#[test]
fn no_unused_let_bindings_in_fixtures() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixtures_dir = PathBuf::from(format!("{}/tests/fixtures/plan_display", manifest_dir));

    let failures = collect_unused_let_bindings_in_fixtures(&fixtures_dir);

    if !failures.is_empty() {
        let msg: Vec<String> = failures
            .iter()
            .map(|(name, bindings)| {
                format!("  {}: unused let bindings: {}", name, bindings.join(", "))
            })
            .collect();
        panic!(
            "Fixture .crn files must not have unused let bindings:\n{}",
            msg.join("\n")
        );
    }
}

/// Regression test for #1997: before, the walker silently skipped any
/// fixture directory that did not contain `main.crn`, so unused `let`
/// bindings in a sibling-only layout (e.g. `resources.crn`) would pass
/// this check unnoticed. The walker now keys off "directory contains
/// any .crn file", so such layouts are exercised too.
#[test]
fn unused_let_check_covers_fixtures_without_main_crn() {
    let tmp_root = std::env::temp_dir().join("carina_test_unused_let_sibling_only");
    let _ = std::fs::remove_dir_all(&tmp_root);
    let fixture_dir = tmp_root.join("sibling_only");
    std::fs::create_dir_all(&fixture_dir).unwrap();

    // Deliberately NO main.crn. The only .crn declares an unused let binding.
    std::fs::write(
        fixture_dir.join("resources.crn"),
        "provider awscc {\n  region = awscc.Region.ap_northeast_1\n}\n\n\
         let orphan = awscc.ec2.Vpc {\n  cidr_block = '10.0.0.0/16'\n}\n",
    )
    .unwrap();

    let failures = collect_unused_let_bindings_in_fixtures(&tmp_root);
    assert_eq!(
        failures.len(),
        1,
        "fixtures without main.crn must still be inspected for unused let bindings"
    );
    assert_eq!(failures[0].0, "sibling_only");
    assert_eq!(failures[0].1, vec!["orphan".to_string()]);

    let _ = std::fs::remove_dir_all(&tmp_root);
}

#[test]
fn plan_snapshot_upstream_state() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("upstream_state");
    let output = format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
    );
    insta::assert_snapshot!(strip_ansi(&output));
}

/// When an upstream_state's state file is missing, the plan must render
/// the unresolved attribute as `(known after upstream apply: <ref>)`
/// instead of leaving the raw dot-form (`network.vpc.vpc_id`) which
/// looks like a string literal. See issue #2366.
#[test]
fn plan_snapshot_upstream_state_unresolved() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("upstream_state_unresolved");
    let output = format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
    );
    let stripped = strip_ansi(&output);
    assert!(
        stripped.contains("(known after upstream apply: network.vpc.vpc_id)"),
        "expected unresolved upstream ref to render as `(known after upstream apply: ...)`, got:\n{}",
        stripped
    );
    insta::assert_snapshot!(stripped);
}

/// Companion to `plan_snapshot_upstream_state_unresolved`: state file is
/// present but `exports` is empty (upstream module declared but not yet
/// applied). The same `(known after upstream apply: <ref>)` rendering
/// must apply, with no warning since the state file was readable. See
/// issue #2366.
#[test]
fn plan_snapshot_upstream_state_empty_exports() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("upstream_state_empty_exports");
    let output = format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
    );
    let stripped = strip_ansi(&output);
    assert!(
        stripped.contains("(known after upstream apply: network.vpc.vpc_id)"),
        "expected empty-exports upstream ref to render as `(known after upstream apply: ...)`, got:\n{}",
        stripped
    );
    insta::assert_snapshot!(stripped);
}

/// Issue #2435: `let X = upstream_state {...}` lives in `state.crn`,
/// the consuming `${X.field['key']}` and bare `X.field['key']` live in
/// `main.crn`. The plan must resolve both forms to the concrete value
/// from the upstream's exports map.
#[test]
fn plan_snapshot_upstream_state_map_subscript() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("upstream_state_map_subscript");
    let output = format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
    );
    let stripped = strip_ansi(&output);
    // Both subscript forms — bare attribute value and inside `${...}`
    // interpolation — must end up substituted with the actual account ids.
    assert!(
        stripped.contains("222222222222"),
        "expected dev account id substituted, got:\n{stripped}"
    );
    assert!(
        stripped.contains("111111111111"),
        "expected prod account id substituted, got:\n{stripped}"
    );
    assert!(
        stripped.contains("shared-222222222222-bucket"),
        "expected `${{orgs.accounts['registry_dev']}}` interpolation substituted, got:\n{stripped}"
    );
    // Pin the bare-attribute form distinctly from the interpolation
    // form: a regression that re-rendered the bare-attribute case as
    // the literal `orgs.accounts['registry_dev']` (the original #2435
    // bug) would still satisfy the broad "contains the id" checks
    // above because the interpolation case would still substitute.
    assert!(
        stripped.contains("DevAccount: \"222222222222\""),
        "expected bare `orgs.accounts['registry_dev']` attribute substituted, got:\n{stripped}"
    );
    insta::assert_snapshot!(stripped);
}

/// Issue #2447: companion to the subscript fixture, but for the
/// dot-notation form `${X.field.key}` / bare `X.field.key`. Pre-fix the
/// dot form passed validate but rendered the literal substring
/// `orgs.accounts.registry_dev` into the output (the parser fell back
/// to `Value::String` because the head wasn't a known binding in the
/// current file). Symmetric with the #2435 subscript fix; both forms
/// must now resolve to the upstream's concrete map value.
#[test]
fn plan_snapshot_upstream_state_map_dot_notation() {
    let (plan, schemas, moved_origins) = build_plan_from_fixture("upstream_state_map_dot_notation");
    let output = format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &[],
        &[],
    );
    let stripped = strip_ansi(&output);
    // Both dot-notation forms — bare attribute value and inside `${...}`
    // interpolation — must end up substituted with the actual account ids.
    assert!(
        stripped.contains("222222222222"),
        "expected dev account id substituted, got:\n{stripped}"
    );
    assert!(
        stripped.contains("111111111111"),
        "expected prod account id substituted, got:\n{stripped}"
    );
    assert!(
        stripped.contains("shared-222222222222-bucket"),
        "expected `${{orgs.accounts.registry_dev}}` interpolation substituted, got:\n{stripped}"
    );
    // Pin that the bare-attribute form does not regress to the literal
    // `orgs.accounts.registry_dev` substring (the original #2447 bug).
    assert!(
        stripped.contains("DevAccount: \"222222222222\""),
        "expected bare `orgs.accounts.registry_dev` attribute substituted, got:\n{stripped}"
    );
    assert!(
        !stripped.contains("orgs.accounts.registry_dev"),
        "literal `orgs.accounts.registry_dev` substring leaked into output:\n{stripped}"
    );
    insta::assert_snapshot!(stripped);
}

#[test]
fn plan_snapshot_exports() {
    use crate::commands::plan::ExportChange;
    use carina_core::parser::TypeExpr;
    use carina_core::resource::Value;

    let (plan, schemas, moved_origins) = build_plan_from_fixture("exports");
    let export_changes = vec![
        ExportChange::Added {
            name: "vpc_id".to_string(),
            type_expr: Some(TypeExpr::String),
            new_value: Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        },
        ExportChange::Added {
            name: "cidr".to_string(),
            type_expr: None,
            new_value: Value::resource_ref("vpc".to_string(), "cidr_block".to_string(), vec![]),
        },
    ];
    let output = format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &export_changes,
        &[],
    );
    insta::assert_snapshot!(strip_ansi(&output));
}

/// Verifies that a project whose provider/resource/exports blocks are
/// spread across sibling .crn files produces the same plan as the
/// single-file `exports` fixture. Guards against regressions where
/// directory-scoped parsing drops definitions in sibling files: the
/// `export_changes` fed to `format_plan` are derived from the loaded
/// `parsed.export_params`, so a dropped `exports.crn` would make the
/// Exports section disappear from the snapshot.
#[test]
fn plan_snapshot_exports_multifile() {
    use crate::commands::plan::compute_export_diffs;

    let fp = build_plan_from_fixture_name("exports_multifile");

    // Assert the multi-file load actually picked up exports.crn before
    // rendering, so the snapshot claim is backed by parsed state.
    let exported_names: Vec<&str> = fp.export_params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        exported_names,
        vec!["vpc_id", "cidr"],
        "exports.crn definitions must be merged when loading a multi-file project"
    );

    let export_changes = compute_export_diffs(&fp.export_params, &HashMap::new());
    let output = format_plan(
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&fp.schemas),
        &fp.moved_origins,
        &export_changes,
        &[],
    );
    insta::assert_snapshot!(strip_ansi(&output));
}

#[test]
fn plan_snapshot_export_changes_mixed() {
    use crate::commands::plan::ExportChange;
    use carina_core::parser::TypeExpr;
    use carina_core::resource::Value;

    let (plan, schemas, moved_origins) = build_plan_from_fixture("no_changes");
    let export_changes = vec![
        ExportChange::Added {
            name: "new_export".to_string(),
            type_expr: Some(TypeExpr::String),
            new_value: Value::String("hello".to_string()),
        },
        ExportChange::Modified {
            name: "changed".to_string(),
            type_expr: Some(TypeExpr::Int),
            old_json: serde_json::json!(42),
            new_value: Value::Int(100),
        },
        ExportChange::Removed {
            name: "obsolete".to_string(),
            old_json: serde_json::json!("gone"),
        },
    ];
    let output = format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &moved_origins,
        &export_changes,
        &[],
    );
    insta::assert_snapshot!(strip_ansi(&output));
}

#[test]
fn snapshot_nested_map_diff() {
    let (plan, schemas, _moved) = build_plan_from_fixture("nested_map_diff");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_deferred_for() {
    let fp = build_plan_from_fixture_name("deferred_for");
    assert!(
        !fp.deferred_for_expressions.is_empty(),
        "expected at least one deferred for-expression"
    );

    let output = strip_ansi(&format_plan(
        &fp.plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &fp.deferred_for_expressions,
    ));
    insta::assert_snapshot!(output);
}

#[test]
fn snapshot_provider_prefix() {
    // Regression guard for #2426 / #2431: anonymous resource identifiers
    // gain a `<provider>_` prefix so plan output and state files
    // self-describe their provider. The header line for the lone Vpc
    // resource must read `+ awscc.ec2.Vpc awscc_ec2_vpc_<8hex>`.
    let (plan, schemas, _moved) = build_plan_from_fixture("provider_prefix");
    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        Some(&schemas),
        &HashMap::new(),
        &[],
        &[],
    ));
    insta::assert_snapshot!(output);
}
