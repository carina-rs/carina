use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_cli::commands::plan::{CurrentStateEntry, PlanFile};
use carina_core::effect::{DeferredReplaceDelete, Effect, NonEmptyDeletes};
use carina_core::parser::{BackendConfig, DeferredForExpression, ForBinding, ProviderConfig};
use carina_core::plan::Plan;
use carina_core::resource::{ConcreteValue, Directives, Resource, ResourceId, State, Value};
use carina_state::{ResourceState, StateFile};
use indexmap::IndexMap;
use tempfile::TempDir;

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
    state_path: PathBuf,
    mock_state_path: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().to_path_buf();
        Self {
            state_path: project.join("carina.state.json"),
            mock_state_path: project.join("mock-provider-state.json"),
            project,
            _tmp: tmp,
        }
    }

    fn write_config(&self) {
        fs::write(
            self.project.join("main.crn"),
            r#"backend local { path = "carina.state.json" }
provider mock {}
"#,
        )
        .unwrap();
    }

    fn init(&self) {
        let output = carina(&self.project)
            .args(["init", "."])
            .output()
            .expect("failed to execute carina init");
        assert_success("carina init", &output);
    }

    fn seed_state(&self) -> StateFile {
        let mut state = StateFile::new();
        state.serial = 1;
        state.resources.push(
            ResourceState::new("test.resource", "validation_records[0]", "mock")
                .with_identifier("old-validation-id")
                .with_attribute("name", serde_json::json!("old-validation-token")),
        );
        fs::write(
            &self.state_path,
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
        state
    }

    fn seed_mock_provider_state(&self) {
        let provider_state = serde_json::json!({
            "test.resource.validation_records[0]": {
                "name": "old-validation-token"
            }
        });
        fs::write(
            &self.mock_state_path,
            carina_core::utils::pretty_with_newline(&provider_state).unwrap(),
        )
        .unwrap();
    }

    fn write_plan(&self, state: &StateFile) -> PathBuf {
        let plan_path = self.project.join("plan.json");
        let plan_file = deferred_replace_plan_file(&self.project, state);
        fs::write(
            &plan_path,
            carina_core::utils::pretty_with_newline(&plan_file).unwrap(),
        )
        .unwrap();
        plan_path
    }

    fn apply_plan(&self, plan_path: &Path) -> Output {
        carina(&self.project)
            .arg("apply")
            .arg(plan_path)
            .args(["--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state_path)
            .output()
            .expect("failed to execute carina apply plan")
    }
}

fn carina(project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
    command
        .current_dir(project)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE");
    command
}

fn assert_success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn mock_resource(name: &str, binding: &str) -> Resource {
    Resource::with_provider("mock", "test.resource", name, None).with_binding(binding)
}

fn state_not_found(resource: &Resource) -> CurrentStateEntry {
    CurrentStateEntry {
        id: resource.id.clone(),
        state: State::not_found(resource.id.clone()),
    }
}

fn deferred_replace_plan_file(project: &Path, state: &StateFile) -> PlanFile {
    let mut cert = mock_resource("cert", "cert")
        .with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("new-cert".to_string())),
        )
        .with_attribute(
            "domain_validation_options",
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("new-validation-token".to_string()),
            )])),
        );
    cert.dependency_bindings.clear();

    let lb = mock_resource("load_balancer", "lb").with_attribute(
        "name",
        Value::Concrete(ConcreteValue::String("lb".to_string())),
    );
    let alias = mock_resource("alias_record", "alias").with_attribute(
        "name",
        Value::Concrete(ConcreteValue::String("alias".to_string())),
    );

    let template_resource = mock_resource("validation_records", "validation_records");
    let template = DeferredForExpression {
        file: Some("main.crn".to_string()),
        line: 1,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "mock.test.resource".to_string(),
        attributes: vec![],
        binding_name: "validation_records".to_string(),
        iterable_binding: "cert".to_string(),
        iterable_attr: "domain_validation_options".to_string(),
        binding: ForBinding::Simple("opt".to_string()),
        template_resource,
    };

    let validation_id =
        ResourceId::with_provider("mock", "test.resource", "validation_records[0]", None);
    let mut plan = Plan::new();
    plan.add(Effect::Create(lb.clone()));
    plan.add(Effect::Create(cert.clone()));
    plan.add(Effect::deferred_replace(
        NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: validation_id.clone(),
            identifier: "old-validation-id".to_string(),
            directives: Directives::default(),
            binding: Some("validation_records[0]".to_string()),
            dependencies: HashSet::from(["cert".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }])
        .expect("fixture has one delete"),
        ResourceId::new("__deferred_for", "validation_records"),
        "cert".to_string(),
        Box::new(template),
    ));
    plan.add(Effect::Create(alias.clone()));

    let sorted_resources = vec![lb.clone(), cert.clone(), alias.clone()];
    let current_states = sorted_resources
        .iter()
        .map(state_not_found)
        .collect::<Vec<_>>();

    PlanFile {
        version: PlanFile::CURRENT_VERSION,
        carina_version: env!("CARGO_PKG_VERSION").to_string(),
        timestamp: "2026-06-25T00:00:00Z".to_string(),
        source_path: project.display().to_string(),
        state_lineage: Some(state.lineage.clone()),
        state_serial: Some(state.serial),
        provider_configs: vec![ProviderConfig {
            name: "mock".to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: None,
            is_default: true,
        }],
        backend_config: Some(BackendConfig {
            backend_type: "local".to_string(),
            attributes: HashMap::from([(
                "path".to_string(),
                Value::Concrete(ConcreteValue::String("carina.state.json".to_string())),
            )]),
        }),
        plan,
        sorted_resources: sorted_resources.clone(),
        unresolved_resources: sorted_resources,
        compositions: vec![],
        data_sources: vec![],
        current_states,
        upstream_snapshot: HashMap::new(),
        upstream_sources: vec![],
        wait_bindings: vec![],
    }
}

#[test]
fn apply_saved_plan_deferred_replace_persists_new_child_and_siblings() {
    let scenario = Scenario::new();
    scenario.write_config();
    scenario.init();
    let state = scenario.seed_state();
    scenario.seed_mock_provider_state();
    let plan_path = scenario.write_plan(&state);

    let output = scenario.apply_plan(&plan_path);
    assert_success("carina apply plan", &output);

    let saved: StateFile =
        serde_json::from_str(&fs::read_to_string(&scenario.state_path).unwrap()).unwrap();
    let validation = saved
        .find_resource("mock", "test.resource", "validation_records[0]")
        .expect("new validation record state must be persisted");
    assert_eq!(validation.identifier.as_deref(), Some("mock-id"));
    for name in ["load_balancer", "cert", "alias_record"] {
        let row = saved
            .find_resource("mock", "test.resource", name)
            .unwrap_or_else(|| panic!("sibling resource {name} must be persisted"));
        assert_eq!(row.identifier.as_deref(), Some("mock-id"));
    }
}
