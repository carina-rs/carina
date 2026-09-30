use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
}

impl Scenario {
    fn new(producer_name: &str, peer_name: &str) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().to_path_buf();
        let module = project.join("checked");
        fs::create_dir(&module).expect("create module directory");

        fs::write(
            module.join("arguments.crn"),
            r#"arguments {
  value: String {
    validation {
      condition     = value != "bad-id"
      error_message = "value must not be the bad identifier"
    }
  }
  peer: String
}

require value == peer, "value and peer identifiers must match"
"#,
        )
        .expect("write module arguments");
        fs::write(
            module.join("attributes.crn"),
            r#"attributes {
  value: String = value
  peer: String = peer
}
"#,
        )
        .expect("write module attributes");

        fs::write(
            project.join("providers.crn"),
            r#"backend local { path = "carina.state.json" }
provider mock {}

let checked_module = use { source = "./checked" }
"#,
        )
        .expect("write root providers");
        fs::write(
            project.join("producer.crn"),
            format!(
                r#"let producer = mock.test.resource {{
  name = "{producer_name}"
}}

let peer = mock.test.resource {{
  name = "{peer_name}"
}}
"#
            ),
        )
        .expect("write root producers");
        fs::write(project.join("main.crn"), "").expect("write initial root main");

        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            project,
            _temp: temp,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
        assert_success(
            "initial carina apply",
            &scenario.carina(&["apply", "--auto-approve", "."]),
        );
        scenario.write_module_call();
        scenario
    }

    fn write_module_call(&self) {
        fs::write(
            self.project.join("main.crn"),
            r#"let checked = checked_module {
  value = producer.identifier
  peer  = peer.identifier
}
"#,
        )
        .expect("write root module call");
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
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

#[test]
fn multifile_directory_checks_ref_valued_module_validation_and_require() {
    let violating = Scenario::new("bad", "good");
    let plan = violating.carina(&["plan", "."]);
    let output = output_text(&plan);
    assert!(
        !plan.status.success(),
        "violating multi-file directory must fail planning\n{output}"
    );
    for expected in [
        "module `checked_module` instance `checked` constraint failed for argument(s) `value`",
        "value must not be the bad identifier",
        "module `checked_module` instance `checked` constraint failed for argument(s) `peer`, `value`",
        "value and peer identifiers must match",
    ] {
        assert!(
            output.contains(expected),
            "plan error must contain {expected:?}\n{output}"
        );
    }

    let satisfied = Scenario::new("good", "good");
    let plan = satisfied.carina(&["plan", "."]);
    assert_success("satisfied multi-file carina plan", &plan);
    let apply = satisfied.carina(&["apply", "--auto-approve", "."]);
    assert_success("satisfied multi-file carina apply", &apply);
}
