use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const PROMISE: &str = "function 'pick': parameter 'x' is declared as 'dev' | 'prod', but its argument has type String, so validate cannot prove it fits. Pass a value known at validate, or declare the parameter as String and let the resource attribute that receives it check the value.";
const MISMATCH: &str = "function 'bad': return value: expected String, got Int";

struct Scenario {
    directory: TempDir,
}

impl Scenario {
    fn applied() -> Self {
        let scenario = Self {
            directory: tempfile::tempdir().unwrap(),
        };
        fs::write(scenario.path("providers.crn"), "provider mock {}\n").unwrap();
        fs::write(
            scenario.path("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .unwrap();
        scenario.write_call("'dev'", "");
        assert_success(&scenario.run(&["init", "."]));
        assert_success(&scenario.run(&["apply", ".", "--auto-approve"]));
        assert_eq!(
            scenario.json("carina.state.json")["resources"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        scenario.write_call("src.identifier", "");
        scenario
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn write_call(&self, argument: &str, extra: &str) {
        fs::write(
            self.path("main.crn"),
            format!(
                "let src = mock.test.resource {{ name = 'source' }}\nfn pick(x: 'dev' | 'prod'): String {{ x }}\nlet dst = mock.test.resource {{ name = pick({argument}) }}\n{extra}",
            ),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(self.directory.path())
            .args(args)
            .env("CARINA_MOCK_STATE_FILE", self.path("mock-state.json"))
            .env("CARINA_MOCK_DELETE_LOG", self.path("delete.log"))
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .output()
            .unwrap()
    }

    fn json(&self, name: &str) -> Value {
        serde_json::from_slice(&fs::read(self.path(name)).unwrap()).unwrap()
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{}", text(output));
}

fn assert_rejected(output: &Output, message: &str) {
    let rendered = text(output);
    assert!(!output.status.success(), "{rendered}");
    assert_eq!(rendered.matches(message).count(), 1, "{rendered}");
}

#[test]
fn tp14_destroy_skips_promise_and_deletes_applied_resources() {
    let scenario = Scenario::applied();
    assert_success(&scenario.run(&["destroy", ".", "--auto-approve"]));
    assert_eq!(
        scenario.json("carina.state.json")["resources"],
        serde_json::json!([])
    );
    assert_eq!(
        scenario.json("mock-state.json")["states"],
        serde_json::json!({})
    );
    assert_eq!(
        fs::read_to_string(scenario.path("delete.log"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn tp14_state_refresh_skips_promise_and_records_provider_state() {
    let scenario = Scenario::applied();
    let before = scenario.json("carina.state.json");
    let mut provider = scenario.json("mock-state.json");
    let source = provider["states"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .find(|row| row["name"] == "source")
        .unwrap();
    source["comment"] = Value::String("refreshed".into());
    fs::write(
        scenario.path("mock-state.json"),
        serde_json::to_vec(&provider).unwrap(),
    )
    .unwrap();

    assert_success(&scenario.run(&["state", "refresh", "."]));
    let after = scenario.json("carina.state.json");
    assert!(after["serial"].as_u64().unwrap() > before["serial"].as_u64().unwrap());
    let rows = after["resources"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .any(|row| row["current"]["attributes"]["comment"] == "refreshed"),
        "{after}"
    );
    assert!(!scenario.path("delete.log").exists());
}

#[test]
fn tp14_validate_plan_apply_keep_exact_promise_rejection() {
    let scenario = Scenario::applied();
    let state = scenario.json("carina.state.json");
    let provider = scenario.json("mock-state.json");
    let validation = scenario.run(&["validate", ".", "--json"]);
    assert_rejected(&validation, PROMISE);
    let report: Value = serde_json::from_slice(&validation.stdout).unwrap();
    let errors = report["errors"].as_array().unwrap();
    assert_eq!(errors.len(), 1, "{report}");
    assert_eq!(errors[0]["message"], PROMISE);
    for args in [vec!["plan", "."], vec!["apply", ".", "--auto-approve"]] {
        assert_rejected(&scenario.run(&args), PROMISE);
    }
    assert_eq!(scenario.json("carina.state.json"), state);
    assert_eq!(scenario.json("mock-state.json"), provider);
    assert!(!scenario.path("delete.log").exists());
}

#[test]
fn tp14_recovery_keeps_main_definite_mismatch_rejection() {
    let scenario = Scenario::applied();
    scenario.write_call("src.identifier", "fn bad(a: Int): String { a }\n");
    let state = scenario.json("carina.state.json");
    let provider = scenario.json("mock-state.json");
    for args in [
        vec!["destroy", ".", "--auto-approve"],
        vec!["state", "refresh", "."],
    ] {
        let output = scenario.run(&args);
        assert_rejected(&output, MISMATCH);
        assert!(!text(&output).contains(PROMISE), "{}", text(&output));
    }
    assert_eq!(scenario.json("carina.state.json"), state);
    assert_eq!(scenario.json("mock-state.json"), provider);
    assert!(!scenario.path("delete.log").exists());
}
