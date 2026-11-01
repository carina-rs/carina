//! Real-CLI coverage for the shared post-expansion plan seam.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

struct Project {
    _temp: TempDir,
    root: PathBuf,
    mock_state: PathBuf,
}

impl Project {
    fn new(main: &str) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let root = temp.path().to_path_buf();
        fs::write(
            root.join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .expect("write backend configuration");
        fs::write(root.join("providers.crn"), "provider mock {}\n")
            .expect("write provider configuration");
        fs::write(root.join("main.crn"), main).expect("write main configuration");

        let project = Self {
            mock_state: root.join("mock-provider-state.json"),
            root,
            _temp: temp,
        };
        project.assert_success("carina init", &project.run(&["init", "."]));
        project
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.root, args)
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, &Path)]) -> Output {
        let mut command = self.command_in(&self.root);
        for (name, value) in env {
            command.env(name, value);
        }
        command.args(args).output().expect("run carina")
    }

    fn run_in(&self, directory: &Path, args: &[&str]) -> Output {
        self.command_in(directory)
            .args(args)
            .output()
            .expect("run carina")
    }

    fn command_in(&self, directory: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(directory)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state);
        command
    }

    fn assert_success(&self, label: &str, output: &Output) {
        assert!(
            output.status.success(),
            "{label} failed:\n{}",
            output_text(output),
        );
    }

    fn write_main(&self, main: &str) {
        fs::write(self.root.join("main.crn"), main).expect("replace main configuration");
    }

    fn inject_generated_rule(&self, source_name: &str, action: &str) {
        let mut provider: Value = read_json(&self.mock_state);
        let source = provider["states"]
            .as_object_mut()
            .expect("mock provider resources")
            .values_mut()
            .find(|resource| resource["name"] == source_name)
            .unwrap_or_else(|| panic!("missing mock provider resource {source_name:?}"));
        source["generated_rules"] = serde_json::json!([{ "action": action }]);
        write_json(&self.mock_state, &provider);
    }

    fn write_upstream(&self, name: &str, main: &str) -> PathBuf {
        let directory = self
            .root
            .parent()
            .expect("downstream project has a parent")
            .join(name);
        fs::create_dir_all(&directory).expect("create upstream project");
        fs::write(
            directory.join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .expect("write upstream backend configuration");
        fs::write(directory.join("providers.crn"), "provider mock {}\n")
            .expect("write upstream provider configuration");
        fs::write(directory.join("main.crn"), main).expect("write upstream configuration");
        directory
    }

    fn add_upstream(&self, name: &str, main: &str) -> PathBuf {
        let directory = self.write_upstream(name, main);
        let init = self.run_in(&directory, &["init", "."]);
        self.assert_success("upstream carina init", &init);
        let apply = self.run_in(
            &directory,
            &["apply", "--auto-approve", "--lock=false", "."],
        );
        self.assert_success("upstream carina apply", &apply);
        directory
    }

    fn state_resource_attributes(&self, identity: &str) -> serde_json::Map<String, Value> {
        let state: Value = read_json(&self.root.join("carina.state.json"));
        let resource = state["resources"]
            .as_array()
            .expect("state resources")
            .iter()
            .find(|resource| resource["identity"] == identity)
            .unwrap_or_else(|| panic!("missing state resource {identity:?}"));
        resource["current"]["attributes"]
            .as_object()
            .expect("state resource attributes")
            .clone()
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn write_json(path: &Path, value: &Value) {
    fs::write(
        path,
        serde_json::to_vec_pretty(value).expect("serialize JSON fixture"),
    )
    .unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

#[test]
fn live_apply_and_cached_plan_match_refreshing_plan_for_state_materialized_child() {
    let project = Project::new(
        r#"let cert = mock.test.deferred_source {
  name = "certificate"
}
"#,
    );
    let initial_apply = project.run(&["apply", "--auto-approve", "--lock=false", "."]);
    project.assert_success("initial apply", &initial_apply);
    project.inject_generated_rule("certificate", "validation-record");

    project.write_main(
        r#"let cert = mock.test.deferred_source {
  name = "certificate"
}

let validation_records = for rule in cert.generated_rules {
  mock.test.resource {
    name    = rule.action
    comment = "before"
  }
}
"#,
    );
    let materialize = project.run(&["apply", "--auto-approve", "--lock=false", "."]);
    project.assert_success("materialize deferred-for child", &materialize);

    project.write_main(
        r#"let cert = mock.test.deferred_source {
  name = "certificate"
}

let validation_records = for rule in cert.generated_rules {
  mock.test.resource {
    name    = rule.action
    comment = "after"
  }
}
"#,
    );

    let plan = project.run(&["plan", "."]);
    project.assert_success("refreshing plan", &plan);
    let plan_text = output_text(&plan);

    let cached_plan = project.run(&["plan", "--refresh=false", "."]);
    project.assert_success("cached-state plan", &cached_plan);
    let cached_plan_text = output_text(&cached_plan);

    let apply = project.run(&["apply", "--auto-approve", "--lock=false", "."]);
    project.assert_success("live apply", &apply);
    let apply_text = output_text(&apply);

    let child_line = "mock.test.resource validation_records[0]";
    let update_summary = "Plan: 0 to add, 1 to change, 0 to destroy.";
    for (label, rendered) in [
        ("refreshing plan", &plan_text),
        ("cached-state plan", &cached_plan_text),
        ("apply", &apply_text),
    ] {
        assert!(
            rendered.contains(child_line) && rendered.contains("'before' → 'after'"),
            "{label} did not materialize the deferred-for child:\n{rendered}",
        );
        assert!(
            rendered.contains(update_summary),
            "{label} did not produce the same child update:\n{rendered}",
        );
        assert!(
            !rendered.contains("Plan: 1 to add"),
            "{label} produced a phantom child create:\n{rendered}",
        );
    }
    assert_eq!(
        project.state_resource_attributes("validation_records[0]")["comment"],
        "after",
        "live apply must persist the materialized child's updated value",
    );
    let follow_up = project.run(&["plan", "."]);
    project.assert_success("follow-up plan", &follow_up);
    let follow_up_text = output_text(&follow_up);
    assert!(
        follow_up_text.contains("No changes"),
        "follow-up plan must converge:\n{follow_up_text}",
    );
}

#[test]
fn live_apply_uses_apply_mode_for_upstream_value_on_materialized_child() {
    let temp = tempfile::tempdir().expect("create temporary project root");
    let root = temp.path().join("downstream");
    fs::create_dir_all(&root).expect("create downstream project");
    fs::write(
        root.join("backend.crn"),
        "backend local { path = 'carina.state.json' }\n",
    )
    .expect("write backend configuration");
    fs::write(root.join("providers.crn"), "provider mock {}\n")
        .expect("write provider configuration");
    fs::write(
        root.join("main.crn"),
        r#"let cert = mock.test.deferred_source {
  name = "certificate"
}
"#,
    )
    .expect("write initial configuration");
    let project = Project {
        mock_state: root.join("mock-provider-state.json"),
        root,
        _temp: temp,
    };
    project.assert_success("carina init", &project.run(&["init", "."]));
    let initial_apply = project.run(&["apply", "--auto-approve", "--lock=false", "."]);
    project.assert_success("initial apply", &initial_apply);
    project.inject_generated_rule("certificate", "validation-record");

    project.write_main(
        r#"let cert = mock.test.deferred_source {
  name = "certificate"
}

let validation_records = for rule in cert.generated_rules {
  mock.test.resource {
    name    = rule.action
    comment = "before"
  }
}
"#,
    );
    let materialize = project.run(&["apply", "--auto-approve", "--lock=false", "."]);
    project.assert_success("materialize deferred-for child", &materialize);

    let upstream = project.add_upstream(
        "upstream",
        r#"let seed = mock.test.resource {
  name = "upstream-seed"
}

exports {}
"#,
    );
    fs::write(
        upstream.join("main.crn"),
        r#"exports {
  child_comment: String = "unused-until-upstream-apply"
}
"#,
    )
    .expect("declare an export missing from upstream state");
    project.write_main(
        r#"let up = upstream_state {
  source = "../upstream"
}

let cert = mock.test.deferred_source {
  name = "certificate"
}

let imported = mock.test.resource {
  name = "imported-name"
}

import {
  to = mock.test.resource 'imported'
  id = "${up.child_comment}"
}

let validation_records = for rule in cert.generated_rules {
  mock.test.resource {
    name    = rule.action
    comment = "after"
  }
}
"#,
    );

    let captured_plan = project.root.join("live-apply-plan.json");
    let apply = project.run_with_env(
        &["apply", "--lock=false", "."],
        &[(
            "CARINA_TEST_CAPTURE_LIVE_APPLY_PLAN",
            captured_plan.as_path(),
        )],
    );
    project.assert_success("live apply with unresolved upstream import", &apply);
    let apply_text = output_text(&apply);
    assert!(
        apply_text.contains("mock.test.resource validation_records[0]")
            && apply_text.contains("'before' → 'after'"),
        "ApplyMode must still build the materialized-child update:\n{apply_text}",
    );
    let captured = read_json(&captured_plan);
    let encoded_plan = captured.to_string();
    assert!(
        encoded_plan.contains("ResourceRef") && encoded_plan.contains("child_comment"),
        "ApplyMode must preserve the strict unresolved import identifier: {captured:#?}",
    );
    assert!(
        !encoded_plan.contains("UpstreamRef") && !encoded_plan.contains("upstream_ref"),
        "ApplyMode must not stamp PlanMode's upstream marker: {captured:#?}",
    );
}

#[test]
fn plan_json_prints_moved_hint_only_to_stderr() {
    let project = Project::new(
        r#"let old_resource = mock.test.resource {
  name = "stable-name"
}
"#,
    );
    let initial_apply = project.run(&["apply", "--auto-approve", "--lock=false", "."]);
    project.assert_success("initial apply", &initial_apply);
    project.write_main(
        r#"let new_resource = mock.test.resource {
  name = "stable-name"
}
"#,
    );

    let plan = project.run(&["plan", "--json", "--refresh=false", "."]);
    project.assert_success("JSON plan", &plan);
    let stdout = String::from_utf8(plan.stdout).expect("plan stdout is UTF-8");
    let stderr = String::from_utf8(plan.stderr).expect("plan stderr is UTF-8");
    serde_json::from_str::<Value>(&stdout).expect("plan stdout remains pure JSON");

    let warning = "Warning: this plan destroys mock.test.resource 'old_resource' and creates mock.test.resource 'new_resource'";
    assert!(
        stderr.contains(warning),
        "real carina plan path did not print the moved hint to stderr:\n{stderr}",
    );
    assert!(
        stderr.contains("from = mock.test.resource 'old_resource'")
            && stderr.contains("to   = mock.test.resource 'new_resource'"),
        "stderr did not contain the complete moved block:\n{stderr}",
    );
    assert!(
        !stdout.contains("Warning: this plan destroys") && !stdout.contains("moved {"),
        "moved hint contaminated JSON stdout:\n{stdout}",
    );
}

fn project_with_moved_hint() -> Project {
    let project = Project::new(
        r#"let old_resource = mock.test.resource {
  name = "stable-name"
}
"#,
    );
    let initial_apply = project.run(&["apply", "--auto-approve", "--lock=false", "."]);
    project.assert_success("initial apply", &initial_apply);
    project.write_main(
        r#"let new_resource = mock.test.resource {
  name = "stable-name"
}
"#,
    );
    project
}

fn assert_moved_hint(stderr: &[u8], label: &str) {
    let stderr = String::from_utf8_lossy(stderr);
    assert!(
        stderr.contains("Warning: this plan destroys mock.test.resource 'old_resource'")
            && stderr.contains("  moved {")
            && stderr.contains("from = mock.test.resource 'old_resource'")
            && stderr.contains("to   = mock.test.resource 'new_resource'"),
        "{label} did not print the complete moved-block hint on stderr:\n{stderr}",
    );
}

#[test]
fn live_apply_prints_moved_hint_to_stderr() {
    let project = project_with_moved_hint();

    let apply = project.run(&["apply", "--auto-approve", "--lock=false", "."]);
    project.assert_success("live apply", &apply);
    assert_moved_hint(&apply.stderr, "live apply");
}

#[test]
fn saved_plan_apply_prints_moved_hint_to_stderr() {
    let project = project_with_moved_hint();
    let plan = project.run(&["plan", "--refresh=false", "--out", "moved.plan.json", "."]);
    project.assert_success("save plan", &plan);

    let saved_plan: Value = read_json(&project.root.join("moved.plan.json"));
    let current_states = saved_plan["current_states"]
        .as_array()
        .expect("saved plan current_states");
    assert!(
        current_states.iter().any(|entry| {
            entry["kind"] == "managed" && entry["id"]["identity"] == "old_resource"
        }),
        "saved plan must retain the orphan row needed by the hint: {current_states:#?}",
    );

    let apply = project.run(&["apply", "--auto-approve", "--lock=false", "moved.plan.json"]);
    project.assert_success("saved-plan apply", &apply);
    assert_moved_hint(&apply.stderr, "saved-plan apply");
}

fn write_moved_legacy_scope_project(project: &Project, prevent_destroy: bool) -> PathBuf {
    let module = project.root.join("net");
    fs::create_dir_all(&module).expect("create net module");
    fs::write(
        module.join("arguments.crn"),
        "arguments { marker: String = \"net\" }\n",
    )
    .expect("write module arguments");
    fs::write(
        module.join("gateway.crn"),
        "let igw = mock.test.resource { name = \"igw\" }\n",
    )
    .expect("write module gateway");
    let directives = prevent_destroy
        .then_some("  directives { prevent_destroy = true }\n")
        .unwrap_or_default();
    fs::write(
        module.join("network.crn"),
        format!(
            "let vpc = mock.test.resource {{\n  name   = \"vpc\"\n  parent = igw.name\n{directives}}}\n"
        ),
    )
    .expect("write module network");
    project.write_main(
        r#"let net_module = use { source = "./net" }
let net = net_module {}

moved {
  from = mock.test.resource "igw"
  to   = mock.test.resource "net.igw"
}

moved {
  from = mock.test.resource "vpc"
  to   = mock.test.resource "net.vpc"
}
"#,
    );

    let state_path = project.root.join("carina.state.json");
    write_json(
        &state_path,
        &serde_json::json!({
            "version": 20,
            "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
            "serial": 5,
            "lineage": "saved-plan-moved-legacy-scope",
            "carina_version": "legacy",
            "resources": [
                {
                    "resource_type": "test.resource",
                    "identity": "igw",
                    "provider": "mock",
                    "current": {
                        "kind": "applied",
                        "identifier": "provider-igw",
                        "attributes": {"name": "igw"},
                        "nested_write_only": {}
                    },
                    "binding": "igw"
                },
                {
                    "resource_type": "test.resource",
                    "identity": "vpc",
                    "provider": "mock",
                    "current": {
                        "kind": "applied",
                        "identifier": "provider-vpc",
                        "attributes": {"name": "vpc", "parent": "igw"},
                        "nested_write_only": {}
                    },
                    "binding": "vpc",
                    "dependency_bindings": ["igw"]
                }
            ],
            "exports": {}
        }),
    );
    state_path
}

#[test]
fn saved_plan_legacy_migration_uses_the_planned_move_scope_before_drift_abort() {
    let project = Project::new("");
    let state_path = write_moved_legacy_scope_project(&project, false);

    let plan = project.run(&[
        "plan",
        "--refresh=false",
        "--out",
        "legacy-moves.plan.json",
        ".",
    ]);
    project.assert_success("save moved legacy plan", &plan);

    // Force the saved-plan drift gate after the lock-held schema migration.
    // The persisted migration must still use the exact move-scoped classifier
    // that produced the plan, even though no plan effect is executed.
    let mut drifted = read_json(&state_path);
    drifted["resources"][1]["current"]["attributes"]["parent"] =
        Value::String("drifted".to_string());
    write_json(&state_path, &drifted);

    let apply = project.run(&[
        "apply",
        "--auto-approve",
        "--lock=false",
        "legacy-moves.plan.json",
    ]);
    assert!(
        !apply.status.success(),
        "saved-plan apply must fail after the lock-held migration:\n{}",
        output_text(&apply),
    );

    let migrated = read_json(&state_path);
    assert_eq!(migrated["version"], Value::from(21));
    let dependency = &migrated["resources"][1]["dependency_bindings"][0];
    assert_eq!(
        dependency["identity"],
        Value::String("net.igw".to_string()),
        "apply --plan must classify the legacy edge with the saved net module scope: {dependency:#?}",
    );
}

#[test]
fn destroy_legacy_migration_uses_the_resolved_move_scope_before_prevent_destroy() {
    let project = Project::new("");
    let state_path = write_moved_legacy_scope_project(&project, true);

    let destroy = project.run(&[
        "destroy",
        "--auto-approve",
        "--lock=false",
        "--refresh=false",
        ".",
    ]);
    assert!(
        !destroy.status.success(),
        "prevent_destroy must refuse the destroy after migration:\n{}",
        output_text(&destroy),
    );
    assert!(
        output_text(&destroy).contains("prevent_destroy"),
        "destroy must reach the configured prevent_destroy refusal:\n{}",
        output_text(&destroy),
    );

    let migrated = read_json(&state_path);
    assert_eq!(migrated["version"], Value::from(21));
    let dependency = &migrated["resources"][1]["dependency_bindings"][0];
    assert_eq!(
        dependency["identity"],
        Value::String("net.igw".to_string()),
        "destroy must classify the legacy edge with the resolved net module scope: {dependency:#?}",
    );
}
