use super::*;

pub struct SavedDocumentConversion;
impl SavedDocumentConversion {
    pub fn new<'a>(
        exports: impl IntoIterator<Item = &'a Value>,
    ) -> Result<Self, SerializationError> {
        let conversion = Self;
        for value in exports {
            conversion.check(value)?;
        }
        Ok(conversion)
    }
    pub fn admit_expression<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
        &self,
        value: &Value<P, B>,
    ) -> Result<(), SerializationError> {
        self.check(value)
    }
    pub fn value(&self, value: &Value) -> Result<SnapshotValue, SerializationError> {
        self.check(value)?;
        redact_secrets_in_value(value)
    }
    pub fn state_value<P: crate::resource::StateValueLeaf>(
        &self,
        value: &Value<P>,
    ) -> Result<SnapshotValue, SerializationError> {
        self.check(value)?;
        redact_state_snapshot_value(&crate::resource::state_value_snapshot(value))
    }
    pub fn redact_secrets_in_resource<S: Clone, D: Clone>(
        &self,
        resource: &crate::resource::Resource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
    ) -> Result<
        crate::resource::Resource<S, SnapshotLeaf, crate::resource::AbsoluteBindingAddress, D>,
        SerializationError,
    > {
        redact_secrets_in_resource_with_admission(self, resource)
    }
    pub fn redact_secrets_in_resource_reusing_persisted_hashes<S: Clone, D: Clone>(
        &self,
        resource: &crate::resource::Resource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
        persisted: &crate::provider::PersistedAttributes,
    ) -> Result<
        crate::resource::Resource<S, SnapshotLeaf, crate::resource::AbsoluteBindingAddress, D>,
        SerializationError,
    > {
        redact_secrets_in_resource_reusing_persisted_hashes_with_admission(
            self, resource, persisted,
        )
    }
    pub fn redact_secrets_in_resource_using_evaluated<S: Clone, D: Clone, T, E>(
        &self,
        source: &crate::resource::Resource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
        evaluated: &crate::resource::Resource<
            T,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            E,
        >,
    ) -> Result<
        crate::resource::Resource<S, SnapshotLeaf, crate::resource::AbsoluteBindingAddress, D>,
        SerializationError,
    > {
        redact_secrets_in_resource_using_evaluated_with_admission(self, source, evaluated)
    }
    pub fn redact_secrets_in_data_source<S: Clone, D: Clone>(
        &self,
        resource: &crate::resource::DataSource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
    ) -> Result<
        crate::resource::DataSource<S, SnapshotLeaf, crate::resource::AbsoluteBindingAddress, D>,
        SerializationError,
    > {
        redact_secrets_in_data_source_with_admission(self, resource)
    }
    pub fn redact_secrets_in_data_source_using_evaluated<S: Clone, D: Clone, T, E>(
        &self,
        source: &crate::resource::DataSource<
            S,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
        evaluated: &crate::resource::DataSource<
            T,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            E,
        >,
    ) -> Result<
        crate::resource::DataSource<S, SnapshotLeaf, crate::resource::AbsoluteBindingAddress, D>,
        SerializationError,
    > {
        redact_secrets_in_data_source_using_evaluated_with_admission(self, source, evaluated)
    }
    pub fn redact_secrets_in_live_composition(
        &self,
        resource: &crate::resource::LiveComposition,
    ) -> Result<crate::resource::LiveComposition<SnapshotLeaf>, SerializationError> {
        redact_secrets_in_live_composition_with_admission(self, resource)
    }
    pub fn redact_secrets_in_state<P: crate::resource::StateValueLeaf>(
        &self,
        state: &crate::resource::State<P>,
    ) -> Result<crate::resource::SnapshotState, SerializationError> {
        redact_secrets_in_state_with_admission(self, state)
    }
    pub fn redact_secrets_in_state_using_resource<D, P: crate::resource::StateValueLeaf>(
        &self,
        state: &crate::resource::State<P>,
        desired: &crate::resource::Resource<
            crate::resource::ModuleSource,
            crate::resource::NoPlaceholder,
            crate::resource::AbsoluteBindingAddress,
            D,
        >,
    ) -> Result<crate::resource::SnapshotState, SerializationError> {
        redact_secrets_in_state_using_resource_with_admission(self, state, desired)
    }
    pub fn redact_secrets_in_state_using_persisted<P: crate::resource::StateValueLeaf>(
        &self,
        state: &crate::resource::State<P>,
        persisted: &crate::provider::PersistedAttributes,
    ) -> Result<crate::resource::SnapshotState, SerializationError> {
        redact_secrets_in_state_using_persisted_with_admission(self, state, persisted)
    }
    pub fn redact_secrets_in_plan_with_desired_and_historical<D>(
        &self,
        plan: &crate::plan::Plan,
        planned_resources: &HashMap<
            crate::resource::ResourceId,
            crate::resource::Resource<
                crate::resource::ModuleSource,
                SnapshotLeaf,
                crate::resource::AbsoluteBindingAddress,
                D,
            >,
        >,
        historical: &HashMap<crate::resource::ResourceId, crate::resource::SnapshotState>,
    ) -> Result<crate::plan::SnapshotPlan, SerializationError> {
        redact_secrets_in_plan_with_desired_and_historical_with_admission(
            self,
            plan,
            planned_resources,
            historical,
        )
    }
}
impl SnapshotAdmission for SavedDocumentConversion {
    fn check<P: crate::resource::ValueKind, B: crate::resource::BindingAddress>(
        &self,
        value: &Value<P, B>,
    ) -> Result<(), SerializationError> {
        if crate::function_evaluation::contains_deferred_user_function_call(value) {
            Err(RetainedUserFunctionCallError.into())
        } else {
            Ok(())
        }
    }
}
