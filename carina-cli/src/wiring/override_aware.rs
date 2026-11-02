//! Canonical resource pairs and resolved resources with permanent name overrides applied.

use carina_core::resource::{PendingResourceIdentityError, ResolvedResourceId};
use std::collections::{HashMap, HashSet};

use carina_core::binding_index::{LivePreApplyInputs, ManagedBindingStability, ResolvedBindings};
use carina_core::config_loader::UnresolvedConfigSnapshot;
use carina_core::data_source_value::DataSourceValues;
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{DeferredCreateTargets, DeferredForExpansionChoice};
use carina_core::effect::deps::UnresolvedResource;
use carina_core::name_override::{
    ApplyDecision, NameOverride, NameOverrideSource, should_apply_override,
};
use carina_core::parser::{File, ResolvedDeferredForPhase};
use carina_core::resource::{
    AbsoluteBindingAddress, ConcreteValue, LoopSlot, ModuleSource, NoPlaceholder,
    ParserPlaceholder, PlaceholderKind, Resource, ResourceId, Value,
};
use carina_core::schema::{BlockNameConflict, SchemaRegistry};

use super::ProjectedStateRead;

/// Evaluated and pre-value-resolution resources whose provider-schema block
/// aliases have been folded into canonical attribute names together.
///
/// The owned file and unresolved resource snapshot are deliberately private.
/// Production construction is limited to the paired schema block-name
/// resolver, and [`OverrideAwareResources`] consumes this type instead of a
/// raw `Vec<Resource>` so neither parser snapshot can bypass that resolver.
///
/// An arbitrary evaluated resource vector cannot replace the resolver-owned
/// evaluated configuration:
///
/// ```compile_fail,E0616
/// # use carina_cli::wiring::CanonicalResourcePair;
/// # use carina_core::resource::Resource;
/// # fn bypass(mut pair: CanonicalResourcePair<()>, arbitrary: Vec<Resource>) {
/// pair.parsed.resources = arbitrary;
/// # }
/// ```
///
/// The pair cannot be minted outside validation wiring, except through the
/// `cfg(test)`/`test-utils`-gated `resolve_config_block_names_for_test` helper:
///
/// ```compile_fail,E0624
/// use carina_cli::wiring::CanonicalResourcePair;
/// use carina_core::parser::File;
///
/// let _ = CanonicalResourcePair::new(File::<()>::default(), Vec::new());
/// ```
///
/// The validation-wiring block-name resolver is also private:
///
/// ```compile_fail,E0624
/// use carina_cli::wiring::CanonicalResourcePair;
///
/// let _ = CanonicalResourcePair::<()>::resolve_block_names(
///     todo!(),
///     todo!(),
///     todo!(),
///     todo!(),
/// );
/// ```
#[derive(Debug, Clone)]
pub struct CanonicalResourcePair<
    E,
    P: PlaceholderKind = NoPlaceholder,
    T: PlaceholderKind = LoopSlot,
> {
    parsed: File<
        E,
        P,
        T,
        ResolvedDeferredForPhase<P, T, AbsoluteBindingAddress>,
        AbsoluteBindingAddress,
    >,
    unresolved: Vec<Resource<ModuleSource, P>>,
    /// Generated prefix values captured before persisted state can replace
    /// them with a pending re-read unknown. A post-refresh reconciliation
    /// restores these fallbacks first, then overlays only the refreshed
    /// current value or refreshed marker.
    prefix_fallbacks: Vec<HashMap<String, Value<P>>>,
}

pub(crate) type ParserCanonicalResourcePair<E> =
    CanonicalResourcePair<E, ParserPlaceholder, ParserPlaceholder>;

type CanonicalResourceParts<E, P, T> = (
    File<E, P, T, ResolvedDeferredForPhase<P, T, AbsoluteBindingAddress>, AbsoluteBindingAddress>,
    Vec<Resource<ModuleSource, P>>,
);

#[cfg(test)]
impl<E: carina_core::parser::IntoExpandedExportParam<AbsoluteBindingAddress>>
    ParserCanonicalResourcePair<E>
{
    pub(crate) fn into_expanded_for_test(
        self,
    ) -> Result<CanonicalResourcePair<E::Expanded>, carina_core::resource::ExpansionError> {
        let Self {
            parsed,
            unresolved,
            prefix_fallbacks,
        } = self;
        let unresolved = unresolved
            .into_iter()
            .map(|resource| resource.into_expanded(&carina_core::schema::SchemaRegistry::new()))
            .collect::<Result<_, _>>()?;
        let prefix_fallbacks = prefix_fallbacks
            .into_iter()
            .map(|attributes| carina_core::resource::into_expanded_value_map(attributes, None))
            .collect::<Result<_, _>>()
            .map_err(carina_core::resource::LeftoverPlaceholders::from)?;
        Ok(CanonicalResourcePair {
            parsed: parsed.into_expanded(&carina_core::schema::SchemaRegistry::new())?,
            unresolved,
            prefix_fallbacks,
        })
    }
}

/// Failure while canonicalizing resource values and assigning anonymous
/// identities inside a [`CanonicalResourcePair`].
#[derive(Debug, thiserror::Error)]
pub enum CanonicalResourceIdentityError {
    #[error(transparent)]
    EnumCanonicalization(#[from] carina_core::value::EnumCanonicalizationError),
    #[error(transparent)]
    ProviderConfig(#[from] carina_core::value::ProviderConfigCanonicalizationError),
    #[error(transparent)]
    Identity(#[from] carina_core::identifier::AnonymousIdentityError),
}

impl<E, P: PlaceholderKind, T: PlaceholderKind> CanonicalResourcePair<E, P, T> {
    fn new(
        parsed: File<
            E,
            P,
            T,
            ResolvedDeferredForPhase<P, T, AbsoluteBindingAddress>,
            AbsoluteBindingAddress,
        >,
        unresolved: Vec<Resource<ModuleSource, P>>,
    ) -> Self {
        Self {
            parsed,
            unresolved,
            prefix_fallbacks: Vec::new(),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn for_test(
        parsed: File<
            E,
            P,
            T,
            ResolvedDeferredForPhase<P, T, AbsoluteBindingAddress>,
            AbsoluteBindingAddress,
        >,
        unresolved: Vec<Resource<ModuleSource, P>>,
    ) -> Self {
        Self::new(parsed, unresolved)
    }

    pub(super) fn into_parts(self) -> CanonicalResourceParts<E, P, T> {
        (self.parsed, self.unresolved)
    }

    /// The canonical evaluated configuration. Callers borrow every non-resource
    /// input from this file so it cannot drift from the resources consumed by
    /// [`OverrideAwareResources`].
    pub fn parsed(
        &self,
    ) -> &File<
        E,
        P,
        T,
        ResolvedDeferredForPhase<P, T, AbsoluteBindingAddress>,
        AbsoluteBindingAddress,
    > {
        &self.parsed
    }

    pub fn unresolved(&self) -> &[Resource<ModuleSource, P>] {
        &self.unresolved
    }

    /// Resolve `<attribute>_prefix` meta-attributes without releasing the
    /// canonical configuration back into an untyped `File`.
    pub fn resolve_attr_prefixes(
        mut self,
        registry: &carina_core::schema::SchemaRegistry,
    ) -> Result<Self, String> {
        carina_core::identifier::resolve_attr_prefixes(&mut self.parsed.resources, registry)?;
        Ok(self)
    }

    /// Canonicalize values and assign anonymous identities without exposing
    /// the mutable resource slice held by the pair.
    pub fn canonicalize_and_compute_anonymous_identifiers(
        &mut self,
        registry: &carina_core::schema::SchemaRegistry,
        providers: &[carina_core::parser::ProviderConfig<P, AbsoluteBindingAddress>],
        factories: &[Box<dyn carina_core::provider::ProviderFactory>],
        conflict_policy: carina_core::identifier::AnonymousIdentityConflictPolicy,
    ) -> Result<(), CanonicalResourceIdentityError> {
        let resources = carina_core::value::canonicalize_resources_with_schemas(
            &mut self.parsed.resources,
            registry,
        )?;
        carina_core::identifier::compute_anonymous_identifiers_with_factories(
            resources,
            providers,
            factories,
            registry,
            conflict_policy,
        )
        .map_err(|error| match error {
            carina_core::identifier::AnonymousIdentityComputationError::ProviderConfig(error) => {
                CanonicalResourceIdentityError::ProviderConfig(error)
            }
            carina_core::identifier::AnonymousIdentityComputationError::Identity(error) => {
                CanonicalResourceIdentityError::Identity(error)
            }
        })
    }
}

impl<E> CanonicalResourcePair<E, ParserPlaceholder, ParserPlaceholder> {
    pub(super) fn resolve_block_names(
        _access: &super::ValidationPipelineAccess,
        parsed: carina_core::parser::ModuleResolvedFile<E>,
        unresolved: UnresolvedConfigSnapshot,
        registry: &SchemaRegistry,
    ) -> Result<Self, Vec<BlockNameConflict>> {
        let (parsed, unresolved) =
            carina_core::schema::resolve_config_block_names(parsed, unresolved, registry)?;
        Ok(Self::new(parsed, unresolved))
    }
}

impl<E> CanonicalResourcePair<E> {
    pub(super) fn from_resolved_file(
        parsed: carina_core::validation::ResolvedFile<E>,
        unresolved: Vec<Resource>,
    ) -> Self {
        Self::new(parsed.into_file(), unresolved)
    }

    /// Dependency-sort the canonical evaluated resources without releasing
    /// the owned file or accepting a replacement resource vector.
    pub fn sorted_resources(&self) -> Result<Vec<Resource>, String> {
        sort_resources_by_dependencies(&self.parsed.resources)
    }

    /// Reconcile generated prefix values against state while retaining the
    /// resolver-owned evaluated file.
    pub fn reconcile_prefixed_names(
        &mut self,
        find_state: &dyn Fn(&str, &str, &str) -> Option<carina_core::identifier::PrefixStateInfo>,
    ) {
        let resource_count = self.parsed.resources.len(); // allow: direct — plan-time reconciliation
        if self.prefix_fallbacks.len() != resource_count {
            self.prefix_fallbacks = self
                .parsed
                .resources
                .iter()
                .map(|resource| {
                    resource
                        .prefixes
                        .keys()
                        .filter_map(|attribute| {
                            resource
                                .attributes
                                .get(attribute)
                                .cloned()
                                .map(|value| (attribute.clone(), value))
                        })
                        .collect()
                })
                .collect();
        }
        carina_core::identifier::reconcile_prefixed_names(&mut self.parsed.resources, find_state);
    }

    /// Rebuild prefix-owned desired values from the post-hydration current
    /// snapshot. Restoring the generated fallback before overlaying current
    /// or persisted state guarantees that clearing a marker cannot retain an
    /// unknown that came from the earlier identity pass, while an omitted
    /// readback still reuses the stored generated name.
    pub fn reconcile_refreshed_prefixed_names(
        &mut self,
        find_state: &dyn Fn(&str, &str, &str) -> Option<carina_core::identifier::PrefixStateInfo>,
    ) {
        let resources = &mut self.parsed.resources; // allow: direct — plan-time reconciliation
        for (resource, fallbacks) in resources.iter_mut().zip(&self.prefix_fallbacks) {
            for (attribute, value) in fallbacks {
                resource.set_attr(attribute.clone(), value.clone());
            }
        }
        carina_core::identifier::reconcile_prefixed_names(&mut self.parsed.resources, find_state);
    }

    /// Reconcile anonymous module-instance addresses against state without
    /// exposing a mutable resource vector.
    pub fn reconcile_anonymous_module_instances(
        &mut self,
        find_state_names_by_type: &dyn Fn(&str, &str) -> Vec<String>,
        claims: &carina_core::identifier::StateBlockClaims,
    ) where
        E: carina_core::parser::ExportParamLike<NoPlaceholder, AbsoluteBindingAddress>,
    {
        carina_core::module_resolver::reconcile_anonymous_module_instances(
            &mut self.parsed,
            find_state_names_by_type,
            claims,
        );
    }

    /// Partition and materialize this pair's own deferred-for templates as one
    /// atomic update of the owned evaluated file. Generated children and
    /// retained templates both inherit canonical attribute names from the
    /// already-resolved file.
    pub fn expand_deferred_for_expressions<F>(
        &mut self,
        bindings: &carina_core::binding_index::IterableBindings<'_>,
        type_checks: &dyn carina_core::validation::deferred_types::ProviderTypeCheckContext,
        classify: F,
    ) -> Result<
        (
            carina_core::parser::ResolvedDeferredForExpressions,
            DeferredCreateTargets,
        ),
        carina_core::parser::DeferredForExpansionError,
    >
    where
        F: FnMut(&carina_core::parser::DeferredForExpression) -> DeferredForExpansionChoice,
    {
        let (selected, retained) = DeferredCreateTargets::partition_for_expansion(
            self.parsed.take_deferred_for_expressions(),
            classify,
        );
        self.parsed.warnings.retain(|warning| {
            warning.kind != carina_core::parser::WarningKind::DeferredFor
                || !retained
                    .templates()
                    .iter()
                    .any(|deferred| deferred.location().matches_deferred_warning(warning))
        });
        self.parsed.replace_deferred_for_expressions(selected);
        self.parsed
            .expand_deferred_for_expressions(bindings, type_checks)?;
        let residual = self.parsed.deferred_for_expressions().clone();
        self.parsed
            .deferred_for_expressions_mut()
            .append_finalized(retained.templates().clone());
        Ok((residual, retained))
    }

    /// Run the provider-bound desired normalization pipeline on the owned
    /// evaluated resources and data sources.
    pub async fn prepare_desired_side_for_plan(
        &mut self,
        provider_configs: &[carina_core::parser::ProviderConfig<
            NoPlaceholder,
            AbsoluteBindingAddress,
        >],
        normalizer: &dyn carina_core::provider::ProviderNormalizer,
        schemas: &carina_core::schema::SchemaRegistry,
    ) -> carina_core::provider::ProviderResult<()> {
        carina_core::value::canonicalize_data_sources_with_schemas(
            &mut self.parsed.data_sources,
            schemas,
        )?;
        carina_core::executor::normalized::apply_desired_normalization_slice(
            &mut self.parsed.resources,
            provider_configs,
            normalizer,
            schemas,
        )
        .await
    }

    fn resources_for_build(&self) -> Result<(Vec<Resource>, Vec<Resource>), String> {
        Ok((self.sorted_resources()?, self.unresolved.clone()))
    }
}

/// Two paired resources resolved to the same identity before execution.
#[derive(Debug, thiserror::Error)]
pub enum ResolvedResourceOriginIndexError {
    #[error(transparent)]
    PendingIdentity(#[from] PendingResourceIdentityError),
    #[error("override-aware resources contain duplicate resolved id {id}")]
    Duplicate { id: ResolvedResourceId },
}

/// Reference-resolution mode for an evaluated/authored resource pair.
#[derive(Debug, Clone, Copy)]
pub enum OverrideAwareBuildMode<'a> {
    Apply,
    Plan {
        unresolved_upstream_bindings: &'a HashSet<&'a str>,
    },
}

/// Resources whose evaluated and authored inputs crossed schema block-name
/// resolution together.
///
/// Raw resource vectors cannot be passed to the constructor:
///
/// ```compile_fail,E0308
/// # use std::collections::HashMap;
/// # use carina_core::binding_index::LivePreApplyInputs;
/// # use carina_cli::wiring::{OverrideAwareBuildMode, OverrideAwareResources};
/// # use carina_core::resource::Resource;
/// # fn bypass(raw: Vec<Resource>, inputs: LivePreApplyInputs<'_>) {
/// let _ = OverrideAwareResources::build_pair_with_stability(
///     &raw,
///     todo!(),
///     inputs,
///     &HashMap::new(),
///     OverrideAwareBuildMode::Apply,
/// );
/// # }
/// ```
pub struct OverrideAwareResources {
    resources: Vec<Resource>,
    unresolved_resources: Vec<Resource>,
    bindings: ResolvedBindings,
    /// Resources whose override was applied from a v7 state (no
    /// recorded original_value). Sub-PR C surfaces this through a warning.
    legacy_overrides_applied: Vec<ResourceId>,
    /// Resources where the DSL value diverged from recorded original_value.
    /// Sub-PR C / plan summary can surface this through the getter.
    skipped_overrides: Vec<ResourceId>,
}

/// Binding sources that remain external to [`OverrideAwareResources`] after
/// late identity reconciliation. The managed set is deliberately absent: the
/// rebuilt view always uses the resource set owned by the receiver.
pub struct RebuildBindingsInputs<'a> {
    pub compositions: &'a [carina_core::resource::LiveComposition],
    pub managed_current: &'a HashMap<ResourceId, carina_core::resource::PlanInputCurrent>,
    pub data_source_values: &'a DataSourceValues,
    pub remote_bindings: &'a HashMap<String, HashMap<String, Value>>,
    pub wait_aliases: &'a [carina_core::binding_index::WaitAliasSpec],
}

impl OverrideAwareResources {
    /// Build evaluated and authored views from the opaque pair returned by the
    /// schema block-name resolver.
    pub(crate) fn build_pair_with_stability<E>(
        resources: &CanonicalResourcePair<E>,
        state: &ProjectedStateRead<'_>,
        inputs: LivePreApplyInputs<'_>,
        managed_stability: &HashMap<ResourceId, ManagedBindingStability>,
        mode: OverrideAwareBuildMode<'_>,
        session: &super::ValidationProviderCatalog,
    ) -> Result<(Self, Self), String> {
        let (evaluated, unresolved) = resources.resources_for_build()?;
        let evaluated =
            Self::build_inner(evaluated, state, &inputs, managed_stability, mode, session)?;
        let unresolved =
            Self::build_inner(unresolved, state, &inputs, managed_stability, mode, session)?;
        Ok((evaluated, unresolved))
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn build_pair_without_state_for_test<E>(
        resources: &CanonicalResourcePair<E>,
        inputs: LivePreApplyInputs<'_>,
        managed_stability: &HashMap<ResourceId, ManagedBindingStability>,
        mode: OverrideAwareBuildMode<'_>,
    ) -> Result<(Self, Self), String> {
        let state = ProjectedStateRead::for_test_unmoved(None);
        carina_core::validation::deferred_types::with_session_for_test(
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                Self::build_pair_with_stability(
                    resources,
                    &state,
                    inputs,
                    managed_stability,
                    mode,
                    session,
                )
            },
        )
    }

    fn build_inner(
        unresolved: Vec<Resource>,
        state: &ProjectedStateRead<'_>,
        inputs: &LivePreApplyInputs<'_>,
        managed_stability: &HashMap<ResourceId, ManagedBindingStability>,
        mode: OverrideAwareBuildMode<'_>,
        session: &super::ValidationProviderCatalog,
    ) -> Result<Self, String> {
        let mut resources = unresolved.clone();
        let mut bindings = ResolvedBindings::pre_apply_live_with_stability(
            LivePreApplyInputs {
                managed: &resources,
                compositions: inputs.compositions,
                managed_current: inputs.managed_current,
                data_source_values: inputs.data_source_values,
                remote_bindings: inputs.remote_bindings,
                wait_aliases: inputs.wait_aliases,
            },
            managed_stability,
        );
        resolve_resources(&mut resources, &bindings, &mode, session)?;

        let snapshot = resources.clone();
        let mut unresolved_resources = unresolved;
        let mut legacy_overrides_applied = Vec::new();
        let mut skipped_overrides = Vec::new();

        for ((resource, snapshot_resource), unresolved_resource) in resources
            .iter_mut()
            .zip(snapshot.iter())
            .zip(unresolved_resources.iter_mut())
        {
            let Some(name_overrides) = state.name_overrides_for(&resource.id) else {
                continue;
            };
            for (attribute, override_) in name_overrides {
                let snapshot_value = snapshot_resource
                    .attributes
                    .get(attribute)
                    .and_then(concrete_string);
                match should_apply_override(snapshot_value, override_) {
                    ApplyDecision::Apply | ApplyDecision::ApplyWithUnknownDsl => {
                        apply_override(resource, unresolved_resource, attribute, override_);
                    }
                    ApplyDecision::ApplyLegacy => {
                        apply_override(resource, unresolved_resource, attribute, override_);
                        push_unique(&mut legacy_overrides_applied, resource.id.clone());
                    }
                    ApplyDecision::Skip => {
                        push_unique(&mut skipped_overrides, resource.id.clone());
                    }
                }
            }
        }

        bindings = ResolvedBindings::pre_apply_live_with_stability(
            LivePreApplyInputs {
                managed: &resources,
                compositions: inputs.compositions,
                managed_current: inputs.managed_current,
                data_source_values: inputs.data_source_values,
                remote_bindings: inputs.remote_bindings,
                wait_aliases: inputs.wait_aliases,
            },
            managed_stability,
        );
        resolve_resources(&mut resources, &bindings, &mode, session)?;

        Ok(Self {
            resources,
            unresolved_resources,
            bindings,
            legacy_overrides_applied,
            skipped_overrides,
        })
    }

    pub fn resources(&self) -> &[Resource] {
        &self.resources
    }

    pub(crate) fn resources_mut(&mut self) -> &mut [Resource] {
        &mut self.resources
    }

    /// Return pre-resolution snapshots with IDs copied from their paired
    /// resolved resources.
    ///
    /// [`OverrideAwareResources`] constructs both vectors from the same input
    /// list and only mutates the resolved side during later identity passes.
    /// Zipping here preserves that construction pairing instead of trusting the
    /// unresolved snapshot's own identity.
    pub fn paired_unresolved_resources(&self) -> Vec<Resource> {
        assert_eq!(
            self.resources.len(),
            self.unresolved_resources.len(),
            "override-aware resources keep resolved and unresolved snapshots paired"
        );
        self.resources
            .iter()
            .zip(&self.unresolved_resources)
            .map(|(resource, unresolved)| paired_unresolved_resource(resource, unresolved))
            .collect()
    }

    pub fn unresolved_by_resolved_id(
        &self,
    ) -> Result<HashMap<ResolvedResourceId, UnresolvedResource>, ResolvedResourceOriginIndexError>
    {
        unresolved_map_from_paired(self.resources(), self.paired_unresolved_resources())
    }

    /// Like [`Self::paired_unresolved_resources`], but prefers a separately
    /// built pre-parser-resolution snapshot for named resources when it can be
    /// matched by binding.
    ///
    /// The separately built snapshot preserves attr-level `ResourceRef`s that
    /// parser resolution may have collapsed into `dependency_bindings`; update
    /// scheduling needs those known read attrs for update-update relaxation.
    /// Module expansion can change counts and anonymous resources have no
    /// unique binding to pair on, so those resources intentionally fall back to
    /// this object's construction pairing unless the source also carries the
    /// same module source.
    pub fn paired_unresolved_resources_with_binding_sources(
        &self,
        binding_sources: &OverrideAwareResources,
    ) -> Vec<Resource> {
        let mut by_binding: HashMap<&AbsoluteBindingAddress, &Resource> = HashMap::new();
        let mut duplicate_bindings: HashSet<&AbsoluteBindingAddress> = HashSet::new();
        for source in binding_sources.unresolved_resources() {
            let Some(binding) = source.binding.as_ref() else {
                continue;
            };
            if by_binding.insert(binding, source).is_some() {
                duplicate_bindings.insert(binding);
            }
        }

        self.resources()
            .iter()
            .zip(self.paired_unresolved_resources())
            .map(|(resource, fallback)| {
                let Some(binding) = resource.binding.as_ref() else {
                    return fallback;
                };
                if duplicate_bindings.contains(binding) {
                    return fallback;
                }
                let Some(source) = by_binding.get(binding) else {
                    return fallback;
                };
                if can_pair_by_binding(resource, source) {
                    paired_unresolved_resource(resource, source)
                } else {
                    fallback
                }
            })
            .collect()
    }

    pub fn unresolved_by_resolved_id_with_binding_sources(
        &self,
        binding_sources: &OverrideAwareResources,
    ) -> Result<HashMap<ResolvedResourceId, UnresolvedResource>, ResolvedResourceOriginIndexError>
    {
        let paired_unresolved =
            self.paired_unresolved_resources_with_binding_sources(binding_sources);
        unresolved_map_from_paired(self.resources(), paired_unresolved)
    }

    pub fn unresolved_resources(&self) -> &[Resource] {
        &self.unresolved_resources
    }

    pub fn bindings(&self) -> &ResolvedBindings {
        &self.bindings
    }

    pub fn apply_plan_stability(
        &mut self,
        managed_current: &HashMap<ResourceId, carina_core::resource::PlanInputCurrent>,
        plan: &carina_core::plan::Plan,
        schemas: &carina_core::schema::SchemaRegistry,
    ) {
        self.bindings
            .apply_plan_stability(&self.resources, managed_current, plan, schemas);
    }

    /// Rebuild the binding view after late identity reconciliation changes
    /// which provider states belong to the managed resource set.
    pub fn rebuild_bindings_after_identity_reconciliation(
        &mut self,
        inputs: RebuildBindingsInputs<'_>,
    ) {
        self.rebuild_bindings_after_identity_reconciliation_with_stability(inputs, &HashMap::new());
    }

    pub fn rebuild_bindings_after_identity_reconciliation_with_stability(
        &mut self,
        inputs: RebuildBindingsInputs<'_>,
        managed_stability: &HashMap<ResourceId, ManagedBindingStability>,
    ) {
        let RebuildBindingsInputs {
            compositions,
            managed_current,
            data_source_values,
            remote_bindings,
            wait_aliases,
        } = inputs;
        self.bindings = ResolvedBindings::pre_apply_live_with_stability(
            LivePreApplyInputs {
                managed: &self.resources,
                compositions,
                managed_current,
                data_source_values,
                remote_bindings,
                wait_aliases,
            },
            managed_stability,
        );
    }

    pub fn legacy_overrides_applied(&self) -> &[ResourceId] {
        &self.legacy_overrides_applied
    }

    pub fn skipped_overrides(&self) -> &[ResourceId] {
        &self.skipped_overrides
    }
}

fn paired_unresolved_resource(resource: &Resource, unresolved: &Resource) -> Resource {
    assert_eq!(
        resource.id.provider, unresolved.id.provider,
        "paired override-aware resources must keep provider order stable"
    );
    assert_eq!(
        resource.id.resource_type, unresolved.id.resource_type,
        "paired override-aware resources must keep type order stable"
    );
    assert_eq!(
        resource.id.provider_instance, unresolved.id.provider_instance,
        "paired override-aware resources must keep provider instance order stable"
    );
    let mut unresolved = unresolved.clone();
    unresolved.id = resource.id.clone();
    unresolved
}

fn can_pair_by_binding(resource: &Resource, source: &Resource) -> bool {
    resource.binding == source.binding
        && (resource.binding.is_some() || resource.loop_provenance == source.loop_provenance)
        && resource.module_source == source.module_source
        && resource.id.provider == source.id.provider
        && resource.id.resource_type == source.id.resource_type
        && resource.id.provider_instance == source.id.provider_instance
}

fn unresolved_map_from_paired(
    resources: &[Resource],
    paired_unresolved: Vec<Resource>,
) -> Result<HashMap<ResolvedResourceId, UnresolvedResource>, ResolvedResourceOriginIndexError> {
    assert_eq!(
        resources.len(),
        paired_unresolved.len(),
        "paired unresolved resources must match resolved resource count"
    );
    let mut by_id = HashMap::with_capacity(paired_unresolved.len());
    for (resource, unresolved) in resources.iter().zip(paired_unresolved) {
        let id = ResolvedResourceId::try_new(resource.id.clone())
            .ok_or_else(|| PendingResourceIdentityError::new(resource.id.clone()))?;
        let previous = by_id.insert(id.clone(), UnresolvedResource::from_pre_resolve(unresolved));
        if previous.is_some() {
            return Err(ResolvedResourceOriginIndexError::Duplicate { id });
        }
    }
    Ok(by_id)
}

fn resolve_resources(
    resources: &mut [Resource],
    bindings: &ResolvedBindings,
    mode: &OverrideAwareBuildMode<'_>,
    session: &super::ValidationProviderCatalog,
) -> Result<(), String> {
    match mode {
        OverrideAwareBuildMode::Apply => {
            carina_core::resolver::resolve_refs_with_state_and_remote(resources, bindings, session)
        }
        OverrideAwareBuildMode::Plan {
            unresolved_upstream_bindings,
        } => carina_core::resolver::resolve_refs_for_plan(
            resources,
            bindings,
            unresolved_upstream_bindings,
            session,
        ),
    }
}

fn concrete_string(value: &Value) -> Option<&str> {
    match value {
        Value::Concrete(ConcreteValue::String(value)) => Some(value.expose_plaintext()),
        _ => None,
    }
}

fn apply_override(
    resource: &mut Resource,
    unresolved_resource: &mut Resource,
    attribute: &str,
    override_: &NameOverride,
) {
    let value = Value::Concrete(ConcreteValue::String(override_.temp_value.clone().into()));
    resource
        .attributes
        .insert(attribute.to_string(), value.clone());
    unresolved_resource
        .attributes
        .insert(attribute.to_string(), value);
}

fn push_unique(resources: &mut Vec<ResourceId>, id: ResourceId) {
    if !resources.contains(&id) {
        resources.push(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use carina_core::binding_index::WaitAliasSpec;
    use carina_core::resource::{AccessPath, DeferredValue, PlanInputCurrent};

    fn absolute_binding(name: impl Into<String>) -> carina_core::resource::AbsoluteBindingAddress {
        carina_core::resource::absolute_binding_address_for_test(name)
    }

    fn string(value: impl Into<String>) -> Value {
        Value::Concrete(ConcreteValue::String(value.into().into()))
    }

    fn resource(binding: &'static str, name: Value) -> Resource {
        let mut resource = Resource::new("mock.thing", binding);
        resource.binding = Some(absolute_binding(binding));
        resource.attributes.insert("name".to_string(), name);
        resource
    }

    fn resource_ref(binding: &str, attribute: &str) -> Value {
        Value::Deferred(DeferredValue::ResourceRef {
            path: AccessPath::new(binding, attribute).resolve_root_for_test(),
        })
    }

    fn overrides(
        entries: Vec<(&ResourceId, &str, NameOverride)>,
    ) -> HashMap<ResourceId, HashMap<String, NameOverride>> {
        let mut overrides = HashMap::new();
        for (id, attribute, override_) in entries {
            overrides
                .entry(id.clone())
                .or_insert_with(HashMap::new)
                .insert(attribute.to_string(), override_);
        }
        overrides
    }

    fn build(
        resources: Vec<Resource>,
        overrides: &HashMap<ResourceId, HashMap<String, NameOverride>>,
    ) -> OverrideAwareResources {
        let compositions = [];
        let data_source_values = DataSourceValues::empty();
        let managed_current: HashMap<ResourceId, PlanInputCurrent> = HashMap::new();
        let remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        let wait_aliases: Vec<WaitAliasSpec> = Vec::new();

        let mut parsed = carina_core::parser::ExpandedParsedFile::default();
        parsed.resources = resources.clone();
        let canonical = CanonicalResourcePair::new(parsed, resources);
        let state = ProjectedStateRead::for_test_name_overrides(overrides);
        carina_core::validation::deferred_types::with_session_for_test(
            &carina_core::parser::LoadedProviderContext::empty_for_test(),
            |session| {
                OverrideAwareResources::build_pair_with_stability(
                    &canonical,
                    &state,
                    LivePreApplyInputs {
                        managed: &[],
                        compositions: &compositions,
                        managed_current: &managed_current,
                        data_source_values: &data_source_values,
                        remote_bindings: &remote_bindings,
                        wait_aliases: &wait_aliases,
                    },
                    &HashMap::new(),
                    OverrideAwareBuildMode::Apply,
                    session,
                )
            },
        )
        .map(|(evaluated, _)| evaluated)
        .expect("override-aware resources should build")
    }

    fn resolved_name(resources: &OverrideAwareResources, binding: &str) -> Option<String> {
        resources
            .resources()
            .iter()
            .find(|resource| {
                resource.binding.as_ref().map(|binding| binding.as_str()) == Some(binding)
            })
            .and_then(|resource| resource.attributes.get("name"))
            .and_then(|value| concrete_string(value).map(ToOwned::to_owned))
    }

    #[test]
    fn chained_cbd_consumer_reading_b_dot_name_resolves_to_b_override() {
        let upstream = resource("a", string("shared-name"));
        let consumer = resource("b", resource_ref("a", "name"));
        let overrides = overrides(vec![
            (
                &upstream.id,
                "name",
                NameOverride {
                    temp_value: "a-cbd".to_string(),
                    original_value: Some("shared-name".to_string()),
                },
            ),
            (
                &consumer.id,
                "name",
                NameOverride {
                    temp_value: "b-cbd".to_string(),
                    original_value: Some("shared-name".to_string()),
                },
            ),
        ]);

        let resources = build(vec![upstream, consumer], &overrides);

        assert_eq!(resolved_name(&resources, "a").as_deref(), Some("a-cbd"));
        assert_eq!(resolved_name(&resources, "b").as_deref(), Some("b-cbd"));
        assert!(resources.skipped_overrides().is_empty());
    }

    #[test]
    fn apply_name_overrides_applies_for_var_substituted_dsl_name() {
        let source = resource("var_foo", string("from-var"));
        let target = resource("target", resource_ref("var_foo", "name"));
        let overrides = overrides(vec![(
            &target.id,
            "name",
            NameOverride {
                temp_value: "target-cbd".to_string(),
                original_value: Some("from-var".to_string()),
            },
        )]);

        let resources = build(vec![source, target], &overrides);

        assert_eq!(
            resolved_name(&resources, "target").as_deref(),
            Some("target-cbd")
        );
        assert!(resources.skipped_overrides().is_empty());
    }

    #[test]
    fn apply_name_overrides_skips_for_ref_substituted_dsl_name_rename() {
        let source = resource("source", string("renamed"));
        let target = resource("target", resource_ref("source", "name"));
        let overrides = overrides(vec![(
            &target.id,
            "name",
            NameOverride {
                temp_value: "target-cbd".to_string(),
                original_value: Some("old-name".to_string()),
            },
        )]);

        let resources = build(vec![source, target.clone()], &overrides);

        assert_eq!(
            resolved_name(&resources, "target").as_deref(),
            Some("renamed")
        );
        assert_eq!(resources.skipped_overrides(), &[target.id]);
    }

    #[test]
    fn override_aware_resources_records_legacy_and_skipped() {
        let legacy = resource("legacy", string("legacy-current"));
        let skipped = resource("skipped", string("skipped-current"));
        let overrides = overrides(vec![
            (
                &legacy.id,
                "name",
                NameOverride {
                    temp_value: "legacy-cbd".to_string(),
                    original_value: None,
                },
            ),
            (
                &skipped.id,
                "name",
                NameOverride {
                    temp_value: "skipped-cbd".to_string(),
                    original_value: Some("recorded-original".to_string()),
                },
            ),
        ]);

        let resources = build(vec![legacy.clone(), skipped.clone()], &overrides);

        assert_eq!(resources.legacy_overrides_applied(), &[legacy.id]);
        assert_eq!(resources.skipped_overrides(), &[skipped.id]);
        assert_eq!(
            resolved_name(&resources, "legacy").as_deref(),
            Some("legacy-cbd")
        );
        assert_eq!(
            resolved_name(&resources, "skipped").as_deref(),
            Some("skipped-current")
        );
    }
}
