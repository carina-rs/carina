//! CLI-level regression coverage for DeferredReplace consumer update ordering.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use carina_cli::commands::plan::{CurrentStateEntry, PlanFile, PlanFileContents};
use carina_cli::wiring::{LoadedProviderFactory, ValidatedConfig};
use carina_core::effect::{DeferredReplaceDelete, DeferredReplacePayload, Effect, NonEmptyDeletes};
use carina_core::parser::{
    BackendConfig, DeferredForExpression, ForBinding, ProviderConfig, ProviderContext,
    SavedProviderConfigs,
};
use carina_core::plan::Plan;
use carina_core::provider::AppliedIdentifier;
use carina_core::resource::{
    AbsoluteBindingAddress, ConcreteValue, Directives, LoopSlot, NoPlaceholder,
    RelativeBindingName, ResolvedResource, Resource, ResourceId, ResourceIdentity, State, Value,
};
use carina_state::{ResourceState, StateFile};
use indexmap::IndexMap;
use tempfile::TempDir;

const UPDATE_DELAY_MS: &str = "200";

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

fn absolute_binding(name: &str) -> AbsoluteBindingAddress {
    carina_core::resource::absolute_binding_address_for_test(name)
}

fn mock_saved_provider_configs() -> SavedProviderConfigs {
    let mut parsed = carina_core::parser::ExpandedInferredFile::default();
    parsed.providers = vec![ProviderConfig {
        name: "mock".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let resolved = carina_core::parser::resolve_provider_attributes_with_remote(
        &parsed,
        &HashMap::new(),
        &ProviderContext::default(),
    )
    .expect("test provider config must resolve");
    SavedProviderConfigs::from(&resolved)
}

fn record_with_mock_provider(contents: PlanFileContents) -> PlanFile {
    let configs: [ProviderConfig; 1] = [ProviderConfig {
        name: "mock".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }];
    let providers = ValidatedConfig::for_test((), Vec::<LoadedProviderFactory>::new(), &configs);
    contents
        .record_for_test(&providers, Vec::new())
        .expect("mock plan provider evidence must be valid")
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

        let mut validation = ResourceState::try_applied(
            "test.resource",
            "validation_records[0]",
            "mock",
            "old-validation-id",
        )
        .expect("fixture identifier is non-empty")
        .with_attribute("name", serde_json::json!("old-validation-token"))
        .with_attribute("identifier", serde_json::json!("old-validation-id"));
        validation.binding = Some(absolute_binding("validation_records[0]"));
        validation
            .dependency_bindings
            .insert(absolute_binding("cert"));
        state
            .upsert_resource(validation)
            .expect("test state setup must be valid");

        let mut consumer =
            ResourceState::try_applied("test.resource", "consumer", "mock", "mock-id")
                .expect("fixture identifier is non-empty")
                .with_attribute("name", serde_json::json!("consumer"))
                .with_attribute("identifier", serde_json::json!("consumer-id"))
                .with_attribute("web_acl_arn", serde_json::json!("old-validation-id"));
        consumer.binding = Some(absolute_binding("consumer"));
        consumer
            .dependency_bindings
            .insert(absolute_binding("validation_records[0]"));
        state
            .upsert_resource(consumer)
            .expect("test state setup must be valid");

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
    let mut resource = Resource::with_provider(
        "mock",
        "test.resource",
        ResourceIdentity::try_from(name.to_string()).unwrap(),
        None,
    );
    resource.binding = Some(absolute_binding(binding));
    resource
}

fn string(value: &str) -> Value {
    Value::Concrete(ConcreteValue::String(value.to_string().into()))
}

fn identity(value: &str) -> ResourceIdentity {
    ResourceIdentity::try_from(value.to_string()).unwrap()
}

fn state_not_found(resource: &Resource) -> CurrentStateEntry {
    CurrentStateEntry::managed(resource.id.clone(), State::not_found(resource.id.clone()))
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
            Value::<NoPlaceholder, RelativeBindingName>::resource_ref(
                "validation_records[0]",
                "identifier",
                vec![],
            )
            .resolve_root_for_test(),
        );
    consumer
        .dependency_bindings
        .insert(absolute_binding("validation_records[0]"));
    let consumer_from = consumer_current_state(&consumer.id);

    let template_resource: carina_core::resource::Resource<
        carina_core::resource::ModuleSource,
        LoopSlot,
        AbsoluteBindingAddress,
    > = carina_core::resource::Resource::with_provider(
        "mock",
        "test.resource",
        carina_core::resource::ResourceIdentity::try_from("validation_records".to_string())
            .unwrap(),
        None,
    )
    .with_absolute_binding(carina_core::resource::absolute_binding_address_for_test(
        "validation_records",
    ));
    let template = DeferredForExpression::new(
        Some("main.crn".to_string()),
        1,
        "for opt in cert.domain_validation_options".to_string(),
        carina_core::parser::DeferredForTarget::bound(
            carina_core::resource::absolute_binding_address_for_test("validation_records"),
        )
        .expect("fixture binding is valid"),
        Value::<NoPlaceholder, RelativeBindingName>::resource_ref(
            "cert",
            "domain_validation_options",
            Vec::new(),
        )
        .resolve_root_for_test(),
        ForBinding::Simple("opt".to_string()),
        template_resource,
    );

    let validation_id =
        ResourceId::with_provider_identity("mock", "test.resource", "validation_records[0]", None);

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(cert.clone())));
    plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: carina_core::resource::ResolvedResourceId::new(validation_id.clone()),
            identifier: AppliedIdentifier::new("old-validation-id")
                .expect("fixture identifier is non-empty"),
            directives: Directives::default(),
            binding: Some(absolute_binding("validation_records[0]")),
            dependencies: HashSet::from([absolute_binding("cert")]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::from([identity("consumer")]),
        }])
        .expect("fixture has one delete"),
        template: Box::new(template),
    })));
    plan.add(Effect::Update {
        identifier: AppliedIdentifier::new("consumer-id").expect("fixture identifier is non-empty"),
        from: Box::new(consumer_from.clone()),
        to: resolved(consumer.clone()),
        changed_attributes: vec!["web_acl_arn".to_string()],
    });

    let sorted_resources = vec![cert.clone(), consumer.clone()];
    let current_states = vec![
        state_not_found(&cert),
        CurrentStateEntry::managed(consumer.id.clone(), consumer_from),
    ];

    let contents = PlanFileContents {
        version: PlanFile::CURRENT_VERSION,
        carina_version: env!("CARGO_PKG_VERSION").to_string(),
        timestamp: "2026-06-25T00:00:00Z".to_string(),
        source_path: carina_core::paths::DisplayPath::new(project).unwrap(),
        source_identity: carina_core::paths::CanonicalPath::new(project).unwrap(),
        state_lineage: Some(state.lineage.clone()),
        state_serial: Some(state.serial),
        provider_configs: mock_saved_provider_configs(),
        backend_config: Some(BackendConfig {
            backend_type: "local".to_string(),
            attributes: HashMap::from([(
                "path".to_string(),
                Value::Concrete(ConcreteValue::String("carina.state.json".into())),
            )]),
            attribute_spans: Default::default(),
        }),
        plan,
        sorted_resources: sorted_resources.clone(),
        unresolved_resources: sorted_resources,
        compositions: vec![],
        data_sources: vec![],
        data_source_origins: vec![],
        data_source_values: vec![],
        current_states,
        upstream_snapshot: HashMap::new(),
        upstream_sources: vec![],
        wait_bindings: vec![],
    };
    record_with_mock_provider(contents)
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
