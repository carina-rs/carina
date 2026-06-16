use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().to_path_buf();
        Self {
            mock_state: project.join("mock-state.json"),
            project,
            _tmp: tmp,
        }
    }

    fn write_config(&self, version: &str) {
        self.write_config_with_name_and_version("r1", version);
    }

    fn write_config_with_name_and_version(&self, name: &str, version: &str) {
        fs::write(
            self.project.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
provider mock {{}}

let r1 = mock.test.resource {{
  name = "{name}"
  tags = {{ version = "{version}" }}
}}
"#
            ),
        )
        .unwrap();
    }

    fn init(&self) {
        let output = carina(&self.project)
            .args(["init", "."])
            .output()
            .expect("failed to execute carina init");
        assert_success("carina init", &output);
    }

    fn apply(&self) -> Output {
        carina(&self.project)
            .args(["apply", ".", "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("failed to execute carina apply")
    }

    fn apply_partial_create(&self) -> Output {
        carina(&self.project)
            .args(["apply", ".", "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_PARTIAL_CREATE_FOR", "*")
            .env("CARINA_MOCK_PARTIAL_CREATE_MISSING", "tags")
            .output()
            .expect("failed to execute carina apply")
    }

    fn apply_partial_update(&self) -> Output {
        carina(&self.project)
            .args(["apply", ".", "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_PARTIAL_UPDATE_FOR", "*")
            .env("CARINA_MOCK_PARTIAL_UPDATE_MISSING", "tags")
            .output()
            .expect("failed to execute carina apply")
    }

    fn plan(&self) -> Output {
        carina(&self.project)
            .args(["plan", "."])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .output()
            .expect("failed to execute carina plan")
    }
}

fn carina(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
        .env_remove("CLICOLOR_FORCE");
    command
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
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn partial_update_records_state_and_next_plan_surfaces_unknown_reason() {
    let scenario = Scenario::new();
    scenario.write_config("one");
    scenario.init();

    let create = scenario.apply();
    assert_success("initial apply", &create);

    scenario.write_config("two");
    let update = scenario.apply_partial_update();
    let update_text = output_text(&update);
    assert_eq!(
        update.status.code(),
        Some(2),
        "partial update apply must exit 2\nstdout/stderr:\n{update_text}"
    );
    assert!(
        update_text.contains("(partial)"),
        "apply output must render partial update\nstdout/stderr:\n{update_text}"
    );

    let state_text = fs::read_to_string(scenario.project.join("carina.state.json")).unwrap();
    assert!(
        !state_text.contains("__carina_unknown"),
        "state file must not persist unknown sentinels:\n{state_text}"
    );
    assert!(
        state_text.contains("\"partial_read\""),
        "state file must persist the partial_read marker:\n{state_text}"
    );
    assert!(
        state_text.contains("\"missing_attributes\""),
        "state file must persist partial missing attributes:\n{state_text}"
    );
    assert!(
        state_text.contains("mock partial update"),
        "state file must persist partial update detail:\n{state_text}"
    );

    let plan = scenario.plan();
    assert_success("carina plan", &plan);
    let plan_text = output_text(&plan);
    assert!(
        plan_text.contains("~ mock.test.resource r1") && plan_text.contains("1 to change"),
        "next plan must show the partially-updated resource as an update\nstdout/stderr:\n{plan_text}"
    );
    assert!(
        plan_text
            .contains("(known after next apply: post-create read failed — mock partial update)"),
        "next plan must surface the partial-update unknown reason\nstdout/stderr:\n{plan_text}"
    );
}

#[test]
fn replace_partial_create_records_state_and_next_plan_surfaces_unknown_reason() {
    let scenario = Scenario::new();
    scenario.write_config_with_name_and_version("r1", "one");
    scenario.init();

    let create = scenario.apply();
    assert_success("initial apply", &create);

    scenario.write_config_with_name_and_version("r1-replacement", "one");
    let replace = scenario.apply_partial_create();
    let replace_text = output_text(&replace);
    assert_eq!(
        replace.status.code(),
        Some(2),
        "replace partial apply must exit 2\nstdout/stderr:\n{replace_text}"
    );
    assert!(
        replace_text.contains("(partial)"),
        "replace apply output must render partial replace\nstdout/stderr:\n{replace_text}"
    );

    let state_text = fs::read_to_string(scenario.project.join("carina.state.json")).unwrap();
    assert!(
        state_text.contains("\"partial_read\""),
        "replace writeback must persist the partial_read marker:\n{state_text}"
    );
    assert!(
        state_text.contains("mock partial create"),
        "state file must persist replace partial detail:\n{state_text}"
    );

    let plan = scenario.plan();
    assert_success("carina plan after replace partial", &plan);
    let plan_text = output_text(&plan);
    assert!(
        plan_text.contains("~ mock.test.resource r1") && plan_text.contains("1 to change"),
        "next plan must show the replace partial resource as an update\nstdout/stderr:\n{plan_text}"
    );
    assert!(
        plan_text
            .contains("(known after next apply: post-create read failed — mock partial create)"),
        "next plan must surface the replace partial reason\nstdout/stderr:\n{plan_text}"
    );
}
