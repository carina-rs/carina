//! CLI-level regression coverage for DeferredReplace consumer update ordering.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_cli::commands::plan::{CurrentStateEntry, PlanFile};
use carina_core::effect::{DeferredReplaceDelete, DeferredReplacePayload, Effect, NonEmptyDeletes};
use carina_core::parser::{BackendConfig, DeferredForExpression, ForBinding, ProviderConfig};
use carina_core::plan::Plan;
use carina_core::resource::{
    ConcreteValue, Directives, ResolvedResource, Resource, ResourceId, ResourceIdentity, State,
    Value,
};
use carina_state::{ResourceState, StateFile};
use indexmap::IndexMap;
use tempfile::TempDir;

const UPDATE_DELAY_MS: &str = "200";

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

struct Scenario {
    _tmp: TempDir,
    project: PathBuf,
    state_path: PathBuf,
    mock_state_path: PathBuf,
    op_log_path: PathBuf,
}

impl Scenario {
    fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().to_path_buf();
        Self {
            state_path: project.join("carina.state.json"),
            mock_state_path: project.join("mock-provider-state.json"),
            op_log_path: project.join("op.log"),
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

        let mut validation = ResourceState::new("test.resource", "validation_records[0]", "mock")
            .with_identifier("old-validation-id")
            .with_attribute("name", serde_json::json!("old-validation-token"))
            .with_attribute("identifier", serde_json::json!("old-validation-id"));
        validation.binding = Some("validation_records[0]".to_string());
        validation.dependency_bindings.insert("cert".to_string());
        state.upsert_resource(validation);

        let mut consumer = ResourceState::new("test.resource", "consumer", "mock")
            .with_identifier("mock-id")
            .with_attribute("name", serde_json::json!("consumer"))
            .with_attribute("identifier", serde_json::json!("consumer-id"))
            .with_attribute("web_acl_arn", serde_json::json!("old-validation-id"));
        consumer.binding = Some("consumer".to_string());
        consumer
            .dependency_bindings
            .insert("validation_records[0]".to_string());
        state.upsert_resource(consumer);

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
                "name": "old-validation-token",
                "identifier": "old-validation-id"
            },
            "test.resource.consumer": {
                "name": "consumer",
                "identifier": "consumer-id",
                "web_acl_arn": "old-validation-id"
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
        let plan_file = deferred_replace_consumer_ordering_plan_file(&self.project, state);
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
            .args(["--parallelism", "8", "--auto-approve", "--lock=false"])
            .env("CARINA_MOCK_ENABLE_TEST_RESOURCE_SCHEMA", "1")
            .env("CARINA_MOCK_STATE_FILE", &self.mock_state_path)
            .env("CARINA_MOCK_OP_LOG", &self.op_log_path)
            .env("CARINA_MOCK_UPDATE_DELAY_MS", UPDATE_DELAY_MS)
            .env_remove("CARINA_MOCK_CREATE_FAIL_FOR")
            .output()
            .expect("failed to execute carina apply plan")
    }

    fn op_log(&self) -> Vec<String> {
        fs::read_to_string(&self.op_log_path)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn mock_resource(name: &str, binding: &str) -> Resource {
    Resource::with_provider("mock", "test.resource", name, None).with_binding(binding)
}

fn string(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.to_string()))
}

fn identity(value: &str) -> ResourceIdentity {
    ResourceId::with_identity("test.resource", value)
        .identity
        .expect("fixture identity is non-empty")
}

fn state_not_found(resource: &Resource) -> CurrentStateEntry {
    CurrentStateEntry {
        id: resource.id.clone(),
        state: State::not_found(resource.id.clone()),
    }
}

fn consumer_current_state(id: &ResourceId) -> State {
    State::existing(
        id.clone(),
        HashMap::from([
            ("name".to_string(), string("consumer")),
            ("identifier".to_string(), string("consumer-id")),
            ("web_acl_arn".to_string(), string("old-validation-id")),
        ]),
    )
    .with_identifier("mock-id")
}

fn deferred_replace_consumer_ordering_plan_file(project: &Path, state: &StateFile) -> PlanFile {
    let cert = mock_resource("cert", "cert")
        .with_attribute("name", string("cert"))
        .with_attribute(
            "domain_validation_options",
            Value::Concrete(ConcreteValue::List(vec![string("new-validation-token")])),
        );

    let mut consumer = mock_resource("consumer", "consumer")
        .with_attribute("name", string("consumer"))
        .with_attribute("identifier", string("consumer-id"))
        .with_attribute(
            "web_acl_arn",
            Value::resource_ref("validation_records[0]", "identifier", vec![]),
        );
    consumer
        .dependency_bindings
        .insert("validation_records[0]".to_string());
    let consumer_from = consumer_current_state(&consumer.id);

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
        ResourceId::with_provider_identity("mock", "test.resource", "validation_records[0]", None);

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(cert.clone())));
    plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: carina_core::resource::ResolvedResourceId::new(validation_id.clone()),
            identifier: "old-validation-id".to_string(),
            directives: Directives::default(),
            binding: Some("validation_records[0]".to_string()),
            dependencies: HashSet::from(["cert".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::from([identity("consumer")]),
        }])
        .expect("fixture has one delete"),
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        upstream_binding: "cert".to_string(),
        template: Box::new(template),
    })));
    plan.add(Effect::Update {
        from: Box::new(consumer_from.clone()),
        to: resolved(consumer.clone()),
        changed_attributes: vec!["web_acl_arn".to_string()],
    });

    let sorted_resources = vec![cert.clone(), consumer.clone()];
    let current_states = vec![
        state_not_found(&cert),
        CurrentStateEntry {
            id: consumer.id.clone(),
            state: consumer_from,
        },
    ];

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
fn deferred_replace_consumer_ordering_updates_consumer_between_create_and_delete() {
    let scenario = Scenario::new();
    scenario.write_config();
    scenario.init();
    let state = scenario.seed_state();
    scenario.seed_mock_provider_state();
    let plan_path = scenario.write_plan(&state);

    let output = scenario.apply_plan(&plan_path);
    assert_success("carina apply plan", &output);

    let op_log = scenario.op_log();
    let create_validation_pos = op_position(
        &op_log,
        "create test.resource.validation_records[0]",
        "new validation record create",
    );
    let update_consumer_pos =
        op_position(&op_log, "update test.resource.consumer", "consumer update");
    let delete_validation_pos = op_position(
        &op_log,
        "delete test.resource.validation_records[0]",
        "old validation record delete",
    );

    assert!(
        create_validation_pos < update_consumer_pos && update_consumer_pos < delete_validation_pos,
        "Issue #3627 regression: DeferredReplace must order the consumer Update between \
         the materialized producer Create and the old producer Delete; expected create \
         validation_records[0] -> update consumer -> delete validation_records[0], \
         op log: {op_log:?}"
    );
}

fn op_position(op_log: &[String], expected: &str, label: &str) -> usize {
    op_log
        .iter()
        .position(|entry| entry == expected)
        .unwrap_or_else(|| {
            panic!(
                "Issue #3627 ordering test must observe {label} (`{expected}`); op log: {op_log:?}"
            )
        })
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
