use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

#[derive(Clone, Copy, Debug)]
enum ApplyMode {
    Live,
    SavedPlan,
}

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    op_log: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().to_path_buf();
        let module = project.join("checked");
        fs::create_dir(&module).expect("create module directory");
        fs::write(
            module.join("main.crn"),
            r#"arguments {
  value: String {
    validation {
      condition     = value != "bad-id"
      error_message = "replacement identifier must not be bad-id"
    }
  }
}

attributes {
  value: String = value
}
"#,
        )
        .expect("write validation module");

        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            op_log: project.join("op.log"),
            project,
            _temp: temp,
        };
        scenario.write_config(false);
        let init = scenario.carina(&["init", "."]);
        assert_success("carina init", &init);
        let initial_apply = scenario.carina(&["apply", "--auto-approve", "."]);
        assert_success("initial apply", &initial_apply);
        fs::write(&scenario.op_log, "").expect("clear initial operation log");
        scenario.write_config(true);
        scenario
    }

    fn write_config(&self, replacement: bool) {
        let producer_name = if replacement { "bad" } else { "good" };
        let consumer = if replacement {
            r#"
let checked = checked_module {
  value = producer.identifier
}

let consumer = mock.test.resource {
  name    = "consumer"
  comment = checked.value
}
"#
        } else {
            ""
        };
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let checked_module = use {{ source = "./checked" }}

let producer = mock.test.resource {{
  name = "{producer_name}"
}}
{consumer}"#
            ),
        )
        .expect("write root configuration");
    }

    fn apply(&self, mode: ApplyMode) -> Output {
        match mode {
            ApplyMode::Live => self.carina(&["apply", "--auto-approve", "."]),
            ApplyMode::SavedPlan => {
                let plan = self.carina(&["plan", "--out", "plan.json", "."]);
                assert_success("carina plan --out", &plan);
                self.carina(&["apply", "--auto-approve", "plan.json"])
            }
        }
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.op_log)
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_replacement_constraint_blocks_consumer(mode: ApplyMode) {
    let scenario = Scenario::new();
    let apply = scenario.apply(mode);
    let output = output_text(&apply);
    assert!(
        !apply.status.success(),
        "{mode:?} apply must reject the replacement value\n{output}"
    );
    assert!(
        output.contains("replacement identifier must not be bad-id"),
        "{mode:?} apply must report the authored validation message\n{output}"
    );

    let operations = fs::read_to_string(&scenario.op_log).expect("read operation log");
    assert!(
        operations.contains("create test.resource.producer"),
        "{mode:?} apply must publish the replacement value before it becomes checkable: {operations:?}"
    );
    assert!(
        !operations.contains("create test.resource.consumer"),
        "{mode:?} apply must gate the consumer before provider dispatch: {operations:?}"
    );
}

#[test]
fn module_constraint_is_rechecked_after_replacement_for_live_and_saved_apply() {
    for mode in [ApplyMode::Live, ApplyMode::SavedPlan] {
        assert_replacement_constraint_blocks_consumer(mode);
    }
}
