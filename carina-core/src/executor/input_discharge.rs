//! Linear preparation of exact provider requests and replacement delete permits.
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use super::EffectFailureKind;
use super::basic::PreparedBasicRequest;
use super::provider_ready::{CheckInput, ProviderPreparationContext};
use crate::{
    binding_index::SecretPreservingAttributes,
    effect::{DeletedInstanceKey, Effect, EffectGeneration},
    input_rules::{PendingInputObligation, executor_write_site},
    provider::{
        AppliedIdentifier, CreateRequest, HostProvider, ProviderReadyResource, UpdateRequest,
    },
    resource::{Directives, ResourceId, SnapshotState},
    schema::SchemaRegistry,
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
/// ```compile_fail,E0277
/// use carina_core::{executor::PreparedWrite, provider::CreateRequest};
/// fn forge(request: CreateRequest) -> PreparedWrite<CreateRequest> { request.into() }
/// ```
/// ```compile_fail,E0277
/// use carina_core::{executor::PreparedWrite, provider::CreateRequest};
/// let forged: PreparedWrite<CreateRequest> = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail,E0616
/// use carina_core::{executor::PreparedWrite, provider::CreateRequest};
/// fn mutate(write: &mut PreparedWrite<CreateRequest>, request: CreateRequest) { write.request = request; }
/// ```
/// ```compile_fail,E0451
/// use carina_core::{executor::PreparedWrite, provider::CreateRequest};
/// fn forge(request: CreateRequest) -> PreparedWrite<CreateRequest> {
///     PreparedWrite { request: std::sync::Arc::new(request), resource_id: todo!(), writeback: todo!(), receipt: todo!() }
/// }
/// ```
/// Scheduled dispatch is private to the executor and requires a prepared
/// request, even if a caller already possesses provider-readiness authority.
///
/// ```compile_fail,E0603
/// use carina_core::{executor::basic::execute_basic_effect, provider::ProviderReadyResource};
/// fn bypass(request: ProviderReadyResource) { execute_basic_effect(request); }
/// ```
pub struct PreparedWrite<R> {
    request: Arc<R>,
    resource_id: ResourceId,
    writeback: SecretPreservingAttributes,
    receipt: Arc<PreparationReceipt>,
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
/// ```compile_fail,E0599
/// use carina_core::executor::DeletePermit;
/// let permit = DeletePermit::default();
/// ```
/// ```compile_fail,E0277
/// use carina_core::executor::DeletePermit;
/// let permit: DeletePermit = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail,E0451
/// use carina_core::executor::DeletePermit;
/// let permit = DeletePermit { _receipts: Vec::new(), target: todo!() };
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
    pub(crate) fn id(&self) -> &ResourceId {
        &self.target.id
    }

    pub(crate) fn identifier(&self) -> &AppliedIdentifier {
        &self.target.identifier
    }

    pub(crate) fn directives(&self) -> &Directives {
        &self.target.directives
    }

    pub(super) async fn dispatch(
        self,
        provider: &dyn HostProvider,
        schemas: &SchemaRegistry,
    ) -> super::basic::BasicEffectResult {
        let target = self.target.clone();
        match super::dispatch_delete(provider, self, schemas).await {
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
        input: Arc<crate::input_rules::RuleInput>,
        schemas: &SchemaRegistry,
    ) -> Result<(), EffectFailureKind> {
        let Some(admission) = &self.admission else {
            return Ok(());
        };
        let token = DischargeToken(());
        let site = executor_write_site(&token, admission, input, schemas)
            .map_err(EffectFailureKind::execution)?
            .ok_or_else(|| {
                EffectFailureKind::message("input rule schema unavailable at dispatch")
            })?;
        if let Some(failure) = site.evaluate().into_dispatch_failure() {
            return Err(failure);
        }

        Ok(())
    }

    pub(super) fn prepare_create(
        self,
        request: CreateRequest,
        writeback: SecretPreservingAttributes,
        schemas: &SchemaRegistry,
    ) -> Result<PreparedWrite<CreateRequest>, EffectFailureKind> {
        if let Some(admission) = &self.admission {
            let input = crate::input_rules::RuleInput::for_dispatch_create(
                &request.resource().protected_desired_tree(),
                &admission.schema,
            );
            self.discharge(input, schemas)?;
        }
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
        from: SnapshotState,
        changed: &[String],
        desired: ProviderReadyResource,
        writeback: SecretPreservingAttributes,
        schemas: &SchemaRegistry,
    ) -> Result<PreparedWrite<UpdateRequest>, EffectFailureKind> {
        let schema = super::provider_ready::managed_update_schema(schemas, desired.as_resource())
            .map_err(EffectFailureKind::execution)?;
        let request = UpdateRequest::checked(from, changed, &desired, schema)
            .map_err(EffectFailureKind::execution)?;
        if let Some(admission) = &self.admission {
            let input = crate::input_rules::RuleInput::for_dispatch_update(
                request.protected_input(),
                &admission.schema,
            );
            self.discharge(input, schemas)?;
        }
        Ok(self.seal_update(request, writeback))
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub(super) fn prepare_update_fixture(
        self,
        request: UpdateRequest,
        writeback: SecretPreservingAttributes,
        schemas: &SchemaRegistry,
    ) -> Result<PreparedWrite<UpdateRequest>, EffectFailureKind> {
        if let Some(admission) = &self.admission {
            let input = crate::input_rules::RuleInput::for_dispatch_update(
                request.protected_input(),
                &admission.schema,
            );
            self.discharge(input, schemas)?;
        }
        Ok(self.seal_update(request, writeback))
    }

    fn seal_update(
        self,
        request: UpdateRequest,
        writeback: SecretPreservingAttributes,
    ) -> PreparedWrite<UpdateRequest> {
        let resource_id = request.protected_input().id().clone();
        PreparedWrite {
            request: Arc::new(request),
            resource_id,
            writeback,
            receipt: Arc::new(PreparationReceipt {
                slot: self.slot,
                _children: Vec::new(),
                deletes: self.deletes,
                _create_request: None,
            }),
        }
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
    /// A delete-only scheduler may retry an actual completed attempt. Apply's
    /// paired delete slots never use this path or lose their discharge receipts.
    pub(super) fn retry_delete<S>(
        &mut self,
        index: usize,
        effect: &Effect<S>,
    ) -> Result<(), EffectFailureKind> {
        if self.required_deletes.contains_key(&index) || self.delete_targets.contains_key(&index) {
            return Err(EffectFailureKind::message(
                "delete retry slot is paired or already available",
            ));
        }
        let target = DeleteTarget::from_effect(effect)
            .ok_or_else(|| EffectFailureKind::message("delete retry slot is not a delete"))?;
        self.delete_targets.insert(index, target);
        Ok(())
    }
    pub(super) fn has_rules(&self, index: usize) -> bool {
        self.checks.contains_key(&index)
    }

    /// Deletes with no paired writes need no obligation-to-slot mapping.
    pub(super) fn without_write_checks<S>(effects: &[Effect<S>]) -> Self {
        Self {
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
        }
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
        let mut result = Self::without_write_checks(effects);
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
    use crate::input_rules::FinalizedPlan;
    use crate::plan::Plan;
    use crate::resource::Value;

    #[test]
    fn provider_selected_union_rules_are_enforced_at_create_and_update_dispatch() {
        use crate::schema::{
            AttributeSchema, AttributeType, InputCondition, InputLiteral, InputPath,
            InputPredicate, InputRule, InputRuleCode, InputRuleScope, ResourceSchema, StructField,
            TypeIdentity,
        };
        let arm = |name: &str, scalar: &str, rules| {
            AttributeType::struct_with_rules(
                name,
                vec![
                    StructField::required(
                        "kind",
                        AttributeType::refined_string(
                            Some(TypeIdentity::bare(scalar)),
                            None,
                            None,
                            None,
                        ),
                    )
                    .build(),
                    StructField::optional("guard", AttributeType::bool()).build(),
                    StructField::optional("target", AttributeType::bool()).build(),
                ],
                vec![],
                rules,
            )
        };
        let required = InputRule::value(
            InputRuleScope::EveryWrite,
            InputPath::new("target").unwrap(),
            InputRuleCode::new("union.target").unwrap(),
            InputPredicate::Present,
        )
        .with_condition(InputCondition::Equals {
            path: InputPath::new("guard").unwrap(),
            literal: InputLiteral::Bool(true),
        });
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("UnionInput")
                .attribute(
                    AttributeSchema::optional(
                        "node",
                        AttributeType::union(vec![
                            arm("A", "First", vec![]),
                            arm("B", "Second", vec![required]),
                        ]),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let mut catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::clone(&callbacks);
        catalog.custom_types = crate::parser::ProviderCustomTypes::loaded(
            HashSet::new(),
            Box::new(move |target, _| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let crate::parser::CustomTypeValidationTarget::Annotation(identity) = target else {
                    panic!("provider annotation callback")
                };
                if identity.identity().kind == "First" {
                    Err(crate::parser::CustomTypeValidationError::Rejected {
                        owner: "mock".into(),
                        identity: identity.identity().clone(),
                        message: "first arm rejected".into(),
                    })
                } else {
                    Ok(())
                }
            }),
        );
        let node = |guard| {
            Value::Concrete(crate::resource::ConcreteValue::Map(
                indexmap::IndexMap::from([
                    (
                        "kind".into(),
                        Value::Concrete(crate::resource::ConcreteValue::String("b".into())),
                    ),
                    (
                        "guard".into(),
                        Value::Concrete(crate::resource::ConcreteValue::Bool(guard)),
                    ),
                ]),
            ))
        };
        let bindings = crate::binding_index::ResolvedBindings::default();
        let gate = super::super::ModuleConstraintGate::from_live_compositions(&[]);
        crate::validation::deferred_types::with_session_for_test(&catalog, |type_checks| {
            let context = ProviderPreparationContext::new(
                &bindings,
                &gate,
                &[],
                &crate::provider::NoopNormalizer,
                &[],
                &schemas,
                type_checks,
            );
            for update in [false, true] {
                let resource =
                    crate::resource::Resource::with_provider("mock", "UnionInput", "example", None)
                        .with_attribute("node", node(false));
                let initial = futures::executor::block_on(super::super::prepare_create_request(
                    resource.clone(),
                    &context,
                ))
                .unwrap();
                let from = SnapshotState::existing(
                    resource.id.clone(),
                    crate::test_support::snapshot_attributes(HashMap::from([(
                        "node".into(),
                        node(false),
                    )])),
                )
                .with_identifier("existing");
                let mut plan = Plan::<crate::resource::ProviderModuleSource>::default();
                plan.add(if update {
                    Effect::Update {
                        from: Box::new(from.clone()),
                        identifier: AppliedIdentifier::new("existing").unwrap(),
                        to: initial.resource().as_inner().clone(),
                        changed_attributes: vec!["node".into()],
                    }
                } else {
                    Effect::Create(initial.resource().as_inner().clone())
                });
                let checked = FinalizedPlan::new(plan)
                    .check_inputs(&schemas, &HashMap::new())
                    .unwrap();
                let check = ExecutionInputChecks::new(
                    checked.execution_input_checks(),
                    checked.effects(),
                    &[0],
                )
                .unwrap()
                .take_write(0)
                .unwrap();
                let request = futures::executor::block_on(super::super::prepare_create_request(
                    resource.with_attribute("node", node(true)),
                    &context,
                ))
                .unwrap();
                let attributes = request.resource().as_resource().resolved_attributes();
                let writeback =
                    SecretPreservingAttributes::from_provider_values(&attributes, &attributes);
                let result = if update {
                    check
                        .prepare_update(
                            from,
                            &["node".into()],
                            request.resource().clone(),
                            writeback,
                            &schemas,
                        )
                        .map(|_| ())
                } else {
                    check
                        .prepare_create(request, writeback, &schemas)
                        .map(|_| ())
                };
                assert!(callbacks.load(std::sync::atomic::Ordering::SeqCst) > 0);
                let error = result
                    .expect_err("provider-selected B must enforce its local rule at dispatch");
                assert!(
                    error.to_string().contains("[union.target]"),
                    "update={update}: {error}"
                );
            }
        });
    }

    #[test]
    fn create_and_update_discharge_preserve_every_late_input_finding() {
        use crate::schema::{
            AttributeSchema, AttributeType, InputPath, InputPredicate, InputRule, InputRuleCode,
            InputRuleScope, NonEmptyString, ResourceSchema,
        };
        let string =
            |value: &str| Value::Concrete(crate::resource::ConcreteValue::String(value.into()));
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "",
            ResourceSchema::new("test")
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                .input_rule(InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("name").unwrap(),
                    InputRuleCode::new("name.prefix").unwrap(),
                    InputPredicate::NoPrefix(NonEmptyString::new("bad-").unwrap()),
                ))
                .input_rule(InputRule::value(
                    InputRuleScope::EveryWrite,
                    InputPath::new("name").unwrap(),
                    InputRuleCode::new("name.suffix").unwrap(),
                    InputPredicate::NoSuffix(NonEmptyString::new("-bad").unwrap()),
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
                for update in [false, true] {
                    let resource = crate::resource::Resource::new("test", "example")
                        .with_attribute("name", string("good"));
                    let initial = futures::executor::block_on(
                        super::super::prepare_create_request(resource.clone(), &context),
                    )
                    .unwrap();
                    let from = SnapshotState::existing(
                        resource.id.clone(),
                        crate::test_support::snapshot_attributes(HashMap::from([(
                            "name".into(),
                            string("old-good"),
                        )])),
                    )
                    .with_identifier("existing");
                    let mut plan = Plan::<crate::resource::ProviderModuleSource>::default();
                    plan.add(if update {
                        Effect::Update {
                            from: Box::new(from.clone()),
                            identifier: AppliedIdentifier::new("existing").unwrap(),
                            to: initial.resource().as_inner().clone(),
                            changed_attributes: vec!["name".into()],
                        }
                    } else {
                        Effect::Create(initial.resource().as_inner().clone())
                    });
                    let checked = FinalizedPlan::new(plan)
                        .check_inputs(&schemas, &HashMap::new())
                        .unwrap();
                    let check = ExecutionInputChecks::new(
                        checked.execution_input_checks(),
                        checked.effects(),
                        &[0],
                    )
                    .unwrap()
                    .take_write(0)
                    .unwrap();
                    let late = resource.with_attribute("name", string("bad-name-bad"));
                    let request = futures::executor::block_on(
                        super::super::prepare_create_request(late, &context),
                    )
                    .unwrap();
                    let attributes = request.resource().as_resource().resolved_attributes();
                    let writeback =
                        SecretPreservingAttributes::from_provider_values(&attributes, &attributes);
                    let error = if update {
                        match check.prepare_update(
                            from,
                            &["name".into()],
                            request.resource().clone(),
                            writeback,
                            &schemas,
                        ) {
                            Err(error) => error,
                            Ok(_) => panic!("late update must fail"),
                        }
                    } else {
                        match check.prepare_create(request, writeback, &schemas) {
                            Err(error) => error,
                            Ok(_) => panic!("late create must fail"),
                        }
                    };
                    let message = error.to_string();
                    assert!(
                        message.contains("[name.prefix]") && message.contains("[name.suffix]"),
                        "update={update}: {message}"
                    );
                }
            },
        );
    }

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
    fn input_discharge_scheduled_requests_check_create_and_update_scopes() {
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
                        let admit = |effect: Effect<crate::resource::ProviderModuleSource>| {
                            let mut plan = Plan::default();
                            plan.add(effect);
                            let checked = crate::input_rules::FinalizedPlan::new(plan)
                                .check_inputs(&schemas, &HashMap::new())?;
                            let scheduled = checked.schedule(&HashMap::new());
                            let ready = super::super::scheduler::release_basic_for_test(
                                &scheduled,
                                0,
                                &HashSet::new(),
                            )
                            .expect("fixture write is initially ready");
                            let check = ExecutionInputChecks::new(
                                checked.execution_input_checks(),
                                scheduled.effects(),
                                &[0],
                            )
                            .unwrap()
                            .take_write(0)
                            .unwrap();
                            Ok::<_, crate::input_rules::InputAdmissionFailure<_>>((ready, check))
                        };
                        match admit(Effect::Create(create.resource().as_inner().clone())) {
                            Ok((ready, check)) => {
                                let attributes =
                                    create.resource().as_resource().resolved_attributes();
                                let writeback = SecretPreservingAttributes::from_provider_values(
                                    &attributes,
                                    &attributes,
                                );
                                let write =
                                    check.prepare_create(create, writeback, &schemas).unwrap();
                                let super::super::scheduler::ScheduledBasicEffect::Create {
                                    create,
                                    ..
                                } = ready
                                else {
                                    panic!("expected fixture create")
                                };
                                let prepared = create.authorize(write);
                                assert_eq!(value, "valid-name");
                                assert_eq!(
                                    prepared.request().resource().attributes["name"],
                                    string(value)
                                );
                            }
                            Err(plan) => {
                                assert_eq!(value, "192.168.1.1");
                                assert!(
                                    matches!(&plan.errors()[0].kind, crate::plan::PlanErrorKind::InputRule { code, .. } if code == "test.name.not_ipv4")
                                );
                            }
                        }
                        let from = crate::resource::SnapshotState::not_found(resource.id.clone());
                        let desired = futures::executor::block_on(
                            super::super::provider_ready::prepare_provider_ready_resource(
                                resource, &context,
                            ),
                        )
                        .unwrap();
                        let update = UpdateRequest::checked(
                            from,
                            &["name".into()],
                            &desired,
                            super::super::provider_ready::managed_update_schema(
                                &schemas,
                                desired.as_resource(),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                        let planned = desired.as_resource().clone();
                        match admit(Effect::Update {
                            from: Box::new(update.from().clone()),
                            identifier: AppliedIdentifier::new("old-id").unwrap(),
                            to: crate::resource::ResolvedResource::new(planned),
                            changed_attributes: vec!["name".into()],
                        }) {
                            Ok((ready, check)) => {
                                let attributes = desired
                                    .as_resource()
                                    .attributes
                                    .clone()
                                    .into_iter()
                                    .collect();
                                let writeback = SecretPreservingAttributes::from_provider_values(
                                    &attributes,
                                    &attributes,
                                );
                                let write = check
                                    .prepare_update(
                                        update.from().clone(),
                                        &["name".into()],
                                        desired,
                                        writeback,
                                        &schemas,
                                    )
                                    .unwrap();
                                let super::super::scheduler::ScheduledBasicEffect::Update {
                                    update,
                                    ..
                                } = ready
                                else {
                                    panic!("expected fixture update")
                                };
                                let prepared = update.authorize(write);
                                assert!(
                                    value == "valid-name" || scope == InputRuleScope::CreateOnly
                                );
                                assert_eq!(
                                    prepared.request().patch().ops[0].sent_value(),
                                    Some(&string(value))
                                );
                            }
                            Err(plan) => {
                                assert_eq!(scope, InputRuleScope::EveryWrite);
                                assert_eq!(value, "192.168.1.1");
                                assert!(
                                    matches!(&plan.errors()[0].kind, crate::plan::PlanErrorKind::InputRule { code, .. } if code == "test.name.not_ipv4")
                                );
                            }
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
