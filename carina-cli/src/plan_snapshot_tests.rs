//! Plan display snapshot tests.
//!
//! Each test loads a .crn fixture (and optionally a state file), builds a plan
//! using the same logic as `--refresh=false`, formats the plan output, strips
//! ANSI color codes, and asserts the result against an `insta` snapshot.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use carina_core::config_loader::load_configuration;
use carina_core::resource::{ResourceId, State};
use carina_core::schema::ResourceSchema;

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
    HashMap<String, ResourceSchema>,
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
    HashMap<String, ResourceSchema>,
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

/// Ensure no fixture .crn file has unused `let` bindings.
///
/// `let` should only be used when a binding is referenced by another resource.
/// This test prevents regressions where unnecessary `let` bindings are added
/// to fixture files.
#[test]
fn no_unused_let_bindings_in_fixtures() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixtures_dir = format!("{}/tests/fixtures/plan_display", manifest_dir);

    let mut failures: Vec<(String, Vec<String>)> = Vec::new();

    for entry in std::fs::read_dir(&fixtures_dir).unwrap() {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_dir() {
            continue;
        }
        let fixture_name = entry.file_name().to_string_lossy().to_string();
        let fixture_dir = PathBuf::from(format!("{}/{}", fixtures_dir, fixture_name));
        let crn_path = fixture_dir.join("main.crn");
        if !crn_path.exists() {
            continue;
        }

        let loaded = load_configuration(&fixture_dir).unwrap();
        let unused = crate::wiring::check_unused_bindings(&loaded.unresolved_parsed);
        if !unused.is_empty() {
            // Moved block targets are structurally required bindings
            let move_targets: HashSet<String> = loaded
                .unresolved_parsed
                .state_blocks
                .iter()
                .filter_map(|sb| {
                    if let carina_core::parser::StateBlock::Moved { to, .. } = sb {
                        Some(to.name.clone())
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
    }

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
/// directory-scoped parsing drops definitions in sibling files.
#[test]
fn plan_snapshot_exports_multifile() {
    use crate::commands::plan::ExportChange;
    use carina_core::parser::TypeExpr;
    use carina_core::resource::Value;

    let (plan, schemas, moved_origins) = build_plan_from_fixture("exports_multifile");
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
