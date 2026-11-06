//! Linear preparation of exact provider requests and replacement delete permits.
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use indexmap::IndexMap;

use super::EffectFailureKind;
use super::basic::PreparedBasicRequest;
use super::provider_ready::{CheckInput, ProviderPreparationContext};
use crate::{
    binding_index::SecretPreservingAttributes,
    effect::{DeletedInstanceKey, Effect, EffectGeneration},
    input_rules::{FinalizedPlan, InputAdmissionFailure},
    input_rules::{InputVerdict, PendingInputObligation, executor_write_site},
    plan::Plan,
    provider::{AppliedIdentifier, CreateRequest, DeleteRequest, Provider, UpdateRequest},
    resource::{Current, Directives, ProviderModuleSource, ResourceId, Value},
    schema::{SchemaKind, SchemaRegistry},
};

// Its constructor is private to discharge. Even other core modules cannot
// manufacture an executor write site from an arbitrary resource or state.
pub(crate) struct DischargeToken(());

#[derive(Clone, Debug)]
struct EffectSlot {
    run: Arc<()>,
    index: usize,
    child: Option<usize>,
}

impl PartialEq for EffectSlot {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.run, &other.run) && self.index == other.index && self.child == other.child
    }
}
impl Eq for EffectSlot {}

/// An immutable, nonserializable provider request. Only discharge constructs it.
/// The request and secret-preserving writeback travel together until dispatch.
///
/// ```compile_fail
/// use carina_core::{executor::PreparedWrite, provider::CreateRequest};
/// fn forge(request: CreateRequest) -> PreparedWrite<CreateRequest> { request.into() }
/// ```
/// ```compile_fail
/// use carina_core::{executor::PreparedWrite, provider::CreateRequest};
/// let forged: PreparedWrite<CreateRequest> = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// use carina_core::{executor::PreparedWrite, provider::CreateRequest};
/// fn mutate(write: &mut PreparedWrite<CreateRequest>, request: CreateRequest) { write.request = request; }
/// ```
/// ```compile_fail
/// use carina_core::{executor::PreparedWrite, provider::CreateRequest};
/// fn forge(request: CreateRequest) -> PreparedWrite<CreateRequest> {
///     PreparedWrite { request: std::sync::Arc::new(request), resource_id: todo!(), writeback: todo!(), receipt: todo!() }
/// }
/// ```
/// Scheduled dispatch is private to the executor and requires a prepared
/// request, even if a caller already possesses provider-readiness authority.
///
/// ```compile_fail
/// use carina_core::{executor::basic::execute_basic_effect, provider::ProviderReadyResource};
/// fn bypass(request: ProviderReadyResource) { execute_basic_effect(request); }
/// ```
pub struct PreparedWrite<R> {
    request: Arc<R>,
    resource_id: ResourceId,
    writeback: SecretPreservingAttributes,
    receipt: Arc<PreparationReceipt>,
}

/// A single finalized request, retained unchanged through admission and discharge.
/// Only the request-specific finalizers can construct this owner.
pub struct FinalizedWrite<R> {
    request: R,
    plan: FinalizedPlan<ProviderModuleSource>,
}

/// Admission diagnostics or a failure to discharge the exact provider request.
#[derive(Debug, thiserror::Error)]
pub enum WriteInputError {
    #[error("input admission rejected the request")]
    Admission(InputAdmissionFailure<ProviderModuleSource>),
    #[error("{0}")]
    Discharge(EffectFailureKind),
}

impl<R> FinalizedWrite<R> {
    pub(crate) fn new(request: R, plan: Plan<ProviderModuleSource>) -> Self {
        Self {
            request,
            plan: FinalizedPlan::new(plan),
        }
    }
}

impl FinalizedWrite<CreateRequest> {
    pub fn check_inputs(
        self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
    ) -> Result<PreparedWrite<CreateRequest>, WriteInputError> {
        let checked = self
            .plan
            .check_inputs(schemas, current)
            .map_err(WriteInputError::Admission)?;
        let mut checks =
            ExecutionInputChecks::new(checked.execution_input_checks(), checked.effects(), &[0]);
        let attributes = self
            .request
            .resource()
            .attributes
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let writeback = SecretPreservingAttributes::from_provider_values(&attributes, &attributes);
        checks
            .take_write(0)
            .and_then(|check| check.prepare_create(self.request, writeback, schemas))
            .map_err(WriteInputError::Discharge)
    }
}

impl FinalizedWrite<UpdateRequest> {
    pub fn check_inputs(
        self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
    ) -> Result<PreparedWrite<UpdateRequest>, WriteInputError> {
        let checked = self
            .plan
            .check_inputs(schemas, current)
            .map_err(WriteInputError::Admission)?;
        let mut checks =
            ExecutionInputChecks::new(checked.execution_input_checks(), checked.effects(), &[0]);
        let attributes = self
            .request
            .patch()
            .ops
            .iter()
            .filter_map(|op| {
                op.sent_value()
                    .map(|value| (op.key().to_owned(), value.clone()))
            })
            .collect();
        let writeback = SecretPreservingAttributes::from_provider_values(&attributes, &attributes);
        let id = self.request.from().id.clone();
        checks
            .take_write(0)
            .and_then(|check| check.prepare_update(&id, self.request, writeback, schemas))
            .map_err(WriteInputError::Discharge)
    }
}

// A receipt is shared by the prepared request and every permit it authorizes.
// A cohort receipt retains the child receipts, including the empty cohort case.
#[derive(Debug)]
struct PreparationReceipt {
    slot: EffectSlot,
    children: Vec<Arc<PreparationReceipt>>,
    deletes: Vec<EffectSlot>,
    // Paired deletes only guard creates. Keeping this same immutable request
    // alive in the receipt makes each permit retain it through Delete itself.
    _create_request: Option<Arc<CreateRequest>>,
}

/// Release authority for one exact delete slot in one execution. It cannot be
/// copied, forged, deserialized, or rebound using a resource's display name.
///
/// ```compile_fail
/// use carina_core::executor::DeletePermit;
/// let permit = DeletePermit::default();
/// ```
/// ```compile_fail
/// use carina_core::executor::DeletePermit;
/// let permit: DeletePermit = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// use carina_core::executor::DeletePermit;
/// let permit = DeletePermit { delete: todo!(), prepared: todo!() };
/// ```
pub struct DeletePermit {
    delete: EffectSlot,
    prepared: Arc<PreparationReceipt>,
}

pub(super) struct DeleteAuthorization {
    // Keep the receipt alive through the provider Delete, even if its create
    // has already completed (DeferredReplace's existing CBD order).
    _permit: Option<DeletePermit>,
    target: Box<DeleteTarget>,
}

/// The exact delete operation captured from its immutable scheduler slot.
/// Dispatch takes this owned target, so an unrelated raw Delete cannot borrow
/// another slot's authorization.
#[derive(Clone)]
struct DeleteTarget {
    id: ResourceId,
    identifier: AppliedIdentifier,
    generation: EffectGeneration,
    directives: Directives,
}

impl DeleteTarget {
    fn from_effect<S>(effect: &Effect<S>) -> Option<Self> {
        let Effect::Delete {
            id,
            identifier,
            generation,
            directives,
            ..
        } = effect
        else {
            return None;
        };
        Some(Self {
            id: id.as_inner().clone(),
            identifier: identifier.clone(),
            generation: generation.clone(),
            directives: directives.clone(),
        })
    }
}

impl DeleteAuthorization {
    pub(super) async fn dispatch(
        self,
        provider: &dyn Provider,
        schemas: &SchemaRegistry,
    ) -> super::basic::BasicEffectResult {
        let target = &self.target;
        let directives =
            schemas.resolve_directives(&target.id, SchemaKind::Resource, &target.directives);
        let request = DeleteRequest::from_resolved(directives);
        match provider
            .delete(&target.id, &target.identifier, request)
            .await
        {
            Ok(()) => super::basic::BasicEffectResult::Deleted {
                instance_key: DeletedInstanceKey::new(
                    target.id.clone(),
                    target.generation.clone(),
                    target.identifier.to_string(),
                ),
            },
            Err(error) => super::basic::BasicEffectResult::Failure {
                failure: EffectFailureKind::provider(error),
                refresh: match target.generation {
                    EffectGeneration::Current => {
                        Some((target.id.clone(), target.identifier.clone()))
                    }
                    EffectGeneration::Deposed(_) => None,
                },
            },
        }
    }
}

pub(super) struct WriteCheck {
    slot: EffectSlot,
    admission: Option<PendingInputObligation>,
    deletes: Vec<EffectSlot>,
}

pub(super) struct PreparedCohort<S> {
    children: Vec<Effect<S>>,
    requests: Vec<PreparedBasicRequest>,
    permits: Vec<DeletePermit>,
}

impl<S> PreparedCohort<S> {
    pub(super) fn into_parts(
        self,
    ) -> (Vec<Effect<S>>, Vec<PreparedBasicRequest>, Vec<DeletePermit>) {
        (self.children, self.requests, self.permits)
    }
}

pub(super) async fn prepare_cohort<S: Clone>(
    check: WriteCheck,
    children: Vec<Effect<S>>,
    context: &ProviderPreparationContext<'_>,
) -> Result<PreparedCohort<S>, EffectFailureKind> {
    let mut requests = Vec::with_capacity(children.len());
    for (ordinal, child) in children.iter().enumerate() {
        let Effect::Create(resource) = child else {
            unreachable!("materialized write")
        };
        let (ready, writeback) = super::basic::resolve_resource(
            Ok(CheckInput::Authored(resource.as_inner().clone())),
            context,
        )
        .await?;
        let write = check.child(ordinal).prepare_create(
            CreateRequest::checked(ready),
            writeback,
            context.schemas(),
        )?;
        requests.push(PreparedBasicRequest::Create(write));
    }
    let writes: Vec<_> = requests
        .iter()
        .map(|request| match request {
            PreparedBasicRequest::Create(write) => write,
            _ => unreachable!("materialized create request"),
        })
        .collect();
    let permits = check.cohort_permits(&writes);
    Ok(PreparedCohort {
        children,
        requests,
        permits,
    })
}

impl WriteCheck {
    pub(super) fn child(&self, index: usize) -> Self {
        Self {
            slot: EffectSlot {
                child: Some(index),
                ..self.slot.clone()
            },
            admission: self.admission.clone(),
            deletes: Vec::new(),
        }
    }

    fn discharge(
        &self,
        id: &ResourceId,
        attributes: &IndexMap<String, Value>,
        schemas: &SchemaRegistry,
        update: bool,
    ) -> Result<(), EffectFailureKind> {
        let Some(admission) = &self.admission else {
            return Ok(());
        };
        let token = DischargeToken(());
        let site =
            executor_write_site(&token, admission, id, attributes, schemas).ok_or_else(|| {
                EffectFailureKind::message("input rule schema unavailable at dispatch")
            })?;
        for applicable in site.applicable_rules() {
            // Update requests carry only sent values. An omitted top-level
            // attribute is neither desired input nor a write to that path.
            if update
                && !attributes
                    .contains_key(applicable.path().segments().next().expect("nonempty path"))
            {
                continue;
            }
            match applicable.evaluate() {
                InputVerdict::Proven => {}
                InputVerdict::Violation(violation) => {
                    return Err(EffectFailureKind::InputRule { violation });
                }
                InputVerdict::Undecided => {
                    return Err(EffectFailureKind::UnresolvedInputRule {
                        path: applicable.path().clone(),
                        code: applicable.code().clone(),
                    });
                }
            }
        }
        Ok(())
    }

    pub(super) fn prepare_create(
        self,
        request: CreateRequest,
        writeback: SecretPreservingAttributes,
        schemas: &SchemaRegistry,
    ) -> Result<PreparedWrite<CreateRequest>, EffectFailureKind> {
        self.discharge(
            &request.resource().id,
            &request.resource().attributes,
            schemas,
            false,
        )?;
        let resource_id = request.resource().id.clone();
        let request = Arc::new(request);
        let retained_request = Arc::clone(&request);
        Ok(PreparedWrite {
            request,
            resource_id,
            writeback,
            receipt: Arc::new(PreparationReceipt {
                slot: self.slot,
                children: Vec::new(),
                deletes: self.deletes,
                _create_request: Some(retained_request),
            }),
        })
    }

    pub(super) fn prepare_update(
        self,
        id: &ResourceId,
        request: UpdateRequest,
        writeback: SecretPreservingAttributes,
        schemas: &SchemaRegistry,
    ) -> Result<PreparedWrite<UpdateRequest>, EffectFailureKind> {
        let attributes = request
            .patch()
            .ops
            .iter()
            .filter_map(|op| {
                op.sent_value()
                    .map(|value| (op.key().to_owned(), value.clone()))
            })
            .collect();
        self.discharge(id, &attributes, schemas, true)?;
        Ok(PreparedWrite {
            request: Arc::new(request),
            resource_id: id.clone(),
            writeback,
            receipt: Arc::new(PreparationReceipt {
                slot: self.slot,
                children: Vec::new(),
                deletes: self.deletes,
                _create_request: None,
            }),
        })
    }

    pub(super) fn cohort_permits<R>(&self, writes: &[&PreparedWrite<R>]) -> Vec<DeletePermit> {
        for (index, write) in writes.iter().enumerate() {
            assert_eq!(
                write.receipt.slot,
                self.child(index).slot,
                "cohort slot changed"
            );
        }
        let receipt = Arc::new(PreparationReceipt {
            slot: self.slot.clone(),
            children: writes
                .iter()
                .map(|write| Arc::clone(&write.receipt))
                .collect(),
            deletes: Vec::new(),
            _create_request: None,
        });
        self.deletes
            .iter()
            .map(|delete| DeletePermit {
                delete: delete.clone(),
                prepared: Arc::clone(&receipt),
            })
            .collect()
    }
}

impl<R> PreparedWrite<R> {
    /// Inspect the exact request without permitting mutation or extracting it
    /// from its discharge witness.
    pub fn request(&self) -> &R {
        &self.request
    }
    pub(super) fn writeback(&self) -> &SecretPreservingAttributes {
        &self.writeback
    }
    pub(super) fn resource_id(&self) -> &ResourceId {
        &self.resource_id
    }
    pub(super) fn into_parts(self) -> (R, SecretPreservingAttributes)
    where
        R: Clone,
    {
        let Self {
            request,
            writeback,
            receipt,
            ..
        } = self;
        drop(receipt);
        (Arc::unwrap_or_clone(request), writeback)
    }
    pub(super) fn delete_permits(&self) -> Vec<DeletePermit> {
        self.receipt
            .deletes
            .iter()
            .map(|delete| DeletePermit {
                delete: delete.clone(),
                prepared: Arc::clone(&self.receipt),
            })
            .collect()
    }
}

/// Runtime mapping from sealed plan slots to expanded scheduler slots. All
/// pairings below use indices; names and ResourceId equality never grant permits.
pub(super) struct ExecutionInputChecks {
    run: Arc<()>,
    checks: HashMap<usize, PendingInputObligation>,
    paired: HashMap<usize, Vec<usize>>,
    required_deletes: HashMap<usize, usize>,
    permits: HashMap<usize, DeletePermit>,
    consumed: HashSet<usize>,
    delete_targets: HashMap<usize, DeleteTarget>,
}

impl ExecutionInputChecks {
    pub(super) fn has_rules(&self, index: usize) -> bool {
        self.checks.contains_key(&index)
    }
    pub(super) fn new<S>(
        checks: Vec<PendingInputObligation>,
        effects: &[Effect<S>],
        sources: &[usize],
    ) -> Self {
        let mut result = Self {
            run: Arc::new(()),
            checks: HashMap::new(),
            paired: HashMap::new(),
            required_deletes: HashMap::new(),
            permits: HashMap::new(),
            consumed: HashSet::new(),
            delete_targets: effects
                .iter()
                .enumerate()
                .filter_map(|(index, effect)| {
                    DeleteTarget::from_effect(effect).map(|target| (index, target))
                })
                .collect(),
        };
        for check in checks {
            let source = check.target().effect_index();
            let write = sources
                .iter()
                .enumerate()
                .find_map(|(idx, &original)| {
                    (original == source && !matches!(effects[idx], Effect::Delete { .. }))
                        .then_some(idx)
                })
                .expect("sealed write slot survives deferred expansion");
            for delete in check.paired_deletes() {
                let candidates: Vec<_> = sources
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, &original)| {
                        (original == delete.effect_index()
                            && matches!(effects[idx], Effect::Delete { .. }))
                        .then_some(idx)
                    })
                    .collect();
                let delete_idx = candidates[delete.absorbed_index().unwrap_or(0)];
                result.paired.entry(write).or_default().push(delete_idx);
                assert!(result.required_deletes.insert(delete_idx, write).is_none());
            }
            result.checks.insert(write, check);
        }
        result
    }

    fn slot(&self, index: usize) -> EffectSlot {
        EffectSlot {
            run: Arc::clone(&self.run),
            index,
            child: None,
        }
    }

    pub(super) fn take_write(&mut self, index: usize) -> Result<WriteCheck, EffectFailureKind> {
        if !self.consumed.insert(index) {
            return Err(EffectFailureKind::message(
                "prepared request missing for consumed write slot",
            ));
        }
        Ok(WriteCheck {
            slot: self.slot(index),
            admission: self.checks.remove(&index),
            deletes: self
                .paired
                .get(&index)
                .into_iter()
                .flatten()
                .map(|&index| self.slot(index))
                .collect(),
        })
    }

    pub(super) fn register_prepared_child(&mut self, index: usize) {
        assert!(self.consumed.insert(index));
    }

    pub(super) fn preparation_pairs<S>(&self, effects: &[Effect<S>]) -> Vec<(usize, Vec<usize>)> {
        let mut pairs: Vec<_> = self
            .paired
            .iter()
            .filter_map(|(&write, deletes)| {
                matches!(effects[write], Effect::Create(_)).then_some((write, deletes.clone()))
            })
            .collect();
        pairs.sort_by_key(|(write, _)| *write);
        pairs
    }

    pub(super) fn replacement_templates<S>(&self, effects: &[Effect<S>]) -> HashSet<usize> {
        self.checks
            .keys()
            .copied()
            .filter(|&index| matches!(effects[index], Effect::DeferredReplace(_)))
            .collect()
    }

    pub(super) fn publish_permits(&mut self, permits: Vec<DeletePermit>) {
        for permit in permits {
            assert!(Arc::ptr_eq(&self.run, &permit.delete.run));
            assert!(Arc::ptr_eq(&self.run, &permit.prepared.slot.run));
            assert_eq!(permit.prepared.slot.child, None);
            let index = permit.delete.index;
            assert_eq!(
                self.required_deletes.get(&index),
                Some(&permit.prepared.slot.index)
            );
            // Touch the cohort's retained receipts: its lifetime covers all
            // children, including requests waiting behind a Delete.
            for child in &permit.prepared.children {
                assert!(Arc::ptr_eq(&self.run, &child.slot.run));
            }
            assert!(self.permits.insert(index, permit).is_none());
        }
    }

    pub(super) fn authorize_delete(
        &mut self,
        index: usize,
    ) -> Result<DeleteAuthorization, EffectFailureKind> {
        let permit = if self.required_deletes.contains_key(&index) {
            let permit = self.permits.remove(&index).ok_or_else(|| {
                EffectFailureKind::message("paired delete has no prepared input permit")
            })?;
            if permit.delete != self.slot(index) {
                return Err(EffectFailureKind::message(
                    "paired delete input permit slot mismatch",
                ));
            }
            Some(permit)
        } else {
            None
        };
        let target = self.delete_targets.remove(&index).ok_or_else(|| {
            EffectFailureKind::message("delete dispatch target missing for effect slot")
        })?;
        Ok(DeleteAuthorization {
            _permit: permit,
            target: Box::new(target),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_discharge_finalized_requests_check_create_and_update_scopes() {
        use crate::schema::{
            InputPath, InputPredicate, InputRule, InputRuleCode, InputRuleScope,
            PatternAttributeSchema, PatternAttributeType, PatternResourceSchema,
        };
        let string =
            |value: &str| Value::Concrete(crate::resource::ConcreteValue::String(value.into()));
        for scope in [InputRuleScope::CreateOnly, InputRuleScope::EveryWrite] {
            let mut schemas = SchemaRegistry::new();
            schemas.insert(
                "",
                PatternResourceSchema::new("test")
                    .attribute(
                        PatternAttributeSchema::optional("name", PatternAttributeType::string())
                            .build(),
                    )
                    .input_rule(InputRule::value(
                        scope,
                        InputPath::new("name").unwrap(),
                        InputRuleCode::new("test.name.not_ipv4").unwrap(),
                        InputPredicate::NotIpv4,
                    ))
                    .validate()
                    .unwrap(),
            );
            let bindings = crate::binding_index::ResolvedBindings::default();
            let gate = super::super::ModuleConstraintGate::from_live_compositions(&[]);
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |type_checks| {
                    let context = ProviderPreparationContext::new(
                        &bindings,
                        &gate,
                        &[],
                        &crate::provider::NoopNormalizer,
                        &[],
                        &schemas,
                        type_checks,
                    );
                    for value in ["valid-name", "192.168.1.1"] {
                        let resource = crate::resource::Resource::new("test", "example")
                            .with_attribute("name", string(value));
                        let create = futures::executor::block_on(
                            super::super::prepare_create_request(resource.clone(), &context),
                        )
                        .unwrap();
                        let finalized = crate::differ::finalize_bootstrap_create(create);
                        match finalized.check_inputs(&schemas, &HashMap::new()) {
                            Ok(prepared) => {
                                assert_eq!(value, "valid-name");
                                assert_eq!(
                                    prepared.request().resource().attributes["name"],
                                    string(value)
                                );
                            }
                            Err(WriteInputError::Admission(plan)) => {
                                assert_eq!(value, "192.168.1.1");
                                assert!(
                                    matches!(&plan.errors()[0].kind, crate::plan::PlanErrorKind::InputRule { code, .. } if code == "test.name.not_ipv4")
                                );
                            }
                            Err(error) => panic!("unexpected discharge failure: {error}"),
                        }
                        let from = crate::resource::SnapshotState::not_found(resource.id.clone());
                        let update =
                            futures::executor::block_on(super::super::prepare_update_request(
                                resource,
                                from,
                                &["name".into()],
                                &context,
                            ))
                            .unwrap();
                        let finalized = crate::differ::finalize_update_request(
                            AppliedIdentifier::new("old-id").unwrap(),
                            update,
                        );
                        match finalized.check_inputs(&schemas, &HashMap::new()) {
                            Ok(prepared) => {
                                assert!(
                                    value == "valid-name" || scope == InputRuleScope::CreateOnly
                                );
                                assert_eq!(
                                    prepared.request().patch().ops[0].sent_value(),
                                    Some(&string(value))
                                );
                            }
                            Err(WriteInputError::Admission(plan)) => {
                                assert_eq!(scope, InputRuleScope::EveryWrite);
                                assert_eq!(value, "192.168.1.1");
                                assert!(
                                    matches!(&plan.errors()[0].kind, crate::plan::PlanErrorKind::InputRule { code, .. } if code == "test.name.not_ipv4")
                                );
                            }
                            Err(error) => panic!("unexpected discharge failure: {error}"),
                        }
                    }
                },
            );
        }
    }

    #[test]
    fn input_discharge_delete_permit_cannot_release_another_slot_or_be_reused() {
        let effects: Vec<Effect> = Vec::new();
        let mut checks = ExecutionInputChecks::new(Vec::new(), &effects, &[]);
        // No names enter this authorization test: each delete has a distinct
        // effect slot even if their resource and generation were identical.
        checks.required_deletes.extend([(1, 0), (2, 3)]);
        let target = DeleteTarget {
            id: ResourceId::with_identity("test", "same-instance"),
            identifier: AppliedIdentifier::new("old-id").unwrap(),
            generation: EffectGeneration::Current,
            directives: Default::default(),
        };
        checks.delete_targets.insert(1, target.clone());
        checks.delete_targets.insert(2, target);
        checks.paired.insert(0, vec![1]);
        let cohort = checks.take_write(0).unwrap();
        checks.publish_permits(cohort.cohort_permits::<CreateRequest>(&[]));
        assert!(checks.authorize_delete(2).is_err());
        assert!(checks.authorize_delete(1).is_ok());
        assert!(checks.authorize_delete(1).is_err());
        assert!(checks.take_write(0).is_err());
    }

    #[test]
    fn input_discharge_delete_permit_cannot_cross_execution_identity() {
        let effects: Vec<Effect> = Vec::new();
        let mut first = ExecutionInputChecks::new(Vec::new(), &effects, &[]);
        first.paired.insert(0, vec![1]);
        let cohort = first.take_write(0).unwrap();
        let permit = cohort.cohort_permits::<CreateRequest>(&[]).pop().unwrap();
        let mut second = ExecutionInputChecks::new(Vec::new(), &effects, &[]);
        second.required_deletes.insert(1, 0);
        // Deliberately corrupt the private store to exercise its final check.
        second.permits.insert(1, permit);
        assert!(second.authorize_delete(1).is_err());
    }
}
