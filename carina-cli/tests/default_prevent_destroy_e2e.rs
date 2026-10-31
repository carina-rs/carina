use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        fs::write(project.join("providers.crn"), "provider mock {}\n")
            .expect("write provider configuration");
        fs::write(
            project.join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .expect("write backend configuration");
        fs::write(project.join("exports.crn"), "exports { ready = true }\n")
            .expect("write sibling configuration");

        Self {
            mock_state: project.join("mock-state.json"),
            project,
            _temp: temp,
        }
    }

    fn write_resource(&self, name: &str, explicit_opt_out: bool) {
        let directives = if explicit_opt_out {
            "  directives { prevent_destroy = false }\n"
        } else {
            ""
        };
        fs::write(
            self.project.join("resources.crn"),
            format!(
                "let account = mock.test.default_prevent_destroy {{\n  name = \"{name}\"\n{directives}}}\n"
            ),
        )
        .expect("write resource configuration");
    }

    fn remove_resource(&self) {
        fs::write(self.project.join("resources.crn"), "").expect("remove resource configuration");
    }

    fn write_resource_with_expression(&self, name: &str) {
        fs::write(
            self.project.join("flags.crn"),
            "let allow_destroy = false\n",
        )
        .expect("write sibling directive input");
        fs::write(
            self.project.join("resources.crn"),
            format!(
                "let account = mock.test.default_prevent_destroy {{\n  name = \"{name}\"\n  directives {{ prevent_destroy = allow_destroy }}\n}}\n"
            ),
        )
        .expect("write resource configuration with directive expression");
    }

    fn run(&self, args: &[&str]) -> Output {
        command(&self.project)
            .args(args)
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("run carina")
    }
}

fn command(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    command
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstatus: {}\noutput:\n{}",
        output.status,
        output_text(output)
    );
}

fn assert_schema_default_refusal(label: &str, output: &Output) {
    let rendered = output_text(output);
    assert!(
        !output.status.success(),
        "{label} unexpectedly succeeded\noutput:\n{rendered}"
    );
    assert!(
        rendered.contains("resource type is protected from destruction by default")
            && rendered.contains("directives { prevent_destroy = false }"),
        "{label} must name the schema default and exact opt-out\noutput:\n{rendered}"
    );
}

#[test]
fn schema_default_protects_remove_replace_and_destroy_until_explicit_opt_out() {
    let scenario = Scenario::new();
    scenario.write_resource("account-v1", false);
    assert_success("carina init", &scenario.run(&["init", "."]));
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.remove_resource();
    assert_schema_default_refusal("removed-resource plan", &scenario.run(&["plan", "."]));

    scenario.write_resource("account-v2", false);
    assert_schema_default_refusal("replacement plan", &scenario.run(&["plan", "."]));
    assert_schema_default_refusal(
        "destroy",
        &scenario.run(&["destroy", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.write_resource("account-v2", true);
    assert_success("opted-out replacement plan", &scenario.run(&["plan", "."]));
    assert_success(
        "opted-out replacement apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.remove_resource();
    assert_success("opted-out removal plan", &scenario.run(&["plan", "."]));

    scenario.write_resource("account-v2", true);
    assert_success(
        "opted-out destroy",
        &scenario.run(&["destroy", ".", "--auto-approve", "--lock=false"]),
    );
}

#[test]
fn non_literal_false_is_valid_but_does_not_opt_out() {
    let scenario = Scenario::new();
    scenario.write_resource_with_expression("account-v1");
    assert_success("carina init", &scenario.run(&["init", "."]));
    let validate = scenario.run(&["validate", "."]);
    assert_success("carina validate", &validate);
    let validate_output = output_text(&validate);
    assert!(
        validate_output.contains("only a literal `false` opts out of schema-default protection"),
        "validate must warn about the ignored non-literal opt-out:\n{validate_output}"
    );
    assert_success(
        "initial apply",
        &scenario.run(&["apply", ".", "--auto-approve", "--lock=false"]),
    );

    scenario.write_resource_with_expression("account-v2");
    let plan = scenario.run(&["plan", "."]);
    assert_schema_default_refusal("non-literal false replacement plan", &plan);
    let plan_output = output_text(&plan);
    assert!(
        plan_output.contains("only a literal `false` opts out of schema-default protection"),
        "plan must warn about the ignored non-literal opt-out:\n{plan_output}"
    );
}
