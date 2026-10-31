//! End-to-end regressions for the parser-placeholder expansion boundary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const EMPTY_INTERPOLATION: &str = "empty interpolation `${}` — fill in the expression or remove it";

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
}

struct ModuleScenario {
    _temp: TempDir,
    module: PathBuf,
}

impl ModuleScenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary module");
        let module = temp.path().join("module");
        fs::create_dir(&module).expect("create module directory");
        write(
            &module,
            "arguments.crn",
            "arguments {\n  input: String\n}\n",
        );
        Self {
            _temp: temp,
            module,
        }
    }

    fn write(&self, name: &str, source: &str) {
        write(&self.module, name, source);
    }

    fn validate(&self) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.module)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(["validate", "."])
            .output()
            .expect("run carina validate on module")
    }
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        write(
            &project,
            "providers.crn",
            r#"backend local { path = "carina.state.json" }

provider mock {}
"#,
        );
        write(&project, "main.crn", "");
        write(&project, "locals.crn", "");
        write(&project, "exports.crn", "");

        Self {
            _temp: temp,
            project,
        }
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

    fn write(&self, name: &str, source: &str) {
        write(&self.project, name, source);
    }

    fn state_path(&self) -> PathBuf {
        self.project.join("carina.state.json")
    }
}

fn write(project: &Path, name: &str, source: &str) {
    fs::write(project.join(name), source).expect("write fixture file");
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

fn assert_failure(label: &str, output: &Output) -> String {
    let text = output_text(output);
    assert!(
        !output.status.success(),
        "{label} unexpectedly passed:\n{text}"
    );
    text
}

fn placeholder_messages(output: &Output) -> Vec<String> {
    let mut messages = output_text(output)
        .lines()
        .filter(|line| line.contains(EMPTY_INTERPOLATION))
        .map(|line| {
            line.trim()
                .strip_prefix("Error: ")
                .unwrap_or(line.trim())
                .to_string()
        })
        .collect::<Vec<_>>();
    messages.sort();
    messages
}

#[test]
fn validate_and_plan_reject_unused_empty_interpolation_let_with_same_location() {
    let scenario = Scenario::new();
    scenario.write("locals.crn", "let greeting = \"${}\"\n");

    let validate = scenario.carina(&["validate", "."]);
    let validate_text = assert_failure("carina validate", &validate);
    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    let plan_text = assert_failure("carina plan", &plan);

    let expected = format!("locals.crn:1:17: let `greeting`: {EMPTY_INTERPOLATION}");
    assert!(
        validate_text.contains(&expected),
        "validate did not locate the leftover at `greeting`:\n{validate_text}",
    );
    assert!(
        plan_text.contains(&expected),
        "plan did not locate the leftover at `greeting`:\n{plan_text}",
    );
    let validate_messages = placeholder_messages(&validate);
    let plan_messages = placeholder_messages(&plan);
    assert_eq!(validate_messages.len(), 1, "{validate_messages:#?}");
    assert!(validate_messages[0].ends_with(&expected));
    assert_eq!(validate_messages, plan_messages);
}

#[test]
fn validate_and_plan_report_every_located_empty_interpolation() {
    let scenario = Scenario::new();
    scenario.write(
        "main.crn",
        r#"mock.test.resource {
  name = "${}"
}
"#,
    );
    scenario.write(
        "exports.crn",
        r#"exports {
  broken: String = "${}"
}
"#,
    );

    let validate = scenario.carina(&["validate", "."]);
    assert_failure("carina validate", &validate);
    let plan = scenario.carina(&["plan", "--refresh=false", "."]);
    assert_failure("carina plan", &plan);

    let validate_messages = placeholder_messages(&validate);
    let plan_messages = placeholder_messages(&plan);
    assert_eq!(validate_messages.len(), 2, "{validate_messages:#?}");
    assert!(
        validate_messages.iter().any(|message| {
            message.contains("main.crn:2:11:") && message.contains("attribute `name`:")
        }),
        "resource attribute error has no location: {validate_messages:#?}",
    );
    assert!(
        validate_messages
            .iter()
            .any(|message| message.contains("exports.crn:2:21: exports `broken`:")),
        "export error has no location: {validate_messages:#?}",
    );
    assert_eq!(validate_messages, plan_messages);
}

#[test]
fn destroy_rejects_located_empty_interpolation_without_modifying_state() {
    let scenario = Scenario::new();
    let mut state = carina_state::StateFile::new();
    state
        .upsert_resource(
            carina_state::ResourceState::try_applied(
                "test.resource",
                "existing",
                "mock",
                "existing-provider-id",
            )
            .expect("fixture identifier is non-empty"),
        )
        .expect("fixture state has one unique resource");
    fs::write(
        scenario.state_path(),
        serde_json::to_string_pretty(&state).expect("serialize fixture state"),
    )
    .expect("write fixture state");
    let state_before = fs::read(scenario.state_path()).expect("read state before destroy");

    scenario.write("locals.crn", "let greeting = \"${}\"\n");
    let destroy = scenario.carina(&["destroy", "--auto-approve", "--refresh=false", "."]);
    let destroy_text = assert_failure("carina destroy", &destroy);

    assert!(
        destroy_text.contains(&format!(
            "locals.crn:1:17: let `greeting`: {EMPTY_INTERPOLATION}"
        )),
        "destroy did not report the located parser placeholder:\n{destroy_text}",
    );
    assert_eq!(
        fs::read(scenario.state_path()).expect("read state after failed destroy"),
        state_before,
        "failed destroy must not rewrite or delete state",
    );
}

#[test]
fn module_definition_rejects_empty_interpolation_in_resource_with_location() {
    let scenario = ModuleScenario::new();
    scenario.write(
        "resources.crn",
        "mock.test.resource {\n  name = \"${}\"\n}\n",
    );

    let output = scenario.validate();
    let text = assert_failure("carina validate module", &output);

    assert!(
        text.contains(
            "resources.crn:2:11: mock.test.resource.<pending>: attribute `name`: empty interpolation `${}`"
        ),
        "the module resource leftover must retain its attribute location:\n{text}",
    );
}

#[test]
fn module_definition_reports_argument_reference_at_authored_file_and_span() {
    let source = "arguments { r: String }\n\nlet reg = r\nprovider mock { region = reg }\n";
    let parsed = carina_core::parser::parse_file(
        source,
        Path::new("module/locals.crn"),
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("parse module-shaped CLI fixture");
    let leftovers = parsed
        .validate_expansion_boundary(
            carina_core::parser::ParserPlaceholderBoundary::ModuleDefinition,
        )
        .expect_err("a provider attribute cannot retain a module argument");
    let text = carina_cli::error::AppError::from(leftovers).to_string();

    assert!(
        text.contains(
            "module/locals.crn:3:11: provider `mock` attribute `region`: unresolved module argument: r"
        ),
        "the CLI error must retain the authored module-argument reference: {text}",
    );
}

#[test]
fn module_definition_allows_argument_reference_in_resource() {
    let scenario = ModuleScenario::new();
    scenario.write("resources.crn", "mock.test.resource {\n  name = input\n}\n");

    let output = scenario.validate();
    assert!(
        output.status.success(),
        "a resource attribute is substituted during module expansion:\n{}",
        output_text(&output),
    );
}

#[test]
fn module_definition_allows_derived_argument_variable_used_in_sibling_resource() {
    let scenario = ModuleScenario::new();
    scenario.write(
        "arguments.crn",
        "arguments {\n  environment: 'dev' | 'prod'\n}\n",
    );
    scenario.write(
        "locals.crn",
        "let state_bucket_name = \"carina-registry-state-${environment}\"\n",
    );
    scenario.write(
        "resources.crn",
        "mock.test.resource {\n  name = state_bucket_name\n}\n",
    );

    let output = scenario.validate();
    assert!(
        output.status.success(),
        "a derived module local is inlined and substituted in its sibling resource:\n{}",
        output_text(&output),
    );
}
