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
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .args(args)
            .output()
            .expect("run carina")
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
fn live_apply_plan_matches_plan_for_state_materialized_deferred_for_child() {
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
    project.assert_success("plan", &plan);
    let plan_text = output_text(&plan);

    let apply = project.run(&["apply", "--auto-approve", "--lock=false", "."]);
    project.assert_success("live apply", &apply);
    let apply_text = output_text(&apply);

    let child_line = "mock.test.resource validation_records[0]";
    let update_summary = "Plan: 0 to add, 1 to change, 0 to destroy.";
    for (label, rendered) in [("plan", &plan_text), ("apply", &apply_text)] {
        assert!(
            rendered.contains(child_line) && rendered.contains("'before' → 'after'"),
            "{label} did not materialize the deferred-for child:\n{rendered}",
        );
        assert!(
            rendered.contains(update_summary),
            "{label} did not produce the same child update:\n{rendered}",
        );
    }
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
