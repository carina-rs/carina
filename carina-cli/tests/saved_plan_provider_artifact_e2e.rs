use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    provider_source: PathBuf,
    state: PathBuf,
}

impl Scenario {
    fn new(wasm_path: &Path) -> Self {
        let temp = tempfile::tempdir().expect("create temporary project");
        let project = temp.path().to_path_buf();
        let provider_source = project.join("provider.wasm");
        fs::copy(wasm_path, &provider_source).expect("copy mock provider fixture");
        fs::write(
            project.join("providers.crn"),
            format!(
                "provider mock {{\n  source = \"file://{}\"\n}}\n",
                provider_source.display()
            ),
        )
        .expect("write provider configuration");
        fs::write(
            project.join("main.crn"),
            r#"let guarded = mock.test.short_budget {
  create_mode = "normal"
}
"#,
        )
        .expect("write resource configuration");
        fs::write(
            project.join("backend.crn"),
            r#"backend local {
  path = "carina.state.json"
}
"#,
        )
        .expect("write backend configuration");
        Self {
            provider_source,
            state: project.join("carina.state.json"),
            project,
            _temp: temp,
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

    fn init(&self) -> Output {
        self.carina(&["init", "."])
    }

    fn plan(&self) -> Output {
        self.carina(&["plan", "--out", "saved-plan.json", "."])
    }

    fn apply_plan(&self) -> Output {
        self.carina(&["apply", "saved-plan.json", "--auto-approve", "--lock=false"])
    }

    fn change_provider_bytes_without_changing_exports(&self) {
        let mut bytes = fs::read(&self.provider_source).expect("read provider fixture");
        append_custom_section(
            &mut bytes,
            "carina.saved-plan-artifact-test",
            b"changed after planning",
        );
        fs::write(&self.provider_source, bytes).expect("rewrite provider fixture");
    }
}

fn wasm_mock_path() -> Option<PathBuf> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()?
        .to_path_buf();
    ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
}

fn push_leb128_u32(bytes: &mut Vec<u8>, mut value: u32) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        bytes.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn append_custom_section(component: &mut Vec<u8>, name: &str, payload: &[u8]) {
    let mut contents = Vec::new();
    push_leb128_u32(
        &mut contents,
        u32::try_from(name.len()).expect("custom section name length fits u32"),
    );
    contents.extend_from_slice(name.as_bytes());
    contents.extend_from_slice(payload);

    component.push(0x00);
    push_leb128_u32(
        component,
        u32::try_from(contents.len()).expect("custom section length fits u32"),
    );
    component.extend_from_slice(&contents);
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
fn saved_plan_rejects_changed_provider_artifact_before_effects() {
    let Some(wasm_path) = wasm_mock_path() else {
        eprintln!(
            "skipping saved-plan provider-artifact test: build carina-provider-mock for wasm32-wasip2 first"
        );
        return;
    };
    let scenario = Scenario::new(&wasm_path);
    assert_success("initial carina init", &scenario.init());
    assert_success("saved plan", &scenario.plan());
    let saved_plan: serde_json::Value = serde_json::from_slice(
        &fs::read(scenario.project.join("saved-plan.json")).expect("read saved plan"),
    )
    .expect("decode saved plan");
    assert_eq!(saved_plan["provider_artifacts"]["mock"]["kind"], "wasm");
    assert_eq!(
        saved_plan["provider_artifacts"]["mock"]["origin"]["kind"],
        "file"
    );

    scenario.change_provider_bytes_without_changing_exports();
    assert_success("carina init after provider change", &scenario.init());
    let apply = scenario.apply_plan();
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr)
    );

    assert!(
        !apply.status.success(),
        "saved-plan apply unexpectedly accepted changed provider bytes:\n{rendered}"
    );
    assert!(
        rendered.contains("Provider artifact mismatch")
            && rendered.contains("Re-run `carina plan --out saved-plan.json ")
            && rendered.contains("to produce a new saved plan before applying."),
        "saved-plan apply did not explain the artifact mismatch:\n{rendered}"
    );
    assert!(
        rendered.contains("loaded wasm sha256"),
        "mismatch must show the identity that was actually loaded:\n{rendered}"
    );
    assert!(
        !rendered.contains("not loaded"),
        "changed provider must load successfully before identity comparison:\n{rendered}"
    );
    assert!(
        !scenario.state.exists(),
        "artifact mismatch reached state writeback"
    );
}

#[test]
fn saved_plan_accepts_unchanged_provider_artifact() {
    let Some(wasm_path) = wasm_mock_path() else {
        eprintln!(
            "skipping saved-plan provider-artifact test: build carina-provider-mock for wasm32-wasip2 first"
        );
        return;
    };
    let scenario = Scenario::new(&wasm_path);
    assert_success("carina init", &scenario.init());
    assert_success("saved plan", &scenario.plan());

    assert_success("unchanged saved-plan apply", &scenario.apply_plan());
    assert!(
        scenario.state.exists(),
        "successful apply did not write state"
    );
}
