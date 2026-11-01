//! The command paths must classify the same configuration graph and state rows.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Project {
    temp: TempDir,
    root: PathBuf,
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir(&root).unwrap();
        let project = Self { temp, root };
        project.write("providers.crn", "provider mock {}\n");
        project.write(
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        );
        project
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.root)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", self.temp.path().join("mock.json"))
            .env(
                "CARINA_MOCK_OP_LOG",
                self.temp.path().join("operations.log"),
            )
            .stdin(Stdio::null())
            .args(args)
            .output()
            .unwrap()
    }

    fn state_path(&self) -> PathBuf {
        self.root.join("carina.state.json")
    }

    fn state(&self) -> Value {
        read_json(&self.state_path())
    }

    fn seed(&self, rows: Vec<Value>, provider: Value) {
        fs::write(
            self.state_path(),
            serde_json::to_vec_pretty(&json!({
                "version": 20,
                "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
                "serial": 7,
                "lineage": "legacy-command-regression",
                "carina_version": "legacy",
                "resources": rows,
                "exports": {},
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            self.temp.path().join("mock.json"),
            serde_json::to_vec(&provider).unwrap(),
        )
        .unwrap();
        success(&self.run(&["init", "."]));
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn rendered(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn success(output: &Output) {
    assert!(output.status.success(), "{}", rendered(output));
}

fn row(resource_type: &str, binding: &str, attributes: Value, dependencies: &[&str]) -> Value {
    json!({
        "resource_type": resource_type,
        "provider": "mock",
        "identity": binding,
        "binding": binding,
        "current": {
            "kind": "applied",
            "identifier": format!("{binding}-provider-id"),
            "attributes": attributes,
            "nested_write_only": {},
        },
        "dependency_bindings": dependencies,
    })
}

fn retyped_binding(version: u32, command: &[&str]) {
    let project = Project::new();
    project.write(
        "main.crn",
        "let vpc = mock.test.resource { name = 'original' }\nlet subnet = mock.test.resource { name = 'subnet' parent = vpc.name }\n",
    );
    success(&project.run(&["init", "."]));
    success(&project.run(&["apply", "--auto-approve", "."]));
    if version == 20 {
        let mut state = project.state();
        state["version"] = json!(20);
        for row in state["resources"].as_array_mut().unwrap() {
            row["dependency_bindings"] = if row["binding"] == "subnet" {
                json!(["vpc"])
            } else {
                json!([])
            };
            row["directives"]["depends_on"] = json!([]);
        }
        fs::write(
            project.state_path(),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();
    }
    project.write(
        "main.crn",
        "let vpc = mock.test.Thing { name = 'replacement' }\nlet subnet = mock.test.resource { name = 'subnet' parent = vpc.name }\n",
    );
    success(&project.run(&["plan", "."]));
    success(&project.run(command));
    assert_eq!(
        project.state()["version"],
        carina_state::StateFile::CURRENT_VERSION
    );
}

#[test]
fn destroy_retyped_binding_with_v21_orphan() {
    retyped_binding(21, &["destroy", "--auto-approve", "."]);
}

#[test]
fn refresh_retyped_binding_with_v21_orphan() {
    retyped_binding(21, &["state", "refresh", "."]);
}

#[test]
fn destroy_retyped_binding_with_v20_orphan() {
    retyped_binding(20, &["destroy", "--auto-approve", "."]);
}

#[test]
fn refresh_retyped_binding_with_v20_orphan() {
    retyped_binding(20, &["state", "refresh", "."]);
}

fn removed_module(root_vpc: bool) {
    let project = Project::new();
    project.write(
        "main.crn",
        if root_vpc {
            "let vpc = mock.test.resource { name = 'root-vpc' }\n"
        } else {
            "// The module and all its declarations have been removed.\n"
        },
    );
    let mut rows = vec![
        row(
            "test.resource",
            "net.vpc",
            json!({"name": "module-vpc"}),
            &[],
        ),
        row(
            "test.resource",
            "net.subnet",
            json!({"name": "subnet", "parent": "module-vpc"}),
            &["vpc"],
        ),
    ];
    rows[1]["directives"] = json!({"depends_on": ["vpc"]});
    rows[1]["deposed"] = json!([{
        "key": "old-subnet",
        "identifier": "old-subnet-id",
        "attributes": {"name": "old-subnet"},
        "dependency_bindings": ["vpc"],
    }]);
    let mut provider = json!({
        "test.resource.net.vpc": {"name": "module-vpc"},
        "test.resource.net.subnet": {"name": "subnet", "parent": "module-vpc"},
    });
    if root_vpc {
        rows.push(row(
            "test.resource",
            "vpc",
            json!({"name": "root-vpc"}),
            &[],
        ));
        provider["test.resource.vpc"] = json!({"name": "root-vpc"});
    }
    project.seed(rows, provider);
    let output = project.run(&["state", "refresh", "."]);
    success(&output);
    assert!(
        !rendered(&output).contains("partially unknown"),
        "{}",
        rendered(&output)
    );
    let state = project.state();
    let subnet = state["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binding"] == "net.subnet")
        .unwrap();
    assert_eq!(
        subnet["dependency_bindings"],
        json!([{
            "provider": "mock", "resource_type": "test.resource", "identity": "net.vpc"
        }]),
        "a removed module must retain its own vpc edge: {state:#}"
    );
    assert_eq!(
        subnet["directives"]["depends_on"],
        subnet["dependency_bindings"]
    );
    assert_eq!(
        subnet["deposed"][0]["dependency_bindings"],
        subnet["dependency_bindings"]
    );
}

#[test]
fn refresh_removed_module_prefers_its_state_scope_over_root() {
    removed_module(true);
}

#[test]
fn refresh_removed_module_retains_its_state_scope_without_root() {
    removed_module(false);
}

fn deferred_module(dependency: &str) -> Project {
    let project = Project::new();
    project.write("main.crn", "let network = use { source = './network' }\nlet net = network {}\nlet vpc = mock.test.resource { name = 'root-vpc' }\n");
    project.write("network/main.crn", "arguments { marker: String = 'net' }\nlet vpc = mock.test.resource { name = 'module-vpc' }\nlet roles = read mock.iam.Roles { name_regex = 'child' }\nlet children = for name in roles.names {\n  mock.test.resource { name = name parent = vpc.name }\n}\n");
    project.seed(
        vec![
            row("test.resource", "vpc", json!({"name": "root-vpc"}), &[]),
            row(
                "test.resource",
                "net.vpc",
                json!({"name": "module-vpc"}),
                &[],
            ),
            row(
                "test.resource",
                "net.children[0]",
                json!({"name": "child", "parent": "module-vpc"}),
                &[dependency],
            ),
        ],
        json!({
            "test.resource.vpc": {"name": "root-vpc"},
            "test.resource.net.vpc": {"name": "module-vpc"},
            "test.resource.net.children[0]": {"name": "child", "parent": "module-vpc"},
            "iam.Roles.net.roles": {"names": ["child"]},
        }),
    );
    project
}

#[test]
fn destroy_deferred_module_uses_the_same_scoped_edges_as_plan() {
    let project = deferred_module("vpc");
    success(&project.run(&["plan", "--out", "plan.json", "."]));
    let plan = read_json(&project.root.join("plan.json"));
    let output = project.run(&["destroy", "."]);
    success(&output);
    assert!(rendered(&output).contains("Destroy cancelled."));
    let state = project.state();
    let child = state["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["binding"] == "net.children[0]")
        .unwrap();
    let expected =
        json!([{"provider": "mock", "resource_type": "test.resource", "identity": "net.vpc"}]);
    assert_eq!(child["dependency_bindings"], expected, "{state:#}");
    let classification = &plan["legacy_dependency_classification"];
    assert!(
        classification["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| {
                entry[0]["identity"] == "net.children[0]"
                    && entry[1]["value_dependencies"] == expected
            }),
        "plan and destroy must preserve the same edges: {plan:#}"
    );
}

#[test]
fn destroy_deferred_module_is_configured_and_strict_after_reads() {
    let project = deferred_module("missing_dependency");
    let before = fs::read(project.state_path()).unwrap();
    for command in [["plan", "."], ["destroy", "."]] {
        let output = project.run(&command);
        assert!(
            !output.status.success(),
            "configured loop rows must fail closed: {}",
            rendered(&output)
        );
        assert!(
            rendered(&output).contains("missing_dependency"),
            "{}",
            rendered(&output)
        );
        assert_eq!(fs::read(project.state_path()).unwrap(), before);
    }
}

#[test]
fn destroy_without_refresh_requires_reads_for_deferred_module_rows() {
    let project = deferred_module("vpc");
    let before = fs::read(project.state_path()).unwrap();
    let output = project.run(&["destroy", "--refresh=false", "."]);
    assert!(
        !output.status.success(),
        "offline destroy must not guess loop ownership: {}",
        rendered(&output)
    );
    let text = rendered(&output);
    assert!(
        text.contains("net.children[0]")
            && text.contains("requires reads")
            && text.contains("rerun destroy with --refresh=true"),
        "{text}"
    );
    assert!(
        !project.temp.path().join("operations.log").exists(),
        "offline destroy must not issue provider reads"
    );
    assert_eq!(fs::read(project.state_path()).unwrap(), before);
}

fn residual_upstream_loop() -> Project {
    let project = Project::new();
    project.write(
        "source.crn",
        "let external = upstream_state { source = '../must-not-be-read' }\n",
    );
    project.write(
        "children.crn",
        "let children = for name in external.names {\n  mock.test.resource { name = name }\n}\n",
    );
    project.seed(
        vec![row(
            "test.resource",
            "children[0]",
            json!({"name": "child"}),
            &["missing_dependency"],
        )],
        json!({"test.resource.children[0]": {"name": "child"}}),
    );
    project
}

#[test]
fn destroy_refresh_cannot_claim_planned_materialization_with_residual_upstream_loops() {
    let project = residual_upstream_loop();
    let before = fs::read(project.state_path()).unwrap();
    let output = project.run(&["destroy", "--auto-approve", "."]);
    let text = rendered(&output);
    assert!(
        !output.status.success() && text.contains("children[0]") && text.contains("requires reads"),
        "refresh=true cannot prove that deferred loops materialized: {text}"
    );
    assert_eq!(fs::read(project.state_path()).unwrap(), before);
    let operations =
        fs::read_to_string(project.temp.path().join("operations.log")).unwrap_or_default();
    assert!(!operations.contains("delete"), "{operations}");
}

#[test]
fn state_refresh_cannot_claim_planned_materialization_with_residual_upstream_loops() {
    let project = residual_upstream_loop();
    let before = fs::read(project.state_path()).unwrap();
    let output = project.run(&["state", "refresh", "."]);
    let text = rendered(&output);
    assert!(
        !output.status.success() && text.contains("children[0]") && text.contains("requires reads"),
        "refresh must retain the residual-loop classification requirement: {text}"
    );
    assert_eq!(fs::read(project.state_path()).unwrap(), before);
}

#[test]
fn refresh_deferred_module_classifies_materialized_rows_strictly() {
    let project = deferred_module("missing_dependency");
    let before = fs::read(project.state_path()).unwrap();
    let output = project.run(&["state", "refresh", "."]);
    assert!(
        !output.status.success(),
        "a configured loop row must fail closed: {}",
        rendered(&output)
    );
    assert!(
        rendered(&output).contains("missing_dependency"),
        "{}",
        rendered(&output)
    );
    assert_eq!(fs::read(project.state_path()).unwrap(), before);
}
