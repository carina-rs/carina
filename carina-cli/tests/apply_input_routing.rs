use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Scenario {
    _temp: tempfile::TempDir,
    project: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().to_path_buf();
        fs::write(project.join("main.crn"), "exports {\n}\n").expect("write project");

        let scenario = Self {
            _temp: temp,
            project,
        };
        let init = scenario.carina(&["init", "."]);
        assert_success("carina init", &init);
        scenario
    }

    fn carina(&self, args: &[&str]) -> Output {
        carina(&self.project, args)
    }

    fn save_plan(&self, plan_name: &str) -> PathBuf {
        let plan = self.carina(&["plan", "--refresh=false", "--out", plan_name, "."]);
        assert_success("carina plan --out", &plan);
        let plan_path = self.project.join(plan_name);
        assert!(plan_path.is_file(), "plan should be written to {plan_name}");
        plan_path
    }

    fn save_and_apply(&self, plan_name: &str) {
        self.save_plan(plan_name);
        let apply = self.carina(&["apply", "--auto-approve", plan_name]);
        assert_success("carina apply saved plan", &apply);
        let stdout = String::from_utf8_lossy(&apply.stdout);
        assert!(
            stdout.contains("Using saved plan from "),
            "apply must take the saved-plan path, not merely exit successfully:\n{stdout}",
        );
    }
}

fn carina(current_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(current_dir)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .expect("run carina")
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn apply_routes_extensionless_plan_file_as_saved_plan() {
    Scenario::new().save_and_apply("myplan");
}

#[test]
fn apply_routes_uppercase_json_plan_file_as_saved_plan() {
    Scenario::new().save_and_apply("plan.JSON");
}

#[test]
fn apply_routes_dot_json_plan_file_as_saved_plan() {
    Scenario::new().save_and_apply(".json");
}

#[test]
fn apply_rejects_nonexistent_input_without_guessing_its_kind() {
    let temp = tempfile::tempdir().expect("tempdir");

    let output = carina(temp.path(), &["apply", "does-not-exist"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "missing input must fail");
    assert!(
        stderr.contains("does-not-exist")
            && stderr.contains("project directory")
            && stderr.contains("plan file written by `carina plan --out`"),
        "error must name the path and both accepted input kinds:\n{stderr}",
    );
}

#[test]
fn apply_explains_when_existing_file_is_not_readable_json() {
    let scenario = Scenario::new();

    let output = scenario.carina(&["apply", "main.crn"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "non-plan file must fail");
    assert!(
        stderr.contains("main.crn")
            && stderr.contains("could not be parsed as JSON")
            && stderr.contains("is not a readable Carina plan file")
            && stderr.contains("may be truncated or corrupted")
            && stderr.contains("re-create it with `carina plan --out`")
            && stderr.contains("project directory")
            && stderr.contains("plan file written by `carina plan --out`")
            && stderr.contains("JSON error:")
            && stderr.contains("expected value at line 1 column 1"),
        "error must explain the accepted inputs and retain the serde detail:\n{stderr}",
    );
    assert!(
        !stderr.contains("is not a Carina plan file"),
        "invalid JSON cannot distinguish arbitrary text from a damaged plan:\n{stderr}",
    );
}

#[test]
fn apply_reports_valid_json_without_a_plan_header_as_not_a_carina_plan() {
    let scenario = Scenario::new();
    fs::write(
        scenario.project.join("json-document"),
        r#"{"kind":"not-a-plan"}"#,
    )
    .expect("write JSON document");

    let output = scenario.carina(&["apply", "json-document"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "non-plan JSON file must fail");
    assert!(
        stderr.contains("json-document")
            && stderr.contains("is not a Carina plan file (or the plan file is corrupted)")
            && stderr.contains("project directory")
            && stderr.contains("plan file written by `carina plan --out`")
            && stderr.contains("Detail:")
            && stderr.contains("missing field `version`"),
        "valid JSON without a plan header must be identified as a non-plan:\n{stderr}",
    );
    assert!(
        !stderr.contains("could not be parsed as JSON"),
        "valid JSON must not be described as unparseable:\n{stderr}",
    );
}

#[test]
fn apply_rejects_carina_state_file_as_not_a_plan() {
    let scenario = Scenario::new();
    let state_path = scenario.project.join("carina-state");
    fs::write(
        &state_path,
        serde_json::to_vec(&carina_state::StateFile::new()).expect("serialize Carina state"),
    )
    .expect("write Carina state");

    let output = scenario.carina(&["apply", "carina-state"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "Carina state file must fail");
    assert!(
        stderr.contains("carina-state")
            && stderr.contains("is not a Carina plan file (or the plan file is corrupted)")
            && stderr.contains("missing field `timestamp`")
            && !stderr.contains("Unsupported plan file version"),
        "a Carina state file must not be treated as a saved plan:\n{stderr}",
    );
}

#[test]
fn apply_rejects_tfstate_shaped_json_as_not_a_plan() {
    let scenario = Scenario::new();
    fs::write(
        scenario.project.join("terraform.tfstate"),
        r#"{"version":4,"serial":1}"#,
    )
    .expect("write tfstate-shaped JSON");

    let output = scenario.carina(&["apply", "terraform.tfstate"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "tfstate-shaped JSON must fail");
    assert!(
        stderr.contains("terraform.tfstate")
            && stderr.contains("is not a Carina plan file (or the plan file is corrupted)")
            && stderr.contains("missing field `timestamp`")
            && !stderr.contains("Unsupported plan file version"),
        "tfstate-shaped JSON must not reach the plan version gate:\n{stderr}",
    );
}

#[test]
fn apply_rejects_top_level_json_array_as_not_a_plan() {
    let scenario = Scenario::new();
    fs::write(scenario.project.join("json-array"), "[7]").expect("write JSON array");

    let output = scenario.carina(&["apply", "json-array"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "top-level JSON array must fail");
    assert!(
        stderr.contains("json-array")
            && stderr.contains("is not a Carina plan file (or the plan file is corrupted)")
            && !stderr.contains("Unsupported plan file version"),
        "a top-level array must not be accepted as a plan header:\n{stderr}",
    );
}

#[test]
fn apply_softens_invalid_version_type_as_possible_plan_corruption() {
    let scenario = Scenario::new();
    fs::write(
        scenario.project.join("invalid-version-plan"),
        r#"{"version":"10","timestamp":"x","source_path":"."}"#,
    )
    .expect("write structurally invalid plan");

    let output = scenario.carina(&["apply", "invalid-version-plan"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "invalid version type must fail");
    assert!(
        stderr.contains("invalid-version-plan")
            && stderr.contains("is not a Carina plan file (or the plan file is corrupted)")
            && stderr.contains("project directory")
            && stderr.contains("plan file written by `carina plan --out`")
            && stderr.contains("Detail:")
            && stderr.contains("invalid type: string"),
        "data errors must allow for a corrupted real plan:\n{stderr}",
    );
}

#[test]
fn apply_reports_truncated_saved_plan_as_unreadable_json() {
    let scenario = Scenario::new();
    let plan_path = scenario.save_plan("truncated-plan");
    let plan = fs::read(&plan_path).expect("read saved plan");
    assert!(plan.len() > 200, "saved plan fixture must exceed 200 bytes");
    fs::write(&plan_path, &plan[..200]).expect("truncate saved plan");

    let output = scenario.carina(&["apply", "truncated-plan"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "truncated saved plan must fail");
    assert!(
        stderr.contains("truncated-plan")
            && stderr.contains("could not be parsed as JSON")
            && stderr.contains("is not a readable Carina plan file")
            && stderr.contains("may be truncated or corrupted")
            && stderr.contains("re-create it with `carina plan --out`")
            && stderr.contains("project directory")
            && stderr.contains("plan file written by `carina plan --out`")
            && stderr.contains("JSON error:")
            && stderr.contains("EOF while parsing"),
        "truncated plan error must be neutral and retain the serde detail:\n{stderr}",
    );
    assert!(
        !stderr.contains("is not a Carina plan file"),
        "a truncated saved plan must not be called a non-plan file:\n{stderr}",
    );
}

#[test]
fn apply_reports_saved_plan_truncated_mid_utf8_as_unreadable_json() {
    let scenario = Scenario::new();
    let plan_path = scenario.save_plan("utf8-truncated-plan");
    let mut plan = fs::read(&plan_path).expect("read saved plan");
    let closing_brace = plan
        .iter()
        .rposition(|byte| *byte == b'}')
        .expect("saved plan must be a JSON object");
    plan.truncate(closing_brace);
    plan.extend_from_slice(b",\n  \"truncation_probe\": \"");
    let multibyte = "日".as_bytes();
    plan.extend_from_slice(&multibyte[..multibyte.len() - 1]);
    fs::write(&plan_path, plan).expect("truncate saved plan within UTF-8 character");

    let output = scenario.carina(&["apply", "utf8-truncated-plan"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "truncated saved plan must fail");
    assert!(
        stderr.contains("utf8-truncated-plan")
            && stderr.contains("could not be parsed as JSON")
            && stderr.contains("is not a readable Carina plan file")
            && stderr.contains("may be truncated or corrupted")
            && stderr.contains("re-create it with `carina plan --out`")
            && stderr.contains("JSON error:")
            && stderr.contains("line")
            && stderr.contains("column"),
        "mid-character truncation must use the neutral JSON diagnostic:\n{stderr}",
    );
    assert!(
        !stderr.contains("is not a Carina plan file"),
        "a truncated saved plan must not be called a non-plan file:\n{stderr}",
    );
}

#[test]
fn apply_reports_binary_file_as_unreadable_json() {
    let scenario = Scenario::new();
    fs::write(scenario.project.join("binary-plan"), [0xff, 0xfe, 0x00]).expect("write binary file");

    let output = scenario.carina(&["apply", "binary-plan"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "binary file must fail");
    assert!(
        stderr.contains("binary-plan")
            && stderr.contains("could not be parsed as JSON")
            && stderr.contains("is not a readable Carina plan file")
            && stderr.contains("may be truncated or corrupted")
            && stderr.contains("project directory")
            && stderr.contains("plan file written by `carina plan --out`")
            && stderr.contains("JSON error:")
            && stderr.contains("line")
            && stderr.contains("column"),
        "binary file must use the neutral JSON diagnostic:\n{stderr}",
    );
    assert!(
        !stderr.contains("is not a Carina plan file"),
        "arbitrary bytes cannot be definitively identified as a non-plan:\n{stderr}",
    );
}

#[cfg(unix)]
#[test]
fn apply_reports_dangling_symlink_target_as_missing() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    symlink("missing-plan-target", temp.path().join("dangling-plan"))
        .expect("create dangling symlink");

    let output = carina(temp.path(), &["apply", "dangling-plan"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "dangling symlink must fail");
    assert!(
        stderr.contains("dangling-plan")
            && stderr.contains("symbolic link")
            && stderr.contains("target does not exist")
            && stderr.contains("project directory")
            && stderr.contains("plan file written by `carina plan --out`"),
        "error must distinguish a dangling symlink from an absent input:\n{stderr}",
    );
    assert!(
        !stderr.contains("Apply input 'dangling-plan' does not exist"),
        "the symlink itself exists even though its target does not:\n{stderr}",
    );
}
