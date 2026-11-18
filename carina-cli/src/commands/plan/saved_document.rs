use super::*;
use carina_core::value::{Saved, SavedAuthored, SavedDocumentConversion};
/// Immutable admitted contents awaiting provider evidence.
/// Its constructor requires admitted values for every DSL-bearing field.
/// The transport view is read-only; recording consumes this owner.
///
/// ```compile_fail,E0451
/// use carina_cli::commands::plan::PlanFile;
///
/// fn rebuild(plan: PlanFile) -> PlanFile {
///     PlanFile { ..plan }
/// }
/// ```
///
/// ```compile_fail,E0277
/// use carina_cli::commands::plan::PlanFileContents;
///
/// fn serialize_unrecorded(contents: &PlanFileContents) {
///     let _ = serde_json::to_string(contents);
/// }
/// ```
///
/// Raw contents cannot record a production plan without input admission.
///
/// ```compile_fail,E0599
/// use carina_cli::commands::plan::PlanFileContents;
/// fn record_raw(contents: PlanFileContents) { let _ = contents.record(); }
/// ```
/// Saved-document fields require admission, including snapshots made without redaction.
///
/// ```compile_fail,E0308
/// use carina_cli::commands::plan::PlanFileContents;
/// use carina_core::value::Saved;
/// fn bypass(contents: &PlanFileContents) {
///     let resource = contents.sorted_resources[0].to_value_resource().unwrap();
///     let _: Saved<_> = resource.to_snapshot();
/// }
/// ```
///
/// ```compile_fail,E0603
/// use carina_core::value::redact_secrets_in_resource;
/// ```
/// ```compile_fail,E0603
/// use carina_core::value::redact_secrets_in_resource_reusing_persisted_hashes;
/// ```
/// ```compile_fail,E0603
/// use carina_core::value::redact_secrets_in_resource_using_evaluated;
/// ```
/// ```compile_fail,E0603
/// use carina_core::value::redact_secrets_in_data_source;
/// ```
/// ```compile_fail,E0603
/// use carina_core::value::redact_secrets_in_data_source_using_evaluated;
/// ```
/// ```compile_fail,E0603
/// use carina_core::value::redact_secrets_in_live_composition;
/// ```
/// ```compile_fail,E0603
/// use carina_core::value::redact_secrets_in_saved_composition;
/// ```
/// ```compile_fail,E0603
/// use carina_core::value::redact_secrets_in_value;
/// ```
/// ```compile_fail,E0603
/// use carina_core::value::redact_secrets_in_attributes;
/// ```
/// ```compile_fail,E0603
/// use carina_core::value::redact_secrets_using_evaluated;
/// ```
#[derive(Debug, Clone)]
pub struct PlanFileContents {
    transport: UnresolvedPlanFileTransport,
}
impl Deref for PlanFileContents {
    type Target = UnresolvedPlanFileTransport;
    fn deref(&self) -> &Self::Target {
        &self.transport
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(bound(deserialize = "S: serde::de::DeserializeOwned"))]
pub(super) struct Parts<S = ModuleSource> {
    pub version: u32,
    pub carina_version: String,
    pub timestamp: String,
    pub source_path: PersistedPath,
    pub source_identity: CanonicalPath,
    pub state_lineage: Option<String>,
    pub state_serial: Option<u64>,
    pub provider_configs: Saved<SavedProviderConfigs>,
    pub backend_config: Saved<Option<carina_core::parser::ExpandedBackendConfig>>,
    pub plan: Saved<UnresolvedSavedPlan>,
    pub sorted_resources: Vec<Saved<Resource<S, SnapshotLeaf, AbsoluteBindingAddress, ()>>>,
    #[serde(deserialize_with = "deserialize_persisted_destroy_dependency_vec")]
    pub sorted_value_destroy_dependencies: Vec<PersistedDestroyDependencies>,
    #[serde(deserialize_with = "deserialize_persisted_destroy_dependency_vec")]
    pub sorted_explicit_destroy_dependencies: Vec<PersistedDestroyDependencies>,
    pub legacy_dependency_classification: Option<LegacyDependencyClassification>,
    pub unresolved_resources:
        Vec<SavedAuthored<Resource<S, SnapshotLeaf, AbsoluteBindingAddress, ()>>>,
    pub compositions: Vec<Saved<SavedComposition>>,
    pub data_sources:
        Vec<Saved<carina_core::resource::DataSource<S, SnapshotLeaf, AbsoluteBindingAddress, ()>>>,
    pub data_source_origins:
        Vec<Saved<carina_core::resource::DataSource<S, SnapshotLeaf, AbsoluteBindingAddress, ()>>>,
    pub data_source_values: Vec<DataSourcePart>,
    pub current_states: Vec<CurrentStateEntry>,
    pub upstream_snapshot: BTreeMap<String, BTreeMap<String, Saved<SnapshotValue>>>,
    pub upstream_sources: Vec<UpstreamSource>,
    pub wait_bindings: Vec<PlanWaitBinding>,
}
#[derive(Debug, serde::Deserialize)]
pub(super) struct DataSourcePart {
    pub id: ResourceId,
    pub value: Saved<DataSourceValue>,
}
impl<S> Parts<S>
where
    Resource<S, SnapshotLeaf, AbsoluteBindingAddress, ()>:
        Into<Resource<SavedModuleSource, SnapshotLeaf, AbsoluteBindingAddress, ()>>,
    carina_core::resource::DataSource<S, SnapshotLeaf, AbsoluteBindingAddress, ()>: Into<
        carina_core::resource::DataSource<
            SavedModuleSource,
            SnapshotLeaf,
            AbsoluteBindingAddress,
            (),
        >,
    >,
{
    fn into_transport(self) -> UnresolvedPlanFileTransport {
        let Self {
            version,
            carina_version,
            timestamp,
            source_path,
            source_identity,
            state_lineage,
            state_serial,
            provider_configs,
            backend_config,
            plan,
            sorted_resources,
            sorted_value_destroy_dependencies,
            sorted_explicit_destroy_dependencies,
            legacy_dependency_classification,
            unresolved_resources,
            compositions,
            data_sources,
            data_source_origins,
            data_source_values,
            current_states,
            upstream_snapshot,
            upstream_sources,
            wait_bindings,
        } = self;
        PlanFileTransport {
            version,
            carina_version,
            timestamp,
            source_path,
            source_identity,
            state_lineage,
            state_serial,
            provider_configs: provider_configs.into_inner(),
            backend_config: backend_config.into_inner().into(),
            plan: plan.into_inner(),
            sorted_resources: sorted_resources
                .into_iter()
                .map(|resource| resource.into_inner().into())
                .collect(),
            sorted_value_destroy_dependencies,
            sorted_explicit_destroy_dependencies,
            legacy_dependency_classification,
            unresolved_resources: unresolved_resources
                .into_iter()
                .map(SavedAuthored::into_saved_source)
                .collect(),
            compositions: compositions.into_iter().map(Saved::into_inner).collect(),
            data_sources: data_sources
                .into_iter()
                .map(|resource| resource.into_inner().into())
                .collect(),
            data_source_origins: data_source_origins
                .into_iter()
                .map(|resource| resource.into_inner().into())
                .collect(),
            data_source_values: data_source_values
                .into_iter()
                .map(|value| SavedDataSourceValue {
                    id: value.id,
                    value: value.value.into_inner(),
                })
                .collect(),
            current_states,
            upstream_snapshot: upstream_snapshot
                .into_iter()
                .map(|(binding, attributes)| {
                    (
                        binding,
                        attributes
                            .into_iter()
                            .map(|(key, value)| (key, value.into_inner()))
                            .collect(),
                    )
                })
                .collect(),
            upstream_sources,
            wait_bindings,
        }
    }
}
impl PlanFileContents {
    pub(super) fn build<E, S, Error>(
        parsed: &carina_core::parser::ExpandedFile<E>,
        sources: &[&dyn carina_core::value::DocumentTree],
        build: impl FnOnce(&SavedDocumentConversion) -> Result<Parts<S>, Error>,
    ) -> Result<Self, Error>
    where
        E: carina_core::parser::ExportParamLike<NoPlaceholder, AbsoluteBindingAddress>,
        Error: From<carina_core::value::SerializationError>,
        Resource<S, SnapshotLeaf, AbsoluteBindingAddress, ()>:
            Into<Resource<SavedModuleSource, SnapshotLeaf, AbsoluteBindingAddress, ()>>,
        carina_core::resource::DataSource<S, SnapshotLeaf, AbsoluteBindingAddress, ()>: Into<
            carina_core::resource::DataSource<
                SavedModuleSource,
                SnapshotLeaf,
                AbsoluteBindingAddress,
                (),
            >,
        >,
    {
        SavedDocumentConversion::document(parsed, sources, |conversion| {
            let parts = build(conversion)?;
            Ok(Self {
                transport: parts.into_transport(),
            })
        })
    }
    pub(super) fn into_transport(self) -> UnresolvedPlanFileTransport {
        self.transport
    }
}

/// Reading an existing document does not mint the authority to record one.
#[derive(Debug, Clone)]
pub(super) struct DecodedContents {
    transport: UnresolvedPlanFileTransport,
}
impl<'de> serde::Deserialize<'de> for DecodedContents {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let parts = Parts::<SavedModuleSource>::deserialize(deserializer)?;
        Ok(Self {
            transport: parts.into_transport(),
        })
    }
}
impl Deref for DecodedContents {
    type Target = UnresolvedPlanFileTransport;
    fn deref(&self) -> &Self::Target {
        &self.transport
    }
}
#[derive(Debug, Clone)]
pub(super) enum StoredContents {
    Recorded(PlanFileContents),
    Decoded(DecodedContents),
}
impl StoredContents {
    pub(super) fn into_transport(self) -> UnresolvedPlanFileTransport {
        match self {
            Self::Recorded(contents) => contents.into_transport(),
            Self::Decoded(contents) => contents.transport,
        }
    }
}
impl Deref for StoredContents {
    type Target = UnresolvedPlanFileTransport;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Recorded(contents) => contents,
            Self::Decoded(contents) => &contents.transport,
        }
    }
}

#[cfg(any(test, feature = "test-utils"))]
pub(super) fn admit_fixture(
    raw: UnresolvedPlanFileTransport,
) -> Result<PlanFileContents, carina_core::value::SerializationError> {
    let parsed = carina_core::parser::ExpandedInferredFile::default();
    PlanFileContents::build(&parsed, &[], |conversion| {
        let PlanFileTransport {
            version,
            carina_version,
            timestamp,
            source_path,
            source_identity,
            state_lineage,
            state_serial,
            provider_configs,
            backend_config,
            plan,
            sorted_resources,
            sorted_value_destroy_dependencies,
            sorted_explicit_destroy_dependencies,
            legacy_dependency_classification,
            unresolved_resources,
            compositions,
            data_sources,
            data_source_origins,
            data_source_values,
            current_states,
            upstream_snapshot,
            upstream_sources,
            wait_bindings,
        } = raw;
        let resources =
            |values: Vec<Resource<SavedModuleSource, SnapshotLeaf, AbsoluteBindingAddress, ()>>| {
                values
                    .into_iter()
                    .map(|value| conversion.snapshot(value))
                    .collect::<Result<Vec<_>, _>>()
            };
        let data = |values: Vec<
            carina_core::resource::DataSource<
                SavedModuleSource,
                SnapshotLeaf,
                AbsoluteBindingAddress,
                (),
            >,
        >| {
            values
                .into_iter()
                .map(|value| conversion.snapshot(value))
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Parts {
            version,
            carina_version,
            timestamp,
            source_path,
            source_identity,
            state_lineage,
            state_serial,
            provider_configs: conversion.snapshot(provider_configs)?,
            backend_config: conversion.snapshot(backend_config.into_wire())?,
            plan: conversion.snapshot(plan)?,
            sorted_resources: resources(sorted_resources)?,
            sorted_value_destroy_dependencies,
            sorted_explicit_destroy_dependencies,
            legacy_dependency_classification,
            unresolved_resources,
            compositions: compositions
                .into_iter()
                .map(|value| conversion.snapshot(value))
                .collect::<Result<Vec<_>, _>>()?,
            data_sources: data(data_sources)?,
            data_source_origins: data(data_source_origins)?,
            data_source_values: data_source_values
                .into_iter()
                .map(|entry| {
                    Ok(DataSourcePart {
                        id: entry.id,
                        value: conversion.snapshot(entry.value)?,
                    })
                })
                .collect::<Result<Vec<_>, carina_core::value::SerializationError>>()?,
            current_states,
            upstream_snapshot: upstream_snapshot
                .into_iter()
                .map(|(binding, attributes)| {
                    Ok((
                        binding,
                        attributes
                            .into_iter()
                            .map(|(key, value)| Ok((key, conversion.snapshot(value)?)))
                            .collect::<Result<_, carina_core::value::SerializationError>>()?,
                    ))
                })
                .collect::<Result<_, carina_core::value::SerializationError>>()?,
            upstream_sources,
            wait_bindings,
        })
    })
}
