use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_core::resource::{ConcreteValue, Value};
use carina_state::check_and_migrate;
use tempfile::TempDir;

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
}

impl Scenario {
    fn from_fixture(name: &str, wasm_path: &Path) -> Self {
        let tmp = TempDir::new().expect("create temporary project");
        let project = tmp.path().to_path_buf();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        for entry in fs::read_dir(fixture).expect("read fixture directory") {
            let entry = entry.expect("read fixture entry");
            fs::copy(entry.path(), project.join(entry.file_name())).expect("copy fixture file");
        }
        fs::write(
            project.join("providers.crn"),
            format!(
                "provider mock {{\n  source = \"file://{}\"\n}}\n",
                wasm_path.display()
            ),
        )
        .expect("write provider configuration");
        Self { _tmp: tmp, project }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE");
        command
    }

    fn init(&self) {
        let output = self
            .command()
            .args(["init", "."])
            .output()
            .expect("run carina init");
        assert_success("carina init", &output);
    }

    fn apply(&self) -> Output {
        self.command()
            .args(["apply", ".", "--auto-approve", "--lock=false"])
            .output()
            .expect("run carina apply")
    }

    fn state(&self) -> carina_state::state::StateFile {
        let raw =
            fs::read_to_string(self.project.join("carina.state.json")).expect("read local state");
        check_and_migrate(&raw)
            .expect("decode local state")
            .into_state()
    }
}

fn wasm_mock_path() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CLI crate has a workspace parent")
        .to_path_buf();
    ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
        .expect("WASM mock missing: build carina-provider-mock for wasm32-wasip2 before running read_back_e2e")
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

fn assert_string_attribute(
    state: &carina_state::state::StateFile,
    identity: &str,
    key: &str,
    expected: &str,
) {
    let row = state
        .find_resource("mock", "test.read_back", identity)
        .unwrap_or_else(|| panic!("missing state row for {identity}"));
    assert_eq!(
        row.persisted_attributes()
            .and_then(|attributes| attributes.get(key)),
        Some(Value::Concrete(ConcreteValue::String(expected.into()))),
        "unexpected {key} on {identity}",
    );
}

#[test]
fn round3_secret_plaintext_never_appears_in_state_or_identifier() {
    let wasm_path = wasm_mock_path();
    let scenario = Scenario::from_fixture("read_back_within_budget", &wasm_path);
    scenario.init();
    let plaintexts = [
        "round3-create-secret-plaintext",
        "round3-update-secret-plaintext",
    ];
    for plaintext in plaintexts {
        fs::write(
            scenario.project.join("producer.crn"),
            format!(
                "let within_source = mock.test.read_back {{\n  create_read_back_pending_polls = 1\n  present = secret(\"{plaintext}\")\n}}\n"
            ),
        )
        .unwrap();
        let apply = scenario.apply();
        assert_success("apply with a secret attribute", &apply);
        let state = fs::read_to_string(scenario.project.join("carina.state.json")).unwrap();
        let output = output_text(&apply);
        for secret in plaintexts {
            assert!(
                !state.contains(secret),
                "secret plaintext leaked into persisted state"
            );
            assert!(
                !output.contains(secret),
                "secret plaintext leaked into output"
            );
        }
    }
}

#[test]
fn read_back_completion_reaches_cross_file_dependent_in_same_apply() {
    let wasm_path = wasm_mock_path();
    let scenario = Scenario::from_fixture("read_back_within_budget", &wasm_path);
    scenario.init();

    let apply = scenario.apply();
    assert_success("same-apply read-back", &apply);

    let state = scenario.state();
    assert_string_attribute(&state, "within_dependent", "from_read_back", "ready");
}

#[test]
fn exhausted_read_back_is_retried_by_the_next_real_apply() {
    let wasm_path = wasm_mock_path();
    let scenario = Scenario::from_fixture("read_back_budget_exhausted", &wasm_path);
    scenario.init();

    let first = scenario.apply();
    let first_text = output_text(&first);
    assert_eq!(
        first.status.code(),
        Some(1),
        "partial apply with an unresolved dependent must exit 1\nstdout/stderr:\n{first_text}",
    );
    assert!(
        first_text.contains("did not complete within the configured budget")
            && first_text.contains("The value will be filled on the next apply or refresh."),
        "budget exhaustion message was not rendered\nstdout/stderr:\n{first_text}",
    );
    assert!(
        !first_text.contains("add a `wait` block"),
        "partial read-back must not suggest a wait\nstdout/stderr:\n{first_text}",
    );

    let first_state = scenario.state();
    let source = first_state
        .find_resource("mock", "test.read_back", "budget_source")
        .expect("partial source must be persisted");
    let marker = source
        .partial_read()
        .expect("partial source must retain its read marker");
    assert!(marker.missing_attributes().contains("computed"));
    let first_source_identifier = source
        .identifier()
        .expect("partial source must retain its provider identifier")
        .as_str()
        .to_string();
    assert_string_attribute(
        &first_state,
        "budget_present_dependent",
        "from_read_back",
        "present",
    );
    let present_dependent_identifier = first_state
        .find_resource("mock", "test.read_back", "budget_present_dependent")
        .and_then(|row| row.identifier())
        .expect("the first-run dependent must have a provider identifier");
    assert_ne!(
        first_source_identifier,
        present_dependent_identifier.as_str(),
        "two creates must receive distinct provider identifiers",
    );
    assert!(
        first_state
            .find_resource("mock", "test.read_back", "budget_missing_dependent")
            .is_none(),
        "the dependent on the missing attribute must not be created in run 1",
    );

    let second = scenario.apply();
    assert_success("second real apply", &second);
    let second_state = scenario.state();
    let source = second_state
        .find_resource("mock", "test.read_back", "budget_source")
        .expect("refreshed source must remain in state");
    assert!(
        source.partial_read().is_none(),
        "successful re-read must clear the partial marker",
    );
    assert_string_attribute(&second_state, "budget_source", "computed", "ready");
    assert_string_attribute(
        &second_state,
        "budget_source",
        "read_identifier",
        &first_source_identifier,
    );
    assert_string_attribute(
        &second_state,
        "budget_missing_dependent",
        "from_read_back",
        "ready",
    );
}
