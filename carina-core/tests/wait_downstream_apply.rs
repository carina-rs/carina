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
use carina_core::data_source_value::DataSourceValues;
use carina_core::data_source_value::ExecutableDataSourceValues;
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::create_plan;
use carina_core::executor::{
    ExecutionObserver, ExecutionOutcome, ExecutionResult, LiveExecutionInput, ProviderCheckInputs,
    UnresolvedResource, execute_live_plan,
};
use carina_core::module_resolver::resolve_modules_providerless as resolve_modules;
use carina_core::parser::ProviderContext;
use carina_core::provider::{
    AppliedIdentifier, AppliedState, BoxFuture, CreateOutcome, CreateRequest, DeleteRequest,
    InitializedProvider, NoopNormalizer, Provider, ProviderInstanceKey, ProviderResult,
    ProviderRouter, ReadOutcome, ReadRequest, UpdateOutcome, UpdateRequest,
};
use carina_core::resolver::resolve_refs_with_state_and_remote;
use carina_core::resource::{
    ConcreteValue, Current, PlanInputCurrent, ResolvedResource, ResolvedResourceId, Resource,
    ResourceId, State, Value,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, ValidatedConfigSchema,
    ValidatedProviderSchemaWitness,
};
use carina_core::validation::deferred_types::{
    ProviderTypeCheckDiagnostic, ProviderTypeCheckSink, TypeCheckSession,
};

fn with_type_check_session<T>(
    catalog: &ProviderContext,
    operation: impl FnOnce(&dyn carina_core::validation::deferred_types::ProviderTypeCheckContext) -> T,
) -> T {
    TypeCheckSession::run(catalog, |session| operation(session)).publish(&mut TestTypeCheckSink)
}

struct TestTypeCheckSink;
impl ProviderTypeCheckSink for TestTypeCheckSink {
    fn record(&mut self, diagnostic: ProviderTypeCheckDiagnostic) {
        match diagnostic {
            ProviderTypeCheckDiagnostic::Deferred(warning) => eprintln!("{}", warning.message),
            ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(error) => panic!("{}", error.message),
        }
    }
}

fn empty_lifted_saved_attrs() -> carina_core::provider::LiftedSavedAttrs {
    carina_core::provider::RawSavedAttrs::default().lift(&SchemaRegistry::new())
}

fn managed_current(states: HashMap<ResourceId, State>) -> HashMap<ResourceId, Current> {
    states
        .into_iter()
        .map(|(id, state)| (id, Current::from_state(state)))
        .collect()
}

fn plan_input_current(
    states: HashMap<ResourceId, State>,
    schemas: &SchemaRegistry,
    resources: &[Resource],
) -> HashMap<ResourceId, PlanInputCurrent> {
    carina_core::resource::into_plan_input_current_map(managed_current(states), schemas, resources)
}

fn completed_result(outcome: ExecutionOutcome) -> ExecutionResult {
    match outcome {
        ExecutionOutcome::Completed(result) => result,
        ExecutionOutcome::Cancelled(result) => panic!(
            "uncancelled execution returned Cancelled: success={}, failure={}, skip={}",
            result.success_count,
            result.failure_count(),
            result.skip_count
        ),
    }
}

fn resolve_plan_graph(
    parsed: carina_core::parser::ExpandedParsedFile,
    resources: &[Resource],
    registry: &SchemaRegistry,
) -> (
    Vec<ResolvedResource>,
    Vec<carina_core::wait::ResolvedWaitBinding>,
) {
    let index = carina_core::resolved_graph::resolve_expanded_graph({
        let mut configuration = parsed;
        configuration.resources = (resources).to_vec();
        configuration.data_sources = Vec::new();
        configuration.into()
    })
    .expect("expanded test graph must resolve");
    let managed = resources
        .iter()
        .cloned()
        .map(|resource| index.resolve_resource(resource, registry))
        .collect::<Result<Vec<_>, _>>()
        .expect("managed test dependencies must resolve");
    let waits = index
        .configuration()
        .wait_bindings
        .iter()
        .map(|wait| index.resolve_wait_binding(wait))
        .collect::<Result<Vec<_>, _>>()
        .expect("wait test dependencies must resolve");
    (managed, waits)
}

/// cert read returns ISSUED (+ certificate_arn) immediately; other
/// resources create/read trivially. Keeps a correct wait fast.
struct MockProvider;

fn fixture_schemas(resources: &[Resource]) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    let owners = resources
        .iter()
        .map(|resource| {
            (
                resource.id.provider.clone(),
                resource.id.resource_type.clone(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    for (provider, resource_type) in owners {
        let mut schema = ResourceSchema::new(resource_type.clone());
        if resource_type == "acm.Certificate" {
            schema = schema
                .attribute(
                    AttributeSchema::provider_populated("status", AttributeType::string()).build(),
                )
                .attribute(
                    AttributeSchema::provider_populated("certificate_arn", AttributeType::string())
                        .build(),
                );
        }
        let witness = ValidatedProviderSchemaWitness::new(
            vec![schema.validate().expect("fixture schema is valid")],
            ValidatedConfigSchema::default(),
            Default::default(),
        )
        .expect("fixture provider witness is valid");
        schemas
            .insert_provider_witness(provider, &witness)
            .expect("fixture provider witness is consistent");
    }
    schemas
}

fn fixture_provider(resources: &[Resource]) -> ProviderRouter {
    let routes = resources
        .iter()
        .map(|resource| ProviderInstanceKey::from(&resource.id))
        .collect::<std::collections::BTreeSet<_>>();
    let mut provider = ProviderRouter::new();
    for route in routes {
        provider.add_provider_instance(
            route,
            InitializedProvider::unavailable(Box::new(MockProvider)),
        );
    }
    provider
}

impl Provider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let id = id.clone();
        let identifier = identifier.clone();
        Box::pin(async move {
            if id.resource_type == "acm.Certificate" {
                let mut attrs = HashMap::new();
                attrs.insert(
                    "status".to_string(),
                    Value::Concrete(ConcreteValue::String("ISSUED".into())),
                );
                attrs.insert(
                    "certificate_arn".to_string(),
                    Value::Concrete(ConcreteValue::String(
                        "arn:aws:acm:us-east-1:111:certificate/abc"
                            .to_string()
                            .into(),
                    )),
                );
                Ok(ReadOutcome::found(
                    AppliedState::new(id, identifier.into_inner(), attrs)
                        .expect("typed read identifier is non-empty"),
                ))
            } else {
                Ok(ReadOutcome::not_found(id))
            }
        })
    }

    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(State::existing(id, HashMap::new())) })
    }

    fn create(
        &self,
        id: &ResourceId,
        _request: CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, carina_core::provider::CreateFailure>> {
        let id = id.clone();
        Box::pin(async move {
            let mut attrs = HashMap::new();
            if id.resource_type == "acm.Certificate" {
                attrs.insert(
                    "status".to_string(),
                    Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".into())),
                );
            }
            attrs.insert(
                "id".to_string(),
                Value::Concrete(ConcreteValue::String("id-123".into())),
            );
            let identifier =
                AppliedIdentifier::new("id-123").expect("fixture identifier is non-empty");
            Ok(attempt
                .issued(
                    identifier,
                    carina_core::provider::Sent::AllRequestAttributes,
                )
                .succeeded(attrs))
        })
    }

    fn resume_create(
        &self,
        _request: carina_core::provider::ResumeCreateRequest,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
    > {
        Box::pin(async { panic!("unexpected resume_create") })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &carina_core::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        Box::pin(async move {
            Ok(UpdateOutcome::success(
                AppliedState::new(id, identifier, HashMap::new())
                    .expect("provider input identifier is non-empty"),
            ))
        })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

struct CollectingObserver {
    failures: Mutex<Vec<String>>,
}

impl ExecutionObserver for CollectingObserver {
    fn on_event(&self, event: &carina_core::executor::ExecutionEvent) {
        if let carina_core::executor::ExecutionEvent::EffectInterrupted { effect, .. } = event {
            self.failures
                .lock()
                .unwrap()
                .push(format!("effect interrupted: {}", effect.resource_id()));
            return;
        }
        if let carina_core::executor::ExecutionEvent::EffectFailed { failure, .. } = event {
            self.failures.lock().unwrap().push(failure.to_string());
        }
    }
}

#[tokio::test]
async fn module_wait_binding_survives_expansion_and_synchronizes_downstream() {
    let mut caller = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    caller.push("tests/fixtures/wait/module_wait_downstream/caller");

    let parsed = parse_directory(&caller, &ProviderContext::default())
        .expect("parse_directory should succeed for the caller fixture");

    let parsed = with_type_check_session(&ProviderContext::default(), |session| {
        resolve_modules(parsed, &caller, session).expect("module resolution should succeed")
    });
    let parsed = parsed
        .into_expanded(&carina_core::schema::SchemaRegistry::new())
        .expect("expanded module fixture contains no parser placeholders");

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
        wb.binding.as_str(),
        "r.cert_issued",
        "wait binding must be instance-prefixed (r.cert_issued)"
    );
    assert_eq!(
        wb.target.as_str(),
        "r.cert",
        "wait target must be instance-prefixed (r.cert)"
    );
    assert_eq!(
        wb.until_predicate.root.as_str(),
        "r.cert",
        "until predicate LHS root must be instance-prefixed; got {:?}",
        wb.until_predicate.root
    );
    assert!(
        wb.depends_on
            .iter()
            .any(|dependency| dependency.binding_reference().as_str() == "r.validation_record"),
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
        carina_core::deps::get_resource_value_ref_dependencies(carina_core::parser::ResourceRef::<
            carina_core::resource::ModuleSource,
            carina_core::resource::NoPlaceholder,
        >::Resource(dist))
        .iter()
        .any(|dependency| dependency.as_str() == "r.cert_issued"),
        "Distribution must depend on the prefixed wait binding \
         `r.cert_issued`; deps were {:?}",
        carina_core::deps::get_resource_value_ref_dependencies(carina_core::parser::ResourceRef::<
            carina_core::resource::ModuleSource,
            carina_core::resource::NoPlaceholder,
        >::Resource(dist))
    );

    // ---- Apply pipeline (mirrors `carina apply`): the Distribution
    // must not fail / be skipped — it must wait for `r.cert_issued`.
    let sorted_resources =
        sort_resources_by_dependencies(&parsed.resources).expect("topological sort should succeed");

    let current_states: HashMap<ResourceId, State> = HashMap::new();
    let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();

    let mut resources_for_plan = sorted_resources.clone();
    let registry = fixture_schemas(&resources_for_plan);
    let provider_types = carina_core::provider::loaded_provider_snapshot_from_factories(
        std::sync::Arc::new(Vec::new()),
        registry.clone(),
    );
    let wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();
    {
        let bindings = carina_core::binding_index::ResolvedBindings::pre_apply_live(
            carina_core::binding_index::LivePreApplyInputs {
                managed: &resources_for_plan.clone(),
                compositions: &[],
                managed_current: &plan_input_current(
                    current_states.clone(),
                    &carina_core::schema::SchemaRegistry::new(),
                    &[],
                ),
                data_source_values: &DataSourceValues::empty(),
                remote_bindings: &remote_bindings,
                wait_aliases: &wait_aliases,
            },
        );
        with_type_check_session(provider_types.provider_context(), |session| {
            resolve_refs_with_state_and_remote(&mut resources_for_plan, &bindings, session)
        })
    }
    .expect("resolve_refs should succeed");

    let provider = fixture_provider(&resources_for_plan);
    let (resolved_resources_for_plan, resolved_waits) =
        resolve_plan_graph(parsed.clone(), &resources_for_plan, &registry);
    let plan = create_plan(
        &resolved_resources_for_plan,
        &[],
        &provider,
        carina_core::differ::PlanPriorStates::new(
            &plan_input_current(
                current_states.clone(),
                &carina_core::schema::SchemaRegistry::new(),
                &[],
            ),
            &carina_core::differ::Orphans::new(),
        ),
        &HashMap::new(),
        &registry,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &resolved_waits,
        &carina_core::references::prepare_empty_for_tests(),
    )
    .expect("test provider hints should succeed");

    assert!(
        plan.effects().iter().any(|e| matches!(
            e,
            carina_core::effect::Effect::Wait { id, .. } if id.identity_str() == "r.cert_issued"
        )),
        "create_plan must emit Effect::Wait for the prefixed wait binding"
    );

    let unresolved_resources: HashMap<ResolvedResourceId, _> = sorted_resources
        .iter()
        .map(|r| {
            (
                ResolvedResourceId::new(r.id.clone()),
                UnresolvedResource::from_pre_resolve(r.clone()),
            )
        })
        .collect();

    let observer = CollectingObserver {
        failures: Mutex::new(Vec::new()),
    };
    let input = LiveExecutionInput {
        provider_context: carina_core::parser::ProviderContext::clone(
            provider_types.provider_context(),
        ),
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved_resources,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        managed_current: managed_current(current_states),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &registry,
        parallelism: carina_core::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: carina_core::executor::ExecutionCheckpointSink::new(drop),
    };

    let result = completed_result(
        execute_live_plan(
            &provider,
            input,
            &observer,
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await,
    );

    let failures = observer.failures.lock().unwrap().clone();
    assert_eq!(
        result.failure_count(),
        0,
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

    let parsed = parse_directory(&root, &ProviderContext::default())
        .expect("parse_directory should succeed for the nested root fixture");

    let parsed = with_type_check_session(&ProviderContext::default(), |session| {
        resolve_modules(parsed, &root, session).expect("nested module resolution should succeed")
    });
    let parsed = parsed
        .into_expanded(&carina_core::schema::SchemaRegistry::new())
        .expect("expanded nested module fixture contains no parser placeholders");

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
    assert_eq!(wb.binding.as_str(), "o.c.cert_issued");
    assert_eq!(wb.target.as_str(), "o.c.cert");
    assert_eq!(
        wb.until_predicate.root.as_str(),
        "o.c.cert",
        "until LHS root must be doubly-prefixed; got {:?}",
        wb.until_predicate.root
    );

    // The inner Distribution's nested ref was doubly-rewritten to match.
    let dist = parsed
        .resources
        .iter()
        .find(|r| r.id.resource_type == "cloudfront.Distribution")
        .expect("expanded Distribution must be present");
    assert!(
        carina_core::deps::get_resource_value_ref_dependencies(carina_core::parser::ResourceRef::<
            carina_core::resource::ModuleSource,
            carina_core::resource::NoPlaceholder,
        >::Resource(dist))
        .iter()
        .any(|dependency| dependency.as_str() == "o.c.cert_issued"),
        "Distribution must depend on the doubly-prefixed wait binding \
         `o.c.cert_issued`; deps were {:?}",
        carina_core::deps::get_resource_value_ref_dependencies(carina_core::parser::ResourceRef::<
            carina_core::resource::ModuleSource,
            carina_core::resource::NoPlaceholder,
        >::Resource(dist))
    );

    // End-to-end apply: the Distribution must wait, not fail/skip.
    let sorted_resources =
        sort_resources_by_dependencies(&parsed.resources).expect("topological sort should succeed");
    let current_states: HashMap<ResourceId, State> = HashMap::new();
    let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();

    let mut resources_for_plan = sorted_resources.clone();
    let registry = fixture_schemas(&resources_for_plan);
    let provider_types = carina_core::provider::loaded_provider_snapshot_from_factories(
        std::sync::Arc::new(Vec::new()),
        registry.clone(),
    );
    let wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();
    {
        let bindings = carina_core::binding_index::ResolvedBindings::pre_apply_live(
            carina_core::binding_index::LivePreApplyInputs {
                managed: &resources_for_plan.clone(),
                compositions: &[],
                managed_current: &plan_input_current(
                    current_states.clone(),
                    &carina_core::schema::SchemaRegistry::new(),
                    &[],
                ),
                data_source_values: &DataSourceValues::empty(),
                remote_bindings: &remote_bindings,
                wait_aliases: &wait_aliases,
            },
        );
        with_type_check_session(provider_types.provider_context(), |session| {
            resolve_refs_with_state_and_remote(&mut resources_for_plan, &bindings, session)
        })
    }
    .expect("resolve_refs should succeed");

    let provider = fixture_provider(&resources_for_plan);
    let (resolved_resources_for_plan, resolved_waits) =
        resolve_plan_graph(parsed.clone(), &resources_for_plan, &registry);
    let plan = create_plan(
        &resolved_resources_for_plan,
        &[],
        &provider,
        carina_core::differ::PlanPriorStates::new(
            &plan_input_current(
                current_states.clone(),
                &carina_core::schema::SchemaRegistry::new(),
                &[],
            ),
            &carina_core::differ::Orphans::new(),
        ),
        &HashMap::new(),
        &registry,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &resolved_waits,
        &carina_core::references::prepare_empty_for_tests(),
    )
    .expect("test provider hints should succeed");
    assert!(
        plan.effects().iter().any(|e| matches!(
            e,
            carina_core::effect::Effect::Wait { id, .. } if id.identity_str() == "o.c.cert_issued"
        )),
        "create_plan must emit Effect::Wait for the doubly-prefixed binding"
    );

    let unresolved_resources: HashMap<ResolvedResourceId, _> = sorted_resources
        .iter()
        .map(|r| {
            (
                ResolvedResourceId::new(r.id.clone()),
                UnresolvedResource::from_pre_resolve(r.clone()),
            )
        })
        .collect();

    let observer = CollectingObserver {
        failures: Mutex::new(Vec::new()),
    };
    let input = LiveExecutionInput {
        provider_context: carina_core::parser::ProviderContext::clone(
            provider_types.provider_context(),
        ),
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved_resources,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings: carina_core::binding_index::ResolvedBindings::default(),
        managed_current: managed_current(current_states),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &registry,
        parallelism: carina_core::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: carina_core::executor::ExecutionCheckpointSink::new(drop),
    };

    let result = completed_result(
        execute_live_plan(
            &provider,
            input,
            &observer,
            carina_core::shutdown::ShutdownToken::running(),
        )
        .await,
    );
    let failures = observer.failures.lock().unwrap().clone();
    assert_eq!(
        result.failure_count(),
        0,
        "nested-module wait must synchronize the Distribution. Failures: {failures:?}"
    );
    assert_eq!(
        result.skip_count, 0,
        "no effect should be skipped. Failures: {failures:?}"
    );
}

/// carina#3085 repro (design Test plan item 4): the Distribution's
/// `acm_certificate_arn = cert_issued.certificate_arn` must **resolve
/// through the wait binding to the target `cert`'s value** during the
/// real `resolve_refs_*` pipeline, so it no longer renders as a
/// never-converging phantom diff (`… → r.cert_issued.certificate_arn`)
/// — AND the `Effect::Wait` dependency edge must still be emitted. Both
/// asserted together so a future change cannot fix the value half by
/// breaking the dependency half (the two-faced invariant).
///
/// Mirrors the `carina-rs/infra` registry usecase that produced the
/// reported phantom. Uses the directory fixture (multi-file caller +
/// module), not a single-string unit test, per the repo's
/// directory-scoped rule.
#[tokio::test]
async fn carina3085_distribution_wait_ref_resolves_no_phantom_via_real_pipeline() {
    let mut caller = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    caller.push("tests/fixtures/wait/module_wait_downstream/caller");

    let parsed = parse_directory(&caller, &ProviderContext::default())
        .expect("parse_directory should succeed");
    let parsed = with_type_check_session(&ProviderContext::default(), |session| {
        resolve_modules(parsed, &caller, session).expect("module resolution should succeed")
    });
    let parsed = parsed
        .into_expanded(&carina_core::schema::SchemaRegistry::new())
        .expect("expanded module fixture contains no parser placeholders");

    let sorted_resources =
        sort_resources_by_dependencies(&parsed.resources).expect("topological sort");

    // State holds the resolved ARN for the wait target `r.cert` — the
    // value `cert_issued.certificate_arn` must passthrough to.
    let cert = sorted_resources
        .iter()
        .find(|r| r.id.resource_type == "acm.Certificate")
        .expect("expanded Certificate must be present");
    let arn = "arn:aws:acm:us-east-1:151116838382:certificate/3fc2dbff";
    let mut cert_attrs = HashMap::new();
    cert_attrs.insert(
        "certificate_arn".to_string(),
        Value::Concrete(ConcreteValue::String(arn.to_string().into())),
    );
    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    current_states.insert(
        cert.id.clone(),
        State::existing(cert.id.clone(), cert_attrs),
    );
    let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();

    let wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();

    let mut resources_for_plan = sorted_resources.clone();
    let registry = fixture_schemas(&resources_for_plan);
    let provider_types = carina_core::provider::loaded_provider_snapshot_from_factories(
        std::sync::Arc::new(Vec::new()),
        registry.clone(),
    );
    {
        let bindings = carina_core::binding_index::ResolvedBindings::pre_apply_live(
            carina_core::binding_index::LivePreApplyInputs {
                managed: &resources_for_plan.clone(),
                compositions: &[],
                managed_current: &plan_input_current(
                    current_states.clone(),
                    &carina_core::schema::SchemaRegistry::new(),
                    &[],
                ),
                data_source_values: &DataSourceValues::empty(),
                remote_bindings: &remote_bindings,
                wait_aliases: &wait_aliases,
            },
        );
        with_type_check_session(provider_types.provider_context(), |session| {
            resolve_refs_with_state_and_remote(&mut resources_for_plan, &bindings, session)
        })
    }
    .expect("resolve_refs should succeed");

    // ---- The phantom is gone: the Distribution's nested
    // `acm_certificate_arn` is the *resolved ARN string*, not a
    // surviving `ResourceRef` to `r.cert_issued.certificate_arn`.
    let dist = resources_for_plan
        .iter()
        .find(|r| r.id.resource_type == "cloudfront.Distribution")
        .expect("Distribution must be present");
    let dc = dist
        .attributes
        .get("distribution_config")
        .expect("distribution_config present");
    let arn_value = match dc {
        Value::Concrete(ConcreteValue::Map(m)) => m
            .get("viewer_certificate")
            .and_then(|vc| match vc {
                Value::Concrete(ConcreteValue::Map(vcm)) => vcm.get("acm_certificate_arn"),
                _ => None,
            })
            .expect("viewer_certificate.acm_certificate_arn present"),
        other => panic!("distribution_config must be a Map, got {other:?}"),
    };
    assert_eq!(
        arn_value,
        &Value::Concrete(ConcreteValue::String(arn.to_string().into())),
        "carina#3085: acm_certificate_arn must resolve through the wait \
         binding to cert's ARN, not stay an unresolved ResourceRef \
         (got {arn_value:?})"
    );

    // ---- The dependency edge is intact: Effect::Wait still emitted.
    let provider = fixture_provider(&resources_for_plan);
    let (resolved_resources_for_plan, resolved_waits) =
        resolve_plan_graph(parsed, &resources_for_plan, &registry);
    let plan = create_plan(
        &resolved_resources_for_plan,
        &[],
        &provider,
        carina_core::differ::PlanPriorStates::new(
            &plan_input_current(
                current_states.clone(),
                &carina_core::schema::SchemaRegistry::new(),
                &[],
            ),
            &carina_core::differ::Orphans::new(),
        ),
        &HashMap::new(),
        &registry,
        &empty_lifted_saved_attrs(),
        &HashMap::new(),
        &resolved_waits,
        &carina_core::references::prepare_empty_for_tests(),
    )
    .expect("test provider hints should succeed");
    assert!(
        plan.effects().iter().any(|e| matches!(
            e,
            carina_core::effect::Effect::Wait { id, .. } if id.identity_str() == "r.cert_issued"
        )),
        "the value-layer alias fix must NOT remove the Effect::Wait \
         dependency edge — both halves of the wait binding must hold"
    );
}
