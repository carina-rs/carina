//! End-to-end diagnostics for unsafe anonymous loop identities.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
}

impl Scenario {
    fn from_fixture(name: &str) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        for entry in fs::read_dir(&fixture).expect("read anonymous-loop error fixture") {
            let entry = entry.expect("read fixture entry");
            fs::copy(entry.path(), project.join(entry.file_name())).expect("copy fixture file");
        }
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
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn assert_validate_and_plan_error(fixture: &str, expected: &str) {
    let scenario = Scenario::from_fixture(fixture);
    assert_success("carina init", &scenario.carina(&["init", "."]));

    for args in [
        ["validate", "."].as_slice(),
        ["plan", "--refresh=false", "."].as_slice(),
    ] {
        let output = scenario.carina(args);
        let command = args[0];
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "carina {command} unexpectedly succeeded\nstdout:\n{}\nstderr:\n{stderr}",
            String::from_utf8_lossy(&output.stdout),
        );
        let typed_text = stderr
            .lines()
            .filter_map(|line| line.strip_prefix("Error: "))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            typed_text,
            expected,
            "carina {command} must print the exact typed diagnostic\n\
             stdout:\n{}\nstderr:\n{stderr}",
            String::from_utf8_lossy(&output.stdout),
        );
    }
}

#[test]
fn anonymous_loop_attribute_derived_error() {
    assert_validate_and_plan_error(
        "anonymous_loop_attribute_derived_error",
        concat!(
            "Anonymous resources produced by the unbound `for` at main.crn:1 cannot be ",
            "identified safely because multiple 'mock.test.Thing' elements derive identity ",
            "from mutable attributes.\n",
            "Rewrite the loop with a binding and migrate its existing state addresses:\n\n",
            "let thing = for name in ['alpha', 'beta'] {\n",
            "  mock.test.Thing {\n",
            "    name = name\n",
            "  }\n",
            "}\n\n",
            "moved {\n",
            "  from = mock.test.Thing '_for0[0]'\n",
            "  to = mock.test.Thing 'thing[0]'\n",
            "}\n\n",
            "moved {\n",
            "  from = mock.test.Thing '_for0[1]'\n",
            "  to = mock.test.Thing 'thing[1]'\n",
            "}",
        ),
    );
}

#[test]
fn anonymous_loop_identical_identity_error() {
    assert_validate_and_plan_error(
        "anonymous_loop_identical_identity_error",
        concat!(
            "Anonymous resources produced by the unbound `for` at main.crn:1 cannot be ",
            "identified safely because multiple 'mock.test.resource' elements have identical ",
            "stable identity inputs.\n",
            "Rewrite the loop with a binding and migrate its existing state addresses:\n\n",
            "let resource = for name in ['same', 'same'] {\n",
            "  mock.test.resource {\n",
            "    name = name\n",
            "  }\n",
            "}\n\n",
            "moved {\n",
            "  from = mock.test.resource '_for0[0]'\n",
            "  to = mock.test.resource 'resource[0]'\n",
            "}\n\n",
            "moved {\n",
            "  from = mock.test.resource '_for0[1]'\n",
            "  to = mock.test.resource 'resource[1]'\n",
            "}",
        ),
    );
}

#[test]
fn anonymous_loop_moved_to_legacy() {
    assert_validate_and_plan_error(
        "anonymous_loop_moved_to_legacy",
        concat!(
            "The `moved.to` address mock.test.resource '_for0[0]' is a legacy synthetic loop ",
            "address and cannot be a destination. Use the current anonymous address ",
            "mock.test.resource 'mock_test_resource_96081a11' instead.",
        ),
    );
}
