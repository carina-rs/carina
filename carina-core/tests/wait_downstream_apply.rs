//! carina#3061 — a `wait` block declared *inside a `use`d module* must
//! survive module expansion (with instance-prefixing) so a downstream
//! resource that references `<wait_binding>.<attr>` keeps its
//! synchronization edge.
//!
//! Real infra shape (carina-rs/infra):
//!   registry/dev/registry/main.crn : let r = registry { ... }   (caller)
//!   usecases/registry/acm.crn      : let cert_issued = wait cert { ... }
//!   usecases/registry/cloudfront.crn: distribution_config = {
//!         viewer_certificate = { acm_certificate_arn =
//!                                cert_issued.certificate_arn } }
//!
//! Pre-fix: `expand_module_call` returns only `Vec<Resource>`, so the
//! module's `wait_bindings` are silently dropped and the module
//! resources' references to the wait binding are never instance-
//! prefixed. `create_plan` emits no `Effect::Wait`, the Distribution
//! gets no dependency edge, dispatches immediately, and fails at 0.0s
//! with the self-contradicting "add a `wait` block" error
//! (`assert_fully_resolved` on the still-`Deferred`
//! `cert_issued.certificate_arn`).
//!
//! Per CLAUDE.md "directory-scoped, never single-file": the fixture is
//! a multi-file caller dir + multi-file module dir, exercising the real
//! merged-parse + module-expansion path the CLI runs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use carina_core::config_loader::parse_directory;
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::create_plan;
use carina_core::executor::{ExecutionInput, ExecutionObserver, execute_plan};
use carina_core::module_resolver::resolve_modules;
use carina_core::parser::ProviderContext;
use carina_core::provider::{
    BoxFuture, CreateRequest, DeleteRequest, NoopNormalizer, Provider, ProviderResult, ReadRequest,
    UpdateRequest,
};
use carina_core::resolver::resolve_refs_with_state_and_remote;
use carina_core::resource::{ConcreteValue, ResourceId, State, Value};
use carina_core::schema::SchemaRegistry;

/// cert read returns ISSUED (+ certificate_arn) immediately; other
/// resources create/read trivially. Keeps a correct wait fast.
struct MockProvider;

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
        let id = id.clone();
        Box::pin(async move {
            if id.resource_type == "acm.Certificate" {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "status".to_string(),
                    Value::Concrete(ConcreteValue::String("ISSUED".to_string())),
                );
                attrs.insert(
                    "certificate_arn".to_string(),
                    Value::Concrete(ConcreteValue::String(
                        "arn:aws:acm:us-east-1:111:certificate/abc".to_string(),
                    )),
                );
                Ok(State::existing(id, attrs).with_identifier("acm-cert-id"))
            } else {
                Ok(State::not_found(id))
            }
        })
    }

    fn read_data_source(
        &self,
        resource: &carina_core::resource::Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
    }

    fn create(
        &self,
        id: &ResourceId,
        _request: CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move {
            let mut attrs = HashMap::new();
            if id.resource_type == "acm.Certificate" {
                attrs.insert(
                    "status".to_string(),
                    Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".to_string())),
                );
            }
            attrs.insert(
                "id".to_string(),
                Value::Concrete(ConcreteValue::String("id-123".to_string())),
            );
            Ok(State::existing(id, attrs).with_identifier("id-123"))
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = id.clone();
        Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &str,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move { Ok(()) })
    }
}

struct CollectingObserver {
    failures: Mutex<Vec<String>>,
}

impl ExecutionObserver for CollectingObserver {
    fn on_event(&self, event: &carina_core::executor::ExecutionEvent) {
        if let carina_core::executor::ExecutionEvent::EffectFailed { error, .. } = event {
            self.failures.lock().unwrap().push(error.to_string());
        }
    }
}

#[tokio::test]
async fn module_wait_binding_survives_expansion_and_synchronizes_downstream() {
    let mut caller = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    caller.push("tests/fixtures/wait/module_wait_downstream/caller");

    let mut parsed = parse_directory(&caller, &ProviderContext::default())
        .expect("parse_directory should succeed for the caller fixture");

    resolve_modules(&mut parsed, &caller).expect("module resolution should succeed");

    // ---- Root-cause assertion #1: the module's wait binding survived
    // expansion, instance-prefixed with the module-call binding (`r`).
    assert_eq!(
        parsed.wait_bindings.len(),
        1,
        "the `wait cert` declared inside the `use`d module must be \
         propagated to the caller after expansion; got {:?}",
        parsed
            .wait_bindings
            .iter()
            .map(|w| (&w.binding, &w.target))
            .collect::<Vec<_>>()
    );
    let wb = &parsed.wait_bindings[0];
    assert_eq!(
        wb.binding, "r.cert_issued",
        "wait binding must be instance-prefixed (r.cert_issued)"
    );
    assert_eq!(
        wb.target, "r.cert",
        "wait target must be instance-prefixed (r.cert)"
    );
    assert_eq!(
        wb.until_predicate.lhs_segments.first().map(String::as_str),
        Some("r.cert"),
        "until predicate LHS root must be instance-prefixed; got {:?}",
        wb.until_predicate.lhs_segments
    );
    assert!(
        wb.depends_on.iter().any(|d| d == "r.validation_record"),
        "depends_on entries must be instance-prefixed; got {:?}",
        wb.depends_on
    );

    // ---- Root-cause assertion #2: the expanded Distribution's nested
    // ref was rewritten to the prefixed wait binding, so it can be
    // linked to the Effect::Wait.
    let dist = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "cloudfront.Distribution")
        .expect("expanded Distribution must be present");
    assert!(
        carina_core::deps::get_resource_value_ref_dependencies(dist).contains("r.cert_issued"),
        "Distribution must depend on the prefixed wait binding \
         `r.cert_issued`; deps were {:?}",
        carina_core::deps::get_resource_value_ref_dependencies(dist)
    );

    // ---- Apply pipeline (mirrors `carina apply`): the Distribution
    // must not fail / be skipped — it must wait for `r.cert_issued`.
    let sorted_resources =
        sort_resources_by_dependencies(&parsed.resources).expect("topological sort should succeed");

    let current_states: HashMap<ResourceId, State> = HashMap::new();
    let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();

    let mut resources_for_plan = sorted_resources.clone();
    resolve_refs_with_state_and_remote(&mut resources_for_plan, &current_states, &remote_bindings)
        .expect("resolve_refs should succeed");

    let registry = SchemaRegistry::new();
    let plan = create_plan(
        &resources_for_plan,
        &current_states,
        &HashMap::new(),
        &registry,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &parsed.wait_bindings,
    );

    assert!(
        plan.effects().iter().any(|e| matches!(
            e,
            carina_core::effect::Effect::Wait { binding, .. } if binding == "r.cert_issued"
        )),
        "create_plan must emit Effect::Wait for the prefixed wait binding"
    );

    let unresolved_resources: HashMap<ResourceId, _> = sorted_resources
        .iter()
        .map(|r| (r.id.clone(), r.clone()))
        .collect();

    let provider = MockProvider;
    let observer = CollectingObserver {
        failures: Mutex::new(Vec::new()),
    };
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &unresolved_resources,
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        current_states,
        normalizer: &NoopNormalizer,
    };

    let result = execute_plan(&provider, input, &observer).await;

    let failures = observer.failures.lock().unwrap().clone();
    assert_eq!(
        result.failure_count, 0,
        "no effect should fail: the Distribution's \
         `cert_issued.certificate_arn` must resolve after the wait, \
         not fail immediately at dispatch. Failures: {failures:?}"
    );
    assert_eq!(
        result.skip_count, 0,
        "no effect should be skipped. Failures: {failures:?}"
    );
}

/// carina#3061, nested case: a `wait` declared *two module levels deep*
/// (root → outer → inner, where `inner` holds the wait + the
/// downstream Distribution) must survive both expansions, with the
/// binding doubly instance-prefixed (`o.c.cert_issued`), in lockstep
/// with the doubly-prefixed downstream ref. This guards the
/// `resolve_nested_modules` propagation path that re-prefixes an
/// already-prefixed wait binding.
#[tokio::test]
async fn nested_module_wait_binding_survives_two_expansions() {
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.push("tests/fixtures/wait/module_wait_nested/root");

    let mut parsed = parse_directory(&root, &ProviderContext::default())
        .expect("parse_directory should succeed for the nested root fixture");

    resolve_modules(&mut parsed, &root).expect("nested module resolution should succeed");

    // The two-level-deep wait binding survived, doubly instance-prefixed
    // (outer call binding `o`, inner call binding `c`).
    assert_eq!(
        parsed.wait_bindings.len(),
        1,
        "the `wait` two modules deep must reach the root caller; got {:?}",
        parsed
            .wait_bindings
            .iter()
            .map(|w| (&w.binding, &w.target))
            .collect::<Vec<_>>()
    );
    let wb = &parsed.wait_bindings[0];
    assert_eq!(wb.binding, "o.c.cert_issued");
    assert_eq!(wb.target, "o.c.cert");
    assert_eq!(
        wb.until_predicate.lhs_segments.first().map(String::as_str),
        Some("o.c.cert"),
        "until LHS root must be doubly-prefixed; got {:?}",
        wb.until_predicate.lhs_segments
    );

    // The inner Distribution's nested ref was doubly-rewritten to match.
    let dist = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "cloudfront.Distribution")
        .expect("expanded Distribution must be present");
    assert!(
        carina_core::deps::get_resource_value_ref_dependencies(dist).contains("o.c.cert_issued"),
        "Distribution must depend on the doubly-prefixed wait binding \
         `o.c.cert_issued`; deps were {:?}",
        carina_core::deps::get_resource_value_ref_dependencies(dist)
    );

    // End-to-end apply: the Distribution must wait, not fail/skip.
    let sorted_resources =
        sort_resources_by_dependencies(&parsed.resources).expect("topological sort should succeed");
    let current_states: HashMap<ResourceId, State> = HashMap::new();
    let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();

    let mut resources_for_plan = sorted_resources.clone();
    resolve_refs_with_state_and_remote(&mut resources_for_plan, &current_states, &remote_bindings)
        .expect("resolve_refs should succeed");

    let registry = SchemaRegistry::new();
    let plan = create_plan(
        &resources_for_plan,
        &current_states,
        &HashMap::new(),
        &registry,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &parsed.wait_bindings,
    );
    assert!(
        plan.effects().iter().any(|e| matches!(
            e,
            carina_core::effect::Effect::Wait { binding, .. } if binding == "o.c.cert_issued"
        )),
        "create_plan must emit Effect::Wait for the doubly-prefixed binding"
    );

    let unresolved_resources: HashMap<ResourceId, _> = sorted_resources
        .iter()
        .map(|r| (r.id.clone(), r.clone()))
        .collect();

    let provider = MockProvider;
    let observer = CollectingObserver {
        failures: Mutex::new(Vec::new()),
    };
    let input = ExecutionInput {
        plan: &plan,
        unresolved_resources: &unresolved_resources,
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        current_states,
        normalizer: &NoopNormalizer,
    };

    let result = execute_plan(&provider, input, &observer).await;
    let failures = observer.failures.lock().unwrap().clone();
    assert_eq!(
        result.failure_count, 0,
        "nested-module wait must synchronize the Distribution. Failures: {failures:?}"
    );
    assert_eq!(
        result.skip_count, 0,
        "no effect should be skipped. Failures: {failures:?}"
    );
}
