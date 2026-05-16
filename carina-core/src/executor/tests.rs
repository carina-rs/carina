use super::*;
use crate::plan::Plan;
use crate::provider::{
    BoxFuture, CreateRequest, DeleteRequest, NoopNormalizer, ProviderError, ProviderResult,
    ReadRequest, UpdateRequest,
};
use crate::resource::{ConcreteValue, DeferredValue, Directives, Resource, Value};
use parallel::{build_dependency_levels, build_dependency_map};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// -----------------------------------------------------------------------
// Mock Provider
// -----------------------------------------------------------------------

struct MockProvider {
    create_results: Mutex<Vec<ProviderResult<State>>>,
    delete_results: Mutex<Vec<ProviderResult<()>>>,
    update_results: Mutex<Vec<ProviderResult<State>>>,
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
        self.create_results.lock().unwrap().push(result);
    }

    fn push_delete(&self, result: ProviderResult<()>) {
        self.delete_results.lock().unwrap().push(result);
    }

    fn push_update(&self, result: ProviderResult<State>) {
        self.update_results.lock().unwrap().push(result);
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

    fn read_data_source(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        self.read(&resource.id, None, ReadRequest)
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id_str = id.to_string();
        self.call_log
            .lock()
            .unwrap()
            .push(("create".to_string(), id_str));
        self.create_resources.lock().unwrap().push(request.resource);
        let result = self.create_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
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
}

// -----------------------------------------------------------------------
// Mock Observer
// -----------------------------------------------------------------------

struct MockObserver {
    events: Mutex<Vec<String>>,
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
        let msg = match event {
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
            ExecutionEvent::EffectFailed { effect, error, .. } => {
                format!("failed:{}:{}", effect.resource_id(), error)
            }
            ExecutionEvent::EffectSkipped { effect, reason, .. } => {
                format!("skipped:{}:{}", effect.resource_id(), reason)
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
        };
        self.events.lock().unwrap().push(msg);
    }
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
    fn normalize_desired(&self, resources: &mut [Resource]) {
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
    }

    fn merge_default_tags(
        &self,
        _resources: &mut [Resource],
        _default_tags: &indexmap::IndexMap<String, Value>,
        _registry: &crate::schema::SchemaRegistry,
    ) {
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

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[tokio::test]
async fn test_simple_create() {
    let provider = MockProvider::new();
    let resource = make_resource("a", &[]);
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(Effect::Create(resource));

    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 0);
    assert!(
        observer
            .events()
            .iter()
            .any(|e| e.starts_with("succeeded:"))
    );
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
    plan.add(Effect::Create(resource));
    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &CanonicalizingNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;
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
        id: rid.clone(),
        from: Box::new(from_state),
        to: to_resource,
        changed_attributes: vec!["marker".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &CanonicalizingNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;
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
    plan.add(Effect::Create(ra));
    plan.add(Effect::Create(rb));
    provider.push_create(Ok(ok_state(&ra_id)));
    provider.push_create(Ok(ok_state(&rb_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &CanonicalizingNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;
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

#[tokio::test]
async fn test_simple_delete() {
    let provider = MockProvider::new();
    let rid = ResourceId::new("test", "a");

    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: rid.clone(),
        identifier: "id-123".to_string(),
        directives: Directives::default(),
        binding: None,
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
    });

    provider.push_delete(Ok(()));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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
    plan.add(Effect::Create(ra));
    plan.add(Effect::Create(rb));

    // First create fails
    provider.push_create(Err(ProviderError::api_error("create failed")));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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
async fn test_cbd_creates_before_deletes() {
    // CBD Replace: create should happen before delete
    let provider = MockProvider::new();
    let rid = ResourceId::new("test", "a");
    let from = State::existing(rid.clone(), HashMap::new()).with_identifier("old-id");
    let to = Resource::new("test", "a");

    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: rid.clone(),
        from: Box::new(from),
        to,
        directives: Directives {
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["attr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    provider.push_create(Ok(ok_state(&rid)));
    provider.push_delete(Ok(()));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 0);

    let calls = provider.calls();
    assert_eq!(calls[0].0, "create");
    assert_eq!(calls[1].0, "delete");
}

#[tokio::test]
async fn test_dbd_deletes_before_creates() {
    // Non-CBD Replace: delete should happen before create
    let provider = MockProvider::new();
    let rid = ResourceId::new("test", "a");
    let from = State::existing(rid.clone(), HashMap::new()).with_identifier("old-id");
    let to = Resource::new("test", "a");

    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: rid.clone(),
        from: Box::new(from),
        to,
        directives: Directives::default(),
        changed_create_only: vec!["attr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    provider.push_delete(Ok(()));
    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 0);

    let calls = provider.calls();
    assert_eq!(calls[0].0, "delete");
    assert_eq!(calls[1].0, "create");
}

#[tokio::test]
async fn test_phased_cbd_creates_in_forward_order_deletes_in_reverse() {
    // Two interdependent replaces: vpc (parent) and subnet (depends on vpc)
    // Both are CBD. Expected order:
    //   Phase 2: create vpc, create subnet (forward)
    //   Phase 3: delete subnet, delete vpc (reverse)
    //   Phase 4: finalize (success events)
    let provider = MockProvider::new();
    let vpc_id = ResourceId::new("test", "vpc");
    let subnet_id = ResourceId::new("test", "subnet");

    let vpc_from = State::existing(vpc_id.clone(), HashMap::new()).with_identifier("vpc-old");
    let mut vpc_to = Resource::new("test", "vpc");
    vpc_to.binding = Some("vpc".to_string());

    let subnet_from =
        State::existing(subnet_id.clone(), HashMap::new()).with_identifier("subnet-old");
    let mut subnet_to = Resource::new("test", "subnet");
    subnet_to.binding = Some("subnet".to_string());
    subnet_to.dependency_bindings = std::collections::BTreeSet::from(["vpc".to_string()]);

    let cbd_directives = Directives {
        create_before_destroy: true,
        ..Default::default()
    };

    let mut plan = Plan::new();
    // Order in plan: vpc first, subnet second
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(vpc_from),
        to: vpc_to,
        directives: cbd_directives.clone(),
        changed_create_only: vec!["attr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });
    plan.add(Effect::Replace {
        id: subnet_id.clone(),
        from: Box::new(subnet_from),
        to: subnet_to,
        directives: cbd_directives,
        changed_create_only: vec!["attr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    // Phase 2: create vpc, create subnet
    provider.push_create(Ok(ok_state(&vpc_id)));
    provider.push_create(Ok(ok_state(&subnet_id)));
    // Phase 3: delete subnet (reverse), delete vpc (reverse)
    provider.push_delete(Ok(()));
    provider.push_delete(Ok(()));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

    assert_eq!(result.success_count, 2);
    assert_eq!(result.failure_count, 0);

    let calls = provider.calls();
    // Phase 2: creates in forward order (vpc before subnet)
    assert_eq!(calls[0], ("create".to_string(), vpc_id.to_string()));
    assert_eq!(calls[1], ("create".to_string(), subnet_id.to_string()));
    // Phase 3: deletes in reverse order (subnet before vpc)
    assert_eq!(calls[2], ("delete".to_string(), subnet_id.to_string()));
    assert_eq!(calls[3], ("delete".to_string(), vpc_id.to_string()));
}

#[tokio::test]
async fn test_phased_noncbd_creates_after_deletes() {
    // Two interdependent non-CBD replaces: vpc (parent) and subnet (depends on vpc)
    // Expected order:
    //   Phase 3: delete subnet, delete vpc (reverse dependency)
    //   Phase 4: create vpc, create subnet (forward dependency)
    let provider = MockProvider::new();
    let vpc_id = ResourceId::new("test", "vpc");
    let subnet_id = ResourceId::new("test", "subnet");

    let vpc_from = State::existing(vpc_id.clone(), HashMap::new()).with_identifier("vpc-old");
    let mut vpc_to = Resource::new("test", "vpc");
    vpc_to.binding = Some("vpc".to_string());

    let subnet_from =
        State::existing(subnet_id.clone(), HashMap::new()).with_identifier("subnet-old");
    let mut subnet_to = Resource::new("test", "subnet");
    subnet_to.binding = Some("subnet".to_string());
    subnet_to.dependency_bindings = std::collections::BTreeSet::from(["vpc".to_string()]);

    let dbd_directives = Directives::default();

    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(vpc_from),
        to: vpc_to,
        directives: dbd_directives.clone(),
        changed_create_only: vec!["attr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });
    plan.add(Effect::Replace {
        id: subnet_id.clone(),
        from: Box::new(subnet_from),
        to: subnet_to,
        directives: dbd_directives,
        changed_create_only: vec!["attr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    // Phase 3: delete subnet, delete vpc
    provider.push_delete(Ok(()));
    provider.push_delete(Ok(()));
    // Phase 4: create vpc, create subnet
    provider.push_create(Ok(ok_state(&vpc_id)));
    provider.push_create(Ok(ok_state(&subnet_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

    assert_eq!(result.success_count, 2);
    assert_eq!(result.failure_count, 0);

    let calls = provider.calls();
    // Phase 3: deletes in reverse dependency order
    assert_eq!(calls[0], ("delete".to_string(), subnet_id.to_string()));
    assert_eq!(calls[1], ("delete".to_string(), vpc_id.to_string()));
    // Phase 4: creates in forward dependency order
    assert_eq!(calls[2], ("create".to_string(), vpc_id.to_string()));
    assert_eq!(calls[3], ("create".to_string(), subnet_id.to_string()));
}

#[tokio::test]
async fn test_observer_events_emitted_correctly() {
    let provider = MockProvider::new();
    let resource = make_resource("a", &[]);
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(Effect::Create(resource));

    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    execute_plan(&provider, input, &observer).await;

    let events = observer.events();
    assert_eq!(events.len(), 2);
    assert!(events[0].starts_with("started:"));
    assert!(events[1].starts_with("succeeded:"));
}

#[tokio::test]
async fn test_read_effect_is_no_op() {
    let provider = MockProvider::new();
    let resource = Resource::new("test", "data").with_read_only(true);

    let mut plan = Plan::new();
    plan.add(Effect::Read { resource });

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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
    plan.add(Effect::Create(vpc));
    plan.add(Effect::Create(subnet_a));
    plan.add(Effect::Create(subnet_b));

    provider.push_create(Ok(ok_state(&vpc_id)));
    provider.push_create(Ok(ok_state(&subnet_a_id)));
    provider.push_create(Ok(ok_state(&subnet_b_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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
    plan.add(Effect::Create(vpc));
    plan.add(Effect::Create(subnet_a));
    plan.add(Effect::Create(subnet_b));
    plan.add(Effect::Create(subnet_c));

    provider.push_create(Ok(ok_state(&vpc_id)));
    // subnet_a fails, subnet_b succeeds
    provider.push_create(Err(ProviderError::api_error("create failed")));
    provider.push_create(Ok(ok_state(&subnet_b_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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
    plan.add(Effect::Create(a));
    plan.add(Effect::Create(b));
    plan.add(Effect::Create(c));

    provider.push_create(Ok(ok_state(&a_id)));
    provider.push_create(Ok(ok_state(&b_id)));
    provider.push_create(Ok(ok_state(&c_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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
    plan.add(Effect::Create(a));
    plan.add(Effect::Create(b));
    plan.add(Effect::Create(c));
    plan.add(Effect::Create(d));

    let levels = build_dependency_levels(plan.effects(), &HashMap::new());

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
    plan.add(Effect::Create(vpc)); // idx 0
    plan.add(Effect::Create(tgw)); // idx 1
    plan.add(Effect::Create(subnet)); // idx 2
    plan.add(Effect::Create(tgw_attach)); // idx 3
    plan.add(Effect::Create(rt)); // idx 4
    plan.add(Effect::Create(route)); // idx 5

    let levels = build_dependency_levels(plan.effects(), &HashMap::new());

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

        fn read_data_source(&self, _resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("not implemented")) })
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            let id_clone = id.clone();
            let name = id.name_str().to_string();
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
                Ok(State::existing(id_clone, attrs).with_identifier("id-123"))
            })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<State>> {
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
    plan.add(Effect::Create(a));
    plan.add(Effect::Create(b));
    plan.add(Effect::Create(c));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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

#[tokio::test]
async fn test_waiting_events_emitted_for_dependent_effects() {
    // Setup: A has no deps, C depends on A.
    // C should get a Waiting event before A completes.
    let a = make_resource("a", &[]);
    let c = make_resource("c", &["a"]);

    let mut plan = Plan::new();
    plan.add(Effect::Create(a));
    plan.add(Effect::Create(c));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let provider = MockProvider::new();
    // Push create results for both resources
    let a_id = ResourceId::new("test", "a");
    let c_id = ResourceId::new("test", "c");
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
    let result = execute_plan(&provider, input, &observer).await;

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
/// Before the fix, `build_dependency_map` returned empty deps for deletes,
/// allowing parent and child deletes to run concurrently.
#[test]
fn test_build_dependency_levels_respects_delete_dependencies() {
    // Scenario: vpc (no deps), subnet (depends on vpc)
    // For creation: subnet depends on vpc → vpc first, then subnet
    // For deletion: vpc delete must wait for subnet delete → subnet first, then vpc
    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: ResourceId::new("ec2.Vpc", "my-vpc"),
        identifier: "vpc-123".to_string(),
        directives: Directives::default(),
        binding: Some("vpc".to_string()),
        dependencies: HashSet::new(), // vpc has no deps
        explicit_dependencies: HashSet::new(),
    });
    plan.add(Effect::Delete {
        id: ResourceId::new("ec2.Subnet", "my-subnet"),
        identifier: "subnet-456".to_string(),
        directives: Directives::default(),
        binding: Some("subnet".to_string()),
        dependencies: HashSet::from(["vpc".to_string()]), // subnet depends on vpc
        explicit_dependencies: HashSet::new(),
    });

    let levels = build_dependency_levels(plan.effects(), &HashMap::new());

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

/// Characterization test for #1306: build_dependency_levels and build_dependency_map
/// must produce consistent results. This test verifies that after refactoring
/// build_dependency_levels to reuse build_dependency_map, the level assignments
/// remain the same.
#[test]
fn test_build_dependency_levels_consistent_with_dependency_map() {
    // a (no deps), b depends on a, c depends on a, d depends on b and c
    let a = make_resource("a", &[]);
    let b = make_resource("b", &["a"]);
    let c = make_resource("c", &["a"]);
    let d = make_resource("d", &["b", "c"]);

    let mut plan = Plan::new();
    plan.add(Effect::Create(a));
    plan.add(Effect::Create(b));
    plan.add(Effect::Create(c));
    plan.add(Effect::Create(d));

    let levels = build_dependency_levels(plan.effects(), &HashMap::new());
    let dep_map = build_dependency_map(plan.effects(), &HashMap::new());

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

/// Characterization test for #1306: both execution paths (sequential and phased)
/// must produce the same results for an update effect with binding map propagation.
#[tokio::test]
async fn test_update_effect_binding_map_propagation() {
    let provider = MockProvider::new();
    let ra_id = ResourceId::new("test", "a");

    // Create initial state
    let from_state = State::existing(ra_id.clone(), HashMap::new()).with_identifier("id-original");
    let to_resource = make_resource("a", &[]);

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        id: ra_id.clone(),
        from: Box::new(from_state),
        to: to_resource,
        changed_attributes: vec!["some_attr".to_string()],
    });

    let updated_state =
        State::existing(ra_id.clone(), HashMap::new()).with_identifier("id-updated");
    provider.push_update(Ok(updated_state));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 0);
    assert!(result.applied_states.contains_key(&ra_id));

    let events = observer.events();
    assert!(events.iter().any(|e| e.starts_with("started:")));
    assert!(events.iter().any(|e| e.starts_with("succeeded:")));
}

/// Regression test for #1195: build_dependency_map also respects delete dependencies.
#[test]
fn test_build_dependency_map_respects_delete_dependencies() {
    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: ResourceId::new("ec2.Vpc", "my-vpc"),
        identifier: "vpc-123".to_string(),
        directives: Directives::default(),
        binding: Some("vpc".to_string()),
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
    });
    plan.add(Effect::Delete {
        id: ResourceId::new("ec2.Subnet", "my-subnet"),
        identifier: "subnet-456".to_string(),
        directives: Directives::default(),
        binding: Some("subnet".to_string()),
        dependencies: HashSet::from(["vpc".to_string()]),
        explicit_dependencies: std::collections::HashSet::new(),
    });

    let deps = build_dependency_map(plan.effects(), &HashMap::new());

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
    let mut vpc = Resource::new("test", "my-vpc");
    vpc.binding = Some("vpc".to_string());
    vpc.set_attr(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
    );
    let vpc_id = vpc.id.clone();

    // Subnet resource that references vpc.vpc_id
    let mut subnet = Resource::new("test", "my-subnet");
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
    plan.add(Effect::Create(vpc));
    plan.add(Effect::Create(subnet));

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
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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
    create_results: Mutex<Vec<ProviderResult<State>>>,
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
        self.create_results.lock().unwrap().push(result);
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

    fn read_data_source(&self, _resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id_str = id.to_string();
        let attrs = request.resource.resolved_attributes();
        self.create_log.lock().unwrap().push((id_str, attrs));
        let result = self.create_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
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
}

/// Regression test: when a resource is deleted and a dependent resource is
/// replaced (CBD), the delete must wait for the replace to complete.
///
/// Scenario (TGW attachment):
///   - tgw_a: Delete (binding removed from .crn)
///   - tgw_attachment: Replace (CBD) — from depends on tgw_a, to depends on tgw_b
///
/// Without the fix, both execute in parallel and tgw_a delete fails because
/// the old attachment (which references tgw_a) hasn't been deleted yet.
#[tokio::test]
async fn test_delete_waits_for_replace_cbd_of_dependent() {
    let provider = MockProvider::new();
    let tgw_a_id = ResourceId::new("test", "tgw_a");
    let tgw_b_id = ResourceId::new("test", "tgw_b");
    let attachment_id = ResourceId::new("test", "attachment");

    // tgw_a is being deleted (binding removed from desired config)
    let tgw_a_deps: HashSet<String> = HashSet::new();

    // attachment: Replace (CBD)
    // from: depends on tgw_a (recorded in state's dependency_bindings)
    let attachment_from = State::existing(attachment_id.clone(), HashMap::new())
        .with_identifier("attach-old")
        .with_dependency_bindings(std::collections::BTreeSet::from(["tgw_a".to_string()]));
    // to: depends on tgw_b (different TGW — dependency changed)
    let mut attachment_to = Resource::new("test", "attachment");
    attachment_to.binding = Some("attachment".to_string());
    attachment_to.dependency_bindings = std::collections::BTreeSet::from(["tgw_b".to_string()]);

    let cbd_directives = Directives {
        create_before_destroy: true,
        ..Default::default()
    };

    let mut plan = Plan::new();

    // tgw_b: Create (new resource)
    let mut tgw_b = Resource::new("test", "tgw_b");
    tgw_b.binding = Some("tgw_b".to_string());
    plan.add(Effect::Create(tgw_b));

    // tgw_a: Delete
    plan.add(Effect::Delete {
        id: tgw_a_id.clone(),
        identifier: "tgw-old".to_string(),
        directives: Default::default(),
        binding: Some("tgw_a".to_string()),
        dependencies: tgw_a_deps,
        explicit_dependencies: std::collections::HashSet::new(),
    });

    // attachment: Replace (CBD) — from depends on tgw_a
    plan.add(Effect::Replace {
        id: attachment_id.clone(),
        from: Box::new(attachment_from),
        to: attachment_to,
        directives: cbd_directives,
        changed_create_only: vec!["transit_gateway_id".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    // tgw_b create
    provider.push_create(Ok(ok_state(&tgw_b_id)));
    // attachment CBD: create new
    provider.push_create(Ok(ok_state(&attachment_id)));
    // attachment CBD: delete old
    provider.push_delete(Ok(()));
    // tgw_a: delete (should happen AFTER attachment replace completes)
    provider.push_delete(Ok(()));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

    assert_eq!(result.success_count, 3);
    assert_eq!(result.failure_count, 0);

    let calls = provider.calls();
    // tgw_b create must happen first (attachment depends on it)
    assert_eq!(calls[0], ("create".to_string(), tgw_b_id.to_string()));
    // attachment create (CBD: create before delete)
    assert_eq!(calls[1], ("create".to_string(), attachment_id.to_string()));
    // attachment delete (old)
    assert_eq!(calls[2], ("delete".to_string(), attachment_id.to_string()));
    // tgw_a delete MUST come after attachment replace completes
    assert_eq!(calls[3], ("delete".to_string(), tgw_a_id.to_string()));
}

/// Regression test for carina-provider-awscc#47:
/// When the Delete effect has binding: None (because the resource became anonymous
/// in step2), the reverse dependency from Replace(CBD).from.dependency_bindings
/// must still be resolved via the resource's state-recorded binding.
#[tokio::test]
async fn test_delete_waits_for_replace_cbd_even_when_delete_binding_is_none() {
    let provider = MockProvider::new();
    let tgw_a_id = ResourceId::new("test", "tgw_a");
    let tgw_b_id = ResourceId::new("test", "tgw_b");
    let attachment_id = ResourceId::new("test", "attachment");

    // tgw_a Delete has binding: None (anonymous in step2 .crn)
    // but state recorded it as "tgw_a"
    let tgw_a_deps: HashSet<String> = HashSet::new();

    // attachment Replace (CBD): from depends on tgw_a (state-recorded)
    let attachment_from = State::existing(attachment_id.clone(), HashMap::new())
        .with_identifier("attach-old")
        .with_dependency_bindings(std::collections::BTreeSet::from(["tgw_a".to_string()]));
    let mut attachment_to = Resource::new("test", "attachment");
    attachment_to.binding = Some("attachment".to_string());
    attachment_to.dependency_bindings = std::collections::BTreeSet::from(["tgw_b".to_string()]);

    let cbd_directives = Directives {
        create_before_destroy: true,
        ..Default::default()
    };

    let mut plan = Plan::new();

    // tgw_b: Create
    let mut tgw_b = Resource::new("test", "tgw_b");
    tgw_b.binding = Some("tgw_b".to_string());
    plan.add(Effect::Create(tgw_b));

    // tgw_a: Delete — binding is None (the key difference from the previous test)
    plan.add(Effect::Delete {
        id: tgw_a_id.clone(),
        identifier: "tgw-old".to_string(),
        directives: Default::default(),
        binding: None,
        dependencies: tgw_a_deps,
        explicit_dependencies: std::collections::HashSet::new(),
    });

    // attachment: Replace (CBD)
    plan.add(Effect::Replace {
        id: attachment_id.clone(),
        from: Box::new(attachment_from),
        to: attachment_to,
        directives: cbd_directives,
        changed_create_only: vec!["transit_gateway_id".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });

    provider.push_create(Ok(ok_state(&tgw_b_id)));
    provider.push_create(Ok(ok_state(&attachment_id)));
    provider.push_delete(Ok(()));
    provider.push_delete(Ok(()));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

    assert_eq!(result.success_count, 3);
    assert_eq!(result.failure_count, 0);

    let calls = provider.calls();
    assert_eq!(calls[0], ("create".to_string(), tgw_b_id.to_string()));
    assert_eq!(calls[1], ("create".to_string(), attachment_id.to_string()));
    assert_eq!(calls[2], ("delete".to_string(), attachment_id.to_string()));
    // tgw_a delete MUST still come after attachment replace, even though binding is None
    assert_eq!(calls[3], ("delete".to_string(), tgw_a_id.to_string()));
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
    plan.add(Effect::Create(cert));
    plan.add(Effect::Wait {
        binding: "cert_issued".to_string(),
        target_id: cert_id.clone(),
        target_identifier: None,
        until: WaitPredicate::Equals {
            attr: AttrPath::single("status"),
            value: Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
        },
        until_surface: "cert.status == ISSUED".to_string(),
        timeout: std::time::Duration::from_secs(60),
        interval: std::time::Duration::from_millis(1),
        explicit_dependencies: std::collections::HashSet::new(),
    });
    plan.add(Effect::Create(dist));

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
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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

#[tokio::test]
async fn test_wait_state_writeback_skips_synthetic_wait_id() {
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let provider = MockProvider::new();
    let cert = make_resource("cert", &[]);
    let cert_id = cert.id.clone();

    let mut plan = Plan::new();
    plan.add(Effect::Create(cert));
    plan.add(Effect::Wait {
        binding: "cert_issued".to_string(),
        target_id: cert_id.clone(),
        target_identifier: None,
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
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;
    assert_eq!(result.success_count, 2);
    // The wait's captured State is keyed under a synthetic `__wait`
    // ResourceId. This is what guarantees state writeback never sees
    // it as a real resource — `sorted_resources` (the writeback input)
    // does not contain `__wait` IDs.
    let synthetic = ResourceId::new("__wait", "cert_issued");
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
    plan.add(Effect::Create(cert));
    plan.add(Effect::Create(record));

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
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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
    plan.add(Effect::Create(cert));
    plan.add(Effect::Create(record));

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
        bindings: ResolvedBindings::default(),
        current_states: HashMap::new(),
        normalizer: &NoopNormalizer,
    };

    let observer = MockObserver::new();
    let result = execute_plan(&provider, input, &observer).await;

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
