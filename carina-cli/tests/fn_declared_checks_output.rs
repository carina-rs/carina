use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MARKER: &str = "declared-secret-cobalt-784531";

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
    fn assert_private_artifact(&self, path: &Path) {
        if path.exists() {
            assert!(
                !std::fs::read_to_string(path).unwrap().contains(MARKER),
                "secret in artifact"
            );
        }
    }
}
fn hashes(project: &Project) -> Vec<serde_json::Value> {
    let path = project.path().join("carina.state.json");
    project.assert_private_artifact(&path);
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
fn refresh_of_function_authored_secret_state_prints_no_values() {
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
        project.assert_private_artifact(&saved);
        let output = project.success(&["state", "refresh"]);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("State refreshed: 0 resources updated, 2 resources unchanged."));
        assert!(!stdout.contains("computed:"));
        assert_eq!(hashes(&project), before);
    }
}
