//! Main's conditional checking scope, without internal conversion syntax.

use std::fs;
use std::process::{Command, Output};

const SELECTOR: &str = "CARINA_4260_SELECT_BRANCH";

struct Project(tempfile::TempDir);

impl Project {
    fn new(source: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
        fs::write(
            directory.path().join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .unwrap();
        fs::write(directory.path().join("main.crn"), source).unwrap();
        let project = Self(directory);
        let initialized = project.run(&["init", "."], "good");
        assert!(initialized.status.success(), "{}", rendered(&initialized));
        project
    }

    fn run(&self, arguments: &[&str], selection: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(self.0.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env(SELECTOR, selection)
            .env(
                "CARINA_MOCK_STATE_FILE",
                self.0.path().join("mock-provider-state.json"),
            )
            .args(arguments)
            .output()
            .unwrap()
    }
}

fn rendered(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn no_as_env_branch_checks_only_the_selected_value_during_validate_and_plan() {
    let project = Project::new(concat!(
        "let selection = env('CARINA_4260_SELECT_BRANCH')\n",
        "let choose_bad = lookup({ bad = true good = false }, selection, false)\n",
        "let sink = mock.test.pattern_modes {\n",
        " full = if choose_bad { 'invalid' } else { 'abc' }\n",
        "}\n",
    ));
    for (selection, accepted) in [("good", true), ("bad", false)] {
        for arguments in [
            vec!["validate", "."],
            vec![
                "plan",
                "--refresh=false",
                "--out",
                "conditional.plan.json",
                ".",
            ],
        ] {
            let result = project.run(&arguments, selection);
            let message = rendered(&result);
            assert_eq!(
                result.status.success(),
                accepted,
                "{arguments:?}, {SELECTOR}={selection}: {message}"
            );
            if !accepted {
                assert!(
                    message.contains("full") && message.contains("pattern"),
                    "{message}"
                );
            }
        }
    }
}
