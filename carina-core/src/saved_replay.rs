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
        let mut plan = crate::value::rehydrate_plan_desired_secrets(
            &self.plan,
            resources,
            data_sources,
            import_identifiers,
            templates,
        )?;
        #[allow(clippy::iter_over_hash_type)]
        // Keyed runtime metadata; iteration order never reaches output.
        plan.retain_observation_desired(
            resources
                .values()
                .map(|resource| (&resource.id, &resource.attributes)),
        );
        Ok(SavedPlanReplay { plan })
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
/// use carina_core::{saved_replay::RecordedPlanSnapshot, plan::SnapshotPlan, value::Saved};
/// fn forge(plan: Saved<SnapshotPlan>) -> RecordedPlanSnapshot {
///     RecordedPlanSnapshot { plan }
/// }
/// ```
#[derive(Debug)]
pub struct RecordedPlanSnapshot {
    plan: crate::value::Saved<crate::plan::SnapshotPlan>,
}
impl RecordedPlanSnapshot {
    pub fn plan(&self) -> &crate::plan::SnapshotPlan {
        self.plan.as_inner()
    }
    pub fn into_wire(self) -> UnresolvedSavedPlan {
        crate::plan::UnresolvedSavedPlan::from_recorded_snapshot(self.plan.into_inner().into())
    }
}
impl InputCheckedPlan {
    /// Raw historical maps cannot enter saved-plan output.
    /// Previously: the raw-map call compiled successfully.
    ///
    /// ```compile_fail,E0308
    /// use std::collections::HashMap;
    /// use carina_core::{input_rules::InputCheckedPlan, resource::{ResourceId, Resource, SnapshotLeaf, SnapshotState}, value::{Saved, SavedDocumentConversion}};
    /// fn bypass(checked: &InputCheckedPlan, conversion: &SavedDocumentConversion, desired: &HashMap<ResourceId, Saved<Resource<carina_core::resource::ModuleSource, SnapshotLeaf>>>, raw: &HashMap<ResourceId, SnapshotState>) {
    ///     let _ = checked.record_snapshot(conversion, desired, raw);
    /// }
    /// ```
    /// Detached subsets cannot replace the complete observation batch.
    ///
    /// ```compile_fail,E0308
    /// use std::collections::HashMap;
    /// use carina_core::{input_rules::InputCheckedPlan, resource::{ResourceId, Resource, SnapshotLeaf, ModuleSource}, value::{ProtectedObservation, Saved, SavedDocumentConversion}};
    /// fn omit(checked: &InputCheckedPlan, conversion: &SavedDocumentConversion, desired: &HashMap<ResourceId, Saved<Resource<ModuleSource, SnapshotLeaf>>>) {
    ///     let _ = checked.record_snapshot(conversion, desired, std::iter::empty::<&ProtectedObservation>());
    /// }
    /// ```
    pub fn record_snapshot<D: Clone>(
        &self,
        conversion: &crate::value::SavedDocumentConversion,
        desired: &HashMap<
            ResourceId,
            crate::value::Saved<
                Resource<
                    crate::resource::ModuleSource,
                    SnapshotLeaf,
                    crate::resource::AbsoluteBindingAddress,
                    D,
                >,
            >,
        >,
        observations: &crate::value::ProtectedObservations,
    ) -> Result<RecordedPlanSnapshot, crate::value::SerializationError> {
        let plan = observations.protect_plan_history(self.plan())?;
        let historical = observations
            .recorded_history()?
            .into_iter()
            .map(|(id, state)| conversion.snapshot(state).map(|state| (id, state)))
            .collect::<Result<HashMap<_, _>, crate::value::SerializationError>>()?;
        Ok(RecordedPlanSnapshot {
            plan: conversion.redact_secrets_in_plan_with_desired_and_historical(
                &plan,
                desired,
                &historical,
            )?,
        })
    }
}

impl
    crate::provider_text::SavedProviderConfiguration<
        NoPlaceholder,
        crate::provider_text::SavedInputsResolved,
    >
{
    /// The loaded configuration owns all saved desired resources, including
    /// unchanged resources that have no effect. Bind that owner during replay.
    pub fn bind_observation_plan(&self, mut plan: SavedPlanReplay) -> SavedPlanReplay {
        plan.plan.retain_observation_desired(
            self.resources()
                .iter()
                .map(|resource| (&resource.id, &resource.attributes)),
        );
        plan
    }
}
