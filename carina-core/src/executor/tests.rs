use super::*;
use crate::effect::deps::{
    ScheduleInputs, build_effect_dependency_analysis as build_dependency_analysis,
};
use crate::effect::{DeferredReplaceDelete, DeferredReplacePayload, NonEmptyDeletes};
use crate::plan::Plan;
use crate::provider::{
    BoxFuture, CreateRequest, DeleteRequest, NoopNormalizer, ProviderError, ProviderResult,
    ReadRequest, UpdateRequest,
};
use crate::resource::{
    AccessPath, ConcreteValue, DataSource, DeferredValue, Directives, ResolvedDataSource,
    ResolvedResource, Resource, ResourceIdentity, UnknownReason, Value,
};
use crate::value::SerializationError;
use parallel::build_dependency_levels;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio_util::sync::CancellationToken;

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

fn resolved_data_source(resource: DataSource) -> ResolvedDataSource {
    ResolvedDataSource::new(resource)
}

fn create_effect(resource: Resource) -> Effect {
    Effect::Create(resolved(resource))
}

fn validation_deferred_for_expression() -> crate::parser::DeferredForExpression {
    let mut template_resource = Resource::new("test", "validation_records");
    template_resource.binding = Some("validation_records".to_string());
    template_resource
        .dependency_bindings
        .insert("cert".to_string());
    template_resource.set_attr(
        "name",
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValuePath {
            path: AccessPath::with_fields("opt", "resource_record", vec!["name".to_string()]),
        })),
    );
    template_resource.set_attr(
        "value",
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValuePath {
            path: AccessPath::with_fields("opt", "resource_record", vec!["value".to_string()]),
        })),
    );

    crate::parser::DeferredForExpression {
        file: None,
        line: 1,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "test.ValidationRecord".to_string(),
        attributes: template_resource
            .attributes
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        binding_name: "validation_records".to_string(),
        iterable_binding: "cert".to_string(),
        iterable_attr: "domain_validation_options".to_string(),
        binding: crate::parser::ForBinding::Simple("opt".to_string()),
        template_resource,
    }
}

// -----------------------------------------------------------------------
// Mock Provider
// -----------------------------------------------------------------------

struct MockProvider {
    create_results: Mutex<Vec<ProviderResult<crate::provider::CreateOutcome>>>,
    delete_results: Mutex<Vec<ProviderResult<()>>>,
    update_results: Mutex<Vec<ProviderResult<crate::provider::UpdateOutcome>>>,
    read_results: Mutex<Vec<ProviderResult<State>>>,
    /// Records calls in order: ("create"|"delete"|"update"|"read", resource_id_string)
    call_log: Arc<Mutex<Vec<(String, String)>>>,
    /// Resources passed in to `create()` in call order — lets a test
    /// assert that the executor handed the provider a fully-resolved
    /// resource (no remaining `Value::Deferred(ResourceRef)` etc.).
    create_resources: Arc<Mutex<Vec<Resource>>>,
    /// `UpdateRequest`s passed in to `update()` in call order — lets a
    /// test assert the patch carries re-normalized attribute values.
    update_requests: Arc<Mutex<Vec<UpdateRequest>>>,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            create_results: Mutex::new(Vec::new()),
            delete_results: Mutex::new(Vec::new()),
            update_results: Mutex::new(Vec::new()),
            read_results: Mutex::new(Vec::new()),
            call_log: Arc::new(Mutex::new(Vec::new())),
            create_resources: Arc::new(Mutex::new(Vec::new())),
            update_requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn push_create(&self, result: ProviderResult<State>) {
        self.create_results
            .lock()
            .unwrap()
            .push(result.map(|state| crate::provider::CreateOutcome::Success { state }));
    }

    fn push_create_outcome(&self, result: ProviderResult<crate::provider::CreateOutcome>) {
        self.create_results.lock().unwrap().push(result);
    }

    fn push_delete(&self, result: ProviderResult<()>) {
        self.delete_results.lock().unwrap().push(result);
    }

    fn push_update(&self, result: ProviderResult<State>) {
        self.update_results
            .lock()
            .unwrap()
            .push(result.map(|state| crate::provider::UpdateOutcome::Success { state }));
    }

    fn push_read(&self, result: ProviderResult<State>) {
        self.read_results.lock().unwrap().push(result);
    }

    fn calls(&self) -> Vec<(String, String)> {
        self.call_log.lock().unwrap().clone()
    }

    fn captured_create_resources(&self) -> Vec<Resource> {
        self.create_resources.lock().unwrap().clone()
    }

    fn captured_update_requests(&self) -> Vec<UpdateRequest> {
        self.update_requests.lock().unwrap().clone()
    }
}

impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id_str = id.to_string();
        self.call_log
            .lock()
            .unwrap()
            .push(("read".to_string(), id_str));
        let result = self.read_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        self.read(&resource.id, None, ReadRequest)
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::CreateOutcome>> {
        let id_str = id.to_string();
        self.call_log
            .lock()
            .unwrap()
            .push(("create".to_string(), id_str));
        self.create_resources
            .lock()
            .unwrap()
            .push(request.resource.as_resource().clone());
        let result = self.create_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        let id_str = id.to_string();
        self.call_log
            .lock()
            .unwrap()
            .push(("update".to_string(), id_str));
        self.update_requests.lock().unwrap().push(request);
        let result = self.update_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id_str = id.to_string();
        self.call_log
            .lock()
            .unwrap()
            .push(("delete".to_string(), id_str));
        let result = self.delete_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: crate::effect::PlanOp) -> Vec<String> {
        Vec::new()
    }
}

fn call_position(calls: &[(String, String)], op: &str, id: &str) -> usize {
    calls
        .iter()
        .position(|(call_op, call_id)| call_op == op && call_id == id)
        .unwrap_or_else(|| panic!("expected {op} call for {id}; calls: {calls:?}"))
}

// -----------------------------------------------------------------------
// Mock Observer
// -----------------------------------------------------------------------

struct MockObserver {
    events: Mutex<Vec<String>>,
}

fn format_execution_event(event: &ExecutionEvent<'_>) -> String {
    match event {
        ExecutionEvent::Waiting {
            effect,
            pending_dependencies,
        } => {
            format!(
                "waiting:{}:[{}]",
                effect.resource_id(),
                pending_dependencies.join(",")
            )
        }
        ExecutionEvent::EffectStarted { effect } => {
            format!("started:{}", effect.resource_id())
        }
        ExecutionEvent::EffectSucceeded { effect, .. } => {
            format!("succeeded:{}", effect.resource_id())
        }
        ExecutionEvent::EffectPartiallySucceeded { effect, .. } => {
            format!("partial:{}", effect.resource_id())
        }
        ExecutionEvent::EffectFailed { effect, error, .. } => {
            format!("failed:{}:{}", effect.resource_id(), error)
        }
        ExecutionEvent::EffectSkipped { effect, reason, .. } => {
            format!("skipped:{}:{}", effect.resource_id(), reason)
        }
        ExecutionEvent::WaitPolling { observation, .. } => {
            format!(
                "wait_polling:{}:{}",
                observation.binding(),
                observation.target_id()
            )
        }
        ExecutionEvent::CascadeUpdateSucceeded { id } => {
            format!("cascade_ok:{}", id)
        }
        ExecutionEvent::CascadeUpdateFailed { id, error } => {
            format!("cascade_fail:{}:{}", id, error)
        }
        ExecutionEvent::RenameSucceeded { id, from, to } => {
            format!("rename_ok:{}:{}:{}", id, from, to)
        }
        ExecutionEvent::RenameFailed { id, error } => {
            format!("rename_fail:{}:{}", id, error)
        }
        ExecutionEvent::RefreshStarted => "refresh_started".to_string(),
        ExecutionEvent::RefreshSucceeded { id } => {
            format!("refresh_ok:{}", id)
        }
        ExecutionEvent::RefreshFailed { id, error } => {
            format!("refresh_fail:{}:{}", id, error)
        }
    }
}

impl MockObserver {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

impl ExecutionObserver for MockObserver {
    fn on_event(&self, event: &ExecutionEvent) {
        self.events
            .lock()
            .unwrap()
            .push(format_execution_event(event));
    }
}

fn completed_result(outcome: ExecutionOutcome) -> ExecutionResult {
    match outcome {
        ExecutionOutcome::Completed(result) => result,
        ExecutionOutcome::Cancelled(result) => panic!(
            "uncancelled execution returned Cancelled: success={}, failure={}, skip={}",
            result.success_count, result.failure_count, result.skip_count
        ),
    }
}

fn resource_with_attr(value: Value) -> Resource {
    let mut resource = Resource::new("test", "resolved-resource-test");
    resource.set_attr("attr", value);
    resource
}

fn for_value_path_unknown() -> Value {
    Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValuePath {
        path: AccessPath::with_fields("opt", "resource_record", vec!["name".to_string()]),
    }))
}

#[test]
fn resolved_resource_constructor_rejects_value_unknown() {
    let err = basic::resolved_resource(resource_with_attr(for_value_path_unknown()))
        .expect_err("deferred unknown must not construct a ResolvedResource");

    assert!(
        matches!(err, SerializationError::UnknownNotAllowed { .. }),
        "expected UnknownNotAllowed, got {err:?}"
    );
}

#[test]
fn resolved_resource_constructor_rejects_resource_ref() {
    let value = Value::Deferred(DeferredValue::ResourceRef {
        path: AccessPath::new("cert", "domain_validation_options"),
    });

    let err = basic::resolved_resource(resource_with_attr(value))
        .expect_err("resource refs must not construct a ResolvedResource");

    assert!(
        matches!(err, SerializationError::UnresolvedResourceRef { .. }),
        "expected UnresolvedResourceRef, got {err:?}"
    );
}

#[test]
fn resolved_resource_constructor_rejects_nested_deferred() {
    let mut map = indexmap::IndexMap::new();
    map.insert(
        "secret".to_string(),
        Value::Deferred(DeferredValue::Secret(Box::new(for_value_path_unknown()))),
    );
    let nested = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::Map(map),
    )]));

    let err = basic::resolved_resource(resource_with_attr(nested))
        .expect_err("nested deferred values must not construct a ResolvedResource");

    assert!(
        matches!(err, SerializationError::UnknownNotAllowed { .. }),
        "expected UnknownNotAllowed, got {err:?}"
    );
}

// -----------------------------------------------------------------------
// Mock Normalizer
// -----------------------------------------------------------------------

/// Rewrites any string `"raw_dsl"` to `"CANONICAL"`, recursing into
/// Map / List containers. Models a real provider normalizer that
/// canonicalizes a DSL spelling nested under a struct field (the
/// aws#315 IAM-policy `version`/`effect` shape). Used to prove the
/// apply path re-runs `normalize_desired` after reference
/// re-resolution (carina#3060).
struct CanonicalizingNormalizer;

fn canonicalize_value(v: &Value) -> Option<Value> {
    match v {
        Value::Concrete(ConcreteValue::String(s)) if s == "raw_dsl" => Some(Value::Concrete(
            ConcreteValue::String("CANONICAL".to_string()),
        )),
        Value::Concrete(ConcreteValue::Map(m)) => {
            let mut out = m.clone();
            let mut changed = false;
            for (k, val) in m {
                if let Some(nv) = canonicalize_value(val) {
                    out.insert(k.clone(), nv);
                    changed = true;
                }
            }
            changed.then_some(Value::Concrete(ConcreteValue::Map(out)))
        }
        Value::Concrete(ConcreteValue::List(items)) => {
            let mut out = items.clone();
            let mut changed = false;
            for (i, item) in items.iter().enumerate() {
                if let Some(nv) = canonicalize_value(item) {
                    out[i] = nv;
                    changed = true;
                }
            }
            changed.then_some(Value::Concrete(ConcreteValue::List(out)))
        }
        _ => None,
    }
}

impl crate::provider::ProviderNormalizer for CanonicalizingNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [Resource],
    ) -> crate::provider::BoxFuture<'a, ()> {
        Box::pin(async move {
            for r in resources.iter_mut() {
                let keys: Vec<String> = r.attributes.keys().cloned().collect();
                for k in keys {
                    if let Some(v) = r.get_attr(&k)
                        && let Some(nv) = canonicalize_value(v)
                    {
                        r.set_attr(k, nv);
                    }
                }
            }
        })
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
    ) -> crate::provider::BoxFuture<'a, ()> {
        crate::provider::ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
        _saved_attrs: &'a crate::provider::SavedAttrs,
    ) -> crate::provider::BoxFuture<'a, ()> {
        crate::provider::ready_noop()
    }

    fn merge_default_tags<'a>(
        &'a self,
        _resources: &'a mut [Resource],
        _default_tags: &'a indexmap::IndexMap<String, Value>,
        _registry: &'a crate::schema::SchemaRegistry,
    ) -> crate::provider::BoxFuture<'a, ()> {
        crate::provider::ready_noop()
    }
}

struct DefaultTagsNormalizer;

impl crate::provider::ProviderNormalizer for DefaultTagsNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        _resources: &'a mut [Resource],
    ) -> crate::provider::BoxFuture<'a, ()> {
        crate::provider::ready_noop()
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
    ) -> crate::provider::BoxFuture<'a, ()> {
        crate::provider::ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
        _saved_attrs: &'a crate::provider::SavedAttrs,
    ) -> crate::provider::BoxFuture<'a, ()> {
        crate::provider::ready_noop()
    }

    fn merge_default_tags<'a>(
        &'a self,
        resources: &'a mut [Resource],
        default_tags: &'a indexmap::IndexMap<String, Value>,
        _registry: &'a crate::schema::SchemaRegistry,
    ) -> crate::provider::BoxFuture<'a, ()> {
        Box::pin(async move {
            for resource in resources {
                let mut tags = match resource.get_attr("tags") {
                    Some(Value::Concrete(ConcreteValue::Map(tags))) => tags.clone(),
                    _ => indexmap::IndexMap::new(),
                };
                for (key, value) in default_tags {
                    tags.entry(key.clone()).or_insert_with(|| value.clone());
                }
                resource.set_attr("tags", Value::Concrete(ConcreteValue::Map(tags)));
            }
        })
    }
}

struct SecretListToScalarNormalizer;

impl crate::provider::ProviderNormalizer for SecretListToScalarNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [Resource],
    ) -> crate::provider::BoxFuture<'a, ()> {
        Box::pin(async move {
            for resource in resources {
                if let Some(Value::Concrete(ConcreteValue::List(items))) =
                    resource.get_attr("master_password")
                    && let Some(Value::Concrete(ConcreteValue::String(first))) = items.first()
                {
                    resource.set_attr(
                        "master_password",
                        Value::Concrete(ConcreteValue::String(first.clone())),
                    );
                }
            }
        })
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
    ) -> crate::provider::BoxFuture<'a, ()> {
        crate::provider::ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut HashMap<ResourceId, State>,
        _saved_attrs: &'a crate::provider::SavedAttrs,
    ) -> crate::provider::BoxFuture<'a, ()> {
        crate::provider::ready_noop()
    }

    fn merge_default_tags<'a>(
        &'a self,
        _resources: &'a mut [Resource],
        _default_tags: &'a indexmap::IndexMap<String, Value>,
        _registry: &'a crate::schema::SchemaRegistry,
    ) -> crate::provider::BoxFuture<'a, ()> {
        crate::provider::ready_noop()
    }
}

// -----------------------------------------------------------------------
// Mock ProviderFactory + shared test fixtures
// -----------------------------------------------------------------------

use crate::provider::ProviderFactory;
use crate::schema::SchemaRegistry;
use std::sync::LazyLock;

/// Empty schema registry shared by tests that don't exercise the
/// canonicalize stage. `'static` so it can back `&` in `ExecutionInput`.
static TEST_SCHEMAS: LazyLock<SchemaRegistry> = LazyLock::new(SchemaRegistry::new);

/// Registry whose `test`-provider `sg` resource declares `subject` as
/// `Union[String, list(String)]` (the `string_or_list_of_strings`
/// shape), so the apply-path canonicalize stage (#2481/#2511) has a
/// schema to act on. carina#3063: this stage is plan pipeline stage 1
/// and must also be re-applied at apply time.
static CANON_SCHEMAS: LazyLock<SchemaRegistry> = LazyLock::new(|| {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut reg = SchemaRegistry::new();
    let schema = ResourceSchema::new("sg").attribute(AttributeSchema::new(
        "subject",
        AttributeType::union(vec![
            AttributeType::string(),
            AttributeType::list(AttributeType::string()),
        ]),
    ));
    reg.insert("test", schema);
    reg
});

static AUGMENT_COMPARISON_SCHEMAS: LazyLock<SchemaRegistry> = LazyLock::new(|| {
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, StructField, enum_identity,
    };

    let mut reg = SchemaRegistry::new();
    let schema = ResourceSchema::new("a")
        .attribute(AttributeSchema::new("description", AttributeType::string()))
        .attribute(AttributeSchema::new("size", AttributeType::float()))
        .attribute(AttributeSchema::new(
            "options",
            AttributeType::struct_(
                "Options",
                vec![
                    StructField::new("enabled", AttributeType::bool()),
                    StructField::new("label", AttributeType::string()),
                ],
            ),
        ))
        .attribute(AttributeSchema::new(
            "mode",
            AttributeType::enum_(
                enum_identity("Mode", Some("test")),
                Some(vec!["AES256".to_string(), "aws:kms".to_string()]),
                Vec::new(),
                None,
                None,
            ),
        ))
        .attribute(AttributeSchema::new("write_only_token", AttributeType::string()).write_only())
        .attribute(AttributeSchema::new(
            "master_password",
            AttributeType::string(),
        ));
    reg.insert("test", schema);
    reg
});

/// Factory that maps the enum DSL alias `all` → AWS canonical `"-1"`
/// for the `ip_protocol` attribute, modeling plan-time stage 3
/// (`resolve_enum_aliases`). carina#3063: apply must re-apply this.
struct AliasFactory;
impl ProviderFactory for AliasFactory {
    fn name(&self) -> &str {
        "test"
    }
    fn display_name(&self) -> &str {
        "Test"
    }
    fn provider_config_attribute_types(&self) -> HashMap<String, crate::schema::AttributeType> {
        HashMap::new()
    }
    fn validate_config(
        &self,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> Result<(), String> {
        Ok(())
    }
    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        String::new()
    }
    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn crate::provider::Provider>>> {
        unreachable!("test factory does not create providers")
    }
    fn schemas(&self) -> Vec<crate::schema::ResourceSchema> {
        use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, enum_identity};

        vec![ResourceSchema::new("sg").attribute(AttributeSchema::new(
            "ip_protocol",
            AttributeType::enum_(
                enum_identity("IpProtocol", Some("test.ec2.SecurityGroup")),
                Some(vec!["all".to_string(), "tcp".to_string()]),
                Vec::new(),
                None,
                None,
            ),
        ))]
    }
    fn get_enum_alias_reverse(
        &self,
        _resource_type: &str,
        attr_name: &str,
        value: &str,
    ) -> Option<String> {
        (attr_name == "ip_protocol" && value == "all").then(|| "-1".to_string())
    }
}

// -----------------------------------------------------------------------
// Helper functions
// -----------------------------------------------------------------------

fn make_resource(binding: &str, deps: &[&str]) -> Resource {
    let mut r = Resource::new("test", binding);
    r.binding = Some(binding.to_string());
    for dep in deps {
        r.set_attr(
            format!("ref_{}", dep),
            Value::resource_ref(dep.to_string(), "id".to_string(), vec![]),
        );
    }
    // Save dependency bindings as metadata (normally done by resolver)
    if !deps.is_empty() {
        r.dependency_bindings = deps.iter().map(|d| d.to_string()).collect();
    }
    r
}

fn ok_state(id: &ResourceId) -> State {
    // The `id` attribute mirrors what a real provider's read-back
    // publishes after Create — without it, dependents created via
    // `make_resource(name, &["dep"])` (which writes `ref_dep =
    // ResourceRef(dep, "id")`) cannot resolve their references and
    // post-#3032 the executor rejects them at the apply seam.
    let mut attrs = HashMap::new();
    attrs.insert(
        "id".to_string(),
        Value::Concrete(ConcreteValue::String("id-123".to_string())),
    );
    State::existing(id.clone(), attrs).with_identifier("id-123")
}

fn empty_execution_result() -> ExecutionResult {
    ExecutionResult {
        success_count: 0,
        failure_count: 0,
        partial_count: 0,
        partial_diagnostics: Vec::new(),
        skip_count: 0,
        applied_states: Default::default(),
        runtime_synthesized_resources: Vec::new(),
        successfully_deleted: HashSet::new(),
        permanent_name_overrides: HashMap::new(),
        current_states: HashMap::new(),
        bindings: ResolvedBindings::default(),
        failed_refreshes: HashSet::new(),
    }
}

fn provider_config_with_default_tags(
    tags: indexmap::IndexMap<String, Value>,
) -> crate::parser::ProviderConfig {
    crate::parser::ProviderConfig {
        name: "test".to_string(),
        attributes: indexmap::IndexMap::new(),
        default_tags: tags,
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: indexmap::IndexMap::new(),
        binding: None,
        is_default: true,
    }
}

fn create_independent_create_plan<const N: usize>(names: [&str; N]) -> Plan {
    let mut plan = Plan::new();
    for name in names {
        plan.add(create_effect(make_resource(name, &[])));
    }
    plan
}

struct DelayedCountingProvider {
    default_delay: std::time::Duration,
    delays: HashMap<String, std::time::Duration>,
    cancel_after_create: Option<String>,
    cancel: Option<CancellationToken>,
    started: Arc<Mutex<Vec<String>>>,
}

impl DelayedCountingProvider {
    fn new(default_delay: std::time::Duration) -> Self {
        Self {
            default_delay,
            delays: HashMap::new(),
            cancel_after_create: None,
            cancel: None,
            started: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_delay(mut self, name: &str, delay: std::time::Duration) -> Self {
        self.delays.insert(name.to_string(), delay);
        self
    }

    fn started_names(&self) -> Vec<String> {
        self.started.lock().unwrap().clone()
    }

    fn delay_for(&self, id: &ResourceId) -> std::time::Duration {
        self.delays
            .get(id.identity_or_empty())
            .copied()
            .unwrap_or(self.default_delay)
    }
}

impl Provider for DelayedCountingProvider {
    fn name(&self) -> &str {
        "delayed-counting"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("read not used")) })
    }

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        self.read(&resource.id, None, ReadRequest)
    }

    fn create(
        &self,
        id: &ResourceId,
        _request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::CreateOutcome>> {
        let id = id.clone();
        let delay = self.delay_for(&id);
        let started = self.started.clone();
        let cancel_after_create = self.cancel_after_create.clone();
        let cancel = self.cancel.clone();
        Box::pin(async move {
            started
                .lock()
                .unwrap()
                .push(id.identity_or_empty().to_string());
            tokio::time::sleep(delay).await;
            if cancel_after_create.as_deref() == Some(id.identity_or_empty())
                && let Some(cancel) = cancel
            {
                cancel.cancel();
            }
            Ok(crate::provider::CreateOutcome::Success {
                state: ok_state(&id),
            })
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        let id = id.clone();
        let started = self.started.clone();
        Box::pin(async move {
            started
                .lock()
                .unwrap()
                .push(format!("update:{}", id.identity_or_empty()));
            let mut attrs = HashMap::new();
            attrs.insert(
                "finalized".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            );
            Ok(crate::provider::UpdateOutcome::Success {
                state: State::existing(id, attrs).with_identifier("finalized-id"),
            })
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        let started = self.started.clone();
        Box::pin(async move {
            started
                .lock()
                .unwrap()
                .push(format!("delete:{}", id.identity_or_empty()));
            Ok(())
        })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: crate::effect::PlanOp) -> Vec<String> {
        Vec::new()
    }
}

struct PendingWaitProvider {
    reads: AtomicUsize,
}

impl PendingWaitProvider {
    fn new() -> Self {
        Self {
            reads: AtomicUsize::new(0),
        }
    }

    fn read_count(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
}

impl Provider for PendingWaitProvider {
    fn name(&self) -> &str {
        "pending-wait"
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let id = id.clone();
        Box::pin(async move {
            let mut attrs = HashMap::new();
            attrs.insert(
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("PENDING".to_string())),
            );
            Ok(State::existing(id, attrs).with_identifier("pending-id"))
        })
    }

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        self.read(&resource.id, None, ReadRequest)
    }

    fn create(
        &self,
        id: &ResourceId,
        _request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::CreateOutcome>> {
        let id = id.clone();
        Box::pin(async move {
            Ok(crate::provider::CreateOutcome::Success {
                state: ok_state(&id),
            })
        })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("update not used")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("delete not used")) })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: crate::effect::PlanOp) -> Vec<String> {
        Vec::new()
    }
}

struct CancelsAfterSuccesses {
    successes: AtomicUsize,
    threshold: usize,
    token: CancellationToken,
}

impl CancelsAfterSuccesses {
    fn new(threshold: usize) -> Self {
        Self {
            successes: AtomicUsize::new(0),
            threshold,
            token: CancellationToken::new(),
        }
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl ExecutionObserver for CancelsAfterSuccesses {
    fn on_event(&self, event: &ExecutionEvent) {
        if matches!(event, ExecutionEvent::EffectSucceeded { .. }) {
            let successes = self.successes.fetch_add(1, Ordering::Relaxed) + 1;
            if successes >= self.threshold {
                self.token.cancel();
            }
        }
    }
}

struct CancelsWhenStarted {
    name: String,
    token: CancellationToken,
}

impl CancelsWhenStarted {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            token: CancellationToken::new(),
        }
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

struct CancelsWhenWaitStarted {
    binding: String,
    token: CancellationToken,
}

impl CancelsWhenWaitStarted {
    fn new(binding: &str) -> Self {
        Self {
            binding: binding.to_string(),
            token: CancellationToken::new(),
        }
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl ExecutionObserver for CancelsWhenWaitStarted {
    fn on_event(&self, event: &ExecutionEvent) {
        if let ExecutionEvent::EffectStarted {
            effect: Effect::Wait { identity, .. },
        } = event
            && identity.as_str() == self.binding
        {
            self.token.cancel();
        }
    }
}

struct RecordingCancelsWhenWaitStarted {
    binding: String,
    token: CancellationToken,
    events: Mutex<Vec<String>>,
}

impl RecordingCancelsWhenWaitStarted {
    fn new(binding: &str) -> Self {
        Self {
            binding: binding.to_string(),
            token: CancellationToken::new(),
            events: Mutex::new(Vec::new()),
        }
    }

    fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

impl ExecutionObserver for RecordingCancelsWhenWaitStarted {
    fn on_event(&self, event: &ExecutionEvent) {
        self.events
            .lock()
            .unwrap()
            .push(format_execution_event(event));
        if let ExecutionEvent::EffectStarted {
            effect: Effect::Wait { identity, .. },
        } = event
            && identity.as_str() == self.binding
        {
            self.token.cancel();
        }
    }
}

impl ExecutionObserver for CancelsWhenStarted {
    fn on_event(&self, event: &ExecutionEvent) {
        if let ExecutionEvent::EffectStarted { effect } = event
            && effect.resource_id().identity_or_empty() == self.name
        {
            self.token.cancel();
        }
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[test]
fn execution_outcome_completed_and_cancelled_are_matchable() {
    let completed = ExecutionOutcome::Completed(empty_execution_result());
    let cancelled = ExecutionOutcome::Cancelled(empty_execution_result());

    match completed {
        ExecutionOutcome::Completed(result) => {
            assert_eq!(result.success_count, 0);
            assert!(result.applied_states.is_empty());
        }
        ExecutionOutcome::Cancelled(_) => panic!("completed outcome changed variant"),
    }

    match cancelled {
        ExecutionOutcome::Cancelled(result) => {
            assert_eq!(result.failure_count, 0);
            assert!(result.successfully_deleted.is_empty());
        }
        ExecutionOutcome::Completed(_) => panic!("cancelled outcome changed variant"),
    }
}

#[tokio::test]
async fn execute_plan_returns_completed_when_not_cancelled() {
    let provider = MockProvider::new();
    let resource = make_resource("one", &[]);
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(resource));

    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let cancel = CancellationToken::new();

    let outcome = execute_plan(&provider, input, &observer, cancel).await;

    match outcome {
        ExecutionOutcome::Completed(result) => {
            assert_eq!(result.failure_count, 0);
            assert_eq!(result.success_count, 1);
        }
        ExecutionOutcome::Cancelled(_) => panic!("uncancelled execution returned Cancelled"),
    }
}

#[tokio::test]
async fn execute_plan_with_pre_cancelled_token_returns_cancelled_at_t4_or_later() {
    let provider = MockProvider::new();
    let resource = make_resource("one", &[]);

    let mut plan = Plan::new();
    plan.add(create_effect(resource));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let outcome = execute_plan(&provider, input, &observer, cancel).await;

    match outcome {
        ExecutionOutcome::Cancelled(result) => {
            assert_eq!(result.success_count, 0);
            assert!(result.applied_states.is_empty());
            assert!(provider.calls().is_empty());
        }
        ExecutionOutcome::Completed(_) => panic!("pre-cancelled execution returned Completed"),
    }
}

#[tokio::test]
async fn execute_plan_with_empty_plan_and_pre_cancelled_token_returns_completed() {
    let provider = MockProvider::new();
    let plan = Plan::new();
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };
    let observer = MockObserver::new();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let outcome = execute_plan(&provider, input, &observer, cancel).await;

    match outcome {
        ExecutionOutcome::Completed(result) => {
            assert_eq!(result.success_count, 0);
            assert_eq!(result.skip_count, 0);
        }
        ExecutionOutcome::Cancelled(_) => panic!("empty pre-cancelled plan returned Cancelled"),
    }
}

#[tokio::test]
async fn execute_plan_cancelled_after_three_completed_keeps_in_flight_and_drops_pending() {
    let provider = DelayedCountingProvider::new(std::time::Duration::from_millis(1));
    let observer = CancelsAfterSuccesses::new(3);
    let cancel = observer.token();
    let plan = create_independent_create_plan(["r1", "r2", "r3", "r4", "r5"]);
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
    };

    let outcome = execute_plan(&provider, input, &observer, cancel).await;

    let result = match outcome {
        ExecutionOutcome::Cancelled(result) => result,
        ExecutionOutcome::Completed(_) => panic!("cancelled run returned Completed"),
    };
    assert_eq!(result.applied_states.len(), 3);
    assert_eq!(provider.started_names(), vec!["r1", "r2", "r3"]);
}

#[tokio::test]
async fn execute_plan_cancels_in_flight_wait_effect_promptly() {
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let provider = PendingWaitProvider::new();
    let observer = CancelsWhenWaitStarted::new("cert_ready");
    let cancel = observer.token();

    let cert = make_resource("cert", &[]);
    let cert_id = cert.id.clone();
    let mut dist = make_resource("dist", &[]);
    dist.set_attr(
        "ref_cert_ready".to_string(),
        Value::resource_ref("cert_ready".to_string(), "id".to_string(), vec![]),
    );
    dist.dependency_bindings = ["cert_ready".to_string()].into_iter().collect();
    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: ResourceIdentity::new("cert_ready"),
        target_id: crate::resource::ResolvedResourceId::new(cert_id),
        until: WaitPredicate::Equals {
            attr: AttrPath::single("status"),
            value: Value::Concrete(ConcreteValue::String("READY".to_string())),
        },
        until_surface: "cert.status == READY".to_string(),
        timeout: std::time::Duration::from_secs(60),
        interval: std::time::Duration::from_secs(60),
        explicit_dependencies: std::collections::HashSet::new(),
    });
    plan.add(create_effect(dist));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
    };

    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        execute_plan(&provider, input, &observer, cancel),
    )
    .await
    .expect("cancelled wait should not block until its natural timeout");

    let result = match outcome {
        ExecutionOutcome::Cancelled(result) => result,
        ExecutionOutcome::Completed(_) => panic!("cancelled wait returned Completed"),
    };
    assert_eq!(result.success_count, 1, "the completed Create is preserved");
    assert_eq!(
        result.skip_count, 2,
        "the in-flight Wait and suppressed downstream Create are skipped"
    );
    assert!(
        provider.read_count() > 0,
        "wait should have polled at least once"
    );
}

#[tokio::test]
async fn execute_plan_cancelled_wait_emits_cancelled_skip_not_unsatisfiable() {
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let provider = PendingWaitProvider::new();
    let observer = RecordingCancelsWhenWaitStarted::new("cert_ready");
    let cancel = observer.token();

    let cert = make_resource("cert", &[]);
    let cert_id = cert.id.clone();
    let mut dist = make_resource("dist", &[]);
    dist.set_attr(
        "ref_cert_ready".to_string(),
        Value::resource_ref("cert_ready".to_string(), "id".to_string(), vec![]),
    );
    dist.dependency_bindings = ["cert_ready".to_string()].into_iter().collect();

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: ResourceIdentity::new("cert_ready"),
        target_id: crate::resource::ResolvedResourceId::new(cert_id),
        until: WaitPredicate::Equals {
            attr: AttrPath::single("status"),
            value: Value::Concrete(ConcreteValue::String("READY".to_string())),
        },
        until_surface: "cert.status == READY".to_string(),
        timeout: std::time::Duration::from_secs(60),
        interval: std::time::Duration::from_secs(60),
        explicit_dependencies: std::collections::HashSet::new(),
    });
    plan.add(create_effect(dist));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
    };

    let outcome = execute_plan(&provider, input, &observer, cancel).await;

    let result = match outcome {
        ExecutionOutcome::Cancelled(result) => result,
        ExecutionOutcome::Completed(_) => panic!("cancelled wait returned Completed"),
    };
    let events = observer.events();
    assert!(
        events.iter().any(|event| event == "succeeded:test.cert"),
        "cert create should succeed before cancellation; events: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event == "skipped:test.cert:cancelled"),
        "in-flight Wait should be skipped as cancelled; events: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event == "skipped:test.dist:cancelled"),
        "downstream Create should be skipped as cancelled; events: {events:?}"
    );
    assert!(
        events.iter().all(|event| !event.contains("unsatisfiable")),
        "cancelled Wait must not be reported as unsatisfiable; events: {events:?}"
    );
    assert!(
        events
            .iter()
            .all(|event| !event.contains("dependency 'cert_ready' failed")),
        "cancelled Wait must not mark its binding failed; events: {events:?}"
    );
    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 0);
    assert_eq!(result.skip_count, 2);
}

#[tokio::test]
async fn execute_plan_cancelled_while_effect_in_flight_records_that_effect() {
    let provider = DelayedCountingProvider::new(std::time::Duration::ZERO)
        .with_delay("r2", std::time::Duration::from_millis(10));
    let observer = CancelsWhenStarted::new("r2");
    let cancel = observer.token();
    let plan = create_independent_create_plan(["r1", "r2", "r3"]);
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
    };

    let outcome = execute_plan(&provider, input, &observer, cancel).await;

    let result = match outcome {
        ExecutionOutcome::Cancelled(result) => result,
        ExecutionOutcome::Completed(_) => panic!("cancelled run returned Completed"),
    };
    assert!(
        result
            .applied_states
            .contains_key(&ResourceId::with_identity("test", "r2"))
    );
    assert_eq!(provider.started_names(), vec!["r1", "r2"]);
}

#[tokio::test]
async fn test_simple_create() {
    let provider = MockProvider::new();
    let resource = make_resource("a", &[]);
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(resource));

    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 0);
    assert!(
        observer
            .events()
            .iter()
            .any(|e| e.starts_with("succeeded:"))
    );
}

#[tokio::test]
async fn partial_create_records_state_and_diagnostic() {
    let provider = MockProvider::new();
    let resource = make_resource("a", &[]);
    let rid = resource.id.clone();
    let diagnostic = crate::provider::PartialReadDiagnostic::new(
        "mock partial create".to_string(),
        vec!["computed".to_string()],
    )
    .expect("missing attributes are non-empty");

    let mut plan = Plan::new();
    plan.add(create_effect(resource));

    provider.push_create_outcome(Ok(crate::provider::CreateOutcome::partial_success(
        ok_state(&rid),
        "mock partial create".to_string(),
        vec!["computed".to_string()],
    )));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 0);
    assert_eq!(result.partial_count, 1);
    assert!(result.applied_states.contains_key(&rid));
    assert_eq!(result.partial_diagnostics, vec![(rid, diagnostic)]);
    assert!(observer.events().iter().any(|e| e.starts_with("partial:")));
}

/// carina#3060: the apply execution path must re-apply the provider
/// normalizer after reference re-resolution, before building the
/// provider request. Plan-time normalization is undone when the
/// executor rebuilds attributes from the (un-normalized) source, so
/// without a re-normalize the provider receives the raw DSL spelling.
///
/// This exercises the *apply path* (`execute_plan`), not
/// `normalize_desired` in isolation — the gap the prior
/// carina-provider-aws#316 unit test missed.
#[tokio::test]
async fn test_apply_renormalizes_after_resolution() {
    let provider = MockProvider::new();
    let mut resource = make_resource("a", &[]);
    // The DSL spelling a provider normalizer would canonicalize at
    // plan time. The executor must re-canonicalize it on the apply
    // path so the provider never sees `"raw_dsl"`.
    resource.set_attr(
        "marker",
        Value::Concrete(ConcreteValue::String("raw_dsl".to_string())),
    );
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &CanonicalizingNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 1);

    let captured = provider.captured_create_resources();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].get_attr("marker"),
        Some(&Value::Concrete(ConcreteValue::String(
            "CANONICAL".to_string()
        ))),
        "apply path must re-run normalize_desired so the provider \
         receives the canonical value, not the raw DSL spelling"
    );
}

/// carina#3063: the apply path must also re-apply plan-time stage 3
/// (enum-alias resolution, `get_enum_alias_reverse`), not just
/// `normalize_desired` (stage 2). After plan-time normalization the
/// value is the namespaced DSL form (`...IpProtocol.all`); apply
/// re-resolves from the un-normalized source, so without re-applying
/// the alias stage the provider receives the namespaced/aliased form
/// instead of the AWS canonical `"-1"`.
#[tokio::test]
async fn test_apply_reapplies_enum_alias_stage() {
    let provider = MockProvider::new();
    // `id.provider = "test"` so the per-resource factory lookup finds
    // `AliasFactory` (whose `name()` is `"test"`).
    let mut resource = Resource::with_provider("test", "sg", "a", None);
    resource.binding = Some("a".to_string());
    // Post-normalize_desired shape: namespaced DSL enum identifier.
    resource.set_attr(
        "ip_protocol",
        Value::Concrete(ConcreteValue::String(
            "test.ec2.SecurityGroup.IpProtocol.all".to_string(),
        )),
    );
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    provider.push_create(Ok(ok_state(&rid)));

    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(AliasFactory)];
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &factories,
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 1);

    let captured = provider.captured_create_resources();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].get_attr("ip_protocol"),
        Some(&Value::Concrete(ConcreteValue::String("-1".to_string()))),
        "apply path must re-apply the enum-alias stage so the provider \
         receives the AWS canonical value, not the namespaced DSL form"
    );
}

/// carina#3063, Update path: the enum-alias stage must be re-applied on
/// Update too, not just Create (carina#3060's lesson — aws#315 was an
/// Update bug). The `UpdateRequest.patch` is built from the re-resolved
/// `to`; without the apply-path enum-alias re-resolution the patch
/// would carry the namespaced DSL form instead of the AWS canonical.
#[tokio::test]
async fn test_apply_reapplies_enum_alias_stage_update_path() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "sg", "a", None);
    to_resource.binding = Some("a".to_string());
    to_resource.set_attr(
        "ip_protocol",
        Value::Concrete(ConcreteValue::String(
            "test.ec2.SecurityGroup.IpProtocol.all".to_string(),
        )),
    );
    let rid = to_resource.id.clone();

    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".to_string())),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["ip_protocol".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(AliasFactory)];
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &factories,
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let op = reqs[0]
        .patch
        .ops
        .iter()
        .find(|op| op.key == "ip_protocol")
        .expect("patch must contain the changed `ip_protocol` attribute");
    assert_eq!(
        op.value,
        Some(Value::Concrete(ConcreteValue::String("-1".to_string()))),
        "Update patch must carry the enum-alias-canonical value, not \
         the namespaced DSL form"
    );
}

/// carina#3063, canonicalize stage (plan pipeline stage 1): the apply
/// path must also re-apply `canonicalize_resources_with_schemas`. With
/// a schema declaring `subject` as `Union[String, list(String)]`, a
/// scalar string must reach the provider canonicalized to a
/// single-element `StringList` — the same coercion the plan path does
/// (#2481/#2511), undone by apply-time re-resolution without this.
#[tokio::test]
async fn test_apply_reapplies_canonicalize_stage() {
    let provider = MockProvider::new();
    let mut resource = Resource::with_provider("test", "sg", "a", None);
    resource.binding = Some("a".to_string());
    resource.set_attr(
        "subject",
        Value::Concrete(ConcreteValue::String("repo:foo:*".to_string())),
    );
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &CANON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 1);

    let captured = provider.captured_create_resources();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].get_attr("subject"),
        Some(&Value::Concrete(ConcreteValue::StringList(vec![
            "repo:foo:*".to_string()
        ]))),
        "apply path must re-apply the canonicalize stage so a scalar in \
         a Union[String,list] field reaches the provider as StringList"
    );
}

/// carina#3060, Update path (the path closest to the aws#315 symptom —
/// `aws.s3.BucketPolicy` failed on *Update*, not Create). The
/// `UpdateRequest.patch` is built from the re-resolved `to`; without
/// the apply-path re-normalize the patch would carry the raw DSL
/// spelling and the provider would reject it (`MalformedPolicy`).
#[tokio::test]
async fn test_apply_renormalizes_update_path() {
    let provider = MockProvider::new();
    let mut to_resource = make_resource("a", &[]);
    to_resource.set_attr(
        "marker",
        Value::Concrete(ConcreteValue::String("raw_dsl".to_string())),
    );
    let rid = to_resource.id.clone();

    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "marker".to_string(),
        Value::Concrete(ConcreteValue::String("old".to_string())),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["marker".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &CanonicalizingNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let marker_op = reqs[0]
        .patch
        .ops
        .iter()
        .find(|op| op.key == "marker")
        .expect("patch must contain the changed `marker` attribute");
    assert_eq!(
        marker_op.value,
        Some(Value::Concrete(ConcreteValue::String(
            "CANONICAL".to_string()
        ))),
        "Update patch must carry the re-normalized value, not raw DSL"
    );
}

#[tokio::test]
async fn test_apply_update_patch_preserves_provider_default_tags() {
    let provider = MockProvider::new();
    let mut to_resource = make_resource("tagged", &[]);
    let rid = to_resource.id.clone();
    let mut desired_tags = indexmap::IndexMap::new();
    desired_tags.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("v1".to_string())),
    );
    to_resource.set_attr("tags", Value::Concrete(ConcreteValue::Map(desired_tags)));

    let mut current_tags = indexmap::IndexMap::new();
    current_tags.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("old".to_string())),
    );
    current_tags.insert(
        "ManagedBy".to_string(),
        Value::Concrete(ConcreteValue::String("carina".to_string())),
    );
    current_tags.insert(
        "Project".to_string(),
        Value::Concrete(ConcreteValue::String("issue-3480".to_string())),
    );
    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "tags".to_string(),
        Value::Concrete(ConcreteValue::Map(current_tags)),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut default_tags = indexmap::IndexMap::new();
    default_tags.insert(
        "ManagedBy".to_string(),
        Value::Concrete(ConcreteValue::String("carina".to_string())),
    );
    default_tags.insert(
        "Project".to_string(),
        Value::Concrete(ConcreteValue::String("issue-3480".to_string())),
    );
    let provider_configs = vec![provider_config_with_default_tags(default_tags)];

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["tags".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &DefaultTagsNormalizer,
        provider_configs: &provider_configs,
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let tags_op = reqs[0]
        .patch
        .ops
        .iter()
        .find(|op| op.key == "tags")
        .expect("patch must contain the changed `tags` attribute");
    let Some(Value::Concrete(ConcreteValue::Map(tags))) = tags_op.value.as_ref() else {
        panic!(
            "expected tags Replace value to be a map, got {:?}",
            tags_op.value
        );
    };
    assert_eq!(
        tags.get("Name"),
        Some(&Value::Concrete(ConcreteValue::String("v1".to_string())))
    );
    assert_eq!(
        tags.get("ManagedBy"),
        Some(&Value::Concrete(ConcreteValue::String(
            "carina".to_string()
        )))
    );
    assert_eq!(
        tags.get("Project"),
        Some(&Value::Concrete(ConcreteValue::String(
            "issue-3480".to_string()
        )))
    );
}

#[tokio::test]
async fn test_apply_effective_changed_uses_plan_time_comparison_semantics() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "a", "a", None);
    to_resource.binding = Some("a".to_string());
    let rid = to_resource.id.clone();

    to_resource.set_attr(
        "description",
        Value::Concrete(ConcreteValue::String("new".to_string())),
    );
    to_resource.set_attr("size", Value::Concrete(ConcreteValue::Float(1.0)));
    let mut desired_options = indexmap::IndexMap::new();
    desired_options.insert(
        "label".to_string(),
        Value::Concrete(ConcreteValue::String("stable".to_string())),
    );
    to_resource.set_attr(
        "options",
        Value::Concrete(ConcreteValue::Map(desired_options)),
    );
    to_resource.set_attr(
        "mode",
        Value::Concrete(ConcreteValue::enum_identifier("test.Mode.AES256")),
    );

    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("old".to_string())),
    );
    from_attrs.insert("size".to_string(), Value::Concrete(ConcreteValue::Int(1)));
    let mut current_options = indexmap::IndexMap::new();
    current_options.insert(
        "label".to_string(),
        Value::Concrete(ConcreteValue::String("stable".to_string())),
    );
    current_options.insert(
        "enabled".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    from_attrs.insert(
        "options".to_string(),
        Value::Concrete(ConcreteValue::Map(current_options)),
    );
    from_attrs.insert(
        "mode".to_string(),
        Value::Concrete(ConcreteValue::String("AES256".to_string())),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["description".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &AUGMENT_COMPARISON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let patched_keys: Vec<&str> = reqs[0].patch.ops.iter().map(|op| op.key.as_str()).collect();
    assert_eq!(patched_keys, vec!["description"]);
}

#[tokio::test]
async fn test_apply_effective_changed_skips_internal_and_write_only_attributes() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "a", "a", None);
    to_resource.binding = Some("a".to_string());
    let rid = to_resource.id.clone();

    to_resource.set_attr(
        "description",
        Value::Concrete(ConcreteValue::String("new".to_string())),
    );
    to_resource.set_attr(
        "_provider_only",
        Value::Concrete(ConcreteValue::String("metadata".to_string())),
    );
    to_resource.set_attr(
        "write_only_token",
        Value::Concrete(ConcreteValue::String("secret-token".to_string())),
    );

    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("old".to_string())),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["description".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &AUGMENT_COMPARISON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let patched_keys: Vec<&str> = reqs[0].patch.ops.iter().map(|op| op.key.as_str()).collect();
    assert_eq!(patched_keys, vec!["description"]);
}

#[tokio::test]
async fn test_apply_effective_changed_skips_matching_unwrapped_secret_hash() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "a", "a", None);
    to_resource.binding = Some("a".to_string());
    let rid = to_resource.id.clone();

    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".to_string()),
    ))));
    to_resource.set_attr("master_password", secret_value.clone());

    let secret_ctx = crate::value::SecretHashContext::new(
        rid.display_type(),
        rid.identity_or_empty(),
        "master_password",
    );
    let hash_json = crate::value::value_to_json_with_context(&secret_value, Some(&secret_ctx))
        .expect("secret hash must serialize");
    let hash_str = hash_json
        .as_str()
        .expect("secret hash must serialize to a string")
        .to_string();

    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "master_password".to_string(),
        Value::Concrete(ConcreteValue::String(hash_str)),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec![],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &AUGMENT_COMPARISON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].patch.ops.is_empty());
}

#[tokio::test]
async fn test_apply_effective_changed_skips_secret_shape_divergence() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "a", "a", None);
    to_resource.binding = Some("a".to_string());
    let rid = to_resource.id.clone();

    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".to_string()),
    ))));
    to_resource.set_attr(
        "master_password",
        Value::Concrete(ConcreteValue::List(vec![
            secret_value.clone(),
            secret_value.clone(),
        ])),
    );

    let secret_ctx = crate::value::SecretHashContext::new(
        rid.display_type(),
        rid.identity_or_empty(),
        "master_password",
    );
    let hash_json = crate::value::value_to_json_with_context(&secret_value, Some(&secret_ctx))
        .expect("secret hash must serialize");
    let hash_str = hash_json
        .as_str()
        .expect("secret hash must serialize to a string")
        .to_string();

    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "master_password".to_string(),
        Value::Concrete(ConcreteValue::String(hash_str)),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec![],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &SecretListToScalarNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &AUGMENT_COMPARISON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0].patch.ops.is_empty(),
        "shape-divergent secret comparison must fail closed instead of patching plaintext"
    );
}

/// carina#3060 acceptance, exact shape: a normalizable value nested
/// under a struct attribute *on a resource that also has a
/// ResourceRef*. This is the real aws#315 regression shape — the ref
/// forces `resolve_resource` to rebuild attributes from the
/// un-normalized source, so the nested `marker` would revert to
/// `"raw_dsl"` without the apply-path re-normalize. Exercises the real
/// `execute_plan` path (Create `a` → state → Create `b` resolves
/// `ref_a` from `a`'s post-create state).
#[tokio::test]
async fn test_apply_renormalizes_nested_value_under_ref_bearing_resource() {
    let provider = MockProvider::new();
    let ra = make_resource("a", &[]);
    let ra_id = ra.id.clone();

    // `b` depends on `a` (ResourceRef `ref_a`) AND carries a
    // normalizable value nested inside a Map attribute `config`.
    let mut rb = make_resource("b", &["a"]);
    let mut config = indexmap::IndexMap::new();
    config.insert(
        "marker".to_string(),
        Value::Concrete(ConcreteValue::String("raw_dsl".to_string())),
    );
    rb.set_attr("config", Value::Concrete(ConcreteValue::Map(config)));
    let rb_id = rb.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(ra));
    plan.add(create_effect(rb));
    provider.push_create(Ok(ok_state(&ra_id)));
    provider.push_create(Ok(ok_state(&rb_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &CanonicalizingNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 2, "both creates should succeed");

    let captured = provider.captured_create_resources();
    let b = captured
        .iter()
        .find(|r| r.id == rb_id)
        .expect("resource b must have been created");
    let Some(Value::Concrete(ConcreteValue::Map(cfg))) = b.get_attr("config") else {
        panic!("expected config Map on b, got {:?}", b.get_attr("config"));
    };
    assert_eq!(
        cfg.get("marker"),
        Some(&Value::Concrete(ConcreteValue::String(
            "CANONICAL".to_string()
        ))),
        "nested value under a ref-bearing resource must be \
         re-normalized at apply, not reverted to raw DSL"
    );
    // The ref itself must still have resolved correctly.
    assert_eq!(
        b.get_attr("ref_a"),
        Some(&Value::Concrete(ConcreteValue::String(
            "id-123".to_string()
        ))),
        "ResourceRef must resolve from a's post-create state"
    );
}

/// carina#3112 regression: the apply-path `renormalize` (carina#3060)
/// invokes `ProviderNormalizer::normalize_desired` from inside the
/// async apply execution loop, multiple times in sequence (once per
/// resource). The WASM normalizer host impl drives the async guest by
/// `.await`ing a `tokio::sync::Mutex<Store>` lock.
///
/// While the trait was *synchronous*, `WasmProviderNormalizer` bridged
/// sync→async with `block_in_place` + a nested
/// `Handle::current().block_on(async { store.lock().await })`. A
/// `tokio::sync::MutexGuard` from one nested `block_on` was not
/// released before the next nested `block_on` re-acquired the same
/// `Mutex` — a self-deadlock observed deterministically on the second
/// `normalize_desired` of an apply.
///
/// This test models that exact shape with a normalizer that acquires a
/// `tokio::sync::Mutex` *across an `.await`* inside `normalize_desired`,
/// driven through the real `execute_plan` apply path over two
/// resources (so `normalize_desired` runs twice in sequence). It only
/// compiles once `ProviderNormalizer` is async (the impl `.await`s),
/// and it only completes (rather than hanging) once the nested
/// `block_on` is gone — `#[tokio::test(flavor = "current_thread")]`
/// reproduces the single-runtime contention the real apply hits.
#[tokio::test(flavor = "current_thread")]
async fn test_async_normalizer_does_not_self_deadlock_on_apply_path() {
    use tokio::sync::Mutex as AsyncMutex;

    /// Holds an async `Mutex` and acquires it across an `.await` inside
    /// `normalize_desired` — the same lock-across-await shape the WASM
    /// store lock has. A nested `block_on` around this would deadlock
    /// the second time it runs.
    struct LockHoldingNormalizer {
        store: AsyncMutex<u32>,
    }

    impl crate::provider::ProviderNormalizer for LockHoldingNormalizer {
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [Resource],
        ) -> crate::provider::BoxFuture<'a, ()> {
            Box::pin(async move {
                let mut guard = self.store.lock().await;
                *guard += 1;
                let n = *guard;
                drop(guard);
                for r in resources.iter_mut() {
                    r.set_attr(
                        "normalize_count",
                        Value::Concrete(ConcreteValue::String(n.to_string())),
                    );
                }
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
        ) -> crate::provider::BoxFuture<'a, ()> {
            crate::provider::ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> crate::provider::BoxFuture<'a, ()> {
            crate::provider::ready_noop()
        }

        fn merge_default_tags<'a>(
            &'a self,
            _resources: &'a mut [Resource],
            _default_tags: &'a indexmap::IndexMap<String, Value>,
            _registry: &'a crate::schema::SchemaRegistry,
        ) -> crate::provider::BoxFuture<'a, ()> {
            crate::provider::ready_noop()
        }
    }

    let provider = MockProvider::new();
    let ra = make_resource("a", &[]);
    let ra_id = ra.id.clone();
    // `b` carries a ResourceRef to `a`, forcing `resolve_resource` to
    // rebuild attributes — the exact path that runs `renormalize`.
    let rb = make_resource("b", &["a"]);
    let rb_id = rb.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(ra));
    plan.add(create_effect(rb));
    provider.push_create(Ok(ok_state(&ra_id)));
    provider.push_create(Ok(ok_state(&rb_id)));

    let normalizer = LockHoldingNormalizer {
        store: AsyncMutex::new(0),
    };
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &normalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    // This guards the structural property the fix establishes: the
    // apply path drives a lock-across-await normalizer to completion
    // once per resource, in sequence, without re-acquiring the lock
    // before the prior guard dropped. The literal WASM self-deadlock
    // required the old *sync* trait + a nested `block_on` and can only
    // be reproduced with a real WASM guest (the issue's user-driven
    // real-infra smoke); this test cannot even express the old shape
    // because the trait is now async — that is the point.
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(
        result.success_count, 2,
        "both creates must complete — no self-deadlock acquiring the \
         normalizer's async lock a second time"
    );

    // Each resource's `normalize_desired` acquired the lock exactly
    // once and ran in sequence (counts 1 and 2), proving the futures
    // were driven to completion sequentially, not concurrently.
    let counts: std::collections::HashSet<String> = provider
        .captured_create_resources()
        .iter()
        .filter_map(|r| match r.get_attr("normalize_count") {
            Some(Value::Concrete(ConcreteValue::String(s))) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        counts,
        ["1".to_string(), "2".to_string()].into_iter().collect(),
        "normalize_desired must have run once per resource, in sequence"
    );
}

#[tokio::test]
async fn test_simple_delete() {
    let provider = MockProvider::new();
    let rid = ResourceId::with_identity("test", "a");

    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: crate::resource::ResolvedResourceId::new(rid.clone()),
        identifier: "id-123".to_string(),
        directives: Directives::default(),
        binding: None,
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });

    provider.push_delete(Ok(()));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.success_count, 1);
    assert!(result.successfully_deleted.contains(&rid));
}

#[tokio::test]
async fn test_failed_effect_propagates_to_dependent() {
    let provider = MockProvider::new();
    let ra = make_resource("a", &[]);
    let rb = make_resource("b", &["a"]);
    let _rid_a = ra.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(ra));
    plan.add(create_effect(rb));

    // First create fails
    provider.push_create(Err(ProviderError::api_error("create failed")));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.failure_count, 1);
    assert_eq!(result.skip_count, 1);
    assert!(observer.events().iter().any(|e| e.contains("failed:")));
    assert!(
        observer
            .events()
            .iter()
            .any(|e| e.contains("skipped:") && e.contains("dependency 'a' failed"))
    );
}

#[tokio::test]
async fn test_observer_events_emitted_correctly() {
    let provider = MockProvider::new();
    let resource = make_resource("a", &[]);
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(resource));

    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let _ =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    let events = observer.events();
    assert_eq!(events.len(), 2);
    assert!(events[0].starts_with("started:"));
    assert!(events[1].starts_with("succeeded:"));
}

#[tokio::test]
async fn test_read_effect_is_no_op() {
    let provider = MockProvider::new();
    let resource = DataSource::new("test", "data");

    let mut plan = Plan::new();
    plan.add(Effect::Read {
        resource: resolved_data_source(resource),
    });

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 0);
    assert!(provider.calls().is_empty());
}

#[tokio::test]
async fn test_independent_effects_run_in_parallel() {
    // vpc has no deps, subnet_a and subnet_b both depend on vpc.
    // Expected: vpc runs first (level 0), then subnet_a and subnet_b
    // run concurrently (level 1).
    let provider = MockProvider::new();
    let vpc = make_resource("vpc", &[]);
    let subnet_a = make_resource("subnet_a", &["vpc"]);
    let subnet_b = make_resource("subnet_b", &["vpc"]);
    let vpc_id = vpc.id.clone();
    let subnet_a_id = subnet_a.id.clone();
    let subnet_b_id = subnet_b.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(vpc));
    plan.add(create_effect(subnet_a));
    plan.add(create_effect(subnet_b));

    provider.push_create(Ok(ok_state(&vpc_id)));
    provider.push_create(Ok(ok_state(&subnet_a_id)));
    provider.push_create(Ok(ok_state(&subnet_b_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.success_count, 3);
    assert_eq!(result.failure_count, 0);

    // vpc should be created first (level 0), before either subnet
    let calls = provider.calls();
    assert_eq!(calls[0], ("create".to_string(), vpc_id.to_string()));

    // Both subnets should be created (level 1), order may vary
    let remaining: HashSet<String> = calls[1..].iter().map(|(_, id)| id.clone()).collect();
    assert!(remaining.contains(&subnet_a_id.to_string()));
    assert!(remaining.contains(&subnet_b_id.to_string()));
}

#[tokio::test]
async fn test_parallel_failure_skips_dependents() {
    // vpc (level 0), subnet_a depends on vpc, subnet_b depends on vpc.
    // vpc succeeds. subnet_a fails. subnet_c depends on subnet_a => skipped.
    let provider = MockProvider::new();
    let vpc = make_resource("vpc", &[]);
    let subnet_a = make_resource("subnet_a", &["vpc"]);
    let subnet_b = make_resource("subnet_b", &["vpc"]);
    let subnet_c = make_resource("subnet_c", &["subnet_a"]);
    let vpc_id = vpc.id.clone();
    let _subnet_a_id = subnet_a.id.clone();
    let subnet_b_id = subnet_b.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(vpc));
    plan.add(create_effect(subnet_a));
    plan.add(create_effect(subnet_b));
    plan.add(create_effect(subnet_c));

    provider.push_create(Ok(ok_state(&vpc_id)));
    // subnet_a fails, subnet_b succeeds
    provider.push_create(Err(ProviderError::api_error("create failed")));
    provider.push_create(Ok(ok_state(&subnet_b_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    // vpc + subnet_b succeed, subnet_a fails, subnet_c skipped
    assert_eq!(result.success_count, 2);
    assert_eq!(result.failure_count, 1);
    assert_eq!(result.skip_count, 1);

    // Verify subnet_c was skipped due to subnet_a failure
    assert!(
        observer
            .events()
            .iter()
            .any(|e| e.contains("skipped:") && e.contains("dependency 'subnet_a' failed"))
    );
}

#[tokio::test]
async fn test_dependency_levels_sequential_chain() {
    // a -> b -> c: should be 3 levels, executed sequentially
    let provider = MockProvider::new();
    let a = make_resource("a", &[]);
    let b = make_resource("b", &["a"]);
    let c = make_resource("c", &["b"]);
    let a_id = a.id.clone();
    let b_id = b.id.clone();
    let c_id = c.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(a));
    plan.add(create_effect(b));
    plan.add(create_effect(c));

    provider.push_create(Ok(ok_state(&a_id)));
    provider.push_create(Ok(ok_state(&b_id)));
    provider.push_create(Ok(ok_state(&c_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.success_count, 3);

    // Calls should be in order: a, b, c
    let calls = provider.calls();
    assert_eq!(calls[0], ("create".to_string(), a_id.to_string()));
    assert_eq!(calls[1], ("create".to_string(), b_id.to_string()));
    assert_eq!(calls[2], ("create".to_string(), c_id.to_string()));
}

#[test]
fn test_build_dependency_levels() {
    // a (no deps), b depends on a, c depends on a, d depends on b and c
    let a = make_resource("a", &[]);
    let b = make_resource("b", &["a"]);
    let c = make_resource("c", &["a"]);
    let d = make_resource("d", &["b", "c"]);

    let mut plan = Plan::new();
    plan.add(create_effect(a));
    plan.add(create_effect(b));
    plan.add(create_effect(c));
    plan.add(create_effect(d));

    let levels = build_dependency_levels(plan.effects(), &HashMap::new(), &[]);

    // Level 0: a (index 0)
    // Level 1: b (index 1), c (index 2)
    // Level 2: d (index 3)
    assert_eq!(levels.len(), 3);
    assert_eq!(levels[0], vec![0]);
    assert_eq!(levels[1], vec![1, 2]);
    assert_eq!(levels[2], vec![3]);
}

/// Regression test for #1078: route must depend on tgw_attach even when
/// resolve_refs_with_state partially resolves `tgw_attach.transit_gateway_id`
/// to `ResourceRef { binding: "tgw", attr: "id" }`.
///
/// Before the fix, the route and tgw_attach were placed at the same dependency
/// level and executed in parallel, causing an AWS API error.
#[test]
fn test_build_dependency_levels_transitive_ref_preserves_direct_dep() {
    use crate::plan::Plan;

    // Simulate the resources as they appear in the effects after
    // resolve_refs_with_state: ResourceRef values are partially resolved,
    // but _dependency_bindings records the original direct dependencies.

    // tgw_attach depends on tgw, vpc, subnet
    let mut tgw_attach = Resource::new("ec2.transit_gateway_attachment", "tgw_attach");
    tgw_attach.binding = Some("tgw_attach".to_string());
    tgw_attach.dependency_bindings = std::collections::BTreeSet::from([
        "tgw".to_string(),
        "vpc".to_string(),
        "subnet".to_string(),
    ]);

    // route depends on rt and tgw_attach (but after partial resolution,
    // transit_gateway_id points to ResourceRef { binding: "tgw" })
    let mut route = Resource::new("ec2.route", "my-route");
    route.set_attr(
        "transit_gateway_id".to_string(),
        Value::resource_ref("tgw".to_string(), "id".to_string(), vec![]),
    );
    route.dependency_bindings =
        std::collections::BTreeSet::from(["rt".to_string(), "tgw_attach".to_string()]);

    // Other resources
    let mut vpc = Resource::new("ec2.Vpc", "vpc");
    vpc.binding = Some("vpc".to_string());

    let mut tgw = Resource::new("ec2.transit_gateway", "tgw");
    tgw.binding = Some("tgw".to_string());

    let mut subnet = Resource::new("ec2.Subnet", "subnet");
    subnet.binding = Some("subnet".to_string());
    subnet.dependency_bindings = std::collections::BTreeSet::from(["vpc".to_string()]);

    let mut rt = Resource::new("ec2.RouteTable", "rt");
    rt.binding = Some("rt".to_string());
    rt.dependency_bindings = std::collections::BTreeSet::from(["vpc".to_string()]);

    let mut plan = Plan::new();
    plan.add(create_effect(vpc)); // idx 0
    plan.add(create_effect(tgw)); // idx 1
    plan.add(create_effect(subnet)); // idx 2
    plan.add(create_effect(tgw_attach)); // idx 3
    plan.add(create_effect(rt)); // idx 4
    plan.add(create_effect(route)); // idx 5

    let levels = build_dependency_levels(plan.effects(), &HashMap::new(), &[]);

    // Find the level of tgw_attach (idx 3) and route (idx 5)
    let tgw_attach_level = levels.iter().position(|group| group.contains(&3)).unwrap();
    let route_level = levels.iter().position(|group| group.contains(&5)).unwrap();

    assert!(
        route_level > tgw_attach_level,
        "route (level {}) must be at a higher level than tgw_attach (level {}). levels: {:?}",
        route_level,
        tgw_attach_level,
        levels
    );
}

/// Verify fine-grained scheduling: effect C (depends on A) starts before
/// effect B (independent, slow) completes.
///
/// Setup:
///   A (no deps, fast), B (no deps, slow), C (depends on A, fast)
///
/// With level-based execution:
///   Level 0: A and B run concurrently, wait for both.
///   Level 1: C starts after B finishes (~100ms total).
///
/// With fine-grained scheduling:
///   A and B start concurrently. A finishes quickly (~5ms).
///   C starts immediately (A is done), while B is still running.
///   C should start (and finish) before B completes.
#[tokio::test]
async fn test_fine_grained_scheduling_starts_dependent_before_slow_peer_completes() {
    use std::time::Duration;

    // A provider that delays certain resources
    struct DelayedProvider {
        delays: HashMap<String, Duration>,
        call_log: Arc<Mutex<Vec<(String, String, Instant)>>>,
    }

    impl Provider for DelayedProvider {
        fn name(&self) -> &str {
            "delayed"
        }

        fn read(
            &self,
            _id: &ResourceId,
            _identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("not implemented")) })
        }

        fn read_data_source(&self, _resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("not implemented")) })
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::CreateOutcome>> {
            let id_clone = id.clone();
            let name = id.identity_or_empty().to_string();
            let delay = self.delays.get(&name).copied().unwrap_or(Duration::ZERO);
            let log = self.call_log.clone();
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                log.lock()
                    .unwrap()
                    .push(("create".to_string(), name, Instant::now()));
                // Publish `id` so dependents created via
                // `make_resource(name, &["dep"])` resolve their
                // `ResourceRef(parent, "id")` (post-#3032 the executor
                // rejects unresolved refs at the apply seam).
                let mut attrs = HashMap::new();
                attrs.insert(
                    "id".to_string(),
                    Value::Concrete(ConcreteValue::String("id-123".to_string())),
                );
                Ok(crate::provider::CreateOutcome::Success {
                    state: State::existing(id_clone, attrs).with_identifier("id-123"),
                })
            })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("not implemented")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("not implemented")) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: crate::effect::PlanOp,
        ) -> Vec<String> {
            Vec::new()
        }
    }

    let mut delays = HashMap::new();
    delays.insert("a".to_string(), Duration::from_millis(5));
    delays.insert("b".to_string(), Duration::from_millis(200));
    delays.insert("c".to_string(), Duration::from_millis(5));

    let call_log = Arc::new(Mutex::new(Vec::new()));
    let provider = DelayedProvider {
        delays,
        call_log: call_log.clone(),
    };

    let a = make_resource("a", &[]);
    let b = make_resource("b", &[]);
    let c = make_resource("c", &["a"]);

    let mut plan = Plan::new();
    plan.add(create_effect(a));
    plan.add(create_effect(b));
    plan.add(create_effect(c));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.success_count, 3);
    assert_eq!(result.failure_count, 0);

    // Verify C completed before B.
    // With fine-grained scheduling, C starts right after A completes
    // (while B is still sleeping), so C should finish before B.
    let log = call_log.lock().unwrap();
    let c_time = log.iter().find(|(_, name, _)| name == "c").unwrap().2;
    let b_time = log.iter().find(|(_, name, _)| name == "b").unwrap().2;
    assert!(
        c_time < b_time,
        "C should complete before B with fine-grained scheduling. \
         C completed at {:?}, B completed at {:?}",
        c_time,
        b_time,
    );
}

struct DelayedUpdateProvider {
    delay: std::time::Duration,
    change_unrelated_id: bool,
    active: Arc<std::sync::atomic::AtomicUsize>,
    max_active: Arc<std::sync::atomic::AtomicUsize>,
}

impl DelayedUpdateProvider {
    fn new(delay: std::time::Duration) -> Self {
        Self {
            delay,
            change_unrelated_id: false,
            active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn violates_unrelated_id(delay: std::time::Duration) -> Self {
        Self {
            delay,
            change_unrelated_id: true,
            active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn max_active(&self) -> usize {
        self.max_active.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Provider for DelayedUpdateProvider {
    fn name(&self) -> &str {
        "delayed-update"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn read_data_source(&self, _resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::CreateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let delay = self.delay;
        let change_unrelated_id = self.change_unrelated_id;
        let active = self.active.clone();
        let max_active = self.max_active.clone();
        Box::pin(async move {
            let now_active = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            max_active.fetch_max(now_active, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

            let mut attrs = request.from.attributes.clone();
            attrs.insert(
                "tags".to_string(),
                Value::Concrete(ConcreteValue::String("new".to_string())),
            );
            if change_unrelated_id && id.identity_or_empty() == "vpc" {
                attrs.insert(
                    "id".to_string(),
                    Value::Concrete(ConcreteValue::String("provider-violated-id".to_string())),
                );
            }
            Ok(crate::provider::UpdateOutcome::Success {
                state: State::existing(id, attrs).with_identifier(&identifier),
            })
        })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: crate::effect::PlanOp) -> Vec<String> {
        Vec::new()
    }
}

fn tag_update_resource(binding: &str, parent_ref: Option<&str>) -> Resource {
    let mut resource = Resource::new("test", binding);
    resource.binding = Some(binding.to_string());
    resource.set_attr(
        "id",
        Value::Concrete(ConcreteValue::String(format!("{binding}-id"))),
    );
    resource.set_attr(
        "tags",
        Value::Concrete(ConcreteValue::String("new".to_string())),
    );
    if let Some(parent) = parent_ref {
        resource.set_attr("vpc_id", Value::resource_ref(parent, "id", vec![]));
    }
    resource
}

fn tag_update_state(id: &ResourceId, binding: &str) -> State {
    State::existing(
        id.clone(),
        HashMap::from([
            (
                "id".to_string(),
                Value::Concrete(ConcreteValue::String(format!("{binding}-id"))),
            ),
            (
                "tags".to_string(),
                Value::Concrete(ConcreteValue::String("old".to_string())),
            ),
        ]),
    )
    .with_identifier(format!("{binding}-id"))
}

async fn run_tag_sweep(parallelism: NonZeroUsize) -> (std::time::Duration, usize) {
    let mut resources = Vec::new();
    resources.push(tag_update_resource("vpc", None));
    for idx in 0..12 {
        resources.push(tag_update_resource(&format!("child{idx}"), Some("vpc")));
    }

    let mut current_states = HashMap::new();
    let mut plan = Plan::new();
    for resource in &resources {
        let binding = resource.binding.as_deref().unwrap();
        let from = tag_update_state(&resource.id, binding);
        current_states.insert(resource.id.clone(), from.clone());
        plan.add(Effect::Update {
            from: Box::new(from),
            to: resolved(resource.clone()),
            changed_attributes: vec!["tags".to_string()],
        });
    }

    let unresolved_resources: HashMap<ResourceId, UnresolvedResource> = resources
        .iter()
        .map(|resource| {
            (
                resource.id.clone(),
                UnresolvedResource::from_pre_resolve(resource.clone()),
            )
        })
        .collect();
    let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: &resources,
        compositions: &[],
        data_sources: &[],
        current_states: &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let provider = DelayedUpdateProvider::new(std::time::Duration::from_millis(200));
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &unresolved_resources,
        compositions: &[],
        bindings,
        current_states,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism,
    };

    let observer = MockObserver::new();
    let started = Instant::now();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    let elapsed = started.elapsed();

    assert_eq!(result.success_count, 13);
    assert_eq!(result.failure_count, 0);
    (elapsed, provider.max_active())
}

async fn run_provider_contract_case(unknown_read: bool) -> usize {
    let mut parent = tag_update_resource("vpc", None);
    let mut child = tag_update_resource("child", None);
    if unknown_read {
        child.directives.depends_on.push("vpc".to_string());
    } else {
        child.set_attr("vpc_id", Value::resource_ref("vpc", "id", vec![]));
    }
    parent.binding = Some("vpc".to_string());
    child.binding = Some("child".to_string());
    let resources = vec![parent, child];

    let mut current_states = HashMap::new();
    let mut plan = Plan::new();
    for resource in &resources {
        let binding = resource.binding.as_deref().unwrap();
        let from = tag_update_state(&resource.id, binding);
        current_states.insert(resource.id.clone(), from.clone());
        plan.add(Effect::Update {
            from: Box::new(from),
            to: resolved(resource.clone()),
            changed_attributes: vec!["tags".to_string()],
        });
    }

    let unresolved_resources: HashMap<ResourceId, UnresolvedResource> = resources
        .iter()
        .map(|resource| {
            (
                resource.id.clone(),
                UnresolvedResource::from_pre_resolve(resource.clone()),
            )
        })
        .collect();
    let bindings = ResolvedBindings::pre_apply(crate::binding_index::PreApplyInputs {
        managed: &resources,
        compositions: &[],
        data_sources: &[],
        current_states: &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let provider =
        DelayedUpdateProvider::violates_unrelated_id(std::time::Duration::from_millis(100));
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &unresolved_resources,
        compositions: &[],
        bindings,
        current_states,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(2).unwrap(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 2);
    assert_eq!(result.failure_count, 0);
    provider.max_active()
}

#[tokio::test]
async fn provider_contract_violation_does_not_relax_unknown_read_edges() {
    let max_active = run_provider_contract_case(true).await;
    assert_eq!(
        max_active, 1,
        "unknown reads must keep the child update serialized even if the provider mutates unrelated attrs",
    );
}

#[tokio::test]
async fn provider_contract_violation_known_disjoint_edge_still_relaxes_by_static_invariant() {
    let max_active = run_provider_contract_case(false).await;
    assert_eq!(
        max_active, 2,
        "known disjoint reads should relax by the static read/write invariant even under a violating mock provider",
    );
}

#[tokio::test]
async fn test_parallel_update_relaxation_with_cap_eight_finishes_in_two_rounds() {
    let (_elapsed, max_active) = run_tag_sweep(NonZeroUsize::new(8).unwrap()).await;
    assert!(
        max_active <= 8,
        "scheduler must not dispatch more than the cap, max_active={max_active}",
    );
    assert!(
        max_active > 1,
        "relaxed update edges should allow concurrent updates, max_active={max_active}",
    );
}

#[tokio::test]
async fn test_parallelism_one_keeps_update_sweep_serial() {
    let (elapsed, max_active) = run_tag_sweep(NonZeroUsize::new(1).unwrap()).await;

    assert!(
        elapsed >= std::time::Duration::from_millis(2400),
        "cap=1 should serialize thirteen 200ms updates, got {elapsed:?}",
    );
    assert_eq!(max_active, 1);
}

#[tokio::test]
async fn test_waiting_events_emitted_for_dependent_effects() {
    // Setup: A has no deps, C depends on A.
    // C should get a Waiting event before A completes.
    let a = make_resource("a", &[]);
    let c = make_resource("c", &["a"]);

    let mut plan = Plan::new();
    plan.add(create_effect(a));
    plan.add(create_effect(c));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let provider = MockProvider::new();
    // Push create results for both resources
    let a_id = ResourceId::with_identity("test", "a");
    let c_id = ResourceId::with_identity("test", "c");
    // Publish `id` in state.attributes so dependents resolve their
    // `ResourceRef(parent, "id")` refs (post-#3032 the executor
    // rejects unresolved refs at the apply seam).
    let id_attr = |val: &str| -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert(
            "id".to_string(),
            Value::Concrete(ConcreteValue::String(val.to_string())),
        );
        m
    };
    provider.push_create(Ok(
        State::existing(a_id, id_attr("id-a")).with_identifier("id-a")
    ));
    provider.push_create(Ok(
        State::existing(c_id, id_attr("id-c")).with_identifier("id-c")
    ));
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.success_count, 2);

    let events = observer.events.lock().unwrap();
    // C should have a waiting event before it starts
    let waiting_events: Vec<_> = events
        .iter()
        .filter(|e| e.starts_with("waiting:"))
        .collect();
    assert!(
        !waiting_events.is_empty(),
        "Expected at least one waiting event, got events: {:?}",
        *events
    );
    // The waiting event for C should mention dependency "a"
    let c_waiting = waiting_events
        .iter()
        .find(|e| e.contains("test.c"))
        .expect("Expected a waiting event for resource C");
    assert!(
        c_waiting.contains("[a]"),
        "Waiting event should list 'a' as pending dependency, got: {}",
        c_waiting
    );
}

/// Regression test for #1195: Delete effects must respect reverse dependency ordering.
///
/// When deleting resources, children must be deleted before parents.
/// If subnet depends on vpc, the vpc delete must wait for subnet delete.
/// Before the fix, dependency analysis returned empty deps for deletes,
/// allowing parent and child deletes to run concurrently.
#[test]
fn test_build_dependency_levels_respects_delete_dependencies() {
    // Scenario: vpc (no deps), subnet (depends on vpc)
    // For creation: subnet depends on vpc → vpc first, then subnet
    // For deletion: vpc delete must wait for subnet delete → subnet first, then vpc
    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity("ec2.Vpc", "vpc")),
        identifier: "vpc-123".to_string(),
        directives: Directives::default(),
        binding: Some("vpc".to_string()),
        dependencies: HashSet::new(), // vpc has no deps
        explicit_dependencies: HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });
    plan.add(Effect::Delete {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Subnet",
            "subnet",
        )),
        identifier: "subnet-456".to_string(),
        directives: Directives::default(),
        binding: Some("subnet".to_string()),
        dependencies: HashSet::from(["vpc".to_string()]), // subnet depends on vpc
        explicit_dependencies: HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });

    let levels = build_dependency_levels(plan.effects(), &HashMap::new(), &[]);

    // Find levels for each effect
    let vpc_level = levels.iter().position(|group| group.contains(&0)).unwrap();
    let subnet_level = levels.iter().position(|group| group.contains(&1)).unwrap();

    // vpc delete (idx 0) must be at a HIGHER level than subnet delete (idx 1)
    // because vpc must wait for subnet to be deleted first (reverse ordering)
    assert!(
        vpc_level > subnet_level,
        "vpc delete (level {}) must be at a higher level than subnet delete (level {}). \
         Delete ordering must be reversed: children deleted before parents. levels: {:?}",
        vpc_level,
        subnet_level,
        levels
    );
}

/// Characterization test for #1306: build_dependency_levels and dependency analysis
/// must produce consistent results. This test verifies that after refactoring
/// build_dependency_levels to reuse the same dependency analysis, the level assignments
/// remain the same.
#[test]
fn test_build_dependency_levels_consistent_with_dependency_map() {
    // a (no deps), b depends on a, c depends on a, d depends on b and c
    let a = make_resource("a", &[]);
    let b = make_resource("b", &["a"]);
    let c = make_resource("c", &["a"]);
    let d = make_resource("d", &["b", "c"]);

    let mut plan = Plan::new();
    plan.add(create_effect(a));
    plan.add(create_effect(b));
    plan.add(create_effect(c));
    plan.add(create_effect(d));

    let levels = build_dependency_levels(plan.effects(), &HashMap::new(), &[]);
    let dep_map =
        build_dependency_analysis(plan.effects(), &HashMap::new(), &[], ScheduleInputs::Apply)
            .into_deps_of();

    // Verify levels are consistent with the dependency map:
    // For every effect, its level must be greater than all its dependencies' levels.
    for (idx, deps) in &dep_map {
        let idx_level = levels.iter().position(|group| group.contains(idx)).unwrap();
        for dep in deps {
            let dep_level = levels.iter().position(|group| group.contains(dep)).unwrap();
            assert!(
                idx_level > dep_level,
                "Effect {} (level {}) must be at a higher level than dependency {} (level {})",
                idx,
                idx_level,
                dep,
                dep_level
            );
        }
    }

    // Verify the same structure as the existing test
    assert_eq!(levels.len(), 3);
    assert_eq!(levels[0], vec![0]);
    assert_eq!(levels[1], vec![1, 2]);
    assert_eq!(levels[2], vec![3]);
}

/// Characterization test for #1306: the executor must propagate binding
/// maps after an update effect.
#[tokio::test]
async fn test_update_effect_binding_map_propagation() {
    let provider = MockProvider::new();
    let ra_id = ResourceId::with_identity("test", "a");

    // Create initial state
    let from_state = State::existing(ra_id.clone(), HashMap::new()).with_identifier("id-original");
    let to_resource = make_resource("a", &[]);

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["some_attr".to_string()],
    });

    let updated_state =
        State::existing(ra_id.clone(), HashMap::new()).with_identifier("id-updated");
    provider.push_update(Ok(updated_state));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 0);
    assert!(result.applied_states.contains_key(&ra_id));

    let events = observer.events();
    assert!(events.iter().any(|e| e.starts_with("started:")));
    assert!(events.iter().any(|e| e.starts_with("succeeded:")));
}

/// Regression test for #1195: dependency analysis also respects delete dependencies.
#[test]
fn test_dependency_analysis_respects_delete_dependencies() {
    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity("ec2.Vpc", "vpc")),
        identifier: "vpc-123".to_string(),
        directives: Directives::default(),
        binding: Some("vpc".to_string()),
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });
    plan.add(Effect::Delete {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Subnet",
            "subnet",
        )),
        identifier: "subnet-456".to_string(),
        directives: Directives::default(),
        binding: Some("subnet".to_string()),
        dependencies: HashSet::from(["vpc".to_string()]),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });

    let deps =
        build_dependency_analysis(plan.effects(), &HashMap::new(), &[], ScheduleInputs::Apply)
            .into_deps_of();

    // vpc delete (idx 0) must depend on subnet delete (idx 1)
    // because subnet must be deleted before vpc (reverse dependency)
    assert!(
        deps[&0].contains(&1),
        "vpc delete should depend on subnet delete (reverse ordering). deps: {:?}",
        deps
    );
    // subnet delete (idx 1) should NOT depend on vpc delete (idx 0)
    assert!(
        !deps[&1].contains(&0),
        "subnet delete should not depend on vpc delete. deps: {:?}",
        deps
    );
}

/// Test that ResourceRef values in dependent resources are resolved using
/// state attributes from predecessor resources (binding_map propagation).
#[tokio::test]
async fn test_resource_ref_resolved_from_predecessor_state() {
    let provider = RecordingMockProvider::new();

    // VPC resource with binding "vpc"
    let mut vpc = Resource::new("test", "vpc");
    vpc.binding = Some("vpc".to_string());
    vpc.set_attr(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    let vpc_id = vpc.id.clone();

    // Subnet resource that references vpc.vpc_id
    let mut subnet = Resource::new("test", "subnet");
    subnet.set_attr(
        "vpc_id",
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    subnet.set_attr(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
    );
    subnet.dependency_bindings = std::collections::BTreeSet::from(["vpc".to_string()]);
    let subnet_id = subnet.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(vpc));
    plan.add(create_effect(subnet));

    // VPC create returns state with vpc_id
    let vpc_state = State::existing(
        vpc_id.clone(),
        vec![(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-12345".to_string())),
        )]
        .into_iter()
        .collect(),
    )
    .with_identifier("vpc-12345");
    provider.push_create(Ok(vpc_state));

    // Subnet create returns state
    let subnet_state = State::existing(
        subnet_id.clone(),
        vec![(
            "subnet_id".to_string(),
            Value::Concrete(ConcreteValue::String("subnet-67890".to_string())),
        )]
        .into_iter()
        .collect(),
    )
    .with_identifier("subnet-67890");
    provider.push_create(Ok(subnet_state));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.success_count, 2, "Both resources should succeed");
    assert_eq!(result.failure_count, 0, "No failures expected");

    // Check that the subnet received vpc_id = "vpc-12345" (resolved from state)
    let create_calls = provider.create_calls();
    assert_eq!(create_calls.len(), 2, "Should have 2 create calls");

    // First call should be VPC
    assert_eq!(create_calls[0].0, vpc_id.to_string());

    // Second call should be subnet with resolved vpc_id
    assert_eq!(create_calls[1].0, subnet_id.to_string());
    let subnet_attrs = &create_calls[1].1;
    assert_eq!(
        subnet_attrs.get("vpc_id"),
        Some(&Value::Concrete(ConcreteValue::String(
            "vpc-12345".to_string()
        ))),
        "Subnet's vpc_id should be resolved from VPC state, got: {:?}",
        subnet_attrs.get("vpc_id")
    );
}

/// A mock provider that records the resource attributes passed to create().
type CreateLog = Vec<(String, HashMap<String, Value>)>;

struct RecordingMockProvider {
    create_results: Mutex<Vec<ProviderResult<crate::provider::CreateOutcome>>>,
    /// Records: (resource_id_string, resolved_attributes)
    create_log: Arc<Mutex<CreateLog>>,
}

impl RecordingMockProvider {
    fn new() -> Self {
        Self {
            create_results: Mutex::new(Vec::new()),
            create_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn push_create(&self, result: ProviderResult<State>) {
        self.create_results
            .lock()
            .unwrap()
            .push(result.map(|state| crate::provider::CreateOutcome::Success { state }));
    }

    fn create_calls(&self) -> Vec<(String, HashMap<String, Value>)> {
        self.create_log.lock().unwrap().clone()
    }
}

impl Provider for RecordingMockProvider {
    fn name(&self) -> &str {
        "recording_mock"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn read_data_source(&self, _resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::CreateOutcome>> {
        let id_str = id.to_string();
        let attrs = request.resource.as_resource().resolved_attributes();
        self.create_log.lock().unwrap().push((id_str, attrs));
        let result = self.create_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: crate::effect::PlanOp) -> Vec<String> {
        Vec::new()
    }
}

#[tokio::test]
async fn test_wait_effect_polls_then_unblocks_downstream() {
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let provider = MockProvider::new();

    // Plan: Create cert → Wait cert_issued (target = cert) → Create dist
    let cert = make_resource("cert", &[]);
    let cert_id = cert.id.clone();
    let mut dist = make_resource("dist", &[]);
    // `dist` references the wait binding so the scheduler links it.
    dist.set_attr(
        "ref_cert_issued".to_string(),
        Value::resource_ref("cert_issued".to_string(), "arn".to_string(), vec![]),
    );
    dist.dependency_bindings = ["cert_issued".to_string()].into_iter().collect();
    let dist_id = dist.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: ResourceIdentity::new("cert_issued"),
        target_id: crate::resource::ResolvedResourceId::new(cert_id.clone()),
        until: WaitPredicate::Equals {
            attr: AttrPath::single("status"),
            value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        until_surface: "cert.status == ISSUED".to_string(),
        timeout: std::time::Duration::from_secs(60),
        interval: std::time::Duration::from_millis(1),
        explicit_dependencies: std::collections::HashSet::new(),
    });
    plan.add(create_effect(dist));

    // create cert → state with status PENDING (the Create result; the
    // wait polls via read for ISSUED).
    let mut create_attrs = HashMap::new();
    create_attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".to_string())),
    );
    provider.push_create(Ok(
        State::existing(cert_id.clone(), create_attrs).with_identifier("acm-cert-id")
    ));
    // wait reads: PENDING → PENDING → ISSUED
    let mut pending = HashMap::new();
    pending.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".to_string())),
    );
    let mut issued = HashMap::new();
    issued.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
    );
    issued.insert(
        "arn".to_string(),
        Value::Concrete(ConcreteValue::String("arn:aws:acm:...".to_string())),
    );
    provider.push_read(Ok(State::existing(cert_id.clone(), pending.clone())));
    provider.push_read(Ok(State::existing(cert_id.clone(), pending)));
    provider.push_read(Ok(State::existing(cert_id.clone(), issued)));
    // create dist → succeeds
    provider.push_create(Ok(ok_state(&dist_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(
        result.success_count,
        3,
        "expected 3 successful effects (cert create + wait + dist create), got {} (events: {:?})",
        result.success_count,
        observer.events()
    );
    assert_eq!(result.failure_count, 0);

    let calls = provider.calls();
    assert_eq!(calls[0], ("create".to_string(), cert_id.to_string()));
    // Three reads from the wait polling loop.
    assert_eq!(calls[1], ("read".to_string(), cert_id.to_string()));
    assert_eq!(calls[2], ("read".to_string(), cert_id.to_string()));
    assert_eq!(calls[3], ("read".to_string(), cert_id.to_string()));
    // dist create must follow the wait.
    assert_eq!(calls[4], ("create".to_string(), dist_id.to_string()));
}

/// carina#3061 — a downstream resource that references
/// `<wait_binding>.<attr>` **nested inside a Map attribute** (the real
/// `awscc.cloudfront.Distribution` shape:
/// `distribution_config.viewer_certificate.acm_certificate_arn =
/// cert_issued.certificate_arn`) must resolve to the wait target's
/// post-`until` attribute value at apply time.
///
/// The regression: `dependency_bindings` is populated by the real
/// resolver helper (`get_resource_value_ref_dependencies`), exactly as
/// the apply pipeline does — *not* hand-set as in
/// `test_wait_effect_polls_then_unblocks_downstream`. If the nested-map
/// `ResourceRef` to the wait binding does not produce a scheduler edge
/// to the `Effect::Wait`, the Distribution is dispatched before the
/// wait records `cert_issued`'s attributes and `assert_fully_resolved`
/// rejects the still-`Deferred` ref with the self-contradicting
/// "add a `wait` block" error.
#[tokio::test]
async fn test_wait_downstream_nested_map_ref_resolves_at_apply() {
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let provider = MockProvider::new();

    let cert = make_resource("cert", &[]);
    let cert_id = cert.id.clone();

    // `dist` references the wait binding from *inside a nested Map*,
    // mirroring `viewer_certificate = { acm_certificate_arn =
    // cert_issued.certificate_arn }`.
    let mut dist = Resource::new("test", "dist");
    dist.binding = Some("dist".to_string());
    let mut viewer_certificate = indexmap::IndexMap::new();
    viewer_certificate.insert(
        "acm_certificate_arn".to_string(),
        Value::resource_ref(
            "cert_issued".to_string(),
            "certificate_arn".to_string(),
            vec![],
        ),
    );
    let mut distribution_config = indexmap::IndexMap::new();
    distribution_config.insert(
        "viewer_certificate".to_string(),
        Value::Concrete(ConcreteValue::Map(viewer_certificate)),
    );
    dist.set_attr(
        "distribution_config".to_string(),
        Value::Concrete(ConcreteValue::Map(distribution_config)),
    );
    // Populate dependency_bindings the way the real apply pipeline does
    // (resolver.rs:70 -> get_resource_value_ref_dependencies), instead
    // of hand-setting the set. This is the load-bearing difference from
    // the existing flat-ref test.
    dist.dependency_bindings = crate::deps::get_resource_value_ref_dependencies(
        crate::parser::ResourceRef::Resource(&dist),
    )
    .into_iter()
    .collect();
    let dist_id = dist.id.clone();

    // Sanity: the resolver helper must have recovered the wait binding
    // from the *nested* ref. If this fails the scheduler can never link
    // the Distribution to the wait.
    assert!(
        dist.dependency_bindings.contains("cert_issued"),
        "get_resource_value_ref_dependencies must recover the nested \
         `cert_issued` ref; got {:?}",
        dist.dependency_bindings
    );

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: ResourceIdentity::new("cert_issued"),
        target_id: crate::resource::ResolvedResourceId::new(cert_id.clone()),
        until: WaitPredicate::Equals {
            attr: AttrPath::single("status"),
            value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        until_surface: "cert.status == ISSUED".to_string(),
        timeout: std::time::Duration::from_secs(60),
        interval: std::time::Duration::from_millis(1),
        explicit_dependencies: std::collections::HashSet::new(),
    });
    plan.add(create_effect(dist));

    // create cert → PENDING; wait polls read → PENDING → ISSUED+arn.
    let mut create_attrs = HashMap::new();
    create_attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".to_string())),
    );
    provider.push_create(Ok(
        State::existing(cert_id.clone(), create_attrs).with_identifier("acm-cert-id")
    ));
    let mut pending = HashMap::new();
    pending.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".to_string())),
    );
    let mut issued = HashMap::new();
    issued.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
    );
    issued.insert(
        "certificate_arn".to_string(),
        Value::Concrete(ConcreteValue::String(
            "arn:aws:acm:us-east-1:111:certificate/abc".to_string(),
        )),
    );
    provider.push_read(Ok(State::existing(cert_id.clone(), pending)));
    provider.push_read(Ok(State::existing(cert_id.clone(), issued)));
    provider.push_create(Ok(ok_state(&dist_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(
        result.failure_count,
        0,
        "no effect should fail; events: {:?}",
        observer.events()
    );
    assert_eq!(
        result.success_count,
        3,
        "cert create + wait + dist create must all succeed; events: {:?}",
        observer.events()
    );

    // The Distribution's create must have run after the wait and seen
    // the resolved `certificate_arn` nested in the Map.
    let calls = provider.calls();
    let dist_create_pos = calls
        .iter()
        .position(|(op, id)| op == "create" && id == &dist_id.to_string())
        .expect("dist create must have happened");
    let last_read_pos = calls
        .iter()
        .rposition(|(op, _)| op == "read")
        .expect("wait must have polled via read");
    assert!(
        dist_create_pos > last_read_pos,
        "dist create ({dist_create_pos}) must follow the wait's last \
         poll ({last_read_pos}); calls: {calls:?}"
    );
}

#[tokio::test]
async fn test_wait_state_writeback_skips_synthetic_wait_id() {
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let provider = MockProvider::new();
    let cert = make_resource("cert", &[]);
    let cert_id = cert.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: ResourceIdentity::new("cert_issued"),
        target_id: crate::resource::ResolvedResourceId::new(cert_id.clone()),
        until: WaitPredicate::Equals {
            attr: AttrPath::single("status"),
            value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        until_surface: "cert.status == ISSUED".to_string(),
        timeout: std::time::Duration::from_secs(60),
        interval: std::time::Duration::from_millis(1),
        explicit_dependencies: std::collections::HashSet::new(),
    });

    let mut issued = HashMap::new();
    issued.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
    );
    provider.push_create(Ok(
        State::existing(cert_id.clone(), issued.clone()).with_identifier("acm-cert-id")
    ));
    provider.push_read(Ok(State::existing(cert_id.clone(), issued)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(result.success_count, 2);
    // The wait's captured State is keyed under a synthetic `__wait`
    // ResourceId. This is what guarantees state writeback never sees
    // it as a real resource — `sorted_resources` (the writeback input)
    // does not contain `__wait` IDs.
    let synthetic = ResourceId::with_identity("__wait", "cert_issued");
    assert!(
        result.applied_states.contains_key(&synthetic),
        "wait should register its captured State under the __wait synthetic id"
    );
}

/// carina#3032 — when a chained `[idx].field` access cannot be
/// resolved at apply time (because the upstream resource has not
/// published the referenced attribute yet — e.g. ACM
/// `domain_validation_options` is populated asynchronously after
/// RequestCertificate), the executor must fail with an actionable
/// error that names the unresolved reference, **not** silently pass
/// the literal `ResourceRef` to the provider where it surfaces as
/// a generic "cannot serialize at WASM provider boundary" error.
///
/// Pre-fix: `resolve_ref_value` bails out on the missing
/// `domain_validation_options` key (resolver.rs:254 catch-all),
/// returns the original `ResourceRef` unchanged, the dependent's
/// `resource_records` reaches `Provider::create()` as
/// `Value::Concrete(List([Value::Deferred(ResourceRef { … })]))`,
/// and the WASM serializer's `core_to_wit_value` rejects it with
/// the unhelpful contract message.
///
/// Post-fix: the executor's `resolve_resource` rejects any value
/// still containing a `ResourceRef` / `BindingRef` after resolution,
/// with an error that points at the unresolved attribute path and
/// suggests using `wait` to synchronize on the upstream attribute.
#[tokio::test]
async fn test_chained_index_then_field_unresolved_at_apply_fails_with_clear_error() {
    use crate::resource::{AccessPath, ConcreteValue, PathSegment, Subscript};

    let provider = MockProvider::new();

    // The cert resource — no DSL attrs that reference DVO; the
    // attribute would be populated only by the create's read-back
    // state. Mirror the real ACM Certificate's user-facing shape.
    let cert = {
        let mut r = Resource::new("test", "cert");
        r.binding = Some("cert".to_string());
        r.set_attr(
            "domain_name",
            Value::Concrete(ConcreteValue::String("example.com".to_string())),
        );
        r
    };
    let cert_id = cert.id.clone();

    // The dependent resource mirrors the failing route53 RecordSet
    // attributes from the issue:
    //   resource_records = [cert.domain_validation_options[0].resource_record_value]
    let record = {
        let mut r = Resource::new("test", "record");
        r.binding = Some("record".to_string());
        r.dependency_bindings = ["cert".to_string()].into_iter().collect();
        let value_path = AccessPath::with_segments(
            "cert",
            "domain_validation_options",
            vec![
                PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                },
                PathSegment::Field {
                    name: "resource_record_value".to_string(),
                },
            ],
        );
        r.set_attr(
            "resource_records",
            Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
                DeferredValue::ResourceRef { path: value_path },
            )])),
        );
        r
    };
    let record_id = record.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(create_effect(record));

    // Mirror the AWS RequestCertificate read-back race: the DVO list
    // is populated asynchronously by ACM after RequestCertificate
    // returns, so the create read-back surfaces zero DVO entries
    // and the AWS provider's `read_acm_certificate` *omits* the
    // `domain_validation_options` key entirely
    // (carina-provider-aws::services::acm::certificate.rs:210
    // `if !dvs.is_empty()`).
    provider.push_create(Ok(
        State::existing(cert_id.clone(), HashMap::new()).with_identifier("acm-cert-id")
    ));
    // Reserve a create slot for the record in case the executor
    // attempts it before failing — pre-fix it would have, and the
    // mock would otherwise panic-on-empty-queue masking the actual
    // bug.
    provider.push_create(Ok(
        State::existing(record_id.clone(), HashMap::new()).with_identifier("rrset-id")
    ));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    // Cert succeeds; the record fails at apply-time resolution
    // *before* reaching the provider — no `create` call for the
    // record should be recorded.
    assert_eq!(result.success_count, 1, "events: {:?}", observer.events());
    assert_eq!(result.failure_count, 1, "events: {:?}", observer.events());

    let captured = provider.captured_create_resources();
    assert!(
        captured.iter().all(|r| r.id != record_id),
        "record resource must NOT be passed to create (resolution \
         should fail upstream); captured: {:?}",
        captured.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
    );

    // The error message must name the unresolved reference path so
    // the user can fix it (typically by adding a `wait` block on the
    // upstream attribute).
    let failed_event = observer
        .events()
        .iter()
        .find(|e| e.starts_with("failed:") && e.contains("record"))
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "expected a `failed:` event for the record resource; \
                 got events: {:?}",
                observer.events()
            )
        });
    assert!(
        failed_event.contains("cert.domain_validation_options"),
        "error must name the unresolved attribute path so the user \
         knows what to wait on; got: {failed_event}",
    );
    assert!(
        failed_event.contains("wait"),
        "error must suggest `wait` as the synchronization mechanism; \
         got: {failed_event}",
    );
}

/// Regression for carina#3046.
///
/// Companion to `test_chained_index_then_field_unresolved_at_apply_fails_with_clear_error`
/// above: when the upstream's post-create state *does* publish the
/// chained-access attribute (the AWS ACM case where the provider's
/// `read_acm_certificate` returns `domain_validation_options` populated),
/// the downstream's chained reference
/// `cert.domain_validation_options[0].resource_record.name` must
/// resolve into a concrete value before the downstream's `create()`
/// is invoked. The provider must see a fully-resolved literal, not a
/// `Value::Deferred(ResourceRef)`.
///
/// Pre-fix (the bug this issue captures) the executor errored out
/// with the "has not been published yet" message even though the
/// value was structurally present in the upstream's binding map.
#[tokio::test]
async fn test_chained_index_then_nested_field_resolves_from_post_create_state() {
    use crate::resource::{AccessPath, ConcreteValue, PathSegment, Subscript};
    use indexmap::IndexMap;

    let provider = RecordingMockProvider::new();

    // Upstream: ACM Certificate. No DSL attrs that mention DVO; the
    // attribute appears only via the create's post-read state, exactly
    // as `carina-provider-aws::services::acm::certificate.rs::read_acm_certificate`
    // inserts it.
    let cert = {
        let mut r = Resource::new("test", "cert");
        r.binding = Some("cert".to_string());
        r.set_attr(
            "domain_name",
            Value::Concrete(ConcreteValue::String("example.com".to_string())),
        );
        r
    };
    let cert_id = cert.id.clone();

    // Downstream: route53 RecordSet referencing the cert's
    // chained-access path. Uses the post-aws#295 *nested* shape:
    // `resource_record` is a struct with `name`/`type`/`value`.
    let record = {
        let mut r = Resource::new("test", "record");
        r.binding = Some("record".to_string());
        r.dependency_bindings = ["cert".to_string()].into_iter().collect();
        let chained_dvo = |leaf: &str| {
            AccessPath::with_segments(
                "cert",
                "domain_validation_options",
                vec![
                    PathSegment::Subscript {
                        index: Subscript::Int { index: 0 },
                    },
                    PathSegment::Field {
                        name: "resource_record".to_string(),
                    },
                    PathSegment::Field {
                        name: leaf.to_string(),
                    },
                ],
            )
        };
        let name_path = chained_dvo("name");
        let value_path = chained_dvo("value");
        r.set_attr(
            "name",
            Value::Deferred(DeferredValue::ResourceRef { path: name_path }),
        );
        r.set_attr(
            "resource_records",
            Value::Concrete(ConcreteValue::List(vec![Value::Deferred(
                DeferredValue::ResourceRef { path: value_path },
            )])),
        );
        r
    };
    let record_id = record.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(create_effect(record));

    // Cert create returns post-read state with DVO populated. Shape
    // mirrors what `read_acm_certificate` inserts after aws#295.
    let mut rr: IndexMap<String, Value> = IndexMap::new();
    rr.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("_abc.example.com.".to_string())),
    );
    rr.insert(
        "type".to_string(),
        Value::Concrete(ConcreteValue::String("CNAME".to_string())),
    );
    rr.insert(
        "value".to_string(),
        Value::Concrete(ConcreteValue::String(
            "_xyz.acm-validations.aws.".to_string(),
        )),
    );
    let mut dvo_entry: IndexMap<String, Value> = IndexMap::new();
    dvo_entry.insert(
        "domain_name".to_string(),
        Value::Concrete(ConcreteValue::String("example.com".to_string())),
    );
    dvo_entry.insert(
        "resource_record".to_string(),
        Value::Concrete(ConcreteValue::Map(rr)),
    );
    let cert_state = State::existing(
        cert_id.clone(),
        vec![(
            "domain_validation_options".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(dvo_entry),
            )])),
        )]
        .into_iter()
        .collect(),
    )
    .with_identifier("acm-cert-id");
    provider.push_create(Ok(cert_state));

    let record_state =
        State::existing(record_id.clone(), HashMap::new()).with_identifier("rrset-id");
    provider.push_create(Ok(record_state));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(
        result.failure_count,
        0,
        "no failures expected; events: {:?}",
        observer.events()
    );
    assert_eq!(
        result.success_count,
        2,
        "both cert and record must succeed; events: {:?}",
        observer.events()
    );

    // The downstream `create()` call must have received concrete
    // values resolved from the upstream's post-create state, not the
    // original `Value::Deferred(ResourceRef)`.
    let calls = provider.create_calls();
    assert_eq!(calls.len(), 2, "expected 2 create calls");
    assert_eq!(
        calls[0].0,
        cert_id.to_string(),
        "cert must be created before record (dependency order)",
    );
    let (record_call_id, record_attrs) = &calls[1];
    assert_eq!(record_call_id, &record_id.to_string());

    assert_eq!(
        record_attrs.get("name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "_abc.example.com.".to_string()
        ))),
        "record's `name` must resolve from chained access; got: {:?}",
        record_attrs.get("name"),
    );

    let resource_records = record_attrs
        .get("resource_records")
        .expect("record must carry `resource_records` attribute");
    assert_eq!(
        resource_records,
        &Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("_xyz.acm-validations.aws.".to_string())
        )])),
        "`resource_records` list element must resolve from chained \
         access into the post-create state; got: {resource_records:?}",
    );
}

// -----------------------------------------------------------------------
// carina#3119: wait target identifier must be resolved at apply time from
// the just-created resource's state, not the plan-time value.
// -----------------------------------------------------------------------

/// Provider whose `read` only succeeds when handed the *correct* created
/// identifier; with `None` (or a wrong identifier) it returns not-found,
/// exactly like the real AWS ACM provider. It records every identifier
/// passed to `read` so the test can assert what the apply path threaded
/// through.
struct IdentifierAwareProvider {
    expected_identifier: String,
    /// State returned by `create` (carries the real identifier + the
    /// attribute the wait predicate checks).
    created_state: Mutex<Option<State>>,
    read_identifiers: Arc<Mutex<Vec<Option<String>>>>,
}

impl IdentifierAwareProvider {
    fn new(expected_identifier: &str, created_state: State) -> Self {
        Self {
            expected_identifier: expected_identifier.to_string(),
            created_state: Mutex::new(Some(created_state)),
            read_identifiers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn read_identifiers(&self) -> Vec<Option<String>> {
        self.read_identifiers.lock().unwrap().clone()
    }
}

impl Provider for IdentifierAwareProvider {
    fn name(&self) -> &str {
        "identifier-aware"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let owned = identifier.map(|s| s.to_string());
        self.read_identifiers.lock().unwrap().push(owned.clone());
        let id = id.clone();
        let expected = self.expected_identifier.clone();
        let state = self.created_state.lock().unwrap().clone();
        Box::pin(async move {
            match owned {
                Some(ref got) if got == &expected => {
                    state.ok_or_else(|| ProviderError::api_error("no canned state for read"))
                }
                _ => Err(ProviderError::not_found(format!(
                    "wait target {id} not found (deleted out-of-band?)"
                ))
                .for_resource(id)),
            }
        })
    }

    fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
        self.read(&resource.id, None, ReadRequest)
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::CreateOutcome>> {
        let state = self.created_state.lock().unwrap().clone();
        Box::pin(async move {
            state
                .map(|state| crate::provider::CreateOutcome::Success { state })
                .ok_or_else(|| ProviderError::api_error("no canned create state"))
        })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        Box::pin(async move { Err(ProviderError::api_error("update not expected")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn required_permissions(&self, _id: &ResourceId, _op: crate::effect::PlanOp) -> Vec<String> {
        Vec::new()
    }
}

/// Regression for carina#3119: a resource created *in the same apply run*
/// must be polled with the identifier from the just-completed Create's state
/// (held in `applied_states`), not poll `provider.read` with no
/// identifier.
///
/// This exercises the real apply path (`execute_plan`). The pre-existing
/// wait unit tests in `wait.rs` use a provider that ignores the
/// identifier, so they never caught this.
#[tokio::test]
async fn wait_resolves_target_identifier_from_just_created_state() {
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let mut cert = Resource::new("test", "cert");
    cert.binding = Some("cert".to_string());
    let cert_id = cert.id.clone();

    // Post-create state: the provider hands back the real identifier
    // (unknown at plan time) plus the attribute the wait predicate reads.
    let mut created_attrs = HashMap::new();
    created_attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("issued".to_string())),
    );
    let created_state =
        State::existing(cert_id.clone(), created_attrs).with_identifier("cert-arn-real");

    let provider = IdentifierAwareProvider::new("cert-arn-real", created_state);

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: ResourceIdentity::new("cert_issued"),
        target_id: crate::resource::ResolvedResourceId::new(cert_id.clone()),
        until: WaitPredicate::Equals {
            attr: AttrPath::single("status"),
            value: Value::Concrete(ConcreteValue::String("issued".to_string())),
        },
        until_surface: "cert.status == \"issued\"".to_string(),
        timeout: std::time::Duration::from_secs(5),
        interval: std::time::Duration::from_millis(10),
        explicit_dependencies: std::collections::HashSet::new(),
    });

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(
        result.failure_count,
        0,
        "wait must not fail; the just-created identifier should reach \
         provider.read. read identifiers seen: {:?}",
        provider.read_identifiers()
    );
    assert_eq!(result.success_count, 2, "both Create and Wait must succeed");
    assert!(
        provider
            .read_identifiers()
            .iter()
            .any(|i| i.as_deref() == Some("cert-arn-real")),
        "the wait read must be called with the created identifier \
         resolved from applied_states, not the plan-time None; got: {:?}",
        provider.read_identifiers()
    );
}

#[tokio::test]
async fn deferred_create_returns_error_when_upstream_binding_missing() {
    let provider = MockProvider::new();
    let mut plan = Plan::new();
    plan.add(Effect::DeferredCreate {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        upstream_binding: "missing_cert".to_string(),
        template: Box::new(validation_deferred_for_expression()),
    });

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };
    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.failure_count, 1);
    assert!(observer.events().iter().any(|event| {
        event.contains("failed:__deferred_for.validation_records") && event.contains("missing_cert")
    }));
}

#[tokio::test]
async fn deferred_create_returns_error_when_iterable_attr_missing() {
    let mut cert = Resource::new("test", "cert_missing_attr");
    cert.binding = Some("cert".to_string());
    let cert_id = cert.id.clone();

    let provider = MockProvider::new();
    provider.push_create(Ok(State::existing(cert_id.clone(), HashMap::new())));

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::DeferredCreate {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        upstream_binding: "cert".to_string(),
        template: Box::new(validation_deferred_for_expression()),
    });

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
    };
    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.failure_count, 1);
    assert!(observer.events().iter().any(|event| {
        event.contains("failed:__deferred_for.validation_records")
            && event.contains("domain_validation_options")
    }));
}

#[tokio::test]
async fn apply_time_deferred_create_emits_failed_on_shape_mismatch() {
    let mut cert = Resource::new("test", "cert_shape_mismatch");
    cert.binding = Some("cert".to_string());
    let cert_id = cert.id.clone();
    let cert_state = State::existing(
        cert_id.clone(),
        HashMap::from([(
            "domain_validation_options".to_string(),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::new())),
        )]),
    );

    let provider = MockProvider::new();
    provider.push_create(Ok(cert_state));

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::DeferredCreate {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        upstream_binding: "cert".to_string(),
        template: Box::new(validation_deferred_for_expression()),
    });

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
    };
    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.failure_count, 1);
    assert!(observer.events().iter().any(|event| {
        event.contains("failed:__deferred_for.validation_records")
            && event.contains("expected list")
            && event.contains("got map")
    }));
}

#[tokio::test]
async fn dispatch_deferred_replace_orders_matching_delete_after_materialized_create() {
    let cert_id = ResourceId::with_identity("test", "cert");
    let cert_state = State::existing(
        cert_id.clone(),
        HashMap::from([(
            "domain_validation_options".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(indexmap::IndexMap::from([(
                    "resource_record".to_string(),
                    Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
                        (
                            "name".to_string(),
                            Value::Concrete(ConcreteValue::String("_name".to_string())),
                        ),
                        (
                            "value".to_string(),
                            Value::Concrete(ConcreteValue::String("_value".to_string())),
                        ),
                    ]))),
                )])),
            )])),
        )]),
    );
    let validation_id = ResourceId::with_identity("test", "validation_records[0]");
    let validation_state =
        State::existing(validation_id.clone(), HashMap::new()).with_identifier("new-validation");
    let provider = MockProvider::new();
    provider.push_create(Ok(cert_state));
    provider.push_create(Ok(validation_state));
    provider.push_delete(Ok(()));
    provider.push_delete(Ok(()));

    let mut cert = Resource::new("test", "cert");
    cert.binding = Some("cert".to_string());
    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(vec![
            DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "test",
                    "validation_records[0]",
                )),
                identifier: "old-validation-0".to_string(),
                directives: Directives::default(),
                binding: Some("validation_records[0]".to_string()),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            },
            DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "test",
                    "validation_records[1]",
                )),
                identifier: "old-validation-1".to_string(),
                directives: Directives::default(),
                binding: Some("validation_records[1]".to_string()),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            },
        ])
        .expect("fixture has deletes"),
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        upstream_binding: "cert".to_string(),
        template: Box::new(validation_deferred_for_expression()),
    })));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };
    let observer = MockObserver::new();

    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);
    assert_eq!(
        result.failure_count,
        0,
        "events: {:?}; calls: {:?}",
        observer.events(),
        provider.calls()
    );

    let calls = provider.calls();
    let create_validation_pos = call_position(&calls, "create", "test.validation_records[0]");
    let delete_validation_0_pos = call_position(&calls, "delete", "test.validation_records[0]");
    let delete_validation_1_pos = call_position(&calls, "delete", "test.validation_records[1]");

    assert!(
        create_validation_pos < delete_validation_0_pos,
        "matching DeferredReplace delete must wait for the materialized create; calls: {calls:?}"
    );
    assert_ne!(
        delete_validation_0_pos, delete_validation_1_pos,
        "both absorbed deletes must dispatch; calls: {calls:?}"
    );
}

#[tokio::test]
async fn dispatch_deferred_replace_skips_delete_when_materialized_create_fails() {
    let cert_id = ResourceId::with_identity("test", "cert");
    let cert_state = State::existing(
        cert_id.clone(),
        HashMap::from([(
            "domain_validation_options".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(indexmap::IndexMap::from([(
                    "resource_record".to_string(),
                    Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
                        (
                            "name".to_string(),
                            Value::Concrete(ConcreteValue::String("_name".to_string())),
                        ),
                        (
                            "value".to_string(),
                            Value::Concrete(ConcreteValue::String("_value".to_string())),
                        ),
                    ]))),
                )])),
            )])),
        )]),
    );
    let provider = MockProvider::new();
    provider.push_create(Ok(cert_state));
    provider.push_create(Err(ProviderError::api_error("create failed")));

    let mut cert = Resource::new("test", "cert");
    cert.binding = Some("cert".to_string());
    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test",
                "validation_records[0]",
            )),
            identifier: "old-validation".to_string(),
            directives: Directives::default(),
            binding: Some("validation_records[0]".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }])
        .expect("fixture has one delete"),
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        upstream_binding: "cert".to_string(),
        template: Box::new(validation_deferred_for_expression()),
    })));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };
    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(result.failure_count, 1);
    assert!(
        provider
            .calls()
            .iter()
            .any(|(op, id)| op == "create" && id == "test.validation_records[0]"),
        "test setup must dispatch the materialized create; calls: {:?}",
        provider.calls()
    );
    assert!(
        !provider
            .calls()
            .iter()
            .any(|(op, id)| op == "delete" && id == "test.validation_records[0]"),
        "old delete must not dispatch after materialized create failure; calls: {:?}",
        provider.calls()
    );
}

#[tokio::test]
async fn deferred_replace_delete_runs_in_flight_after_completed_sibling_wakes_normalizer() {
    #[derive(Clone)]
    struct LockOrderScenario {
        aws_normalize: Arc<AsyncMutex<()>>,
        awscc_shared: Arc<AsyncMutex<()>>,
        alb_waiting_for_awscc: Arc<Notify>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl LockOrderScenario {
        fn new() -> Self {
            Self {
                aws_normalize: Arc::new(AsyncMutex::new(())),
                awscc_shared: Arc::new(AsyncMutex::new(())),
                alb_waiting_for_awscc: Arc::new(Notify::new()),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn record(&self, call: impl Into<String>) {
            self.calls.lock().unwrap().push(call.into());
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[derive(Clone)]
    struct LockOrderNormalizer {
        scenario: LockOrderScenario,
    }

    impl crate::provider::ProviderNormalizer for LockOrderNormalizer {
        fn normalize_desired<'a>(&'a self, resources: &'a mut [Resource]) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                let is_alb = resources
                    .iter()
                    .any(|resource| resource.id.identity_or_empty() == "alb");
                {
                    let _aws = self.scenario.aws_normalize.lock().await;
                    tokio::task::yield_now().await;
                }
                if is_alb {
                    self.scenario.record("alb-normalize-wait-awscc");
                    self.scenario.alb_waiting_for_awscc.notify_one();
                }
                let _awscc = self.scenario.awscc_shared.lock().await;
                tokio::task::yield_now().await;
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
        ) -> BoxFuture<'a, ()> {
            crate::provider::ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut HashMap<ResourceId, State>,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ()> {
            crate::provider::ready_noop()
        }

        fn merge_default_tags<'a>(
            &'a self,
            _resources: &'a mut [Resource],
            _default_tags: &'a indexmap::IndexMap<String, Value>,
            _registry: &'a crate::schema::SchemaRegistry,
        ) -> BoxFuture<'a, ()> {
            crate::provider::ready_noop()
        }
    }

    #[derive(Clone)]
    struct LockOrderProvider {
        scenario: LockOrderScenario,
    }

    impl LockOrderProvider {
        async fn create_state(
            &self,
            id: &ResourceId,
        ) -> ProviderResult<crate::provider::CreateOutcome> {
            self.scenario.record(format!("create:{id}"));
            if id.identity_or_empty() == "cert" {
                let _provider_lock = self.scenario.awscc_shared.lock().await;
                self.scenario.alb_waiting_for_awscc.notified().await;
                return Ok(crate::provider::CreateOutcome::Success {
                    state: cert_state_for_deferred_replace_deadlock(id),
                });
            }

            if id.identity_or_empty() == "alb" {
                let _provider_lock = self.scenario.awscc_shared.lock().await;
                return Ok(crate::provider::CreateOutcome::Success {
                    state: State::existing(id.clone(), HashMap::new()).with_identifier("alb-id"),
                });
            }

            Ok(crate::provider::CreateOutcome::Success {
                state: State::existing(id.clone(), HashMap::new())
                    .with_identifier(format!("{}-id", id.identity_or_empty())),
            })
        }

        async fn delete_id(&self, id: &ResourceId) -> ProviderResult<()> {
            self.scenario.record(format!("delete:{id}"));
            let _provider_lock = self.scenario.awscc_shared.lock().await;
            Ok(())
        }
    }

    impl Provider for LockOrderProvider {
        fn name(&self) -> &str {
            "lock-order"
        }

        fn read(
            &self,
            id: &ResourceId,
            _identifier: Option<&str>,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id = id.clone();
            Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
        }

        fn read_data_source(&self, resource: &DataSource) -> BoxFuture<'_, ProviderResult<State>> {
            self.read(&resource.id, None, ReadRequest)
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::CreateOutcome>> {
            let id = id.clone();
            Box::pin(async move { self.create_state(&id).await })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("update not used")) })
        }

        fn delete(
            &self,
            id: &ResourceId,
            _identifier: &str,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            let id = id.clone();
            Box::pin(async move { self.delete_id(&id).await })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: crate::effect::PlanOp,
        ) -> Vec<String> {
            Vec::new()
        }
    }

    let scenario = LockOrderScenario::new();
    let provider = LockOrderProvider {
        scenario: scenario.clone(),
    };
    let normalizer = LockOrderNormalizer {
        scenario: scenario.clone(),
    };
    let mut plan = Plan::new();
    let cert = resource_with_binding("cert", "cert");
    let cert_id = cert.id.clone();
    plan.add(create_effect(cert.clone()));
    plan.add(create_effect(resource_with_binding("alb", "alb")));
    plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test",
                "validation_records[0]",
            )),
            identifier: "old-validation".to_string(),
            directives: Directives::default(),
            binding: Some("validation_records[0]".to_string()),
            dependencies: HashSet::from(["cert".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }])
        .expect("fixture has one delete"),
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        upstream_binding: "cert".to_string(),
        template: Box::new(validation_deferred_for_expression()),
    })));

    let unresolved = HashMap::from([(cert_id, UnresolvedResource::from_pre_resolve(cert.clone()))]);
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &unresolved,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &normalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(2).unwrap(),
    };
    let observer = MockObserver::new();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        execute_plan(&provider, input, &observer, CancellationToken::new()),
    )
    .await
    .expect("expanded DeferredReplace deletes must stay in in_flight");
    let result = completed_result(outcome);

    assert_eq!(result.failure_count, 0);
    assert!(
        result
            .runtime_synthesized_resources
            .iter()
            .any(|resource| resource.id
                == ResourceId::with_identity("test", "validation_records[0]")),
        "deferred replace gate must synthesize the child create"
    );
    assert!(
        result
            .applied_states
            .contains_key(&ResourceId::with_identity("test", "validation_records[0]")),
        "post-state must include the synthesized child create"
    );
    assert!(
        scenario
            .calls()
            .iter()
            .any(|call| call == "delete:test.validation_records[0]"),
        "absorbed delete must run through the provider delete path"
    );
}

fn resource_with_binding(name: &str, binding: &str) -> Resource {
    let mut resource = Resource::new("test", name);
    resource.binding = Some(binding.to_string());
    resource
}

fn cert_state_for_deferred_replace_deadlock(id: &ResourceId) -> State {
    State::existing(
        id.clone(),
        HashMap::from([(
            "domain_validation_options".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(indexmap::IndexMap::from([(
                    "resource_record".to_string(),
                    Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
                        (
                            "name".to_string(),
                            Value::Concrete(ConcreteValue::String("_name".to_string())),
                        ),
                        (
                            "value".to_string(),
                            Value::Concrete(ConcreteValue::String("_value".to_string())),
                        ),
                    ]))),
                )])),
            )])),
        )]),
    )
    .with_identifier("cert-id")
}

// -----------------------------------------------------------------------
// carina#3252: a managed-resource attribute that references a
// `read aws.*` data source must resolve from the data source's
// pre-refreshed `current_states` row at apply time. Pre-fix, the
// pre-apply binding view never merged the data source's `State.attributes`
// into its binding, so the executor's `assert_fully_resolved` rejected
// the unresolved `ResourceRef` with the misleading "add a `wait` block"
// message that does not apply to data sources.
// -----------------------------------------------------------------------

/// End-to-end binding-view → executor wiring: a downstream managed
/// resource references `admin_access_roles.arns` (a `read aws.iam.Roles`
/// data source). The data source's read result lives in
/// `current_states[data_source.id]`. `ResolvedBindings::pre_apply` (the
/// only constructor real apply uses) must surface that read state on the
/// data source's binding so the executor's resolve step finds a concrete
/// value to hand to the provider.
///
/// Mirrors the production `carina apply` repro in carina#3252:
/// `assume_role_policy_document.statement[].principal.aws = admin_access_roles.arns`
/// on `carina-rs/infra@main`.
#[tokio::test]
async fn test_data_source_read_state_resolves_for_downstream_resource() {
    use crate::binding_index::{PreApplyInputs, ResolvedBindings};
    use crate::resource::{AccessPath, ResourceId};

    // `RecordingMockProvider` captures the resolved attribute map the
    // executor handed to `create()`, so the test can assert what the
    // provider actually saw.
    let provider = RecordingMockProvider::new();

    // Data source: `let admin_access_roles = read aws.iam.Roles { ... }`.
    // The DSL attributes hold input filters only; the produced `arns`
    // list lives in `current_states[ds.id]`. `with_provider` matches
    // the id shape real apply uses.
    let ds_id = ResourceId::with_provider_identity("aws", "iam.Roles", "admin_access_roles", None);
    let mut ds = DataSource::with_provider("aws", "iam.Roles", "admin_access_roles", None);
    ds.id = ds_id.clone();
    ds.binding = Some("admin_access_roles".to_string());
    ds.attributes.insert(
        "path_prefix".to_string(),
        Value::Concrete(ConcreteValue::String(
            "/aws-reserved/sso.amazonaws.com/".to_string(),
        )),
    );

    let arn_value = "arn:aws:iam::111111111111:role/aws-reserved/sso.amazonaws.com/AWSReservedSSO_AdministratorAccess_abcdef0123456789";
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    current_states.insert(
        ds_id.clone(),
        State::existing(
            ds_id.clone(),
            vec![(
                "arns".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String(arn_value.to_string()),
                )])),
            )]
            .into_iter()
            .collect(),
        ),
    );

    // Downstream managed: `awscc.iam.Role rd.role`. Its
    // `assume_role_policy_document_arns` attr is a `ResourceRef` to
    // `admin_access_roles.arns`. (Modelled as a top-level attribute
    // for test concision; the production case nests it inside a
    // struct, but the resolve path is the same per-attribute one.)
    let role_id = ResourceId::with_provider_identity("awscc", "iam.Role", "rd.role", None);
    let role = {
        let mut r = Resource::with_provider("awscc", "iam.Role", "rd.role", None);
        r.id = role_id.clone();
        r.binding = Some("role".to_string());
        // No `dependency_bindings` — the executor's dependency-graph
        // path only registers managed-resource bindings into
        // `binding_to_idx` (data sources never become effects), so the
        // field isn't load-bearing here. The dependency is implicit:
        // the data source's read result lives in `current_states`
        // before the executor runs.
        r.set_attr(
            "assume_role_policy_document_arns",
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("admin_access_roles", "arns"),
            }),
        );
        r
    };

    let mut plan = Plan::new();
    plan.add(create_effect(role.clone()));

    let role_state =
        State::existing(role_id.clone(), HashMap::new()).with_identifier("rd.role-iam-id");
    provider.push_create(Ok(role_state));

    // Build bindings the way the real apply path does (carina#3248):
    // one typed constructor that lays managed + data sources +
    // current_states in one go.
    let bindings = ResolvedBindings::pre_apply(PreApplyInputs {
        managed: &[role],
        compositions: &[],
        data_sources: &[ds],
        current_states: &crate::resource::into_plan_input_map(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        compositions: &[],
        bindings,
        current_states,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, CancellationToken::new()).await);

    assert_eq!(
        result.failure_count,
        0,
        "data-source ref must resolve; pre-fix this errored with \
         'has not been published yet' (carina#3252). events: {:?}",
        observer.events(),
    );
    assert_eq!(result.success_count, 1);

    // The executor must have handed the provider a concrete
    // List<String> resolved from the data source's read state — not the
    // original `Value::Deferred(ResourceRef)`.
    let calls = provider.create_calls();
    assert_eq!(calls.len(), 1);
    let (_, role_attrs) = &calls[0];
    let resolved = role_attrs
        .get("assume_role_policy_document_arns")
        .expect("downstream attr must be present in the create call");
    assert_eq!(
        resolved,
        &Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String(arn_value.to_string())
        )])),
        "the ResourceRef must resolve to the data source's read-state \
         `arns`; got {resolved:?}",
    );
}
