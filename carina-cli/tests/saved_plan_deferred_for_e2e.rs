//! Saved-plan coverage for bound deferred `for` templates (carina#4155).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_core::parser::DeferredForTarget;
use carina_core::resource::ResourceIdentity;
use serde_json::Value;
use tempfile::TempDir;

struct Scenario {
    _temp: TempDir,
    project: PathBuf,
    mock_state: PathBuf,
    operation_log: PathBuf,
    expected_sources: usize,
}

impl Scenario {
    fn root() -> Self {
        // The built-in mock omits unconfigured tags. The fallback stays
        // deferred while planning and resolves to zero loop instances at apply.
        Self::new(
            r#"backend local { path = "carina.state.json" }
provider mock {}
let src = mock.test.resource { name = "s" }
let rs = for _, t in (src.tags ?? {}) {
  mock.test.resource { name = "off-${t}" }
}
"#,
            None,
            1,
        )
    }

    fn module() -> Self {
        Self::new(
            r#"backend local { path = "carina.state.json" }
provider mock {}
let m = use { source = "./mod" }
m { flag = false }
"#,
            Some(
                r#"arguments { flag: Bool }
let src = mock.test.resource { name = "s" }
let rs = for _, t in (src.tags ?? {}) {
  mock.test.resource { name = "off-${t}" }
}
"#,
            ),
            1,
        )
    }

    fn nested_module_call_loop() -> Self {
        Self::new_with_modules(
            r#"backend local { path = "carina.state.json" }
provider mock {}
let m = use { source = "./mod" }
let ms = for k in ["p", "q"] {
  m { prefix = k }
}
"#,
            &[
                (
                    "mod/main.crn",
                    r#"arguments { prefix: String }
let inner = use { source = "../inner" }
inner { prefix = prefix }
"#,
                ),
                (
                    "inner/main.crn",
                    r#"arguments { prefix: String }
let src = mock.test.resource { name = prefix }
let rs = for _, t in (src.tags ?? {}) {
  mock.test.resource { name = "${prefix}-${t}" }
}
"#,
                ),
            ],
            2,
        )
    }

    fn new(root_config: &str, module_config: Option<&str>, expected_sources: usize) -> Self {
        let modules = module_config
            .map(|contents| vec![("mod/main.crn", contents)])
            .unwrap_or_default();
        Self::new_with_modules(root_config, &modules, expected_sources)
    }

    fn new_with_modules(
        root_config: &str,
        modules: &[(&str, &str)],
        expected_sources: usize,
    ) -> Self {
        let temp = tempfile::tempdir().expect("create saved-plan deferred-for fixture");
        let project = temp.path().to_path_buf();
        fs::write(project.join("main.crn"), root_config).expect("write root configuration");
        for (relative_path, contents) in modules {
            let path = project.join(relative_path);
            fs::create_dir_all(path.parent().expect("module file has a parent"))
                .expect("create module directory");
            fs::write(path, contents).expect("write module configuration");
        }

        let scenario = Self {
            mock_state: project.join("mock-provider-state.json"),
            operation_log: project.join("mock-provider-operations.log"),
            expected_sources,
            project,
            _temp: temp,
        };
        assert_success("carina init", &scenario.carina(&["init", "."]));
        scenario
    }

    fn carina(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(&self.project)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state)
            .env("CARINA_MOCK_OP_LOG", &self.operation_log)
            .args(args)
            .output()
            .expect("run carina")
    }

    fn assert_saved_plan_apply_converges(&self) {
        let plan = self.carina(&["plan", ".", "--out", "p.plan"]);
        assert_success("carina plan --out", &plan);
        let saved_plan = read_json(&self.project.join("p.plan"));
        let deferred = saved_plan["plan"]["effects"]
            .as_array()
            .expect("saved plan effects")
            .iter()
            .filter_map(|effect| effect.get("DeferredCreate"))
            .collect::<Vec<_>>();
        assert_eq!(
            deferred.len(),
            self.expected_sources,
            "every module instance must retain its deferred loop: {deferred:#?}",
        );
        for deferred in deferred {
            let target: DeferredForTarget =
                serde_json::from_value(deferred["template"]["target"].clone())
                    .expect("deserialize saved deferred target");
            let template_identity: ResourceIdentity = serde_json::from_value(
                deferred["template"]["template_resource"]["id"]["identity"].clone(),
            )
            .expect("deserialize saved template identity");
            let DeferredForTarget::Bound(target) = target else {
                panic!("saved deferred target must be bound")
            };
            assert_eq!(
                target.template_address().to_string(),
                template_identity.as_str(),
                "saved template identity must be derived from its bound target",
            );
        }

        let apply = self.carina(&["apply", "p.plan", "--auto-approve", "--lock=false"]);
        assert_success("carina apply saved plan", &apply);

        let provider_state = read_json(&self.mock_state);
        let states = provider_state["states"]
            .as_object()
            .expect("mock provider states");
        assert_eq!(
            states.len(),
            self.expected_sources,
            "the mock omits tags for an untagged test.resource, so the coalesced deferred loop must create zero children: {states:#?}",
        );
        assert!(
            states.keys().all(|address| address.ends_with(".src")),
            "every created resource must be a deferred-loop source: {states:#?}"
        );

        let operations =
            fs::read_to_string(&self.operation_log).expect("read mock provider operation log");
        let creates = operations
            .lines()
            .filter(|line| line.starts_with("create "))
            .collect::<Vec<_>>();
        assert_eq!(
            creates.len(),
            self.expected_sources,
            "the source must be created before the empty deferred loop completes: {operations}",
        );
        assert!(
            creates.iter().all(|create| create.ends_with(".src")),
            "every create must be a source resource: {operations}"
        );

        let replan = self.carina(&["plan", "."]);
        assert_success("carina re-plan", &replan);
        let output = output_text(&replan);
        assert!(
            output.contains("No changes. Infrastructure is up-to-date."),
            "re-plan after saved-plan apply must converge:\n{output}",
        );
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
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
fn saved_plan_apply_accepts_root_bound_deferred_for_template() {
    Scenario::root().assert_saved_plan_apply_converges();
}

#[test]
fn saved_plan_apply_accepts_module_bound_deferred_for_template() {
    Scenario::module().assert_saved_plan_apply_converges();
}

#[test]
fn saved_plan_apply_accepts_nested_module_call_loop_deferred_for_templates() {
    Scenario::nested_module_call_loop().assert_saved_plan_apply_converges();
}
