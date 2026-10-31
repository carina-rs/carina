//! Directory-scoped coverage for anonymous declared-identity completeness.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const INCOMPLETE_BUDGET_IDENTITY_ERROR: &str = "Anonymous resource of type \
    'mock.budgets.Budget' cannot derive an address because identity value \
    'budget.budget_name' is missing or not known at plan time. Write the identity value or give \
    the resource a name with `let`.";

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create identity-completeness fixture");
        let project = temp.path().to_path_buf();
        fs::write(
            project.join("backend.crn"),
            "backend local { path = \"carina.state.json\" }\n",
        )
        .expect("write sibling backend");
        fs::write(project.join("providers.crn"), "provider mock {}\n")
            .expect("write sibling provider");
        Self {
            _temp: temp,
            project,
        }
    }

    fn write(&self, relative: impl AsRef<Path>, source: &str) {
        let path = self.project.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create fixture directory");
        }
        fs::write(path, source).expect("write fixture source");
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(args)
            .output()
            .expect("run carina")
    }

    fn init(&self) {
        assert_success("carina init", &self.carina(&["init", "."]));
    }
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstatus: {}\noutput:\n{}",
        output.status,
        output_text(output),
    );
}

fn planned_identity(output: &Output, display_type: &str) -> String {
    let rendered = output_text(output);
    let marker = format!("{display_type} ");
    rendered
        .lines()
        .find_map(|line| {
            let suffix = line.split_once(&marker)?.1;
            suffix
                .split_whitespace()
                .next()
                .filter(|identity| identity.contains('_'))
                .map(str::to_string)
        })
        .unwrap_or_else(|| panic!("plan did not render an address for {display_type}:\n{rendered}"))
}

fn state_identity(scenario: &Scenario, resource_type: &str) -> String {
    let state: serde_json::Value = serde_json::from_slice(
        &fs::read(scenario.project.join("carina.state.json")).expect("read local state"),
    )
    .expect("parse local state");
    let rows = state["resources"]
        .as_array()
        .expect("state resources")
        .iter()
        .filter(|row| row["resource_type"] == resource_type)
        .collect::<Vec<_>>();
    let [row] = rows.as_slice() else {
        panic!("expected one {resource_type} state row: {rows:#?}");
    };
    row["identity"]
        .as_str()
        .expect("state identity")
        .to_string()
}

fn assert_validate_and_plan_error(scenario: &Scenario) {
    for args in [
        ["validate", "."].as_slice(),
        ["plan", "--refresh=false", "."].as_slice(),
    ] {
        let output = scenario.carina(args);
        let rendered = output_text(&output);
        assert!(
            !output.status.success() && rendered.contains(INCOMPLETE_BUDGET_IDENTITY_ERROR),
            "carina {} must report the shared typed identity error\nstatus: {}\noutput:\n{rendered}",
            args[0],
            output.status,
        );
    }
}

const BUDGET_WITHOUT_NAME: &str = r#"mock.budgets.Budget {
  budget = {
    budget_type = "COST"
    time_unit = "MONTHLY"
  }
}
"#;

#[test]
fn omitted_identity_value_is_rejected_in_a_multi_file_directory() {
    let scenario = Scenario::new();
    scenario.write("budget.crn", BUDGET_WITHOUT_NAME);
    scenario.init();

    assert_validate_and_plan_error(&scenario);

    scenario.write(
        "budget.crn",
        &format!("let monthly_budget = {BUDGET_WITHOUT_NAME}"),
    );
    assert_success(
        "named carina validate",
        &scenario.carina(&["validate", "."]),
    );
    assert_success(
        "named carina plan",
        &scenario.carina(&["plan", "--refresh=false", "."]),
    );
}

#[test]
fn apply_time_resource_reference_identity_keeps_its_plan_time_address() {
    let scenario = Scenario::new();
    scenario.write(
        "source.crn",
        r#"let source = mock.test.resource {
  name = "identity-source"
}
"#,
    );
    scenario.write(
        "budget.crn",
        r#"mock.budgets.Budget {
  budget = {
    budget_name = source.identifier
    budget_type = "COST"
    time_unit = "MONTHLY"
  }
}
"#,
    );
    scenario.init();

    assert_success(
        "reference identity validate",
        &scenario.carina(&["validate", "."]),
    );
    let initial_plan = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("reference identity initial plan", &initial_plan);
    let planned = planned_identity(&initial_plan, "mock.budgets.Budget");

    assert_success(
        "reference identity apply",
        &scenario.carina(&["apply", "--auto-approve", "--lock=false", "."]),
    );
    assert_eq!(
        state_identity(&scenario, "budgets.Budget"),
        planned,
        "resolving source.identifier during apply must not re-key the Budget",
    );

    let converged = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("reference identity converged plan", &converged);
    assert!(
        output_text(&converged).contains("No changes. Infrastructure is up-to-date."),
        "the post-apply plan must retain the reference-derived address:\n{}",
        output_text(&converged),
    );
}

#[test]
fn plan_time_module_argument_identity_succeeds_in_a_multi_file_directory() {
    let scenario = Scenario::new();
    scenario.write(
        "budget_module/arguments.crn",
        r#"arguments {
  budget_name: String
}
"#,
    );
    scenario.write(
        "budget_module/budget.crn",
        r#"mock.budgets.Budget {
  budget = {
    budget_name = budget_name
    budget_type = "COST"
    time_unit = "MONTHLY"
  }
}
"#,
    );
    scenario.write(
        "modules.crn",
        r#"let budget_module = use { source = "./budget_module" }

budget_module {
  budget_name = "module-budget"
}
"#,
    );
    scenario.init();

    let validate = scenario.carina(&["validate", "."]);
    assert_success("module-argument carina validate", &validate);
    assert!(
        !output_text(&validate).contains(INCOMPLETE_BUDGET_IDENTITY_ERROR),
        "a concrete module argument must not be classified as incomplete",
    );
    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_success("module-argument carina plan", &plan);
    assert!(
        !output_text(&plan).contains(INCOMPLETE_BUDGET_IDENTITY_ERROR),
        "a concrete module argument must produce an anonymous address",
    );
}
