use super::*;

macro_rules! unexpected_resume_create {
    () => {
        fn resume_create(
            &self,
            _request: crate::provider::ResumeCreateRequest,
        ) -> BoxFuture<'_, Result<crate::provider::ResumeOutcome, crate::provider::ResumeFailure>> {
            Box::pin(async { panic!("unexpected resume_create") })
        }
    };
}
use crate::data_source_value::{DataSourceValue, DataSourceValues, ExecutableDataSourceValues};
use crate::effect::deps::{
    ScheduleInputs, build_effect_dependency_analysis as build_dependency_analysis,
};
use crate::effect::{
    DeferredReplaceDelete, DeferredReplacePayload, DeletedInstanceKey, EffectGeneration,
    NonEmptyDeletes, ReleasedListKey, ReleasedListKeys,
};
use crate::plan::Plan;
use crate::provider::{
    AppliedIdentifier, AppliedState, BoxFuture, CreateRequest, DeleteRequest, NoopNormalizer,
    ProviderError, ProviderReadyDataSource, ProviderResult, ReadOutcome, ReadRequest,
    UpdateRequest, fixture_read_outcome, fixture_read_outcome_for_identifier,
};
use crate::resource::{
    AbsoluteBindingAddress, ExpandedAccessPath as AccessPath,
    ExpandedConcreteValue as ConcreteValue, ExpandedDataSource as DataSource,
    ExpandedDeferredValue as DeferredValue, ExpandedDirectives as Directives,
    ExpandedResource as Resource, ExpandedUnknownReason as UnknownReason, ExpandedValue as Value,
    PathSegment, RelativeBindingName, ResolvedDataSource, ResolvedResource, ResourceIdentity,
    RootScope,
};
use crate::value::SerializationError;
use parallel::build_dependency_levels;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{Mutex as AsyncMutex, Notify};

use crate::shutdown::ShutdownToken;
use crate::shutdown::testing::{TestShutdownTrigger as ShutdownTrigger, shutdown_channel};

fn uncancelled_shutdown() -> ShutdownToken {
    ShutdownToken::running()
}

fn absolute_binding(binding: &str) -> AbsoluteBindingAddress {
    RootScope::new().resolve(RelativeBindingName::new(binding))
}

#[test]
fn executor_test_fixtures_do_not_use_raw_cancellation_tokens() {
    let retired_type = ["tokio_util::sync::", "CancellationToken"].concat();
    assert!(
        !include_str!("tests.rs").contains(&retired_type),
        "executor fixtures must exercise the full ShutdownToken state machine"
    );
}

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

fn managed_wait_target(id: ResourceId) -> crate::wait::ManagedWaitTarget {
    crate::wait::ManagedWaitTarget::new(&ResolvedResource::from_id(
        crate::resource::ResolvedResourceId::new(id),
    ))
}

fn resolved_data_source(resource: DataSource) -> ResolvedDataSource {
    ResolvedDataSource::new(resource)
}

fn test_data_source_values(
    data_sources: &[DataSource],
    states: &HashMap<ResourceId, State>,
) -> DataSourceValues {
    DataSourceValues::from_data_sources(data_sources, |data_source| {
        states.get(&data_source.id).map_or_else(
            || DataSourceValue::deferred_to_apply(Vec::new()),
            |state| DataSourceValue::read(state.clone()),
        )
    })
}

fn executable_data_source_values(
    data_sources: &[DataSource],
    states: &HashMap<ResourceId, State>,
) -> ExecutableDataSourceValues {
    test_data_source_values(data_sources, states)
        .into_executable()
        .expect("test data-source values must be executable")
}

fn executable_deferred_data_source_values(
    data_sources: &[DataSource],
) -> ExecutableDataSourceValues {
    DataSourceValues::from_data_sources(data_sources, |data_source| {
        DataSourceValue::deferred_to_apply(unresolved_data_source_inputs(data_source))
    })
    .into_executable()
    .expect("deferred test data-source values must be executable")
}

fn create_effect(resource: Resource) -> Effect {
    Effect::Create(resolved(resource))
}

fn for_value_access(fields: &[&str]) -> Value {
    fields.iter().fold(
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValue)),
        |base, field| {
            crate::resource::apply_postfix_access(
                base,
                PathSegment::Field {
                    name: (*field).to_string(),
                },
                0,
                crate::resource::AccessMissPolicy::Error,
            )
            .unwrap()
        },
    )
}

fn validation_deferred_for_expression() -> crate::parser::ExpandedDeferredForExpression {
    let mut template_resource = Resource::new("test", "validation_records");
    template_resource.binding = Some(absolute_binding("validation_records"));
    template_resource
        .dependency_bindings
        .insert(absolute_binding("cert"));
    template_resource.set_attr("name", for_value_access(&["resource_record", "name"]));
    template_resource.set_attr("value", for_value_access(&["resource_record", "value"]));

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
        target: crate::parser::DeferredForTarget::Bound {
            binding: absolute_binding("validation_records"),
        },
        iterable: Value::root_resource_ref("cert", "domain_validation_options", Vec::new()),
        binding: crate::parser::ForBinding::Simple("opt".to_string()),
        template_resource,
    }
}

fn anonymous_deferred_collision_fixture() -> (
    crate::parser::ExpandedDeferredForExpression,
    ResolvedBindings,
    SchemaRegistry,
    Resource,
) {
    let parsed = crate::parser::parse_file(
        "let upstream = upstream_state { source = '../upstream' }\n\
         for value in upstream.values {\n\
           mock.test.Child { identity_input = value }\n\
         }\n",
        std::path::Path::new("main.crn"),
        &crate::parser::ProviderContext::default(),
    )
    .expect("unbound deferred loop parses");
    let parsed = crate::module_resolver::resolve_modules_providerless(
        crate::config_loader::ParsedRootDirectory::from_parsed_for_test(parsed),
        std::path::Path::new("."),
    )
    .expect("deferred fixture crosses the module-resolution seam");
    let template = parsed.deferred_for_expressions()[0].clone();
    let mut bindings = ResolvedBindings::default();
    bindings.set_with_completeness(
        "upstream",
        HashMap::from([(
            "values".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("stable".into()),
            )])),
        )]),
        crate::binding_index::BindingValueSource::Upstream,
        crate::binding_index::BindingCompleteness::Complete,
    );
    let schema = crate::schema::ResourceSchema::new("test.Child").attribute(
        crate::schema::AttributeSchema::optional(
            "identity_input",
            crate::schema::AttributeType::string(),
        )
        .create_only()
        .build(),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        schema.validate().expect("schema fixture must be valid"),
    );
    let children = deferred_dispatch::materialize_deferred_create(
        &template,
        &bindings,
        &deferred_dispatch::DeferredIdentityInputs {
            provider_configs: &[],
            factories: &[],
            schemas: &schemas,
        },
    )
    .expect("fixture child materializes");
    let [Effect::Create(child)] = children.as_slice() else {
        panic!("fixture must produce one create");
    };

    (template, bindings, schemas, child.as_inner().clone())
}

#[test]
fn exported_plan_scheduler_edges_match_the_real_executor_scheduler() {
    fn origins(resources: &[Resource]) -> HashMap<ResourceId, UnresolvedResource> {
        resources
            .iter()
            .map(|resource| {
                (
                    resource.id.clone(),
                    UnresolvedResource::from_pre_resolve(resource.clone()),
                )
            })
            .collect()
    }

    fn assert_matches_executor(
        case: &str,
        effects: Vec<Effect>,
        unresolved: HashMap<ResourceId, UnresolvedResource>,
        expected: BTreeSet<(PlanSchedulerEndpoint, PlanSchedulerEndpoint)>,
    ) -> BTreeSet<(PlanSchedulerEndpoint, PlanSchedulerEndpoint)> {
        let borrowed: HashMap<_, _> = unresolved
            .iter()
            .map(|(id, resource)| (id.clone(), resource))
            .collect();
        let actual = build_plan_scheduler_dependency_edges(&effects, &borrowed, &[]);

        assert_eq!(actual, expected, "scheduler-edge export drifted for {case}");
        actual
    }

    fn primary(effect_index: usize) -> PlanSchedulerEndpoint {
        PlanSchedulerEndpoint {
            effect_index,
            phase: PlanSchedulerPhase::Primary,
        }
    }

    fn teardown(effect_index: usize) -> PlanSchedulerEndpoint {
        PlanSchedulerEndpoint {
            effect_index,
            phase: PlanSchedulerPhase::DeferredReplaceTeardown,
        }
    }

    let a = make_resource("plain_a", &[]);
    let b = make_resource("plain_b", &["plain_a"]);
    let c = make_resource("plain_c", &["plain_b"]);
    assert_matches_executor(
        "plain create/update/delete chain",
        vec![
            create_effect(a.clone()),
            Effect::Update {
                from: Box::new(State::existing(b.id.clone(), HashMap::new())),
                identifier: crate::effect::fixture_applied_identifier("plain-b"),
                to: ResolvedResource::new(b.clone()),
                changed_attributes: vec!["value".to_string()],
            },
            Effect::Delete {
                id: crate::resource::ResolvedResourceId::new(c.id.clone()),
                identifier: crate::effect::fixture_applied_identifier("plain-c"),
                generation: EffectGeneration::Current,
                directives: Directives::default(),
                binding: c.binding.clone(),
                dependencies: c.dependency_bindings.iter().cloned().collect(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            },
        ],
        origins(&[a, b, c]),
        BTreeSet::from([(primary(0), primary(1))]),
    );

    let replacement = make_resource("replacement", &[]);
    assert_matches_executor(
        "replacement pair",
        vec![
            create_effect(replacement.clone()),
            Effect::Delete {
                id: crate::resource::ResolvedResourceId::new(replacement.id.clone()),
                identifier: crate::effect::fixture_applied_identifier("replacement-old"),
                generation: EffectGeneration::Current,
                directives: Directives::default(),
                binding: replacement.binding.clone(),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            },
        ],
        origins(std::slice::from_ref(&replacement)),
        BTreeSet::from([(primary(0), primary(1))]),
    );

    let cert = make_resource("cert", &[]);
    let deferred_replace_edges = assert_matches_executor(
        "DeferredReplace with absorbed deletes",
        vec![
            create_effect(cert.clone()),
            Effect::DeferredReplace(Box::new(DeferredReplacePayload {
                deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                    id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                        "test",
                        "validation_records[0]",
                    )),
                    identifier: crate::effect::fixture_applied_identifier("validation-old"),
                    directives: Directives::default(),
                    binding: Some(absolute_binding("validation_records[0]")),
                    dependencies: HashSet::new(),
                    explicit_dependencies: HashSet::new(),
                    blocked_by_updates: HashSet::new(),
                }])
                .expect("fixture has one delete"),
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "__deferred_for",
                    "validation_records",
                )),
                template: Box::new(validation_deferred_for_expression()),
            })),
        ],
        origins(std::slice::from_ref(&cert)),
        BTreeSet::from([(primary(0), primary(1)), (primary(1), teardown(1))]),
    );
    assert!(
        deferred_replace_edges.contains(&(primary(1), teardown(1))),
        "DeferredReplace primary phase must precede its absorbed-delete teardown phase"
    );

    use crate::wait::predicate::{AttrPath, WaitPredicate};
    let wait_target = make_resource("wait_target", &[]);
    let mut wait_consumer = make_resource("wait_consumer", &["wait_ready"]);
    wait_consumer
        .dependency_bindings
        .insert(absolute_binding("wait_ready"));
    assert_matches_executor(
        "wait",
        vec![
            create_effect(wait_target.clone()),
            Effect::Wait {
                identity: crate::resource::absolute_binding_address_for_test("wait_ready"),
                target_id: managed_wait_target(wait_target.id.clone()),
                until: WaitPredicate::equals(
                    AttrPath::single("status"),
                    Value::Concrete(ConcreteValue::String("ready".into())),
                )
                .unwrap(),
                until_surface: "wait_target.status == ready".to_string(),
                timeout: std::time::Duration::from_secs(60),
                interval: std::time::Duration::from_secs(1),
                explicit_dependencies: HashSet::new(),
            },
            create_effect(wait_consumer.clone()),
        ],
        origins(&[wait_target, wait_consumer]),
        BTreeSet::from([(primary(0), primary(1)), (primary(1), primary(2))]),
    );
}

// -----------------------------------------------------------------------
// Mock Provider
// -----------------------------------------------------------------------

struct MockProvider {
    create_results: Mutex<Vec<ProviderResult<crate::provider::CreateOutcome>>>,
    classified_create_results: Mutex<Vec<ClassifiedCreateFixture>>,
    resume_results: Mutex<Vec<ResumeFixture>>,
    delete_results: Mutex<Vec<ProviderResult<()>>>,
    update_results: Mutex<Vec<ProviderResult<crate::provider::UpdateOutcome>>>,
    read_results: Mutex<Vec<ProviderResult<State>>>,
    /// Records calls in order: ("create"|"delete"|"update"|"read", resource_id_string)
    call_log: Arc<Mutex<Vec<(String, String)>>>,
    /// Resources passed in to `create()` in call order — lets a test
    /// assert that the executor handed the provider a fully-resolved
    /// resource (no remaining `Value::Deferred(ResourceRef)` etc.).
    create_resources: Arc<Mutex<Vec<crate::resource::ProviderResource>>>,
    /// `UpdateRequest`s passed in to `update()` in call order — lets a
    /// test assert the patch carries re-normalized attribute values.
    update_requests: Arc<Mutex<Vec<UpdateRequest>>>,
    /// Data sources passed to `read_data_source()` in call order.
    data_source_reads: Arc<Mutex<Vec<crate::resource::ProviderDataSource>>>,
}

enum ClassifiedCreateFixture {
    OutcomeUnknown,
    Created,
    CreatedNotFound,
    CreatedReadFailed,
    InFlight,
}

enum ResumeFixture {
    Resumed,
    CreatedFound,
    CreatedNotFound,
    CreatedReadFailed,
    Ended,
    ResentInFlightWithInvalidSent,
}

impl MockProvider {
    fn new() -> Self {
        Self {
            create_results: Mutex::new(Vec::new()),
            classified_create_results: Mutex::new(Vec::new()),
            resume_results: Mutex::new(Vec::new()),
            delete_results: Mutex::new(Vec::new()),
            update_results: Mutex::new(Vec::new()),
            read_results: Mutex::new(Vec::new()),
            call_log: Arc::new(Mutex::new(Vec::new())),
            create_resources: Arc::new(Mutex::new(Vec::new())),
            update_requests: Arc::new(Mutex::new(Vec::new())),
            data_source_reads: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn push_create(&self, result: ProviderResult<State>) {
        self.create_results
            .lock()
            .unwrap()
            .push(result.map(|state| {
                crate::provider::CreateOutcome::fixture_success(applied_state_for_test(state))
            }));
    }

    fn push_create_outcome(&self, result: ProviderResult<crate::provider::CreateOutcome>) {
        self.create_results.lock().unwrap().push(result);
    }

    fn push_classified_create(&self, result: ClassifiedCreateFixture) {
        self.classified_create_results.lock().unwrap().push(result);
    }

    fn push_resume(&self, result: ResumeFixture) {
        self.resume_results.lock().unwrap().push(result);
    }

    fn push_delete(&self, result: ProviderResult<()>) {
        self.delete_results.lock().unwrap().push(result);
    }

    fn push_update(&self, result: ProviderResult<State>) {
        self.update_results
            .lock()
            .unwrap()
            .push(result.map(|state| crate::provider::UpdateOutcome::Success {
                state: applied_state_for_test(state),
            }));
    }

    fn push_read(&self, result: ProviderResult<State>) {
        self.read_results.lock().unwrap().push(result);
    }

    fn calls(&self) -> Vec<(String, String)> {
        self.call_log.lock().unwrap().clone()
    }

    fn captured_create_resources(&self) -> Vec<crate::resource::ProviderResource> {
        self.create_resources.lock().unwrap().clone()
    }

    fn captured_update_requests(&self) -> Vec<UpdateRequest> {
        self.update_requests.lock().unwrap().clone()
    }

    fn captured_data_source_reads(&self) -> Vec<crate::resource::ProviderDataSource> {
        self.data_source_reads.lock().unwrap().clone()
    }
}

fn applied_state_for_test(state: State) -> AppliedState {
    assert!(
        state.exists,
        "mutation fixtures must describe an existing resource"
    );
    let identifier = state
        .identifier
        .expect("mutation fixtures must carry a provider identifier");
    AppliedState::new(state.id, identifier, state.attributes)
        .expect("mutation fixture identifier must be non-empty")
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
        let id_str = id.to_string();
        self.call_log
            .lock()
            .unwrap()
            .push(("read".to_string(), id_str));
        let result = self.read_results.lock().unwrap().remove(0);
        let identifier = identifier.clone();
        Box::pin(async move {
            result.map(|state| fixture_read_outcome_for_identifier(state, &identifier))
        })
    }

    fn read_data_source(
        &self,
        resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        self.data_source_reads
            .lock()
            .unwrap()
            .push(resource.as_data_source().clone());
        self.call_log
            .lock()
            .unwrap()
            .push(("read".to_string(), resource.id.to_string()));
        let result = self.read_results.lock().unwrap().remove(0);
        Box::pin(async move { result })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: crate::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>> {
        let id_str = id.to_string();
        self.call_log
            .lock()
            .unwrap()
            .push(("create".to_string(), id_str));
        self.create_resources
            .lock()
            .unwrap()
            .push(request.resource().as_resource().clone());
        let classified = {
            let mut results = self.classified_create_results.lock().unwrap();
            (!results.is_empty()).then(|| results.remove(0))
        };
        if let Some(classified) = classified {
            let id = id.clone();
            return Box::pin(async move {
                match classified {
                    ClassifiedCreateFixture::OutcomeUnknown => Err(attempt.outcome_unknown(
                        crate::provider::EgressSent::for_test(),
                        ProviderError::internal("create outcome is unknown"),
                    )),
                    ClassifiedCreateFixture::Created => {
                        let identifier = AppliedIdentifier::new("created-id")
                            .expect("fixture identifier is non-empty");
                        let observed = AppliedState::from_identifier(
                            id.clone(),
                            identifier.clone(),
                            HashMap::from([(
                                "id".to_string(),
                                Value::Concrete(ConcreteValue::String("created-id".into())),
                            )]),
                        );
                        Err(attempt
                            .issued(identifier, crate::provider::Sent::AllRequestAttributes)
                            .failed(
                                crate::provider::Observation::Read(ReadOutcome::found(observed)),
                                ProviderError::api_error("post-create fixture failed"),
                            ))
                    }
                    ClassifiedCreateFixture::CreatedNotFound => {
                        let identifier = AppliedIdentifier::new("created-id")
                            .expect("fixture identifier is non-empty");
                        Err(attempt
                            .issued(identifier, crate::provider::Sent::AllRequestAttributes)
                            .failed(
                                crate::provider::Observation::Read(ReadOutcome::not_found(id)),
                                ProviderError::api_error("post-create fixture failed"),
                            ))
                    }
                    ClassifiedCreateFixture::CreatedReadFailed => {
                        let identifier = AppliedIdentifier::new("created-id")
                            .expect("fixture identifier is non-empty");
                        Err(attempt
                            .issued(identifier, crate::provider::Sent::AllRequestAttributes)
                            .failed(
                                crate::provider::Observation::ReadFailed(ProviderError::api_error(
                                    "read-back access denied",
                                )),
                                ProviderError::api_error("post-create fixture failed"),
                            ))
                    }
                    ClassifiedCreateFixture::InFlight => {
                        let pending = attempt.pending(
                            crate::provider::PendingToken::new("fixture-create-token"),
                            crate::provider::Sent::AllRequestAttributes,
                            1,
                        );
                        Err(crate::provider::InFlightCreate::from_pending(pending)
                            .into_create_failure(ProviderError::timeout(
                                "create fixture remains in flight",
                            )))
                    }
                }
            });
        }
        let result = self.create_results.lock().unwrap().remove(0);
        Box::pin(async move { result.map_err(|error| attempt.untracked(error)) })
    }

    fn resume_create(
        &self,
        request: crate::provider::ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<crate::provider::ResumeOutcome, crate::provider::ResumeFailure>> {
        let result = {
            let mut results = self.resume_results.lock().unwrap();
            (!results.is_empty()).then(|| results.remove(0))
        };
        Box::pin(async move {
            match result.expect("unexpected resume_create") {
                ResumeFixture::Resumed => {
                    let (resume, _create_request) = request.into_parts();
                    let crate::provider::Resume::Issued(resumed) = resume else {
                        return Err(resume.preserve(ProviderError::internal(
                            "resumed fixture requires an issued continuation",
                        )));
                    };
                    let id = resumed.id().clone();
                    let identifier = resumed.identifier().clone();
                    let identifier_value = identifier.as_str().into();
                    let observed = AppliedState::from_identifier(
                        id,
                        identifier,
                        HashMap::from([(
                            "id".to_string(),
                            Value::Concrete(ConcreteValue::String(identifier_value)),
                        )]),
                    );
                    Ok(resumed.completed(observed))
                }
                ResumeFixture::CreatedFound => {
                    let (resume, _create_request) = request.into_parts();
                    let crate::provider::Resume::Issued(resumed) = resume else {
                        return Err(resume.preserve(ProviderError::internal(
                            "created-found fixture requires an issued continuation",
                        )));
                    };
                    let id = resumed.id().clone();
                    let identifier = resumed.identifier().clone();
                    let observed = AppliedState::from_identifier(
                        id,
                        identifier,
                        HashMap::from([(
                            "id".to_string(),
                            Value::Concrete(ConcreteValue::String("created-id".into())),
                        )]),
                    );
                    Err(resumed.failed(
                        crate::provider::Observation::Read(ReadOutcome::found(observed)),
                        ProviderError::api_error("resumed create fixture failed"),
                    ))
                }
                ResumeFixture::CreatedNotFound => {
                    let (resume, _create_request) = request.into_parts();
                    let crate::provider::Resume::Issued(resumed) = resume else {
                        return Err(resume.preserve(ProviderError::internal(
                            "created-not-found fixture requires an issued continuation",
                        )));
                    };
                    let id = resumed.id().clone();
                    Err(resumed.failed(
                        crate::provider::Observation::Read(ReadOutcome::not_found(id)),
                        ProviderError::api_error("resumed create fixture failed"),
                    ))
                }
                ResumeFixture::CreatedReadFailed => {
                    let (resume, _create_request) = request.into_parts();
                    let crate::provider::Resume::Issued(resumed) = resume else {
                        return Err(resume.preserve(ProviderError::internal(
                            "created-read-failed fixture requires an issued continuation",
                        )));
                    };
                    Err(resumed.failed(
                        crate::provider::Observation::ReadFailed(ProviderError::api_error(
                            "resumed read-back access denied",
                        )),
                        ProviderError::api_error("resumed create fixture failed"),
                    ))
                }
                ResumeFixture::Ended => {
                    let (resume, _create_request) = request.into_parts();
                    Err(resume.discarded(ProviderError::not_found("recorded create ended")))
                }
                ResumeFixture::ResentInFlightWithInvalidSent => {
                    let (resume, _create_request) = request.into_parts();
                    let attempt = match resume {
                        crate::provider::Resume::Unissued(attempt) => attempt.resend(),
                        crate::provider::Resume::Issued(resumed) => {
                            return Err(resumed.preserve(ProviderError::internal(
                                "invalid-sent fixture requires an unissued continuation",
                            )));
                        }
                    };
                    let pending = attempt.pending(
                        crate::provider::PendingToken::new("resent-token"),
                        crate::provider::Sent::only(["outside_request"]),
                        1,
                    );
                    Err(pending.preserve(ProviderError::timeout("resent create is pending")))
                }
            }
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
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
        _identifier: &crate::provider::AppliedIdentifier,
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

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
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
        ExecutionEvent::EffectStarted { effect, start: _ } => {
            format!("started:{}", effect.resource_id())
        }
        ExecutionEvent::EffectInterrupted { effect, progress } => {
            format!(
                "interrupted:{}:{}/{}",
                effect.resource_id(),
                progress.completed,
                progress.total
            )
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
        ExecutionEvent::WaitPolling {
            effect: _,
            observation,
            elapsed: _,
        } => {
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
        ExecutionEvent::ModuleConstraintFailed { error } => {
            format!("module_constraint_failed:{error}")
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

fn effect_failure_event<'a>(events: &'a [String], id: &ResourceId) -> &'a str {
    let prefix = format!("failed:{id}:");
    events
        .iter()
        .find(|event| event.starts_with(&prefix))
        .unwrap_or_else(|| panic!("missing EffectFailed event for {id}: {events:?}"))
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
    for_value_access(&["resource_record", "name"])
}

#[test]
fn resolved_resource_constructor_rejects_value_unknown() {
    let err = basic::resolved_resource(resource_with_attr(for_value_path_unknown()))
        .expect_err("deferred unknown must not construct a ResolvedResource");

    assert!(
        matches!(err, SerializationError::UnresolvedAccess { .. }),
        "expected UnresolvedAccess, got {err:?}"
    );
}

#[test]
fn resolved_resource_constructor_rejects_resource_ref() {
    let value = Value::Deferred(DeferredValue::ResourceRef {
        path: AccessPath::root("cert", "domain_validation_options"),
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
        matches!(err, SerializationError::UnresolvedAccess { .. }),
        "expected UnresolvedAccess, got {err:?}"
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
        Value::Concrete(ConcreteValue::String(s)) if s == "raw_dsl" => {
            Some(Value::Concrete(ConcreteValue::String("CANONICAL".into())))
        }
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
        resources: &'a mut [crate::resource::ProviderResource],
    ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
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
            Ok(())
        })
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut crate::provider::StateTransformMap,
    ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
        crate::provider::ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut crate::provider::StateTransformMap,
        _saved_attrs: &'a crate::provider::SavedAttrs,
    ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
        crate::provider::ready_noop()
    }
}

struct SecretListToScalarNormalizer;

impl crate::provider::ProviderNormalizer for SecretListToScalarNormalizer {
    fn normalize_desired<'a>(
        &'a self,
        resources: &'a mut [crate::resource::ProviderResource],
    ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
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
            Ok(())
        })
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut crate::provider::StateTransformMap,
    ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
        crate::provider::ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut crate::provider::StateTransformMap,
        _saved_attrs: &'a crate::provider::SavedAttrs,
    ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
        crate::provider::ready_noop()
    }
}

// -----------------------------------------------------------------------
// Mock ProviderFactory + shared test fixtures
// -----------------------------------------------------------------------

use crate::provider::ProviderFactory;
use crate::schema::SchemaRegistry;
use std::sync::LazyLock;

/// Minimal matching schema registry shared by tests that don't exercise the
/// canonicalize stage. `'static` so it can back `&` in `ExecutionInput`.
static TEST_SCHEMAS: LazyLock<SchemaRegistry> = LazyLock::new(|| {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "",
        (crate::schema::ResourceSchema::new("test"))
            .validate()
            .expect("schema fixture must be valid"),
    );
    registry
});

static DEFAULT_TAG_SCHEMAS: LazyLock<SchemaRegistry> = LazyLock::new(|| {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut registry = SchemaRegistry::new();
    registry.insert(
        "test",
        (ResourceSchema::new("test").attribute(
            AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    registry
});

/// Registry whose `test`-provider `sg` resource declares `subject` as
/// `Union[String, list(String)]` (the `string_or_list_of_strings`
/// shape), so the apply-path canonicalize stage (#2481/#2511) has a
/// schema to act on. carina#3063: this stage is plan pipeline stage 1
/// and must also be re-applied at apply time.
static CANON_SCHEMAS: LazyLock<SchemaRegistry> = LazyLock::new(|| {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut reg = SchemaRegistry::new();
    let schema = ResourceSchema::new("sg").attribute(
        AttributeSchema::optional(
            "subject",
            AttributeType::union(vec![
                AttributeType::string(),
                AttributeType::list(AttributeType::string()),
            ]),
        )
        .build(),
    );
    reg.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    reg
});

static AUGMENT_COMPARISON_SCHEMAS: LazyLock<SchemaRegistry> = LazyLock::new(|| {
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, StructField, enum_identity,
    };

    let mut reg = SchemaRegistry::new();
    let schema = ResourceSchema::new("a")
        .attribute(AttributeSchema::optional("description", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("size", AttributeType::float()).build())
        .attribute(
            AttributeSchema::optional("items", AttributeType::list(AttributeType::string()))
                .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "options",
                AttributeType::struct_(
                    "Options",
                    vec![
                        StructField::optional("enabled", AttributeType::bool()).build(),
                        StructField::optional("label", AttributeType::string()).build(),
                    ],
                ),
            )
            .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "mode",
                AttributeType::enum_(
                    enum_identity("Mode", Some("test")),
                    Some(vec!["AES256".to_string(), "aws:kms".to_string()]),
                    Vec::new(),
                    Some(crate::schema::legacy_validator(|_| Ok(()))),
                    None,
                ),
            )
            .build(),
        )
        .attribute(
            AttributeSchema::optional("write_only_token", AttributeType::string())
                .write_only()
                .build(),
        )
        .attribute(
            AttributeSchema::optional("create_only_write_only_token", AttributeType::string())
                .write_only()
                .create_only()
                .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "master_password",
                AttributeType::union(vec![
                    AttributeType::string(),
                    AttributeType::list(AttributeType::string()),
                ]),
            )
            .build(),
        );
    reg.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    reg
});

/// Factory with a modern enum schema. Schema resolution retains the
/// `CanonicalEnum("all")` API spelling without a provider-owned alias table.
struct AliasFactory;

fn alias_resource_schema() -> crate::schema::ResourceSchema {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, enum_identity};

    ResourceSchema::new("sg").attribute(
        AttributeSchema::optional(
            "ip_protocol",
            AttributeType::enum_(
                enum_identity("IpProtocol", Some("test.ec2.SecurityGroup")),
                Some(vec!["all".to_string(), "tcp".to_string()]),
                Vec::new(),
                None,
                None,
            ),
        )
        .build(),
    )
}

static ALIAS_SCHEMAS: LazyLock<SchemaRegistry> = LazyLock::new(|| {
    let mut registry = SchemaRegistry::new();
    registry.insert(
        "test",
        (alias_resource_schema())
            .validate()
            .expect("schema fixture must be valid"),
    );
    registry
});

impl ProviderFactory for AliasFactory {
    fn name(&self) -> &str {
        "test"
    }
    fn display_name(&self) -> &str {
        "Test"
    }
    fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(
        &self,
        _attributes: &crate::provider::CanonicalProviderConfigAttributes,
    ) -> crate::provider::ProviderResult<()> {
        Ok(())
    }
    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        String::new()
    }
    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &crate::provider::ProviderReadyConfig,
        _initialize_outcome: crate::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>> {
        unreachable!("test factory does not create providers")
    }
    fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
        crate::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
        vec![
            alias_resource_schema()
                .validate()
                .expect("alias schema must be valid"),
        ]
    }
}

// -----------------------------------------------------------------------
// Helper functions
// -----------------------------------------------------------------------

fn make_resource(binding: &str, deps: &[&str]) -> Resource {
    let mut r = Resource::new(
        "test",
        crate::resource::ResourceIdentity::try_from(binding.to_string()).unwrap(),
    );
    r.binding = Some(absolute_binding(binding));
    for dep in deps {
        r.set_attr(
            format!("ref_{}", dep),
            Value::root_resource_ref(dep.to_string(), "id".to_string(), vec![]),
        );
    }
    // Save dependency bindings as metadata (normally done by resolver)
    if !deps.is_empty() {
        r.dependency_bindings = deps.iter().map(|dep| absolute_binding(dep)).collect();
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
        Value::Concrete(ConcreteValue::String("id-123".into())),
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
        interrupted_count: 0,
        applied_states: Default::default(),
        runtime_synthesized_resources: Vec::new(),
        successfully_deleted: HashSet::new(),
        permanent_name_overrides: HashMap::new(),
        managed_current: HashMap::new(),
        data_source_states: HashMap::new(),
        bindings: ResolvedBindings::default(),
        failed_refreshes: HashSet::new(),
    }
}

fn provider_config_with_default_tags(
    tags: indexmap::IndexMap<String, Value>,
) -> crate::parser::ExpandedProviderConfig {
    crate::parser::ProviderConfig {
        name: "test".to_string(),
        attributes: indexmap::IndexMap::new(),
        default_tags: tags,
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: indexmap::IndexMap::new(),
        binding: None,
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
    guest_egress: HashSet<String>,
    started: Arc<Mutex<Vec<String>>>,
}

impl DelayedCountingProvider {
    fn new(default_delay: std::time::Duration) -> Self {
        Self {
            default_delay,
            delays: HashMap::new(),
            guest_egress: HashSet::new(),
            started: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_delay(mut self, name: &str, delay: std::time::Duration) -> Self {
        self.delays.insert(name.to_string(), delay);
        self
    }

    fn with_guest_egress(mut self, name: &str) -> Self {
        self.guest_egress.insert(name.to_string());
        self
    }

    fn started_names(&self) -> Vec<String> {
        self.started.lock().unwrap().clone()
    }

    fn delay_for(&self, id: &ResourceId) -> std::time::Duration {
        self.delays
            .get(id.identity_str().expect("resolved identity"))
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
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(async { Err(ProviderError::internal("read not used")) })
    }

    fn read_data_source(
        &self,
        _resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
    }

    fn create(
        &self,
        id: &ResourceId,
        _request: CreateRequest,
        attempt: crate::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>> {
        let id = id.clone();
        let delay = self.delay_for(&id);
        let guest_egress = self
            .guest_egress
            .contains(id.identity_str().expect("resolved identity"));
        let started = self.started.clone();
        Box::pin(async move {
            started
                .lock()
                .unwrap()
                .push(id.identity_str().expect("resolved identity").to_string());
            let guest_call = guest_egress.then(|| {
                let call = attempt.start_guest_call();
                call.egress_recorder().begin_request().dispatched();
                call
            });
            tokio::time::sleep(delay).await;
            if let Some(call) = guest_call {
                let decoded = call
                    .finish()
                    .decode((), |()| Ok(()))
                    .await
                    .expect("fixture guest return decodes");
                let (_attempt, ()) = decoded.into_parts();
            }
            Ok(crate::provider::CreateOutcome::fixture_success(
                applied_state_for_test(ok_state(&id)),
            ))
        })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        let id = id.clone();
        let started = self.started.clone();
        Box::pin(async move {
            started.lock().unwrap().push(format!(
                "update:{}",
                id.identity_str().expect("resolved identity")
            ));
            let mut attrs = HashMap::new();
            attrs.insert(
                "finalized".to_string(),
                Value::Concrete(ConcreteValue::Bool(true)),
            );
            Ok(crate::provider::UpdateOutcome::Success {
                state: AppliedState::new(id, "finalized-id", attrs)
                    .expect("fixture identifier is non-empty"),
            })
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        let started = self.started.clone();
        Box::pin(async move {
            started.lock().unwrap().push(format!(
                "delete:{}",
                id.identity_str().expect("resolved identity")
            ));
            Ok(())
        })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
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
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let id = id.clone();
        Box::pin(async move {
            let mut attrs = HashMap::new();
            attrs.insert(
                "status".to_string(),
                Value::Concrete(ConcreteValue::String("PENDING".into())),
            );
            Ok(fixture_read_outcome(
                State::existing(id, attrs).with_identifier("pending-id"),
            ))
        })
    }

    fn read_data_source(
        &self,
        _resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
    }

    fn create(
        &self,
        id: &ResourceId,
        _request: CreateRequest,
        _attempt: crate::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>> {
        let id = id.clone();
        Box::pin(async move {
            Ok(crate::provider::CreateOutcome::fixture_success(
                applied_state_for_test(ok_state(&id)),
            ))
        })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("update not used")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("delete not used")) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

struct CancelsAfterSuccesses {
    successes: AtomicUsize,
    threshold: usize,
    trigger: ShutdownTrigger,
    token: ShutdownToken,
}

impl CancelsAfterSuccesses {
    fn new(threshold: usize) -> Self {
        let (trigger, token) = shutdown_channel();
        Self {
            successes: AtomicUsize::new(0),
            threshold,
            trigger,
            token,
        }
    }

    fn token(&self) -> ShutdownToken {
        self.token.clone()
    }
}

impl ExecutionObserver for CancelsAfterSuccesses {
    fn on_event(&self, event: &ExecutionEvent) {
        if matches!(event, ExecutionEvent::EffectSucceeded { .. }) {
            let successes = self.successes.fetch_add(1, Ordering::Relaxed) + 1;
            if successes >= self.threshold {
                self.trigger.request_graceful_shutdown();
            }
        }
    }
}

struct CancelsWhenStarted {
    name: String,
    trigger: ShutdownTrigger,
    token: ShutdownToken,
    cleanup_priority: bool,
    events: Mutex<Vec<String>>,
}

impl CancelsWhenStarted {
    fn new(name: &str) -> Self {
        let (trigger, token) = shutdown_channel();
        Self {
            name: name.to_string(),
            trigger,
            token,
            cleanup_priority: false,
            events: Mutex::new(Vec::new()),
        }
    }

    fn with_cleanup_priority(name: &str) -> Self {
        let (trigger, token) = shutdown_channel();
        Self {
            name: name.to_string(),
            trigger,
            token,
            cleanup_priority: true,
            events: Mutex::new(Vec::new()),
        }
    }

    fn token(&self) -> ShutdownToken {
        self.token.clone()
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

struct CancelsWhenWaitStarted {
    binding: String,
    trigger: ShutdownTrigger,
    token: ShutdownToken,
}

impl CancelsWhenWaitStarted {
    fn new(binding: &str) -> Self {
        let (trigger, token) = shutdown_channel();
        Self {
            binding: binding.to_string(),
            trigger,
            token,
        }
    }

    fn token(&self) -> ShutdownToken {
        self.token.clone()
    }
}

impl ExecutionObserver for CancelsWhenWaitStarted {
    fn on_event(&self, event: &ExecutionEvent) {
        if let ExecutionEvent::EffectStarted {
            effect: Effect::Wait { identity, .. },
            start: _,
        } = event
            && identity.as_str() == self.binding
        {
            self.trigger.request_graceful_shutdown();
        }
    }
}

struct RecordingCancelsWhenWaitStarted {
    binding: String,
    trigger: ShutdownTrigger,
    token: ShutdownToken,
    events: Mutex<Vec<String>>,
}

impl RecordingCancelsWhenWaitStarted {
    fn new(binding: &str) -> Self {
        let (trigger, token) = shutdown_channel();
        Self {
            binding: binding.to_string(),
            trigger,
            token,
            events: Mutex::new(Vec::new()),
        }
    }

    fn token(&self) -> ShutdownToken {
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
            start: _,
        } = event
            && identity.as_str() == self.binding
        {
            self.trigger.request_graceful_shutdown();
        }
    }
}

impl ExecutionObserver for CancelsWhenStarted {
    fn on_event(&self, event: &ExecutionEvent) {
        self.events
            .lock()
            .unwrap()
            .push(format_execution_event(event));
        if let ExecutionEvent::EffectStarted { effect, start: _ } = event
            && effect
                .resource_id()
                .identity_str()
                .expect("resolved identity")
                == self.name
        {
            if self.cleanup_priority {
                self.trigger.prioritize_cleanup();
            } else {
                self.trigger.request_graceful_shutdown();
            }
        }
    }
}

struct PendingRefreshProvider;

impl Provider for PendingRefreshProvider {
    fn name(&self) -> &str {
        "pending-refresh"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(std::future::pending())
    }

    fn read_data_source(
        &self,
        _resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        attempt: crate::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>> {
        Box::pin(async move { Err(attempt.untracked(ProviderError::internal("create not used"))) })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::api_error("update failed")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("delete not used")) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

struct PrioritizeCleanupOnRefresh {
    trigger: ShutdownTrigger,
}

impl ExecutionObserver for PrioritizeCleanupOnRefresh {
    fn on_event(&self, event: &ExecutionEvent) {
        if matches!(event, ExecutionEvent::RefreshStarted) {
            self.trigger.prioritize_cleanup();
        }
    }
}

struct ControlledReadyCreateProvider {
    ready: Arc<std::sync::atomic::AtomicBool>,
    started: Arc<Notify>,
    waker: Arc<Mutex<Option<std::task::Waker>>>,
}

struct CheckpointBlockingProvider {
    release_second: Notify,
    release_third: Notify,
}

impl Provider for CheckpointBlockingProvider {
    fn name(&self) -> &str {
        "checkpoint-blocking"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(async { Err(ProviderError::internal("read not used")) })
    }

    fn read_data_source(
        &self,
        _resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
    }

    fn create(
        &self,
        id: &ResourceId,
        _request: CreateRequest,
        _attempt: crate::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>> {
        let id = id.clone();
        Box::pin(async move {
            if id.identity_str() == Some("second") {
                self.release_second.notified().await;
            } else if id.identity_str() == Some("third") {
                self.release_third.notified().await;
            }
            Ok(crate::provider::CreateOutcome::fixture_success(
                applied_state_for_test(ok_state(&id)),
            ))
        })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("update not used")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("delete not used")) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

impl ControlledReadyCreateProvider {
    fn new() -> Self {
        Self {
            ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            started: Arc::new(Notify::new()),
            waker: Arc::new(Mutex::new(None)),
        }
    }

    fn complete(&self) {
        self.ready.store(true, Ordering::SeqCst);
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        }
    }
}

impl Provider for ControlledReadyCreateProvider {
    fn name(&self) -> &str {
        "controlled-ready-create"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(async { Err(ProviderError::internal("read not used")) })
    }

    fn read_data_source(
        &self,
        _resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
    }

    fn create(
        &self,
        id: &ResourceId,
        _request: CreateRequest,
        _attempt: crate::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>> {
        let id = id.clone();
        let ready = Arc::clone(&self.ready);
        let started = Arc::clone(&self.started);
        let waker = Arc::clone(&self.waker);
        Box::pin(std::future::poll_fn(move |cx| {
            started.notify_one();
            if ready.load(Ordering::SeqCst) {
                std::task::Poll::Ready(Ok(crate::provider::CreateOutcome::fixture_success(
                    applied_state_for_test(ok_state(&id)),
                )))
            } else {
                *waker.lock().unwrap() = Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        }))
    }

    unexpected_resume_create!();

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("update not used")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("delete not used")) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let cancel = uncancelled_shutdown();

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
async fn checkpoint_sink_receives_each_completed_effect_before_execution_ends() {
    let provider = CheckpointBlockingProvider {
        release_second: Notify::new(),
        release_third: Notify::new(),
    };
    let first = make_resource("first", &[]);
    let first_id = first.id.clone();
    let second = make_resource("second", &[]);
    let second_id = second.id.clone();
    let third = make_resource("third", &[]);
    let third_id = third.id.clone();
    let plan = create_independent_create_plan(["first", "second", "third"]);
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let checkpoint_sink = ExecutionCheckpointSink::new(move |snapshot| {
        checkpoint_tx
            .send(snapshot)
            .expect("checkpoint receiver must remain alive");
    });
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink,
    };
    let observer = MockObserver::new();
    let execution = execute_plan(&provider, input, &observer, uncancelled_shutdown());
    tokio::pin!(execution);

    let first_snapshot = tokio::select! {
        snapshot = checkpoint_rx.recv() => snapshot.expect("first checkpoint"),
        _ = &mut execution => panic!("execution ended before publishing the first checkpoint"),
    };
    assert_eq!(first_snapshot.success_count, 1);
    assert!(first_snapshot.applied_states.contains_key(&first_id));
    assert!(!first_snapshot.applied_states.contains_key(&second_id));

    provider.release_second.notify_one();
    let second_snapshot = tokio::select! {
        biased;
        snapshot = checkpoint_rx.recv() => snapshot.expect("second checkpoint"),
        _ = &mut execution => panic!("execution ended before publishing the second checkpoint"),
    };
    assert_eq!(second_snapshot.success_count, 2);
    assert!(second_snapshot.applied_states.contains_key(&first_id));
    assert!(second_snapshot.applied_states.contains_key(&second_id));
    assert!(!second_snapshot.applied_states.contains_key(&third_id));

    provider.release_third.notify_one();
    assert!(matches!(execution.await, ExecutionOutcome::Completed(_)));
    let third_snapshot = checkpoint_rx.recv().await.expect("third checkpoint");
    assert_eq!(third_snapshot.success_count, 3);
    assert!(third_snapshot.applied_states.contains_key(&third_id));
}

#[tokio::test]
async fn precompleted_state_only_result_is_checkpointed_before_provider_completion() {
    let provider = ControlledReadyCreateProvider::new();
    let resource = make_resource("provider-blocked", &[]);
    let resource_id = resource.id.clone();
    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    let (checkpoint_tx, mut checkpoint_rx) = tokio::sync::mpsc::unbounded_channel();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 1,
        checkpoint_sink: ExecutionCheckpointSink::new(move |snapshot| {
            checkpoint_tx
                .send(snapshot)
                .expect("checkpoint receiver must remain alive");
        }),
    };
    let observer = MockObserver::new();
    let execution = execute_plan(&provider, input, &observer, uncancelled_shutdown());
    tokio::pin!(execution);

    let state_only_snapshot = tokio::select! {
        snapshot = checkpoint_rx.recv() => snapshot.expect("state-only checkpoint"),
        _ = &mut execution => panic!("provider-blocked execution ended unexpectedly"),
    };
    assert_eq!(state_only_snapshot.success_count, 1);
    assert!(state_only_snapshot.applied_states.is_empty());

    provider.complete();
    assert!(matches!(execution.await, ExecutionOutcome::Completed(_)));
    let provider_snapshot = checkpoint_rx.recv().await.expect("provider checkpoint");
    assert_eq!(provider_snapshot.success_count, 2);
    assert!(provider_snapshot.applied_states.contains_key(&resource_id));
}

fn persisted_create_for_executor(id: &ResourceId) -> crate::provider::InFlightCreate {
    let pending = crate::provider::CreateAttempt::new(id.clone())
        .0
        .issued(
            AppliedIdentifier::new("created-id").expect("fixture identifier must be non-empty"),
            crate::provider::Sent::AllRequestAttributes,
        )
        .pending(
            crate::provider::PendingToken::new("persisted-create-token"),
            1,
        );
    pending.persist(
        crate::provider::PersistedAttributes::default(),
        crate::provider::PersistedAttributes::default(),
    )
}

fn persisted_unissued_create_for_executor(id: &ResourceId) -> crate::provider::InFlightCreate {
    let pending = crate::provider::CreateAttempt::new(id.clone()).0.pending(
        crate::provider::PendingToken::new("persisted-create-token"),
        crate::provider::Sent::AllRequestAttributes,
        1,
    );
    pending.persist(
        crate::provider::PersistedAttributes::default(),
        crate::provider::PersistedAttributes::default(),
    )
}

async fn execute_lifecycle_fixture(
    provider: &MockProvider,
    plan: &Plan,
    managed_current: HashMap<ResourceId, Current>,
) -> ExecutionResult {
    execute_lifecycle_fixture_with_observer(provider, plan, managed_current, &MockObserver::new())
        .await
}

async fn execute_lifecycle_fixture_with_observer(
    provider: &MockProvider,
    plan: &Plan,
    managed_current: HashMap<ResourceId, Current>,
    observer: &MockObserver,
) -> ExecutionResult {
    let input = ExecutionInput {
        plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current,
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    completed_result(execute_plan(provider, input, observer, uncancelled_shutdown()).await)
}

#[tokio::test]
async fn create_success_publishes_complete() {
    let provider = MockProvider::new();
    let source = make_resource("source", &[]);
    let dependent = make_resource("dependent", &["source"]);
    let source_id = source.id.clone();
    let dependent_id = dependent.id.clone();
    let mut plan = Plan::new();
    plan.add(create_effect(source));
    plan.add(create_effect(dependent));
    provider.push_create(Ok(ok_state(&source_id)));
    provider.push_create(Ok(ok_state(&dependent_id)));

    let result = execute_lifecycle_fixture(&provider, &plan, HashMap::new()).await;

    assert_eq!(
        (
            result.success_count,
            result.failure_count,
            result.skip_count
        ),
        (2, 0, 0)
    );
    assert!(matches!(
        result.applied_states.get(&source_id),
        Some(Applied::Complete { .. })
    ));
    assert!(matches!(
        result.applied_states.get(&dependent_id),
        Some(Applied::Complete { .. })
    ));
}

#[tokio::test]
async fn resume_resumed_publishes_observed() {
    let provider = MockProvider::new();
    let source = make_resource("source", &[]);
    let dependent = make_resource("dependent", &["source"]);
    let source_id = source.id.clone();
    let dependent_id = dependent.id.clone();
    let pending = persisted_create_for_executor(&source_id);
    let started_at = pending.started_at().clone();
    let mut plan = Plan::new();
    plan.add_resume_create(resolved(source), started_at);
    plan.add(create_effect(dependent));
    provider.push_resume(ResumeFixture::Resumed);
    provider.push_create(Ok(ok_state(&dependent_id)));

    let result = execute_lifecycle_fixture(
        &provider,
        &plan,
        HashMap::from([(source_id.clone(), Current::CreateInFlight(pending))]),
    )
    .await;

    assert_eq!(
        (
            result.success_count,
            result.failure_count,
            result.skip_count
        ),
        (2, 0, 0)
    );
    let Some(Applied::Observed(created)) = result.applied_states.get(&source_id) else {
        panic!("completed resume did not write an observed create")
    };
    assert!(matches!(
        created.observation(),
        crate::provider::CreateObservation::Observed(_)
    ));
    assert!(matches!(
        result.applied_states.get(&dependent_id),
        Some(Applied::Complete { .. })
    ));
}

#[tokio::test]
async fn resume_dispatch_validates_resent_sent_against_checked_request() {
    let provider = MockProvider::new();
    provider.push_resume(ResumeFixture::ResentInFlightWithInvalidSent);
    let resource = make_resource("source", &[]);
    let id = resource.id.clone();
    let pending = persisted_unissued_create_for_executor(&id);
    let bindings = ResolvedBindings::default();
    let module_gate = crate::executor::ModuleConstraintGate::from_live_compositions(&[]);
    let preparation = crate::executor::ProviderPreparationContext::new(
        &bindings,
        &module_gate,
        &[],
        &NoopNormalizer,
        &[],
        &TEST_SCHEMAS,
    );
    let request = crate::executor::prepare_create_request(resource, &preparation)
        .await
        .expect("fixture request passes provider preparation");
    let request = crate::provider::ResumeCreateRequest::new(pending, request)
        .expect("fixture continuation and request share a route");

    let failure = crate::executor::dispatch_resume_create(&provider, request)
        .await
        .expect_err("resent pending fixture returns a classified failure");
    let crate::provider::ResumeFailureKind::InFlight { pending, error } = failure.into_kind()
    else {
        panic!("resent pending classification was not preserved");
    };
    assert_eq!(
        pending.sent(),
        crate::provider::SentRecord::Request(crate::provider::Sent::only(Vec::<String>::new()))
    );
    assert_eq!(
        error.detail().context(),
        "provider returned sent attributes outside the checked create request"
    );
    assert_eq!(
        error
            .detail()
            .cause()
            .expect("boundary validation retains the typed provider cause")
            .to_string(),
        "resent create is pending"
    );
}

#[tokio::test]
async fn create_untracked_skips_dependents_and_writes_nothing() {
    let provider = MockProvider::new();
    let source = make_resource("source", &[]);
    let dependent = make_resource("dependent", &["source"]);
    let source_id = source.id.clone();
    let dependent_id = dependent.id.clone();
    let mut plan = Plan::new();
    plan.add(create_effect(source));
    plan.add(create_effect(dependent));
    provider.push_create(Err(ProviderError::api_error("create rejected")));

    let result = execute_lifecycle_fixture(&provider, &plan, HashMap::new()).await;

    assert_eq!(
        (
            result.success_count,
            result.failure_count,
            result.skip_count
        ),
        (0, 1, 1)
    );
    assert!(!result.applied_states.contains_key(&source_id));
    assert!(!result.applied_states.contains_key(&dependent_id));
}

#[tokio::test]
async fn create_before_destroy_outcome_unknown_records_replacement_create_state() {
    let provider = MockProvider::new();
    let source = make_resource("source", &[]);
    let source_id = source.id.clone();
    let directives = Directives {
        create_before_destroy: true,
        ..Directives::default()
    };
    let mut plan = Plan::new();
    plan.add(create_effect(source));
    plan.add(Effect::Delete {
        id: crate::resource::ResolvedResourceId::new(source_id.clone()),
        identifier: crate::effect::fixture_applied_identifier("old-id"),
        generation: EffectGeneration::Current,
        directives,
        binding: Some(absolute_binding("source")),
        dependencies: HashSet::new(),
        explicit_dependencies: HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });
    provider.push_classified_create(ClassifiedCreateFixture::OutcomeUnknown);
    let old = State::existing(source_id.clone(), HashMap::new()).with_identifier("old-id");

    let result = execute_lifecycle_fixture(
        &provider,
        &plan,
        HashMap::from([(source_id.clone(), Current::from_state(old))]),
    )
    .await;

    assert_eq!(
        (
            result.success_count,
            result.failure_count,
            result.skip_count
        ),
        (0, 1, 1)
    );
    let entry = result.applied_states.get(&source_id);
    assert!(
        matches!(entry, Some(Applied::CreateOutcomeUnknown(unknown)) if unknown.id() == &source_id),
        "{entry:?}"
    );
    assert_eq!(
        provider.calls(),
        vec![("create".to_string(), source_id.to_string())],
        "the failed CBD create must prevent deletion of the old generation"
    );
}

#[tokio::test]
async fn resume_ended_skips_dependents_and_marks_create_ended() {
    let provider = MockProvider::new();
    let source = make_resource("source", &[]);
    let dependent = make_resource("dependent", &["source"]);
    let source_id = source.id.clone();
    let dependent_id = dependent.id.clone();
    let pending = persisted_create_for_executor(&source_id);
    let mut plan = Plan::new();
    plan.add_resume_create(resolved(source), pending.started_at().clone());
    plan.add(create_effect(dependent));
    provider.push_resume(ResumeFixture::Ended);

    let result = execute_lifecycle_fixture(
        &provider,
        &plan,
        HashMap::from([(source_id.clone(), Current::CreateInFlight(pending))]),
    )
    .await;

    assert_eq!(
        (
            result.success_count,
            result.failure_count,
            result.skip_count
        ),
        (0, 1, 1)
    );
    assert!(matches!(
        result.applied_states.get(&source_id),
        Some(Applied::CreateEnded)
    ));
    assert!(!result.applied_states.contains_key(&dependent_id));
}

#[tokio::test]
async fn created_failure_records_observation() {
    let provider = MockProvider::new();
    let observer = MockObserver::new();
    let source = make_resource("source", &[]);
    let dependent = make_resource("dependent", &["source"]);
    let source_id = source.id.clone();
    let dependent_id = dependent.id.clone();
    let mut plan = Plan::new();
    plan.add(create_effect(source));
    plan.add(create_effect(dependent));
    provider.push_classified_create(ClassifiedCreateFixture::Created);

    let result =
        execute_lifecycle_fixture_with_observer(&provider, &plan, HashMap::new(), &observer).await;

    assert_eq!(
        (
            result.success_count,
            result.failure_count,
            result.skip_count
        ),
        (0, 1, 1)
    );
    let Some(Applied::Observed(created)) = result.applied_states.get(&source_id) else {
        panic!("created failure did not retain an observed create")
    };
    assert_eq!(created.identifier().as_str(), "created-id");
    assert!(!result.applied_states.contains_key(&dependent_id));
    let events = observer.events();
    assert_eq!(
        effect_failure_event(&events, &source_id),
        format!(
            "failed:{source_id}:post-create fixture failed\nresource created; recorded in state, the next apply completes it"
        )
    );
}

#[tokio::test]
async fn created_failure_not_found_reports_unobserved_and_records_identifier() {
    let provider = MockProvider::new();
    let observer = MockObserver::new();
    let mut source = make_resource("source", &[]);
    source.id.provider = "test".to_string();
    let source_id = source.id.clone();
    let mut plan = Plan::new();
    plan.add(create_effect(source));
    provider.push_classified_create(ClassifiedCreateFixture::CreatedNotFound);

    let result =
        execute_lifecycle_fixture_with_observer(&provider, &plan, HashMap::new(), &observer).await;

    let Some(Applied::Observed(created)) = result.applied_states.get(&source_id) else {
        panic!("created failure did not retain an observed create")
    };
    assert_eq!(created.identifier().as_str(), "created-id");
    let events = observer.events();
    let failure = effect_failure_event(&events, &source_id);
    assert_eq!(
        failure,
        format!(
            "failed:{source_id}:post-create fixture failed\ntest reported identifier 'created-id' but the resource could not be observed; recorded in state so the next plan re-checks it\n  reason: provider read after create returned not found"
        )
    );
}

#[tokio::test]
async fn created_failure_read_failed_reports_unobserved_and_records_identifier() {
    let provider = MockProvider::new();
    let observer = MockObserver::new();
    let mut source = make_resource("source", &[]);
    source.id.provider = "test".to_string();
    let source_id = source.id.clone();
    let mut plan = Plan::new();
    plan.add(create_effect(source));
    provider.push_classified_create(ClassifiedCreateFixture::CreatedReadFailed);

    let result =
        execute_lifecycle_fixture_with_observer(&provider, &plan, HashMap::new(), &observer).await;

    let Some(Applied::Observed(created)) = result.applied_states.get(&source_id) else {
        panic!("created failure did not retain an observed create")
    };
    assert_eq!(created.identifier().as_str(), "created-id");
    let events = observer.events();
    let failure = effect_failure_event(&events, &source_id);
    assert_eq!(
        failure,
        format!(
            "failed:{source_id}:post-create fixture failed\ntest reported identifier 'created-id' but the resource could not be observed; recorded in state so the next plan re-checks it\n  reason: read-back access denied"
        )
    );
}

#[tokio::test]
async fn resumed_created_failure_found_reports_observed_note() {
    let provider = MockProvider::new();
    let observer = MockObserver::new();
    let mut source = make_resource("source", &[]);
    source.id.provider = "test".to_string();
    let source_id = source.id.clone();
    let pending = persisted_create_for_executor(&source_id);
    let mut plan = Plan::new();
    plan.add_resume_create(resolved(source), pending.started_at().clone());
    provider.push_resume(ResumeFixture::CreatedFound);

    let result = execute_lifecycle_fixture_with_observer(
        &provider,
        &plan,
        HashMap::from([(source_id.clone(), Current::CreateInFlight(pending))]),
        &observer,
    )
    .await;

    let Some(Applied::Observed(created)) = result.applied_states.get(&source_id) else {
        panic!("resumed created failure did not retain an observed create")
    };
    assert_eq!(created.identifier().as_str(), "created-id");
    assert!(matches!(
        created.observation(),
        crate::provider::CreateObservation::Observed(_)
    ));
    let events = observer.events();
    assert_eq!(
        effect_failure_event(&events, &source_id),
        format!(
            "failed:{source_id}:resumed create fixture failed\nresource created; recorded in state, the next apply completes it"
        )
    );
}

#[tokio::test]
async fn resumed_created_failure_not_found_reports_unobserved_and_records_identifier() {
    let provider = MockProvider::new();
    let observer = MockObserver::new();
    let mut source = make_resource("source", &[]);
    source.id.provider = "test".to_string();
    let source_id = source.id.clone();
    let pending = persisted_create_for_executor(&source_id);
    let mut plan = Plan::new();
    plan.add_resume_create(resolved(source), pending.started_at().clone());
    provider.push_resume(ResumeFixture::CreatedNotFound);

    let result = execute_lifecycle_fixture_with_observer(
        &provider,
        &plan,
        HashMap::from([(source_id.clone(), Current::CreateInFlight(pending))]),
        &observer,
    )
    .await;

    let Some(Applied::Observed(created)) = result.applied_states.get(&source_id) else {
        panic!("resumed created failure did not retain an observed create")
    };
    assert_eq!(created.identifier().as_str(), "created-id");
    let events = observer.events();
    let failure = effect_failure_event(&events, &source_id);
    assert_eq!(
        failure,
        format!(
            "failed:{source_id}:resumed create fixture failed\ntest reported identifier 'created-id' but the resource could not be observed; recorded in state so the next plan re-checks it\n  reason: provider read after create returned not found"
        )
    );
}

#[tokio::test]
async fn resumed_created_failure_read_failed_reports_reason() {
    let provider = MockProvider::new();
    let observer = MockObserver::new();
    let mut source = make_resource("source", &[]);
    source.id.provider = "test".to_string();
    let source_id = source.id.clone();
    let pending = persisted_create_for_executor(&source_id);
    let mut plan = Plan::new();
    plan.add_resume_create(resolved(source), pending.started_at().clone());
    provider.push_resume(ResumeFixture::CreatedReadFailed);

    let result = execute_lifecycle_fixture_with_observer(
        &provider,
        &plan,
        HashMap::from([(source_id.clone(), Current::CreateInFlight(pending))]),
        &observer,
    )
    .await;

    let Some(Applied::Observed(created)) = result.applied_states.get(&source_id) else {
        panic!("resumed created failure did not retain an observed create")
    };
    assert_eq!(created.identifier().as_str(), "created-id");
    let events = observer.events();
    assert_eq!(
        effect_failure_event(&events, &source_id),
        format!(
            "failed:{source_id}:resumed create fixture failed\ntest reported identifier 'created-id' but the resource could not be observed; recorded in state so the next plan re-checks it\n  reason: resumed read-back access denied"
        )
    );
}

#[tokio::test]
async fn in_flight_failure_records_pending() {
    let provider = MockProvider::new();
    let source = make_resource("source", &[]);
    let dependent = make_resource("dependent", &["source"]);
    let source_id = source.id.clone();
    let dependent_id = dependent.id.clone();
    let mut plan = Plan::new();
    plan.add(create_effect(source));
    plan.add(create_effect(dependent));
    provider.push_classified_create(ClassifiedCreateFixture::InFlight);

    let result = execute_lifecycle_fixture(&provider, &plan, HashMap::new()).await;

    assert_eq!(
        (
            result.success_count,
            result.failure_count,
            result.skip_count
        ),
        (0, 1, 1)
    );
    assert!(matches!(
        result.applied_states.get(&source_id),
        Some(Applied::CreateInFlight(_))
    ));
    assert!(!result.applied_states.contains_key(&dependent_id));
}

#[tokio::test]
async fn resume_resolution_failure_has_no_applied_entry() {
    let provider = MockProvider::new();
    let mut source = make_resource("source", &[]);
    source.set_attr(
        "unresolved",
        Value::root_resource_ref("missing".to_string(), "id".to_string(), vec![]),
    );
    let source_id = source.id.clone();
    let pending = persisted_create_for_executor(&source_id);
    let mut plan = Plan::new();
    plan.add_resume_create(resolved(source), pending.started_at().clone());

    let result = execute_lifecycle_fixture(
        &provider,
        &plan,
        HashMap::from([(source_id.clone(), Current::CreateInFlight(pending))]),
    )
    .await;

    assert_eq!(
        (
            result.success_count,
            result.failure_count,
            result.skip_count
        ),
        (0, 1, 0)
    );
    assert!(!result.applied_states.contains_key(&source_id));
}

#[tokio::test]
async fn skipped_or_cancelled_resume_has_no_applied_entry() {
    let provider = MockProvider::new();
    let failed = make_resource("failed", &[]);
    let resume = make_resource("resume", &["failed"]);
    let failed_id = failed.id.clone();
    let resume_id = resume.id.clone();
    let pending = persisted_create_for_executor(&resume_id);
    let mut plan = Plan::new();
    plan.add(create_effect(failed));
    plan.add_resume_create(resolved(resume), pending.started_at().clone());
    provider.push_create(Err(ProviderError::api_error("dependency failed")));

    let result = execute_lifecycle_fixture(
        &provider,
        &plan,
        HashMap::from([(resume_id.clone(), Current::CreateInFlight(pending))]),
    )
    .await;

    assert_eq!(
        (
            result.success_count,
            result.failure_count,
            result.skip_count
        ),
        (0, 1, 1)
    );
    assert!(!result.applied_states.contains_key(&failed_id));
    assert!(!result.applied_states.contains_key(&resume_id));
}

fn provider_boundary_constraint_schemas() -> SchemaRegistry {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("test")
            .attribute(
                AttributeSchema::optional(
                    "target",
                    AttributeType::refined_string(None, Some("^good-".to_string()), None, None),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional("token", AttributeType::string())
                    .write_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "config",
                    AttributeType::struct_(
                        "Config",
                        vec![
                            StructField::optional("issuer", AttributeType::string()).build(),
                            StructField::optional("client_secret", AttributeType::string())
                                .write_only()
                                .build(),
                        ],
                    ),
                )
                .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas
}

fn deferred_child_constraint_schemas() -> SchemaRegistry {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("test").attribute(
            AttributeSchema::optional(
                "value",
                AttributeType::refined_string(None, Some("^good-".to_string()), None, None),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas
}

fn deferred_data_source_constraint_schemas() -> SchemaRegistry {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (ResourceSchema::new("Lookup").as_data_source().attribute(
            AttributeSchema::optional(
                "filter",
                AttributeType::refined_string(None, Some("^good-".to_string()), None, None),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas
}

#[tokio::test]
async fn invalid_resolved_create_value_never_reaches_provider() {
    let provider = MockProvider::new();
    let mut resource = make_resource("invalid-create", &[]);
    resource.set_attr(
        "target",
        Value::Concrete(ConcreteValue::String("bad".into())),
    );
    let id = resource.id.clone();
    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    provider.push_create(Ok(ok_state(&id)));
    let schemas = provider_boundary_constraint_schemas();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 1);
    assert!(provider.calls().is_empty());
}

#[tokio::test]
async fn invalid_secret_create_value_is_masked_and_never_reaches_provider() {
    use crate::binding_index::BindingValueSource;

    let plaintext = "apply-time-plaintext";
    let provider = MockProvider::new();
    let mut resource = make_resource("invalid-secret-create", &[]);
    resource.set_attr(
        "target",
        Value::root_resource_ref("secret-source", "value", vec![]),
    );
    let id = resource.id.clone();
    let unresolved = HashMap::from([(
        id.clone(),
        UnresolvedResource::from_pre_resolve(resource.clone()),
    )]);
    let mut bindings = ResolvedBindings::default();
    bindings.set(
        "secret-source",
        HashMap::from([(
            "value".to_string(),
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String(plaintext.to_string().into()),
            )))),
        )]),
        BindingValueSource::Local,
    );
    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    provider.push_create(Ok(ok_state(&id)));
    let schemas = provider_boundary_constraint_schemas();
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    let events = observer.events().join("\n");

    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 1);
    assert!(provider.calls().is_empty());
    assert!(events.contains("(secret)"), "events: {events}");
    assert!(
        !events.contains(plaintext),
        "secret leaked in apply error: {events}"
    );
}

struct MustNotNormalizeInvalidValue;

impl crate::provider::ProviderNormalizer for MustNotNormalizeInvalidValue {
    fn normalize_desired<'a>(
        &'a self,
        _resources: &'a mut [crate::resource::ProviderResource],
    ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
        Box::pin(async { panic!("normalizer ran before the value-constraint gate") })
    }

    fn normalize_state<'a>(
        &'a self,
        _current_states: &'a mut crate::provider::StateTransformMap,
    ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
        crate::provider::ready_noop()
    }

    fn hydrate_read_state<'a>(
        &'a self,
        _current_states: &'a mut crate::provider::StateTransformMap,
        _saved_attrs: &'a crate::provider::SavedAttrs,
    ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
        crate::provider::ready_noop()
    }
}

#[tokio::test]
async fn invalid_resolved_value_is_rejected_before_provider_normalization() {
    let provider = MockProvider::new();
    let mut resource = make_resource("invalid-before-normalize", &[]);
    resource.set_attr(
        "target",
        Value::Concrete(ConcreteValue::String("bad".into())),
    );
    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    let schemas = provider_boundary_constraint_schemas();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &MustNotNormalizeInvalidValue,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 1);
    assert!(provider.calls().is_empty());
}

#[tokio::test]
async fn invalid_resolved_update_value_never_reaches_provider() {
    let provider = MockProvider::new();
    let mut resource = make_resource("invalid-update", &[]);
    resource.set_attr(
        "target",
        Value::Concrete(ConcreteValue::String("bad".into())),
    );
    let id = resource.id.clone();
    let from = State::existing(
        id.clone(),
        HashMap::from([(
            "target".to_string(),
            Value::Concrete(ConcreteValue::String("good-before".into())),
        )]),
    )
    .with_identifier("id-123");
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier(
            from.id.identity_display().to_string(),
        ),
        from: Box::new(from),
        to: resolved(resource),
        changed_attributes: vec!["target".to_string()],
    });
    provider.push_update(Ok(ok_state(&id)));
    let schemas = provider_boundary_constraint_schemas();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 1);
    assert!(provider.calls().is_empty());
}

#[tokio::test]
async fn execute_plan_missing_update_schema_fails_before_provider_update() {
    let provider = MockProvider::new();
    let mut resource = Resource::with_provider("missing", "missing.resource", "no-schema", None);
    resource.set_attr(
        "description",
        Value::Concrete(ConcreteValue::String("after".into())),
    );
    let id = resource.id.clone();
    let from = State::existing(
        id.clone(),
        HashMap::from([(
            "description".to_string(),
            Value::Concrete(ConcreteValue::String("before".into())),
        )]),
    )
    .with_identifier("id-123");
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        from: Box::new(from),
        to: resolved(resource),
        changed_attributes: vec!["description".to_string()],
    });
    provider.push_update(Ok(ok_state(&id)));
    let schemas = SchemaRegistry::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();

    // Deliberately bypass the convenience fixture that supplies empty schemas:
    // this test verifies the production boundary rejects an absent schema.
    let comparison_provider = crate::provider::ProviderRouter::new();
    let result = completed_result(
        execute_runtime_plan(
            &provider,
            &comparison_provider,
            input.into(),
            &observer,
            uncancelled_shutdown(),
        )
        .await,
    );

    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 1);
    let events = observer.events();
    assert!(
        events.iter().any(|event| {
            event.starts_with(&format!("failed:{id}:"))
                && event.contains("no managed schema is registered")
        }),
        "missing schema must be reported through EffectFailed: {events:?}"
    );
    assert!(
        provider
            .calls()
            .iter()
            .all(|(operation, _)| operation != "update"),
        "provider update must not run when the managed schema is missing"
    );
    assert!(provider.captured_update_requests().is_empty());
}

#[tokio::test]
async fn valid_checked_values_preserve_create_payload_and_update_patch() {
    let provider = MockProvider::new();
    let mut created = make_resource("valid-create", &[]);
    let create_value = Value::Concrete(ConcreteValue::String("good-create".into()));
    created.set_attr("target", create_value.clone());
    let create_id = created.id.clone();

    let mut updated = make_resource("valid-update", &[]);
    let update_value = Value::Concrete(ConcreteValue::String("good-update".into()));
    updated.set_attr("target", update_value.clone());
    updated.set_attr(
        "token",
        Value::Concrete(ConcreteValue::String("unchanged-token".into())),
    );
    updated.set_attr(
        "config",
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
            (
                "issuer".to_string(),
                Value::Concrete(ConcreteValue::String("https://issuer.example.com".into())),
            ),
            (
                "client_secret".to_string(),
                Value::Concrete(ConcreteValue::String("client-secret".into())),
            ),
        ]))),
    );
    let update_id = updated.id.clone();
    let from = State::existing(
        update_id.clone(),
        HashMap::from([
            (
                "target".to_string(),
                Value::Concrete(ConcreteValue::String("good-before".into())),
            ),
            (
                "obsolete".to_string(),
                Value::Concrete(ConcreteValue::String("remove-me".into())),
            ),
            (
                "token".to_string(),
                Value::Concrete(ConcreteValue::String("unchanged-token".into())),
            ),
            (
                "config".to_string(),
                Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                    "issuer".to_string(),
                    Value::Concrete(ConcreteValue::String("https://issuer.example.com".into())),
                )]))),
            ),
        ]),
    )
    .with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(create_effect(created));
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier(
            from.id.identity_display().to_string(),
        ),
        from: Box::new(from),
        to: resolved(updated),
        changed_attributes: vec!["target".to_string(), "obsolete".to_string()],
    });
    provider.push_create(Ok(ok_state(&create_id)));
    provider.push_update(Ok(ok_state(&update_id)));
    let schemas = provider_boundary_constraint_schemas();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert_eq!(result.success_count, 2);
    assert_eq!(result.failure_count, 0);
    assert_eq!(
        provider.captured_create_resources()[0].get_attr("target"),
        Some(&create_value)
    );
    let requests = provider.captured_update_requests();
    let target = requests[0]
        .patch()
        .ops
        .iter()
        .find(|op| op.key() == "target")
        .expect("valid target must remain in the update patch");
    assert_eq!(target.sent_value(), Some(&update_value));
    assert!(
        requests[0].patch().ops.iter().any(
            |op| matches!(op, crate::provider::PatchOp::Reassert { key, .. } if key == "token")
        )
    );
    assert!(
        requests[0].patch().ops.iter().any(
            |op| matches!(op, crate::provider::PatchOp::Reassert { key, .. } if key == "config")
        )
    );
    assert!(
        !requests[0].patch().ops.iter().any(
            |op| matches!(op, crate::provider::PatchOp::Replace { key, .. } if key == "config")
        )
    );
    let Some(Applied::Complete { request, .. }) = result.applied_states.get(&update_id) else {
        panic!("successful update must publish accepted-request provenance");
    };
    let AcceptedRequestKind::Update { sent, removed } = request.kind() else {
        panic!("update must publish update request provenance");
    };
    assert_eq!(
        sent,
        &BTreeSet::from([
            "config".to_string(),
            "target".to_string(),
            "token".to_string(),
        ])
    );
    assert_eq!(removed, &BTreeSet::from(["obsolete".to_string()]));
}

#[tokio::test]
async fn absent_entirely_write_only_struct_stays_reassert_during_apply_augmentation() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let provider = MockProvider::new();
    let mut updated = make_resource("entirely-write-only", &[]);
    updated.set_attr(
        "target",
        Value::Concrete(ConcreteValue::String("good-after".into())),
    );
    updated.set_attr(
        "creds",
        Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
            "password".to_string(),
            Value::Concrete(ConcreteValue::String("client-secret".into())),
        )]))),
    );
    let id = updated.id.clone();
    let from = State::existing(
        id.clone(),
        HashMap::from([(
            "target".to_string(),
            Value::Concrete(ConcreteValue::String("good-before".into())),
        )]),
    )
    .with_identifier("id-123");
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        from: Box::new(from),
        to: resolved(updated),
        changed_attributes: vec!["target".to_string()],
    });
    provider.push_update(Ok(ok_state(&id)));
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("test")
            .attribute(
                AttributeSchema::optional(
                    "target",
                    AttributeType::refined_string(None, Some("^good-".to_string()), None, None),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "creds",
                    AttributeType::struct_(
                        "Credentials",
                        vec![
                            StructField::optional("password", AttributeType::string())
                                .write_only()
                                .build(),
                        ],
                    ),
                )
                .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert_eq!(result.failure_count, 0);
    let requests = provider.captured_update_requests();
    assert!(
        requests[0].patch().ops.iter().any(
            |op| matches!(op, crate::provider::PatchOp::Reassert { key, .. } if key == "creds")
        )
    );
    assert!(
        !requests[0].patch().ops.iter().any(
            |op| matches!(op, crate::provider::PatchOp::Replace { key, .. } if key == "creds")
        )
    );
}

#[tokio::test]
async fn invalid_value_published_by_upstream_create_blocks_dependent_create() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};

    let provider = MockProvider::new();
    let producer = make_resource("producer", &[]);
    let producer_id = producer.id.clone();
    let mut consumer = make_resource("consumer", &[]);
    consumer.set_attr(
        "target",
        Value::root_resource_ref("producer", "value", vec![]),
    );
    consumer
        .dependency_bindings
        .insert(absolute_binding("producer"));

    provider.push_create(Ok(State::existing(
        producer_id.clone(),
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("bad".into())),
        )]),
    )
    .with_identifier("producer-id")));

    let mut plan = Plan::new();
    plan.add(create_effect(producer.clone()));
    plan.add(create_effect(consumer.clone()));
    let unresolved = HashMap::from([
        (
            producer.id.clone(),
            UnresolvedResource::from_pre_resolve(producer.clone()),
        ),
        (
            consumer.id.clone(),
            UnresolvedResource::from_pre_resolve(consumer.clone()),
        ),
    ]);
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &[producer, consumer],
        compositions: &[],
        managed_current: &HashMap::new(),
        data_source_values: &DataSourceValues::empty(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let schemas = provider_boundary_constraint_schemas();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 1);
    assert_eq!(
        provider.calls(),
        vec![("create".to_string(), producer_id.to_string())]
    );
}

#[tokio::test]
async fn invalid_value_published_by_upstream_create_blocks_dependent_update() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};

    let provider = MockProvider::new();
    let producer = make_resource("producer-update", &[]);
    let producer_id = producer.id.clone();
    let mut unresolved_consumer = make_resource("consumer-update", &[]);
    unresolved_consumer.set_attr(
        "target",
        Value::root_resource_ref("producer-update", "value", vec![]),
    );
    unresolved_consumer
        .dependency_bindings
        .insert(absolute_binding("producer-update"));
    let mut planned_consumer = unresolved_consumer.clone();
    planned_consumer.set_attr(
        "target",
        Value::Concrete(ConcreteValue::String("good-before".into())),
    );
    let consumer_id = planned_consumer.id.clone();
    let from = State::existing(
        consumer_id.clone(),
        HashMap::from([(
            "target".to_string(),
            Value::Concrete(ConcreteValue::String("good-before".into())),
        )]),
    )
    .with_identifier("consumer-id");

    provider.push_create(Ok(State::existing(
        producer_id.clone(),
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("bad".into())),
        )]),
    )
    .with_identifier("producer-id")));

    let mut plan = Plan::new();
    plan.add(create_effect(producer.clone()));
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier(
            from.id.identity_display().to_string(),
        ),
        from: Box::new(from),
        to: resolved(planned_consumer.clone()),
        changed_attributes: vec!["target".to_string()],
    });
    let unresolved = HashMap::from([
        (
            producer.id.clone(),
            UnresolvedResource::from_pre_resolve(producer.clone()),
        ),
        (
            consumer_id,
            UnresolvedResource::from_pre_resolve(unresolved_consumer.clone()),
        ),
    ]);
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &[producer, unresolved_consumer],
        compositions: &[],
        managed_current: &HashMap::new(),
        data_source_values: &DataSourceValues::empty(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let schemas = provider_boundary_constraint_schemas();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 1);
    assert_eq!(
        provider.calls(),
        vec![("create".to_string(), producer_id.to_string())]
    );
}

#[tokio::test]
async fn invalid_apply_time_value_blocks_deferred_dispatch_child() {
    let provider = MockProvider::new();
    let mut cert = Resource::new("test", "cert-constraint");
    cert.binding = Some(absolute_binding("cert"));
    let cert_id = cert.id.clone();
    provider.push_create(Ok(State::existing(
        cert_id.clone(),
        HashMap::from([(
            "domain_validation_options".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::Map(indexmap::IndexMap::from([(
                    "resource_record".to_string(),
                    Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([
                        (
                            "name".to_string(),
                            Value::Concrete(ConcreteValue::String("good-name".into())),
                        ),
                        (
                            "value".to_string(),
                            Value::Concrete(ConcreteValue::String("bad".into())),
                        ),
                    ]))),
                )])),
            )])),
        )]),
    )
    .with_identifier("cert-id")));

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::DeferredCreate {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        template: Box::new(validation_deferred_for_expression()),
    });
    let schemas = deferred_child_constraint_schemas();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 1);
    assert_eq!(
        provider.calls(),
        vec![("create".to_string(), cert_id.to_string())]
    );
}

#[tokio::test]
async fn invalid_deferred_data_source_input_never_reaches_provider() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};

    let provider = MockProvider::new();
    let upstream = make_resource("data-source-producer", &[]);
    let upstream_id = upstream.id.clone();
    let mut data_source = DataSource::with_provider("test", "Lookup", "lookup", None);
    data_source.binding = Some(absolute_binding("lookup"));
    data_source.attributes.insert(
        "filter".to_string(),
        Value::root_resource_ref("data-source-producer", "value", vec![]),
    );
    let data_source_id = data_source.id.clone();

    provider.push_create(Ok(State::existing(
        upstream_id.clone(),
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("bad".into())),
        )]),
    )
    .with_identifier("producer-id")));
    provider.push_read(Ok(State::existing(data_source_id.clone(), HashMap::new())));

    let mut plan = Plan::new();
    plan.add(create_effect(upstream.clone()));
    plan.add(Effect::Read {
        resource: resolved_data_source(data_source.clone()),
    });
    let executable_data_source_values =
        executable_deferred_data_source_values(std::slice::from_ref(&data_source));
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &[upstream],
        compositions: &[],
        managed_current: &HashMap::new(),
        data_source_values: &test_data_source_values(
            std::slice::from_ref(&data_source),
            &HashMap::new(),
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let schemas = deferred_data_source_constraint_schemas();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: executable_data_source_values,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 1);
    assert!(provider.captured_data_source_reads().is_empty());
    assert_eq!(
        provider.calls(),
        vec![("create".to_string(), upstream_id.to_string())]
    );
    let events = observer.events().join("\n");
    let leaf = "does not match required pattern /^good-/";
    assert!(events.contains(leaf), "missing constraint detail: {events}");
    assert_eq!(
        events.matches(leaf).count(),
        1,
        "constraint detail must appear exactly once: {events}"
    );
}

#[tokio::test]
async fn plan_normalized_apply_resolved_data_source_schema_violation_blocks_read() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};

    let provider = MockProvider::new();
    let upstream = make_resource("plan-data-source-producer", &[]);
    let upstream_id = upstream.id.clone();
    let mut data_source = DataSource::with_provider("test", "Lookup", "plan-lookup", None);
    data_source.binding = Some(absolute_binding("plan-lookup"));
    data_source.attributes.insert(
        "filter".to_string(),
        Value::root_resource_ref("plan-data-source-producer", "value", vec![]),
    );
    let data_source_id = data_source.id.clone();

    provider.push_create(Ok(State::existing(
        upstream_id.clone(),
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("bad".into())),
        )]),
    )
    .with_identifier("producer-id")));
    provider.push_read(Ok(State::existing(data_source_id.clone(), HashMap::new())));

    let mut plan = Plan::new();
    plan.add(create_effect(upstream.clone()));
    plan.add(Effect::Read {
        resource: resolved_data_source(data_source.clone()),
    });
    let executable_data_source_values =
        executable_deferred_data_source_values(std::slice::from_ref(&data_source));
    let resource_origins = HashMap::from([(
        upstream.id.clone(),
        UnresolvedResource::from_pre_resolve(upstream.clone()),
    )]);
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: std::slice::from_ref(&upstream),
        compositions: &[],
        managed_current: &HashMap::new(),
        data_source_values: &test_data_source_values(
            std::slice::from_ref(&data_source),
            &HashMap::new(),
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let schemas = deferred_data_source_constraint_schemas();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &resource_origins,
            data_source_origins: std::slice::from_ref(&data_source),
        },
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: executable_data_source_values,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert!(
        provider.captured_data_source_reads().is_empty(),
        "schema-invalid apply-resolved data source reached the provider"
    );
    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 1);
    assert_eq!(
        provider.calls(),
        vec![("create".to_string(), upstream_id.to_string())]
    );
}

fn pending_module_composition(
    instance: &'static str,
    arguments: Vec<(&str, Value)>,
    constraints: Vec<crate::resource::PendingModuleConstraint>,
) -> crate::resource::LiveComposition {
    use crate::parser::TypeExpr;
    use crate::resource::{LiveComposition, LiveCompositionArgument, LiveCompositionSignature};

    LiveComposition {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "_virtual", instance,
        )),
        signature: LiveCompositionSignature {
            arguments: arguments
                .into_iter()
                .map(|(name, value)| {
                    (
                        name.to_string(),
                        LiveCompositionArgument::from_value(
                            value,
                            crate::validation::resolve_consumer_static_type_for_signature(
                                &TypeExpr::String,
                                &crate::parser::ProviderContext::default(),
                            ),
                            crate::parser::ArgumentOrigin::Defaulted,
                        ),
                    )
                })
                .collect(),
            attributes: indexmap::IndexMap::new(),
            pending_constraints: constraints,
        },
        binding: Some(absolute_binding(instance)),
        dependency_bindings: std::collections::BTreeSet::new(),
        module_name: "checked_module".to_string(),
        instance: instance.to_string(),
        call_path: Default::default(),
        quoted_string_attrs: HashSet::new(),
    }
}

fn module_not_bad_constraint(argument: &str) -> crate::resource::PendingModuleConstraint {
    use crate::parser::{CompareOp, ValidateExpr};
    use crate::resource::{ModuleConstraintId, PendingModuleConstraint};

    PendingModuleConstraint {
        id: ModuleConstraintId::argument_validation(argument, 0),
        expression: ValidateExpr::Compare {
            lhs: Box::new(ValidateExpr::Var(argument.to_string())),
            op: CompareOp::Ne,
            rhs: Box::new(ValidateExpr::String("bad".to_string())),
        },
        message: format!("{argument} must not be bad"),
    }
}

fn module_require_equal_constraint(
    left: &str,
    right: &str,
) -> crate::resource::PendingModuleConstraint {
    use crate::parser::{CompareOp, ValidateExpr};
    use crate::resource::{ModuleConstraintId, PendingModuleConstraint};

    PendingModuleConstraint {
        id: ModuleConstraintId::require(0),
        expression: ValidateExpr::Compare {
            lhs: Box::new(ValidateExpr::Var(left.to_string())),
            op: CompareOp::Eq,
            rhs: Box::new(ValidateExpr::Var(right.to_string())),
        },
        message: format!("{left} and {right} must match"),
    }
}

#[tokio::test]
async fn plan_normalized_apply_resolved_data_source_module_violation_blocks_read() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use crate::resource::LiveCompositionAttribute;

    let provider = MockProvider::new();
    let upstream = make_resource("module-data-source-producer", &[]);
    let upstream_id = upstream.id.clone();
    let mut data_source = DataSource::with_provider("test", "Lookup", "module-lookup", None);
    data_source.binding = Some(absolute_binding("module-lookup"));
    data_source.attributes.insert(
        "filter".to_string(),
        Value::root_resource_ref("checked", "value", vec![]),
    );
    let data_source_id = data_source.id.clone();
    let mut composition = pending_module_composition(
        "root.checked",
        vec![(
            "value",
            Value::root_resource_ref("module-data-source-producer", "value", vec![]),
        )],
        vec![module_not_bad_constraint("value")],
    );
    composition.binding = Some(absolute_binding("checked"));
    composition.signature.attributes.insert(
        "value".to_string(),
        LiveCompositionAttribute::from_value(
            Value::root_resource_ref("module-data-source-producer", "value", vec![]),
            None,
        ),
    );

    provider.push_create(Ok(State::existing(
        upstream_id.clone(),
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("bad".into())),
        )]),
    )
    .with_identifier("producer-id")));
    provider.push_read(Ok(State::existing(data_source_id.clone(), HashMap::new())));

    let mut plan = Plan::new();
    plan.add(create_effect(upstream.clone()));
    plan.add(Effect::Read {
        resource: resolved_data_source(data_source.clone()),
    });
    let executable_data_source_values =
        executable_deferred_data_source_values(std::slice::from_ref(&data_source));
    let resource_origins = HashMap::from([(
        upstream.id.clone(),
        UnresolvedResource::from_pre_resolve(upstream.clone()),
    )]);
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: std::slice::from_ref(&upstream),
        compositions: std::slice::from_ref(&composition),
        managed_current: &HashMap::new(),
        data_source_values: &test_data_source_values(
            std::slice::from_ref(&data_source),
            &HashMap::new(),
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let schemas = SchemaRegistry::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &resource_origins,
            data_source_origins: std::slice::from_ref(&data_source),
        },
        compositions: std::slice::from_ref(&composition),
        bindings,
        managed_current: HashMap::new(),
        data_source_values: executable_data_source_values,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert!(
        provider.captured_data_source_reads().is_empty(),
        "module-invalid apply-resolved data source reached the provider"
    );
    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 1);
    assert_eq!(
        provider.calls(),
        vec![("create".to_string(), upstream_id.to_string())]
    );
}

fn mark_module_resource(resource: &mut Resource, instance: &str) {
    resource.module_source = Some(crate::resource::ModuleSource::expanded_module(
        "checked_module",
        instance,
        crate::resource::ModuleInstanceScope::for_call("checked_module", Some(instance), instance),
        crate::resource::CompositionCallPath::direct(crate::resource::CompositionCall {
            id: crate::parser::ModuleCallId::test_dummy_in(format!("{instance}.crn")),
            module_name: "checked_module".to_string(),
            binding: Some(absolute_binding(instance)),
            instance: instance.to_string(),
            module_source: Some("./checked_module".to_string()),
            module_directory: None,
            loop_origin: None,
        }),
    ));
}

#[tokio::test]
async fn pending_module_validation_and_require_block_first_consuming_effect() {
    let provider = MockProvider::new();
    let producer = make_resource("module-producer", &[]);
    let producer_id = producer.id.clone();
    let mut consumer = make_resource("module-consumer", &[]);
    consumer.set_attr(
        "input",
        Value::root_resource_ref("module-producer", "value", vec![]),
    );
    consumer
        .dependency_bindings
        .insert(absolute_binding("module-producer"));
    mark_module_resource(&mut consumer, "root.checked");
    let consumer_id = consumer.id.clone();
    let composition = pending_module_composition(
        "root.checked",
        vec![
            (
                "value",
                Value::root_resource_ref("module-producer", "value", vec![]),
            ),
            (
                "other",
                Value::root_resource_ref("module-producer", "other", vec![]),
            ),
        ],
        vec![
            module_not_bad_constraint("value"),
            module_require_equal_constraint("value", "other"),
        ],
    );

    provider.push_create(Ok(State::existing(
        producer_id.clone(),
        HashMap::from([
            (
                "value".to_string(),
                Value::Concrete(ConcreteValue::String("bad".into())),
            ),
            (
                "other".to_string(),
                Value::Concrete(ConcreteValue::String("different".into())),
            ),
        ]),
    )
    .with_identifier("producer-id")));
    provider.push_create(Ok(ok_state(&consumer_id)));

    let mut plan = Plan::new();
    plan.add(create_effect(producer));
    plan.add(create_effect(consumer));
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 1);
    assert_eq!(
        provider.calls(),
        vec![("create".to_string(), producer_id.to_string())]
    );
    let events = observer.events().join("\n");
    for expected in [
        "checked_module",
        "root.checked",
        "value must not be bad",
        "value and other must match",
        "'bad'",
        "'different'",
    ] {
        assert!(events.contains(expected), "events: {events}");
    }
    assert!(!events.contains("Deferred("), "events: {events}");
    assert!(!events.contains("ResourceRef {"), "events: {events}");
}

#[tokio::test]
async fn nested_forwarded_module_constraint_blocks_provider_dispatch() {
    use crate::binding_index::BindingValueSource;

    let provider = MockProvider::new();
    let mut resource = make_resource("nested-module-consumer", &[]);
    mark_module_resource(&mut resource, "root.outer.inner");
    let id = resource.id.clone();
    provider.push_create(Ok(ok_state(&id)));

    let composition = pending_module_composition(
        "root.outer.inner",
        vec![(
            "value",
            Value::root_resource_ref("forwarded", "value", vec![]),
        )],
        vec![module_not_bad_constraint("value")],
    );
    let mut bindings = ResolvedBindings::default();
    bindings.set(
        "forwarded",
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("bad".into())),
        )]),
        BindingValueSource::Local,
    );
    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.failure_count, 1);
    assert!(provider.calls().is_empty());
    let events = observer.events().join("\n");
    assert!(events.contains("root.outer.inner"), "events: {events}");
    assert!(events.contains("value must not be bad"), "events: {events}");
}

#[tokio::test]
async fn outer_module_constraint_blocks_nested_inner_consumer_provider_dispatch() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use crate::resource::LiveCompositionAttribute;

    let provider = MockProvider::new();
    let violating = Value::Concrete(ConcreteValue::String("bad".into()));
    let mut outer = pending_module_composition(
        "root.outer",
        vec![("value", violating.clone())],
        vec![module_not_bad_constraint("value")],
    );
    outer.binding = Some(absolute_binding("outer_call"));
    outer.signature.attributes.insert(
        "forwarded".to_string(),
        LiveCompositionAttribute::from_value(violating, None),
    );

    let mut inner_resource = make_resource("nested-inner-consumer", &[]);
    inner_resource.set_attr(
        "input",
        Value::root_resource_ref("outer_call", "forwarded", vec![]),
    );
    mark_module_resource(&mut inner_resource, "root.outer.inner");
    let resource_id = inner_resource.id.clone();
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: std::slice::from_ref(&inner_resource),
        compositions: std::slice::from_ref(&outer),
        managed_current: &HashMap::new(),
        data_source_values: &DataSourceValues::empty(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    provider.push_create(Ok(ok_state(&resource_id)));
    let mut plan = Plan::new();
    plan.add(create_effect(inner_resource));
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&outer),
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert!(
        provider.calls().is_empty(),
        "the inner provider received the violating forwarded value: {:?}",
        provider.captured_create_resources()
    );
    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 1);
}

#[tokio::test]
async fn module_export_constraint_blocks_top_level_consumer_provider_dispatch() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use crate::resource::LiveCompositionAttribute;

    let provider = MockProvider::new();
    let violating = Value::Concrete(ConcreteValue::String("bad".into()));
    let mut composition = pending_module_composition(
        "root.exporter",
        vec![("value", violating.clone())],
        vec![module_not_bad_constraint("value")],
    );
    composition.binding = Some(absolute_binding("exporter"));
    composition.signature.attributes.insert(
        "forwarded".to_string(),
        LiveCompositionAttribute::from_value(violating, None),
    );

    let mut top_level = make_resource("top-level-consumer", &[]);
    top_level.set_attr(
        "input",
        Value::root_resource_ref("exporter", "forwarded", vec![]),
    );
    assert!(top_level.module_source.is_none());
    let resource_id = top_level.id.clone();
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: std::slice::from_ref(&top_level),
        compositions: std::slice::from_ref(&composition),
        managed_current: &HashMap::new(),
        data_source_values: &DataSourceValues::empty(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    provider.push_create(Ok(ok_state(&resource_id)));
    let mut plan = Plan::new();
    plan.add(create_effect(top_level));
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert!(
        provider.calls().is_empty(),
        "the top-level provider received the violating module export: {:?}",
        provider.captured_create_resources()
    );
    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 1);
}

#[tokio::test]
async fn repeated_module_violation_blocks_every_dispatch_but_reports_once() {
    let provider = MockProvider::new();
    let composition = pending_module_composition(
        "root.checked",
        vec![(
            "value",
            Value::Concrete(ConcreteValue::String("bad".into())),
        )],
        vec![module_not_bad_constraint("value")],
    );
    let mut first = make_resource("first-blocked-consumer", &[]);
    mark_module_resource(&mut first, "root.checked");
    let mut second = make_resource("second-blocked-consumer", &[]);
    mark_module_resource(&mut second, "root.checked");
    let mut plan = Plan::new();
    plan.add(create_effect(first));
    plan.add(create_effect(second));
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert!(provider.calls().is_empty());
    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 2);
    let events = observer.events().join("\n");
    assert_eq!(
        events.matches("value must not be bad").count(),
        1,
        "the authored violation should only be reported once: {events}"
    );
}

#[tokio::test]
async fn module_constraint_rechecks_after_binding_changes_in_same_apply() {
    use crate::binding_index::BindingValueSource;

    let provider = MockProvider::new();
    let mut first = make_resource("first-module-consumer", &[]);
    mark_module_resource(&mut first, "root.checked");
    let first_id = first.id.clone();
    let updater = make_resource("source", &["first-module-consumer"]);
    let updater_id = updater.id.clone();
    let mut second = make_resource("second-module-consumer", &[]);
    second.set_attr("input", Value::root_resource_ref("source", "value", vec![]));
    second
        .dependency_bindings
        .insert(absolute_binding("source"));
    mark_module_resource(&mut second, "root.checked");
    let second_id = second.id.clone();

    let composition = pending_module_composition(
        "root.checked",
        vec![("value", Value::root_resource_ref("source", "value", vec![]))],
        vec![module_not_bad_constraint("value")],
    );
    let mut bindings = ResolvedBindings::default();
    bindings.set(
        "source",
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("good".into())),
        )]),
        BindingValueSource::Local,
    );
    provider.push_create(Ok(ok_state(&first_id)));
    provider.push_create(Ok(State::existing(
        updater_id.clone(),
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("bad".into())),
        )]),
    )
    .with_identifier("source-id")));
    provider.push_create(Ok(ok_state(&second_id)));

    let mut plan = Plan::new();
    plan.add(create_effect(first));
    plan.add(create_effect(updater));
    plan.add(create_effect(second));
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

    assert_eq!(result.success_count, 2);
    assert_eq!(result.failure_count, 1);
    assert_eq!(
        provider.calls(),
        vec![
            ("create".to_string(), first_id.to_string()),
            ("create".to_string(), updater_id.to_string()),
        ]
    );
}

#[tokio::test]
async fn unresolved_instance_constraint_waits_for_terminal_sweep() {
    let provider = MockProvider::new();
    let mut resource = make_resource("independent-module-resource", &[]);
    mark_module_resource(&mut resource, "root.checked");
    let resource_id = resource.id.clone();
    let composition = pending_module_composition(
        "root.checked",
        vec![(
            "value",
            Value::root_resource_ref("not-published", "value", vec![]),
        )],
        vec![module_not_bad_constraint("value")],
    );
    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    provider.push_create(Ok(ok_state(&resource_id)));
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 1);
    assert_eq!(
        provider.calls(),
        vec![("create".to_string(), resource_id.to_string())]
    );
    let events = observer.events().join("\n");
    assert!(
        events.contains("still unresolved at end of apply"),
        "events: {events}"
    );
}

#[tokio::test]
async fn unapplied_module_argument_reference_stays_pending_at_early_gate() {
    let provider = MockProvider::new();
    let mut early = make_resource("early-independent-consumer", &[]);
    mark_module_resource(&mut early, "root.checked");
    let early_id = early.id.clone();
    let producer = make_resource("future-producer", &[]);
    let producer_id = producer.id.clone();
    let composition = pending_module_composition(
        "root.checked",
        vec![(
            "value",
            Value::root_resource_ref("future-producer", "value", vec![]),
        )],
        vec![module_not_bad_constraint("value")],
    );

    provider.push_create(Ok(ok_state(&early_id)));
    provider.push_create(Ok(State::existing(
        producer_id.clone(),
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("good".into())),
        )]),
    )
    .with_identifier("producer-id")));

    let mut plan = Plan::new();
    plan.add(create_effect(early));
    plan.add(create_effect(producer));
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.success_count, 2);
    assert_eq!(result.failure_count, 0);
    assert_eq!(
        provider.calls(),
        vec![
            ("create".to_string(), early_id.to_string()),
            ("create".to_string(), producer_id.to_string()),
        ]
    );
    let events = observer.events().join("\n");
    assert!(
        !events.contains("could not resolve argument"),
        "events: {events}"
    );
}

#[tokio::test]
async fn terminal_module_constraint_sweep_rejects_unconsumed_violation() {
    use crate::binding_index::BindingValueSource;

    let provider = MockProvider::new();
    let composition = pending_module_composition(
        "root.export_only",
        vec![(
            "value",
            Value::root_resource_ref("exported", "value", vec![]),
        )],
        vec![module_not_bad_constraint("value")],
    );
    let mut bindings = ResolvedBindings::default();
    bindings.set(
        "exported",
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("bad".into())),
        )]),
        BindingValueSource::Local,
    );
    let plan = Plan::new();
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 1);
    let events = observer.events().join("\n");
    for expected in [
        "checked_module",
        "root.export_only",
        "value must not be bad",
        "'bad'",
    ] {
        assert!(events.contains(expected), "events: {events}");
    }
}

#[tokio::test]
async fn terminal_module_constraint_sweep_rejects_still_pending_input() {
    let provider = MockProvider::new();
    let composition = pending_module_composition(
        "root.unused",
        vec![(
            "value",
            Value::root_resource_ref("missing", "value", vec![]),
        )],
        vec![module_not_bad_constraint("value")],
    );
    let plan = Plan::new();
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.failure_count, 1);
    let events = observer.events().join("\n");
    for expected in [
        "checked_module",
        "root.unused",
        "value must not be bad",
        "still unresolved",
        "missing.value",
    ] {
        assert!(events.contains(expected), "events: {events}");
    }
    assert!(!events.contains("ResourceRef {"), "events: {events}");
}

#[tokio::test]
async fn terminal_pending_constraint_is_silent_after_failed_and_skipped_effects() {
    let provider = MockProvider::new();
    let failed = make_resource("failed", &[]);
    let skipped = make_resource("skipped", &["failed"]);
    provider.push_create(Err(ProviderError::api_error("upstream create failed")));
    let composition = pending_module_composition(
        "root.unused",
        vec![(
            "value",
            Value::root_resource_ref("missing", "value", vec![]),
        )],
        vec![module_not_bad_constraint("value")],
    );
    let mut plan = Plan::new();
    plan.add(create_effect(failed));
    plan.add(create_effect(skipped));
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    let events = observer.events().join("\n");

    assert_eq!(result.failure_count, 1, "events: {events}");
    assert_eq!(result.skip_count, 1, "events: {events}");
    assert!(
        events.contains("upstream create failed"),
        "events: {events}"
    );
    assert!(
        !events.contains("still unresolved at end of apply"),
        "the upstream failure already explains the pending input: {events}"
    );
    assert!(
        !events.contains("module_constraint_failed"),
        "pending constraint was double-counted: {events}"
    );
}

#[tokio::test]
async fn terminal_pending_constraint_is_silent_after_failure_without_skips() {
    let provider = MockProvider::new();
    let failed = make_resource("failed-without-dependent", &[]);
    provider.push_create(Err(ProviderError::api_error("standalone create failed")));
    let composition = pending_module_composition(
        "root.unused",
        vec![(
            "value",
            Value::root_resource_ref("missing", "value", vec![]),
        )],
        vec![module_not_bad_constraint("value")],
    );
    let mut plan = Plan::new();
    plan.add(create_effect(failed));
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    let events = observer.events().join("\n");

    assert_eq!(result.failure_count, 1, "events: {events}");
    assert_eq!(result.skip_count, 0, "events: {events}");
    assert!(
        events.contains("standalone create failed"),
        "events: {events}"
    );
    assert!(
        !events.contains("still unresolved at end of apply"),
        "the provider failure already explains the pending input: {events}"
    );
    assert!(
        !events.contains("module_constraint_failed"),
        "pending constraint was double-counted: {events}"
    );
}

#[tokio::test]
async fn terminal_pending_constraint_is_silent_after_skip_without_failure() {
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    let provider = MockProvider::new();
    let target_id = ResourceId::with_identity("test", "wait-target");
    let pending_state = State::existing(
        target_id.clone(),
        HashMap::from([(
            "status".to_string(),
            Value::Concrete(ConcreteValue::String("PENDING".into())),
        )]),
    )
    .with_identifier("target-id");
    provider.push_read(Ok(pending_state.clone()));
    let composition = pending_module_composition(
        "root.unused",
        vec![(
            "value",
            Value::root_resource_ref("missing", "value", vec![]),
        )],
        vec![module_not_bad_constraint("value")],
    );
    let mut plan = Plan::new();
    plan.add(Effect::Wait {
        identity: crate::resource::absolute_binding_address_for_test("target_ready"),
        target_id: managed_wait_target(target_id.clone()),
        until: WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String("READY".into())),
        )
        .unwrap(),
        until_surface: "wait-target.status == READY".to_string(),
        timeout: std::time::Duration::from_secs(60),
        interval: std::time::Duration::from_millis(1),
        explicit_dependencies: HashSet::new(),
    });
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: std::slice::from_ref(&composition),
        bindings: ResolvedBindings::default(),
        managed_current: crate::resource::current_map_from_states(HashMap::from([(
            target_id,
            pending_state,
        )])),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    let events = observer.events().join("\n");

    assert_eq!(result.failure_count, 0, "events: {events}");
    assert_eq!(result.skip_count, 1, "events: {events}");
    assert!(events.contains("no mutator remaining"), "events: {events}");
    assert!(
        !events.contains("still unresolved at end of apply"),
        "the skipped effect already explains the pending input: {events}"
    );
    assert!(
        !events.contains("module_constraint_failed"),
        "pending constraint was double-counted: {events}"
    );
}

#[test]
fn terminal_gate_still_reports_decidable_violations_when_pending_is_suppressed() {
    let composition = pending_module_composition(
        "root.known",
        vec![(
            "value",
            Value::Concrete(ConcreteValue::String("bad".into())),
        )],
        vec![module_not_bad_constraint("value")],
    );
    let gate = ModuleConstraintGate::from_live_compositions(std::slice::from_ref(&composition));

    let error = gate
        .finish(&ResolvedBindings::default(), false)
        .expect_err("a known violation must not be suppressed with pending inputs");

    assert_eq!(error.failures().len(), 1);
    assert!(error.to_string().contains("value must not be bad"));
    assert!(!error.to_string().contains("still unresolved"));
}

#[tokio::test]
async fn execute_plan_with_pre_cancelled_token_returns_cancelled_at_t4_or_later() {
    let provider = MockProvider::new();
    let resource = make_resource("one", &[]);

    let mut plan = Plan::new();
    plan.add(create_effect(resource));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let (shutdown, cancel) = shutdown_channel();
    shutdown.request_graceful_shutdown();

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
async fn cancelled_skip_batch_publishes_one_cumulative_checkpoint() {
    let provider = MockProvider::new();
    let plan = create_independent_create_plan(["one", "two", "three"]);
    let published_skip_counts = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&published_skip_counts);
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: ExecutionCheckpointSink::new(move |snapshot| {
            recorded.lock().unwrap().push(snapshot.skip_count);
        }),
    };
    let (trigger, shutdown) = shutdown_channel();
    trigger.request_graceful_shutdown();

    let outcome = execute_plan(&provider, input, &MockObserver::new(), shutdown).await;

    let ExecutionOutcome::Cancelled(result) = outcome else {
        panic!("pre-cancelled execution must return Cancelled")
    };
    assert_eq!(result.skip_count, 3);
    assert_eq!(
        *published_skip_counts.lock().unwrap(),
        [3],
        "one loop batch must publish one cumulative snapshot"
    );
}

#[tokio::test]
async fn execute_plan_with_empty_plan_and_pre_cancelled_token_returns_completed() {
    let provider = MockProvider::new();
    let plan = Plan::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();
    let (shutdown, cancel) = shutdown_channel();
    shutdown.request_graceful_shutdown();

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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
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
        Value::root_resource_ref("cert_ready".to_string(), "id".to_string(), vec![]),
    );
    dist.dependency_bindings = [absolute_binding("cert_ready")].into_iter().collect();
    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: crate::resource::absolute_binding_address_for_test("cert_ready"),
        target_id: managed_wait_target(cert_id),
        until: WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String("READY".into())),
        )
        .unwrap(),
        until_surface: "cert.status == READY".to_string(),
        timeout: std::time::Duration::from_secs(60),
        interval: std::time::Duration::from_secs(60),
        explicit_dependencies: std::collections::HashSet::new(),
    });
    plan.add(create_effect(dist));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let outcome = execute_plan(&provider, input, &observer, cancel).await;

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
        Value::root_resource_ref("cert_ready".to_string(), "id".to_string(), vec![]),
    );
    dist.dependency_bindings = [absolute_binding("cert_ready")].into_iter().collect();

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: crate::resource::absolute_binding_address_for_test("cert_ready"),
        target_id: managed_wait_target(cert_id),
        until: WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String("READY".into())),
        )
        .unwrap(),
        until_surface: "cert.status == READY".to_string(),
        timeout: std::time::Duration::from_secs(60),
        interval: std::time::Duration::from_secs(60),
        explicit_dependencies: std::collections::HashSet::new(),
    });
    plan.add(create_effect(dist));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
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
async fn execute_plan_cleanup_priority_abandons_in_flight_and_keeps_completed_effects() {
    let provider = DelayedCountingProvider::new(std::time::Duration::ZERO)
        .with_delay("r2", std::time::Duration::from_secs(60))
        .with_delay("r3", std::time::Duration::from_secs(60));
    let observer = CancelsWhenStarted::with_cleanup_priority("r3");
    let shutdown = observer.token();
    let plan = create_independent_create_plan(["r1", "r2", "r3"]);
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(2).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let outcome = execute_plan(&provider, input, &observer, shutdown).await;
    let result = match outcome {
        ExecutionOutcome::Cancelled(result) => result,
        ExecutionOutcome::Completed(_) => panic!("cleanup-priority run returned Completed"),
    };

    assert!(
        result
            .applied_states
            .contains_key(&ResourceId::with_identity("test", "r1")),
        "the effect completed before escalation must remain available for state writeback"
    );
    assert!(
        !result
            .applied_states
            .contains_key(&ResourceId::with_identity("test", "r2")),
        "the abandoned in-flight effect has no trustworthy completion result"
    );
    assert!(
        !result
            .applied_states
            .contains_key(&ResourceId::with_identity("test", "r3")),
        "the second abandoned in-flight effect has no trustworthy completion result"
    );
    assert_eq!(provider.started_names(), vec!["r1", "r2", "r3"]);
    assert_eq!(result.interrupted_count, 2);
    let interrupted: Vec<_> = observer
        .events()
        .into_iter()
        .filter(|event| event.starts_with("interrupted:"))
        .collect();
    assert_eq!(
        interrupted,
        ["interrupted:test.r2:2/3", "interrupted:test.r3:3/3"],
        "started, abandoned effects must be interrupted in effect-index order"
    );
}

#[tokio::test]
async fn cleanup_abandonment_checkpoints_unknown_outcome_after_guest_egress() {
    let provider = DelayedCountingProvider::new(std::time::Duration::from_secs(60))
        .with_guest_egress("unknown");
    let observer = CancelsWhenStarted::with_cleanup_priority("unknown");
    let shutdown = observer.token();
    let plan = create_independent_create_plan(["unknown"]);
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = match execute_plan(&provider, input, &observer, shutdown).await {
        ExecutionOutcome::Cancelled(result) => result,
        ExecutionOutcome::Completed(_) => panic!("cleanup-priority run returned Completed"),
    };
    let id = ResourceId::with_identity("test", "unknown");
    assert!(matches!(
        result.applied_states.get(&id),
        Some(Applied::CreateOutcomeUnknown(unknown)) if unknown.id() == &id
    ));
    assert_eq!(result.interrupted_count, 1);
}

#[tokio::test]
async fn execute_plan_harvests_ready_effect_before_cleanup_priority() {
    let (trigger, shutdown) = shutdown_channel();
    let provider = ControlledReadyCreateProvider::new();
    let resource = make_resource("ready", &[]);
    let id = resource.id.clone();
    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    let observer = MockObserver::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let execution = execute_plan(&provider, input, &observer, shutdown);
    tokio::pin!(execution);
    tokio::select! {
        _ = provider.started.notified() => {}
        outcome = &mut execution => panic!("execution completed before the provider was released: {}", matches!(outcome, ExecutionOutcome::Completed(_))),
    }

    provider.complete();
    trigger.prioritize_cleanup();
    let outcome = execution.await;
    let result = match outcome {
        ExecutionOutcome::Cancelled(result) => result,
        ExecutionOutcome::Completed(_) => panic!("cleanup-priority run returned Completed"),
    };

    assert_eq!(result.success_count, 1);
    assert!(
        result.applied_states.contains_key(&id),
        "a provider result that was already ready must be harvested before shutdown"
    );
}

#[tokio::test]
async fn execute_plan_cleanup_priority_abandons_a_pending_failure_refresh() {
    let provider = PendingRefreshProvider;
    let mut desired = make_resource("target", &[]);
    desired.set_attr("value", Value::Concrete(ConcreteValue::Int(2)));
    let id = desired.id.clone();
    let current = State::existing(
        id,
        HashMap::from([("value".to_string(), Value::Concrete(ConcreteValue::Int(1)))]),
    )
    .with_identifier("target-id");
    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("target-id"),
        from: Box::new(current),
        to: resolved(desired),
        changed_attributes: vec!["value".to_string()],
    });
    let (trigger, shutdown) = shutdown_channel();
    let observer = PrioritizeCleanupOnRefresh { trigger };
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        execute_plan(&provider, input, &observer, shutdown),
    )
    .await
    .expect("cleanup priority did not abandon the pending failure refresh");

    assert!(matches!(outcome, ExecutionOutcome::Cancelled(_)));
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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        applied_state_for_test(ok_state(&rid)),
        "mock partial create".to_string(),
        vec!["computed".to_string()],
    )));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        Value::Concrete(ConcreteValue::String("raw_dsl".into())),
    );
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &CanonicalizingNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let captured = provider.captured_create_resources();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].get_attr("marker"),
        Some(&Value::Concrete(ConcreteValue::String("CANONICAL".into()))),
        "apply path must re-run normalize_desired so the provider \
         receives the canonical value, not the raw DSL spelling"
    );
}

/// Plan-time normalization may deliberately transform a literal into a form
/// that does not satisfy a constraint written for the authored DSL spelling.
/// The literal already passed the plan-time gate in authored form, so apply
/// must not re-check the normalized plan value merely because another
/// attribute on the resource needs apply-time resolution.
#[tokio::test]
async fn apply_gate_does_not_revalidate_plan_normalized_literal() {
    use crate::binding_index::BindingValueSource;
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let provider = MockProvider::new();
    let mut source = make_resource("normalized-literal", &[]);
    source.set_attr(
        "marker",
        Value::Concrete(ConcreteValue::String("raw_dsl".into())),
    );
    source.set_attr(
        "runtime",
        Value::root_resource_ref("runtime-value", "value", vec![]),
    );
    let id = source.id.clone();

    // This is the resource persisted in the plan after normalize_desired.
    let mut planned = source.clone();
    planned.set_attr(
        "marker",
        Value::Concrete(ConcreteValue::String("CANONICAL".into())),
    );

    let mut plan = Plan::new();
    plan.add(create_effect(planned));
    provider.push_create(Ok(ok_state(&id)));

    let unresolved = HashMap::from([(id.clone(), UnresolvedResource::from_pre_resolve(source))]);
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("test")
            .attribute(
                AttributeSchema::optional(
                    "marker",
                    AttributeType::refined_string(None, Some("^raw_dsl$".to_string()), None, None),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("runtime", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let mut bindings = ResolvedBindings::default();
    bindings.set(
        "runtime-value",
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("known-at-apply".into())),
        )]),
        BindingValueSource::Local,
    );
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &CanonicalizingNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    let events = observer.events().join("\n");

    assert!(
        !events.contains("Invalid value 'CANONICAL'"),
        "apply revalidated the plan-normalized literal instead of only the runtime value: {events}"
    );
    assert_eq!(
        result.failure_count, 0,
        "normalized literals were checked in authored form during planning"
    );
    assert_eq!(result.success_count, 1);
    let captured = provider.captured_create_resources();
    assert_eq!(captured.len(), 1, "the provider must be called once");
    assert_eq!(
        captured[0].get_attr("marker"),
        Some(&Value::Concrete(ConcreteValue::String("CANONICAL".into()))),
        "the provider still receives the normalized representation"
    );
}

/// A plan-normalized effect is invalid executor input unless its authored
/// pre-normalization snapshot accompanies it. Treating a missing snapshot as
/// authored silently turns the apply gate back into a full check of normalized
/// values.
#[tokio::test]
async fn plan_normalized_resource_without_authored_origin_is_an_invariant_error() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let provider = MockProvider::new();
    let mut planned = make_resource("missing-origin", &[]);
    planned.set_attr(
        "marker",
        Value::Concrete(ConcreteValue::String("CANONICAL".into())),
    );
    let id = planned.id.clone();
    provider.push_create(Ok(ok_state(&id)));

    let mut plan = Plan::new();
    plan.add(create_effect(planned));

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("test").attribute(
            AttributeSchema::optional(
                "marker",
                AttributeType::refined_string(None, Some("^raw_dsl$".to_string()), None, None),
            )
            .build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let resource_origins = HashMap::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &resource_origins,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    let events = observer.events().join("\n");

    assert_eq!(result.failure_count, 1);
    assert!(
        events.contains("missing authored value origin for plan-normalized resource"),
        "expected an executor invariant error, got: {events}"
    );
    assert!(provider.captured_create_resources().is_empty());
}

/// Saved plans carry canonical provider input plus the authored source used to
/// identify which attributes became known at apply. A literal canonicalized at
/// plan time must not be rechecked merely because a sibling reference resolves.
#[tokio::test]
async fn apply_gate_does_not_revalidate_plan_normalized_data_source_literal() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema};

    let provider = MockProvider::new();
    let producer = make_resource("data-source-origin-producer", &[]);
    let producer_id = producer.id.clone();

    let mut authored = DataSource::with_provider("test", "Lookup", "origin-check", None);
    authored.binding = Some(absolute_binding("origin-check"));
    authored.set_attr(
        "marker",
        Value::Concrete(ConcreteValue::String("raw_dsl".into())),
    );
    authored.set_attr(
        "runtime",
        Value::root_resource_ref("data-source-origin-producer", "value", vec![]),
    );
    let data_source_id = authored.id.clone();

    let mut planned = authored.clone();
    planned.set_attr(
        "marker",
        Value::Concrete(ConcreteValue::String("CANONICAL".into())),
    );

    provider.push_create(Ok(State::existing(
        producer_id.clone(),
        HashMap::from([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("known-at-apply".into())),
        )]),
    )
    .with_identifier("producer-id")));
    provider.push_read(Ok(
        State::existing(data_source_id.clone(), HashMap::new()).with_identifier("lookup-id")
    ));

    let mut plan = Plan::new();
    plan.add(create_effect(producer.clone()));
    plan.add(Effect::Read {
        resource: resolved_data_source(planned),
    });

    let executable_data_source_values =
        executable_deferred_data_source_values(std::slice::from_ref(&authored));
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: std::slice::from_ref(&producer),
        compositions: &[],
        managed_current: &HashMap::new(),
        data_source_values: &test_data_source_values(
            std::slice::from_ref(&authored),
            &HashMap::new(),
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let resource_origins = HashMap::from([(
        producer_id.clone(),
        UnresolvedResource::from_pre_resolve(producer.clone()),
    )]);
    let data_source_origins = [authored];
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (ResourceSchema::new("Lookup")
            .as_data_source()
            .attribute(
                AttributeSchema::optional(
                    "marker",
                    AttributeType::refined_string(None, Some("^raw_dsl$".to_string()), None, None),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("runtime", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &resource_origins,
            data_source_origins: &data_source_origins,
        },
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: executable_data_source_values,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    let events = observer.events().join("\n");

    assert_eq!(result.failure_count, 0, "{events}");
    assert_eq!(result.success_count, 2, "{events}");
    assert_eq!(provider.captured_data_source_reads().len(), 1);
}

/// carina#3063: the apply path must re-apply host schema canonicalization, not
/// just `normalize_desired`. Apply re-resolves from the authored source;
/// without this stage the provider would receive an unresolved identifier.
#[tokio::test]
async fn test_apply_reapplies_host_enum_canonicalization() {
    let provider = MockProvider::new();
    // `id.provider = "test"` so the per-resource factory lookup finds
    // `AliasFactory` (whose `name()` is `"test"`).
    let mut resource = Resource::with_provider("test", "sg", "a", None);
    resource.binding = Some(absolute_binding("a"));
    // Authored parser shape: a typed namespaced DSL enum identifier.
    resource.set_attr(
        "ip_protocol",
        Value::Concrete(ConcreteValue::enum_identifier(
            "test.ec2.SecurityGroup.IpProtocol.all",
        )),
    );
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    provider.push_create(Ok(ok_state(&rid)));

    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(AliasFactory)];
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &factories,
        schemas: &ALIAS_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let captured = provider.captured_create_resources();
    assert_eq!(captured.len(), 1);
    assert!(
        matches!(
            captured[0].get_attr("ip_protocol"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(canonical)))
                if canonical.identity().to_string() == "test.ec2.SecurityGroup.IpProtocol"
                    && canonical.api_value() == "all"
        ),
        "apply path must re-apply typed enum resolution"
    );
}

/// carina#3063, Update path: host enum canonicalization must be re-applied on
/// Update too, not just Create. The patch is built from the re-resolved `to`;
/// without the apply-path pass it would carry the unresolved DSL form.
#[tokio::test]
async fn test_apply_reapplies_host_enum_canonicalization_update_path() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "sg", "a", None);
    to_resource.binding = Some(absolute_binding("a"));
    to_resource.set_attr(
        "ip_protocol",
        Value::Concrete(ConcreteValue::enum_identifier(
            "test.ec2.SecurityGroup.IpProtocol.all",
        )),
    );
    let rid = to_resource.id.clone();

    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "ip_protocol".to_string(),
        Value::Concrete(ConcreteValue::String("tcp".into())),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["ip_protocol".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(AliasFactory)];
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &factories,
        schemas: &ALIAS_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let op = reqs[0]
        .patch()
        .ops
        .iter()
        .find(|op| op.key() == "ip_protocol")
        .expect("patch must contain the changed `ip_protocol` attribute");
    assert!(
        matches!(
            op.sent_value(),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(canonical)))
                if canonical.identity().to_string() == "test.ec2.SecurityGroup.IpProtocol"
                    && canonical.api_value() == "all"
        ),
        "Update patch must carry the schema-resolved enum value"
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
    resource.binding = Some(absolute_binding("a"));
    resource.set_attr(
        "subject",
        Value::Concrete(ConcreteValue::String("repo:foo:*".into())),
    );
    let rid = resource.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(resource));
    provider.push_create(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &CANON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let captured = provider.captured_create_resources();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].get_attr("subject"),
        Some(&Value::Concrete(ConcreteValue::StringList(vec![
            "repo:foo:*".into()
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
        Value::Concrete(ConcreteValue::String("raw_dsl".into())),
    );
    let rid = to_resource.id.clone();

    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "marker".to_string(),
        Value::Concrete(ConcreteValue::String("old".into())),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["marker".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &CanonicalizingNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let marker_op = reqs[0]
        .patch()
        .ops
        .iter()
        .find(|op| op.key() == "marker")
        .expect("patch must contain the changed `marker` attribute");
    assert_eq!(
        marker_op.sent_value(),
        Some(&Value::Concrete(ConcreteValue::String("CANONICAL".into()))),
        "Update patch must carry the re-normalized value, not raw DSL"
    );
}

#[tokio::test]
async fn test_apply_update_patch_preserves_provider_default_tags() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "test", "tagged", None);
    let rid = to_resource.id.clone();
    let mut desired_tags = indexmap::IndexMap::new();
    desired_tags.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("v1".into())),
    );
    to_resource.set_attr("tags", Value::Concrete(ConcreteValue::Map(desired_tags)));

    let mut current_tags = indexmap::IndexMap::new();
    current_tags.insert(
        "Name".to_string(),
        Value::Concrete(ConcreteValue::String("old".into())),
    );
    current_tags.insert(
        "ManagedBy".to_string(),
        Value::Concrete(ConcreteValue::String("carina".into())),
    );
    current_tags.insert(
        "Project".to_string(),
        Value::Concrete(ConcreteValue::String("issue-3480".into())),
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
        Value::Concrete(ConcreteValue::String("carina".into())),
    );
    default_tags.insert(
        "Project".to_string(),
        Value::Concrete(ConcreteValue::String("issue-3480".into())),
    );
    let provider_configs = vec![provider_config_with_default_tags(default_tags)];

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["tags".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &provider_configs,
        factories: &[],
        schemas: &DEFAULT_TAG_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let tags_op = reqs[0]
        .patch()
        .ops
        .iter()
        .find(|op| op.key() == "tags")
        .expect("patch must contain the changed `tags` attribute");
    let Some(Value::Concrete(ConcreteValue::Map(tags))) = tags_op.sent_value() else {
        panic!(
            "expected tags Replace value to be a map, got {:?}",
            tags_op.sent_value()
        );
    };
    assert_eq!(
        tags.get("Name"),
        Some(&Value::Concrete(ConcreteValue::String("v1".into())))
    );
    assert_eq!(
        tags.get("ManagedBy"),
        Some(&Value::Concrete(ConcreteValue::String("carina".into())))
    );
    assert_eq!(
        tags.get("Project"),
        Some(&Value::Concrete(ConcreteValue::String("issue-3480".into())))
    );
}

#[tokio::test]
async fn test_apply_effective_changed_uses_plan_time_comparison_semantics() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "a", "a", None);
    to_resource.binding = Some(absolute_binding("a"));
    let rid = to_resource.id.clone();

    to_resource.set_attr(
        "description",
        Value::Concrete(ConcreteValue::String("new".into())),
    );
    to_resource.set_attr("size", Value::Concrete(ConcreteValue::Float(1.0)));
    to_resource.set_attr(
        "items",
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("authored".into()),
        )])),
    );
    let mut desired_options = indexmap::IndexMap::new();
    desired_options.insert(
        "label".to_string(),
        Value::Concrete(ConcreteValue::String("stable".into())),
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
        Value::Concrete(ConcreteValue::String("old".into())),
    );
    from_attrs.insert("size".to_string(), Value::Concrete(ConcreteValue::Int(1)));
    from_attrs.insert(
        "items".to_string(),
        Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("authored".into())),
            Value::Concrete(ConcreteValue::String("provider-default".into())),
        ])),
    );
    let mut current_options = indexmap::IndexMap::new();
    current_options.insert(
        "label".to_string(),
        Value::Concrete(ConcreteValue::String("stable".into())),
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
        Value::Concrete(ConcreteValue::String("AES256".into())),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["description".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &AUGMENT_COMPARISON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let patched_keys: Vec<&str> = reqs[0]
        .patch()
        .ops
        .iter()
        .map(crate::provider::PatchOp::key)
        .collect();
    assert_eq!(patched_keys, vec!["description"]);
}

#[tokio::test]
async fn test_apply_effective_changed_includes_resource_ref_changed_during_resolution() {
    use crate::binding_index::BindingValueSource;

    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "a", "a", None);
    to_resource.binding = Some(absolute_binding("a"));
    let rid = to_resource.id.clone();
    to_resource.set_attr(
        "description",
        Value::root_resource_ref("dependency", "description", vec![]),
    );

    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("old".into())),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec![],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let mut bindings = ResolvedBindings::default();
    bindings.set(
        "dependency",
        HashMap::from([(
            "description".to_string(),
            Value::Concrete(ConcreteValue::String("new".into())),
        )]),
        BindingValueSource::Local,
    );
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &AUGMENT_COMPARISON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let description_op = reqs[0]
        .patch()
        .ops
        .iter()
        .find(|op| op.key() == "description")
        .expect("apply-time resolved ResourceRef must be added to the patch");
    assert_eq!(
        description_op.sent_value(),
        Some(&Value::Concrete(ConcreteValue::String("new".into())))
    );
}

#[tokio::test]
async fn test_apply_effective_changed_includes_updatable_and_skips_create_only_write_only() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "a", "a", None);
    to_resource.binding = Some(absolute_binding("a"));
    let rid = to_resource.id.clone();

    to_resource.set_attr(
        "description",
        Value::Concrete(ConcreteValue::String("new".into())),
    );
    to_resource.set_attr(
        "_provider_only",
        Value::Concrete(ConcreteValue::String("metadata".into())),
    );
    to_resource.set_attr(
        "write_only_token",
        Value::Concrete(ConcreteValue::String("secret-token".into())),
    );
    to_resource.set_attr(
        "create_only_write_only_token",
        Value::Concrete(ConcreteValue::String("create-secret-token".into())),
    );

    let mut from_attrs = HashMap::new();
    from_attrs.insert(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("old".into())),
    );
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["description".to_string()],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &AUGMENT_COMPARISON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    let patched_keys: Vec<&str> = reqs[0]
        .patch()
        .ops
        .iter()
        .map(crate::provider::PatchOp::key)
        .collect();
    // Literal attributes omitted by the plan are final; only values changed
    // by apply-time resolution are reconsidered. The ordinary write-only value
    // is reasserted, while the create-only write-only value remains excluded.
    assert_eq!(patched_keys, vec!["description", "write_only_token"]);
}

#[tokio::test]
async fn test_apply_effective_changed_skips_matching_unwrapped_secret_hash() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "a", "a", None);
    to_resource.binding = Some(absolute_binding("a"));
    let rid = to_resource.id.clone();

    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".into()),
    ))));
    to_resource.set_attr("master_password", secret_value.clone());

    let hash_json = crate::value::value_to_json(&secret_value).expect("secret hash must serialize");
    let persisted_secret = crate::value::PersistedValue::from_serialized_json(hash_json)
        .expect("state hash must use the persisted structural format")
        .decode()
        .expect("structural secret decodes to a typed value");

    let mut from_attrs = HashMap::new();
    from_attrs.insert("master_password".to_string(), persisted_secret);
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec![],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &AUGMENT_COMPARISON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    assert!(reqs[0].patch().ops.is_empty());
}

#[tokio::test]
async fn test_apply_effective_changed_skips_secret_shape_divergence() {
    let provider = MockProvider::new();
    let mut to_resource = Resource::with_provider("test", "a", "a", None);
    to_resource.binding = Some(absolute_binding("a"));
    let rid = to_resource.id.clone();

    let secret_value = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
        ConcreteValue::String("my-password".into()),
    ))));
    to_resource.set_attr(
        "master_password",
        Value::Concrete(ConcreteValue::List(vec![
            secret_value.clone(),
            secret_value.clone(),
        ])),
    );

    let hash_json = crate::value::value_to_json(&secret_value).expect("secret hash must serialize");
    let typed_hash = crate::value::PersistedValue::from_serialized_json(hash_json)
        .expect("structural secret must decode")
        .decode()
        .expect("structural secret is non-null");

    let mut from_attrs = HashMap::new();
    from_attrs.insert("master_password".to_string(), typed_hash);
    let from_state = State::existing(rid.clone(), from_attrs).with_identifier("id-123");

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec![],
    });
    provider.push_update(Ok(ok_state(&rid)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &SecretListToScalarNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &AUGMENT_COMPARISON_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 1);

    let reqs = provider.captured_update_requests();
    assert_eq!(reqs.len(), 1);
    assert!(
        reqs[0].patch().ops.is_empty(),
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
        Value::Concrete(ConcreteValue::String("raw_dsl".into())),
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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &CanonicalizingNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
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
        Some(&Value::Concrete(ConcreteValue::String("CANONICAL".into()))),
        "nested value under a ref-bearing resource must be \
         re-normalized at apply, not reverted to raw DSL"
    );
    // The ref itself must still have resolved correctly.
    assert_eq!(
        b.get_attr("ref_a"),
        Some(&Value::Concrete(ConcreteValue::String("id-123".into()))),
        "ResourceRef must resolve from a's post-create state"
    );
}

/// #3885: a value alias of a for-expanded address must already be a canonical
/// ResourceRef before execution. This drives authored source through parsing,
/// pre-apply resolution, and the real apply scheduler.
#[tokio::test]
async fn apply_resolves_field_access_through_aliased_for_expanded_address() {
    let provider = MockProvider::new();
    let mut parsed = crate::parser::parse(
        r#"
            let roles = for n in [0] {
                mock.test.Thing { name = "role" }
            }
            let r0 = roles[0]
            let consumer = mock.test.Thing { name = r0.identifier }
        "#,
        &crate::parser::ProviderContext::default(),
    )
    .expect("reference alias source parses");
    crate::parser::resolve_resource_refs(&mut parsed)
        .expect("pre-apply reference resolution succeeds");
    let parsed = parsed.resolve_root(&RootScope::new());

    let role = parsed
        .resources
        .iter()
        .find(|resource| {
            resource
                .binding
                .as_ref()
                .map(AbsoluteBindingAddress::as_str)
                == Some("roles[0]")
        })
        .expect("for-expanded role")
        .clone();
    let consumer = parsed
        .resources
        .iter()
        .find(|resource| {
            resource
                .binding
                .as_ref()
                .map(AbsoluteBindingAddress::as_str)
                == Some("consumer")
        })
        .expect("consumer")
        .clone();
    assert_eq!(
        consumer.get_attr("name"),
        Some(&Value::root_resource_ref("roles[0]", "identifier", vec![])),
        "alias access must be canonical before apply"
    );
    assert!(
        consumer
            .dependency_bindings
            .contains(&absolute_binding("roles[0]"))
    );

    let mut role_attrs = HashMap::new();
    role_attrs.insert(
        "identifier".to_string(),
        Value::Concrete(ConcreteValue::String("role-0-id".into())),
    );
    provider.push_create(Ok(
        State::existing(role.id.clone(), role_attrs).with_identifier("role-0-id")
    ));
    provider.push_create(Ok(ok_state(&consumer.id)));

    let mut plan = Plan::new();
    plan.add(create_effect(role));
    plan.add(create_effect(consumer.clone()));
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );
    assert_eq!(result.success_count, 2);
    assert_eq!(result.failure_count, 0);
    let created = provider.captured_create_resources();
    let applied_consumer = created
        .iter()
        .find(|resource| resource.id == consumer.id)
        .expect("consumer reached provider");
    assert_eq!(
        applied_consumer.get_attr("name"),
        Some(&Value::Concrete(ConcreteValue::String("role-0-id".into())))
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
            resources: &'a mut [crate::resource::ProviderResource],
        ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
            Box::pin(async move {
                let mut guard = self.store.lock().await;
                *guard += 1;
                let n = *guard;
                drop(guard);
                for r in resources.iter_mut() {
                    r.set_attr(
                        "normalize_count",
                        Value::Concrete(ConcreteValue::String(n.to_string().into())),
                    );
                }
                Ok(())
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
        ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
            crate::provider::ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> crate::provider::BoxFuture<'a, crate::provider::ProviderResult<()>> {
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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &normalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
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
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
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
            Some(Value::Concrete(ConcreteValue::String(s))) => {
                Some(s.expose_plaintext().to_owned())
            }
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
        identifier: crate::effect::fixture_applied_identifier("id-123"),
        generation: EffectGeneration::Current,
        directives: Directives::default(),
        binding: None,
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });

    provider.push_delete(Ok(()));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.success_count, 1);
    assert!(
        result
            .successfully_deleted
            .contains(&DeletedInstanceKey::current(rid.clone(), "id-123"))
    );
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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let _ =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.success_count, 0);
    assert_eq!(result.failure_count, 0);
    assert!(provider.calls().is_empty());
}

#[tokio::test]
async fn release_list_keys_never_reaches_provider_executor() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};

    let provider = MockProvider::new();
    let schema = ResourceSchema::new("test.KeyedSettings")
        .attribute(
            AttributeSchema::optional(
                "settings",
                AttributeType::list(AttributeType::struct_(
                    "Setting",
                    vec![
                        StructField::required("key", AttributeType::string()).build(),
                        StructField::required("value", AttributeType::string()).build(),
                    ],
                )),
            )
            .keyed_partial_update("key")
            .build(),
        )
        .validate()
        .expect("release fixture schema is valid");
    let key_field = schema
        .effective_attribute("settings")
        .and_then(|attribute| attribute.keyed_partial_update())
        .expect("release fixture has keyed semantics")
        .key_field()
        .clone();
    let mut plan = Plan::new();
    plan.add(Effect::ReleaseListKeys {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "test.KeyedSettings",
            "settings",
        )),
        attribute: "settings".to_string(),
        key_field,
        keys: ReleasedListKeys::try_new(vec![ReleasedListKey::new(
            "b",
            Value::Concrete(ConcreteValue::String("2".into())),
        )])
        .expect("release fixture has one key"),
    });

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let result = completed_result(
        execute_plan(
            &provider,
            input,
            &MockObserver::new(),
            uncancelled_shutdown(),
        )
        .await,
    );

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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
    tgw_attach.binding = Some(absolute_binding("tgw_attach"));
    tgw_attach.dependency_bindings = std::collections::BTreeSet::from([
        absolute_binding("tgw"),
        absolute_binding("vpc"),
        absolute_binding("subnet"),
    ]);

    // route depends on rt and tgw_attach (but after partial resolution,
    // transit_gateway_id points to ResourceRef { binding: "tgw" })
    let mut route = Resource::new("ec2.route", "my-route");
    route.set_attr(
        "transit_gateway_id".to_string(),
        Value::root_resource_ref("tgw".to_string(), "id".to_string(), vec![]),
    );
    route.dependency_bindings =
        std::collections::BTreeSet::from([absolute_binding("rt"), absolute_binding("tgw_attach")]);

    // Other resources
    let mut vpc = Resource::new("ec2.Vpc", "vpc");
    vpc.binding = Some(absolute_binding("vpc"));

    let mut tgw = Resource::new("ec2.transit_gateway", "tgw");
    tgw.binding = Some(absolute_binding("tgw"));

    let mut subnet = Resource::new("ec2.Subnet", "subnet");
    subnet.binding = Some(absolute_binding("subnet"));
    subnet.dependency_bindings = std::collections::BTreeSet::from([absolute_binding("vpc")]);

    let mut rt = Resource::new("ec2.RouteTable", "rt");
    rt.binding = Some(absolute_binding("rt"));
    rt.dependency_bindings = std::collections::BTreeSet::from([absolute_binding("vpc")]);

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
            _identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            Box::pin(async { Err(ProviderError::internal("not implemented")) })
        }

        fn read_data_source(
            &self,
            _resource: &ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("not implemented")) })
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
            _attempt: crate::provider::CreateAttempt,
        ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>>
        {
            let id_clone = id.clone();
            let name = id.identity_str().expect("resolved identity").to_string();
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
                    Value::Concrete(ConcreteValue::String("id-123".into())),
                );
                Ok(crate::provider::CreateOutcome::fixture_success(
                    AppliedState::new(id_clone, "id-123", attrs)
                        .expect("fixture identifier is non-empty"),
                ))
            })
        }

        unexpected_resume_create!();

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &crate::provider::AppliedIdentifier,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("not implemented")) })
        }

        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &crate::provider::AppliedIdentifier,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            Box::pin(async { Err(ProviderError::internal("not implemented")) })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: crate::effect::PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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

struct YieldingUpdateProvider {
    change_unrelated_id: bool,
    active: Arc<std::sync::atomic::AtomicUsize>,
    max_active: Arc<std::sync::atomic::AtomicUsize>,
}

impl YieldingUpdateProvider {
    fn new() -> Self {
        Self {
            change_unrelated_id: false,
            active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn violates_unrelated_id() -> Self {
        Self {
            change_unrelated_id: true,
            active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_active: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn max_active(&self) -> usize {
        self.max_active.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Provider for YieldingUpdateProvider {
    fn name(&self) -> &str {
        "yielding-update"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn read_data_source(
        &self,
        _resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        attempt: crate::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>> {
        Box::pin(async move { Err(attempt.untracked(ProviderError::internal("not implemented"))) })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        id: &ResourceId,
        identifier: &crate::provider::AppliedIdentifier,
        request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        let change_unrelated_id = self.change_unrelated_id;
        let active = self.active.clone();
        let max_active = self.max_active.clone();
        Box::pin(async move {
            let now_active = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            max_active.fetch_max(now_active, std::sync::atomic::Ordering::SeqCst);
            tokio::task::yield_now().await;
            active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

            let mut attrs = request.from().attributes.clone();
            attrs.insert(
                "tags".to_string(),
                Value::Concrete(ConcreteValue::String("new".into())),
            );
            if change_unrelated_id && id.identity_str().expect("resolved identity") == "vpc" {
                attrs.insert(
                    "id".to_string(),
                    Value::Concrete(ConcreteValue::String("provider-violated-id".into())),
                );
            }
            Ok(crate::provider::UpdateOutcome::Success {
                state: AppliedState::new(id, identifier, attrs)
                    .expect("provider input identifier is non-empty"),
            })
        })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

fn tag_update_resource(binding: &str, parent_ref: Option<&str>) -> Resource {
    let mut resource = Resource::new(
        "test",
        crate::resource::ResourceIdentity::try_from(binding.to_string()).unwrap(),
    );
    resource.binding = Some(absolute_binding(binding));
    resource.set_attr(
        "id",
        Value::Concrete(ConcreteValue::String(format!("{binding}-id").into())),
    );
    resource.set_attr("tags", Value::Concrete(ConcreteValue::String("new".into())));
    if let Some(parent) = parent_ref {
        resource.set_attr("vpc_id", Value::root_resource_ref(parent, "id", vec![]));
    }
    resource
}

fn tag_update_state(id: &ResourceId, binding: &str) -> State {
    State::existing(
        id.clone(),
        HashMap::from([
            (
                "id".to_string(),
                Value::Concrete(ConcreteValue::String(format!("{binding}-id").into())),
            ),
            (
                "tags".to_string(),
                Value::Concrete(ConcreteValue::String("old".into())),
            ),
        ]),
    )
    .with_identifier(format!("{binding}-id"))
}

async fn run_tag_sweep(parallelism: NonZeroUsize) -> usize {
    let mut resources = Vec::new();
    resources.push(tag_update_resource("vpc", None));
    for idx in 0..12 {
        resources.push(tag_update_resource(&format!("child{idx}"), Some("vpc")));
    }

    let mut current_states = HashMap::new();
    let mut plan = Plan::new();
    for resource in &resources {
        let binding = resource.binding.as_ref().unwrap().as_str();
        let from = tag_update_state(&resource.id, binding);
        current_states.insert(resource.id.clone(), from.clone());
        plan.add(Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
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
    let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
        managed: &resources,
        compositions: &[],
        managed_current: &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        data_source_values: &DataSourceValues::empty(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let provider = YieldingUpdateProvider::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved_resources,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings,
        managed_current: crate::resource::current_map_from_states(current_states),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.success_count, 13);
    assert_eq!(result.failure_count, 0);
    provider.max_active()
}

async fn run_provider_contract_case(unknown_read: bool) -> usize {
    let mut parent = tag_update_resource("vpc", None);
    let mut child = tag_update_resource("child", None);
    if unknown_read {
        child.directives.depends_on.push(absolute_binding("vpc"));
    } else {
        child.set_attr("vpc_id", Value::root_resource_ref("vpc", "id", vec![]));
    }
    parent.binding = Some(absolute_binding("vpc"));
    child.binding = Some(absolute_binding("child"));
    let resources = vec![parent, child];

    let mut current_states = HashMap::new();
    let mut plan = Plan::new();
    for resource in &resources {
        let binding = resource.binding.as_ref().unwrap().as_str();
        let from = tag_update_state(&resource.id, binding);
        current_states.insert(resource.id.clone(), from.clone());
        plan.add(Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                from.id.identity_display().to_string(),
            ),
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
    let bindings = ResolvedBindings::pre_apply_live(crate::binding_index::LivePreApplyInputs {
        managed: &resources,
        compositions: &[],
        managed_current: &crate::resource::into_plan_input_current_map_from_states(
            current_states.clone(),
            &crate::schema::SchemaRegistry::new(),
            &[],
        ),
        data_source_values: &DataSourceValues::empty(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let provider = YieldingUpdateProvider::violates_unrelated_id();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved_resources,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings,
        managed_current: crate::resource::current_map_from_states(current_states),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(2).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
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
    let max_active = run_tag_sweep(NonZeroUsize::new(8).unwrap()).await;
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
    let max_active = run_tag_sweep(NonZeroUsize::new(1).unwrap()).await;
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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
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
            Value::Concrete(ConcreteValue::String(val.to_string().into())),
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
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        identifier: crate::effect::fixture_applied_identifier("vpc-123"),
        generation: EffectGeneration::Current,
        directives: Directives::default(),
        binding: Some(absolute_binding("vpc")),
        dependencies: HashSet::new(), // vpc has no deps
        explicit_dependencies: HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });
    plan.add(Effect::Delete {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Subnet",
            "subnet",
        )),
        identifier: crate::effect::fixture_applied_identifier("subnet-456"),
        generation: EffectGeneration::Current,
        directives: Directives::default(),
        binding: Some(absolute_binding("subnet")),
        dependencies: HashSet::from([absolute_binding("vpc")]), // subnet depends on vpc
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

#[test]
fn orphan_delete_waits_for_update_that_previously_depended_on_it() {
    let consumer_id = ResourceId::with_identity("test.Consumer", "consumer");
    let orphan_id = ResourceId::with_identity("test.Dependency", "dependency");
    let desired = Resource::new("test.Consumer", "consumer")
        .with_root_binding("consumer")
        .with_attribute("version", Value::Concrete(ConcreteValue::Int(2)));
    let consumer_state = State::existing(
        consumer_id.clone(),
        HashMap::from([(
            "version".to_string(),
            Value::Concrete(ConcreteValue::Int(1)),
        )]),
    )
    .with_identifier("consumer-id")
    .with_root_dependency_bindings(std::collections::BTreeSet::from(["dependency".to_string()]));
    let orphan_state =
        State::existing(orphan_id.clone(), HashMap::new()).with_identifier("dependency-id");
    let current_states = HashMap::from([(consumer_id, consumer_state)]);
    let base_schemas = crate::schema::SchemaRegistry::new();
    let (schemas, comparison_provider) =
        crate::provider::comparison_fixture_for_test(&base_schemas, std::iter::once(&desired.id));
    let plan_input_states = crate::resource::into_plan_input_current_map_from_states(
        current_states,
        &schemas,
        std::slice::from_ref(&desired),
    );
    let desired = ResolvedResource::collect_resolved([desired]).unwrap();
    let orphans = crate::differ::Orphans::from([crate::differ::Orphan::from_state(
        orphan_state,
        Some(absolute_binding("dependency")),
        Directives::default(),
    )]);
    let plan = crate::differ::create_plan(
        &desired,
        &[],
        &comparison_provider,
        crate::differ::PlanPriorStates::new(&plan_input_states, &orphans),
        &HashMap::new(),
        &schemas,
        &crate::provider::RawSavedAttrs::default().lift(&schemas),
        &HashMap::new(),
        &[],
    )
    .expect("test provider hints should succeed");

    let update_idx = plan
        .effects()
        .iter()
        .position(|effect| matches!(effect, Effect::Update { .. }))
        .expect("Consumer must be updated");
    let delete_idx = plan
        .effects()
        .iter()
        .position(|effect| effect.resource_id() == &orphan_id)
        .expect("Orphan dependency must be deleted");
    let Effect::Delete {
        blocked_by_updates, ..
    } = &plan.effects()[delete_idx]
    else {
        panic!("Expected orphan Delete");
    };
    assert_eq!(
        blocked_by_updates,
        &HashSet::from([ResourceIdentity::new("consumer")])
    );

    let levels = build_dependency_levels(plan.effects(), &HashMap::new(), &[]);
    let update_level = levels
        .iter()
        .position(|level| level.contains(&update_idx))
        .unwrap();
    let delete_level = levels
        .iter()
        .position(|level| level.contains(&delete_idx))
        .unwrap();
    assert!(
        update_level < delete_level,
        "Consumer update must run before orphan delete; levels: {levels:?}"
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
            .expect("dependency analysis must resolve")
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
        identifier: crate::effect::fixture_applied_identifier("id-original"),
        from: Box::new(from_state),
        to: resolved(to_resource),
        changed_attributes: vec!["some_attr".to_string()],
    });

    let updated_state =
        State::existing(ra_id.clone(), HashMap::new()).with_identifier("id-updated");
    provider.push_update(Ok(updated_state));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        identifier: crate::effect::fixture_applied_identifier("vpc-123"),
        generation: EffectGeneration::Current,
        directives: Directives::default(),
        binding: Some(absolute_binding("vpc")),
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });
    plan.add(Effect::Delete {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Subnet",
            "subnet",
        )),
        identifier: crate::effect::fixture_applied_identifier("subnet-456"),
        generation: EffectGeneration::Current,
        directives: Directives::default(),
        binding: Some(absolute_binding("subnet")),
        dependencies: HashSet::from([absolute_binding("vpc")]),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });

    let deps =
        build_dependency_analysis(plan.effects(), &HashMap::new(), &[], ScheduleInputs::Apply)
            .expect("dependency analysis must resolve")
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
    vpc.binding = Some(absolute_binding("vpc"));
    vpc.set_attr(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
    );
    let vpc_id = vpc.id.clone();

    // Subnet resource that references vpc.vpc_id
    let mut subnet = Resource::new("test", "subnet");
    subnet.set_attr(
        "vpc_id",
        Value::root_resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    subnet.set_attr(
        "cidr_block",
        Value::Concrete(ConcreteValue::String("10.0.1.0/24".into())),
    );
    subnet.dependency_bindings = std::collections::BTreeSet::from([absolute_binding("vpc")]);
    let subnet_id = subnet.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(vpc));
    plan.add(create_effect(subnet));

    // VPC create returns state with vpc_id
    let vpc_state = State::existing(
        vpc_id.clone(),
        vec![(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-12345".into())),
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
            Value::Concrete(ConcreteValue::String("subnet-67890".into())),
        )]
        .into_iter()
        .collect(),
    )
    .with_identifier("subnet-67890");
    provider.push_create(Ok(subnet_state));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        Some(&Value::Concrete(ConcreteValue::String("vpc-12345".into()))),
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
            .push(result.map(|state| {
                crate::provider::CreateOutcome::fixture_success(applied_state_for_test(state))
            }));
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
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn read_data_source(
        &self,
        _resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: crate::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>> {
        let id_str = id.to_string();
        let attrs = request.resource().as_resource().resolved_attributes();
        self.create_log.lock().unwrap().push((id_str, attrs));
        let result = self.create_results.lock().unwrap().remove(0);
        Box::pin(async move { result.map_err(|error| attempt.untracked(error)) })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Err(ProviderError::internal("not implemented")) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

struct CascadeReplaceProvider {
    create_log: Arc<Mutex<CreateLog>>,
    call_log: Arc<Mutex<Vec<(String, String)>>>,
}

impl CascadeReplaceProvider {
    fn new() -> Self {
        Self {
            create_log: Arc::new(Mutex::new(Vec::new())),
            call_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn create_calls(&self) -> Vec<(String, HashMap<String, Value>)> {
        self.create_log.lock().unwrap().clone()
    }

    fn calls(&self) -> Vec<(String, String)> {
        self.call_log.lock().unwrap().clone()
    }
}

impl Provider for CascadeReplaceProvider {
    fn name(&self) -> &str {
        "cascade_replace"
    }

    fn read(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(async { Err(ProviderError::internal("read not used")) })
    }

    fn read_data_source(
        &self,
        _resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: crate::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>> {
        let id = id.clone();
        let attrs = request.resource().as_resource().resolved_attributes();
        self.call_log
            .lock()
            .unwrap()
            .push(("create".to_string(), id.to_string()));
        self.create_log
            .lock()
            .unwrap()
            .push((id.to_string(), attrs.clone()));

        Box::pin(async move {
            if id.resource_type == "ec2.Vpc" {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                return Ok(crate::provider::CreateOutcome::fixture_success(
                    AppliedState::new(
                        id,
                        "vpc-new",
                        HashMap::from([(
                            "vpc_id".to_string(),
                            Value::Concrete(ConcreteValue::String("vpc-new".into())),
                        )]),
                    )
                    .expect("fixture identifier is non-empty"),
                ));
            }

            if id.resource_type == "ec2.Subnet" {
                return Ok(crate::provider::CreateOutcome::fixture_success(
                    AppliedState::new(
                        id,
                        "subnet-new",
                        HashMap::from([(
                            "subnet_id".to_string(),
                            Value::Concrete(ConcreteValue::String("subnet-new".into())),
                        )]),
                    )
                    .expect("fixture identifier is non-empty"),
                ));
            }

            Err(attempt.untracked(ProviderError::internal(format!("unexpected create {id}"))))
        })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        Box::pin(async { Err(ProviderError::internal("update not used")) })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let id = id.clone();
        self.call_log
            .lock()
            .unwrap()
            .push(("delete".to_string(), id.to_string()));
        Box::pin(async { Ok(()) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn cascading_replacement_child_create_uses_new_parent_binding() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use crate::differ::create_plan_with_cascades;
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};

    fn string(value: &str) -> Value {
        Value::Concrete(ConcreteValue::String(value.to_string().into()))
    }

    fn state(id: &ResourceId, attrs: &[(&str, Value)], identifier: &str) -> State {
        State::existing(
            id.clone(),
            attrs
                .iter()
                .map(|(key, value)| ((*key).to_string(), value.clone()))
                .collect(),
        )
        .with_identifier(identifier)
    }

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "",
        (ResourceSchema::new("ec2.Vpc")
            .attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::provider_populated("vpc_id", AttributeType::string()).build(),
            )
            .with_coexisting_replacement())
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "",
        (ResourceSchema::new("ec2.Subnet")
            .attribute(
                AttributeSchema::optional("vpc_id", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("cidr_block", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("availability_zone", AttributeType::string())
                    .create_only()
                    .build(),
            )
            .attribute(
                AttributeSchema::provider_populated("subnet_id", AttributeType::string()).build(),
            )
            .with_coexisting_replacement())
        .validate()
        .expect("schema fixture must be valid"),
    );

    let mut managed_vpc = Resource::new("ec2.Vpc", "vpc")
        .with_root_binding("vpc")
        .with_attribute("cidr_block", string("10.221.0.0/16"));
    managed_vpc.directives.create_before_destroy = true;
    let unresolved_vpc = managed_vpc.clone();

    let managed_subnet = Resource::new("ec2.Subnet", "subnet")
        .with_root_binding("subnet")
        .with_attribute("vpc_id", string("vpc-old"))
        .with_attribute("cidr_block", string("10.221.1.0/24"))
        .with_attribute("availability_zone", string("ap-northeast-1c"));
    let unresolved_subnet = Resource::new("ec2.Subnet", "subnet")
        .with_root_binding("subnet")
        .with_attribute("vpc_id", Value::root_resource_ref("vpc", "vpc_id", vec![]))
        .with_attribute("cidr_block", string("10.221.1.0/24"))
        .with_attribute("availability_zone", string("ap-northeast-1c"));

    let vpc_id = managed_vpc.id.clone();
    let subnet_id = managed_subnet.id.clone();
    let current_states = HashMap::from([
        (
            vpc_id.clone(),
            state(
                &vpc_id,
                &[
                    ("cidr_block", string("10.220.0.0/16")),
                    ("vpc_id", string("vpc-old")),
                ],
                "vpc-old",
            ),
        ),
        (
            subnet_id.clone(),
            state(
                &subnet_id,
                &[
                    ("vpc_id", string("vpc-old")),
                    ("cidr_block", string("10.220.1.0/24")),
                    ("availability_zone", string("ap-northeast-1a")),
                ],
                "subnet-old",
            ),
        ),
    ]);
    let plan_input_states = crate::resource::into_plan_input_current_map_from_states(
        current_states.clone(),
        &schemas,
        &[managed_vpc.clone(), managed_subnet.clone()],
    );
    let managed_resources = vec![managed_vpc, managed_subnet];
    let authored_resources = vec![unresolved_vpc, unresolved_subnet];
    let unresolved_resources = authored_resources
        .iter()
        .cloned()
        .map(|resource| {
            (
                resource.id.clone(),
                UnresolvedResource::from_pre_resolve(resource),
            )
        })
        .collect::<HashMap<_, _>>();
    let unresolved = ResolvedResource::collect_resolved(authored_resources).unwrap();
    let managed = ResolvedResource::collect_resolved(managed_resources.clone()).unwrap();
    let (schemas, comparison_provider) = crate::provider::comparison_fixture_for_test(
        &schemas,
        managed.iter().map(|resource| &resource.id),
    );
    let plan = create_plan_with_cascades(
        &managed,
        &unresolved,
        &[],
        &comparison_provider,
        crate::differ::PlanPriorStates::new(&plan_input_states, &crate::differ::Orphans::new()),
        &HashMap::new(),
        &schemas,
        &crate::provider::RawSavedAttrs::default().lift(&schemas),
        &HashMap::new(),
        &[],
        &std::collections::HashSet::new(),
    )
    .expect("test provider hints should succeed");
    assert_eq!(plan.replace_display_info().count(), 2);

    let deps = build_dependency_analysis(
        plan.effects(),
        &unresolved_resources,
        &[],
        ScheduleInputs::Apply,
    )
    .expect("dependency analysis must resolve")
    .into_deps_of();
    let vpc_create_idx = plan
        .effects()
        .iter()
        .position(|effect| matches!(effect, Effect::Create(resource) if resource.id == vpc_id))
        .expect("plan should contain replacement VPC create");
    let subnet_create_idx = plan
        .effects()
        .iter()
        .position(|effect| matches!(effect, Effect::Create(resource) if resource.id == subnet_id))
        .expect("plan should contain replacement subnet create");
    assert!(
        deps[&subnet_create_idx].contains(&vpc_create_idx),
        "subnet create must depend on replacement VPC create; deps: {deps:?}; effects: {:?}",
        plan.effects()
    );
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &managed_resources,
        compositions: &[],
        managed_current: &plan_input_states,
        data_source_values: &DataSourceValues::empty(),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });
    let provider = CascadeReplaceProvider::new();
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved_resources,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings,
        managed_current: crate::resource::current_map_from_states(current_states),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.failure_count, 0, "events: {:?}", observer.events());

    let calls = provider.create_calls();
    let subnet_create = calls
        .iter()
        .find(|(id, _)| id == "ec2.Subnet.subnet")
        .unwrap_or_else(|| panic!("subnet create missing from calls: {calls:?}"));
    assert_eq!(
        subnet_create.1.get("vpc_id"),
        Some(&Value::Concrete(ConcreteValue::String("vpc-new".into()))),
        "subnet create must resolve vpc.vpc_id from the replacement VPC state; calls: {calls:?}"
    );

    let call_log = provider.calls();
    let vpc_create_pos = call_position(&call_log, "create", "ec2.Vpc.vpc");
    let subnet_create_pos = call_position(&call_log, "create", "ec2.Subnet.subnet");
    assert!(
        vpc_create_pos < subnet_create_pos,
        "replacement parent create must dispatch before dependent child create; calls: {call_log:?}"
    );
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
        Value::root_resource_ref("cert_issued".to_string(), "arn".to_string(), vec![]),
    );
    dist.dependency_bindings = [absolute_binding("cert_issued")].into_iter().collect();
    let dist_id = dist.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: crate::resource::absolute_binding_address_for_test("cert_issued"),
        target_id: managed_wait_target(cert_id.clone()),
        until: WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String("ISSUED".into())),
        )
        .unwrap(),
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
        Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".into())),
    );
    provider.push_create(Ok(
        State::existing(cert_id.clone(), create_attrs).with_identifier("acm-cert-id")
    ));
    // wait reads: PENDING → PENDING → ISSUED
    let mut pending = HashMap::new();
    pending.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".into())),
    );
    let mut issued = HashMap::new();
    issued.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".into())),
    );
    issued.insert(
        "arn".to_string(),
        Value::Concrete(ConcreteValue::String("arn:aws:acm:...".into())),
    );
    provider.push_read(Ok(State::existing(cert_id.clone(), pending.clone())));
    provider.push_read(Ok(State::existing(cert_id.clone(), pending)));
    provider.push_read(Ok(State::existing(cert_id.clone(), issued)));
    // create dist → succeeds
    provider.push_create(Ok(ok_state(&dist_id)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
    dist.binding = Some(absolute_binding("dist"));
    let mut viewer_certificate = indexmap::IndexMap::new();
    viewer_certificate.insert(
        "acm_certificate_arn".to_string(),
        Value::root_resource_ref(
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
        dist.dependency_bindings
            .contains(&absolute_binding("cert_issued")),
        "get_resource_value_ref_dependencies must recover the nested \
         `cert_issued` ref; got {:?}",
        dist.dependency_bindings
    );

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: crate::resource::absolute_binding_address_for_test("cert_issued"),
        target_id: managed_wait_target(cert_id.clone()),
        until: WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String("ISSUED".into())),
        )
        .unwrap(),
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
        Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".into())),
    );
    provider.push_create(Ok(
        State::existing(cert_id.clone(), create_attrs).with_identifier("acm-cert-id")
    ));
    let mut pending = HashMap::new();
    pending.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("PENDING_VALIDATION".into())),
    );
    let mut issued = HashMap::new();
    issued.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".into())),
    );
    issued.insert(
        "certificate_arn".to_string(),
        Value::Concrete(ConcreteValue::String(
            "arn:aws:acm:us-east-1:111:certificate/abc"
                .to_string()
                .into(),
        )),
    );
    provider.push_read(Ok(State::existing(cert_id.clone(), pending)));
    provider.push_read(Ok(State::existing(cert_id.clone(), issued)));
    provider.push_create(Ok(ok_state(&dist_id)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        identity: crate::resource::absolute_binding_address_for_test("cert_issued"),
        target_id: managed_wait_target(cert_id.clone()),
        until: WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String("ISSUED".into())),
        )
        .unwrap(),
        until_surface: "cert.status == ISSUED".to_string(),
        timeout: std::time::Duration::from_secs(60),
        interval: std::time::Duration::from_millis(1),
        explicit_dependencies: std::collections::HashSet::new(),
    });

    let mut issued = HashMap::new();
    issued.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("ISSUED".into())),
    );
    provider.push_create(Ok(
        State::existing(cert_id.clone(), issued.clone()).with_identifier("acm-cert-id")
    ));
    provider.push_read(Ok(State::existing(cert_id.clone(), issued)));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
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
        r.binding = Some(absolute_binding("cert"));
        r.set_attr(
            "domain_name",
            Value::Concrete(ConcreteValue::String("example.com".into())),
        );
        r
    };
    let cert_id = cert.id.clone();

    // The dependent resource mirrors the failing route53 RecordSet
    // attributes from the issue:
    //   resource_records = [cert.domain_validation_options[0].resource_record_value]
    let record = {
        let mut r = Resource::new("test", "record");
        r.binding = Some(absolute_binding("record"));
        r.dependency_bindings = [absolute_binding("cert")].into_iter().collect();
        let value_path = AccessPath::root_with_segments(
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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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

#[tokio::test]
async fn test_apply_rejects_wrong_shape_access_without_wait_guidance() {
    use crate::resource::{AccessPath, ConcreteValue, PathSegment, Subscript};

    let provider = MockProvider::new();
    let mut consumer = Resource::new("test", "consumer");
    consumer.binding = Some(absolute_binding("consumer"));
    consumer.dependency_bindings.insert(absolute_binding("q"));
    consumer.set_attr(
        "name",
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root_with_segments(
                "q",
                "identifier",
                vec![PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                }],
            ),
        }),
    );
    let consumer_id = consumer.id.clone();

    let mut plan = Plan::new();
    plan.add(create_effect(consumer));
    // Keep a slot available so a regression that forwards the unresolved
    // value reaches the assertion below instead of panicking in the mock.
    provider.push_create(Ok(
        State::existing(consumer_id.clone(), HashMap::new()).with_identifier("consumer-id")
    ));

    let mut bindings = ResolvedBindings::default();
    bindings.set(
        "q",
        HashMap::from([(
            "identifier".to_string(),
            Value::Concrete(ConcreteValue::String("q-123".into())),
        )]),
        crate::binding_index::BindingValueSource::Local,
    );

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.success_count, 0, "events: {:?}", observer.events());
    assert_eq!(result.failure_count, 1, "events: {:?}", observer.events());
    assert!(
        provider
            .captured_create_resources()
            .iter()
            .all(|resource| resource.id != consumer_id),
        "wrong-shape value must fail before the provider boundary"
    );
    let events = observer.events();
    let failure = events
        .iter()
        .find(|event| event.starts_with("failed:") && event.contains("consumer"))
        .expect("consumer failure event");
    assert!(
        failure.contains("cannot apply access [0] to string"),
        "unexpected apply error: {failure}"
    );
    assert!(
        !failure.contains("wait"),
        "wrong shape is permanent and must not suggest waiting: {failure}"
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
        r.binding = Some(absolute_binding("cert"));
        r.set_attr(
            "domain_name",
            Value::Concrete(ConcreteValue::String("example.com".into())),
        );
        r
    };
    let cert_id = cert.id.clone();

    // Downstream: route53 RecordSet referencing the cert's
    // chained-access path. Uses the post-aws#295 *nested* shape:
    // `resource_record` is a struct with `name`/`type`/`value`.
    let record = {
        let mut r = Resource::new("test", "record");
        r.binding = Some(absolute_binding("record"));
        r.dependency_bindings = [absolute_binding("cert")].into_iter().collect();
        let chained_dvo = |leaf: &str| {
            AccessPath::root_with_segments(
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
        Value::Concrete(ConcreteValue::String("_abc.example.com.".into())),
    );
    rr.insert(
        "type".to_string(),
        Value::Concrete(ConcreteValue::String("CNAME".into())),
    );
    rr.insert(
        "value".to_string(),
        Value::Concrete(ConcreteValue::String("_xyz.acm-validations.aws.".into())),
    );
    let mut dvo_entry: IndexMap<String, Value> = IndexMap::new();
    dvo_entry.insert(
        "domain_name".to_string(),
        Value::Concrete(ConcreteValue::String("example.com".into())),
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
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
            "_abc.example.com.".into()
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
            ConcreteValue::String("_xyz.acm-validations.aws.".into())
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
    read_identifiers: Arc<Mutex<Vec<String>>>,
}

impl IdentifierAwareProvider {
    fn new(expected_identifier: &str, created_state: State) -> Self {
        Self {
            expected_identifier: expected_identifier.to_string(),
            created_state: Mutex::new(Some(created_state)),
            read_identifiers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn read_identifiers(&self) -> Vec<String> {
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
        identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        let owned = identifier.as_str().to_string();
        self.read_identifiers.lock().unwrap().push(owned.clone());
        let id = id.clone();
        let expected = self.expected_identifier.clone();
        let state = self.created_state.lock().unwrap().clone();
        Box::pin(async move {
            if owned == expected {
                state
                    .map(fixture_read_outcome)
                    .ok_or_else(|| ProviderError::api_error("no canned state for read"))
            } else {
                Err(ProviderError::not_found(format!(
                    "wait target {id} not found (deleted out-of-band?)"
                ))
                .for_resource(id))
            }
        })
    }

    fn read_data_source(
        &self,
        _resource: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
    }

    fn create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        attempt: crate::provider::CreateAttempt,
    ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>> {
        let state = self.created_state.lock().unwrap().clone();
        Box::pin(async move {
            state
                .map(|state| {
                    crate::provider::CreateOutcome::fixture_success(applied_state_for_test(state))
                })
                .ok_or_else(|| {
                    attempt.untracked(ProviderError::api_error("no canned create state"))
                })
        })
    }

    unexpected_resume_create!();

    fn update(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
        Box::pin(async move { Err(ProviderError::api_error("update not expected")) })
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &crate::provider::AppliedIdentifier,
        _request: DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: crate::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
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
    cert.binding = Some(absolute_binding("cert"));
    let cert_id = cert.id.clone();

    // Post-create state: the provider hands back the real identifier
    // (unknown at plan time) plus the attribute the wait predicate reads.
    let mut created_attrs = HashMap::new();
    created_attrs.insert(
        "status".to_string(),
        Value::Concrete(ConcreteValue::String("issued".into())),
    );
    let created_state =
        State::existing(cert_id.clone(), created_attrs).with_identifier("cert-arn-real");

    let provider = IdentifierAwareProvider::new("cert-arn-real", created_state);

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::Wait {
        identity: crate::resource::absolute_binding_address_for_test("cert_issued"),
        target_id: managed_wait_target(cert_id.clone()),
        until: WaitPredicate::equals(
            AttrPath::single("status"),
            Value::Concrete(ConcreteValue::String("issued".into())),
        )
        .unwrap(),
        until_surface: "cert.status == \"issued\"".to_string(),
        timeout: std::time::Duration::from_secs(5),
        interval: std::time::Duration::from_millis(10),
        explicit_dependencies: std::collections::HashSet::new(),
    });

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
            .any(|identifier| identifier == "cert-arn-real"),
        "the wait read must be called with the created identifier \
         resolved from applied_states, not the plan-time None; got: {:?}",
        provider.read_identifiers()
    );
}

#[tokio::test]
async fn deferred_create_reports_iterable_reference_when_binding_missing() {
    let provider = MockProvider::new();
    let mut plan = Plan::new();
    plan.add(Effect::DeferredCreate {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        template: Box::new(validation_deferred_for_expression()),
    });

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.failure_count, 1);
    assert!(
        observer.events().iter().any(|event| {
            event.contains("failed:__deferred_for.validation_records")
                && event.contains("cert.domain_validation_options")
        }),
        "events: {:?}",
        observer.events(),
    );
}

#[tokio::test]
async fn apply_time_unbound_child_collision_with_another_desired_id_is_typed_error() {
    let (template, bindings, schemas, child) = anonymous_deferred_collision_fixture();
    let collision_id = child.id.clone();
    let provider = MockProvider::new();
    provider.push_create(Ok(
        State::existing(collision_id.clone(), HashMap::new()).with_identifier("first")
    ));
    provider.push_create(Ok(
        State::existing(collision_id.clone(), HashMap::new()).with_identifier("second")
    ));
    let mut plan = Plan::new();
    plan.add(create_effect(child));
    plan.add(Effect::DeferredCreate {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "anonymous_collision",
        )),
        template: Box::new(template),
    });
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.failure_count, 1, "events: {:?}", observer.events());
    assert!(
        observer.events().iter().any(|event| {
            event.contains("anonymous")
                && event.contains("collid")
                && event.contains(collision_id.identity_str().unwrap_or_default())
        }),
        "expected a typed anonymous-id collision: {:?}",
        observer.events(),
    );
}

#[tokio::test]
async fn apply_time_unbound_child_collision_with_unabsorbed_state_row_is_typed_error() {
    let (template, bindings, schemas, child) = anonymous_deferred_collision_fixture();
    let collision_id = child.id.clone();
    let provider = MockProvider::new();
    provider.push_create(Ok(
        State::existing(collision_id.clone(), HashMap::new()).with_identifier("new")
    ));
    let mut plan = Plan::new();
    plan.add(Effect::DeferredCreate {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "anonymous_state_collision",
        )),
        template: Box::new(template),
    });
    let current = Current::from_state(
        State::existing(collision_id.clone(), HashMap::new()).with_identifier("existing"),
    );
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::from([(collision_id.clone(), current)]),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.failure_count, 1, "events: {:?}", observer.events());
    assert!(
        observer.events().iter().any(|event| {
            event.contains("anonymous")
                && event.contains("collid")
                && event.contains(collision_id.identity_str().unwrap_or_default())
        }),
        "expected a typed state collision: {:?}",
        observer.events(),
    );
}

fn legacy_unbound_deferred_replace(
    template: crate::parser::ExpandedDeferredForExpression,
    legacy_id: &ResourceId,
) -> Effect {
    Effect::DeferredReplace(Box::new(DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: crate::resource::ResolvedResourceId::new(legacy_id.clone()),
            identifier: crate::effect::fixture_applied_identifier("existing"),
            directives: Directives::default(),
            binding: legacy_id.identity_str().map(absolute_binding),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }])
        .expect("fixture has one delete"),
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "anonymous_legacy_replace",
        )),
        template: Box::new(template),
    }))
}

#[tokio::test]
async fn unbound_deferred_replace_creates_children_before_deleting_legacy_rows() {
    let (template, bindings, schemas, child) = anonymous_deferred_collision_fixture();
    let child_id = child.id.clone();
    let legacy_id = ResourceId::with_provider_identity("mock", "test.Child", "_values[0]", None);
    let provider = MockProvider::new();
    provider.push_delete(Ok(()));
    provider.push_create(Ok(
        State::existing(child_id.clone(), HashMap::new()).with_identifier("replacement")
    ));

    let mut plan = Plan::new();
    plan.add(legacy_unbound_deferred_replace(template, &legacy_id));
    let current = Current::from_state(
        State::existing(legacy_id.clone(), HashMap::new()).with_identifier("existing"),
    );
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::from([(legacy_id.clone(), current)]),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
    assert_eq!(result.failure_count, 0, "events: {:?}", observer.events());

    let calls = provider.calls();
    assert!(
        call_position(&calls, "create", &child_id.to_string())
            < call_position(&calls, "delete", &legacy_id.to_string()),
        "anonymous children must exist before their legacy rows are deleted; calls: {calls:?}",
    );
}

#[tokio::test]
async fn unbound_deferred_replace_keeps_legacy_rows_when_gate_fails() {
    let (template, _bindings, schemas, _child) = anonymous_deferred_collision_fixture();
    let legacy_id = ResourceId::with_provider_identity("mock", "test.Child", "_values[0]", None);
    let provider = MockProvider::new();
    let mut plan = Plan::new();
    plan.add(legacy_unbound_deferred_replace(template, &legacy_id));
    let current = Current::from_state(
        State::existing(legacy_id.clone(), HashMap::new()).with_identifier("existing"),
    );
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::from([(legacy_id.clone(), current)]),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.failure_count, 1, "events: {:?}", observer.events());
    assert_eq!(result.skip_count, 1, "events: {:?}", observer.events());
    assert!(provider.calls().is_empty(), "calls: {:?}", provider.calls());
    assert!(
        result.managed_current.contains_key(&legacy_id),
        "the failed gate must leave the live legacy row intact"
    );
    let skipped = observer
        .events()
        .into_iter()
        .find(|event| event.starts_with("skipped:"))
        .expect("legacy delete must be skipped after the gate failure");
    assert!(skipped.contains("main.crn:2"), "event: {skipped}");
    assert!(!skipped.contains("_loop_"), "event: {skipped}");
}

#[tokio::test]
async fn completed_plain_anonymous_delete_releases_id_for_unbound_deferred_child() {
    let (template, bindings, schemas, child) = anonymous_deferred_collision_fixture();
    let child_id = child.id.clone();
    let provider = MockProvider::new();
    provider.push_delete(Ok(()));
    provider.push_create(Ok(
        State::existing(child_id.clone(), HashMap::new()).with_identifier("replacement")
    ));

    let mut plan = Plan::new();
    plan.add(Effect::Delete {
        id: crate::resource::ResolvedResourceId::new(child_id.clone()),
        identifier: crate::effect::fixture_applied_identifier("existing"),
        generation: EffectGeneration::Current,
        directives: Directives::default(),
        binding: None,
        dependencies: HashSet::new(),
        explicit_dependencies: HashSet::new(),
        blocked_by_updates: HashSet::new(),
    });
    plan.add(Effect::DeferredCreate {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "anonymous_remint",
        )),
        template: Box::new(template),
    });
    let current = Current::from_state(
        State::existing(child_id.clone(), HashMap::new()).with_identifier("existing"),
    );
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::from([(child_id.clone(), current)]),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &schemas,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.failure_count, 0, "events: {:?}", observer.events());
    assert_eq!(result.success_count, 2, "events: {:?}", observer.events());
    let calls = provider.calls();
    assert!(
        call_position(&calls, "delete", &child_id.to_string())
            < call_position(&calls, "create", &child_id.to_string()),
        "the old anonymous row must be deleted before the same id is re-created; calls: {calls:?}",
    );
}

#[tokio::test]
async fn deferred_create_returns_error_when_iterable_attr_missing() {
    let mut cert = Resource::new("test", "cert_missing_attr");
    cert.binding = Some(absolute_binding("cert"));
    let cert_id = cert.id.clone();

    let provider = MockProvider::new();
    provider.push_create(Ok(
        State::existing(cert_id.clone(), HashMap::new()).with_identifier("cert-id")
    ));

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::DeferredCreate {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        template: Box::new(validation_deferred_for_expression()),
    });

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.failure_count, 1);
    assert!(observer.events().iter().any(|event| {
        event.contains("failed:__deferred_for.validation_records")
            && event.contains("domain_validation_options")
    }));
}

#[tokio::test]
async fn apply_time_deferred_create_emits_failed_on_shape_mismatch() {
    let mut cert = Resource::new("test", "cert_shape_mismatch");
    cert.binding = Some(absolute_binding("cert"));
    let cert_id = cert.id.clone();
    let cert_state = State::existing(
        cert_id.clone(),
        HashMap::from([(
            "domain_validation_options".to_string(),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::new())),
        )]),
    )
    .with_identifier("cert-id");

    let provider = MockProvider::new();
    provider.push_create(Ok(cert_state));

    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::DeferredCreate {
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        template: Box::new(validation_deferred_for_expression()),
    });

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(1).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
                            Value::Concrete(ConcreteValue::String("_name".into())),
                        ),
                        (
                            "value".to_string(),
                            Value::Concrete(ConcreteValue::String("_value".into())),
                        ),
                    ]))),
                )])),
            )])),
        )]),
    )
    .with_identifier("cert-id");
    let validation_id = ResourceId::with_identity("test", "validation_records[0]");
    let validation_state =
        State::existing(validation_id.clone(), HashMap::new()).with_identifier("new-validation");
    let provider = MockProvider::new();
    provider.push_create(Ok(cert_state));
    provider.push_create(Ok(validation_state));
    provider.push_delete(Ok(()));
    provider.push_delete(Ok(()));

    let mut cert = Resource::new("test", "cert");
    cert.binding = Some(absolute_binding("cert"));
    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(vec![
            DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "test",
                    "validation_records[0]",
                )),
                identifier: crate::effect::fixture_applied_identifier("old-validation-0"),
                directives: Directives::default(),
                binding: Some(absolute_binding("validation_records[0]")),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            },
            DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "test",
                    "validation_records[1]",
                )),
                identifier: crate::effect::fixture_applied_identifier("old-validation-1"),
                directives: Directives::default(),
                binding: Some(absolute_binding("validation_records[1]")),
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
        template: Box::new(validation_deferred_for_expression()),
    })));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();

    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);
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
                            Value::Concrete(ConcreteValue::String("_name".into())),
                        ),
                        (
                            "value".to_string(),
                            Value::Concrete(ConcreteValue::String("_value".into())),
                        ),
                    ]))),
                )])),
            )])),
        )]),
    )
    .with_identifier("cert-id");
    let provider = MockProvider::new();
    provider.push_create(Ok(cert_state));
    provider.push_create(Err(ProviderError::api_error("create failed")));

    let mut cert = Resource::new("test", "cert");
    cert.binding = Some(absolute_binding("cert"));
    let mut plan = Plan::new();
    plan.add(create_effect(cert));
    plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test",
                "validation_records[0]",
            )),
            identifier: crate::effect::fixture_applied_identifier("old-validation"),
            directives: Directives::default(),
            binding: Some(absolute_binding("validation_records[0]")),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }])
        .expect("fixture has one delete"),
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        template: Box::new(validation_deferred_for_expression()),
    })));

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
        fn normalize_desired<'a>(
            &'a self,
            resources: &'a mut [crate::resource::ProviderResource],
        ) -> BoxFuture<'a, ProviderResult<()>> {
            Box::pin(async move {
                let is_alb = resources.iter().any(|resource| {
                    resource.id.identity_str().expect("resolved identity") == "alb"
                });
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
                Ok(())
            })
        }

        fn normalize_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
        ) -> BoxFuture<'a, ProviderResult<()>> {
            crate::provider::ready_noop()
        }

        fn hydrate_read_state<'a>(
            &'a self,
            _current_states: &'a mut crate::provider::StateTransformMap,
            _saved_attrs: &'a crate::provider::SavedAttrs,
        ) -> BoxFuture<'a, ProviderResult<()>> {
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
            if id.identity_str().expect("resolved identity") == "cert" {
                let _provider_lock = self.scenario.awscc_shared.lock().await;
                self.scenario.alb_waiting_for_awscc.notified().await;
                return Ok(crate::provider::CreateOutcome::fixture_success(
                    applied_state_for_test(cert_state_for_deferred_replace_deadlock(id)),
                ));
            }

            if id.identity_str().expect("resolved identity") == "alb" {
                let _provider_lock = self.scenario.awscc_shared.lock().await;
                return Ok(crate::provider::CreateOutcome::fixture_success(
                    AppliedState::new(id.clone(), "alb-id", HashMap::new())
                        .expect("fixture identifier is non-empty"),
                ));
            }

            let identifier = format!("{}-id", id.identity_str().expect("resolved identity"));
            Ok(crate::provider::CreateOutcome::fixture_success(
                AppliedState::new(id.clone(), identifier, HashMap::new())
                    .expect("derived fixture identifier is non-empty"),
            ))
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
            identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
            let id = id.clone();
            let identifier = identifier.clone();
            Box::pin(async move {
                Ok(ReadOutcome::found(
                    AppliedState::new(id, identifier.into_inner(), HashMap::new())
                        .expect("typed read identifier is non-empty"),
                ))
            })
        }

        fn read_data_source(
            &self,
            _resource: &ProviderReadyDataSource,
        ) -> BoxFuture<'_, ProviderResult<State>> {
            Box::pin(async { Err(ProviderError::internal("read_data_source not used")) })
        }

        fn create(
            &self,
            id: &ResourceId,
            _request: CreateRequest,
            attempt: crate::provider::CreateAttempt,
        ) -> BoxFuture<'_, Result<crate::provider::CreateOutcome, crate::provider::CreateFailure>>
        {
            let id = id.clone();
            Box::pin(async move {
                self.create_state(&id)
                    .await
                    .map_err(|error| attempt.untracked(error))
            })
        }

        unexpected_resume_create!();

        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &crate::provider::AppliedIdentifier,
            _request: UpdateRequest,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::UpdateOutcome>> {
            Box::pin(async { Err(ProviderError::internal("update not used")) })
        }

        fn delete(
            &self,
            id: &ResourceId,
            _identifier: &crate::provider::AppliedIdentifier,
            _request: DeleteRequest,
        ) -> BoxFuture<'_, ProviderResult<()>> {
            let id = id.clone();
            Box::pin(async move { self.delete_id(&id).await })
        }

        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: crate::effect::PlanOp,
        ) -> ProviderResult<Vec<String>> {
            Ok(Vec::new())
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
    let alb = resource_with_binding("alb", "alb");
    plan.add(create_effect(cert.clone()));
    plan.add(create_effect(alb.clone()));
    plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
        deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test",
                "validation_records[0]",
            )),
            identifier: crate::effect::fixture_applied_identifier("old-validation"),
            directives: Directives::default(),
            binding: Some(absolute_binding("validation_records[0]")),
            dependencies: HashSet::from([absolute_binding("cert")]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }])
        .expect("fixture has one delete"),
        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        template: Box::new(validation_deferred_for_expression()),
    })));

    let unresolved = HashMap::from([
        (cert_id, UnresolvedResource::from_pre_resolve(cert.clone())),
        (alb.id.clone(), UnresolvedResource::from_pre_resolve(alb)),
    ]);
    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::PlanNormalized {
            resource_origins: &unresolved,
            data_source_origins: &[],
        },
        compositions: &[],
        bindings: ResolvedBindings::default(),
        managed_current: HashMap::new(),
        data_source_values: ExecutableDataSourceValues::empty(),
        normalizer: &normalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: NonZeroUsize::new(2).unwrap(),
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };
    let observer = MockObserver::new();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        execute_plan(&provider, input, &observer, uncancelled_shutdown()),
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

fn resource_with_binding(name: &'static str, binding: &str) -> Resource {
    let mut resource = Resource::new("test", name);
    resource.binding = Some(absolute_binding(binding));
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
                            Value::Concrete(ConcreteValue::String("_name".into())),
                        ),
                        (
                            "value".to_string(),
                            Value::Concrete(ConcreteValue::String("_value".into())),
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
// pre-refreshed `data_source_states` row at apply time. Pre-fix, the
// pre-apply binding view never merged the data source's `State.attributes`
// into its binding, so the executor's `assert_fully_resolved` rejected
// the unresolved `ResourceRef` with the misleading "add a `wait` block"
// message that does not apply to data sources.
// -----------------------------------------------------------------------

/// End-to-end binding-view → executor wiring: a downstream managed
/// resource references `admin_access_roles.arns` (a `read aws.iam.Roles`
/// data source). The data source's read result lives in
/// `data_source_states[data_source.id]`. `ResolvedBindings::pre_apply` (the
/// only constructor real apply uses) must surface that read state on the
/// data source's binding so the executor's resolve step finds a concrete
/// value to hand to the provider.
///
/// Mirrors the production `carina apply` repro in carina#3252:
/// `assume_role_policy_document.statement[].principal.aws = admin_access_roles.arns`
/// on `carina-rs/infra@main`.
#[tokio::test]
async fn test_data_source_read_state_resolves_for_downstream_resource() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use crate::resource::{AccessPath, ResourceId};

    // `RecordingMockProvider` captures the resolved attribute map the
    // executor handed to `create()`, so the test can assert what the
    // provider actually saw.
    let provider = RecordingMockProvider::new();

    // Data source: `let admin_access_roles = read aws.iam.Roles { ... }`.
    // The DSL attributes hold input filters only; the produced `arns`
    // list lives in `data_source_states[ds.id]`. `with_provider` matches
    // the id shape real apply uses.
    let ds_id = ResourceId::with_provider_identity("aws", "iam.Roles", "admin_access_roles", None);
    let mut ds = DataSource::with_provider("aws", "iam.Roles", "admin_access_roles", None);
    ds.id = ds_id.clone();
    ds.binding = Some(absolute_binding("admin_access_roles"));
    ds.attributes.insert(
        "path_prefix".to_string(),
        Value::Concrete(ConcreteValue::String(
            "/aws-reserved/sso.amazonaws.com/".into(),
        )),
    );

    let arn_value = "arn:aws:iam::111111111111:role/aws-reserved/sso.amazonaws.com/AWSReservedSSO_AdministratorAccess_abcdef0123456789";
    let mut data_source_states: HashMap<ResourceId, State> = HashMap::new();
    data_source_states.insert(
        ds_id.clone(),
        State::existing(
            ds_id.clone(),
            vec![(
                "arns".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String(arn_value.to_string().into()),
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
        r.binding = Some(absolute_binding("role"));
        // No `dependency_bindings` — the executor's dependency-graph
        // path only registers managed-resource bindings into
        // `binding_to_idx` (data sources never become effects), so the
        // field isn't load-bearing here. The dependency is implicit:
        // the data source's read result lives in `data_source_states`
        // before the executor runs.
        r.set_attr(
            "assume_role_policy_document_arns",
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::root("admin_access_roles", "arns"),
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
    // one typed constructor that lays managed resources, data sources, and
    // their split state maps in one go.
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &[role],
        compositions: &[],
        managed_current: &HashMap::new(),
        data_source_values: &test_data_source_values(
            std::slice::from_ref(&ds),
            &data_source_states,
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: executable_data_source_values(
            std::slice::from_ref(&ds),
            &data_source_states,
        ),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

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
            ConcreteValue::String(arn_value.to_string().into())
        )])),
        "the ResourceRef must resolve to the data source's read-state \
         `arns`; got {resolved:?}",
    );
}

/// carina#3666: a data-source read whose input depends on a value
/// produced by an earlier apply effect must execute during apply and
/// publish its read state before downstream consumers run.
#[tokio::test]
async fn test_apply_time_data_source_read_publishes_for_downstream_resource() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use crate::resource::{AccessPath, PathSegment, ResourceId, State, Subscript};

    let provider = MockProvider::new();

    let upstream_id = ResourceId::with_provider_identity("test", "Upstream", "target", None);
    let mut upstream = Resource::with_provider("test", "Upstream", "target", None);
    upstream.id = upstream_id.clone();
    upstream.binding = Some(absolute_binding("target"));
    upstream.set_attr(
        "name",
        Value::Concrete(ConcreteValue::String("target".into())),
    );

    let ds_id = ResourceId::with_provider_identity("test", "Lookup", "roles", None);
    let mut data_source = DataSource::with_provider("test", "Lookup", "roles", None);
    data_source.id = ds_id.clone();
    data_source.binding = Some(absolute_binding("roles"));
    data_source.attributes.insert(
        "filter".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root("target", "generated"),
        }),
    );

    let consumer_id = ResourceId::with_provider_identity("test", "Consumer", "consumer", None);
    let mut consumer = Resource::with_provider("test", "Consumer", "consumer", None);
    consumer.id = consumer_id.clone();
    consumer.binding = Some(absolute_binding("consumer"));
    consumer.set_attr(
        "description",
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root_with_segments(
                "roles",
                "names",
                vec![PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                }],
            ),
        }),
    );

    let upstream_state = State::existing(
        upstream_id.clone(),
        HashMap::from([(
            "generated".to_string(),
            Value::Concrete(ConcreteValue::String("target-generated".into())),
        )]),
    )
    .with_identifier("target-id");
    provider.push_create(Ok(upstream_state));

    let read_state = State::existing(
        ds_id.clone(),
        HashMap::from([(
            "names".to_string(),
            Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                ConcreteValue::String("target-generated".into()),
            )])),
        )]),
    )
    .with_identifier("roles-id");
    provider.push_read(Ok(read_state));

    let consumer_state =
        State::existing(consumer_id.clone(), HashMap::new()).with_identifier("consumer-id");
    provider.push_create(Ok(consumer_state));

    let mut plan = Plan::new();
    plan.add(create_effect(upstream.clone()));
    plan.add(Effect::Read {
        resource: resolved_data_source(data_source.clone()),
    });
    plan.add(create_effect(consumer.clone()));

    let current_states: HashMap<ResourceId, State> = HashMap::new();
    let executable_data_source_values =
        executable_deferred_data_source_values(std::slice::from_ref(&data_source));
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &[upstream, consumer],
        compositions: &[],
        managed_current: &HashMap::new(),
        data_source_values: &test_data_source_values(
            std::slice::from_ref(&data_source),
            &current_states,
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: executable_data_source_values,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(
        result.failure_count,
        0,
        "apply-time read must publish before the consumer runs; events: {:?}",
        observer.events(),
    );
    assert_eq!(result.success_count, 3);

    let calls = provider.calls();
    let upstream_pos = call_position(&calls, "create", &upstream_id.to_string());
    let read_pos = call_position(&calls, "read", &ds_id.to_string());
    let consumer_pos = call_position(&calls, "create", &consumer_id.to_string());
    assert!(
        upstream_pos < read_pos && read_pos < consumer_pos,
        "expected upstream -> read -> consumer provider calls, got {calls:?}",
    );

    let read_requests = provider.captured_data_source_reads();
    assert_eq!(read_requests.len(), 1);
    assert_eq!(
        read_requests[0].get_attr("filter"),
        Some(&Value::Concrete(ConcreteValue::String(
            "target-generated".into()
        ))),
        "apply-time read input must resolve from the upstream create output",
    );

    let creates = provider.captured_create_resources();
    let consumer_create = creates
        .iter()
        .find(|resource| resource.id == consumer_id)
        .expect("consumer create request should be captured");
    assert_eq!(
        consumer_create.get_attr("description"),
        Some(&Value::Concrete(ConcreteValue::String(
            "target-generated".into()
        ))),
        "consumer input must resolve from the read output",
    );
}

#[tokio::test]
async fn test_apply_time_data_source_read_failure_skips_downstream_resource() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use crate::resource::{AccessPath, PathSegment, ResourceId, State, Subscript};

    let provider = MockProvider::new();

    let upstream_id = ResourceId::with_provider_identity("test", "Upstream", "target", None);
    let mut upstream = Resource::with_provider("test", "Upstream", "target", None);
    upstream.id = upstream_id.clone();
    upstream.binding = Some(absolute_binding("target"));
    upstream.set_attr(
        "name",
        Value::Concrete(ConcreteValue::String("target".into())),
    );

    let ds_id = ResourceId::with_provider_identity("test", "Lookup", "roles", None);
    let mut data_source = DataSource::with_provider("test", "Lookup", "roles", None);
    data_source.id = ds_id.clone();
    data_source.binding = Some(absolute_binding("roles"));
    data_source.attributes.insert(
        "filter".to_string(),
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root("target", "generated"),
        }),
    );

    let consumer_id = ResourceId::with_provider_identity("test", "Consumer", "consumer", None);
    let mut consumer = Resource::with_provider("test", "Consumer", "consumer", None);
    consumer.id = consumer_id.clone();
    consumer.binding = Some(absolute_binding("consumer"));
    consumer.set_attr(
        "description",
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root_with_segments(
                "roles",
                "names",
                vec![PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                }],
            ),
        }),
    );

    provider.push_create(Ok(State::existing(
        upstream_id.clone(),
        HashMap::from([(
            "generated".to_string(),
            Value::Concrete(ConcreteValue::String("target-generated".into())),
        )]),
    )
    .with_identifier("target-id")));
    provider.push_read(Err(ProviderError::api_error("lookup failed")));

    let mut plan = Plan::new();
    plan.add(create_effect(upstream.clone()));
    plan.add(Effect::Read {
        resource: resolved_data_source(data_source.clone()),
    });
    plan.add(create_effect(consumer.clone()));

    let current_states: HashMap<ResourceId, State> = HashMap::new();
    let executable_data_source_values =
        executable_deferred_data_source_values(std::slice::from_ref(&data_source));
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &[upstream, consumer],
        compositions: &[],
        managed_current: &HashMap::new(),
        data_source_values: &test_data_source_values(
            std::slice::from_ref(&data_source),
            &current_states,
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: executable_data_source_values,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.success_count, 1);
    assert_eq!(result.failure_count, 1);
    assert_eq!(result.skip_count, 1);
    let events = observer.events();
    assert!(
        events.iter().any(|event| {
            event.contains(&format!("failed:{ds_id}"))
                && event.contains("data source read failed")
                && event.contains("lookup failed")
        }),
        "read failure must be surfaced clearly; events: {events:?}",
    );
    assert!(
        events
            .iter()
            .any(|event| event.contains(&format!("skipped:{consumer_id}"))),
        "consumer must be skipped after read failure; events: {events:?}",
    );
    assert!(
        provider
            .captured_create_resources()
            .iter()
            .all(|resource| resource.id != consumer_id),
        "consumer create must not run after the read fails",
    );
}

#[tokio::test]
async fn test_apply_time_data_source_read_retries_throttling_errors() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use crate::resource::ResourceId;

    let provider = MockProvider::new();
    let ds_id = ResourceId::with_provider_identity("test", "Lookup", "roles", None);
    let mut data_source = DataSource::with_provider("test", "Lookup", "roles", None);
    data_source.id = ds_id.clone();
    data_source.binding = Some(absolute_binding("roles"));
    data_source.set_attr(
        "filter".to_string(),
        Value::Concrete(ConcreteValue::String("literal".into())),
    );

    provider.push_read(Err(ProviderError::api_error("ThrottlingException")));
    provider.push_read(Err(ProviderError::api_error("Rate exceeded")));
    provider.push_read(Ok(
        State::existing(ds_id.clone(), HashMap::new()).with_identifier("roles-id")
    ));

    let mut plan = Plan::new();
    plan.add(Effect::Read {
        resource: resolved_data_source(data_source.clone()),
    });

    let executable_data_source_values =
        executable_deferred_data_source_values(std::slice::from_ref(&data_source));
    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &[],
        compositions: &[],
        managed_current: &HashMap::new(),
        data_source_values: &test_data_source_values(
            std::slice::from_ref(&data_source),
            &HashMap::new(),
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: executable_data_source_values,
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.failure_count, 0);
    assert_eq!(result.success_count, 1);
    assert_eq!(
        provider.captured_data_source_reads().len(),
        3,
        "apply-time reads must use the same retry policy as refresh reads"
    );
}

#[tokio::test]
async fn test_pre_apply_data_source_read_remains_noop_in_executor() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};
    use crate::resource::{AccessPath, PathSegment, ResourceId, State, Subscript};

    let provider = MockProvider::new();

    let ds_id = ResourceId::with_provider_identity("test", "Lookup", "roles", None);
    let mut data_source = DataSource::with_provider("test", "Lookup", "roles", None);
    data_source.id = ds_id.clone();
    data_source.binding = Some(absolute_binding("roles"));
    data_source.attributes.insert(
        "filter".to_string(),
        Value::Concrete(ConcreteValue::String("literal".into())),
    );

    let consumer_id = ResourceId::with_provider_identity("test", "Consumer", "consumer", None);
    let mut consumer = Resource::with_provider("test", "Consumer", "consumer", None);
    consumer.id = consumer_id.clone();
    consumer.binding = Some(absolute_binding("consumer"));
    consumer.set_attr(
        "description",
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::root_with_segments(
                "roles",
                "names",
                vec![PathSegment::Subscript {
                    index: Subscript::Int { index: 0 },
                }],
            ),
        }),
    );

    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    current_states.insert(
        ds_id.clone(),
        State::existing(
            ds_id.clone(),
            HashMap::from([(
                "names".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("literal-result".into()),
                )])),
            )]),
        ),
    );
    provider.push_create(Ok(
        State::existing(consumer_id.clone(), HashMap::new()).with_identifier("consumer-id")
    ));

    let mut plan = Plan::new();
    plan.add(Effect::Read {
        resource: resolved_data_source(data_source.clone()),
    });
    plan.add(create_effect(consumer.clone()));

    let bindings = ResolvedBindings::pre_apply_live(LivePreApplyInputs {
        managed: &[consumer],
        compositions: &[],
        managed_current: &HashMap::new(),
        data_source_values: &test_data_source_values(
            std::slice::from_ref(&data_source),
            &current_states,
        ),
        remote_bindings: &HashMap::new(),
        wait_aliases: &[],
    });

    let input = ExecutionInput {
        plan: &plan,
        provider_check_inputs: ProviderCheckInputs::Authored,
        compositions: &[],
        bindings,
        managed_current: HashMap::new(),
        data_source_values: executable_data_source_values(
            std::slice::from_ref(&data_source),
            &current_states,
        ),
        normalizer: &NoopNormalizer,
        provider_configs: &[],
        factories: &[],
        schemas: &TEST_SCHEMAS,
        parallelism: crate::executor::TEST_UNCAPPED,
        precompleted_success_count: 0,
        checkpoint_sink: crate::executor::ExecutionCheckpointSink::noop(),
    };

    let observer = MockObserver::new();
    let result =
        completed_result(execute_plan(&provider, input, &observer, uncancelled_shutdown()).await);

    assert_eq!(result.failure_count, 0, "events: {:?}", observer.events());
    assert_eq!(result.success_count, 1);
    assert_eq!(
        provider.captured_data_source_reads().len(),
        0,
        "pre-apply reads are already refreshed and must not be double-read by the executor",
    );
    let calls = provider.calls();
    assert!(
        calls.iter().all(|(op, _)| op != "read"),
        "executor should only create the consumer, got {calls:?}",
    );
}
