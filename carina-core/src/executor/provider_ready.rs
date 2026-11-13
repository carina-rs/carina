//! Checked provider-boundary preparation.
//!
//! [`ProviderReady`] is the capability required by provider requests. Its
//! payload is private and this module is the only place that constructs it,
//! so normalization or identity resolution alone cannot cross the provider
//! boundary without value-constraint validation.

#[cfg(test)]
use crate::provider::ProviderFactory;
#[cfg(test)]
use crate::provider::ProviderNormalizer;
#[cfg(test)]
use crate::resource::ParserPlaceholder;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Deref;
use std::sync::Mutex;

use indexmap::IndexMap;
use thiserror::Error;

use crate::binding_index::ResolvedBindings;
use crate::executor::normalized::{apply_dispatch_desired_normalization, unwrap_secret};
pub use crate::module_resolver::ModuleConstraintFailure;
use crate::module_resolver::{
    evaluate_live_pending_constraints, evaluate_saved_pending_constraints,
};
use crate::parser::ProviderConfig;
use crate::provider::{
    CreateRequest, HostNormalizer, ProviderDirectives, ProviderWireError, ProviderWireTarget,
    ResourceWirePayload, UpdateRequest, WireAttributes, WireReady,
};
use crate::resource::{
    AbsoluteBindingAddress, DataSource, LiveComposition, NoPlaceholder, ProviderDataSource,
    ProviderModuleSource, ProviderResource, ResolvedDataSource, ResolvedResource, Resource,
    ResourceId, SavedComposition, Value,
};
use crate::schema::{
    CompiledResourceSchema as ResourceSchema, SchemaKind, SchemaRegistry, TypeError,
};
use crate::validation::admission::{
    RechecksResolved,
    final_input::{FinalInput, admit_dispatch},
    kind,
};
use crate::value::SerializationError;

#[cfg(test)]
#[path = "provider_ready_admission_tests.rs"]
mod attribute_admission_tests;

/// An immutable value that passed the provider-boundary constraint gate.
///
/// The fields are deliberately private. There is no `new`, `From`,
/// `Default`, or deserialization path; callers obtain this capability only
/// from [`prepare_provider_ready_resource`].
///
/// ```compile_fail
/// use carina_core::provider::{ProviderReady, ProviderReadyResource};
/// fn rebind(ready: ProviderReadyResource) -> ProviderReadyResource {
///     ProviderReady { admission: ready.admission, ..ready } // private fields
/// }
/// ```
///
/// ```compile_fail
/// use carina_core::provider::ProviderReadyResource;
/// use carina_core::resource::{ProviderModuleSource, ResolvedResource};
/// fn bypass(raw: ResolvedResource<ProviderModuleSource>) -> ProviderReadyResource {
///     raw.into()
/// }
/// ```
///
/// ```compile_fail
/// use carina_core::provider::ProviderReadyResource;
/// use carina_core::resource::Value;
/// fn mutate(ready: &mut ProviderReadyResource, value: Value) {
///     ready.as_resource().attributes.insert("unchecked".into(), value);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ProviderReady<T: ResourceWirePayload, K: RechecksResolved = kind::ResourceAttribute> {
    admission: FinalInput<DispatchPayload<WireReady<T>>>,
    directives: ProviderDirectives,
    selection: crate::input_rules::InputSelectionAuthority,
    kind: std::marker::PhantomData<K>,
}

impl<T: ResourceWirePayload + PartialEq, K: RechecksResolved> PartialEq for ProviderReady<T, K> {
    fn eq(&self, other: &Self) -> bool {
        self.admission == other.admission && self.directives == other.directives
    }
}

impl<T: ResourceWirePayload, K: RechecksResolved> ProviderReady<T, K> {
    fn settled(
        pending: crate::validation::deferred_types::PendingProviderTypeChecks<
            '_,
            (
                FinalInput<DispatchPayload<WireReady<T>>>,
                ProviderDirectives,
            ),
        >,
        selection: crate::input_rules::InputSelectionAuthority,
    ) -> Result<Self, ProviderPreparationError> {
        let (admission, directives) = settle_preparation(pending)?;
        Ok(Self {
            directives,
            admission,
            selection,
            kind: std::marker::PhantomData,
        })
    }

    fn diagnostic_attributes(&self) -> &IndexMap<String, Value> {
        self.admission.payload().diagnostic_attributes()
    }

    pub(crate) fn diagnostic_values(&self) -> impl Iterator<Item = &Value> {
        self.diagnostic_attributes().values()
    }
    pub(crate) fn diagnostic_value(&self, key: &str, sent: &Value) -> Value {
        fn annotate(sent: &Value, source: &Value) -> Value {
            use crate::resource::{ConcreteValue, DeferredValue};
            match (sent, source) {
                (_, Value::Deferred(DeferredValue::Secret(_))) => sent.clone().into_secret(),
                (
                    Value::Concrete(ConcreteValue::Map(values)),
                    Value::Concrete(ConcreteValue::Map(source)),
                ) => Value::Concrete(ConcreteValue::Map(
                    values
                        .iter()
                        .map(|(key, value)| {
                            (
                                key.clone(),
                                source.get(key).map_or_else(
                                    || value.clone(),
                                    |source| annotate(value, source),
                                ),
                            )
                        })
                        .collect(),
                )),
                (
                    Value::Concrete(ConcreteValue::List(values)),
                    Value::Concrete(ConcreteValue::List(source)),
                ) => Value::Concrete(ConcreteValue::List(
                    values
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            source
                                .get(index)
                                .map_or_else(|| value.clone(), |source| annotate(value, source))
                        })
                        .collect(),
                )),
                _ => sent.clone(),
            }
        }
        self.diagnostic_attributes()
            .get(key)
            .map_or_else(|| sent.clone(), |source| annotate(sent, source))
    }

    /// Borrow the checked payload.
    pub fn as_inner(&self) -> &T {
        self.admission.payload().payload().as_inner()
    }

    /// Borrow the schema-resolved directives paired with this payload.
    pub fn directives(&self) -> &ProviderDirectives {
        &self.directives
    }

    /// The complete projection checked before native or WASM dispatch.
    pub fn wire_attributes(&self) -> &WireAttributes {
        self.admission.payload().payload().wire()
    }
}

impl<T: ResourceWirePayload, K: RechecksResolved> Deref for ProviderReady<T, K> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.as_inner()
    }
}

/// Preparation binds the exact transport payload to its protected source.
/// Only the two constructors below may strip annotations; callers of final
/// admission cannot supply a separate, potentially mismatched diagnostic map.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DispatchPayload<T> {
    payload: T,
    diagnostic_attributes: IndexMap<String, Value>,
}

impl<T> DispatchPayload<T> {
    pub(crate) fn payload(&self) -> &T {
        &self.payload
    }
    pub(crate) fn diagnostic_attributes(&self) -> &IndexMap<String, Value> {
        &self.diagnostic_attributes
    }
}

impl DispatchPayload<ResolvedResource<ProviderModuleSource>> {
    fn resource(
        resource: ResolvedResource<ProviderModuleSource>,
        restored: &super::normalized::RestoredSecrets,
        schemas: &SchemaRegistry,
    ) -> Result<Self, crate::resource::ResolvedResourceError> {
        let diagnostic_attributes = resource.as_resource().attributes.clone();
        let mut resource = resource.into_inner();
        restored.remove(&mut resource.attributes);
        Ok(Self {
            payload: ResolvedResource::resolve_with_schemas(resource, schemas)?,
            diagnostic_attributes,
        })
    }
}

impl DispatchPayload<ResolvedDataSource<ProviderModuleSource>> {
    fn data_source(value: ResolvedDataSource<ProviderModuleSource>) -> Self {
        let diagnostic_attributes = value.attributes.clone();
        let mut value = value.into_inner();
        for attribute in value.attributes.values_mut() {
            *attribute = unwrap_secret(attribute);
        }
        Self {
            payload: ResolvedDataSource::new(value),
            diagnostic_attributes,
        }
    }
}

impl<T: ResourceWirePayload> DispatchPayload<T> {
    pub(crate) fn into_wire(self) -> Result<DispatchPayload<WireReady<T>>, ProviderWireError> {
        let target = ProviderWireTarget::Resource(self.payload.resource_id().clone());
        Ok(DispatchPayload {
            payload: WireReady::try_new(self.payload, target).map_err(|error| {
                match self.diagnostic_attributes.get(error.attribute()) {
                    Some(source) => error.with_diagnostic_source(source),
                    None => error,
                }
            })?,
            diagnostic_attributes: self.diagnostic_attributes,
        })
    }
}

#[derive(Debug)]
struct PreparationDiagnostics(Vec<crate::validation::deferred_types::ProviderTypeCheckDiagnostic>);
impl fmt::Display for PreparationDiagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, diagnostic) in self.0.iter().enumerate() {
            if index != 0 {
                writeln!(f)?;
            }
            let (crate::validation::deferred_types::ProviderTypeCheckDiagnostic::Deferred(finding)
                | crate::validation::deferred_types::ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(finding)) = diagnostic;
            write!(f, "{}", finding.message)?;
        }
        Ok(())
    }
}
impl std::error::Error for PreparationDiagnostics {}

fn settle_preparation<T>(
    pending: crate::validation::deferred_types::PendingProviderTypeChecks<'_, T>,
) -> Result<T, ProviderPreparationError> {
    let mut diagnostics = Vec::new();
    let output = pending.publish(&mut diagnostics)?;
    if diagnostics.is_empty() {
        return Ok(output);
    }
    Err(ProviderPreparationError::Provider(
        crate::provider::HostError::invalid_input_caused_by(
            "provider type checks remain unsettled",
            crate::provider_text::HostDiagnostic::capture_host_or_provider(PreparationDiagnostics(
                diagnostics,
            )),
        ),
    ))
}

/// A fully resolved managed resource whose known value constraints have run.
/// Its transport attributes are distinct from protected input-rule evidence:
/// the latter can only be constructed inside provider preparation.
///
/// ```compile_fail,E0603
/// use carina_core::executor::provider_ready::ProtectedDesiredTree;
/// ```
pub type ProviderReadyResource = ProviderReady<ResolvedResource<ProviderModuleSource>>;

/// Full resolved desired tree, with protection annotations intact. Only provider
/// preparation owns this capability; a transport attribute map cannot forge it.
pub(crate) struct ProtectedDesiredTree<'a> {
    resource_id: &'a ResourceId,
    attributes: &'a IndexMap<String, Value>,
    ignored: &'a crate::resource::ResolvedIgnoreChanges,
    selection: &'a crate::input_rules::InputSelectionAuthority,
}

impl ProtectedDesiredTree<'_> {
    pub(crate) fn selection_authority(&self) -> &crate::input_rules::InputSelectionAuthority {
        self.selection
    }
    pub(crate) fn id(&self) -> &ResourceId {
        self.resource_id
    }
    pub(crate) fn attributes(&self) -> &IndexMap<String, Value> {
        self.attributes
    }
    pub(crate) fn ignored(&self) -> &crate::resource::ResolvedIgnoreChanges {
        self.ignored
    }
}
impl ProviderReadyResource {
    pub(crate) fn protected_desired_tree(&self) -> ProtectedDesiredTree<'_> {
        ProtectedDesiredTree {
            resource_id: &self.id,
            attributes: self.diagnostic_attributes(),
            ignored: self.ignore_changes(),
            selection: &self.selection,
        }
    }
}

/// A fully resolved data source whose known input constraints have run.
pub type ProviderReadyDataSource =
    ProviderReady<ResolvedDataSource<ProviderModuleSource>, kind::DataSourceAttribute>;

pub use crate::validation::admission::configuration::provider::{
    CanonicalProviderConfigAttributes, ProviderReadyConfig,
};

/// Dependencies shared by every checked provider-boundary preparation.
///
/// Keeping these values together gives create, update, and data-source
/// preparation one coherent execution snapshot, rather than letting each
/// downstream helper accept independently supplied views.
pub struct ProviderPreparationContext<'a> {
    bindings: &'a ResolvedBindings,
    module_gate: &'a ModuleConstraintGate,
    provider_configs: &'a [ProviderConfig<
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
    >],
    normalizer: &'a dyn HostNormalizer,
    type_checks: &'a dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    schemas: &'a SchemaRegistry,
}

impl<'a> ProviderPreparationContext<'a> {
    pub fn new(
        bindings: &'a ResolvedBindings,
        module_gate: &'a ModuleConstraintGate,
        provider_configs: &'a crate::parser::ResolvedProviderConfigs,
        normalizer: &'a dyn HostNormalizer,
        schemas: &'a SchemaRegistry,
        type_checks: &'a dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Self {
        Self::with_configs(
            bindings,
            module_gate,
            provider_configs.as_provider_configs(),
            normalizer,
            schemas,
            type_checks,
        )
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(
        bindings: &'a ResolvedBindings,
        module_gate: &'a ModuleConstraintGate,
        provider_configs: &'a [ProviderConfig<NoPlaceholder, AbsoluteBindingAddress>],
        normalizer: &'a dyn HostNormalizer,
        schemas: &'a SchemaRegistry,
        type_checks: &'a dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Self {
        crate::parser::ResolvedProviderConfigs::from_test_configs(provider_configs.to_vec())
            .expect("provider preparation fixtures must be known");
        Self::with_configs(
            bindings,
            module_gate,
            provider_configs,
            normalizer,
            schemas,
            type_checks,
        )
    }

    fn with_configs(
        bindings: &'a ResolvedBindings,
        module_gate: &'a ModuleConstraintGate,
        provider_configs: &'a [ProviderConfig<NoPlaceholder, AbsoluteBindingAddress>],
        normalizer: &'a dyn HostNormalizer,
        schemas: &'a SchemaRegistry,
        type_checks: &'a dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Self {
        Self {
            bindings,
            module_gate,
            provider_configs,
            normalizer,
            schemas,
            type_checks,
        }
    }

    pub(super) fn type_checks(
        &self,
    ) -> &dyn crate::validation::deferred_types::ProviderTypeCheckContext {
        self.type_checks
    }

    pub(super) fn bindings(&self) -> &ResolvedBindings {
        self.bindings
    }

    pub(super) fn schemas(&self) -> &SchemaRegistry {
        self.schemas
    }

    pub(crate) fn prepare_update_patch(
        &self,
        changed: &[String],
        resource: &ProviderReadyResource,
        from: &crate::resource::SnapshotState,
    ) -> Result<FinalInput<WireReady<crate::provider::UpdatePatch>>, ProviderPreparationError> {
        let schema = self
            .schemas
            .get_for(resource.as_resource())
            .ok_or_else(|| ProviderPreparationError::MissingManagedSchema {
                resource: resource.id.clone(),
            })?;
        let patch = crate::provider::build_update_patch(
            changed,
            resource,
            from,
            schema.schema().as_schema(),
        )?;
        let admitted = crate::validation::admission::final_input::admit_update_patch(
            patch,
            resource,
            schema,
            self.bindings,
            self.type_checks,
        )?;
        settle_preparation(self.type_checks.pending(admitted))
    }
}

/// Query-only preparation does not consume defaults and cannot prepare a
/// managed create or update. Preview planning and read-only commands use this
/// route for data-source refreshes.
pub struct DataSourcePreparationContext<'a> {
    inner: ProviderPreparationContext<'a>,
}

impl<'a> DataSourcePreparationContext<'a> {
    pub fn new(
        bindings: &'a ResolvedBindings,
        module_gate: &'a ModuleConstraintGate,
        schemas: &'a SchemaRegistry,
        type_checks: &'a dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Self {
        Self {
            inner: ProviderPreparationContext::with_configs(
                bindings,
                module_gate,
                &[],
                &crate::provider::NoopNormalizer,
                schemas,
                type_checks,
            ),
        }
    }
}

pub fn prepare_data_source_query(
    resource: DataSource,
    context: &DataSourcePreparationContext<'_>,
) -> Result<ProviderReadyDataSource, ProviderPreparationError> {
    prepare_provider_ready_data_source(resource, &context.inner)
}

/// Whether a provider-bound value is still in its authored form or came from
/// the plan after normalization. The latter always carries the authored
/// snapshot; omitting it is not representable at the preparation seam.
pub(super) enum CheckInput<T, A = T> {
    Authored(T),
    PlanNormalized { resolved: T, authored: A },
}

pub(super) type ResourceCheckInput<S> =
    CheckInput<Resource<S, NoPlaceholder, AbsoluteBindingAddress, ()>>;
pub(super) type DataSourceCheckInput<S> =
    CheckInput<DataSource<S, NoPlaceholder, AbsoluteBindingAddress, ()>>;

/// A resolved provider payload paired with the only valid schema-check scope.
///
/// The resolver constructs this value while it resolves references, so the
/// validation attribute map and the set that became known at apply cannot be
/// supplied independently by a call site.
pub(super) struct ResolvedCheckInput<T> {
    value: T,
    check: ResourceValueCheck,
}

impl<T> ResolvedCheckInput<T> {
    pub(super) fn authored(value: T) -> Self {
        Self {
            value,
            check: ResourceValueCheck::All,
        }
    }

    pub(super) fn plan_normalized(
        value: T,
        attributes: HashMap<String, Value>,
        names: HashSet<String>,
    ) -> Self {
        Self {
            value,
            check: ResourceValueCheck::ApplyResolved { attributes, names },
        }
    }

    pub(super) fn value(&self) -> &T {
        &self.value
    }
}

/// Resolution failures and evaluated violations remain distinct at every gate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModuleConstraintGateError {
    #[error(transparent)]
    UnavailableSecret(#[from] crate::module_resolver::ModuleConstraintResolutionError),
    #[error(transparent)]
    Violations(#[from] ModuleConstraintViolations),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleConstraintViolations {
    failures: Vec<ModuleConstraintFailure>,
}

impl ModuleConstraintViolations {
    pub fn failures(&self) -> &[ModuleConstraintFailure] {
        &self.failures
    }

    pub fn into_failures(self) -> Vec<ModuleConstraintFailure> {
        self.failures
    }
}

impl fmt::Display for ModuleConstraintViolations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.failures.is_empty() {
            return f.write_str(
                "provider dispatch blocked by an already reported module constraint violation",
            );
        }
        for (index, failure) in self.failures.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{failure}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ModuleConstraintViolations {}

/// Apply-time module constraint gate shared by one execution.
///
/// Every call resolves and evaluates every composition's pending constraints
/// from the bindings supplied to that call. There is deliberately no
/// successful result cache or value fingerprint: a later binding change must
/// always be observed. The small reporting set only prevents a terminal sweep
/// from emitting the exact same already-reported violation a second time; it
/// is never consulted to skip evaluation.
pub struct ModuleConstraintGate {
    compositions: ModuleConstraintCompositions,
    reported_failures: Mutex<HashSet<ModuleConstraintFailure>>,
}

enum ModuleConstraintCompositions {
    Live(Vec<LiveComposition>),
    Saved(Vec<SavedComposition<NoPlaceholder>>),
}

impl ModuleConstraintGate {
    pub fn from_live_compositions(compositions: &[LiveComposition]) -> Self {
        Self {
            compositions: ModuleConstraintCompositions::Live(compositions.to_vec()),
            reported_failures: Mutex::new(HashSet::new()),
        }
    }

    pub fn from_saved_compositions(compositions: &[SavedComposition<NoPlaceholder>]) -> Self {
        Self {
            compositions: ModuleConstraintCompositions::Saved(compositions.to_vec()),
            reported_failures: Mutex::new(HashSet::new()),
        }
    }

    fn evaluate(
        &self,
        bindings: &ResolvedBindings,
        terminal: bool,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Result<Vec<ModuleConstraintFailure>, crate::module_resolver::ModuleConstraintResolutionError>
    {
        match &self.compositions {
            ModuleConstraintCompositions::Live(compositions) => {
                evaluate_live_pending_constraints(compositions, bindings, terminal, session)
            }
            ModuleConstraintCompositions::Saved(compositions) => {
                evaluate_saved_pending_constraints(compositions, bindings, terminal, session)
            }
        }
    }

    /// Re-evaluate every pending constraint whose inputs may now be known.
    ///
    /// This is deliberately global rather than scoped to the provider-bound
    /// resource's module instance. A value can cross a nested module boundary
    /// or leave a module through an attribute before reaching its provider
    /// consumer, so the consumer's `module_source` does not identify every
    /// constraint that governs its inputs.
    ///
    /// One ceiling remains: a cross-argument `require` over X and Y can still
    /// be pending when a consumer of X runs if Y is unknown. It is evaluated
    /// when Y becomes known at a later gate, or rejected by the terminal sweep.
    pub fn check(
        &self,
        bindings: &ResolvedBindings,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Result<(), ModuleConstraintGateError> {
        let failures = self.evaluate(bindings, false, session)?;
        if failures.is_empty() {
            return Ok(());
        }

        let mut reported = self
            .reported_failures
            .lock()
            .expect("module constraint reporting lock poisoned");
        let new_failures = failures
            .into_iter()
            .filter(|failure| reported.insert(failure.clone()))
            .collect();

        // An empty diagnostic still returns Err: reporting suppression must
        // never turn a known violation into permission to call a provider.
        Err(ModuleConstraintViolations {
            failures: new_failures,
        }
        .into())
    }

    /// Evaluate every remaining constraint at the end of apply.
    ///
    /// When `report_unresolved` is true, a constraint whose arguments are
    /// still unresolved is an explicit failure. An execution that already has
    /// failed or skipped effects passes false because those effects may be the
    /// only reason an input was never published. Decidable violations are
    /// evaluated and reported in either mode.
    ///
    /// Failures already emitted by a consuming provider effect are
    /// re-evaluated but omitted from this returned diagnostic only when the
    /// complete rendered failure is unchanged.
    pub fn finish(
        &self,
        bindings: &ResolvedBindings,
        report_unresolved: bool,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) -> Result<(), ModuleConstraintGateError> {
        let failures = self.evaluate(bindings, report_unresolved, session)?;
        self.report_new(failures)
    }

    fn report_new(
        &self,
        failures: Vec<ModuleConstraintFailure>,
    ) -> Result<(), ModuleConstraintGateError> {
        let mut reported = self
            .reported_failures
            .lock()
            .expect("module constraint reporting lock poisoned");
        let failures = failures
            .into_iter()
            .filter(|failure| reported.insert(failure.clone()))
            .collect::<Vec<_>>();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(ModuleConstraintViolations { failures }.into())
        }
    }
}

impl ProviderReadyResource {
    /// Borrow the provider-facing resource payload.
    pub fn as_resource(&self) -> &ProviderResource {
        self.as_inner().as_resource()
    }
}

impl ProviderReadyDataSource {
    /// Borrow the provider-facing data-source payload.
    pub fn as_data_source(&self) -> &ProviderDataSource {
        self.as_inner().as_inner()
    }
}

/// Failure while preparing a provider-facing resource.
#[derive(Debug, Error)]
pub enum ProviderPreparationError {
    #[error(transparent)]
    FinalInput(#[from] crate::validation::admission::final_input::FinalInputError),
    #[error("input discharge failed")] // error-chain: caller-renders-sources
    InputDischarge(#[source] super::EffectFailureKind),
    #[error("bootstrap create is waiting for scheduler dependencies")]
    InitialCreateNotReady,
    /// The complete provider input has no wire representation.
    #[error("{0}")]
    Wire(Box<ProviderWireError>),
    #[error(transparent)]
    TypeSettlement(#[from] crate::validation::deferred_types::ProviderTypeCheckFailure),
    #[error("{resource}: attribute admission failed")] // error-chain: caller-renders-sources
    Admission {
        resource: Box<ResourceId>,
        #[source]
        error: Box<crate::validation::admission::AdmissionErrors>,
    },
    /// An unresolved deferred value survived apply-time reference resolution.
    #[error(transparent)]
    Serialization(#[from] SerializationError),
    /// A known value violates its resource schema or provider validator.
    #[error("{resource}: value constraint failed before provider dispatch: {errors}")]
    ValueConstraint {
        resource: Box<ResourceId>,
        errors: ProviderValueConstraintErrors,
    },
    /// A pending module argument constraint became decidable and failed.
    #[error(transparent)]
    ModuleConstraint(#[from] ModuleConstraintGateError),
    /// Authored lifecycle metadata could not be resolved against the managed
    /// schema before constructing the provider-ready resource.
    #[error(transparent)]
    ResolvedResource(Box<crate::resource::ResolvedResourceError>),
    /// The managed resource has no schema to classify write-only attributes.
    #[error("{resource}: no managed schema is registered")]
    MissingManagedSchema { resource: ResourceId },
    #[error("{resource}: no data-source schema is registered")]
    MissingDataSourceSchema { resource: ResourceId },
    /// An ignored write-only value would have to be resent even though its
    /// current provider value is unobservable.
    #[error(transparent)]
    IgnoredWriteOnly(#[from] crate::provider::IgnoredWriteOnlyValueError),
    #[error(transparent)]
    UnavailableSecret(#[from] crate::resource::UnavailableSecretError),
    /// Provider-specific normalization failed before dispatch.
    #[error("{0}")]
    Provider(crate::provider::HostError),
}

impl From<crate::provider::UpdatePatchError> for ProviderPreparationError {
    fn from(error: crate::provider::UpdatePatchError) -> Self {
        match error {
            crate::provider::UpdatePatchError::IgnoredWriteOnly(error) => {
                Self::IgnoredWriteOnly(error)
            }
            crate::provider::UpdatePatchError::UnavailableSecret(error) => {
                Self::UnavailableSecret(error)
            }
        }
    }
}

impl From<ProviderWireError> for ProviderPreparationError {
    fn from(error: ProviderWireError) -> Self {
        Self::Wire(Box::new(error))
    }
}

impl From<crate::resource::ResolvedResourceError> for ProviderPreparationError {
    fn from(error: crate::resource::ResolvedResourceError) -> Self {
        Self::ResolvedResource(Box::new(error))
    }
}

impl From<crate::provider::HostError> for ProviderPreparationError {
    fn from(error: crate::provider::HostError) -> Self {
        Self::Provider(error)
    }
}

impl From<crate::value::EnumCanonicalizationError> for ProviderPreparationError {
    fn from(error: crate::value::EnumCanonicalizationError) -> Self {
        Self::Provider(error.into())
    }
}

/// Provider-specific configuration validation failure.
///
/// This wrapper retains the provider-specific
/// [`ProviderFactory::validate_config`] diagnostic as typed data.
#[derive(Debug, Error)]
#[error("{error}")]
pub struct ProviderConfigValidationError {
    error: crate::provider::HostError,
}

impl ProviderConfigValidationError {
    pub(crate) fn deferred(
        diagnostics: Vec<crate::validation::deferred_types::ProviderTypeCheckDiagnostic>,
    ) -> Self {
        Self::new(crate::provider::HostError::invalid_input_caused_by(
            "provider configuration type checks remain unsettled",
            crate::provider_text::HostDiagnostic::capture_host_or_provider(PreparationDiagnostics(
                diagnostics,
            )),
        ))
    }

    pub(crate) fn new(error: crate::provider::HostError) -> Self {
        Self { error }
    }

    pub fn provider_error(&self) -> &crate::provider::HostError {
        &self.error
    }
}

/// Failure while preparing configuration for provider construction.
#[derive(Debug, Error)]
pub enum ProviderConfigPreparationError {
    #[error("{0}")]
    UnknownAttributes(
        crate::validation::admission::configuration::provider::UnknownProviderConfigAttributes,
    ),
    #[error("{0}")]
    Multiple(ProviderConfigPreparationErrors),
    /// Provider semantic validation and initialization require wire values.
    #[error("{0}")]
    Wire(ProviderWireError),
    /// A concrete attribute violates its host-side type schema.
    #[error("provider {provider}: {attribute}: {error}")]
    InvalidAttribute {
        provider: String,
        attribute: crate::parser::AttributeKey,
        error: ProviderValueConstraintErrors,
    },
    /// The provider rejected cross-attribute or provider-specific semantics.
    #[error("provider {provider}: {error}")]
    ProviderValidation {
        provider: String,
        error: ProviderConfigValidationError,
    },
}

/// A terminal collection; individual typed diagnostics remain inspectable.
#[derive(Debug)]
pub struct ProviderConfigPreparationErrors {
    errors: Vec<ProviderConfigPreparationError>,
}

impl fmt::Display for ProviderConfigPreparationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, diagnostic) in self
            .errors
            .iter()
            .flat_map(|error| error.diagnostics())
            .enumerate()
        {
            if index > 0 {
                f.write_str("\n")?;
            }
            fmt::Display::fmt(&diagnostic, f)?;
        }
        Ok(())
    }
}

/// A configuration owner's diagnostic, retaining its exact authored key.
#[derive(Debug, Clone, Copy)]
pub enum ProviderConfigDiagnostic<'a> {
    Preparation(&'a ProviderConfigPreparationError),
    Attribute {
        provider: &'a str,
        attribute: &'a crate::parser::AttributeKey,
        error: &'a TypeError,
    },
}

impl ProviderConfigDiagnostic<'_> {
    pub fn attribute(&self) -> Option<&crate::parser::AttributeKey> {
        match self {
            Self::Preparation(error) => error.attribute(),
            Self::Attribute { attribute, .. } => Some(attribute),
        }
    }
    pub fn schema_error(&self) -> Option<&TypeError> {
        match self {
            Self::Preparation(_) => None,
            Self::Attribute { error, .. } => Some(error),
        }
    }
}

impl fmt::Display for ProviderConfigDiagnostic<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preparation(error) => fmt::Display::fmt(error, f),
            Self::Attribute {
                provider,
                attribute,
                error,
            } => {
                write!(f, "provider {provider}: {attribute}: {error}")
            }
        }
    }
}

impl From<ProviderWireError> for ProviderConfigPreparationError {
    fn from(error: ProviderWireError) -> Self {
        Self::Wire(error)
    }
}

impl From<ProviderConfigPreparationError> for crate::provider::HostError {
    fn from(error: ProviderConfigPreparationError) -> Self {
        Self::internal_caused_by(
            "Failed to prepare provider configuration",
            crate::provider_text::HostDiagnostic::capture_host_or_provider(error),
        )
    }
}

impl ProviderConfigPreparationError {
    pub fn into_diagnostics(self) -> Vec<Self> {
        match self {
            Self::UnknownAttributes(errors) => errors.into_diagnostics(),
            Self::Multiple(errors) => errors
                .errors
                .into_iter()
                .flat_map(Self::into_diagnostics)
                .collect(),
            error @ (Self::Wire(_)
            | Self::InvalidAttribute { .. }
            | Self::ProviderValidation { .. }) => vec![error],
        }
    }

    pub(crate) fn collect(mut errors: Vec<Self>) -> Result<(), Self> {
        match errors.len() {
            0 => Ok(()),
            1 => Err(errors.pop().expect("one configuration error")),
            _ => Err(Self::Multiple(ProviderConfigPreparationErrors { errors })),
        }
    }

    pub fn diagnostics(&self) -> Vec<ProviderConfigDiagnostic<'_>> {
        match self {
            Self::UnknownAttributes(errors) => errors.diagnostics().collect(),
            Self::Multiple(errors) => errors.errors.iter().flat_map(Self::diagnostics).collect(),
            Self::Wire(_) | Self::InvalidAttribute { .. } | Self::ProviderValidation { .. } => {
                vec![ProviderConfigDiagnostic::Preparation(self)]
            }
        }
    }

    /// Attribute responsible for a host-side type/canonicalization failure.
    pub fn attribute(&self) -> Option<&crate::parser::AttributeKey> {
        match self {
            Self::Wire(error) => Some(error.attribute()),
            Self::InvalidAttribute { attribute, .. } => Some(attribute),
            Self::ProviderValidation { .. } | Self::UnknownAttributes(_) | Self::Multiple(_) => {
                None
            }
        }
    }

    /// Typed provider failure when provider-specific validation was reached.
    pub fn provider_error(&self) -> Option<&crate::provider::HostError> {
        match self {
            Self::Wire(_) | Self::UnknownAttributes(_) | Self::Multiple(_) => None,
            Self::InvalidAttribute { .. } => None,
            Self::ProviderValidation { error, .. } => Some(error.provider_error()),
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub use crate::validation::admission::configuration::provider::prepare as prepare_provider_ready_config;
pub use crate::validation::admission::configuration::provider::{
    analysis as validate_and_canonicalize_provider_config, validate_parser_provider_config,
};

/// Typed collection of schema failures from one provider-boundary check.
///
/// `Display` intentionally retains the historical `; `-joined text. This is a
/// composite leaf: exposing the first item through `Error::source` would make
/// a chain renderer repeat it while still omitting the remaining items. All
/// structured errors remain available through [`Self::errors`].
#[derive(Debug)]
pub struct ProviderValueConstraintErrors(Box<crate::schema::ResourceValueValidation>);

impl ProviderValueConstraintErrors {
    pub fn errors(&self) -> &[TypeError] {
        self.0.error_slice()
    }

    fn from_admission(report: crate::schema::ResourceValueValidation) -> Self {
        Self(Box::new(report))
    }
}

impl From<Vec<TypeError>> for ProviderValueConstraintErrors {
    fn from(errors: Vec<TypeError>) -> Self {
        Self(Box::new(
            crate::schema::ResourceValueValidation::from_errors(errors),
        ))
    }
}

impl fmt::Display for ProviderValueConstraintErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.0.errors().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            self.0.diagnostic_error(error).fmt(f)?;
        }
        Ok(())
    }
}

impl std::error::Error for ProviderValueConstraintErrors {}

#[cfg(test)]
#[test]
fn review_r10_constraint_display_uses_the_structured_errors() {
    let mut errors = ProviderValueConstraintErrors::from(vec![TypeError::MissingRequired {
        name: "old".into(),
    }]);
    *errors.0 =
        crate::schema::ResourceValueValidation::from_errors(vec![TypeError::MissingRequired {
            name: "current".into(),
        }]);
    assert_eq!(errors.to_string(), errors.errors()[0].to_string());
}

/// Validate and prepare a resource for provider dispatch.
///
/// This is the sole constructor for [`ProviderReadyResource`]. It preserves
/// secret annotations and validates every newly known schema value
/// (including provider custom types), re-runs the complete desired-side
/// normalization pipeline, and finally proves no provider-ineligible deferred value remains.
pub async fn prepare_provider_ready_resource<S: Clone, D: Clone + Default>(
    resource: Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    Box::pin(prepare_provider_ready_resource_with_value_check(
        ResolvedCheckInput::authored(resource),
        context,
    ))
    .await
}

/// Apply-path preparation when a pre-resolution source snapshot identifies
/// exactly which attributes became known during this effect.
///
/// The [`ResolvedCheckInput`] retains authored values for every other
/// attribute and the resolver-derived set that became known during this
/// effect. This prevents provider normalization performed during planning
/// from being mistaken for user input while still rerunning cross-attribute
/// validators when at least one input became known now.
pub(super) async fn prepare_provider_ready_resource_after_resolution<
    S: Clone,
    D: Clone + Default,
>(
    input: ResolvedCheckInput<
        Resource<S, crate::resource::NoPlaceholder, crate::resource::AbsoluteBindingAddress, D>,
    >,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    // Retain authored values and normalization state in the preparation frame,
    // rather than embedding it in every dispatch caller.
    Box::pin(prepare_provider_ready_resource_with_value_check(
        input, context,
    ))
    .await
}

enum ResourceValueCheck {
    All,
    ApplyResolved {
        attributes: HashMap<String, Value>,
        names: HashSet<String>,
    },
}

async fn prepare_provider_ready_resource_with_value_check<S: Clone, D: Clone + Default>(
    input: ResolvedCheckInput<
        Resource<S, crate::resource::NoPlaceholder, crate::resource::AbsoluteBindingAddress, D>,
    >,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyResource, ProviderPreparationError> {
    let ResolvedCheckInput {
        mut value,
        check: value_check,
    } = input;
    let resource = &mut value;
    context
        .module_gate
        .check(context.bindings, context.type_checks)?;
    let schema = context.schemas.get_for(resource).ok_or_else(|| {
        ProviderPreparationError::MissingManagedSchema {
            resource: resource.id.clone(),
        }
    });
    if schema.is_err() {
        crate::resource::assert_resource_known_for_apply(
            resource,
            &crate::resource::knowledge::KnowledgeContext::apply(context.bindings),
        )?;
    }
    let schema = schema?;
    {
        let authored = match &value_check {
            ResourceValueCheck::All => None,
            ResourceValueCheck::ApplyResolved { attributes, names } => Some((attributes, names)),
        };
        validate_known_provider_values(
            &resource.id,
            crate::validation::admission::attributes::admit_apply_resource(
                resource,
                authored,
                schema,
                context.bindings,
                context.type_checks,
            ),
        )?
    };

    let alias_conflicts = crate::schema::resolve_resource_block_names_in_session(
        &mut value,
        context.schemas,
        context.type_checks,
    );
    if !alias_conflicts.is_empty() {
        return Err(ProviderPreparationError::Provider(
            crate::provider::HostError::invalid_input(
                alias_conflicts
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        ));
    }
    let (normalized, restored) = apply_dispatch_desired_normalization(
        value,
        context.provider_configs,
        context.normalizer,
        context.schemas,
    )
    .await?;
    let resource = normalized.into_resource();
    super::normalized::reject_provider_boundary_block_aliases_with_authority(
        std::slice::from_ref(&resource),
        context.schemas,
        &crate::input_rules::InputSelectionAuthority::in_catalog(context.type_checks.catalog()),
    )
    .map_err(ProviderPreparationError::Provider)?;
    crate::resource::assert_resource_known_for_apply(
        &resource,
        &crate::resource::knowledge::KnowledgeContext::apply(context.bindings),
    )?;
    let resource =
        ResolvedResource::resolve_with_schemas(resource.into_provider_resource(), context.schemas)?;
    // The final projection uses the bound protected source for diagnostics.
    let input = DispatchPayload::resource(resource, &restored, context.schemas)?;
    let resource = input.payload();
    let directives = context.schemas.resolve_directives(
        &resource.as_resource().id,
        SchemaKind::Resource,
        &resource.as_resource().directives,
    );
    let defaults = resource.as_resource().default_tag_keys.clone();
    let admitted = admit_dispatch(
        input,
        Some(&defaults),
        schema,
        context.bindings,
        context.type_checks,
    )?;
    ProviderReady::settled(
        context.type_checks.pending((admitted, directives)),
        crate::input_rules::InputSelectionAuthority::in_catalog(context.type_checks.catalog()),
    )
}

/// Validate and prepare a data source for provider dispatch.
///
/// This authored-input entry point runs every value constraint before schema
/// canonicalization, matching refresh and plan-time behavior. Apply execution
/// of a plan-normalized data source uses the private resolver-produced companion
/// path below, which checks only attributes that became known at apply. Both
/// paths produce the same sealed [`ProviderReadyDataSource`] witness before the
/// provider can be called.
pub fn prepare_provider_ready_data_source(
    resource: DataSource,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyDataSource, ProviderPreparationError> {
    prepare_provider_ready_data_source_after_resolution(
        ResolvedCheckInput::authored(resource),
        context,
    )
}

/// Apply-path data-source preparation for a value whose resolver has retained
/// authored inputs and derived which attributes became known during apply.
/// Plan-normalized literal attributes are deliberately outside that check
/// scope because they already passed the plan-time authored-value gate.
pub(super) fn prepare_provider_ready_data_source_after_resolution<S, D>(
    input: ResolvedCheckInput<
        DataSource<S, crate::resource::NoPlaceholder, crate::resource::AbsoluteBindingAddress, D>,
    >,
    context: &ProviderPreparationContext<'_>,
) -> Result<ProviderReadyDataSource, ProviderPreparationError> {
    let ResolvedCheckInput {
        mut value,
        check: value_check,
    } = input;
    let resource = &mut value;
    context
        .module_gate
        .check(context.bindings, context.type_checks)?;
    let schema = context
        .schemas
        .get_for_data_source(resource)
        .ok_or_else(|| ProviderPreparationError::MissingDataSourceSchema {
            resource: resource.id.clone(),
        });
    if schema.is_err() {
        for value in resource.attributes.values() {
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::apply(
                    context.bindings,
                ))
                .map_err(crate::resource::knowledge_serialization_error)
                .and_then(crate::resource::require_known_for_apply)?;
        }
    }
    let schema = schema?;
    {
        let authored = match &value_check {
            ResourceValueCheck::All => None,
            ResourceValueCheck::ApplyResolved { attributes, names } => Some((attributes, names)),
        };
        validate_known_provider_values(
            &resource.id,
            crate::validation::admission::attributes::admit_apply_data_source(
                resource,
                authored,
                schema,
                context.bindings,
                context.type_checks,
            ),
        )?
    };

    crate::value::canonicalize_data_sources_with_schemas(
        std::slice::from_mut(&mut value),
        context.schemas,
    )?;
    for value in value.attributes.values() {
        (value)
            .knowledge(&crate::resource::knowledge::KnowledgeContext::apply(
                context.bindings,
            ))
            .map_err(crate::resource::knowledge_serialization_error)
            .and_then(crate::resource::require_known_for_apply)?;
    }
    let value = ResolvedDataSource::new(value.into_provider_data_source());
    let input = DispatchPayload::data_source(value);
    let value = input.payload();
    let directives = context.schemas.resolve_directives(
        &value.as_inner().id,
        SchemaKind::DataSource,
        &value.as_inner().directives,
    );
    let admitted = admit_dispatch(input, None, schema, context.bindings, context.type_checks)?;
    ProviderReady::settled(
        context.type_checks.pending((admitted, directives)),
        crate::input_rules::InputSelectionAuthority::in_catalog(context.type_checks.catalog()),
    )
}

/// Prepare a checked create request through the provider-boundary gate.
pub async fn prepare_create_request(
    resource: Resource,
    context: &ProviderPreparationContext<'_>,
) -> Result<CreateRequest, ProviderPreparationError> {
    let resource = prepare_provider_ready_resource(resource, context).await?;
    Ok(CreateRequest::checked(resource))
}

/// Look up the schema required to construct an update request.
pub(super) fn managed_update_schema<'a, S, D>(
    schemas: &'a SchemaRegistry,
    resource: &Resource<
        S,
        crate::resource::NoPlaceholder,
        crate::resource::AbsoluteBindingAddress,
        D,
    >,
) -> Result<&'a ResourceSchema, ProviderPreparationError> {
    schemas
        .get_for(resource)
        .map(|schema| schema.schema().as_schema())
        .ok_or_else(|| ProviderPreparationError::MissingManagedSchema {
            resource: resource.id.clone(),
        })
}

/// Prepare a checked update request through the provider-boundary gate.
pub async fn prepare_update_request(
    resource: Resource,
    from: crate::resource::SnapshotState,
    changed_attributes: &[String],
    context: &ProviderPreparationContext<'_>,
) -> Result<UpdateRequest, ProviderPreparationError> {
    let resource = prepare_provider_ready_resource(resource, context).await?;
    UpdateRequest::checked(from, changed_attributes, &resource, context)
}

fn validate_known_provider_values(
    id: &ResourceId,
    result: Result<(), crate::validation::admission::attributes::ApplyAdmissionErrors>,
) -> Result<(), ProviderPreparationError> {
    use crate::validation::admission::attributes::ApplyAdmissionErrors;
    result.map_err(|error| match error {
        ApplyAdmissionErrors::Admission(error) => {
            // Placement has already been refused by admission. Retain the
            // established boundary error projection for unresolved inputs.
            if let crate::resolver::ResolutionError::Knowledge(knowledge) = error.resolution_error()
                && let crate::resource::knowledge::KnowledgeDiagnostic::Absolute(knowledge) =
                    knowledge.as_ref()
            {
                return ProviderPreparationError::Serialization(
                    crate::resource::knowledge_serialization_error(knowledge.clone()),
                );
            }
            ProviderPreparationError::Admission {
                resource: Box::new(id.clone()),
                error: Box::new(error),
            }
        }
        ApplyAdmissionErrors::Constraints(report) => ProviderPreparationError::ValueConstraint {
            resource: Box::new(id.clone()),
            errors: ProviderValueConstraintErrors::from_admission(report),
        },
    })
}

#[cfg(test)]
#[path = "provider_ready/compact_list_configuration_tests.rs"]
mod compact_list_configuration_tests;

#[cfg(test)]
mod provider_config_tests {
    use crate::schema::{PatternAttributeSchema, PatternAttributeType, PatternResourceSchema};
    use std::sync::Arc;

    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::future::BoxFuture;
    use indexmap::IndexMap;

    use super::*;
    use crate::parser::ResolvedProviderConfigs;
    use crate::provider::{ProviderError, ProviderResult};
    use crate::resource::{
        ConcreteValue, DeferredValue, ParsedIgnorePath, ParsedIgnorePathSegment,
    };
    use crate::schema::{
        CompiledAttributeType as AttributeType, RootlessType, TypeIdentity, ValidatedConfigSchema,
        legacy_validator,
    };

    fn assert_final_wire_shape<T: ResourceWirePayload>(
        input: DispatchPayload<T>,
        schema: crate::schema::OwnerBoundResourceSchema<'_>,
        session: &dyn crate::validation::deferred_types::ProviderTypeCheckContext,
    ) {
        let result = admit_dispatch(input, None, schema, &ResolvedBindings::default(), session);
        let error = result.expect_err("out-of-range duration must fail wire projection");
        assert!(
            matches!(error, ProviderPreparationError::Wire(_)),
            "{error:?}"
        );
    }

    #[test]
    fn issue_4304_3a_review3_final_create_wire_failure_has_transport_shape() {
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                let resource: Resource =
                    Resource::with_provider("test", "review3", "wire-create", None).with_attribute(
                        "oversized",
                        Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(
                            u64::MAX,
                        ))),
                    );
                let schemas = crate::test_utils::preparation_schemas_with_attributes(
                    &resource.id,
                    SchemaKind::Resource,
                    &resource.attributes,
                );
                let schema = schemas.get_for(&resource).unwrap();
                let diagnostic_attributes = resource.attributes.clone();
                let payload = ResolvedResource::resolve_with_schemas(
                    resource.into_provider_resource(),
                    &schemas,
                )
                .unwrap();
                assert_final_wire_shape(
                    DispatchPayload {
                        payload,
                        diagnostic_attributes,
                    },
                    schema,
                    session,
                );
            },
        );
    }

    #[test]
    fn issue_4304_3a_review3_final_data_source_wire_failure_has_transport_shape() {
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                let mut resource: DataSource =
                    DataSource::pending_with_provider("test", "review3", None);
                resource.id =
                    ResourceId::with_provider_identity("test", "review3", "wire-ds", None);
                resource.attributes.insert(
                    "oversized".into(),
                    Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(
                        u64::MAX,
                    ))),
                );
                let schemas = crate::test_utils::preparation_schemas_with_attributes(
                    &resource.id,
                    SchemaKind::DataSource,
                    &resource.attributes,
                );
                let schema = schemas.get_for_data_source(&resource).unwrap();
                let input = DispatchPayload::data_source(ResolvedDataSource::new(
                    resource.into_provider_data_source(),
                ));
                assert_final_wire_shape(input, schema, session);
            },
        );
    }

    #[tokio::test]
    async fn issue_4304_3a_review3_create_projects_the_final_payload_once() {
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |session| {
                let resource: Resource =
                    Resource::with_provider("test", "review3", "project-create", None)
                        .with_attribute("value", Value::Concrete(ConcreteValue::Bool(true)));
                let id = resource.id.clone();
                let before = crate::provider_wire::projection_count_for_test(&id);
                let schemas = crate::test_utils::preparation_schemas_with_attributes(
                    &id,
                    SchemaKind::Resource,
                    &resource.attributes,
                );
                let bindings = ResolvedBindings::default();
                let gate = ModuleConstraintGate::from_live_compositions(&[]);
                let context = ProviderPreparationContext::for_test(
                    &bindings,
                    &gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &schemas,
                    session,
                );
                prepare_create_request(resource, &context).await.unwrap();
                assert_eq!(
                    crate::provider_wire::projection_count_for_test(&id) - before,
                    1
                );
            },
        )
        .await;
    }

    #[test]
    fn issue_4304_3a_review3_data_source_projects_the_final_payload_once() {
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                let mut resource: DataSource =
                    DataSource::pending_with_provider("test", "project-ds", None);
                resource.id =
                    ResourceId::with_provider_identity("test", "project-ds", "project-ds", None);
                resource
                    .attributes
                    .insert("value".into(), Value::Concrete(ConcreteValue::Bool(true)));
                let id = resource.id.clone();
                let before = crate::provider_wire::projection_count_for_test(&id);
                let schemas = crate::test_utils::preparation_schemas_with_attributes(
                    &id,
                    SchemaKind::DataSource,
                    &resource.attributes,
                );
                let bindings = ResolvedBindings::default();
                let gate = ModuleConstraintGate::from_live_compositions(&[]);
                let context = ProviderPreparationContext::for_test(
                    &bindings,
                    &gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &schemas,
                    session,
                );
                prepare_provider_ready_data_source(resource, &context).unwrap();
                assert_eq!(
                    crate::provider_wire::projection_count_for_test(&id) - before,
                    1
                );
            },
        );
    }

    #[tokio::test]
    async fn issue_4304_3a_review3_single_projection_preserves_secret_wire_diagnostics() {
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |session| {
                let resource = Resource::with_provider("test", "review3", "secret-wire", None)
                    .with_attribute(
                        "config",
                        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                            "public".into(),
                            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                                "private-wire-key".into(),
                                Value::Concrete(ConcreteValue::Duration(
                                    std::time::Duration::from_secs(u64::MAX),
                                )),
                            )])))
                            .into_secret(),
                        )]))),
                    );
                let schemas = crate::test_utils::preparation_schemas_with_attributes(
                    &resource.id,
                    SchemaKind::Resource,
                    &resource.attributes,
                );
                let bindings = ResolvedBindings::default();
                let gate = ModuleConstraintGate::from_live_compositions(&[]);
                let context = ProviderPreparationContext::for_test(
                    &bindings,
                    &gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &schemas,
                    session,
                );
                let error = prepare_create_request(resource, &context)
                    .await
                    .unwrap_err();
                let ProviderPreparationError::Wire(error) = error else {
                    panic!("{error}")
                };
                assert_eq!(
                    error.path(),
                    &[
                        crate::protocol_convert::WirePathSegment::Key("public".into()),
                        crate::protocol_convert::WirePathSegment::Secret,
                    ]
                );
                assert!(matches!(
                    error.conversion_error(),
                    crate::protocol_convert::ProtocolConvertError::DurationOutOfRange
                ));
                let message = format!("{error:?}: {error}");
                assert!(message.contains("config.public"), "{message}");
                for private in ["private-wire-key", &u64::MAX.to_string()] {
                    assert!(!message.contains(private), "{message}");
                }
            },
        )
        .await;
    }

    #[tokio::test]
    async fn secret_origin_4275_round2_dispatch_masks_still_deferred_secret() {
        let resource = Resource::with_provider("test", "widget", "example", None).with_attribute(
            "password",
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::root_resource_ref("producer", "private_attr", vec![])).into(),
            )),
        );
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = SchemaRegistry::new();
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |type_checks| {
                let context = ProviderPreparationContext::for_test(
                    &bindings,
                    &module_gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &schemas,
                    type_checks,
                );
                let error = prepare_create_request(resource, &context)
                    .await
                    .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains(crate::resource::INSIDE_SECRET_MESSAGE)
                );
                for rendered in [error.to_string(), format!("{error:?}")] {
                    assert!(!rendered.contains("producer"), "{rendered}");
                    assert!(!rendered.contains("private_attr"), "{rendered}");
                }
            },
        )
        .await;
    }

    #[tokio::test]
    async fn prepare_create_request_resolves_authored_ignore_changes() {
        let mut resource = Resource::with_provider("test", "widget", "example", None)
            .with_attribute(
                "comment",
                Value::Concrete(ConcreteValue::String("managed-elsewhere".into())),
            );
        resource.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
            ParsedIgnorePathSegment::Attribute("comment".to_string()),
        ])];
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            PatternResourceSchema::new("widget")
                .attribute(
                    PatternAttributeSchema::optional("comment", PatternAttributeType::string())
                        .build(),
                )
                .default_prevent_destroy()
                .validate()
                .expect("valid test schema"),
        );
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |type_checks| {
                let context = ProviderPreparationContext::for_test(
                    &bindings,
                    &module_gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &schemas,
                    type_checks,
                );

                let request = prepare_create_request(resource, &context)
                    .await
                    .expect("provider preparation must use schema resolution");

                assert_eq!(request.resource().ignore_changes().iter().len(), 1);
                assert!(request.directives().prevent_destroy());
            },
        )
        .await
    }

    #[tokio::test]
    async fn prepare_update_request_resolves_schema_default_directives() {
        let resource = Resource::with_provider("test", "widget", "example", None);
        let from = crate::resource::SnapshotState::existing(resource.id.clone(), HashMap::new());
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "test",
            PatternResourceSchema::new("widget")
                .default_prevent_destroy()
                .validate()
                .expect("valid test schema"),
        );
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |type_checks| {
                let context = ProviderPreparationContext::for_test(
                    &bindings,
                    &module_gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &schemas,
                    type_checks,
                );

                let request = prepare_update_request(resource, from, &[], &context)
                    .await
                    .expect("provider preparation must resolve directives against the schema");

                assert!(request.directives().prevent_destroy());
            },
        )
        .await
    }

    #[tokio::test]
    async fn prepare_update_request_rejects_a_missing_managed_schema() {
        let resource = Resource::with_provider("test", "widget", "example", None);
        let id = resource.id.clone();
        let from = crate::resource::SnapshotState::existing(id.clone(), HashMap::new());
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = SchemaRegistry::new();
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |type_checks| {
                let context = ProviderPreparationContext::for_test(
                    &bindings,
                    &module_gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &schemas,
                    type_checks,
                );

                let error = prepare_update_request(resource, from, &[], &context)
                    .await
                    .expect_err("updates without a managed schema must fail preparation");

                assert!(matches!(
                    &error,
                    ProviderPreparationError::MissingManagedSchema { resource } if resource == &id
                ));
                let rendered = error.to_string();
                assert!(rendered.contains(&id.to_string()), "{rendered}");
                assert!(
                    rendered.contains("no managed schema is registered"),
                    "{rendered}"
                );
            },
        )
        .await
    }

    struct EnumConfigFactory;

    impl ProviderFactory for EnumConfigFactory {
        fn name(&self) -> &str {
            "test"
        }

        fn display_name(&self) -> &str {
            "enum config test provider"
        }

        fn validate_config(
            &self,
            attributes: &CanonicalProviderConfigAttributes,
            _session: &crate::provider::ProviderValidationInputs,
        ) -> crate::provider::ProviderResult<()> {
            match attributes.get("region") {
                Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                    if value.api_value() == "ap-northeast-1" =>
                {
                    Ok(())
                }
                other => Err(ProviderError::invalid_input(format!(
                    "expected canonical region, got {other:?}"
                ))),
            }
        }

        fn create_provider(
            &self,
            _config: &crate::provider::ProviderSession,
            _initialize_outcome: crate::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>> {
            unreachable!("config preparation tests do not create providers")
        }

        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
                    "region".to_string(),
                    PatternAttributeType::enum_(
                        crate::schema::TypeIdentity::new(
                            Some("test"),
                            Vec::<String>::new(),
                            "Region",
                        ),
                        Some(vec!["ap-northeast-1".to_string()]),
                        vec![("ap-northeast-1".to_string(), "ap_northeast_1".to_string())],
                        None,
                        None,
                    ),
                )]))
                .expect("test provider config schema must be valid"),
                Default::default(),
                Vec::new(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            Vec::new()
        }
    }

    struct ConfigFactory {
        validate_calls: AtomicUsize,
        plugin_error: Option<ConfigFailure>,
    }

    #[derive(Clone, Copy)]
    enum ConfigFailure {
        Invalid(&'static str),
        Timeout(&'static str),
    }

    fn admit_declaration(declaration: crate::schema::AttributeType) -> AttributeType {
        crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
            "value".to_string(),
            declaration,
        )]))
        .unwrap()
        .parts()["value"]
            .clone()
    }

    struct RoutingFactory {
        name: &'static str,
        declaration: crate::schema::AttributeType,
        sibling_declaration: Option<crate::schema::AttributeType>,
        accepted: Option<&'static str>,
        validate_calls: Arc<AtomicUsize>,
        seen_values: Option<Arc<Mutex<Vec<String>>>>,
    }

    fn assert_round5_annotation_error(
        declaration: crate::schema::AttributeType,
        identity: TypeIdentity,
        kind: crate::schema::RefinedScalarKind,
        value: Value,
        expected: TypeError,
    ) {
        use crate::parser::{
            CustomTypeValidationError, CustomTypeValidationTarget, OwnedTypeIdentity,
        };

        let calls = Arc::new(AtomicUsize::new(0));
        let factories: Arc<Vec<Box<dyn ProviderFactory>>> =
            Arc::new(vec![Box::new(RoutingFactory {
                name: "owner",
                declaration,
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::clone(&calls),
                seen_values: None,
            })]);
        let snapshot = crate::provider::loaded_provider_snapshot_from_factories(
            Arc::clone(&factories),
            SchemaRegistry::new(),
            crate::EnvironmentSnapshot::empty(),
        );
        let owned = OwnedTypeIdentity::provider("owner", identity.clone(), kind);
        let resolver = test_schema_path_resolver(&factories);
        for result in [
            crate::parser::validate_custom_type_value(
                &resolver,
                CustomTypeValidationTarget::Annotation(&owned),
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap()
                    .into_known()
                    .unwrap(),
            ),
            crate::parser::validate_schema_custom_type(
                &owned,
                &value
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap()
                    .into_known()
                    .unwrap(),
                snapshot.provider_context(),
            ),
        ] {
            assert_eq!(
                result,
                Err(CustomTypeValidationError::InvalidDeclarationValue {
                    owner: "owner".into(),
                    identity: identity.clone(),
                    error: Box::new(expected.clone()),
                })
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    fn assert_round5_enum_annotation_error(input: &str) {
        let identity = TypeIdentity::from_dotted("shared.Mode");
        assert_round5_annotation_error(
            PatternAttributeType::enum_(
                identity.clone(),
                Some(vec!["COMMON".into()]),
                Vec::new(),
                None,
                None,
            ),
            identity.clone(),
            crate::schema::RefinedScalarKind::String,
            Value::Concrete(ConcreteValue::String(input.into())),
            TypeError::StringLiteralExpectedEnum {
                user_typed: input.into(),
                attribute: None,
                type_name: "Mode".into(),
                expected: vec![crate::schema::ExpectedEnumVariant::from_identity(
                    &identity, "COMMON", false,
                )],
                extra_message: None,
            },
        );
    }

    #[test]
    fn issue_4276_round5_annotation_rejects_matching_enum_string() {
        assert_round5_enum_annotation_error("COMMON");
    }

    #[test]
    fn issue_4276_round5_annotation_rejects_unknown_enum_string() {
        assert_round5_enum_annotation_error("bogus");
    }

    #[test]
    fn issue_4276_round5_numeric_annotation_rejects_enum_identifier() {
        let identity = TypeIdentity::from_dotted("shared.Count");
        for (declaration, kind) in [
            (
                PatternAttributeType::refined_int(Some(identity.clone()), None),
                crate::schema::RefinedScalarKind::Int,
            ),
            (
                PatternAttributeType::refined_float(Some(identity.clone()), None),
                crate::schema::RefinedScalarKind::Float,
            ),
        ] {
            assert_round5_annotation_error(
                declaration,
                identity.clone(),
                kind,
                Value::Concrete(ConcreteValue::enum_identifier("5")),
                TypeError::value_type_mismatch("shared.Count".into(), "EnumIdentifier".into()),
            );
        }
    }

    #[tokio::test]
    async fn issue_4276_round5_apply_resolved_state_string_list_skips_provider_validation() {
        let stored = crate::protocol_convert::protocol_to_core_persisted_value(
            &carina_provider_protocol::types::PersistedValue::StringList(vec!["bad".into()]),
        )
        .unwrap();
        let value = stored.to_value_at("producer.value").unwrap();
        assert!(matches!(
            value,
            Value::Concrete(ConcreteValue::StringList(_))
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(RoutingFactory {
            name: "owner",
            declaration: PatternAttributeType::list(PatternAttributeType::refined_string(
                Some(TypeIdentity::bare("VpcId")),
                None,
                None,
                None,
            )),
            sibling_declaration: None,
            accepted: Some("vpc-good"),
            validate_calls: Arc::clone(&calls),
            seen_values: None,
        })];
        let schemas = test_schema_registry(&factories);
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |type_checks| {
                let context = ProviderPreparationContext::for_test(
                    &bindings,
                    &module_gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &schemas,
                    type_checks,
                );
                let resource = Resource::with_provider("owner", "test.owner", "example", None)
                    .with_attribute("value", value.clone());
                let input = ResolvedCheckInput::plan_normalized(
                    resource,
                    HashMap::from([("value".into(), value.clone())]),
                    HashSet::from(["value".into()]),
                );
                let ready = prepare_provider_ready_resource_after_resolution(input, &context)
                    .await
                    .expect("state StringList items receive only local validation at apply");
                assert_eq!(
                    ready.as_resource().resolved_attributes().get("value"),
                    Some(&value)
                );
                assert_eq!(calls.load(Ordering::SeqCst), 0);
            },
        )
        .await;
    }

    #[tokio::test]
    async fn issue_4276_round2_d_provider_normalized_string_list_is_revalidated() {
        struct DefaultingNormalizer;
        impl ProviderNormalizer for DefaultingNormalizer {
            fn normalize_desired<'a>(
                &'a self,
                resources: &'a mut [ProviderResource],
            ) -> BoxFuture<'a, ProviderResult<()>> {
                Box::pin(async move {
                    resources[0].set_attr(
                        "value",
                        Value::Concrete(ConcreteValue::StringList(vec!["bad".into()])),
                    );
                    Ok(())
                })
            }
            fn normalize_state<'a>(
                &'a self,
                _: &'a mut crate::provider::StateTransformMap,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                crate::provider::ready_noop()
            }
            fn hydrate_read_state<'a>(
                &'a self,
                _: &'a mut crate::provider::StateTransformMap,
                _: &'a crate::provider::SavedAttrs,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                crate::provider::ready_noop()
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let factories: Arc<Vec<Box<dyn ProviderFactory>>> =
            Arc::new(vec![Box::new(RoutingFactory {
                name: "owner",
                declaration: PatternAttributeType::list(PatternAttributeType::refined_string(
                    Some(TypeIdentity::bare("VpcId")),
                    None,
                    None,
                    None,
                )),
                sibling_declaration: None,
                accepted: Some("vpc-good"),
                validate_calls: Arc::clone(&calls),
                seen_values: None,
            })]);
        let snapshot = crate::provider::loaded_provider_snapshot_from_factories(
            Arc::clone(&factories),
            SchemaRegistry::new(),
            crate::EnvironmentSnapshot::empty(),
        );
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        crate::validation::deferred_types::with_session_for_test_async(
            snapshot.provider_context(),
            async |type_checks| {
                let normalizer_boundary =
                    crate::provider::ConfiguredNormalizer::fixture(&DefaultingNormalizer);
                let context = ProviderPreparationContext::for_test(
                    &bindings,
                    &module_gate,
                    &[],
                    &normalizer_boundary,
                    snapshot.schemas().schemas(),
                    type_checks,
                );
                let resource = Resource::with_provider("owner", "test.owner", "example", None);
                let error = prepare_provider_ready_resource(resource, &context)
                    .await.expect_err("normalizer output must satisfy the full declaration");
                let ProviderPreparationError::FinalInput(
                    crate::validation::admission::final_input::FinalInputError::Membership { path, origin, .. }
                ) = error else { panic!("expected final-input membership failure: {error:?}"); };
                assert!(path.to_string().starts_with("value"));
                assert!(matches!(origin, crate::validation::admission::final_input::FinalInputOrigin::ProviderNormalization));
                assert_eq!(calls.load(Ordering::SeqCst), 1);

                let authored = Resource::with_provider("owner", "test.owner", "example", None)
                    .with_attribute(
                        "value",
                        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                            ConcreteValue::String("bad".into()),
                        )])),
                    );
                let error = prepare_provider_ready_resource(authored, &context)
                    .await
                    .unwrap_err();
                let ProviderPreparationError::ValueConstraint { errors, .. } = error else {
                    panic!("expected authored value constraint, got {error:?}")
                };
                assert_eq!(
                    errors
                        .errors()
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                    vec!["owner rejected `VpcId`: owner accepts `vpc-good`, not `bad`"]
                );
                assert_eq!(calls.load(Ordering::SeqCst), 2);
            },
        )
        .await;
    }

    mod final_input_admission_tests {
        use super::*;
        use crate::schema::{
            PatternMatch, PatternSpec, PatternStructField, StringIdentityLayer,
            StringMemberConstraints,
        };

        struct ReplacingNormalizer(Option<Value>, &'static str);

        impl ProviderNormalizer for ReplacingNormalizer {
            fn normalize_desired<'a>(
                &'a self,
                resources: &'a mut [ProviderResource],
            ) -> BoxFuture<'a, ProviderResult<()>> {
                Box::pin(async move {
                    for resource in resources {
                        match &self.0 {
                            Some(value) => resource.set_attr(self.1, value.clone()),
                            None => {
                                resource.attributes.shift_remove(self.1);
                            }
                        }
                    }
                    Ok(())
                })
            }

            fn normalize_state<'a>(
                &'a self,
                _: &'a mut crate::provider::StateTransformMap,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                crate::provider::ready_noop()
            }

            fn hydrate_read_state<'a>(
                &'a self,
                _: &'a mut crate::provider::StateTransformMap,
                _: &'a crate::provider::SavedAttrs,
            ) -> BoxFuture<'a, ProviderResult<()>> {
                crate::provider::ready_noop()
            }
        }

        fn text(value: &str) -> Value {
            Value::Concrete(ConcreteValue::String(value.into()))
        }

        async fn cleanup_candidate(unknown: bool, plan: bool) -> ProviderPreparationError {
            let mut schemas = SchemaRegistry::new();
            let slot = PatternAttributeType::struct_(
                "Options",
                vec![
                    PatternStructField::required("first", PatternAttributeType::bool()).build(),
                    PatternStructField::required("second", PatternAttributeType::bool()).build(),
                ],
            );
            schemas.insert(
                "mock",
                PatternResourceSchema::new("test.Final")
                    .attribute(PatternAttributeSchema::optional("value", slot).build())
                    .attribute(
                        PatternAttributeSchema::optional("other", PatternAttributeType::bool())
                            .build(),
                    )
                    .validate()
                    .unwrap(),
            );
            let bindings = ResolvedBindings::default();
            let gate = ModuleConstraintGate::from_live_compositions(&[]);
            crate::validation::deferred_types::with_session_for_test_async(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                async |session| {
                    let invalid = Value::Concrete(ConcreteValue::Map(IndexMap::from([
                        ("first".into(), text("invalid")),
                        ("second".into(), text("invalid")),
                    ])));
                    let normalizer =
                        crate::provider::ConfiguredNormalizer::fixture(ReplacingNormalizer(
                            Some(invalid),
                            if unknown { "undeclared" } else { "value" },
                        ));
                    let context = ProviderPreparationContext::for_test(
                        &bindings,
                        &gate,
                        &[],
                        &normalizer,
                        &schemas,
                        session,
                    );
                    let resource = Resource::with_provider("mock", "test.Final", "example", None);
                    if plan {
                        // FinalPlanInputs receives the canonicalized, normalized plan payload.
                        let mut candidate =
                            crate::executor::normalized::apply_host_desired_normalization(
                                resource,
                                &[],
                                &normalizer,
                                &schemas,
                            )
                            .await
                            .unwrap()
                            .into_resource();
                        if !unknown {
                            candidate.set_attr("other", text("invalid"));
                        }
                        let resolved =
                            ResolvedResource::resolve_with_schemas(candidate.into(), &schemas)
                                .unwrap();
                        let result =
                            crate::validation::admission::final_input::FinalPlanInputs::admit(
                                vec![resolved],
                                vec![],
                                &schemas,
                                &bindings,
                                session,
                            );
                        ProviderPreparationError::FinalInput(
                            result.expect_err("final plan input must be rejected"),
                        )
                    } else {
                        // Plan provenance bypasses authored-shape checking of the candidate.
                        let mut resource = resource;
                        if !unknown {
                            resource.set_attr("other", text("invalid"));
                        }
                        prepare_provider_ready_resource_after_resolution(
                            ResolvedCheckInput::plan_normalized(
                                resource,
                                HashMap::new(),
                                HashSet::new(),
                            ),
                            &context,
                        )
                        .await
                        .expect_err("final dispatch input must be rejected")
                    }
                },
            )
            .await
        }

        #[test]
        fn issue_4304_3a_review1_plan_reports_every_resource_and_data_source() {
            let mut schemas = SchemaRegistry::new();
            for data in [false, true] {
                let schema = PatternResourceSchema::new("test.Final").attribute(
                    PatternAttributeSchema::optional("value", PatternAttributeType::bool()).build(),
                );
                schemas.insert(
                    "mock",
                    (if data {
                        schema.as_data_source()
                    } else {
                        schema
                    })
                    .validate()
                    .unwrap(),
                );
            }
            let resources = ["first", "second"]
                .into_iter()
                .map(|name| {
                    let resource = Resource::with_provider("mock", "test.Final", name, None)
                        .with_attribute("value", text("invalid"));
                    ResolvedResource::resolve_with_schemas(resource.into(), &schemas).unwrap()
                })
                .collect();
            let mut data =
                crate::resource::DataSource::with_provider("mock", "test.Final", "lookup", None);
            data.set_attr("value", text("invalid"));
            crate::validation::deferred_types::with_session_for_test(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                |session| {
                    let error = crate::validation::admission::final_input::FinalPlanInputs::admit(
                        resources,
                        vec![ResolvedDataSource::new(data.into_provider_data_source())],
                        &schemas,
                        &ResolvedBindings::default(),
                        session,
                    )
                    .unwrap_err();
                    assert_eq!(error.findings().len(), 3, "{error:?}");
                    let message = diagnostic(&error);
                    for name in ["first", "second", "lookup"] {
                        assert!(message.contains(name), "missing {name}: {message}");
                    }
                },
            );
        }

        #[tokio::test]
        async fn issue_4304_3a_cleanup_plan_keeps_all_findings() {
            let error = cleanup_candidate(false, true).await;
            let ProviderPreparationError::FinalInput(error) = error else {
                panic!("wrong error: {error:?}");
            };
            assert_eq!(error.findings().len(), 3);
            let message = diagnostic(&error);
            for path in ["value.first", "value.second", "other"] {
                assert!(message.contains(path), "missing {path}: {message}");
            }
        }

        #[tokio::test]
        async fn issue_4304_3a_cleanup_dispatch_keeps_all_findings() {
            let error = cleanup_candidate(false, false).await;
            let ProviderPreparationError::FinalInput(error) = error else {
                panic!("wrong error: {error:?}");
            };
            assert_eq!(error.findings().len(), 3);
            let message = diagnostic(&error);
            for path in ["value.first", "value.second", "other"] {
                assert!(message.contains(path), "missing {path}: {message}");
            }
        }

        #[tokio::test]
        async fn issue_4304_3a_cleanup_plan_rejects_undeclared_normalizer_attribute() {
            let message = diagnostic(&cleanup_candidate(true, true).await);
            assert!(
                message.contains("undeclared") && message.contains("provider normalization"),
                "{message}"
            );
        }

        #[tokio::test]
        async fn issue_4304_3a_cleanup_dispatch_rejects_undeclared_normalizer_attribute() {
            let message = diagnostic(&cleanup_candidate(true, false).await);
            assert!(
                message.contains("undeclared") && message.contains("provider normalization"),
                "{message}"
            );
        }

        #[tokio::test]
        async fn issue_4304_3a_cleanup_update_patch_preserves_all_pending_diagnostics() {
            let mut schemas = SchemaRegistry::new();
            schemas.insert(
                "mock",
                PatternResourceSchema::new("test.Final")
                    .attribute(
                        PatternAttributeSchema::optional("value", PatternAttributeType::bool())
                            .build(),
                    )
                    .validate()
                    .unwrap(),
            );
            let bindings = ResolvedBindings::default();
            let gate = ModuleConstraintGate::from_live_compositions(&[]);
            let ready = crate::validation::deferred_types::with_session_for_test_async(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                async |session| {
                    let context = ProviderPreparationContext::for_test(
                        &bindings,
                        &gate,
                        &[],
                        &crate::provider::NoopNormalizer,
                        &schemas,
                        session,
                    );
                    prepare_provider_ready_resource(
                        Resource::with_provider("mock", "test.Final", "example", None)
                            .with_attribute("value", Value::Concrete(ConcreteValue::Bool(true))),
                        &context,
                    )
                    .await
                    .unwrap()
                },
            )
            .await;
            let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
            let result =
                crate::validation::deferred_types::TypeCheckSession::run(&catalog, |session| {
                    for (label, name) in [
                        ("first pending check", "MissingFirst"),
                        ("second pending check", "MissingSecond"),
                    ] {
                        let expected = crate::parser::parse_type_expr_str(name, &catalog).unwrap();
                        session.record_type(&expected, label);
                    }
                    let context = ProviderPreparationContext::for_test(
                        &bindings,
                        &gate,
                        &[],
                        &crate::provider::NoopNormalizer,
                        &schemas,
                        session,
                    );
                    let from = crate::resource::SnapshotState::existing(
                        ready.id.clone(),
                        crate::test_support::snapshot_attributes(HashMap::<String, Value>::new()),
                    );
                    context.prepare_update_patch(&["value".into()], &ready, &from)
                })
                .publish(&mut Vec::new());
            let error = result.unwrap_err();
            let message = diagnostic(&error);
            assert!(
                message.contains("first pending check") && message.contains("second pending check"),
                "{message}"
            );
            let ProviderPreparationError::Provider(error) = error else {
                panic!("wrong error: {error:?}")
            };
            assert!(
                error
                    .detail()
                    .cause()
                    .is_some_and(|cause| cause.is::<PreparationDiagnostics>()),
                "pending diagnostics lost their typed cause"
            );
        }

        #[tokio::test]
        async fn issue_4304_3a_update_patch_readmits_observed_ignored_leaves() {
            let mut schemas = SchemaRegistry::new();
            schemas.insert(
                "test",
                PatternResourceSchema::new("widget")
                    .attribute(
                        PatternAttributeSchema::required(
                            "config",
                            PatternAttributeType::struct_(
                                "Config",
                                vec![
                                    PatternStructField::required(
                                        "version",
                                        PatternAttributeType::refined_string(
                                            None,
                                            None,
                                            Some((None, Some(3))),
                                            None,
                                        ),
                                    )
                                    .build(),
                                    PatternStructField::required(
                                        "memory",
                                        PatternAttributeType::int(),
                                    )
                                    .build(),
                                ],
                            ),
                        )
                        .build(),
                    )
                    .validate()
                    .unwrap(),
            );
            let config = |version: &str, memory| {
                Value::Concrete(ConcreteValue::Map(IndexMap::from([
                    ("version".into(), text(version)),
                    ("memory".into(), Value::Concrete(ConcreteValue::Int(memory))),
                ])))
            };
            let mut resource = Resource::with_provider("test", "widget", "example", None)
                .with_attribute("config", config("ok", 256));
            resource.directives.ignore_changes = vec![ParsedIgnorePath::new(vec![
                ParsedIgnorePathSegment::Attribute("config".into()),
                ParsedIgnorePathSegment::Attribute("version".into()),
            ])];
            let bindings = ResolvedBindings::default();
            let gate = ModuleConstraintGate::from_live_compositions(&[]);
            crate::validation::deferred_types::with_session_for_test_async(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                async |session| {
                    let context = ProviderPreparationContext::for_test(
                        &bindings,
                        &gate,
                        &[],
                        &crate::provider::NoopNormalizer,
                        &schemas,
                        session,
                    );
                    for (observed, protected) in [
                        ("old", false),
                        ("invalid-observed", false),
                        ("invalid-observed", true),
                    ] {
                        let mut resource = resource.clone();
                        if protected {
                            let Value::Concrete(ConcreteValue::Map(fields)) =
                                resource.attributes.get_mut("config").unwrap()
                            else {
                                panic!("fixture config is a struct")
                            };
                            fields.insert("version".into(), text("ok").into_secret());
                        }
                        let from = crate::resource::SnapshotState::existing(
                            resource.id.clone(),
                            crate::test_support::snapshot_attributes(HashMap::from([(
                                "config".into(),
                                config(observed, 128),
                            )])),
                        );
                        let result = prepare_update_request(
                            resource.clone(),
                            from,
                            &["config".into()],
                            &context,
                        )
                        .await;
                        if observed == "old" {
                            let request = result.expect("valid observed graft is admitted");
                            assert_eq!(
                                request.patch().ops[0].sent_value(),
                                Some(&config(observed, 256)),
                            );
                        } else {
                            let error = result
                                .expect_err("resource evidence cannot authorize a different patch");
                            let message = diagnostic(&error);
                            if protected {
                                for rendered in [&message, &format!("{error:?}")] {
                                    assert!(!rendered.contains(observed), "{rendered}");
                                }
                            } else {
                                assert!(message.contains("config.version"), "{message}");
                            }
                            assert!(message.contains("observed update patch"), "{message}");
                            assert!(std::error::Error::source(&error).is_some());
                        }
                    }
                },
            )
            .await;
        }

        fn diagnostic(error: &dyn std::error::Error) -> String {
            let mut rendered = error.to_string();
            let mut cause = error.source();
            while let Some(error) = cause {
                rendered.push_str(&format!("\n{error}"));
                cause = error.source();
            }
            rendered
        }

        async fn prepare_plan_input(
            authored: Resource,
            context: &ProviderPreparationContext<'_>,
        ) -> Result<ProviderReadyResource, ProviderPreparationError> {
            let attributes = authored.resolved_attributes();
            let planned = crate::executor::normalized::apply_host_desired_normalization(
                authored,
                context.provider_configs,
                context.normalizer,
                context.schemas,
            )
            .await
            .expect("fixture normalization must produce the candidate payload")
            .into_resource();
            prepare_provider_ready_resource_after_resolution(
                ResolvedCheckInput::plan_normalized(planned, attributes, HashSet::new()),
                context,
            )
            .await
        }

        async fn assert_plan_rejects_normalized_value(nested: bool) {
            let calls = Arc::new(AtomicUsize::new(0));
            let list = PatternAttributeType::list(PatternAttributeType::string_member(
                StringIdentityLayer::new(Some(TypeIdentity::bare("VpcId")), None, None, None),
                StringMemberConstraints::new(
                    Some(PatternSpec::new("^vpc-[a-z]+$", PatternMatch::Full)),
                    None,
                ),
            ));
            let invalid = Value::Concrete(ConcreteValue::StringList(vec!["bad".into()]));
            let (declaration, normalized, slot) = if nested {
                (
                    PatternAttributeType::struct_(
                        "Options",
                        vec![PatternStructField::required("ids", list).build()],
                    ),
                    Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                        "ids".into(),
                        invalid,
                    )]))),
                    "value.ids",
                )
            } else {
                (list, invalid, "value")
            };
            let factories: Arc<Vec<Box<dyn ProviderFactory>>> =
                Arc::new(vec![Box::new(RoutingFactory {
                    name: "owner",
                    declaration,
                    sibling_declaration: None,
                    accepted: Some("vpc-good"),
                    validate_calls: Arc::clone(&calls),
                    seen_values: None,
                })]);
            let snapshot = crate::provider::loaded_provider_snapshot_from_factories(
                Arc::clone(&factories),
                SchemaRegistry::new(),
                crate::EnvironmentSnapshot::empty(),
            );
            let bindings = ResolvedBindings::default();
            let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
            crate::validation::deferred_types::with_session_for_test_async(
                snapshot.provider_context(),
                async |session| {
                    let normalizer = crate::provider::ConfiguredNormalizer::fixture(
                        ReplacingNormalizer(Some(normalized), "value"),
                    );
                    let context = ProviderPreparationContext::for_test(
                        &bindings,
                        &module_gate,
                        &[],
                        &normalizer,
                        snapshot.schemas().schemas(),
                        session,
                    );
                    let result = prepare_plan_input(
                        Resource::with_provider("owner", "test.owner", "example", None),
                        &context,
                    )
                    .await;
                    assert_eq!(
                        calls.load(Ordering::SeqCst),
                        0,
                        "local destination membership must reject before the provider validate callback"
                    );
                    let error = result.expect_err(
                        "provider normalization introduced an invalid final input; plan preparation must reject it",
                    );
                    assert!(matches!(
                        error,
                        ProviderPreparationError::ValueConstraint { .. }
                            | ProviderPreparationError::Admission { .. }
                            | ProviderPreparationError::FinalInput(_)
                    ));
                    let rendered = diagnostic(&error);
                    assert!(rendered.contains(slot), "missing slot {slot}: {rendered}");
                    assert!(
                        rendered.contains("provider") && rendered.to_ascii_lowercase().contains("normaliz"),
                        "missing provider normalization origin: {rendered}"
                    );
                },
            )
            .await;
        }

        #[tokio::test]
        async fn issue_4304_3a_plan_rejects_provider_normalized_string_list() {
            assert_plan_rejects_normalized_value(false).await;
        }

        #[tokio::test]
        async fn issue_4304_3a_plan_rejects_provider_normalized_nested_struct_field() {
            assert_plan_rejects_normalized_value(true).await;
        }

        async fn scalar_final_error(
            normalized: Option<Value>,
            authored: Value,
            required: bool,
        ) -> ProviderPreparationError {
            let scalar =
                PatternAttributeType::refined_string(None, None, Some((None, Some(4))), None);
            let attribute = if required {
                PatternAttributeSchema::required("value", scalar).build()
            } else {
                PatternAttributeSchema::optional("value", scalar).build()
            };
            let mut schemas = SchemaRegistry::new();
            schemas.insert(
                "mock",
                PatternResourceSchema::new("test.Scalar")
                    .attribute(attribute)
                    .validate()
                    .unwrap(),
            );
            let bindings = ResolvedBindings::default();
            let gate = ModuleConstraintGate::from_live_compositions(&[]);
            crate::validation::deferred_types::with_session_for_test_async(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                async |session| {
                    let normalizer = crate::provider::ConfiguredNormalizer::fixture(
                        ReplacingNormalizer(normalized, "value"),
                    );
                    let context = ProviderPreparationContext::for_test(
                        &bindings,
                        &gate,
                        &[],
                        &normalizer,
                        &schemas,
                        session,
                    );
                    prepare_plan_input(
                        Resource::with_provider("mock", "test.Scalar", "example", None)
                            .with_attribute("value", authored),
                        &context,
                    )
                    .await
                    .expect_err("normalization output must satisfy the full scalar slot")
                },
            )
            .await
        }

        #[tokio::test]
        async fn issue_4304_3a_plan_normalization_preserves_secret_diagnostic_protection() {
            let private = "provider-created-private-value";
            let error =
                scalar_final_error(Some(text(private)), text("good").into_secret(), false).await;
            for rendered in [diagnostic(&error), format!("{error:?}")] {
                assert!(
                    !rendered.contains(private),
                    "secret normalization output leaked: {rendered}"
                );
                assert!(
                    rendered.to_ascii_lowercase().contains("normaliz"),
                    "missing normalization origin: {rendered}"
                );
            }
        }

        #[tokio::test]
        async fn issue_4304_3a_normalizer_cannot_remove_required_slot() {
            let error = scalar_final_error(None, text("good"), true).await;
            let ProviderPreparationError::FinalInput(
                crate::validation::admission::final_input::FinalInputError::Membership {
                    path,
                    error,
                    ..
                },
            ) = error
            else {
                panic!("wrong failure: {error:?}");
            };
            assert_eq!(path.to_string(), "value");
            assert!(matches!(*error, TypeError::MissingRequired { .. }));
        }

        #[derive(Clone, Copy)]
        enum Entry {
            Plan,
            Dispatch,
        }

        fn tag_fixture(
            name: &str,
            tags: PatternAttributeType,
        ) -> (crate::parser::ExpandedParsedFile, SchemaRegistry) {
            let mut schemas = SchemaRegistry::new();
            schemas.insert(
                "mock",
                PatternResourceSchema::new("test.Tagged")
                    .attribute(PatternAttributeSchema::optional("tags", tags).build())
                    .validate()
                    .expect("valid destination tags declaration"),
            );
            let path = crate::paths::ResolvedPath::new(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/issue_4304_3a")
                    .join(name),
            )
            .unwrap();
            let provider_context =
                crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
            let parsed = crate::config_loader::parse_directory(&path, &provider_context)
                .expect("providers.crn and main.crn must parse together");
            let expanded = crate::validation::deferred_types::with_session_for_test(
                &provider_context,
                |session| {
                    crate::module_resolver::resolve_modules_with_providerless_config(
                        parsed, &path, session, &schemas,
                    )
                },
            )
            .expect("directory fixture must resolve")
            .into_expanded(&schemas, &provider_context)
            .expect("fixture has no module placeholders");
            assert_eq!(
                expanded.providers.len(),
                if name == "instances" { 2 } else { 1 }
            );
            assert_eq!(
                expanded.resources.len(),
                match name {
                    "key_pattern" => 1,
                    "instances" => 3,
                    _ => 2,
                }
            );
            (expanded, schemas)
        }

        async fn prepare_tags(
            resource: Resource,
            entry: Entry,
            context: &ProviderPreparationContext<'_>,
        ) -> Result<ProviderReadyResource, ProviderPreparationError> {
            match entry {
                Entry::Plan => prepare_plan_input(resource, context).await,
                Entry::Dispatch => prepare_create_request(resource, context)
                    .await
                    .map(|request| request.resource().clone()),
            }
        }

        async fn assert_tag_constraint(
            fixture: &str,
            tags: PatternAttributeType,
            key: &str,
            entry: Entry,
        ) {
            let (file, schemas) = tag_fixture(fixture, tags);
            let bindings = ResolvedBindings::default();
            let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
            crate::validation::deferred_types::with_session_for_test_async(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                async |session| {
                    let context = ProviderPreparationContext::for_test(
                        &bindings,
                        &module_gate,
                        &file.providers,
                        &crate::provider::NoopNormalizer,
                        &schemas,
                        session,
                    );
                    // Check the valid sibling first. In particular, an invalid
                    // default value hidden by an authored override is inapplicable.
                    if fixture != "key_pattern" {
                        let overridden = file.resources.iter().find(|resource| {
                            resource.id.identity_str() == Some("overridden")
                        }).unwrap().clone();
                        let ready = prepare_tags(overridden, entry, &context)
                            .await
                            .expect("authored overrides must survive final-map admission");
                        let tags = ready.as_resource().get_attr("tags").unwrap();
                        let Value::Concrete(ConcreteValue::Map(tags)) = tags else {
                            panic!("expected merged tag map: {tags:?}");
                        };
                        assert_eq!(tags.get("Owner"), Some(&text("mine")));
                        assert!(!ready.as_resource().default_tag_keys.contains("Owner"));
                        if fixture == "entry_count" {
                            assert_eq!(tags.len(), 2, "overrides must not count as extra entries");
                        }
                    }

                    let applicable = file.resources.iter().find(|resource| {
                        resource.id.identity_str() == Some("applicable")
                    }).unwrap().clone();
                    let error = prepare_tags(applicable, entry, &context)
                        .await
                        .expect_err("the exact merged default_tags map must satisfy the destination tags declaration");
                    assert!(matches!(
                        error,
                        ProviderPreparationError::ValueConstraint { .. }
                            | ProviderPreparationError::Admission { .. }
                            | ProviderPreparationError::FinalInput(_)
                    ));
                    let rendered = diagnostic(&error);
                    for required in ["tags", key, "mock", "default_tags"] {
                        assert!(rendered.contains(required), "missing {required}: {rendered}");
                    }
                },
            )
            .await;
        }

        fn value_bounded_tags() -> PatternAttributeType {
            PatternAttributeType::map(PatternAttributeType::string_member(
                StringIdentityLayer::new(None, None, None, None),
                StringMemberConstraints::new(None, Some((None, Some(4)))),
            ))
        }

        fn key_bounded_tags() -> PatternAttributeType {
            PatternAttributeType::map_with_key(
                PatternAttributeType::string_member(
                    StringIdentityLayer::new(None, None, None, None),
                    StringMemberConstraints::new(
                        Some(PatternSpec::new("^[A-Z][a-z]+$", PatternMatch::Full)),
                        None,
                    ),
                ),
                PatternAttributeType::string(),
            )
        }

        fn count_bounded_tags() -> PatternAttributeType {
            PatternAttributeType::refined_map(
                PatternAttributeType::string(),
                PatternAttributeType::string(),
                Some((None, Some(2))),
            )
        }

        #[tokio::test]
        async fn issue_4304_3a_plan_default_tag_value_length_preserves_authored_override() {
            assert_tag_constraint("value_length", value_bounded_tags(), "Owner", Entry::Plan).await;
        }

        #[tokio::test]
        async fn issue_4304_3a_dispatch_default_tag_value_length_preserves_authored_override() {
            assert_tag_constraint(
                "value_length",
                value_bounded_tags(),
                "Owner",
                Entry::Dispatch,
            )
            .await;
        }

        #[tokio::test]
        async fn issue_4304_3a_plan_default_tag_key_pattern() {
            assert_tag_constraint(
                "key_pattern",
                key_bounded_tags(),
                "invalid_key",
                Entry::Plan,
            )
            .await;
        }

        #[tokio::test]
        async fn issue_4304_3a_dispatch_default_tag_key_pattern() {
            assert_tag_constraint(
                "key_pattern",
                key_bounded_tags(),
                "invalid_key",
                Entry::Dispatch,
            )
            .await;
        }

        #[tokio::test]
        async fn issue_4304_3a_plan_merged_default_tags_entry_count() {
            assert_tag_constraint("entry_count", count_bounded_tags(), "Team", Entry::Plan).await;
        }

        #[tokio::test]
        async fn issue_4304_3a_dispatch_merged_default_tags_entry_count() {
            assert_tag_constraint("entry_count", count_bounded_tags(), "Team", Entry::Dispatch)
                .await;
        }

        #[tokio::test]
        async fn issue_4304_3a_named_provider_defaults_are_admitted_per_instance() {
            let (file, schemas) = tag_fixture("instances", value_bounded_tags());
            let bindings = ResolvedBindings::default();
            let gate = ModuleConstraintGate::from_live_compositions(&[]);
            crate::validation::deferred_types::with_session_for_test_async(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                async |session| {
                    let context = ProviderPreparationContext::for_test(
                        &bindings,
                        &gate,
                        &file.providers,
                        &crate::provider::NoopNormalizer,
                        &schemas,
                        session,
                    );
                    for entry in [Entry::Plan, Entry::Dispatch] {
                        for (name, expected, instance) in [
                            ("default_resource", "east", None),
                            ("overridden", "mine", Some("west")),
                        ] {
                            let resource = file
                                .resources
                                .iter()
                                .find(|resource| resource.id.identity_str() == Some(name))
                                .unwrap()
                                .clone();
                            let ready = prepare_tags(resource, entry, &context).await.unwrap();
                            assert_eq!(
                                ready.as_resource().id.provider_instance.as_deref(),
                                instance
                            );
                            let Some(Value::Concrete(ConcreteValue::Map(tags))) =
                                ready.as_resource().get_attr("tags")
                            else {
                                panic!("expected tag map");
                            };
                            assert_eq!(tags.get("Owner"), Some(&text(expected)));
                        }
                        let resource = file
                            .resources
                            .iter()
                            .find(|resource| resource.id.identity_str() == Some("applicable"))
                            .unwrap()
                            .clone();
                        let error = prepare_tags(resource, entry, &context).await.unwrap_err();
                        let rendered = diagnostic(&error);
                        for part in ["west", "Owner", "default_tags"] {
                            assert!(rendered.contains(part), "{rendered}");
                        }
                    }
                },
            )
            .await;
        }

        #[tokio::test]
        async fn issue_4304_3a_normalized_tag_keeps_its_origin_beside_valid_defaults() {
            let (mut file, schemas) = tag_fixture("value_length", value_bounded_tags());
            file.providers[0].default_tags.insert("Owner", text("core"));
            let resource = file
                .resources
                .iter()
                .find(|resource| resource.id.identity_str() == Some("applicable"))
                .unwrap()
                .clone()
                .with_attribute(
                    "tags",
                    Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                        "Authored".into(),
                        text("good"),
                    )]))),
                );
            let normalizer = crate::provider::ConfiguredNormalizer::fixture(ReplacingNormalizer(
                Some(Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                    "Authored".into(),
                    text("invalid-normalized-value"),
                )])))),
                "tags",
            ));
            let bindings = ResolvedBindings::default();
            let gate = ModuleConstraintGate::from_live_compositions(&[]);
            crate::validation::deferred_types::with_session_for_test_async(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                async |session| {
                    let context = ProviderPreparationContext::for_test(
                        &bindings, &gate, &file.providers, &normalizer, &schemas, session,
                    );
                    for entry in [Entry::Plan, Entry::Dispatch] {
                        let error = prepare_tags(resource.clone(), entry, &context)
                            .await.expect_err("normalization must reject the authored key it changed");
                        let ProviderPreparationError::FinalInput(
                            crate::validation::admission::final_input::FinalInputError::Membership { origin, .. }
                        ) = &error else { panic!("expected final membership error: {error:?}") };
                        assert!(matches!(origin, crate::validation::admission::final_input::FinalInputOrigin::ProviderNormalization));
                        assert!(diagnostic(&error).contains("Authored"));
                    }
                },
            ).await;
        }

        #[tokio::test]
        async fn issue_4304_3a_admission_evidence_cannot_authorize_replacement_payload() {
            let mut schemas = SchemaRegistry::new();
            schemas.insert(
                "mock",
                PatternResourceSchema::new("test.Tagged")
                    .attribute(
                        PatternAttributeSchema::optional("tags", value_bounded_tags()).build(),
                    )
                    .validate()
                    .unwrap(),
            );
            let bindings = ResolvedBindings::default();
            let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
            crate::validation::deferred_types::with_session_for_test_async(
                &crate::parser::LoadedProviderContext::empty_for_test(),
                async |session| {
                    let context = ProviderPreparationContext::for_test(
                        &bindings,
                        &module_gate,
                        &[],
                        &crate::provider::NoopNormalizer,
                        &schemas,
                        session,
                    );
                    let original = Resource::with_provider("mock", "test.Tagged", "example", None)
                        .with_attribute(
                            "tags",
                            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                                "Owner".into(),
                                text("good"),
                            )]))),
                        );
                    let admitted = prepare_provider_ready_resource(original, &context)
                        .await
                        .unwrap();
                    let mut replacement = admitted.as_resource().clone();
                    replacement.set_attr(
                        "tags",
                        Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                            "Owner".into(),
                            text("invalid-replacement"),
                        )]))),
                    );
                    // FinalInput owns P and exposes no detachable evidence.
                    // The compile_fail examples in final_input.rs forbid rebinding
                    // or mutation; a cloned replacement must be admitted again.
                    let result = prepare_provider_ready_resource(replacement, &context).await;
                    assert!(
                        result.is_err(),
                        "admission evidence for P authorized a different payload P': {result:?}"
                    );
                },
            )
            .await;
        }
    }

    impl ProviderFactory for RoutingFactory {
        fn name(&self) -> &str {
            self.name
        }

        fn display_name(&self) -> &str {
            self.name
        }

        fn validate_config(
            &self,
            _attributes: &CanonicalProviderConfigAttributes,
            _session: &crate::provider::ProviderValidationInputs,
        ) -> ProviderResult<()> {
            Ok(())
        }

        fn validate_custom_type(
            &self,
            _identity: &TypeIdentity,
            value: &crate::provider::CanonicalCustomTypeValue,
            _session: &crate::provider::ProviderValidationInputs,
        ) -> ProviderResult<()> {
            self.validate_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(seen_values) = &self.seen_values {
                seen_values.lock().unwrap().push(value.to_string());
            }
            match self.accepted {
                Some(expected) if value.as_str() != expected => {
                    Err(ProviderError::invalid_input(format!(
                        "{} accepts `{expected}`, not `{}`",
                        self.name,
                        value.as_str()
                    )))
                }
                Some(_) | None => Ok(()),
            }
        }

        fn create_provider(
            &self,
            _config: &crate::provider::ProviderSession,
            _initialize_outcome: crate::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>> {
            unreachable!("routing tests do not create providers")
        }

        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                crate::schema::ValidatedConfigSchema::default(),
                Default::default(),
                Vec::new(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            let mut schema = PatternResourceSchema::new(format!("test.{}", self.name)).attribute(
                PatternAttributeSchema::optional("value", self.declaration.clone()).build(),
            );
            if let Some(sibling) = &self.sibling_declaration {
                schema = schema.attribute(
                    PatternAttributeSchema::optional("sibling", sibling.clone()).build(),
                );
            }
            vec![
                schema
                    .validate()
                    .expect("routing test schema must validate"),
            ]
        }
    }

    fn test_schema_path_resolver(
        factories: &[Box<dyn ProviderFactory>],
    ) -> impl crate::parser::CustomTypeResolver + '_ {
        crate::provider::FactoryCustomTypeResolver::new(factories)
    }

    fn test_schema_registry(factories: &[Box<dyn ProviderFactory>]) -> SchemaRegistry {
        let mut registry = SchemaRegistry::new();
        for factory in factories {
            for schema in factory.schemas() {
                registry.insert(factory.name(), schema);
            }
        }
        registry
    }

    fn test_lookup_for_owner<'a>(
        resolver: &'a dyn crate::parser::CustomTypeResolver,
        registry: &'a SchemaRegistry,
        owner: &str,
    ) -> impl Fn(&AttributeType, &Value) -> Result<(), TypeError> + use<'a> {
        let resource_type = format!("test.{owner}");
        let schema = registry
            .get(owner, &resource_type, crate::schema::SchemaKind::Resource)
            .expect("routing fixture schema must be registered under its owner");
        let lookup = crate::parser::custom_type_lookup_for_schema(resolver, schema);
        move |declaration, value| {
            crate::schema::RootlessType::new(declaration.clone())
                .validate_with_lookup(value, &lookup)
                .map_err(|errors| errors.into_iter().next().expect("validation failure"))
        }
    }

    fn test_custom_type_validator(
        factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>>,
    ) -> crate::parser::CustomTypeValidatorFn {
        let resolver = crate::provider::FactoryCustomTypeResolver::new(factories);
        Box::new(move |target, value| {
            crate::parser::validate_custom_type_value(
                &resolver,
                target,
                &(value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap()
                    .into_known()
                    .unwrap(),
            )
        })
    }

    #[test]
    fn same_identity_routes_each_value_to_its_owning_factory() {
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration = PatternAttributeType::refined_string(Some(identity), None, None, None);
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(RoutingFactory {
                name: "owner",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: Some("owner-value"),
                validate_calls: Arc::clone(&owner_calls),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "other",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: Some("other-value"),
                validate_calls: Arc::clone(&other_calls),
                seen_values: None,
            }),
        ];
        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        let owner_lookup = test_lookup_for_owner(&resolver, &registry, "owner");
        let other_lookup = test_lookup_for_owner(&resolver, &registry, "other");

        owner_lookup(
            &admit_declaration(declaration.clone()),
            &Value::Concrete(ConcreteValue::String("owner-value".into())),
        )
        .expect("the owner must apply its own verdict");
        assert!(
            owner_lookup(
                &admit_declaration(declaration.clone()),
                &Value::Concrete(ConcreteValue::String("other-value".into())),
            )
            .is_err(),
            "the owner's verdict must not be replaced by the colliding factory"
        );
        other_lookup(
            &admit_declaration(declaration.clone()),
            &Value::Concrete(ConcreteValue::String("other-value".into())),
        )
        .expect("the other factory must apply its own verdict");
        assert!(
            other_lookup(
                &admit_declaration(declaration.clone()),
                &Value::Concrete(ConcreteValue::String("owner-value".into())),
            )
            .is_err(),
            "the other factory's verdict must stay owner-qualified"
        );

        assert_eq!(owner_calls.load(Ordering::SeqCst), 2);
        assert_eq!(other_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn resource_lookup_rejects_an_unknown_schema_owner() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration =
            PatternAttributeType::refined_string(Some(identity.clone()), None, None, None);
        let factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
        let resolver = test_schema_path_resolver(&factories);
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "missing",
            PatternResourceSchema::new("test.missing")
                .attribute(PatternAttributeSchema::optional("value", declaration.clone()).build())
                .validate()
                .expect("missing-owner fixture schema must validate"),
        );
        let lookup = test_lookup_for_owner(&resolver, &registry, "missing");

        let error = lookup(
            &admit_declaration(declaration.clone()),
            &Value::Concrete(ConcreteValue::String("value".into())),
        )
        .expect_err("an owner absent from the factory index must not disable validation");

        let TypeError::CustomTypeValidation {
            error,
            attribute: None,
        } = error
        else {
            panic!("expected typed custom-type validation error");
        };
        assert!(matches!(
            error.as_ref(),
            crate::parser::CustomTypeValidationError::MissingDeclaration { owner, identity: actual }
                if owner == "missing" && actual == &identity
        ));
    }

    #[test]
    fn unloaded_provider_context_never_approves_an_identified_declaration() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration =
            PatternAttributeType::refined_string(Some(identity.clone()), None, None, None);
        let context = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let mut registry = SchemaRegistry::new();
        registry.insert(
            "missing",
            PatternResourceSchema::new("test.missing")
                .attribute(PatternAttributeSchema::optional("value", declaration.clone()).build())
                .validate()
                .expect("unloaded-owner fixture schema must validate"),
        );
        let lookup = test_lookup_for_owner(context.custom_type_resolver(), &registry, "missing");

        let error = lookup(
            &admit_declaration(declaration.clone()),
            &Value::Concrete(ConcreteValue::String("value".into())),
        )
        .expect_err("an unloaded context must not turn a custom declaration into success");

        assert!(matches!(
            error,
            TypeError::CustomTypeValidation { error, attribute: None }
                if matches!(error.as_ref(),
                    crate::parser::CustomTypeValidationError::ProvidersNotLoaded)
        ));
    }

    #[test]
    fn shared_factory_resolver_rejects_an_unknown_schema_owner() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration = AttributeType::refined_string(Some(identity.clone()), None, None, None);
        let declaration = crate::parser::IdentifiedCustomTypeDeclaration::new(&declaration)
            .expect("fixture declaration is identified");
        let factories: Vec<Box<dyn ProviderFactory>> = Vec::new();
        let validator = crate::provider::FactoryCustomTypeResolver::new(factories.as_slice());

        let error = crate::parser::validate_custom_type_value(
            &validator,
            crate::parser::CustomTypeValidationTarget::Declaration {
                owner: "missing",
                declaration,
            },
            &(Value::Concrete(ConcreteValue::String("value".into())))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
        )
        .expect_err("an owner absent from the shared index must not disable validation");

        assert!(matches!(
            error,
            crate::parser::CustomTypeValidationError::MissingDeclaration {
                owner,
                identity: actual,
            } if owner == "missing" && actual == identity
        ));
    }

    #[test]
    fn multi_owner_annotation_requires_every_owner_to_accept() {
        use crate::parser::{NonEmptySchemaTypeOwners, OwnedTypeIdentity, SchemaTypeOwner};

        let identity = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
        let declaration =
            PatternAttributeType::refined_string(Some(identity.clone()), None, None, None);
        let factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>> = std::sync::Arc::new(vec![
            Box::new(RoutingFactory {
                name: "aws",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: Some("arn:accepted"),
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "awscc",
                declaration,
                sibling_declaration: None,
                accepted: Some("arn:other"),
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            }),
        ]);
        let validator = test_custom_type_validator(factories);
        let owned = OwnedTypeIdentity::with_owners(
            identity,
            NonEmptySchemaTypeOwners::try_from_iter([
                SchemaTypeOwner::Provider("aws".to_string()),
                SchemaTypeOwner::Provider("awscc".to_string()),
            ])
            .expect("two owners are non-empty"),
            crate::schema::RefinedScalarKind::String,
        );
        let value = Value::Concrete(ConcreteValue::String("arn:accepted".into()));

        let error = validator(
            crate::parser::CustomTypeValidationTarget::Annotation(&owned),
            &value,
        )
        .expect_err("one rejecting owner rejects the shared annotation");

        assert!(error.to_string().contains("awscc"), "{error}");
    }

    #[test]
    fn multi_owner_enum_definition_disagreement_is_typed() {
        use crate::parser::{
            CustomTypeValidationError, NonEmptySchemaTypeOwners, OwnedTypeIdentity, SchemaTypeOwner,
        };

        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Mode");
        let first = PatternAttributeType::enum_(
            identity.clone(),
            Some(vec!["COMMON".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let second = PatternAttributeType::enum_(
            identity.clone(),
            Some(vec!["COMMON".to_string(), "OTHER".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>> = std::sync::Arc::new(vec![
            Box::new(RoutingFactory {
                name: "one",
                declaration: first,
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "two",
                declaration: second,
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            }),
        ]);
        let validator = test_custom_type_validator(factories);
        let owned = OwnedTypeIdentity::with_owners(
            identity.clone(),
            NonEmptySchemaTypeOwners::try_from_iter([
                SchemaTypeOwner::Provider("two".to_string()),
                SchemaTypeOwner::Provider("one".to_string()),
            ])
            .expect("two owners are non-empty"),
            crate::schema::RefinedScalarKind::String,
        );
        let value = Value::Concrete(ConcreteValue::enum_identifier("shared.Mode.COMMON"));

        let error = validator(
            crate::parser::CustomTypeValidationTarget::Annotation(&owned),
            &value,
        )
        .expect_err("owners with different enum sets must not be first-matched");

        assert!(
            matches!(
                &error,
                CustomTypeValidationError::ConflictingEnumDefinitions {
                    identity: actual,
                    first_owner,
                    second_owner,
                } if actual == &identity && first_owner == "one" && second_owner == "two"
            ),
            "{error:?}"
        );
    }

    #[test]
    fn sibling_enum_declarations_do_not_report_an_owner_conflict_with_itself() {
        use crate::parser::{CustomTypeValidationTarget, OwnedTypeIdentity};

        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Mode");
        let first = PatternAttributeType::enum_(
            identity.clone(),
            Some(vec!["COMMON".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let sibling = PatternAttributeType::enum_(
            identity.clone(),
            Some(vec!["COMMON".to_string(), "OTHER".to_string()]),
            Vec::new(),
            None,
            None,
        );
        let factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>> =
            std::sync::Arc::new(vec![Box::new(RoutingFactory {
                name: "owner",
                declaration: first,
                sibling_declaration: Some(sibling),
                accepted: None,
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            })]);
        let validator = test_custom_type_validator(factories);
        let owned = OwnedTypeIdentity::provider(
            "owner",
            identity,
            crate::schema::RefinedScalarKind::String,
        );
        let value = Value::Concrete(ConcreteValue::enum_identifier("shared.Mode.COMMON"));

        validator(CustomTypeValidationTarget::Annotation(&owned), &value)
            .expect("sibling declarations belong to one owner, not two conflicting owners");
    }

    #[test]
    fn declaration_local_schema_failure_remains_typed() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration = PatternAttributeType::refined_string(
            Some(identity.clone()),
            Some(crate::schema::PatternSpec::new(
                "^accepted$".to_string(),
                crate::schema::PatternMatch::Full,
            )),
            None,
            None,
        );
        let factories: std::sync::Arc<Vec<Box<dyn ProviderFactory>>> =
            std::sync::Arc::new(vec![Box::new(RoutingFactory {
                name: "owner",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::new(AtomicUsize::new(0)),
                seen_values: None,
            })]);
        let validator = test_custom_type_validator(factories);
        let declaration = admit_declaration(declaration);
        let declaration = crate::parser::IdentifiedCustomTypeDeclaration::new(&declaration)
            .expect("fixture declaration is identified");

        let error = validator(
            crate::parser::CustomTypeValidationTarget::Declaration {
                owner: "owner",
                declaration,
            },
            &Value::Concrete(ConcreteValue::String("rejected".into())),
        )
        .expect_err("the declaration pattern must reject the value");

        assert!(matches!(
            error,
            crate::parser::CustomTypeValidationError::InvalidDeclarationValue {
                owner,
                identity: actual,
                error,
            } if owner == "owner"
                && actual == identity
                && matches!(error.as_ref(), TypeError::PatternMismatch { .. })
        ));
    }

    #[test]
    fn resource_lookup_validates_only_the_exact_declared_type() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let base = crate::schema::StringIdentityLayer::new(Some(identity), None, None, None);
        let own_declaration = PatternAttributeType::string_member(
            base.clone(),
            crate::schema::StringMemberConstraints::new(
                Some(crate::schema::PatternSpec::new(
                    "^own$",
                    crate::schema::PatternMatch::Full,
                )),
                None,
            ),
        );
        let sibling_declaration = PatternAttributeType::string_member(
            base,
            crate::schema::StringMemberConstraints::new(
                Some(crate::schema::PatternSpec::new(
                    "^sibling$",
                    crate::schema::PatternMatch::Full,
                )),
                None,
            ),
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(RoutingFactory {
            name: "owner",
            declaration: own_declaration.clone(),
            sibling_declaration: Some(sibling_declaration),
            accepted: None,
            validate_calls: Arc::clone(&calls),
            seen_values: None,
        })];

        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        test_lookup_for_owner(&resolver, &registry, "owner")(
            &admit_declaration(own_declaration.clone()),
            &Value::Concrete(ConcreteValue::String("own".into())),
        )
        .expect("a sibling declaration with the same identity must not constrain this attribute");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn same_identity_enum_spelling_resolves_in_the_owning_schema() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Mode");
        let owner_type = PatternAttributeType::enum_(
            identity.clone(),
            Some(vec!["OWNER_API".to_string()]),
            vec![("OWNER_API".to_string(), "owner".to_string())],
            None,
            None,
        );
        let other_type = PatternAttributeType::enum_(
            identity,
            Some(vec!["OTHER_API".to_string()]),
            vec![("OTHER_API".to_string(), "other".to_string())],
            None,
            None,
        );
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(RoutingFactory {
                name: "owner",
                declaration: owner_type.clone(),
                sibling_declaration: None,
                accepted: Some("OWNER_API"),
                validate_calls: Arc::clone(&owner_calls),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "other",
                declaration: other_type.clone(),
                sibling_declaration: None,
                accepted: Some("OTHER_API"),
                validate_calls: Arc::clone(&other_calls),
                seen_values: None,
            }),
        ];

        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        test_lookup_for_owner(&resolver, &registry, "owner")(
            &admit_declaration(owner_type.clone()),
            &Value::Concrete(ConcreteValue::enum_identifier("shared.Mode.owner")),
        )
        .expect("the owner alias must resolve to the owner's API spelling");
        test_lookup_for_owner(&resolver, &registry, "other")(
            &admit_declaration(other_type.clone()),
            &Value::Concrete(ConcreteValue::enum_identifier("shared.Mode.other")),
        )
        .expect("the other alias must resolve to the other API spelling");

        assert_eq!(owner_calls.load(Ordering::SeqCst), 1);
        assert_eq!(other_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn owning_factory_schema_rejects_scalar_shape_mismatch() {
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Count");
        let calls = Arc::new(AtomicUsize::new(0));
        let factory = RoutingFactory {
            name: "owner",
            declaration: PatternAttributeType::refined_int(Some(identity.clone()), None),
            sibling_declaration: None,
            accepted: None,
            validate_calls: Arc::clone(&calls),
            seen_values: None,
        };

        let error = crate::provider::validate_custom_type_with_factory(
            &factory,
            crate::parser::IdentifiedCustomTypeDeclaration::new(&admit_declaration(
                factory.declaration.clone(),
            ))
            .expect("fixture declaration is identified"),
            &(Value::Concrete(ConcreteValue::String("5".into())))
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .unwrap()
                .into_known()
                .unwrap(),
            &crate::provider::ProviderValidationInputs::collect(
                factory.name(),
                std::iter::empty::<&Value>(),
            ),
        )
        .expect_err("an Int declaration must not accept bridge-shaped String input");

        let rendered = error.to_string();
        assert!(rendered.contains("expected shared.Count"), "{rendered}");
        assert!(rendered.contains("got String"), "{rendered}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn executor_lookup_receives_canonical_numeric_text() {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen_values = Arc::new(Mutex::new(Vec::new()));
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Number");
        let declaration = PatternAttributeType::refined_float(Some(identity), None);
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(RoutingFactory {
            name: "owner",
            declaration: declaration.clone(),
            sibling_declaration: None,
            accepted: None,
            validate_calls: Arc::clone(&calls),
            seen_values: Some(Arc::clone(&seen_values)),
        })];
        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        let lookup = test_lookup_for_owner(&resolver, &registry, "owner");

        for value in [
            Value::Concrete(ConcreteValue::Int(60)),
            Value::Concrete(ConcreteValue::Int(-61)),
            Value::Concrete(ConcreteValue::Float(crate::finite(1.25))),
        ] {
            lookup(&admit_declaration(declaration.clone()), &value)
                .expect("numeric scalar must bridge");
        }

        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(*seen_values.lock().unwrap(), ["60", "-61", "1.25"]);
    }

    #[test]
    fn issue_4276_round1_provider_config_invalid_attribute_keeps_main_text() {
        let declaration = PatternAttributeType::refined_string(
            Some(TypeIdentity::bare("Short")),
            None,
            Some((None, Some(3))),
            None,
        );
        let factory = RoutingFactory {
            name: "owner",
            declaration: declaration.clone(),
            sibling_declaration: None,
            accepted: Some("valid"),
            validate_calls: Arc::default(),
            seen_values: None,
        };
        let schema = crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
            "values".into(),
            PatternAttributeType::list(declaration),
        )]))
        .unwrap();
        let attributes: IndexMap<String, Value> = IndexMap::from([(
            "values".into(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::Concrete(ConcreteValue::String("abcd".into())),
                Value::Concrete(ConcreteValue::String("bad".into())),
            ])),
        )]);
        let factory = crate::test_utils::configuration::ConfigFactory::new(schema)
            .with_custom_validation(move |identity, value, inputs| {
                factory.validate_custom_type(identity, value, inputs)
            });
        let error = validate_and_canonicalize_provider_config(
            &factory,
            "owner",
            &((attributes.clone()).into()),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ProviderConfigPreparationError::InvalidAttribute { .. }
        ));
        assert_eq!(
            error.to_string(),
            "provider owner: values: List item at index 0: Invalid Short value 'abcd': length 4 is outside allowed range unbounded..=3"
        );
    }

    #[test]
    fn combined_walk_canonicalization_errors_keep_owner_attribution_at_every_entry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let declaration = PatternAttributeType::refined_float(
            Some(TypeIdentity::new(Some("shared"), ["number"], "Number")),
            None,
        );
        let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(RoutingFactory {
            name: "owner",
            declaration: declaration.clone(),
            sibling_declaration: None,
            accepted: None,
            validate_calls: Arc::clone(&calls),
            seen_values: None,
        })];
        let declaration = admit_declaration(declaration);
        let value = Value::Concrete(ConcreteValue::Int(9_007_199_254_740_993));
        let expected = TypeError::from(
            crate::provider::validate_custom_type_declaration_with_factory(
                factories[0].as_ref(),
                crate::parser::IdentifiedCustomTypeDeclaration::new(&declaration).unwrap(),
                &value,
            )
            .unwrap_err(),
        );
        let registry = test_schema_registry(&factories);
        let schema = registry
            .get("owner", "test.owner", crate::schema::SchemaKind::Resource)
            .unwrap();
        let resolver = test_schema_path_resolver(&factories);
        let attributes = HashMap::from([("value".to_string(), value.clone())]);
        for errors in [
            schema.schema().validate_with_origins_and_lookup(
                &crate::schema::classified_attributes(&attributes).unwrap(),
                &|_| false,
                &crate::parser::custom_type_lookup_for_schema(&resolver, schema),
            ),
            crate::schema::tests::resolved_admission_for_test(
                schema.schema().as_schema(),
                &crate::schema::classified_attributes(&attributes).unwrap(),
                None,
                &|_| false,
                &crate::parser::custom_type_lookup_for_schema(&resolver, schema),
            ),
            crate::schema::tests::resolved_admission_for_test(
                schema.schema().as_schema(),
                &crate::schema::classified_attributes(&attributes).unwrap(),
                Some(&HashSet::from(["value".to_string()])),
                &|_| false,
                &crate::parser::custom_type_lookup_for_schema(&resolver, schema),
            ),
        ] {
            assert_eq!(
                errors.unwrap_err(),
                vec![expected.clone().with_attribute("value")]
            );
        }
        let inputs =
            crate::provider::ProviderValidationInputs::collect(factories[0].name(), [&value]);
        let lookup = crate::parser::custom_type_lookup_for_factory(factories[0].as_ref(), &inputs);
        assert_eq!(
            crate::schema::RootlessType::new(declaration)
                .validate_with_lookup(&value, &lookup)
                .unwrap_err(),
            vec![expected],
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn decoded_refinement_bridge_calls_only_the_owning_factory_once() {
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let declaration = PatternAttributeType::refined_string(Some(identity), None, None, None);
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(RoutingFactory {
                name: "owner",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::clone(&owner_calls),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "other",
                declaration: declaration.clone(),
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::clone(&other_calls),
                seen_values: None,
            }),
        ];
        let decoded = crate::protocol_convert::protocol_to_attribute_type(
            &carina_provider_protocol::types::AttributeType::String {
                identity_layer: carina_provider_protocol::types::StringIdentityLayer {
                    pattern: None,
                    length: None,
                    validate: None,
                    to_dsl: None,
                    identity: Some("shared.Identifier".to_string()),
                },
                member_layer: Default::default(),
            },
        )
        .expect("callback-free protocol refinement must decode");
        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        let lookup = test_lookup_for_owner(&resolver, &registry, "owner");

        crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
            "decoded".to_string(),
            decoded,
        )]))
        .unwrap()
        .rootless_type("decoded")
        .unwrap()
        .validate_with_lookup(
            &Value::<NoPlaceholder>::Concrete(ConcreteValue::String("value".into())),
            &lookup,
        )
        .expect("the owning bridge validator must accept the value");

        assert_eq!(owner_calls.load(Ordering::SeqCst), 1);
        assert_eq!(other_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn native_refinement_callback_runs_once_without_factory_fallback() {
        let native_calls = Arc::new(AtomicUsize::new(0));
        let owner_calls = Arc::new(AtomicUsize::new(0));
        let other_calls = Arc::new(AtomicUsize::new(0));
        let identity = TypeIdentity::new(Some("shared"), Vec::<String>::new(), "Identifier");
        let calls = Arc::clone(&native_calls);
        let native_type = PatternAttributeType::refined_string_with_validator(
            identity.clone(),
            None,
            None,
            legacy_validator(move |_value| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            None,
        );
        let factories: Vec<Box<dyn ProviderFactory>> = vec![
            Box::new(RoutingFactory {
                name: "owner",
                declaration: PatternAttributeType::refined_string(
                    Some(identity.clone()),
                    None,
                    None,
                    None,
                ),
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::clone(&owner_calls),
                seen_values: None,
            }),
            Box::new(RoutingFactory {
                name: "other",
                declaration: PatternAttributeType::refined_string(Some(identity), None, None, None),
                sibling_declaration: None,
                accepted: None,
                validate_calls: Arc::clone(&other_calls),
                seen_values: None,
            }),
        ];
        let registry = test_schema_registry(&factories);
        let resolver = test_schema_path_resolver(&factories);
        let lookup = test_lookup_for_owner(&resolver, &registry, "owner");

        RootlessType::new(native_type)
            .validate_with_lookup(
                &Value::<NoPlaceholder>::Concrete(ConcreteValue::String("value".into())),
                &lookup,
            )
            .expect("the native callback must accept the value");

        assert_eq!(native_calls.load(Ordering::SeqCst), 1);
        assert_eq!(owner_calls.load(Ordering::SeqCst), 0);
        assert_eq!(other_calls.load(Ordering::SeqCst), 0);
    }

    struct RefinedConfigFactory {
        validate_calls: AtomicUsize,
    }

    impl ProviderFactory for RefinedConfigFactory {
        fn name(&self) -> &str {
            "owner"
        }

        fn display_name(&self) -> &str {
            "refined config test provider"
        }

        fn validate_config(
            &self,
            _attributes: &CanonicalProviderConfigAttributes,
            _session: &crate::provider::ProviderValidationInputs,
        ) -> ProviderResult<()> {
            Ok(())
        }

        fn validate_custom_type(
            &self,
            _identity: &TypeIdentity,
            value: &crate::provider::CanonicalCustomTypeValue,
            _session: &crate::provider::ProviderValidationInputs,
        ) -> ProviderResult<()> {
            self.validate_calls.fetch_add(1, Ordering::SeqCst);
            if value.as_str() == "valid" {
                Ok(())
            } else {
                Err(ProviderError::invalid_input("invalid endpoint"))
            }
        }

        fn create_provider(
            &self,
            _config: &crate::provider::ProviderSession,
            _initialize_outcome: crate::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>> {
            unreachable!("config preparation tests do not create providers")
        }

        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
                    "endpoint".to_string(),
                    PatternAttributeType::refined_string(
                        Some(TypeIdentity::new(
                            Some("shared"),
                            Vec::<String>::new(),
                            "Endpoint",
                        )),
                        None,
                        None,
                        None,
                    ),
                )]))
                .expect("test provider config schema must be valid"),
                Default::default(),
                Vec::new(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            Vec::new()
        }
    }

    #[test]
    fn decoded_refined_provider_config_calls_its_factory_once() {
        let factory = RefinedConfigFactory {
            validate_calls: AtomicUsize::new(0),
        };
        let attributes = IndexMap::from([(
            "endpoint".to_string(),
            Value::Concrete(ConcreteValue::String("valid".into())),
        )]);

        validate_and_canonicalize_provider_config(
            &factory,
            "owner",
            &((attributes.clone()).into()),
        )
        .expect("the owning factory must validate its decoded refined config type");

        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn provider_config_lookup_routes_through_selected_factory_not_loose_name() {
        let factory = RefinedConfigFactory {
            validate_calls: AtomicUsize::new(0),
        };
        let attributes = IndexMap::from([(
            "endpoint".to_string(),
            Value::Concrete(ConcreteValue::String("valid".into())),
        )]);

        validate_and_canonicalize_provider_config(
            &factory,
            "display-alias",
            &((attributes.clone()).into()),
        )
        .expect("the already-selected factory is the provider-config routing authority");

        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    }

    impl ConfigFactory {
        fn successful() -> Self {
            Self {
                validate_calls: AtomicUsize::new(0),
                plugin_error: None,
            }
        }

        fn failing(message: &'static str) -> Self {
            Self {
                validate_calls: AtomicUsize::new(0),
                plugin_error: Some(ConfigFailure::Invalid(message)),
            }
        }

        fn timing_out(message: &'static str) -> Self {
            Self {
                validate_calls: AtomicUsize::new(0),
                plugin_error: Some(ConfigFailure::Timeout(message)),
            }
        }
    }

    impl ProviderFactory for ConfigFactory {
        fn name(&self) -> &str {
            "test"
        }

        fn display_name(&self) -> &str {
            "provider config preparation test provider"
        }

        fn validate_config(
            &self,
            _attributes: &CanonicalProviderConfigAttributes,
            _session: &crate::provider::ProviderValidationInputs,
        ) -> crate::provider::ProviderResult<()> {
            self.validate_calls.fetch_add(1, Ordering::SeqCst);
            match self.plugin_error {
                Some(ConfigFailure::Invalid(message)) => Err(ProviderError::invalid_input(message)),
                Some(ConfigFailure::Timeout(message)) => Err(ProviderError::timeout(message)
                    .with_host_cause(crate::provider::HostErrorCause::ValidationSuspended(
                        crate::provider::SuspendedValidationId::from_keyed_fingerprint([0_u8; 32]),
                    ))),
                None => Ok(()),
            }
        }

        fn create_provider(
            &self,
            _config: &crate::provider::ProviderSession,
            _initialize_outcome: crate::provider::InitializeOutcomeMint,
        ) -> BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>> {
            unreachable!("config preparation tests do not create providers")
        }

        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
                    "region".to_string(),
                    PatternAttributeType::refined_string(
                        None,
                        Some(crate::schema::PatternSpec::new(
                            r"^good-[a-z]+$".to_string(),
                            crate::schema::PatternMatch::Full,
                        )),
                        None,
                        None,
                    ),
                )]))
                .expect("test provider config schema must be valid"),
                Default::default(),
                Vec::new(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            Vec::new()
        }
    }

    fn config(value: Value) -> IndexMap<String, Value> {
        IndexMap::from([("region".to_string(), value)])
    }

    fn resolved_configs(attributes: IndexMap<String, Value>) -> ResolvedProviderConfigs {
        ResolvedProviderConfigs::from_test_configs(vec![crate::parser::ProviderConfig {
            name: "test".to_string(),
            attributes: (attributes).into(),
            default_tags: Default::default(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: Default::default(),
            binding: None,
        }])
        .expect("test provider attributes must satisfy the resolved-config invariant")
    }

    #[test]
    fn prepare_provider_ready_config_accepts_valid_attributes() {
        let factory = ConfigFactory::successful();
        let attributes = config(Value::Concrete(ConcreteValue::String("good-tokyo".into())));
        let resolved = resolved_configs(attributes.clone());

        let ready = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect("valid provider config must be prepared");

        assert_eq!(ready.attributes(), &attributes);
        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn prepare_provider_ready_config_has_no_region_without_schema_attribute() {
        let factory = RefinedConfigFactory {
            validate_calls: AtomicUsize::new(0),
        };
        let attributes = IndexMap::from([(
            "endpoint".to_string(),
            Value::Concrete(ConcreteValue::String("valid".into())),
        )]);
        let resolved = resolved_configs(attributes);

        let ready = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect("provider config without a region schema must be prepared");

        assert_eq!(ready.region(), None);
    }

    #[test]
    fn prepare_provider_ready_config_rejects_undeclared_region() {
        let factory = RefinedConfigFactory {
            validate_calls: AtomicUsize::new(0),
        };
        let resolved = resolved_configs(config(Value::Concrete(ConcreteValue::String(
            "us-west-2".into(),
        ))));

        let error = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect_err("undeclared attributes cannot reach the provider");

        assert!(error.to_string().contains("Unknown attribute 'region'"));
        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn prepare_provider_ready_config_region_preserves_literal_string() {
        struct StringRegionConfigFactory;

        impl ProviderFactory for StringRegionConfigFactory {
            fn name(&self) -> &str {
                "test"
            }

            fn display_name(&self) -> &str {
                "string region config test provider"
            }

            fn validate_config(
                &self,
                _attributes: &CanonicalProviderConfigAttributes,
                _session: &crate::provider::ProviderValidationInputs,
            ) -> ProviderResult<()> {
                Ok(())
            }

            fn create_provider(
                &self,
                _config: &crate::provider::ProviderSession,
                _initialize_outcome: crate::provider::InitializeOutcomeMint,
            ) -> BoxFuture<'_, ProviderResult<crate::provider::ProviderInitialization>>
            {
                unreachable!("config preparation tests do not create providers")
            }

            fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
                crate::schema::ValidatedProviderSchemaWitness::new(
                    self.schemas(),
                    ValidatedConfigSchema::validate(BTreeMap::from([(
                        "region".to_string(),
                        PatternAttributeType::string(),
                    )]))
                    .expect("test provider config schema must be valid"),
                    Default::default(),
                    Vec::new(),
                )
                .unwrap()
            }

            fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
                Vec::new()
            }
        }

        let factory = StringRegionConfigFactory;
        for literal in ["us-west-2", "test.Region.us_west_2"] {
            let resolved = resolved_configs(config(Value::Concrete(ConcreteValue::String(
                literal.into(),
            ))));

            let ready = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
                .expect("literal region must be prepared");

            assert_eq!(ready.region(), Some(literal));
        }
    }

    #[test]
    fn prepare_provider_ready_config_region_uses_canonical_enum_api_value() {
        let resolved = resolved_configs(config(Value::Concrete(ConcreteValue::CanonicalEnum(
            crate::resource::CanonicalEnumValue::new_for_test(
                crate::schema::enum_identity("Region", Some("test")),
                "ap-northeast-1",
            ),
        ))));

        let ready =
            prepare_provider_ready_config(&EnumConfigFactory, resolved.iter().next().unwrap())
                .expect("canonical region enum must be prepared");

        assert_eq!(ready.region(), Some("ap-northeast-1"));
    }

    #[test]
    fn prepare_provider_ready_config_has_no_region_when_attribute_is_omitted() {
        let factory = ConfigFactory::successful();
        let resolved = resolved_configs(IndexMap::new());

        let ready = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect("provider config with an omitted region must be prepared");

        assert_eq!(ready.region(), None);
    }

    #[test]
    fn parser_provider_config_skips_unexpandable_placeholder_without_panicking() {
        let factory = ConfigFactory::successful();
        let attributes = IndexMap::from([(
            "region".to_string(),
            Value::Deferred(DeferredValue::Placeholder(
                ParserPlaceholder::EmptyInterpolation {
                    source: crate::resource::DiagnosticSourceSpan::authored(
                        "test.crn".into(),
                        1,
                        1,
                        1,
                        4,
                    ),
                },
            )),
        )]);

        let canonical =
            validate_parser_provider_config(&factory, "test", &((attributes.clone()).into()))
                .expect("placeholder leaves are unknown during parser-stage validation");

        assert!(canonical.attributes().is_empty());
        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn prepare_provider_ready_config_canonicalizes_enum_before_plugin_validation() {
        let attributes = config(Value::Concrete(ConcreteValue::enum_identifier(
            "test.Region.ap_northeast_1",
        )));
        let resolved = resolved_configs(attributes);

        let ready =
            prepare_provider_ready_config(&EnumConfigFactory, resolved.iter().next().unwrap())
                .expect("provider config enum must be canonicalized before validation");

        assert!(matches!(
            ready.attributes().get("region"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.api_value() == "ap-northeast-1"
        ));
        assert_eq!(ready.region(), Some("ap-northeast-1"));
    }

    #[test]
    fn prepare_provider_ready_config_rejects_pattern_violation() {
        let factory = ConfigFactory::successful();
        let attributes = config(Value::Concrete(ConcreteValue::String("bad".into())));
        let resolved = resolved_configs(attributes);

        let error = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect_err("pattern mismatch must fail preparation");
        let message = error.to_string();

        assert!(message.contains("provider test"), "{message}");
        assert!(message.contains("region"), "{message}");
        assert!(message.contains(r"^good-[a-z]+$"), "{message}");
        assert!(std::error::Error::source(&error).is_none());
        assert_eq!(message.matches(r"^good-[a-z]+$").count(), 1, "{message}");
        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn prepare_provider_ready_config_rejects_plugin_validation_failure() {
        let factory = ConfigFactory::failing("configured account is forbidden");
        let attributes = config(Value::Concrete(ConcreteValue::String("good-tokyo".into())));
        let resolved = resolved_configs(attributes);

        let error = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect_err("plugin config validation must fail preparation");
        let message = error.to_string();

        assert!(message.contains("provider test"), "{message}");
        assert!(
            message.contains("configured account is forbidden"),
            "{message}"
        );
        assert!(std::error::Error::source(&error).is_none());
        assert_eq!(
            message.matches("configured account is forbidden").count(),
            1,
            "{message}"
        );
        assert_eq!(factory.validate_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn prepare_provider_ready_config_masks_plugin_errors_for_secrets() {
        let plaintext = "plaintext-must-not-leak";
        let factory = ConfigFactory::failing(plaintext);
        let attributes = config(Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String("good-secret".into()))).into(),
        )));
        let resolved = resolved_configs(attributes);

        let error = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect_err("plugin config validation must fail preparation");
        let message = error.to_string();

        assert!(message.contains("provider test"), "{message}");
        assert!(message.contains("secret"), "{message}");
        assert!(!message.contains(plaintext), "{message}");
    }

    #[test]
    fn secret_error_redaction_preserves_provider_error_kind() {
        let plaintext = "plaintext-timeout-must-not-leak";
        let factory = ConfigFactory::timing_out(plaintext);
        let attributes = config(Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String("good-secret".into()))).into(),
        )));
        let resolved = resolved_configs(attributes);

        let error = prepare_provider_ready_config(&factory, resolved.iter().next().unwrap())
            .expect_err("plugin config validation must fail preparation");
        let provider_error = error
            .provider_error()
            .expect("provider validation errors retain their typed error");

        assert_eq!(
            provider_error.kind(),
            crate::provider::ProviderErrorKind::Timeout
        );
        assert!(matches!(
            provider_error.detail().host_cause(),
            Some(crate::provider::HostErrorCause::ValidationSuspended(_))
        ));
        assert!(!error.to_string().contains(plaintext));
    }

    #[test]
    fn provider_wire_projects_secret_builtin_results_to_plaintext() {
        let value = Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String(
                "prefix-decrypted:ciphertext-suffix".into(),
            )))
            .into(),
        ));
        let ready = crate::provider_wire::WireReady::try_new(
            IndexMap::from([("password".into(), value.clone())]),
            crate::provider_wire::ProviderWireTarget::Configuration {
                provider: "test".into(),
            },
        )
        .unwrap();
        assert_eq!(ready.as_inner()["password"], value);
        let wire = ready.wire().get("password").unwrap();
        assert!(wire.is_secret());
        assert_eq!(
            wire.value(),
            &carina_provider_protocol::types::Value::String(
                "prefix-decrypted:ciphertext-suffix".into(),
            )
        );
    }

    #[test]
    fn data_source_gate_rejects_coalesce_with_known_collection_children() {
        let bindings = ResolvedBindings::default();
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = SchemaRegistry::new();
        crate::validation::deferred_types::with_session_for_test(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            |type_checks| {
                let context = ProviderPreparationContext::for_test(
                    &bindings,
                    &module_gate,
                    &[],
                    &crate::provider::NoopNormalizer,
                    &schemas,
                    type_checks,
                );
                for value in crate::resource::known_value_gate_tests::coalesces() {
                    let source = DataSource::with_provider("test", "lookup", "example", None)
                        .with_attribute("value", value);
                    let error = prepare_provider_ready_data_source(source, &context).unwrap_err();
                    assert!(matches!(
                        error,
                        ProviderPreparationError::Serialization(
                            SerializationError::UnresolvedCoalesce { .. }
                        )
                    ));
                }
            },
        );
    }

    #[tokio::test]
    async fn provider_dispatch_gates_use_apply_binding_evidence() {
        use crate::resource::knowledge::{KnowledgeDiagnostic, KnowledgeError};
        let value = Value::root_resource_ref("cycle", "value", vec![]);
        let mut bindings = ResolvedBindings::default();
        bindings.set(
            "cycle",
            HashMap::from([("value".into(), value.clone())]),
            crate::binding_index::BindingValueSource::Local,
        );
        let module_gate = ModuleConstraintGate::from_live_compositions(&[]);
        let schemas = SchemaRegistry::new();
        crate::validation::deferred_types::with_session_for_test_async(
            &crate::parser::LoadedProviderContext::empty_for_test(),
            async |type_checks| {
                let context = ProviderPreparationContext::for_test(
                    &bindings, &module_gate, &[], &crate::provider::NoopNormalizer, &schemas, type_checks,
                );
                let managed = Resource::with_provider("test", "widget", "example", None)
                    .with_attribute("value", value.clone());
                let source = DataSource::with_provider("test", "lookup", "example", None)
                    .with_attribute("value", value.clone());
                let managed_error = prepare_provider_ready_resource(managed, &context).await.unwrap_err();
                let source_error = prepare_provider_ready_data_source(source, &context).unwrap_err();
                for error in [managed_error, source_error] {
                    assert!(matches!(error,
                        ProviderPreparationError::Serialization(SerializationError::Knowledge { error, .. })
                            if matches!(*error, KnowledgeDiagnostic::Absolute(KnowledgeError::Cycle { .. }))
                    ), "dispatch must classify with the supplied apply bindings");
                }
            },
        ).await;
    }

    #[test]
    fn provider_boundary_rejects_unresolved_secret_coalesce() {
        let secret = |value: &str| {
            Value::Deferred(DeferredValue::Secret(
                Box::new(Value::Concrete(ConcreteValue::String(value.into()))).into(),
            ))
        };
        let value = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(secret("left")),
            default: Box::new(secret("right")),
        });
        assert!(matches!(
            value
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification())
                .map_err(crate::resource::knowledge_serialization_error)
                .and_then(crate::resource::require_known_value),
            Err(crate::value::SerializationError::UnresolvedCoalesce { .. })
        ));
    }
}
