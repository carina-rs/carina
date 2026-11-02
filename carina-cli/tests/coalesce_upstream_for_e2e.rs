//! End-to-end regression coverage for carina#3961.
//!
//! A coalesced upstream export must use the export when it exists and the
//! default only when a successfully loaded upstream state omits the export.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use carina_cli::commands::plan::PlanFile;
use carina_core::effect::UnresolvedSavedEffect as Effect;
use carina_core::resource::{ConcreteValue, Value};

fn run_cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("run carina")
}

fn run_cli_with_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(dir)
        .env("NO_COLOR", "1")
        .env("RUST_LOG", "warn")
        .env_remove("CLICOLOR_FORCE")
        .envs(env.iter().copied())
        .args(args);
    command.output().expect("run carina with environment")
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "carina {operation} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_failure(output: &Output, operation: &str) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "carina {operation} unexpectedly succeeded\nstdout:\n{stdout}\nstderr:\n{stderr}",
    );
    format!("{stdout}\n{stderr}")
}

fn write_common_files(dir: &Path) {
    fs::create_dir_all(dir).expect("create project directory");
    fs::write(
        dir.join("backend.crn"),
        "backend local { path = \"carina.state.json\" }\n",
    )
    .expect("write backend configuration");
    fs::write(dir.join("providers.crn"), "provider mock {}\n")
        .expect("write provider configuration");
}

fn write_upstream_main(dir: &Path, export_accounts: bool) {
    let exports = if export_accounts {
        "\nexports { accounts: list(String) = [\"a\", \"b\"] }\n"
    } else {
        "\nexports {}\n"
    };
    fs::write(
        dir.join("main.crn"),
        format!(
            r#"let seed = mock.test.resource {{
  name = "seed"
}}
{exports}"#
        ),
    )
    .expect("write upstream main configuration");
}

fn write_downstream(dir: &Path) {
    fs::write(
        dir.join("upstream.crn"),
        "let up = upstream_state { source = \"../up\" }\n",
    )
    .expect("write upstream_state configuration");
    fs::write(
        dir.join("main.crn"),
        r#"for x in (up.accounts ?? []) {
  mock.test.resource {
    name = x
  }
}
"#,
    )
    .expect("write downstream main configuration");
}

fn write_managed_coalesce_main(dir: &Path, use_reference: bool, include_consumer: bool) {
    let parent_name = if use_reference {
        "source.name"
    } else {
        r#""seed""#
    };
    let consumer = if include_consumer {
        r#"
let consumer = mock.test.resource {
  name = producer.comment ?? "d"
}
"#
    } else {
        ""
    };
    fs::write(
        dir.join("main.crn"),
        format!(
            r#"let source = mock.test.resource {{
  name = "seed"
}}

let producer = mock.test.resource {{
  name = "producer"
  parent_name = {parent_name}
}}
{consumer}"#,
        ),
    )
    .expect("write managed coalesce configuration");
}

fn write_read_only_update_main(dir: &Path, update_producer: bool) {
    let comment = if update_producer {
        "\n  comment = \"refresh me\""
    } else {
        ""
    };
    fs::write(
        dir.join("main.crn"),
        format!(
            r#"let producer = mock.test.resource {{
  name = "seed"{comment}
}}

let consumer = mock.test.resource {{
  name = producer.identifier ?? "d"
}}

exports {{
  identifier: String = producer.identifier ?? "ed"
}}
"#,
        ),
    )
    .expect("write read-only update configuration");
}

fn plan_json(dir: &Path) -> PlanFile {
    let output = run_cli(dir, &["plan", "--json", "."]);
    assert_success(&output, "plan --json");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json_start = stdout
        .find('{')
        .expect("JSON plan must follow provider and refresh status lines");
    serde_json::from_str(&stdout[json_start..]).unwrap_or_else(|error| {
        panic!(
            "plan output was not a saved-plan JSON document: {error}\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        )
    })
}

fn created_names(plan: &PlanFile) -> Vec<String> {
    let mut names = plan
        .plan
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            Effect::Create(resource) => match resource.as_resource().get_attr("name") {
                Some(Value::Concrete(ConcreteValue::String(name))) => {
                    Some(name.expose_plaintext().to_string())
                }
                other => panic!("create must have a concrete name, got {other:?}"),
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn assert_converged(dir: &Path, operation: &str) {
    let plan = plan_json(dir);
    assert!(
        plan.plan.effects().is_empty(),
        "{operation} must converge after the update apply: {:#?}",
        plan.plan.effects(),
    );
    let output = run_cli(dir, &["apply", "--auto-approve", "."]);
    assert_success(&output, operation);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("No changes"),
        "{operation} must report no changes after converging:\n{}",
        String::from_utf8_lossy(&output.stdout),
    );
}

fn state_resource_attributes(
    dir: &Path,
    binding: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let state: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(dir.join("carina.state.json")).expect("read state file"),
    )
    .expect("state file must be valid JSON");
    state["resources"]
        .as_array()
        .expect("state resources must be an array")
        .iter()
        .find(|resource| resource["binding"].as_str() == Some(binding))
        .unwrap_or_else(|| panic!("state must contain binding `{binding}`"))["current"]
        ["attributes"]
        .as_object()
        .expect("current resource attributes must be an object")
        .clone()
}

fn write_updating_list_iterable(dir: &Path, coalesced: bool, updated: bool) {
    let values = if updated { r#"["a", "b"]"# } else { r#"["a"]"# };
    let iterable = if coalesced {
        "(c.list ?? [])"
    } else {
        "c.list"
    };
    fs::write(
        dir.join("main.crn"),
        format!(
            r#"let c = mock.example.consumer {{
  name = "c"
  list = {values}
}}

for x in {iterable} {{
  mock.test.resource {{ name = x }}
}}
"#,
        ),
    )
    .expect("write updating list iterable fixture");
}

fn write_updating_map_iterable(dir: &Path, coalesced: bool, updated: bool) {
    let b = if updated { "3" } else { "2" };
    let iterable = if coalesced {
        "c.nested ?? {}"
    } else {
        "c.nested"
    };
    fs::write(
        dir.join("main.crn"),
        format!(
            r#"let c = mock.example.consumer {{
  name = "c"
  nested = {{ a = "1", b = "{b}" }}
}}

for k, v in ({iterable}) {{
  mock.test.resource {{ name = "${{k}}-${{v}}" }}
}}
"#,
        ),
    )
    .expect("write updating map iterable fixture");
}

#[test]
fn coalesced_upstream_iterable_uses_present_export_then_defaults_when_absent() {
    let root = tempfile::tempdir().expect("create temporary root");
    let upstream = root.path().join("up");
    let downstream = root.path().join("down");
    write_common_files(&upstream);
    write_common_files(&downstream);
    write_upstream_main(&upstream, true);
    write_downstream(&downstream);

    assert_success(&run_cli(&upstream, &["init", "."]), "init upstream");
    assert_success(
        &run_cli(&upstream, &["apply", "--auto-approve", "."]),
        "apply upstream",
    );
    assert_success(&run_cli(&downstream, &["init", "."]), "init downstream");

    let present = plan_json(&downstream);
    assert_eq!(
        created_names(&present),
        ["a".to_string(), "b".to_string()],
        "a present upstream export must win over the coalesce default",
    );

    write_upstream_main(&upstream, false);
    assert_success(
        &run_cli(&upstream, &["apply", "--auto-approve", "."]),
        "reapply upstream without accounts",
    );

    let absent = plan_json(&downstream);
    assert!(
        absent.plan.effects().is_empty(),
        "a loaded-but-missing export must choose [] with no residual effect: {:#?}",
        absent.plan.effects(),
    );
    assert!(
        absent
            .plan
            .effects()
            .iter()
            .all(|effect| !matches!(effect, Effect::DeferredCreate(_))),
        "the absent export must not leave a deferred create",
    );
}

#[test]
fn coalesced_upstream_iterable_stays_deferred_when_state_is_unavailable() {
    let root = tempfile::tempdir().expect("create temporary root");
    let upstream = root.path().join("up");
    let downstream = root.path().join("down");
    write_common_files(&upstream);
    write_common_files(&downstream);
    write_upstream_main(&upstream, true);
    write_downstream(&downstream);

    // Deliberately do not initialize or apply the upstream project: its
    // configuration declares `accounts`, but no state snapshot exists yet.
    assert_success(&run_cli(&downstream, &["init", "."]), "init downstream");

    let unknown = plan_json(&downstream);
    assert!(created_names(&unknown).is_empty());
    assert_eq!(
        unknown
            .plan
            .effects()
            .iter()
            .filter(|effect| matches!(effect, Effect::DeferredCreate(_)))
            .count(),
        1,
        "an unavailable upstream snapshot must not turn the loop into zero resources: {:#?}",
        unknown.plan.effects(),
    );
}

#[test]
fn unchanged_ref_authored_managed_producer_defaults_at_plan_and_apply() {
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    write_managed_coalesce_main(&project, false, false);

    assert_success(&run_cli(&project, &["init", "."]), "init project");
    assert_success(
        &run_cli(&project, &["apply", "--auto-approve", "."]),
        "apply producer fixture",
    );

    // The reference resolves to the already-applied value, so the producer
    // has no planned effect even though its authored input is still deferred
    // before binding resolution. Its refreshed, missing `comment` is therefore
    // definitive absence and must select the fallback.
    write_managed_coalesce_main(&project, true, true);
    let plan = plan_json(&project);
    assert_eq!(created_names(&plan), ["d".to_string()]);
    assert_success(
        &run_cli(&project, &["apply", "--auto-approve", "."]),
        "apply managed coalesce fallback",
    );

    let settled = plan_json(&project);
    assert!(
        settled.plan.effects().is_empty(),
        "the fallback consumer must have been applied without updating its unchanged producer: {:#?}",
        settled.plan.effects(),
    );
}

#[test]
fn present_read_only_attribute_stays_present_while_producer_updates() {
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    write_read_only_update_main(&project, false);

    assert_success(&run_cli(&project, &["init", "."]), "init project");
    assert_success(
        &run_cli(&project, &["apply", "--auto-approve", "."]),
        "apply read-only coalesce fixture",
    );

    write_read_only_update_main(&project, true);
    let plan = plan_json(&project);
    match plan.plan.effects() {
        [Effect::Update { to, .. }] => assert_eq!(
            to.as_resource()
                .binding
                .as_ref()
                .map(|binding| binding.as_str()),
            Some("producer")
        ),
        effects => panic!(
            "only the authored producer update is expected; the present read-only value must not change the consumer: {effects:#?}"
        ),
    }
    let human_plan = run_cli(&project, &["plan", "."]);
    assert_success(&human_plan, "plan read-only coalesce update");
    let human_plan = String::from_utf8_lossy(&human_plan.stdout);
    assert!(
        !human_plan.contains("Exports:") && !human_plan.contains("export change(s)"),
        "the present provider output must also keep the coalesced export stable:\n{human_plan}",
    );

    assert_success(
        &run_cli(&project, &["apply", "--auto-approve", "."]),
        "apply producer update",
    );
    let settled = plan_json(&project);
    assert!(
        settled.plan.effects().is_empty(),
        "coalesced read-only references must settle to the same value as bare references: {:#?}",
        settled.plan.effects(),
    );
}

#[test]
fn wrong_typed_coalesce_default_is_rejected_before_apply() {
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    fs::write(
        project.join("main.crn"),
        r#"let producer = mock.test.resource {
  name = "seed"
}

let consumer = mock.test.resource {
  name = "consumer"
  comment = producer.comment ?? 42
}
"#,
    )
    .expect("write wrong-typed coalesce fixture");

    assert_success(&run_cli(&project, &["init", "."]), "init project");
    for (operation, args) in [
        ("validate wrong-typed coalesce", &["validate", "."][..]),
        ("plan wrong-typed coalesce", &["plan", "."][..]),
    ] {
        let output = assert_failure(&run_cli(&project, args), operation);
        let output = output.to_ascii_lowercase();
        assert!(
            output.contains("string") && output.contains("int"),
            "{operation} must explain the coalesce type mismatch:\n{output}",
        );
    }
}

#[test]
fn schema_attribute_typo_on_coalesce_lhs_is_rejected() {
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    fs::write(
        project.join("main.crn"),
        r#"let producer = mock.test.resource {
  name = "seed"
}

let consumer = mock.test.resource {
  name = producer.cmment ?? "d"
}
"#,
    )
    .expect("write schema typo coalesce fixture");

    assert_success(&run_cli(&project, &["init", "."]), "init project");
    let output = assert_failure(
        &run_cli(&project, &["validate", "."]),
        "validate schema typo coalesce",
    );
    assert!(
        output.contains("cmment") && output.to_ascii_lowercase().contains("unknown attribute"),
        "validation must retain schema-typed unknown-attribute errors:\n{output}",
    );
}

#[test]
fn updating_authored_list_iterable_expands_before_apply_and_converges() {
    for coalesced in [false, true] {
        let root = tempfile::tempdir().expect("create temporary root");
        let project = root
            .path()
            .join(if coalesced { "coalesced" } else { "plain" });
        write_common_files(&project);
        write_updating_list_iterable(&project, coalesced, false);

        assert_success(&run_cli(&project, &["init", "."]), "init list fixture");
        assert_success(
            &run_cli(&project, &["apply", "--auto-approve", "."]),
            "apply initial list fixture",
        );

        write_updating_list_iterable(&project, coalesced, true);
        let update = plan_json(&project);
        assert!(
            update
                .plan
                .effects()
                .iter()
                .all(|effect| !matches!(effect, Effect::DeferredCreate(_))),
            "an authored desired list is definitive during its producer update: {:#?}",
            update.plan.effects(),
        );
        assert_success(
            &run_cli(&project, &["apply", "--auto-approve", "."]),
            "apply updated list fixture",
        );
        assert_converged(&project, "reapply updated list fixture");
    }
}

#[test]
fn updating_authored_map_iterable_keeps_stable_children_and_converges() {
    for coalesced in [false, true] {
        let root = tempfile::tempdir().expect("create temporary root");
        let project = root
            .path()
            .join(if coalesced { "coalesced" } else { "plain" });
        write_common_files(&project);
        write_updating_map_iterable(&project, coalesced, false);

        assert_success(&run_cli(&project, &["init", "."]), "init map fixture");
        assert_success(
            &run_cli(&project, &["apply", "--auto-approve", "."]),
            "apply initial map fixture",
        );

        write_updating_map_iterable(&project, coalesced, true);
        let update = plan_json(&project);
        assert!(
            update
                .plan
                .effects()
                .iter()
                .all(|effect| !matches!(effect, Effect::DeferredCreate(_))),
            "an authored desired map is definitive during its producer update: {:#?}",
            update.plan.effects(),
        );
        assert_success(
            &run_cli(&project, &["apply", "--auto-approve", "."]),
            "apply updated map fixture",
        );
        assert_converged(&project, "reapply updated map fixture");
    }
}

#[test]
fn update_selected_coalesce_secret_is_hashed_in_state() {
    const SECRET: &str = "coalesce-update-secret-must-not-leak-3961";
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    fs::write(
        project.join("main.crn"),
        r#"let r = mock.test.resource {
  name = "seed"
  comment = "public"
}

let s = mock.test.resource {
  name = "consumer"
  comment = r.comment ?? secret(env("SECRET_V"))
}
"#,
    )
    .expect("write initial secret coalesce fixture");

    assert_success(&run_cli(&project, &["init", "."]), "init secret fixture");
    assert_success(
        &run_cli_with_env(
            &project,
            &["apply", "--auto-approve", "."],
            &[("SECRET_V", SECRET)],
        ),
        "apply initial secret fixture",
    );

    fs::write(
        project.join("main.crn"),
        r#"let r = mock.test.resource {
  name = "seed"
}

let s = mock.test.resource {
  name = "consumer"
  comment = r.comment ?? secret(env("SECRET_V"))
}
"#,
    )
    .expect("write updated secret coalesce fixture");
    assert_success(
        &run_cli_with_env(
            &project,
            &["apply", "--auto-approve", "."],
            &[("SECRET_V", SECRET)],
        ),
        "apply selected secret default",
    );

    let state = fs::read_to_string(project.join("carina.state.json")).expect("read state");
    assert!(
        state.contains("\"$secret\""),
        "the selected secret default must be persisted as a secret hash:\n{state}",
    );
    assert!(
        !state.contains(SECRET),
        "the selected secret default leaked into state plaintext:\n{state}",
    );
}

#[test]
fn selected_public_coalesce_branch_stays_plain_and_plans_only_unrelated_update() {
    const SECRET: &str = "unselected-nested-secret-must-not-taint-siblings-3961";
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    fs::write(
        project.join("main.crn"),
        r#"let r = mock.test.resource {
  name = "seed"
}

let t = mock.test.resource {
  name = "target"
  tags = {
    k = r.identifier ?? secret(env("SECRET_V"))
    p = "plain"
  }
  rule {
    action = r.identifier ?? secret(env("SECRET_V"))
  }
}

let leaf = mock.test.resource {
  name = "leaf"
  comment = r.identifier ?? secret(env("SECRET_V"))
}
"#,
    )
    .expect("write selected-public nested coalesce fixture");

    assert_success(
        &run_cli(&project, &["init", "."]),
        "init selected-public nested fixture",
    );
    assert_success(
        &run_cli_with_env(
            &project,
            &["apply", "--auto-approve", "."],
            &[("SECRET_V", SECRET)],
        ),
        "apply selected-public nested fixture",
    );

    let attributes = state_resource_attributes(&project, "t");
    assert_eq!(attributes["tags"]["k"], "seed-id");
    assert_eq!(attributes["tags"]["p"], "plain");
    assert_eq!(attributes["rules"][0]["action"], "seed-id");
    let leaf_attributes = state_resource_attributes(&project, "leaf");
    assert_eq!(leaf_attributes["comment"], "seed-id");
    let state = fs::read_to_string(project.join("carina.state.json")).expect("read state");
    assert!(
        !state.contains("\"$secret\"") && !state.contains(SECRET),
        "an unselected secret fallback must not taint any state position:\n{state}",
    );

    fs::write(
        project.join("main.crn"),
        r#"let r = mock.test.resource {
  name = "seed"
}

let t = mock.test.resource {
  name = "target"
  comment = "unrelated"
  tags = {
    k = r.identifier ?? secret(env("SECRET_V"))
    p = "plain"
  }
  rule {
    action = r.identifier ?? secret(env("SECRET_V"))
  }
}

let leaf = mock.test.resource {
  name = "leaf"
  comment = r.identifier ?? secret(env("SECRET_V"))
}
"#,
    )
    .expect("write unrelated update for selected-public fixture");
    let plan = run_cli_with_env(&project, &["plan", "."], &[("SECRET_V", SECRET)]);
    assert_success(&plan, "plan unrelated selected-public update");
    let rendered = String::from_utf8_lossy(&plan.stdout);
    assert!(
        rendered.contains("comment")
            && rendered.contains("Plan: 0 to add, 1 to change, 0 to destroy."),
        "the unrelated comment must be the only planned resource update:\n{rendered}",
    );
    assert!(
        !rendered.contains("\n      tags:") && !rendered.contains("\n      rules:"),
        "a selected public branch must not create phantom nested diffs:\n{rendered}",
    );
}

#[test]
fn selected_secret_coalesce_branch_hashes_only_its_nested_child() {
    const SECRET: &str = "selected-nested-secret-must-not-leak-3961";
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    fs::write(
        project.join("main.crn"),
        r#"let r = mock.test.resource {
  name = "seed"
}

let t = mock.test.resource {
  name = "target"
  tags = {
    k = r.comment ?? secret(env("SECRET_V"))
    p = "plain"
  }
}
"#,
    )
    .expect("write selected-secret nested coalesce fixture");

    assert_success(
        &run_cli(&project, &["init", "."]),
        "init selected-secret nested fixture",
    );
    assert_success(
        &run_cli_with_env(
            &project,
            &["apply", "--auto-approve", "."],
            &[("SECRET_V", SECRET)],
        ),
        "apply selected-secret nested fixture",
    );

    let attributes = state_resource_attributes(&project, "t");
    assert!(
        attributes["tags"]["k"].get("$secret").is_some(),
        "the selected secret fallback must be hashed at tags.k: {attributes:#?}",
    );
    assert_eq!(attributes["tags"]["p"], "plain");
    let state = fs::read_to_string(project.join("carina.state.json")).expect("read state");
    assert!(
        !state.contains(SECRET),
        "the selected nested secret leaked into state plaintext:\n{state}",
    );
}

#[test]
fn direct_reference_to_updated_secret_is_hashed_in_consumer_state() {
    const SECRET: &str = "updated-reference-secret-must-not-leak-3961";
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    fs::write(
        project.join("main.crn"),
        r#"let r = mock.test.resource {
  name = "seed"
}
"#,
    )
    .expect("write initial update-secret fixture");

    assert_success(
        &run_cli(&project, &["init", "."]),
        "init update-secret fixture",
    );
    assert_success(
        &run_cli_with_env(
            &project,
            &["apply", "--auto-approve", "."],
            &[("SECRET_V", SECRET)],
        ),
        "apply initial update-secret fixture",
    );

    fs::write(
        project.join("main.crn"),
        r#"let r = mock.test.resource {
  name = "seed"
  comment = secret(env("SECRET_V"))
}

let s = mock.test.resource {
  name = "consumer"
  comment = r.comment
}

let interpolated = mock.test.resource {
  name = "interpolated-consumer"
  comment = "prefix-${r.comment}"
}
"#,
    )
    .expect("write updated secret reference fixture");
    assert_success(
        &run_cli_with_env(
            &project,
            &["apply", "--auto-approve", "."],
            &[("SECRET_V", SECRET)],
        ),
        "apply updated secret reference fixture",
    );

    let state = fs::read_to_string(project.join("carina.state.json")).expect("read state");
    assert!(
        state.contains("\"$secret\""),
        "updated and propagated secret values must be persisted as hashes:\n{state}",
    );
    assert!(
        !state.contains(SECRET),
        "an updated secret propagated through a reference or interpolation leaked into state:\n{state}",
    );
}

#[test]
fn wait_alias_reference_preserves_target_secret_shape_in_consumer_state() {
    const SECRET: &str = "wait-alias-secret-must-not-leak-3961";
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    fs::write(
        project.join("main.crn"),
        r#"let r = mock.test.resource {
  name = "seed"
  comment = secret(env("SECRET_V"))
}

let ready = wait r {
  until = r.name == "seed"
  timeout = 1min
}

let s = mock.test.resource {
  name = "consumer"
  comment = ready.comment
}
"#,
    )
    .expect("write wait-alias secret fixture");

    assert_success(
        &run_cli(&project, &["init", "."]),
        "init wait-alias secret fixture",
    );
    assert_success(
        &run_cli_with_env(
            &project,
            &["apply", "--auto-approve", "."],
            &[("SECRET_V", SECRET)],
        ),
        "apply wait-alias secret fixture",
    );

    let state = fs::read_to_string(project.join("carina.state.json")).expect("read state");
    assert!(
        state.contains("\"$secret\""),
        "a secret propagated through a wait alias must be persisted as a hash:\n{state}",
    );
    assert!(
        !state.contains(SECRET),
        "a wait alias discarded its target's secret shape and leaked plaintext:\n{state}",
    );
}

#[test]
fn wrong_typed_coalesce_default_in_for_iterable_is_rejected_before_apply() {
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    fs::write(
        project.join("main.crn"),
        r#"let producer = mock.test.resource {
  name = "seed"
}

for key, value in (producer.tags ?? 5) {
  mock.test.resource {
    name = "${key}-${value}"
  }
}
"#,
    )
    .expect("write wrong-typed iterable coalesce fixture");

    assert_success(&run_cli(&project, &["init", "."]), "init iterable fixture");
    for (operation, args) in [
        ("validate wrong-typed iterable", &["validate", "."][..]),
        ("plan wrong-typed iterable", &["plan", "."][..]),
    ] {
        let output = assert_failure(&run_cli(&project, args), operation);
        let output = output.to_ascii_lowercase();
        assert!(
            output.contains("map") && output.contains("int"),
            "{operation} must explain the iterable/default type mismatch:\n{output}",
        );
    }
}

fn write_coalesce_chain_fixture(dir: &Path, unparenthesized: bool) {
    let expressions = if unparenthesized {
        vec![
            ("chain", "r.comment ?? r.parent ?? \"d\""),
            ("right", "r.comment ?? (r.parent ?? \"d\")"),
        ]
    } else {
        vec![
            ("left", "(r.comment ?? r.parent) ?? \"d\""),
            ("right", "r.comment ?? (r.parent ?? \"d\")"),
        ]
    };
    let consumers = expressions
        .iter()
        .map(|(name, expression)| {
            format!(
                r#"let {name} = mock.test.resource {{
  name = "{name}"
  comment = {expression}
}}
"#,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        dir.join("main.crn"),
        format!(
            r#"let r = mock.test.resource {{
  name = "seed"
}}

{consumers}"#,
        ),
    )
    .expect("write coalesce chain fixture");
}

fn assert_coalesce_chain_applies(dir: &Path, operation: &str) {
    assert_success(&run_cli(dir, &["validate", "."]), operation);
    assert_success(&run_cli(dir, &["apply", "--auto-approve", "."]), operation);
    assert_converged(dir, operation);
}

#[test]
fn unparenthesized_coalesce_chain_parses_and_resolves_right_associatively() {
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    write_coalesce_chain_fixture(&project, true);
    assert_success(&run_cli(&project, &["init", "."]), "init chain fixture");
    assert_coalesce_chain_applies(&project, "apply unparenthesized coalesce chain");
}

#[test]
fn explicitly_grouped_coalesces_are_semantically_associative() {
    let root = tempfile::tempdir().expect("create temporary root");
    let project = root.path().join("project");
    write_common_files(&project);
    write_coalesce_chain_fixture(&project, false);
    assert_success(
        &run_cli(&project, &["init", "."]),
        "init grouped chain fixture",
    );
    assert_coalesce_chain_applies(&project, "apply both grouped coalesce forms");
}
