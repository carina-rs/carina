use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MARKER: &str = "declared-secret-cobalt-784531";

enum ArtifactPresence {
    Required,
    Absent,
}
struct Project(tempfile::TempDir);
impl Project {
    fn new(main: &str, child: Option<&str>) -> Self {
        let root = tempfile::Builder::new()
            .prefix("declared checks ' ")
            .tempdir()
            .unwrap();
        std::fs::write(root.path().join("providers.crn"), "provider mock {}\n").unwrap();
        std::fs::write(
            root.path().join("backend.crn"),
            "backend local { path = 'carina.state.json' }\n",
        )
        .unwrap();
        std::fs::write(root.path().join("main.crn"), "").unwrap();
        let project = Self(root);
        project.success(&["init"]);
        std::fs::write(project.path().join("main.crn"), main).unwrap();
        if let Some(child) = child {
            std::fs::create_dir(project.path().join("child")).unwrap();
            std::fs::write(project.path().join("child/main.crn"), child).unwrap();
        }
        project
    }
    fn path(&self) -> &Path {
        self.0.path()
    }
    fn run(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_carina"))
            .args(args)
            .arg(self.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env(
                "CARINA_MOCK_STATE_FILE",
                self.path().join("mock-provider-state.json"),
            )
            .output()
            .unwrap();
        for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
            assert!(
                !String::from_utf8_lossy(bytes).contains(MARKER),
                "secret in command output: args={args:?}, stream={stream}"
            );
        }
        output
    }
    fn success(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
    fn assert_private_artifact(&self, path: &Path, presence: ArtifactPresence) {
        match presence {
            ArtifactPresence::Required => {
                assert!(path.is_file(), "missing required artifact");
                assert!(
                    !std::fs::read_to_string(path).unwrap().contains(MARKER),
                    "secret in artifact"
                );
            }
            ArtifactPresence::Absent => assert!(!path.exists(), "unexpected saved artifact"),
        }
    }
}
fn hashes(project: &Project) -> Vec<serde_json::Value> {
    let path = project.path().join("carina.state.json");
    project.assert_private_artifact(&path, ArtifactPresence::Required);
    let state: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let mut values = Vec::new();
    for resource in state["resources"].as_array().unwrap() {
        let value = resource["current"]["attributes"]["computed"].clone();
        assert!(value["$secret"].is_string());
        values.push(value);
    }
    assert_eq!(values.len(), 2);
    values
}

#[test]
fn inlined_function_secret_output_stays_private_and_hashed() {
    // The handle annotation checks shape; the String return annotation is primitive.
    // This function inlines, so the test is a privacy control.
    for module in [false, true] {
        let body = format!(
            "let src = mock.test.resource {{ name = 'src' computed = secret('{MARKER}') }}\nfn g(r: mock.test.resource): String {{ \"prefix-${{r.computed}}\" }}\nmock.test.resource {{ name = 'sink' computed = g(src) }}\n"
        );
        let project = if module {
            Project::new(
                "let child = use { source = './child' }\nlet instance = child { label = 'module' }\n",
                Some(&format!("arguments {{ label: String }}\n{body}")),
            )
        } else {
            Project::new(&body, None)
        };
        project.success(&["apply", "--auto-approve"]);
        let before = hashes(&project);
        project.success(&["plan"]);
        let saved: PathBuf = project.path().join("saved.plan.json");
        project.success(&["plan", "--out", saved.to_str().unwrap()]);
        project.assert_private_artifact(&saved, ArtifactPresence::Required);
        let output = project.success(&["state", "refresh"]);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("State refreshed: 0 resources updated, 2 resources unchanged."));
        assert!(!stdout.contains("computed:"));
        assert_eq!(hashes(&project), before);
    }
}

fn typed_project(body: &str, module: bool) -> Project {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
        });
    let wasm = ["carina-provider-mock.wasm", "carina_provider_mock.wasm"]
        .into_iter()
        .map(|name| target.join("wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
        .expect("the mock WASM fixture must be built");
    let project = Project::new("", None);
    for destination in [
        project.path().join("local.wasm"),
        project
            .path()
            .join(".carina/providers/file/local/local.wasm"),
    ] {
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::copy(&wasm, destination).unwrap();
    }
    std::fs::write(
        project.path().join("providers.crn"),
        "provider mock { source = 'file://local.wasm' }\n",
    )
    .unwrap();
    if module {
        std::fs::write(
            project.path().join("main.crn"),
            "let child = use { source = './child' }\nlet instance = child { label = 'module' }\n",
        )
        .unwrap();
        std::fs::create_dir(project.path().join("child")).unwrap();
        std::fs::write(
            project.path().join("child/main.crn"),
            format!("arguments {{ label: String }}\n{body}"),
        )
        .unwrap();
    } else {
        std::fs::write(project.path().join("main.crn"), body).unwrap();
    }
    project
}

fn assert_binary_refusal(project: &Project, json: bool, problem: &str) {
    let saved = project.path().join("saved.plan.json");
    let args = if json {
        vec!["plan", "--refresh=false", "--json"]
    } else {
        vec!["plan", "--refresh=false", "--out", saved.to_str().unwrap()]
    };
    {
        let expected = format!(
            "Warning: using cached managed-resource state (--refresh=false). Data sources are still read, but the plan may not reflect actual managed infrastructure.\n{}Error: Cannot save plan: {}\nError: Run `{}` directly on the project, without a saved plan.\n",
            if json {
                "Using mock (instance=default, source: file://local.wasm)\n"
            } else {
                ""
            },
            problem,
            carina_core::hint::ProjectCommand::new("apply", project.path()),
        );
        let output = project.run(&args);
        assert_eq!(output.status.code(), Some(1));
        if json {
            assert!(
                output.stdout.is_empty(),
                "refused JSON command must not print a document"
            );
        }
        assert_eq!(String::from_utf8(output.stderr).unwrap(), expected);
        project.assert_private_artifact(&saved, ArtifactPresence::Absent);
    }
}

fn assert_binary_retained_refusal(json: bool) {
    for module in [false, true] {
        let project = typed_project(
            "let src = mock.test.read_back { present = 'allowed' }\nfn checked(r: mock.test.read_back): mock.RecoveryProbe { r.computed }\nlet sink = mock.test.read_back { present = checked(src) }\n",
            module,
        );
        assert_binary_refusal(
            &project,
            json,
            "the plan contains an unevaluated call to the user-defined function 'checked'. A saved plan cannot carry this call.",
        );
    }
}

fn assert_distinct_function_names(json: bool) {
    for module in [false, true] {
        let project = typed_project(
            "let src = mock.test.read_back { present = 'allowed' }\nfn zeta(r: mock.test.read_back): mock.RecoveryProbe { r.computed }\nfn alpha(r: mock.test.read_back): mock.RecoveryProbe { r.computed }\nlet first = mock.test.read_back { present = zeta(src) }\nlet second = mock.test.read_back { present = alpha(src) }\nlet third = mock.test.read_back { present = zeta(src) }\n",
            module,
        );
        assert_binary_refusal(
            &project,
            json,
            "the plan contains unevaluated calls to the user-defined functions 'alpha', 'zeta'. A saved plan cannot carry these calls.",
        );
    }
}
#[test]
fn retained_calls_report_distinct_function_names_in_order_plan_out() {
    assert_distinct_function_names(false);
}
#[test]
fn retained_calls_report_distinct_function_names_in_order_json() {
    assert_distinct_function_names(true);
}

#[test]
fn retained_calls_refuse_saved_output_through_the_binary_json() {
    assert_binary_retained_refusal(true);
}
#[test]
fn retained_calls_refuse_saved_output_through_the_binary_plan_out() {
    assert_binary_retained_refusal(false);
}

#[test]
#[should_panic(expected = "missing required artifact")]
fn private_artifact_requires_the_expected_file() {
    let project = Project::new("", None);
    project.assert_private_artifact(
        &project.path().join("missing.plan.json"),
        ArtifactPresence::Required,
    );
}

#[test]
fn stale_state_value_checks_match_at_root_and_in_module() {
    for module in [false, true] {
        let source = "let src = mock.test.read_back { present = 'before' create_read_back_pending_polls = 1 }\n";
        let project = typed_project(source, module);
        project.success(&["apply", "--auto-approve"]);
        let body = "let src = mock.test.read_back { present = 'after' create_read_back_pending_polls = 1 }\nfn checked(x: mock.test.sdk_validator.Guarded | 'ready'): String { x }\nlet sink = mock.test.read_back { present = checked(src.computed) }\n";
        std::fs::write(
            project
                .path()
                .join(if module { "child/main.crn" } else { "main.crn" }),
            if module {
                format!("arguments {{ label: String }}\n{body}")
            } else {
                body.into()
            },
        )
        .unwrap();
        project.success(&["plan", "--refresh=false"]);
        let path = project.path().join("carina.state.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let rows = state["resources"].as_array_mut().unwrap();
        assert_eq!(rows.len(), 1);
        rows[0]["current"]["attributes"]["computed"] = "bad-name".into();
        let before = serde_json::to_vec(&state).unwrap();
        std::fs::write(&path, &before).unwrap();
        let output = project.run(&["plan", "--refresh=false"]);
        assert_eq!(output.status.code(), Some(1));
        let line = if module { 4 } else { 3 };
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!(
                "Warning: using cached managed-resource state (--refresh=false). Data sources are still read, but the plan may not reflect actual managed infrastructure.\nError: Invalid expression at line {line}: function 'checked': parameter 'x': expected mock.test.sdk_validator.Guarded | 'ready', got string 'bad-name'\n"
            )
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        project.success(&["state", "refresh"]);
        project.success(&["plan", "--refresh=false"]);
    }
}
