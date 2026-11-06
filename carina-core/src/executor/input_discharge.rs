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
    ) -> Result<crate::replacement::CreatePermit, WriteInputError> {
        let checked = self
            .plan
            .check_inputs(schemas, current)
            .map_err(WriteInputError::Admission)?;
        let mut checks =
            ExecutionInputChecks::new(checked.execution_input_checks(), checked.effects(), &[0])
                .map_err(|error| WriteInputError::Discharge(EffectFailureKind::execution(error)))?;
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
            .and_then(|write| {
                super::scheduler::authorize_finalized_create(&checked, current, write)
            })
            .map_err(WriteInputError::Discharge)
    }
}

impl FinalizedWrite<UpdateRequest> {
    pub fn check_inputs(
        self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
    ) -> Result<crate::replacement::UpdatePermit, WriteInputError> {
        let checked = self
            .plan
            .check_inputs(schemas, current)
            .map_err(WriteInputError::Admission)?;
        let mut checks =
            ExecutionInputChecks::new(checked.execution_input_checks(), checked.effects(), &[0])
                .map_err(|error| WriteInputError::Discharge(EffectFailureKind::execution(error)))?;
        let attributes = self.request.patch().sent_projection().into_iter().collect();
        let writeback = SecretPreservingAttributes::from_provider_values(&attributes, &attributes);
        let id = self.request.from().id.clone();
        checks
            .take_write(0)
            .and_then(|check| check.prepare_update(&id, self.request, writeback, schemas))
            .and_then(|write| {
                super::scheduler::authorize_finalized_update(&checked, current, write)
            })
            .map_err(WriteInputError::Discharge)
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub(super) fn prepared_create_for_test(request: CreateRequest) -> PreparedWrite<CreateRequest> {
    let attributes = request
        .resource()
        .attributes
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    PreparedWrite {
        resource_id: request.resource().id.clone(),
        request: Arc::new(request),
        writeback: SecretPreservingAttributes::from_provider_values(&attributes, &attributes),
        receipt: Arc::new(PreparationReceipt {
            slot: EffectSlot {
                run: Arc::new(()),
                index: 0,
                child: None,
            },
            _children: Vec::new(),
            deletes: Vec::new(),
            _create_request: None,
        }),
    }
}

// A receipt is shared by the prepared request and every permit it authorizes.
// A cohort receipt retains the child receipts, including the empty cohort case.
#[derive(Debug)]
struct PreparationReceipt {
    slot: EffectSlot,
    _children: Vec<Arc<PreparationReceipt>>,
    deletes: Vec<EffectSlot>,
    // Paired deletes only guard creates. Keeping this same immutable request
    // alive in the receipt makes each permit retain it through Delete itself.
    _create_request: Option<Arc<CreateRequest>>,
}

/// Release authority for one exact delete slot in one execution. The scheduler
/// and input discharge jointly mint this owner of the immutable delete target.
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
    _receipts: Vec<DeleteDischargeReceipt>,
    target: Box<DeleteTarget>,
}

pub(super) struct DeleteDischargeReceipt {
    delete: EffectSlot,
    prepared: Arc<PreparationReceipt>,
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
            binding: _,
            dependencies: _,
            explicit_dependencies: _,
            blocked_by_updates: _,
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

impl DeletePermit {
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
    children: crate::replacement::AdmittedReplacementEffects<S>,
    requests: Vec<(usize, PreparedBasicRequest)>,
    receipts: Vec<DeleteDischargeReceipt>,
}

impl<S> PreparedCohort<S> {
    pub(super) fn take_receipts(&mut self) -> Vec<DeleteDischargeReceipt> {
        std::mem::take(&mut self.receipts)
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        crate::replacement::AdmittedReplacementEffects<S>,
        Vec<(usize, PreparedBasicRequest)>,
        Vec<DeleteDischargeReceipt>,
    ) {
        (self.children, self.requests, self.receipts)
    }
}

pub(super) async fn prepare_cohort<S: Clone>(
    check: WriteCheck,
    children: crate::replacement::AdmittedReplacementEffects<S>,
    child_offset: usize,
    context: &ProviderPreparationContext<'_>,
) -> Result<PreparedCohort<S>, EffectFailureKind> {
    let mut requests = Vec::new();
    for (index, child) in children.effects().iter().enumerate().skip(child_offset) {
        let Effect::Create(resource) = child else {
            continue;
        };
        let (ready, writeback) = super::basic::resolve_resource(
            Ok(CheckInput::Authored(resource.as_inner().clone())),
            context,
        )
        .await?;
        let write = check.child(requests.len()).prepare_create(
            CreateRequest::checked(ready),
            writeback,
            context.schemas(),
        )?;
        requests.push((index, PreparedBasicRequest::Create(write)));
    }
    let writes: Vec<_> = requests
        .iter()
        .map(|(_, request)| match request {
            PreparedBasicRequest::Create(write) => write,
            _ => unreachable!("materialized create request"),
        })
        .collect();
    let receipts = check.cohort_receipts(&writes);
    Ok(PreparedCohort {
        children,
        requests,
        receipts,
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
    ) -> Result<(), EffectFailureKind> {
        let Some(admission) = &self.admission else {
            return Ok(());
        };
        let token = DischargeToken(());
        let site = executor_write_site(&token, admission, id, attributes, schemas)
            .map_err(EffectFailureKind::execution)?
            .ok_or_else(|| {
                EffectFailureKind::message("input rule schema unavailable at dispatch")
            })?;
        for applicable in site.applicable_rules() {
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
                _children: Vec::new(),
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
        let attributes = request.patch().sent_projection();
        self.discharge(id, &attributes, schemas)?;
        Ok(PreparedWrite {
            request: Arc::new(request),
            resource_id: id.clone(),
            writeback,
            receipt: Arc::new(PreparationReceipt {
                slot: self.slot,
                _children: Vec::new(),
                deletes: self.deletes,
                _create_request: None,
            }),
        })
    }

    pub(super) fn cohort_receipts<R>(
        &self,
        writes: &[&PreparedWrite<R>],
    ) -> Vec<DeleteDischargeReceipt> {
        for (index, write) in writes.iter().enumerate() {
            assert_eq!(
                write.receipt.slot,
                self.child(index).slot,
                "cohort slot changed"
            );
        }
        let receipt = Arc::new(PreparationReceipt {
            slot: self.slot.clone(),
            _children: writes
                .iter()
                .map(|write| Arc::clone(&write.receipt))
                .collect(),
            deletes: Vec::new(),
            _create_request: None,
        });
        self.deletes
            .iter()
            .map(|delete| DeleteDischargeReceipt {
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
            request, writeback, ..
        } = self;
        (Arc::unwrap_or_clone(request), writeback)
    }
    pub(super) fn delete_receipts(&self) -> Vec<DeleteDischargeReceipt> {
        self.receipt
            .deletes
            .iter()
            .map(|delete| DeleteDischargeReceipt {
                delete: delete.clone(),
                prepared: Arc::clone(&self.receipt),
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum InputCheckConstructionError {
    #[error("input-check source count {sources} does not match {effects} expanded effects")]
    SourceCount { sources: usize, effects: usize },
    #[error("sealed write slot {source_index} is missing or ambiguous after deferred expansion")]
    WriteSlot { source_index: usize },
    #[error(
        "sealed delete slot {source_index}, absorbed index {absorbed_index}, is missing after deferred expansion"
    )]
    DeleteSlot {
        source_index: usize,
        absorbed_index: usize,
    },
    #[error("expanded write slot {index} has duplicate input checks")]
    DuplicateWrite { index: usize },
    #[error("expanded delete slot {index} is paired with multiple input checks")]
    DuplicateDelete { index: usize },
}

/// Runtime mapping from sealed plan slots to expanded scheduler slots. All
/// pairings below use indices; names and ResourceId equality never grant permits.
pub(super) struct ExecutionInputChecks {
    run: Arc<()>,
    pub(super) checks: HashMap<usize, PendingInputObligation>,
    pub(super) paired: HashMap<usize, Vec<usize>>,
    pub(super) absorbed: HashMap<usize, HashSet<usize>>,
    required_deletes: HashMap<usize, HashSet<usize>>,
    receipts: HashMap<(usize, usize), DeleteDischargeReceipt>,
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
    ) -> Result<Self, InputCheckConstructionError> {
        if sources.len() != effects.len() {
            return Err(InputCheckConstructionError::SourceCount {
                sources: sources.len(),
                effects: effects.len(),
            });
        }
        let mut result = Self {
            run: Arc::new(()),
            checks: HashMap::new(),
            paired: HashMap::new(),
            absorbed: HashMap::new(),
            required_deletes: HashMap::new(),
            receipts: HashMap::new(),
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
            let mut writes =
                sources
                    .iter()
                    .zip(effects)
                    .enumerate()
                    .filter_map(|(idx, (&original, effect))| {
                        (original == source
                            && matches!(
                                effect,
                                Effect::Create(_)
                                    | Effect::ResumeCreate { .. }
                                    | Effect::Update { .. }
                                    | Effect::DeferredCreate(_)
                                    | Effect::DeferredReplace(_)
                            ))
                        .then_some(idx)
                    });
            let write = writes
                .next()
                .ok_or(InputCheckConstructionError::WriteSlot {
                    source_index: source,
                })?;
            if writes.next().is_some() {
                return Err(InputCheckConstructionError::WriteSlot {
                    source_index: source,
                });
            }
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
                let absorbed_index = delete.absorbed_index().unwrap_or(0);
                let delete_idx = *candidates.get(absorbed_index).ok_or(
                    InputCheckConstructionError::DeleteSlot {
                        source_index: delete.effect_index(),
                        absorbed_index,
                    },
                )?;
                result.paired.entry(write).or_default().push(delete_idx);
                if delete.absorbed_index().is_some() {
                    result.absorbed.entry(write).or_default().insert(delete_idx);
                }
                if result
                    .required_deletes
                    .insert(delete_idx, HashSet::from([write]))
                    .is_some()
                {
                    return Err(InputCheckConstructionError::DuplicateDelete { index: delete_idx });
                }
            }
            if result.checks.insert(write, check).is_some() {
                return Err(InputCheckConstructionError::DuplicateWrite { index: write });
            }
        }
        Ok(result)
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
        // The scheduler appends this child at effects.len(), after all sealed
        // slots and preparation nodes. Its index cannot have been consumed.
        assert!(self.consumed.insert(index));
    }

    pub(super) fn publish_receipts(
        &mut self,
        receipts: Vec<DeleteDischargeReceipt>,
    ) -> Result<(), EffectFailureKind> {
        for permit in receipts {
            let index = permit.delete.index;
            if !Arc::ptr_eq(&self.run, &permit.delete.run)
                || !Arc::ptr_eq(&self.run, &permit.prepared.slot.run)
                || permit.prepared.slot.child.is_some()
                || !self
                    .required_deletes
                    .get(&index)
                    .is_some_and(|writes| writes.contains(&permit.prepared.slot.index))
                || self
                    .receipts
                    .contains_key(&(index, permit.prepared.slot.index))
            {
                return Err(EffectFailureKind::message(
                    "paired delete input permit slot mismatch",
                ));
            }
            self.receipts
                .insert((index, permit.prepared.slot.index), permit);
        }
        Ok(())
    }

    pub(super) fn authorize_delete(
        &mut self,
        index: usize,
        ready: &super::scheduler::ReadyDelete,
    ) -> Result<DeletePermit, EffectFailureKind> {
        let mut receipts = Vec::new();
        for &write in self.required_deletes.get(&index).into_iter().flatten() {
            let receipt = self.receipts.remove(&(index, write)).ok_or_else(|| {
                EffectFailureKind::message("paired delete has no prepared input permit")
            })?;
            if receipt.delete != self.slot(index) {
                return Err(EffectFailureKind::message(
                    "paired delete input permit slot mismatch",
                ));
            }
            receipts.push(receipt);
        }
        let target = self.delete_targets.remove(&index).ok_or_else(|| {
            EffectFailureKind::message("delete dispatch target missing for effect slot")
        })?;
        if ready.instance()
            != &DeletedInstanceKey::new(
                target.id.clone(),
                target.generation.clone(),
                target.identifier.as_str(),
            )
        {
            return Err(EffectFailureKind::message(
                "paired delete input permit slot mismatch",
            ));
        }
        Ok(DeletePermit {
            _receipts: receipts,
            target: Box::new(target),
        })
    }

    pub(super) fn bind_materialized_deletes(
        &mut self,
        write: usize,
        deletes: impl IntoIterator<Item = usize>,
    ) {
        // Only the resolver's confirmed child/old-generation associations enter
        // discharge. Same-kind anonymous gate candidates are graph barriers,
        // not necessarily members of this cohort's replacement.
        for delete in deletes {
            let paired = self.paired.entry(write).or_default();
            if !paired.contains(&delete) {
                paired.push(delete);
            }
            self.required_deletes
                .entry(delete)
                .or_default()
                .insert(write);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_discharge_invalid_plan_mapping_returns_error_without_panicking() {
        use crate::schema::{
            InputPath, InputPredicate, InputRule, InputRuleCode, InputRuleScope,
            PatternAttributeSchema, PatternAttributeType, PatternResourceSchema,
        };
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "",
            PatternResourceSchema::new("test")
                .attribute(
                    PatternAttributeSchema::optional("name", PatternAttributeType::string())
                        .build(),
                )
                .input_rule(InputRule::value(
                    InputRuleScope::CreateOnly,
                    InputPath::new("name").unwrap(),
                    InputRuleCode::new("test.name").unwrap(),
                    InputPredicate::NotIpv4,
                ))
                .validate()
                .unwrap(),
        );
        let resource = crate::resource::ResolvedResource::resolve_raw_fixture(
            crate::resource::Resource::new("test", "example"),
            &schemas,
        )
        .unwrap();
        let mut plan = Plan::new();
        let old = crate::effect::DeferredReplaceDelete {
            id: resource.resolved_id(),
            identifier: AppliedIdentifier::new("old").unwrap(),
            generation: EffectGeneration::Current,
            previous_attributes: HashMap::new(),
            directives: Default::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        plan.add_replacement(crate::plan::ReplacementGroup {
            replacement: crate::test_support::checked_replacement(resource, old, false),
            changed_create_only: crate::effect::ChangedCreateOnly::new(vec!["generation".into()])
                .unwrap(),
            cascade_ref_hints: vec![],
            consumer_updates: Default::default(),
        });
        let checked = FinalizedPlan::new(plan)
            .check_inputs(&schemas, &HashMap::new())
            .unwrap();
        let checks = checked.execution_input_checks();
        for (effects, sources, obligations) in [
            (vec![], vec![], checks.clone()),
            (checked.effects().to_vec(), vec![], checks.clone()),
            (vec![checked.effects()[1].clone()], vec![1], checks.clone()),
            (
                checked.effects().to_vec(),
                vec![0, 1],
                [checks.clone(), checks.clone()].concat(),
            ),
        ] {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ExecutionInputChecks::new(obligations, &effects, &sources)
            }));
            assert!(
                outcome.is_ok(),
                "invalid plan mapping must be a typed error"
            );
            assert!(outcome.unwrap().is_err());
        }
    }

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
        let mut checks = ExecutionInputChecks::new(Vec::new(), &effects, &[]).unwrap();
        // No names enter this authorization test: each delete has a distinct
        // effect slot even if their resource and generation were identical.
        checks
            .required_deletes
            .extend([(1, HashSet::from([0])), (2, HashSet::from([3]))]);
        let target = DeleteTarget {
            id: ResourceId::with_identity("test", "same-instance"),
            identifier: AppliedIdentifier::new("old-id").unwrap(),
            generation: EffectGeneration::Current,
            directives: Default::default(),
        };
        let ready = super::super::scheduler::ReadyDelete::for_test(DeletedInstanceKey::new(
            target.id.clone(),
            target.generation.clone(),
            target.identifier.as_str(),
        ));
        checks.delete_targets.insert(1, target.clone());
        checks.delete_targets.insert(2, target);
        checks.paired.insert(0, vec![1]);
        let cohort = checks.take_write(0).unwrap();
        checks
            .publish_receipts(cohort.cohort_receipts::<CreateRequest>(&[]))
            .unwrap();
        assert!(checks.authorize_delete(2, &ready).is_err());
        assert!(checks.authorize_delete(1, &ready).is_ok());
        assert!(checks.authorize_delete(1, &ready).is_err());
        assert!(checks.take_write(0).is_err());
    }

    #[test]
    fn input_discharge_delete_permit_cannot_cross_execution_identity() {
        let effects: Vec<Effect> = Vec::new();
        let mut first = ExecutionInputChecks::new(Vec::new(), &effects, &[]).unwrap();
        first.paired.insert(0, vec![1]);
        let cohort = first.take_write(0).unwrap();
        let permit = cohort.cohort_receipts::<CreateRequest>(&[]).pop().unwrap();
        let mut second = ExecutionInputChecks::new(Vec::new(), &effects, &[]).unwrap();
        second.required_deletes.insert(1, HashSet::from([0]));
        // Deliberately corrupt the private store to exercise its final check.
        second.receipts.insert((1, 0), permit);
        let ready = super::super::scheduler::ReadyDelete::for_test(DeletedInstanceKey::new(
            ResourceId::with_identity("test", "same-instance"),
            EffectGeneration::Current,
            "old-id",
        ));
        assert!(second.authorize_delete(1, &ready).is_err());
    }
}
