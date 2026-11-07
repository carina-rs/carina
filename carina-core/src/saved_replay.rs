//! Saved recording and replay retain core planning values through serialization and restoration.
//! Saved-plan files are user input. The CLI checks lineage, serial and fingerprints;
//! core resolves their effects and retains that source through scheduling.
use crate::input_rules::{
    FinalizedPlan, InputAdmissionFailure, InputCheckedPlan, InputTypeContext,
};
use crate::plan::{AdmittedPlan, Plan, UnresolvedSavedPlan};
use crate::resource::{
    Current, DataSource, NoPlaceholder, PlanValueKind, Resource, ResourceId, SavedModuleSource,
    SnapshotLeaf, SnapshotState, Value,
};
use crate::schema::SchemaRegistry;
use crate::value::SecretRehydrationSiteError;
use std::collections::HashMap;
use std::ops::Deref;

/// A decoded saved source resolved by core. No raw-plan constructor or mutable access.
#[derive(Debug, Clone)]
pub struct SavedPlanReplay<V: PlanValueKind = NoPlaceholder> {
    plan: Plan<SavedModuleSource, V>,
}
impl<V: PlanValueKind> Deref for SavedPlanReplay<V> {
    type Target = Plan<SavedModuleSource, V>;
    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}
impl UnresolvedSavedPlan {
    pub fn resolve_for_replay(
        self,
        schemas: &SchemaRegistry,
    ) -> Result<SavedPlanReplay<SnapshotLeaf>, crate::resource::ResolvedResourceError> {
        Ok(SavedPlanReplay {
            plan: self.resolve_with_schemas(schemas)?,
        })
    }
}
impl SavedPlanReplay<SnapshotLeaf> {
    pub fn rehydrate_desired(
        &self,
        resources: &HashMap<ResourceId, Resource>,
        data_sources: &HashMap<ResourceId, DataSource>,
        import_identifiers: &HashMap<ResourceId, Value>,
        templates: &crate::parser::ResolvedDeferredForExpressions,
    ) -> Result<SavedPlanReplay, SecretRehydrationSiteError> {
        Ok(SavedPlanReplay {
            plan: crate::value::rehydrate_plan_desired_secrets(
                &self.plan,
                resources,
                data_sources,
                import_identifiers,
                templates,
            )?,
        })
    }
}
impl<V: PlanValueKind> SavedPlanReplay<V> {
    pub fn rehydrate_historical(
        &self,
        redacted: &HashMap<ResourceId, SnapshotState>,
        verified: &HashMap<ResourceId, SnapshotState>,
    ) -> Result<Self, SecretRehydrationSiteError> {
        Ok(Self {
            plan: crate::value::rehydrate_plan_historical_secrets_from_verified_states(
                &self.plan, redacted, verified,
            )?,
        })
    }
    pub fn rebase_ignored_update_sources<P: crate::resource::StateValueLeaf>(
        &mut self,
        actual: &HashMap<ResourceId, crate::resource::State<P>>,
    ) {
        self.plan.rebase_ignored_update_sources(actual);
    }
}

/// Replacement admission preserves the decoded saved-plan value through scheduling.
#[derive(Debug, Clone)]
pub struct AdmittedSavedPlanReplay {
    plan: AdmittedPlan<SavedModuleSource>,
}
impl Deref for AdmittedSavedPlanReplay {
    type Target = AdmittedPlan<SavedModuleSource>;
    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}
impl SavedPlanReplay {
    pub fn admit(
        self,
        schemas: &SchemaRegistry,
    ) -> Result<AdmittedSavedPlanReplay, crate::replacement::ReplacementAdmissionError> {
        Ok(AdmittedSavedPlanReplay {
            plan: self.plan.admit(schemas)?,
        })
    }
}
impl AdmittedSavedPlanReplay {
    pub fn check_inputs_with_types(
        &self,
        schemas: &SchemaRegistry,
        current: &HashMap<ResourceId, Current>,
        types: &InputTypeContext<SavedModuleSource>,
    ) -> Result<InputCheckedPlan<SavedModuleSource>, InputAdmissionFailure<SavedModuleSource>> {
        FinalizedPlan::from_saved_plan(self.plan.clone().into_unadmitted())
            .check_inputs_with_types(schemas, current, types)
    }
}

/// A recording snapshot produced only by redacting a core-checked plan.
/// It cannot be constructed from a raw Plan or mutated after redaction.
///
/// ```compile_fail,E0451
/// use carina_core::{saved_replay::RecordedPlanSnapshot, plan::Plan};
/// let forged = RecordedPlanSnapshot { plan: Plan::new().to_snapshot() };
/// ```
#[derive(Debug)]
pub struct RecordedPlanSnapshot {
    plan: crate::plan::SnapshotPlan,
}
impl RecordedPlanSnapshot {
    pub fn plan(&self) -> &crate::plan::SnapshotPlan {
        &self.plan
    }
    pub fn into_wire(self) -> UnresolvedSavedPlan {
        UnresolvedSavedPlan::from_recorded_snapshot(self.plan.into())
    }
}
impl InputCheckedPlan {
    pub fn record_snapshot<D>(
        &self,
        desired: &HashMap<
            ResourceId,
            Resource<
                crate::resource::ModuleSource,
                SnapshotLeaf,
                crate::resource::AbsoluteBindingAddress,
                D,
            >,
        >,
        historical: &HashMap<ResourceId, SnapshotState>,
    ) -> Result<RecordedPlanSnapshot, crate::value::SerializationError> {
        Ok(RecordedPlanSnapshot {
            plan: crate::value::redact_secrets_in_plan_with_desired_and_historical(
                self.plan(),
                desired,
                historical,
            )?,
        })
    }
}
