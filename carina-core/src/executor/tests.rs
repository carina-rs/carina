use super::*;
use crate::plan::Plan;
use crate::provider::{BoxFuture, ProviderError, ProviderResult};
use crate::resource::{LifecycleConfig, Resource};
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
}

impl MockProvider {
    fn new() -> Self {
        Self {
            create_results: Mutex::new(Vec::new()),
            delete_results: Mutex::new(Vec::new()),
            update_results: Mutex::new(Vec::new()),
            read_results: Mutex::new(Vec::new()),
            call_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn push_create(&self, result: ProviderResult<State>) {
        self.create_results.lock().unwrap().push(result);
    }

    fn push_delete(&self, result: ProviderResult<()>) {
        self.delete_results.lock().unwrap().push(result);
    }

    #[allow(dead_code)]
    fn push_update(&self, result: ProviderResult<State>) {
        self.update_results.lock().unwrap().push(result);
    }

    #[allow(dead_code)]
    fn push_read(&self, result: ProviderResult<State>) {
        self.read_results.lock().unwrap().push(result);
    }

    fn calls(&self) -> Vec<(String, String)> {
        self.call_log.lock().unwrap().clone()
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
        self.read(&resource.id, None)
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let id_str = resource.id.to_string();
        self.call_log
            .lock()
            .unwrap()
            .push(("create".to_string(), id_str));
        let result = self.create_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _from: &State,
        _to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id_str = id.to_string();
        self.call_log
            .lock()
            .unwrap()
            .push(("update".to_string(), id_str));
        let result = self.update_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _lifecycle: &LifecycleConfig,
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
    State::existing(id.clone(), HashMap::new()).with_identifier("id-123")
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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

#[tokio::test]
async fn test_simple_delete() {
    let provider = MockProvider::new();
    let rid = ResourceId::new("test", "a");

    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: rid.clone(),
        identifier: "id-123".to_string(),
        lifecycle: LifecycleConfig::default(),
        binding: None,
        dependencies: HashSet::new(),
    });

    provider.push_delete(Ok(()));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
    provider.push_create(Err(ProviderError::new("create failed")));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
        lifecycle: LifecycleConfig {
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
        lifecycle: LifecycleConfig::default(),
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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

    let cbd_lifecycle = LifecycleConfig {
        create_before_destroy: true,
        ..Default::default()
    };

    let mut plan = Plan::new();
    // Order in plan: vpc first, subnet second
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(vpc_from),
        to: vpc_to,
        lifecycle: cbd_lifecycle.clone(),
        changed_create_only: vec!["attr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });
    plan.add(Effect::Replace {
        id: subnet_id.clone(),
        from: Box::new(subnet_from),
        to: subnet_to,
        lifecycle: cbd_lifecycle,
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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

    let dbd_lifecycle = LifecycleConfig::default();

    let mut plan = Plan::new();
    plan.add(Effect::Replace {
        id: vpc_id.clone(),
        from: Box::new(vpc_from),
        to: vpc_to,
        lifecycle: dbd_lifecycle.clone(),
        changed_create_only: vec!["attr".to_string()],
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });
    plan.add(Effect::Replace {
        id: subnet_id.clone(),
        from: Box::new(subnet_from),
        to: subnet_to,
        lifecycle: dbd_lifecycle,
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
    provider.push_create(Err(ProviderError::new("create failed")));
    provider.push_create(Ok(ok_state(&subnet_b_id)));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::new("not implemented")) })
        }

        fn read_data_source(&self, _resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::new("not implemented")) })
        }

        fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
            let id = resource.id.clone();
            let name = resource.id.name_str().to_string();
            let delay = self.delays.get(&name).copied().unwrap_or(Duration::ZERO);
            let log = self.call_log.clone();
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                log.lock()
                    .unwrap()
                    .push(("create".to_string(), name, Instant::now()));
                Ok(State::existing(id, HashMap::new()).with_identifier("id-123"))
            })
        }

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _from: &State,
            _to: &Resource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::new("not implemented")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &str,
            _lifecycle: &LifecycleConfig,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::new("not implemented")) })
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
    };

    let observer = MockObserver::new();
    let provider = MockProvider::new();
    // Push create results for both resources
    let a_id = ResourceId::new("test", "a");
    let c_id = ResourceId::new("test", "c");
    provider.push_create(Ok(
        State::existing(a_id, HashMap::new()).with_identifier("id-a")
    ));
    provider.push_create(Ok(
        State::existing(c_id, HashMap::new()).with_identifier("id-c")
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
        lifecycle: LifecycleConfig::default(),
        binding: Some("vpc".to_string()),
        dependencies: HashSet::new(), // vpc has no deps
    });
    plan.add(Effect::Delete {
        id: ResourceId::new("ec2.Subnet", "my-subnet"),
        identifier: "subnet-456".to_string(),
        lifecycle: LifecycleConfig::default(),
        binding: Some("subnet".to_string()),
        dependencies: HashSet::from(["vpc".to_string()]), // subnet depends on vpc
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
        lifecycle: LifecycleConfig::default(),
        binding: Some("vpc".to_string()),
        dependencies: HashSet::new(),
    });
    plan.add(Effect::Delete {
        id: ResourceId::new("ec2.Subnet", "my-subnet"),
        identifier: "subnet-456".to_string(),
        lifecycle: LifecycleConfig::default(),
        binding: Some("subnet".to_string()),
        dependencies: HashSet::from(["vpc".to_string()]),
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
    vpc.set_attr("cidr_block", Value::String("10.0.0.0/16".to_string()));
    let vpc_id = vpc.id.clone();

    // Subnet resource that references vpc.vpc_id
    let mut subnet = Resource::new("test", "my-subnet");
    subnet.set_attr(
        "vpc_id",
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    subnet.set_attr("cidr_block", Value::String("10.0.1.0/24".to_string()));
    subnet.dependency_bindings = std::collections::BTreeSet::from(["vpc".to_string()]);
    let subnet_id = subnet.id.clone();

    let mut plan = Plan::new();
    plan.add(Effect::Create(vpc));
    plan.add(Effect::Create(subnet));

    // VPC create returns state with vpc_id
    let vpc_state = State::existing(
        vpc_id.clone(),
        vec![("vpc_id".to_string(), Value::String("vpc-12345".to_string()))]
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
            Value::String("subnet-67890".to_string()),
        )]
        .into_iter()
        .collect(),
    )
    .with_identifier("subnet-67890");
    provider.push_create(Ok(subnet_state));

    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &HashMap::new(),
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
        Some(&Value::String("vpc-12345".to_string())),
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
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async {
            Err(ProviderError {
                message: "not implemented".to_string(),
                resource_id: None,
                cause: None,
                is_timeout: false,
            })
        })
    }

    fn read_data_source(&self, _resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::new("not implemented")) })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let id_str = resource.id.to_string();
        let attrs = resource.resolved_attributes();
        self.create_log.lock().unwrap().push((id_str, attrs));
        let result = self.create_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _from: &State,
        _to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async {
            Err(ProviderError {
                message: "not implemented".to_string(),
                resource_id: None,
                cause: None,
                is_timeout: false,
            })
        })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async {
            Err(ProviderError {
                message: "not implemented".to_string(),
                resource_id: None,
                cause: None,
                is_timeout: false,
            })
        })
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

    let cbd_lifecycle = LifecycleConfig {
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
        lifecycle: Default::default(),
        binding: Some("tgw_a".to_string()),
        dependencies: tgw_a_deps,
    });

    // attachment: Replace (CBD) — from depends on tgw_a
    plan.add(Effect::Replace {
        id: attachment_id.clone(),
        from: Box::new(attachment_from),
        to: attachment_to,
        lifecycle: cbd_lifecycle,
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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

    let cbd_lifecycle = LifecycleConfig {
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
        lifecycle: Default::default(),
        binding: None,
        dependencies: tgw_a_deps,
    });

    // attachment: Replace (CBD)
    plan.add(Effect::Replace {
        id: attachment_id.clone(),
        from: Box::new(attachment_from),
        to: attachment_to,
        lifecycle: cbd_lifecycle,
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
        binding_map: HashMap::new(),
        current_states: HashMap::new(),
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
