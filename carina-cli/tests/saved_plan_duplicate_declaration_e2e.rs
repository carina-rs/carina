use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Scenario {
    _temp: tempfile::TempDir,
    project: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path().to_path_buf();
        let module = project.join("module");
        fs::create_dir_all(&module).expect("create module directory");
        fs::write(
            project.join("main.crn"),
            r#"backend local { path = "state.json" }

let component = use { source = "./module" }
let instance = component { }
"#,
        )
        .expect("write root configuration");
        fs::write(
            module.join("main.crn"),
            "attributes {\n  value = \"clean\"\n}\n",
        )
        .expect("write module");

        let scenario = Self {
            _temp: temp,
            project,
        };
        let output = scenario.carina(&["init", "."]);
        assert_success("carina init", &output);
        scenario
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

    fn save_plan(&self) {
        let output = self.carina(&["plan", "--refresh=false", "--out", "plan.json", "."]);
        assert_success("carina plan --out", &output);
    }

    fn apply_plan(&self) -> Output {
        self.carina(&["apply", "--auto-approve", "plan.json"])
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

#[test]
fn placeholder_free_saved_plan_ignores_module_duplicate_added_after_plan() {
    let scenario = Scenario::new();
    scenario.save_plan();
    fs::write(
        scenario.project.join("module/duplicate.crn"),
        "attributes {\n  value = \"duplicate\"\n}\n",
    )
    .expect("write duplicate module declaration");

    let output = scenario.apply_plan();
    assert_success(
        "placeholder-free saved-plan apply after unrelated module error",
        &output,
    );
}

#[test]
fn saved_plan_apply_accepts_clean_module() {
    let scenario = Scenario::new();
    scenario.save_plan();

    let output = scenario.apply_plan();
    assert_success("carina apply saved plan", &output);
}
