use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use colored::Colorize;
use serde::{Deserialize, Serialize};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::data_source_value::DataSourceValue;
use carina_core::detail_rows::HistoricalDisplayAttributes;
use carina_core::differ::rebind_current_for_move;
use carina_core::effect::{Effect, EffectGeneration};
use carina_core::override_aware::CanonicalResourcePair;
#[cfg(test)]
use carina_core::parser::ProviderConfig;
use carina_core::parser::{
    BackendConfig, ProviderContext, ResolvedProviderConfigs, SavedProviderConfigs, UpstreamState,
};
use carina_core::plan::Plan;
#[cfg(test)]
use carina_core::resource::DeferredValue;
use carina_core::resource::{
    Composition, ConcreteValue, Current, DataSource, Resource, ResourceId, State, Value,
};
use carina_core::schema::SchemaKind;
#[cfg(test)]
use carina_core::value::PersistedValue;
use carina_core::value::{
    PersistedExportValue, redact_secrets_in_plan_with_desired_and_historical,
    redact_secrets_in_resource, redact_secrets_in_resource_reusing_persisted_hashes,
    redact_secrets_in_resource_using_evaluated, redact_secrets_in_state,
    redact_secrets_in_state_using_persisted, redact_secrets_in_state_using_resource,
};
use carina_state::{ResolvedReadBackend, ResourceState, StateFile};

use super::{BackendLockRequirement, ReadBackendWarningPolicy, resolve_backend_for_read};
use crate::DetailLevel;
use crate::commands::shared::plan_errors::render_plan_errors_and_abort;
use crate::commands::shared::progress::ProgressSink;
use crate::display::print_plan;
use crate::error::AppError;
#[cfg(test)]
use crate::plan_image::PlanImageError;
#[cfg(test)]
use crate::plan_image::{
    ImageTheme, InlineImageProtocol, PlanImageTarget, Theme, resolve_plan_image_options,
};
use crate::plan_image::{
    InlineImageRequest, PlanImageInput, RenderedInlineImage, ResolvedPlanImageTarget,
    StateMetadata, render_plan_outputs, write_inline_image, write_plan_image,
};
use crate::plan_summary::PlanChangeSummary;
use crate::wiring::{
    LoadedProviderArtifact, WiringContext, build_factories_from_providers,
    create_plan_from_parsed_with_upstream_with_ctx, provider_factory_source,
    reconcile_canonical_anonymous_identifiers_with_ctx, reconcile_canonical_prefixed_names,
};
use carina_core::hint::ProjectCommand;

fn schema_kind_label(kind: SchemaKind) -> &'static str {
    match kind {
        SchemaKind::Resource => "managed",
        SchemaKind::DataSource => "data source",
    }
}

/// Presence and, when present, semantic identity of one referenced schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeSchemaFingerprint {
    Present(String),
    Absent,
}

/// One schema in a provider fingerprint, retained so apply can identify a
/// changed or vanished type without persisting the full provider schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSchemaTypeFingerprint {
    pub kind: SchemaKind,
    pub resource_type: String,
    pub fingerprint: TypeSchemaFingerprint,
}

/// Fingerprint of exactly the provider schemas referenced by one saved plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSchemaFingerprint {
    pub schemas: Vec<PlanSchemaTypeFingerprint>,
}

/// Provider schema snapshot persisted in a saved plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSchemaFingerprints {
    pub providers: BTreeMap<String, ProviderSchemaFingerprint>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PlanSchemaReference {
    provider: String,
    kind: SchemaKind,
    resource_type: String,
}

/// Immutable inventory of schema types referenced by a decoded saved plan.
///
/// Saved-plan apply captures this before the locked-state reload consumes
/// [`PlanFileContents::current_states`], then carries it to schema verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanSchemaReferences(BTreeSet<PlanSchemaReference>);

/// Structured comparison failure consumed by the saved-plan apply typestate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanSchemaFingerprintMismatch {
    pub(crate) providers: BTreeMap<String, Vec<String>>,
}

/// Saved, typed phase-2 outcome for one configured data source.
///
/// The address is stored alongside the outcome so saved-plan loading can
/// validate that the set is exactly the plan's configured data-source set
/// before constructing [`carina_core::data_source_value::DataSourceValues`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedDataSourceValue {
    pub id: ResourceId,
    pub value: carina_core::data_source_value::DataSourceValue,
}

pub type PlanProviderArtifacts = BTreeMap<String, RecordedProviderArtifact>;

/// Provider artifact identity persisted in a saved plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordedProviderArtifact {
    BuiltIn,
    Wasm {
        sha256: String,
        origin: RecordedArtifactOrigin,
    },
}

/// Provider artifact implementation selected by a saved provider configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderArtifactKind {
    BuiltIn,
    Wasm,
}

impl std::fmt::Display for ProviderArtifactKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::BuiltIn => "built-in",
            Self::Wasm => "wasm",
        })
    }
}

/// Resolver origin persisted for one WASM provider artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordedArtifactOrigin {
    Locked {
        source: String,
        version: RecordedLockedArtifactVersion,
    },
    File,
}

/// Lock-resolved version facts persisted for one provider artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordedLockedArtifactVersion {
    Version { version: String },
    Revision { resolved_sha: String },
    RegistryRevision { revision: String, version: String },
}

impl From<&LoadedProviderArtifact> for RecordedProviderArtifact {
    fn from(artifact: &LoadedProviderArtifact) -> Self {
        match artifact {
            LoadedProviderArtifact::BuiltIn => Self::BuiltIn,
            LoadedProviderArtifact::Wasm(identity) => {
                let origin = match identity.origin() {
                    carina_provider_resolver::LoadedArtifactOrigin::Locked { source, version } => {
                        let version = match version {
                            carina_provider_resolver::LockedArtifactVersion::Version {
                                version,
                            } => RecordedLockedArtifactVersion::Version {
                                version: version.clone(),
                            },
                            carina_provider_resolver::LockedArtifactVersion::Revision {
                                resolved_sha,
                            } => RecordedLockedArtifactVersion::Revision {
                                resolved_sha: resolved_sha.clone(),
                            },
                            carina_provider_resolver::LockedArtifactVersion::RegistryRevision {
                                revision,
                                version,
                            } => RecordedLockedArtifactVersion::RegistryRevision {
                                revision: revision.clone(),
                                version: version.clone(),
                            },
                        };
                        RecordedArtifactOrigin::Locked {
                            source: source.to_string(),
                            version,
                        }
                    }
                    carina_provider_resolver::LoadedArtifactOrigin::File => {
                        RecordedArtifactOrigin::File
                    }
                };
                Self::Wasm {
                    sha256: identity.sha256().to_string(),
                    origin,
                }
            }
        }
    }
}

impl RecordedProviderArtifact {
    fn kind(&self) -> ProviderArtifactKind {
        match self {
            Self::BuiltIn => ProviderArtifactKind::BuiltIn,
            Self::Wasm { .. } => ProviderArtifactKind::Wasm,
        }
    }

    pub(crate) fn summary(&self) -> String {
        match self {
            Self::BuiltIn => "built-in".to_string(),
            Self::Wasm { sha256, origin } => {
                let suffix = match origin {
                    RecordedArtifactOrigin::File => "file".to_string(),
                    RecordedArtifactOrigin::Locked { source, version } => match version {
                        RecordedLockedArtifactVersion::Version { version } => {
                            format!("source {source}, version {version}")
                        }
                        RecordedLockedArtifactVersion::Revision { resolved_sha } => {
                            format!("source {source}, resolved revision {resolved_sha}")
                        }
                        RecordedLockedArtifactVersion::RegistryRevision { revision, version } => {
                            format!(
                                "source {source}, registry revision {revision}, version {version}"
                            )
                        }
                    },
                };
                format!("wasm sha256 {sha256} ({suffix})")
            }
        }
    }
}

pub(crate) fn recorded_provider_artifacts(ctx: &WiringContext) -> PlanProviderArtifacts {
    ctx.provider_artifacts()
        .iter()
        .map(|(provider, artifact)| (provider.clone(), RecordedProviderArtifact::from(artifact)))
        .collect()
}

#[cfg(test)]
mod recorded_provider_artifact_tests {
    use super::{RecordedArtifactOrigin, RecordedProviderArtifact};

    #[test]
    fn wasm_summary_includes_the_full_sha256() {
        let sha256 = "a".repeat(64);
        let artifact = RecordedProviderArtifact::Wasm {
            sha256: sha256.clone(),
            origin: RecordedArtifactOrigin::File,
        };

        assert_eq!(artifact.summary(), format!("wasm sha256 {sha256} (file)"));
    }
}

/// Contents of a saved plan before provider evidence has been recorded.
///
/// This type is intentionally not a saved-plan wire type. Constructing these
/// public fields does not produce a writable [`PlanFile`]; callers must consume
/// the contents through [`PlanFileContents::record`] so the referenced schemas
/// and loaded provider artifacts are captured together.
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
#[derive(Debug, Clone)]
pub struct PlanFileContents {
    /// Plan file format version.
    ///
    /// `PlanFileHeader` in `commands/apply/mod.rs` relies on this field and
    /// `timestamp` to recognize saved plans before decoding format-specific
    /// fields. Their serialized names must remain stable across plan formats;
    /// the version must also remain a `u32` (guarded by a test).
    /// Plan format 26 adds mandatory identities for every loaded provider
    /// artifact. Plan format 25 adds the mandatory typed `data_source_values`
    /// outcomes and removes duplicate data-source rows from `current_states`.
    /// Plan format 24 removes the redundant wait target schema kind now that
    /// waits are managed-only. Plan format 23 adds the mandatory
    /// `schema_fingerprints` snapshot. Plan format 21 explicitly tags managed
    /// and data-source current-state entries, and explicitly tags each managed
    /// [`Current`] variant. A serialized `Present` row is classified again
    /// through [`Current::from_state`] when decoded, so the tag cannot mint an
    /// invalid provider identifier.
    pub version: u32,
    /// Carina version that created this plan
    pub carina_version: String,
    /// ISO 8601 timestamp.
    ///
    /// Its presence is the plan-only discriminator used by `PlanFileHeader`;
    /// keep its serialized name stable across plan formats.
    pub timestamp: String,
    /// Original .crn path (informational).
    ///
    /// `PlanFileHeader` decodes this field on a best-effort basis to render
    /// replan guidance when the format has a string value for it.
    pub source_path: String,
    /// State lineage for drift detection
    pub state_lineage: Option<String>,
    /// State serial for drift detection
    pub state_serial: Option<u64>,
    /// Provider configurations
    pub provider_configs: SavedProviderConfigs,
    /// Backend configuration
    pub backend_config: Option<BackendConfig>,
    /// The plan (effects)
    pub plan: Plan,
    /// Pre-resolution resources sorted by dependencies and keyed with their
    /// paired resolved IDs (for post-apply state saving).
    pub sorted_resources: Vec<Resource>,
    /// Pre-resolution resources captured before ResourceRef substitution and
    /// stored in the same order as `sorted_resources`.
    ///
    /// The saved-plan apply path zips this snapshot with `sorted_resources`
    /// and keys by the sorted resource ID for dependency analysis and
    /// apply-time reference re-resolution.
    pub unresolved_resources: Vec<Resource>,
    /// Virtual resources (module-call attribute containers) emitted
    /// by module expansion at plan time (carina#3248). Persisted so
    /// the saved-plan apply path builds the same `ResolvedBindings`
    /// view as the live-apply path: an attribute referencing
    /// `<module_instance>.<attr>` chains through the composition's
    /// attribute map to the managed sibling literal that backs it.
    ///
    /// Pre-carina#3248 saved plans (version `3`) did not persist
    /// compositions and apply-from-plan passed `&[]` into the executor —
    /// any composition-rooted ref would survive resolution as a
    /// `ResourceRef` and fail-fast at the executor's
    /// `assert_fully_resolved` check, or produce a spurious diff if
    /// it reached the differ (carina#3246).
    ///
    /// Empty when the configuration declares no module calls /
    /// `attributes { ... }` blocks.
    pub compositions: Vec<carina_core::resource::Composition>,
    /// Data sources (`let x = read aws.iam.user { ... }`) emitted by
    /// module expansion at plan time (carina#3248). Persisted so the
    /// saved-plan apply path can re-create the same unified
    /// `ResolvedBindings` view as the live-apply path: a managed
    /// attribute referencing `<read_binding>.<attr>` resolves through
    /// the data source's attribute map.
    ///
    /// Pre-carina#3248 saved plans did not persist data sources
    /// separately (the field was missing); a managed→data-source ref
    /// would have left a `ResourceRef` unresolved at apply time. The
    /// typed-split (#3181) moved data sources out of
    /// `sorted_resources`, so persisting them explicitly is the only
    /// way to keep the saved-plan apply path consistent with the
    /// live-apply path.
    ///
    /// Empty when the configuration declares no `read`-bound
    /// bindings.
    pub data_sources: Vec<carina_core::resource::DataSource>,
    /// Authored, pre-normalization counterparts of `data_sources`, paired by
    /// index and rewritten to the resolved IDs above. Apply uses these to
    /// derive which input attributes became known at runtime without
    /// rechecking canonicalized literals as authored values.
    pub data_source_origins: Vec<carina_core::resource::DataSource>,
    /// One plan-time phase-2 outcome for every entry in `data_sources`.
    ///
    /// Format 25 persists this total collection so saved-plan apply never
    /// reclassifies deferred reads from an incomplete dependency view.
    pub data_source_values: Vec<SavedDataSourceValue>,
    /// Managed current states (for binding construction and state saving).
    /// Data-source read state lives only in `data_source_values`.
    pub current_states: Vec<CurrentStateEntry>,
    /// `upstream_state` bindings as resolved at plan time (#2303).
    ///
    /// Persisted so `apply --plan` can verify the upstream values have
    /// not drifted between plan and apply. If any binding here disagrees
    /// with the freshly-loaded upstream view at apply time, the apply
    /// fails with a structured error rather than silently mixing
    /// plan-time and apply-time values during cascade re-resolution.
    ///
    /// Empty when the configuration declares no `upstream_state` blocks.
    pub upstream_snapshot: HashMap<String, HashMap<String, Value>>,
    /// `upstream_state` block sources (binding name → directory) as
    /// declared in the original `.crn` config (#2303). Persisted so
    /// `apply --plan` can re-load each upstream and compare against
    /// `upstream_snapshot`.
    pub upstream_sources: Vec<UpstreamSource>,
    /// `wait` binding → target binding pairs as declared in the
    /// original `.crn` config (carina#3085). Persisted so `apply
    /// --plan`'s cascade re-resolution can rebuild the wait
    /// passthrough aliases (`<wait-binding>.<attr>` →
    /// `<target>.<attr>`) without the parser `WaitBinding` (which is
    /// not `Serialize` — same constraint that motivated
    /// [`UpstreamSource`]). Only the `(binding, target)` pair is
    /// persisted: the `until` predicate / timeout / `depends_on` are
    /// effect-layer concerns already encoded in the serialized `plan`'s
    /// `Effect::Wait`, not needed to rebuild the value-layer alias.
    /// Empty when the configuration declares no `wait` bindings.
    pub wait_bindings: Vec<PlanWaitBinding>,
}

/// Provider evidence captured atomically from the wiring context used to plan.
///
/// The fields are private so production code cannot assemble stale, partial,
/// or empty sentinel evidence independently from the plan contents.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderEvidence {
    /// Canonical fingerprints of exactly the managed and data-source schemas
    /// this plan used. This field is mandatory since plan format 23.
    schema_fingerprints: PlanSchemaFingerprints,
    /// Exact provider-kind artifacts loaded while this plan was computed.
    /// This field is mandatory since plan format 26.
    provider_artifacts: PlanProviderArtifacts,
}

/// Saved plan file for `plan --out` / `apply <plan-file>`.
///
/// This is a typestate boundary: an in-memory `PlanFile` always contains
/// provider evidence recorded from the planning [`WiringContext`], or evidence
/// that passed the same validation while decoding. Decode requires the schema
/// inventory to match every referenced type exactly and the provider-artifact
/// keys to match the providers implied by the saved provider configurations.
///
/// The format remains 26 because this change does not alter its on-disk shape;
/// it only tightens validation. Plans written by the unreleased #4059 build
/// that violate these rules are rejected as corrupt. No release has shipped
/// since format 26 was introduced.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(into = "PlanFileWire", try_from = "PlanFileWire")]
pub struct PlanFile {
    contents: PlanFileContents,
    evidence: ProviderEvidence,
}

/// Private format-26 wire representation.
///
/// Field order is intentionally identical to the historical `PlanFile`
/// serialization order. Evidence is mandatory and is never defaulted.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlanFileWire {
    version: u32,
    carina_version: String,
    timestamp: String,
    source_path: String,
    state_lineage: Option<String>,
    state_serial: Option<u64>,
    provider_configs: SavedProviderConfigs,
    backend_config: Option<BackendConfig>,
    plan: Plan,
    sorted_resources: Vec<Resource>,
    unresolved_resources: Vec<Resource>,
    #[serde(default)]
    compositions: Vec<Composition>,
    #[serde(default)]
    data_sources: Vec<DataSource>,
    #[serde(default)]
    data_source_origins: Vec<DataSource>,
    data_source_values: Vec<SavedDataSourceValue>,
    current_states: Vec<CurrentStateEntry>,
    upstream_snapshot: HashMap<String, HashMap<String, Value>>,
    upstream_sources: Vec<UpstreamSource>,
    #[serde(default)]
    wait_bindings: Vec<PlanWaitBinding>,
    schema_fingerprints: PlanSchemaFingerprints,
    provider_artifacts: PlanProviderArtifacts,
}

/// A saved plan's provider evidence is incomplete or inconsistent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvidenceError {
    EmptySchemaInventory {
        provider: String,
    },
    DuplicateSchemaFingerprint {
        provider: String,
        kind: SchemaKind,
        resource_type: String,
    },
    MissingSchemaFingerprint {
        provider: String,
        kind: SchemaKind,
        resource_type: String,
    },
    UnexpectedSchemaFingerprint {
        provider: String,
        kind: SchemaKind,
        resource_type: String,
    },
    MissingProviderArtifact {
        provider: String,
    },
    UnexpectedProviderArtifact {
        provider: String,
    },
    UnexpectedProviderArtifactKind {
        provider: String,
        expected: ProviderArtifactKind,
        recorded: ProviderArtifactKind,
    },
}

impl std::fmt::Display for ProviderEvidenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("invalid saved-plan provider evidence: ")?;
        match self {
            Self::EmptySchemaInventory { provider } => write!(
                formatter,
                "schema_fingerprints provider `{provider}` has an empty schemas list"
            ),
            Self::DuplicateSchemaFingerprint {
                provider,
                kind,
                resource_type,
            } => write!(
                formatter,
                "duplicate schema fingerprint for provider `{provider}`, {} type `{resource_type}`",
                schema_kind_label(*kind)
            ),
            Self::MissingSchemaFingerprint {
                provider,
                kind,
                resource_type,
            } => write!(
                formatter,
                "schema_fingerprints is missing provider `{provider}`, {} type `{resource_type}`",
                schema_kind_label(*kind)
            ),
            Self::UnexpectedSchemaFingerprint {
                provider,
                kind,
                resource_type,
            } => write!(
                formatter,
                "schema_fingerprints contains unreferenced provider `{provider}`, {} type `{resource_type}`",
                schema_kind_label(*kind)
            ),
            Self::MissingProviderArtifact { provider } => write!(
                formatter,
                "provider_artifacts is missing expected provider `{provider}`"
            ),
            Self::UnexpectedProviderArtifact { provider } => write!(
                formatter,
                "provider_artifacts contains unexpected provider `{provider}`"
            ),
            Self::UnexpectedProviderArtifactKind {
                provider,
                expected,
                recorded,
            } => write!(
                formatter,
                "provider_artifacts records provider `{provider}` as {recorded}, expected {expected}"
            ),
        }
    }
}

impl std::error::Error for ProviderEvidenceError {}

/// Provider artifact kinds determined by the saved provider configuration.
///
/// Factory-backed providers use the same source-presence rule as
/// [`build_factories_from_providers`]. Source-less named instances therefore do
/// not add an artifact. The built-in mock occupies the same `mock` key as a
/// plugin factory and is also selected for the providerless fallback.
fn expected_provider_artifacts(
    provider_configs: &SavedProviderConfigs,
) -> BTreeMap<String, ProviderArtifactKind> {
    let configs = provider_configs.as_provider_configs();
    let mut expected = configs
        .iter()
        .filter(|config| provider_factory_source(config).is_some())
        .map(|config| (config.name.clone(), ProviderArtifactKind::Wasm))
        .collect::<BTreeMap<_, _>>();
    let uses_builtin_mock = carina_provider_mock::uses_builtin_provider(configs, |provider_name| {
        expected.get(provider_name) == Some(&ProviderArtifactKind::Wasm)
    });
    if uses_builtin_mock {
        expected.insert(
            carina_provider_mock::BUILTIN_PROVIDER_NAME.to_string(),
            ProviderArtifactKind::BuiltIn,
        );
    }
    expected
}

impl ProviderEvidence {
    fn capture(
        references: &PlanSchemaReferences,
        provider_configs: &SavedProviderConfigs,
        ctx: &WiringContext,
    ) -> Result<Self, ProviderEvidenceError> {
        Self::new(
            PlanSchemaFingerprints::capture(references, ctx.schemas()),
            recorded_provider_artifacts(ctx),
            references,
            provider_configs,
        )
    }

    fn new(
        schema_fingerprints: PlanSchemaFingerprints,
        provider_artifacts: PlanProviderArtifacts,
        references: &PlanSchemaReferences,
        provider_configs: &SavedProviderConfigs,
    ) -> Result<Self, ProviderEvidenceError> {
        let evidence = Self {
            schema_fingerprints,
            provider_artifacts,
        };
        evidence.validate(references, provider_configs)?;
        Ok(evidence)
    }

    fn validate(
        &self,
        references: &PlanSchemaReferences,
        provider_configs: &SavedProviderConfigs,
    ) -> Result<(), ProviderEvidenceError> {
        let mut recorded = BTreeSet::new();
        for (provider, fingerprint) in &self.schema_fingerprints.providers {
            if fingerprint.schemas.is_empty() {
                return Err(ProviderEvidenceError::EmptySchemaInventory {
                    provider: provider.clone(),
                });
            }
            for schema in &fingerprint.schemas {
                let reference = PlanSchemaReference {
                    provider: provider.clone(),
                    kind: schema.kind,
                    resource_type: schema.resource_type.clone(),
                };
                if !recorded.insert(reference) {
                    return Err(ProviderEvidenceError::DuplicateSchemaFingerprint {
                        provider: provider.clone(),
                        kind: schema.kind,
                        resource_type: schema.resource_type.clone(),
                    });
                }
            }
        }

        if let Some(missing) = references.0.difference(&recorded).next() {
            return Err(ProviderEvidenceError::MissingSchemaFingerprint {
                provider: missing.provider.clone(),
                kind: missing.kind,
                resource_type: missing.resource_type.clone(),
            });
        }
        if let Some(extra) = recorded.difference(&references.0).next() {
            return Err(ProviderEvidenceError::UnexpectedSchemaFingerprint {
                provider: extra.provider.clone(),
                kind: extra.kind,
                resource_type: extra.resource_type.clone(),
            });
        }

        let expected_artifacts = expected_provider_artifacts(provider_configs);
        let recorded_artifacts = self
            .provider_artifacts
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let expected_names = expected_artifacts.keys().cloned().collect::<BTreeSet<_>>();
        if let Some(provider) = expected_names.difference(&recorded_artifacts).next() {
            return Err(ProviderEvidenceError::MissingProviderArtifact {
                provider: provider.clone(),
            });
        }
        if let Some(provider) = recorded_artifacts.difference(&expected_names).next() {
            return Err(ProviderEvidenceError::UnexpectedProviderArtifact {
                provider: provider.clone(),
            });
        }
        for (provider, expected) in expected_artifacts {
            let recorded = self.provider_artifacts[&provider].kind();
            if recorded != expected {
                return Err(ProviderEvidenceError::UnexpectedProviderArtifactKind {
                    provider,
                    expected,
                    recorded,
                });
            }
        }
        Ok(())
    }
}

impl From<PlanFile> for PlanFileWire {
    fn from(plan_file: PlanFile) -> Self {
        let PlanFile { contents, evidence } = plan_file;
        let PlanFileContents {
            version,
            carina_version,
            timestamp,
            source_path,
            state_lineage,
            state_serial,
            provider_configs,
            backend_config,
            plan,
            sorted_resources,
            unresolved_resources,
            compositions,
            data_sources,
            data_source_origins,
            data_source_values,
            current_states,
            upstream_snapshot,
            upstream_sources,
            wait_bindings,
        } = contents;
        let ProviderEvidence {
            schema_fingerprints,
            provider_artifacts,
        } = evidence;
        Self {
            version,
            carina_version,
            timestamp,
            source_path,
            state_lineage,
            state_serial,
            provider_configs,
            backend_config,
            plan,
            sorted_resources,
            unresolved_resources,
            compositions,
            data_sources,
            data_source_origins,
            data_source_values,
            current_states,
            upstream_snapshot,
            upstream_sources,
            wait_bindings,
            schema_fingerprints,
            provider_artifacts,
        }
    }
}

impl TryFrom<PlanFileWire> for PlanFile {
    type Error = ProviderEvidenceError;

    fn try_from(wire: PlanFileWire) -> Result<Self, Self::Error> {
        let PlanFileWire {
            version,
            carina_version,
            timestamp,
            source_path,
            state_lineage,
            state_serial,
            provider_configs,
            backend_config,
            plan,
            sorted_resources,
            unresolved_resources,
            compositions,
            data_sources,
            data_source_origins,
            data_source_values,
            current_states,
            upstream_snapshot,
            upstream_sources,
            wait_bindings,
            schema_fingerprints,
            provider_artifacts,
        } = wire;
        let contents = PlanFileContents {
            version,
            carina_version,
            timestamp,
            source_path,
            state_lineage,
            state_serial,
            provider_configs,
            backend_config,
            plan,
            sorted_resources,
            unresolved_resources,
            compositions,
            data_sources,
            data_source_origins,
            data_source_values,
            current_states,
            upstream_snapshot,
            upstream_sources,
            wait_bindings,
        };
        let references = contents.referenced_schema_types();
        let evidence = ProviderEvidence::new(
            schema_fingerprints,
            provider_artifacts,
            &references,
            &contents.provider_configs,
        )?;
        Ok(Self { contents, evidence })
    }
}

impl Deref for PlanFile {
    type Target = PlanFileContents;

    fn deref(&self) -> &Self::Target {
        &self.contents
    }
}

impl PlanFileContents {
    /// Atomically bind these contents to the schemas and provider artifacts
    /// loaded in the planning context.
    pub fn record(self, ctx: &WiringContext) -> Result<PlanFile, ProviderEvidenceError> {
        let references = self.referenced_schema_types();
        let evidence = ProviderEvidence::capture(&references, &self.provider_configs, ctx)?;
        Ok(PlanFile {
            contents: self,
            evidence,
        })
    }

    pub(crate) fn referenced_schema_types(&self) -> PlanSchemaReferences {
        fn insert(
            references: &mut BTreeSet<PlanSchemaReference>,
            id: &ResourceId,
            kind: SchemaKind,
        ) {
            references.insert(PlanSchemaReference {
                provider: id.provider.clone(),
                kind,
                resource_type: id.resource_type.clone(),
            });
        }

        let mut references = BTreeSet::new();
        for effect in self.plan.effects() {
            match effect {
                Effect::Read { resource } => {
                    insert(&mut references, &resource.id, SchemaKind::DataSource);
                }
                Effect::Create(resource) => {
                    insert(&mut references, &resource.id, SchemaKind::Resource);
                }
                Effect::ResumeCreate {
                    resource,
                    started_at: _started_at,
                } => {
                    insert(&mut references, &resource.id, SchemaKind::Resource);
                }
                Effect::Update {
                    from,
                    identifier: _identifier,
                    to,
                    changed_attributes: _changed_attributes,
                } => {
                    insert(&mut references, &from.id, SchemaKind::Resource);
                    insert(&mut references, &to.id, SchemaKind::Resource);
                }
                Effect::Delete {
                    id,
                    identifier: _identifier,
                    generation: _generation,
                    directives: _directives,
                    binding: _binding,
                    dependencies: _dependencies,
                    explicit_dependencies: _explicit_dependencies,
                    blocked_by_updates: _blocked_by_updates,
                } => {
                    insert(&mut references, id, SchemaKind::Resource);
                }
                Effect::Import {
                    id,
                    identifier: _identifier,
                } => {
                    insert(&mut references, id, SchemaKind::Resource);
                }
                Effect::Remove { id } => {
                    insert(&mut references, id, SchemaKind::Resource);
                }
                Effect::DeferredCreate { id: _id, template } => {
                    insert(
                        &mut references,
                        &template.template_resource.id,
                        SchemaKind::Resource,
                    );
                }
                Effect::Move { from, to } => {
                    insert(&mut references, from, SchemaKind::Resource);
                    insert(&mut references, to, SchemaKind::Resource);
                }
                Effect::Wait {
                    identity: _identity,
                    target_id,
                    until: _until,
                    until_surface: _until_surface,
                    timeout: _timeout,
                    interval: _interval,
                    explicit_dependencies: _explicit_dependencies,
                } => {
                    insert(
                        &mut references,
                        target_id.id().as_inner(),
                        SchemaKind::Resource,
                    );
                }
                Effect::DeferredReplace(payload) => {
                    insert(
                        &mut references,
                        &payload.template.template_resource.id,
                        SchemaKind::Resource,
                    );
                    for delete in &payload.deletes {
                        insert(&mut references, &delete.id, SchemaKind::Resource);
                    }
                }
            }
        }

        for resource in &self.sorted_resources {
            insert(&mut references, &resource.id, SchemaKind::Resource);
        }
        for resource in &self.unresolved_resources {
            insert(&mut references, &resource.id, SchemaKind::Resource);
        }
        for data_source in self.data_sources.iter().chain(&self.data_source_origins) {
            insert(&mut references, &data_source.id, SchemaKind::DataSource);
        }
        for saved in &self.data_source_values {
            insert(&mut references, &saved.id, SchemaKind::DataSource);
        }
        for current in &self.current_states {
            let CurrentStateEntry::Managed {
                id,
                current: _current,
            } = current;
            insert(&mut references, id, SchemaKind::Resource);
        }
        PlanSchemaReferences(references)
    }
}

impl PlanFile {
    pub const CURRENT_VERSION: u32 = 26;

    #[cfg(test)]
    pub(crate) fn into_contents(self) -> PlanFileContents {
        self.contents
    }

    pub fn schema_fingerprints(&self) -> &PlanSchemaFingerprints {
        &self.evidence.schema_fingerprints
    }

    pub fn provider_artifacts(&self) -> &PlanProviderArtifacts {
        &self.evidence.provider_artifacts
    }

    /// Reload state-owned create continuations after the saved-plan apply has
    /// acquired its state lock. The plan contains only an address and marker;
    /// any status change in either direction invalidates it.
    pub(crate) fn reload_in_flight_from_locked_state(
        self,
        state_file: Option<&StateFile>,
    ) -> Result<ReloadedPlanFile, String> {
        let PlanFile {
            mut contents,
            evidence,
        } = self;
        let moved_origins = MovedOrigins::from_plan(&contents.plan);
        let planned_in_flight_origins: HashSet<_> = contents
            .current_states
            .iter()
            .filter_map(|entry| match entry {
                CurrentStateEntry::Managed { id, current } if current.planned_in_flight() => {
                    Some(moved_origins.origin_for(id).clone())
                }
                CurrentStateEntry::Managed {
                    id: _id,
                    current: _,
                } => None,
            })
            .collect();

        if let Some(state_file) = state_file {
            for row in state_file.resources() {
                let row_in_flight = match row.current() {
                    carina_state::CurrentInstance::CreateInFlight { pending: _pending } => true,
                    carina_state::CurrentInstance::None => false,
                    carina_state::CurrentInstance::Applied {
                        identifier: _identifier,
                        attributes: _attributes,
                        write_only_attributes: _write_only_attributes,
                        nested_write_only: _nested_write_only,
                        partial_read: _partial_read,
                    } => false,
                    carina_state::CurrentInstance::MissingIdentifier {
                        attributes: _attributes,
                        write_only_attributes: _write_only_attributes,
                        nested_write_only: _nested_write_only,
                        partial_read: _partial_read,
                    } => false,
                };
                if !row_in_flight {
                    continue;
                }
                let locked_id = ResourceId::with_provider_identity(
                    row.provider.clone(),
                    row.resource_type.clone(),
                    row.identity.clone(),
                    row.directives.provider_instance.clone(),
                );
                if !planned_in_flight_origins.contains(&locked_id) {
                    return Err(format!(
                        "saved plan is stale for {locked_id}: create-in-flight status changed from false to true; create a new plan before applying"
                    ));
                }
            }
        }

        let mut current_states = Vec::with_capacity(contents.current_states.len());
        for entry in std::mem::take(&mut contents.current_states) {
            match entry {
                CurrentStateEntry::Managed { id, current } => {
                    let persisted_id = moved_origins.origin_for(&id);
                    let mut locked_current = state_file
                        .map(|state| state.build_state_for_resource(persisted_id))
                        .unwrap_or_else(|| Current::absent(id.clone()));
                    if persisted_id != &id {
                        let authority = contents
                            .plan
                            .move_authority(persisted_id, &id)
                            .ok_or_else(|| {
                                format!("saved plan has no move edge from {persisted_id} to {id}")
                            })?;
                        locked_current = rebind_current_for_move(locked_current, &authority)
                            .map_err(|error| error.to_string())?;
                    }
                    let current = match (current.into_reload_kind(), locked_current) {
                        (
                            ReloadSavedManagedCurrent::CreateInFlightMarker,
                            locked @ Current::CreateInFlight(_),
                        ) => Box::new(locked),
                        (ReloadSavedManagedCurrent::CreateInFlightMarker, _locked) => {
                            return Err(format!(
                                "saved plan is stale for {id}: create-in-flight status changed from true to false; create a new plan before applying"
                            ));
                        }
                        (
                            ReloadSavedManagedCurrent::Snapshot(_saved),
                            Current::CreateInFlight(_pending),
                        ) => {
                            return Err(format!(
                                "saved plan is stale for {id}: create-in-flight status changed from false to true; create a new plan before applying"
                            ));
                        }
                        (ReloadSavedManagedCurrent::Snapshot(saved), _locked) => saved,
                    };
                    current_states.push(ReloadedCurrentStateEntry::Managed { id, current });
                }
            }
        }
        Ok(ReloadedPlanFile {
            contents,
            evidence,
            current_states,
        })
    }

    /// Whether any typed, value-bearing plan field still contains a secret
    /// placeholder.
    pub(crate) fn contains_redacted_secret_placeholders(&self) -> bool {
        use carina_core::resource::{Composition, DataSource, Signature};

        fn resource_contains(resource: &Resource) -> bool {
            let Resource {
                id: _id,
                attributes,
                directives: _directives,
                prefixes: _prefixes,
                binding: _binding,
                dependency_bindings: _dependency_bindings,
                module_source: _module_source,
                default_tag_keys: _default_tag_keys,
                quoted_string_attrs: _quoted_string_attrs,
            } = resource;
            attributes
                .values()
                .any(carina_core::value::contains_redacted_secret)
        }

        fn data_source_contains(data_source: &DataSource) -> bool {
            let DataSource {
                id: _id,
                attributes,
                directives: _directives,
                binding: _binding,
                dependency_bindings: _dependency_bindings,
                module_source: _module_source,
                quoted_string_attrs: _quoted_string_attrs,
            } = data_source;
            attributes
                .values()
                .any(carina_core::value::contains_redacted_secret)
        }

        fn composition_contains(composition: &Composition) -> bool {
            let Composition {
                id: _id,
                signature,
                binding: _binding,
                dependency_bindings: _dependency_bindings,
                module_name: _module_name,
                instance: _instance,
                provenance: _provenance,
                quoted_string_attrs: _quoted_string_attrs,
            } = composition;
            let Signature {
                arguments,
                attributes,
                pending_constraints: _pending_constraints,
            } = signature;
            arguments
                .values()
                .any(|argument| argument.contains_redacted_secret_placeholder())
                || attributes
                    .values()
                    .any(|attribute| attribute.contains_redacted_secret_placeholder())
        }

        fn state_contains(state: &State) -> bool {
            let State {
                id: _id,
                identifier: _identifier,
                attributes,
                exists: _exists,
                dependency_bindings: _dependency_bindings,
                partial_read: _partial_read,
            } = state;
            attributes
                .values()
                .any(carina_core::value::contains_redacted_secret)
        }

        fn current_state_contains(entry: &CurrentStateEntry) -> bool {
            let CurrentStateEntry::Managed { id: _id, current } = entry;
            current.contains_redacted_secret(state_contains)
        }

        fn data_source_value_contains(saved: &SavedDataSourceValue) -> bool {
            match &saved.value {
                DataSourceValue::Read(state) => state_contains(state),
                DataSourceValue::DeferredToApply(_) | DataSourceValue::BlockedByPlanError => false,
            }
        }

        let PlanFileContents {
            version: _version,
            carina_version: _carina_version,
            timestamp: _timestamp,
            source_path: _source_path,
            state_lineage: _state_lineage,
            state_serial: _state_serial,
            provider_configs,
            backend_config,
            plan,
            sorted_resources,
            unresolved_resources,
            compositions,
            data_sources,
            data_source_origins,
            data_source_values,
            current_states,
            upstream_snapshot,
            upstream_sources: _upstream_sources,
            wait_bindings: _wait_bindings,
        } = &self.contents;

        provider_configs.contains_redacted_secret()
            || backend_config.as_ref().is_some_and(|backend| {
                let BackendConfig {
                    backend_type: _backend_type,
                    attributes,
                    attribute_spans: _attribute_spans,
                } = backend;
                attributes
                    .values()
                    .any(carina_core::value::contains_redacted_secret)
            })
            || plan.contains_redacted_secret_placeholders()
            || sorted_resources.iter().any(resource_contains)
            || unresolved_resources.iter().any(resource_contains)
            || compositions.iter().any(composition_contains)
            || data_sources.iter().any(data_source_contains)
            || data_source_origins.iter().any(data_source_contains)
            || data_source_values.iter().any(data_source_value_contains)
            || current_states.iter().any(current_state_contains)
            || upstream_snapshot.values().any(|attributes| {
                attributes
                    .values()
                    .any(carina_core::value::contains_redacted_secret)
            })
    }

    pub(crate) fn validate_resource_identities(&self) -> Result<(), String> {
        fn require_identity(id: &ResourceId, location: &str) -> Result<(), String> {
            if id.identity_str().is_none() {
                return Err(format!(
                    "Invalid saved plan: {location} has a pending resource identity"
                ));
            }
            Ok(())
        }

        for (index, resource) in self.sorted_resources.iter().enumerate() {
            require_identity(&resource.id, &format!("sorted_resources[{index}]"))?;
        }
        for (index, resource) in self.unresolved_resources.iter().enumerate() {
            require_identity(&resource.id, &format!("unresolved_resources[{index}]"))?;
        }
        for (index, composition) in self.compositions.iter().enumerate() {
            require_identity(&composition.id, &format!("compositions[{index}]"))?;
        }
        for (index, data_source) in self.data_sources.iter().enumerate() {
            require_identity(&data_source.id, &format!("data_sources[{index}]"))?;
        }
        for (index, data_source) in self.data_source_origins.iter().enumerate() {
            require_identity(&data_source.id, &format!("data_source_origins[{index}]"))?;
        }
        let configured_data_source_ids = self
            .data_sources
            .iter()
            .map(|data_source| &data_source.id)
            .collect::<HashSet<_>>();
        if configured_data_source_ids.len() != self.data_sources.len() {
            return Err(
                "Invalid saved plan: data_sources contains a duplicate address".to_string(),
            );
        }
        let mut outcome_ids = HashSet::with_capacity(self.data_source_values.len());
        let mut has_blocked_outcome = false;
        for (index, saved) in self.data_source_values.iter().enumerate() {
            require_identity(&saved.id, &format!("data_source_values[{index}].id"))?;
            if !outcome_ids.insert(&saved.id) {
                return Err(format!(
                    "Invalid saved plan: data_source_values contains duplicate outcomes for `{}`",
                    saved.id
                ));
            }
            if !configured_data_source_ids.contains(&saved.id) {
                return Err(format!(
                    "Invalid saved plan: data_source_values contains an outcome for unknown data source `{}`",
                    saved.id
                ));
            }
            match &saved.value {
                DataSourceValue::Read(state) => {
                    require_identity(
                        &state.id,
                        &format!("data_source_values[{index}].value.state.id"),
                    )?;
                    if state.id != saved.id {
                        return Err(format!(
                            "Invalid saved plan: data_source_values[{index}] address `{}` does not match nested state address `{}`",
                            saved.id, state.id
                        ));
                    }
                }
                DataSourceValue::DeferredToApply(_) => {}
                DataSourceValue::BlockedByPlanError => has_blocked_outcome = true,
            }
        }
        if has_blocked_outcome && !self.plan.has_errors() {
            return Err(
                "Invalid saved plan: data_source_values contains blocked_by_plan_error but the plan has no plan errors"
                    .to_string(),
            );
        }
        if let Some(missing) = configured_data_source_ids
            .iter()
            .find(|id| !outcome_ids.contains(*id))
        {
            return Err(format!(
                "Invalid saved plan: data_source_values has no outcome for data source `{missing}`"
            ));
        }
        for (index, entry) in self.current_states.iter().enumerate() {
            require_identity(entry.id(), &format!("current_states[{index}].id"))?;
            let CurrentStateEntry::Managed { id: _id, current } = entry;
            if let Some(nested_id) = current.nested_id() {
                require_identity(nested_id, &format!("current_states[{index}].state.id"))?;
            }
        }

        for (index, effect) in self.plan.effects().iter().enumerate() {
            let location = format!("plan.effects[{index}]");
            match effect {
                Effect::Read { resource } => require_identity(&resource.id, &location)?,
                Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => {
                    require_identity(&resource.id, &location)?
                }
                Effect::Update { from, to, .. } => {
                    require_identity(&from.id, &format!("{location}.from"))?;
                    require_identity(&to.id, &format!("{location}.to"))?;
                }
                Effect::Delete { id, .. } | Effect::Import { id, .. } | Effect::Remove { id } => {
                    require_identity(id, &location)?
                }
                Effect::Wait {
                    identity,
                    target_id,
                    until: _until,
                    until_surface: _until_surface,
                    timeout: _timeout,
                    interval: _interval,
                    explicit_dependencies: _explicit_dependencies,
                } => {
                    let target = target_id.id().as_inner();
                    require_identity(target, &location)?;
                    if !self
                        .sorted_resources
                        .iter()
                        .any(|resource| &resource.id == target)
                    {
                        return Err(format!(
                            "Invalid saved plan: wait `{identity}` targets `{target}`, which is not a managed resource in this plan"
                        ));
                    }
                }
                Effect::Move { from, to } => {
                    require_identity(from, &format!("{location}.from"))?;
                    require_identity(to, &format!("{location}.to"))?;
                }
                Effect::DeferredCreate { id, template, .. } => {
                    require_identity(id, &location)?;
                    require_identity(
                        &template.template_resource.id,
                        &format!("{location}.template"),
                    )?;
                }
                Effect::DeferredReplace(payload) => {
                    require_identity(&payload.id, &location)?;
                    require_identity(
                        &payload.template.template_resource.id,
                        &format!("{location}.template"),
                    )?;
                    for (delete_index, delete) in payload.deletes.iter().enumerate() {
                        require_identity(
                            &delete.id,
                            &format!("{location}.deletes[{delete_index}]"),
                        )?;
                    }
                }
            }
        }
        for (index, (id, _key)) in self.plan.abandoned_deposed_generations().enumerate() {
            require_identity(
                id.as_inner(),
                &format!("plan.abandoned_deposed[{index}].id"),
            )?;
        }

        Ok(())
    }

    pub(crate) fn validate_replace_display(&self) -> Result<(), String> {
        let effects = self.plan.effects();
        let mut seen_replace_indices = HashSet::new();
        for (metadata_idx, replacement) in self.plan.replace_display_info().enumerate() {
            let create = effects.get(replacement.create_idx).ok_or_else(|| {
                format!(
                    "Invalid saved plan replacement metadata: replace_display[{metadata_idx}].create_idx {} is out of bounds for {} effects",
                    replacement.create_idx,
                    effects.len()
                )
            })?;
            if !matches!(create, Effect::Create(_)) {
                return Err(format!(
                    "Invalid saved plan replacement metadata: replace_display[{metadata_idx}].create_idx {} points at {}, expected Create",
                    replacement.create_idx,
                    create.kind()
                ));
            }
            if !seen_replace_indices.insert(replacement.create_idx) {
                return Err(format!(
                    "Invalid saved plan replacement metadata: replace_display[{metadata_idx}].create_idx {} duplicates or overlaps another replacement metadata entry",
                    replacement.create_idx
                ));
            }

            let delete = effects.get(replacement.delete_idx).ok_or_else(|| {
                format!(
                    "Invalid saved plan replacement metadata: replace_display[{metadata_idx}].delete_idx {} is out of bounds for {} effects",
                    replacement.delete_idx,
                    effects.len()
                )
            })?;
            match delete {
                Effect::Delete {
                    generation: EffectGeneration::Current,
                    ..
                } => {}
                Effect::Delete { generation, .. } => {
                    return Err(format!(
                        "Invalid saved plan replacement metadata: replace_display[{metadata_idx}].delete_idx {} points at Delete({generation:?}), expected Current Delete",
                        replacement.delete_idx
                    ));
                }
                _ => {
                    return Err(format!(
                        "Invalid saved plan replacement metadata: replace_display[{metadata_idx}].delete_idx {} points at {}, expected Delete",
                        replacement.delete_idx,
                        delete.kind()
                    ));
                }
            }
            if create.resource_id() != delete.resource_id() {
                return Err(format!(
                    "Invalid saved plan replacement metadata: replace_display[{metadata_idx}] cross-wires create_idx {} ({}) with delete_idx {} ({}), expected both halves to address the same resource",
                    replacement.create_idx,
                    create.resource_id(),
                    replacement.delete_idx,
                    delete.resource_id()
                ));
            }
            if !seen_replace_indices.insert(replacement.delete_idx) {
                return Err(format!(
                    "Invalid saved plan replacement metadata: replace_display[{metadata_idx}].delete_idx {} duplicates or overlaps another replacement metadata entry",
                    replacement.delete_idx
                ));
            }
        }

        Ok(())
    }
}

impl PlanSchemaFingerprints {
    fn capture(
        references: &PlanSchemaReferences,
        registry: &carina_core::schema::SchemaRegistry,
    ) -> Self {
        let mut grouped = BTreeMap::<String, Vec<PlanSchemaTypeFingerprint>>::new();

        for reference in &references.0 {
            let fingerprint = registry
                .get(
                    &reference.provider,
                    &reference.resource_type,
                    reference.kind,
                )
                .map_or(TypeSchemaFingerprint::Absent, |schema| {
                    TypeSchemaFingerprint::Present(schema.as_schema().semantic_fingerprint())
                });
            grouped.entry(reference.provider.clone()).or_default().push(
                PlanSchemaTypeFingerprint {
                    kind: reference.kind,
                    resource_type: reference.resource_type.clone(),
                    fingerprint,
                },
            );
        }

        let providers = grouped
            .into_iter()
            .map(|(provider, mut schemas)| {
                schemas.sort_by(|left, right| {
                    (left.kind, left.resource_type.as_str())
                        .cmp(&(right.kind, right.resource_type.as_str()))
                });
                (provider, ProviderSchemaFingerprint { schemas })
            })
            .collect();
        Self { providers }
    }

    fn verify(
        &self,
        references: &PlanSchemaReferences,
        registry: &carina_core::schema::SchemaRegistry,
    ) -> Result<(), PlanSchemaFingerprintMismatch> {
        let mut expected_by_provider = BTreeMap::<String, Vec<PlanSchemaReference>>::new();
        for reference in &references.0 {
            expected_by_provider
                .entry(reference.provider.clone())
                .or_default()
                .push(reference.clone());
        }

        let providers = expected_by_provider
            .keys()
            .chain(self.providers.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut mismatches = BTreeMap::new();
        for provider in providers {
            let expected = expected_by_provider
                .get(&provider)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let saved = self.providers.get(&provider);
            let mut changed_types = Vec::new();
            let mut saved_by_type = BTreeMap::new();
            if let Some(saved) = saved {
                for schema in &saved.schemas {
                    let key = (schema.kind, schema.resource_type.as_str());
                    if saved_by_type.insert(key, schema).is_some() {
                        changed_types.push(format!(
                            "{} `{}` (duplicate fingerprint entry)",
                            schema_kind_label(schema.kind),
                            schema.resource_type,
                        ));
                    }
                }
            }

            let expected_keys = expected
                .iter()
                .map(|reference| (reference.kind, reference.resource_type.as_str()))
                .collect::<BTreeSet<_>>();
            for reference in expected {
                let key = (reference.kind, reference.resource_type.as_str());
                let Some(recorded) = saved_by_type.get(&key) else {
                    changed_types.push(format!(
                        "{} `{}` (not recorded)",
                        schema_kind_label(reference.kind),
                        reference.resource_type,
                    ));
                    continue;
                };
                let current = registry.get(
                    &reference.provider,
                    &reference.resource_type,
                    reference.kind,
                );
                match (&recorded.fingerprint, current) {
                    (TypeSchemaFingerprint::Absent, None) => {}
                    (TypeSchemaFingerprint::Absent, Some(_)) => changed_types.push(format!(
                        "{} `{}` (schema is now present)",
                        schema_kind_label(reference.kind),
                        reference.resource_type,
                    )),
                    (TypeSchemaFingerprint::Present(_), None) => changed_types.push(format!(
                        "{} `{}` (missing)",
                        schema_kind_label(reference.kind),
                        reference.resource_type,
                    )),
                    (TypeSchemaFingerprint::Present(recorded), Some(current))
                        if current.as_schema().semantic_fingerprint() != *recorded =>
                    {
                        changed_types.push(format!(
                            "{} `{}` (changed)",
                            schema_kind_label(reference.kind),
                            reference.resource_type,
                        ));
                    }
                    (TypeSchemaFingerprint::Present(_), Some(_)) => {}
                }
            }

            for schema in saved_by_type.values() {
                if !expected_keys.contains(&(schema.kind, schema.resource_type.as_str())) {
                    changed_types.push(format!(
                        "{} `{}` (not referenced by plan)",
                        schema_kind_label(schema.kind),
                        schema.resource_type,
                    ));
                }
            }

            if !changed_types.is_empty() {
                changed_types.sort();
                changed_types.dedup();
                mismatches.insert(provider, changed_types);
            }
        }
        if mismatches.is_empty() {
            Ok(())
        } else {
            Err(PlanSchemaFingerprintMismatch {
                providers: mismatches,
            })
        }
    }
}

#[cfg(test)]
mod schema_fingerprint_tests {
    use super::*;
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    use indexmap::IndexMap;

    fn reference(provider: &str, resource_type: &str) -> PlanSchemaReference {
        PlanSchemaReference {
            provider: provider.to_string(),
            kind: SchemaKind::Resource,
            resource_type: resource_type.to_string(),
        }
    }

    fn schema(resource_type: &str, attribute: &str) -> ResourceSchema {
        ResourceSchema::new(resource_type)
            .attribute(AttributeSchema::optional(attribute, AttributeType::string()).build())
    }

    fn saved_provider_configs(configs: Vec<ProviderConfig>) -> SavedProviderConfigs {
        let parsed = carina_core::parser::InferredFile {
            providers: configs,
            ..Default::default()
        };
        let resolved = carina_core::parser::resolve_provider_attributes_with_remote(
            &parsed,
            &HashMap::new(),
            &ProviderContext::default(),
        )
        .expect("test provider configs must resolve");
        SavedProviderConfigs::from(&resolved)
    }

    fn provider_config(name: &str, binding: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            name: name.to_string(),
            attributes: IndexMap::new(),
            default_tags: IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: IndexMap::new(),
            binding: binding.map(str::to_string),
        }
    }

    #[test]
    fn providerless_artifact_inventory_is_the_builtin_mock() {
        assert_eq!(
            expected_provider_artifacts(&saved_provider_configs(Vec::new())),
            BTreeMap::from([("mock".to_string(), ProviderArtifactKind::BuiltIn)])
        );
    }

    #[test]
    fn configured_provider_aliases_share_one_artifact_key() {
        let mut default = provider_config("aws", None);
        default.source = Some("file:///aws.wasm".parse().unwrap());
        let configs = saved_provider_configs(vec![default, provider_config("aws", Some("us"))]);
        assert_eq!(
            expected_provider_artifacts(&configs),
            BTreeMap::from([("aws".to_string(), ProviderArtifactKind::Wasm)])
        );
    }

    #[test]
    fn named_instance_without_a_default_does_not_expect_an_artifact() {
        let configs = saved_provider_configs(vec![provider_config("aws", Some("secondary"))]);

        assert!(expected_provider_artifacts(&configs).is_empty());
    }

    #[test]
    fn capture_accepts_a_referenced_type_without_a_loaded_schema() {
        let references =
            PlanSchemaReferences(BTreeSet::from([reference("retired", "example.Gone")]));
        let saved = PlanSchemaFingerprints::capture(&references, &SchemaRegistry::new());
        assert_eq!(
            saved.providers["retired"].schemas[0].fingerprint,
            TypeSchemaFingerprint::Absent,
            "state-only effects must persist an absent schema"
        );
    }

    #[test]
    fn verify_rejects_absent_to_present_schema_transition() {
        let references =
            PlanSchemaReferences(BTreeSet::from([reference("provider", "example.Gone")]));
        let saved = PlanSchemaFingerprints::capture(&references, &SchemaRegistry::new());
        let mut current = SchemaRegistry::new();
        current.insert(
            "provider",
            schema("example.Gone", "name")
                .validate()
                .expect("test schema must be valid"),
        );

        let mismatch = saved
            .verify(&references, &current)
            .expect_err("schema presence changes must invalidate the plan");
        assert_eq!(
            mismatch.providers["provider"],
            vec!["managed `example.Gone` (schema is now present)"]
        );
    }

    #[test]
    fn verify_reports_mismatches_from_every_provider() {
        let references = PlanSchemaReferences(BTreeSet::from([
            reference("alpha", "example.One"),
            reference("beta", "example.Two"),
        ]));
        let mut planned = SchemaRegistry::new();
        planned.insert(
            "alpha",
            schema("example.One", "old")
                .validate()
                .expect("test schema must be valid"),
        );
        planned.insert(
            "beta",
            schema("example.Two", "old")
                .validate()
                .expect("test schema must be valid"),
        );
        let saved = PlanSchemaFingerprints::capture(&references, &planned);

        let mut current = SchemaRegistry::new();
        current.insert(
            "alpha",
            schema("example.One", "new")
                .validate()
                .expect("test schema must be valid"),
        );
        current.insert(
            "beta",
            schema("example.Two", "new")
                .validate()
                .expect("test schema must be valid"),
        );
        let mismatch = saved
            .verify(&references, &current)
            .expect_err("both providers changed");
        assert_eq!(
            mismatch
                .providers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
    }
}

pub(crate) fn collect_historical_display_attributes(
    plan: &Plan,
    managed_current: &HashMap<ResourceId, Current>,
    state_file: Option<&StateFile>,
) -> HistoricalDisplayAttributes {
    let mut display = HistoricalDisplayAttributes::for_plan(plan);
    let moved_origins = MovedOrigins::from_plan(plan);

    for effect in plan.effects() {
        for key in effect.deleted_resource_attribute_keys() {
            match key.generation() {
                carina_core::effect::EffectGeneration::Current => {
                    if let Some(current) = managed_current.get(key.id()) {
                        let state = match current {
                            Current::Absent(state) => state.state(),
                            Current::Present(present) => present.state(),
                            Current::CreateInFlight(_pending) => continue,
                            Current::MissingIdentifier(state) => state.state(),
                        };
                        let persisted_id = moved_origins.origin_for(key.id());
                        let persisted_row =
                            state_file.and_then(|file| file.find_resource_for_id(persisted_id));
                        let persisted = persisted_row.and_then(ResourceState::persisted_attributes);
                        let empty_nested_write_only =
                            carina_core::provider::NestedWriteOnlyRecord::new();
                        let nested_write_only = persisted_row
                            .map(ResourceState::nested_write_only)
                            .unwrap_or(&empty_nested_write_only);
                        display.set_delete_from_provider_and_persisted(
                            key,
                            &state.attributes,
                            persisted,
                            nested_write_only,
                        );
                    }
                }
                carina_core::effect::EffectGeneration::Deposed(deposed_key) => {
                    if let Some(attrs) = state_file.and_then(|sf| {
                        deposed_attributes_for_display(sf, key.id(), key.identifier(), deposed_key)
                    }) {
                        display.set_delete_from_persisted(key, &attrs);
                    }
                }
            }
        }

        // Update::from has already crossed the plan-input canonicalization
        // boundary (including reconstruction of partial-read Unknown values).
        // Mask that shape in place instead of replacing it with the raw
        // refreshed state used by Delete effects.
        if let Effect::Update { from, .. } = effect {
            let persisted_id = moved_origins.origin_for(&from.id);
            let persisted = state_file
                .and_then(|file| file.find_resource_for_id(persisted_id))
                .and_then(ResourceState::persisted_attributes);
            display.set_update_from_provider_and_persisted(
                from.id.clone(),
                &from.attributes,
                persisted,
            );
        }
    }

    display
}

/// Resolves a plan address to the state row that owns its persisted history.
///
/// The mapping cannot be supplied independently of a plan: move effects are
/// the sole authority for historical state-row ownership during planning,
/// saved-plan verification, and writeback.
pub(crate) struct MovedOrigins {
    by_destination: HashMap<ResourceId, ResourceId>,
}

impl MovedOrigins {
    pub(crate) fn from_plan(plan: &Plan) -> Self {
        let by_destination = plan
            .effects()
            .iter()
            .filter_map(|effect| match effect {
                Effect::Move { from, to } => Some((to.as_inner().clone(), from.as_inner().clone())),
                _ => None,
            })
            .collect();
        Self { by_destination }
    }

    pub(crate) fn origin_for<'a>(&'a self, id: &'a ResourceId) -> &'a ResourceId {
        self.by_destination.get(id).unwrap_or(id)
    }

    pub(crate) fn find_persisted_row<'a>(
        &self,
        state_file: &'a StateFile,
        id: &ResourceId,
    ) -> Option<&'a carina_state::ResourceState> {
        state_file.find_resource_for_id(self.origin_for(id))
    }

    /// Resolve the row by state address while deliberately retaining its
    /// stored provider-instance routing. Replacement writeback uses this when
    /// a create changes provider instances and the old instance must be
    /// deposed through its original route.
    pub(crate) fn find_persisted_row_at_address<'a>(
        &self,
        state_file: &'a StateFile,
        id: &ResourceId,
    ) -> Option<&'a carina_state::ResourceState> {
        let origin = self.origin_for(id);
        state_file.find_resource(
            &origin.provider,
            &origin.resource_type,
            origin.identity_str()?,
        )
    }
}

type PersistedResourceLookup<'a> =
    HashMap<(&'a str, &'a str, &'a str, Option<&'a str>), &'a carina_state::ResourceState>;

fn persisted_resource_for_plan_address<'a>(
    persisted_resources: Option<&PersistedResourceLookup<'a>>,
    moved_origins: &MovedOrigins,
    id: &ResourceId,
) -> Option<&'a carina_state::ResourceState> {
    let persisted_id = moved_origins.origin_for(id);
    let identity = persisted_id.identity_str()?;
    persisted_resources?
        .get(&(
            persisted_id.provider.as_str(),
            persisted_id.resource_type.as_str(),
            identity,
            persisted_id.provider_instance.as_deref(),
        ))
        .copied()
}

fn deposed_attributes_for_display(
    state_file: &StateFile,
    id: &ResourceId,
    identifier: &str,
    key: &carina_state::DeposedKey,
) -> Option<carina_core::provider::PersistedAttributes> {
    let row = state_file.find_resource_for_id(id)?;
    let deposed = row.deposed.iter().find(|deposed| {
        deposed.key == *key
            && deposed.identifier == identifier
            && deposed.provider_instance == id.provider_instance
    })?;
    Some(deposed.persisted_attributes().clone())
}

#[cfg(test)]
mod historical_display_tests {
    use super::*;
    use carina_core::resource::{DeferredValue, ResolvedResource};

    #[test]
    fn production_historical_display_combines_desired_and_persisted_secret_shapes() {
        const PLAINTEXT: &str = "hunter2";
        let id = ResourceId::with_identity("mock.test.resource", "masked-update");
        let from = State::existing(
            id.clone(),
            HashMap::from([(
                "comment".to_string(),
                Value::Concrete(ConcreteValue::String(PLAINTEXT.into())),
            )]),
        )
        .with_identifier("masked-update");
        let to = ResolvedResource::new(
            Resource::new("mock.test.resource", "masked-update").with_attribute(
                "comment",
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("rotated".into()),
                )))),
            ),
        );

        let mut plan = Plan::new();
        plan.add(Effect::Update {
            identifier: crate::fixture_applied_identifier("masked-update"),
            from: Box::new(from.clone()),
            to,
            changed_attributes: vec!["comment".to_string()],
        });
        let current_states = HashMap::from([(id, Current::from_state(from))]);

        // Exercise the same collector and row builder used by CLI and TUI
        // production paths. There is deliberately no persisted state shape:
        // the desired Secret shape must still mask the provider plaintext.
        let historical = collect_historical_display_attributes(&plan, &current_states, None);
        let app = carina_tui::App::new(
            &plan,
            &carina_core::schema::SchemaRegistry::new(),
            &historical,
            None,
        );
        let rendered = format!("{:?}", app.nodes[0].detail_rows);

        assert!(
            !rendered.contains(PLAINTEXT),
            "detail rows leaked: {rendered}"
        );
        assert!(
            rendered.contains("(secret)"),
            "detail rows were not masked: {rendered}"
        );
    }
}

/// Serializable `(binding, target)` pair for a `wait` declaration —
/// the value-layer half of a wait binding (carina#3085). The parser
/// `carina_core::parser::WaitBinding` is not `Serialize`/`Deserialize`
/// (pulling serde into `carina-core::parser::ast` has wider blast
/// radius than warranted), so the plan file persists this minimal
/// projection, mirroring [`UpstreamSource`]'s rationale. Converted to
/// `carina_core::binding_index::WaitAliasSpec` at apply-from-plan time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanWaitBinding {
    pub binding: String,
    pub target: String,
}

/// Serializable representation of an `upstream_state` declaration's
/// source directory — needed because `parser::ast::UpstreamState` is
/// not `Serialize`/`Deserialize` and pulling a `serde` derive into
/// `carina-core::parser::ast` would have wider blast radius than this
/// PR wants. Constructed once at plan time and consumed at
/// apply-from-plan time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpstreamSource {
    pub binding: String,
    pub source: std::path::PathBuf,
}

/// Typed saved-plan snapshot with an explicit ownership tag.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CurrentStateEntry {
    Managed {
        id: ResourceId,
        #[serde(with = "managed_current_serde")]
        current: SavedManagedCurrent,
    },
}

/// Current-state snapshots after every create-in-flight marker has been
/// reconciled with the locked state file. Only this type exposes a managed
/// [`Current`].
#[derive(Debug, Clone)]
pub(crate) enum ReloadedCurrentStateEntry {
    Managed {
        id: ResourceId,
        current: Box<Current>,
    },
}

#[cfg(test)]
impl ReloadedCurrentStateEntry {
    pub(crate) fn managed(id: ResourceId, state: State) -> Self {
        Self::Managed {
            id,
            current: Box::new(Current::from_state(state)),
        }
    }

    pub(crate) fn managed_current(id: ResourceId, current: Current) -> Self {
        Self::Managed {
            id,
            current: Box::new(current),
        }
    }
}

/// A decoded saved plan that has crossed the locked-state reload boundary.
///
/// The embedded [`PlanFileContents::current_states`] has been consumed and is never
/// exposed through this type. Runtime current states live only in the sibling
/// `current_states` field below. Contents and provider evidence are stored as
/// siblings so apply-time mutation cannot make an in-memory [`PlanFile`]
/// inconsistent with its evidence.
#[derive(Debug)]
pub(crate) struct ReloadedPlanFile {
    contents: PlanFileContents,
    evidence: ProviderEvidence,
    current_states: Vec<ReloadedCurrentStateEntry>,
}

impl ReloadedPlanFile {
    pub(crate) fn verify_schema_fingerprints(
        &self,
        references: &PlanSchemaReferences,
        schemas: &carina_core::schema::SchemaRegistry,
    ) -> Result<(), PlanSchemaFingerprintMismatch> {
        self.evidence
            .schema_fingerprints
            .verify(references, schemas)
    }

    pub(crate) fn provider_artifacts(&self) -> &PlanProviderArtifacts {
        &self.evidence.provider_artifacts
    }

    pub(crate) fn provider_configs(&self) -> &SavedProviderConfigs {
        &self.contents.provider_configs
    }

    pub(crate) fn rehydrate_provider_config_secrets(
        &mut self,
        evaluated: &SavedProviderConfigs,
    ) -> Result<(), carina_core::value::SecretRehydrationSiteError> {
        self.contents.provider_configs = self
            .contents
            .provider_configs
            .rehydrate_secrets(evaluated)?;
        Ok(())
    }

    pub(crate) fn plan(&self) -> &Plan {
        &self.contents.plan
    }

    pub(crate) fn plan_mut(&mut self) -> &mut Plan {
        &mut self.contents.plan
    }

    pub(crate) fn sorted_resources(&self) -> &[Resource] {
        &self.contents.sorted_resources
    }

    pub(crate) fn sorted_resources_mut(&mut self) -> &mut Vec<Resource> {
        &mut self.contents.sorted_resources
    }

    pub(crate) fn unresolved_resources(&self) -> &[Resource] {
        &self.contents.unresolved_resources
    }

    pub(crate) fn unresolved_resources_mut(&mut self) -> &mut Vec<Resource> {
        &mut self.contents.unresolved_resources
    }

    pub(crate) fn compositions(&self) -> &[Composition] {
        &self.contents.compositions
    }

    pub(crate) fn compositions_mut(&mut self) -> &mut Vec<Composition> {
        &mut self.contents.compositions
    }

    pub(crate) fn data_sources(&self) -> &[DataSource] {
        &self.contents.data_sources
    }

    pub(crate) fn data_sources_mut(&mut self) -> &mut Vec<DataSource> {
        &mut self.contents.data_sources
    }

    pub(crate) fn data_source_origins(&self) -> &[DataSource] {
        &self.contents.data_source_origins
    }

    pub(crate) fn data_source_origins_mut(&mut self) -> &mut Vec<DataSource> {
        &mut self.contents.data_source_origins
    }

    pub(crate) fn data_source_values(&self) -> &[SavedDataSourceValue] {
        &self.contents.data_source_values
    }

    pub(crate) fn data_source_values_mut(&mut self) -> &mut [SavedDataSourceValue] {
        &mut self.contents.data_source_values
    }

    pub(crate) fn upstream_snapshot(&self) -> &HashMap<String, HashMap<String, Value>> {
        &self.contents.upstream_snapshot
    }

    pub(crate) fn upstream_snapshot_mut(&mut self) -> &mut HashMap<String, HashMap<String, Value>> {
        &mut self.contents.upstream_snapshot
    }

    pub(crate) fn upstream_sources(&self) -> &[UpstreamSource] {
        &self.contents.upstream_sources
    }

    pub(crate) fn wait_bindings(&self) -> &[PlanWaitBinding] {
        &self.contents.wait_bindings
    }

    pub(crate) fn current_states(&self) -> &[ReloadedCurrentStateEntry] {
        &self.current_states
    }

    pub(crate) fn current_states_mut(&mut self) -> &mut [ReloadedCurrentStateEntry] {
        &mut self.current_states
    }
}

/// Opaque managed current-state snapshot stored in a saved plan. It never
/// exposes a [`Current`]: the consuming locked-state reload is the only
/// conversion into runtime current state.
#[derive(Debug, Clone)]
pub struct SavedManagedCurrent(SavedManagedCurrentKind);

#[derive(Debug, Clone)]
enum SavedManagedCurrentKind {
    Absent(Box<State>),
    Present(Box<State>),
    MissingIdentifier(Box<State>),
    CreateInFlightMarker,
}

enum ReloadSavedManagedCurrent {
    Snapshot(Box<Current>),
    CreateInFlightMarker,
}

impl SavedManagedCurrent {
    pub fn from_current(current: Current) -> Self {
        let saved = match current {
            Current::Absent(state) => {
                SavedManagedCurrentKind::Absent(Box::new(state.state().clone()))
            }
            Current::Present(present) => {
                let mut state = present.state().clone();
                state.identifier = Some(present.identifier().as_str().to_owned());
                SavedManagedCurrentKind::Present(Box::new(state))
            }
            Current::CreateInFlight(_pending) => SavedManagedCurrentKind::CreateInFlightMarker,
            Current::MissingIdentifier(state) => {
                SavedManagedCurrentKind::MissingIdentifier(Box::new(state.state().clone()))
            }
        };
        Self(saved)
    }

    fn planned_in_flight(&self) -> bool {
        matches!(self.0, SavedManagedCurrentKind::CreateInFlightMarker)
    }

    fn into_reload_kind(self) -> ReloadSavedManagedCurrent {
        match self.0 {
            SavedManagedCurrentKind::Absent(state)
            | SavedManagedCurrentKind::Present(state)
            | SavedManagedCurrentKind::MissingIdentifier(state) => {
                ReloadSavedManagedCurrent::Snapshot(Box::new(Current::from_state(*state)))
            }
            SavedManagedCurrentKind::CreateInFlightMarker => {
                ReloadSavedManagedCurrent::CreateInFlightMarker
            }
        }
    }

    fn contains_redacted_secret(&self, state_contains: impl FnOnce(&State) -> bool) -> bool {
        match &self.0 {
            SavedManagedCurrentKind::Absent(state)
            | SavedManagedCurrentKind::Present(state)
            | SavedManagedCurrentKind::MissingIdentifier(state) => state_contains(state),
            SavedManagedCurrentKind::CreateInFlightMarker => false,
        }
    }

    fn nested_id(&self) -> Option<&ResourceId> {
        match &self.0 {
            SavedManagedCurrentKind::Absent(state)
            | SavedManagedCurrentKind::Present(state)
            | SavedManagedCurrentKind::MissingIdentifier(state) => Some(&state.id),
            SavedManagedCurrentKind::CreateInFlightMarker => None,
        }
    }

    #[cfg(test)]
    fn create_in_flight_marker() -> Self {
        Self(SavedManagedCurrentKind::CreateInFlightMarker)
    }
}

mod managed_current_serde {
    use super::{SavedManagedCurrent, SavedManagedCurrentKind};
    use carina_core::resource::{Current, State};
    use serde::de::Error as _;
    use serde::ser::SerializeStruct as _;
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<State>,
    }

    pub fn serialize<S>(current: &SavedManagedCurrent, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut wire = serializer.serialize_struct("Current", 2)?;
        match &current.0 {
            SavedManagedCurrentKind::CreateInFlightMarker => {
                wire.serialize_field("status", "create_in_flight")?;
            }
            SavedManagedCurrentKind::Absent(state) => {
                wire.serialize_field("status", "absent")?;
                wire.serialize_field("state", state)?;
            }
            SavedManagedCurrentKind::Present(state) => {
                wire.serialize_field("status", "present")?;
                wire.serialize_field("state", state)?;
            }
            SavedManagedCurrentKind::MissingIdentifier(state) => {
                wire.serialize_field("status", "missing_identifier")?;
                wire.serialize_field("state", state)?;
            }
        }
        wire.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<SavedManagedCurrent, D::Error>
    where
        D: Deserializer<'de>,
    {
        let Wire { status, state } = Wire::deserialize(deserializer)?;
        if status == "create_in_flight" {
            if state.is_some() {
                return Err(D::Error::custom(
                    "create-in-flight saved-plan markers must not contain state or a provider token",
                ));
            }
            return Ok(SavedManagedCurrent(
                SavedManagedCurrentKind::CreateInFlightMarker,
            ));
        }
        let state = state.ok_or_else(|| {
            D::Error::custom(format!(
                "managed current-state tag {status:?} requires a state payload"
            ))
        })?;
        let current = Current::from_state(state);
        let classified_status = match &current {
            Current::Absent(_state) => "absent",
            Current::Present(_present) => "present",
            Current::CreateInFlight(_pending) => {
                return Err(D::Error::custom(
                    "saved-plan state payload cannot construct an in-flight create",
                ));
            }
            Current::MissingIdentifier(_state) => "missing_identifier",
        };
        if status != classified_status {
            return Err(D::Error::custom(format!(
                "managed current-state tag {status:?} does not match classified state {classified_status:?}"
            )));
        }
        Ok(SavedManagedCurrent::from_current(current))
    }
}

#[cfg(test)]
mod managed_current_serde_tests {
    use super::{
        CurrentStateEntry, PlanFile, PlanFileContents, ReloadSavedManagedCurrent,
        ReloadedCurrentStateEntry, SavedManagedCurrent,
    };
    use carina_core::provider::{AppliedIdentifier, Sent};
    use carina_core::resource::{
        Current, ResolvedResource, Resource, ResourceId, State, fixture_present_current,
    };
    use carina_state::{ResourceState, StateFile};

    fn plan_file_with_current(id: ResourceId, current: SavedManagedCurrent) -> PlanFile {
        PlanFileContents {
            version: PlanFile::CURRENT_VERSION,
            carina_version: "test".to_string(),
            timestamp: "2026-10-23T08:00:00Z".to_string(),
            source_path: "test.crn".to_string(),
            state_lineage: None,
            state_serial: None,
            provider_configs: serde_json::from_value(serde_json::json!([]))
                .expect("empty saved provider configs"),
            backend_config: None,
            plan: carina_core::plan::Plan::new(),
            sorted_resources: Vec::new(),
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: Vec::new(),
            data_source_origins: Vec::new(),
            data_source_values: Vec::new(),
            current_states: vec![CurrentStateEntry::Managed { id, current }],
            upstream_snapshot: Default::default(),
            upstream_sources: Vec::new(),
            wait_bindings: Vec::new(),
        }
        .record(&crate::wiring::WiringContext::new_for_test(Vec::new(), &[]))
        .expect("test plan provider evidence must be valid")
    }

    #[test]
    fn present_round_trip_uses_typed_identifier_when_raw_identifier_differs() {
        let id = ResourceId::with_provider_identity("aws", "s3.Bucket", "logs", None);
        let raw = State::existing(id.clone(), Default::default())
            .with_identifier("raw-provider-identifier");
        let typed = AppliedIdentifier::new("typed-provider-identifier")
            .expect("fixture identifier is non-empty");
        let entry = CurrentStateEntry::Managed {
            id: id.clone(),
            current: SavedManagedCurrent::from_current(fixture_present_current(raw, typed)),
        };

        let encoded = serde_json::to_value(entry).expect("managed current serializes");
        assert_eq!(
            encoded["current"]["state"]["identifier"],
            serde_json::json!("typed-provider-identifier"),
            "the typed Present identifier must be authoritative on disk",
        );

        let decoded: CurrentStateEntry =
            serde_json::from_value(encoded).expect("managed current round-trips");
        match decoded {
            CurrentStateEntry::Managed {
                id: decoded_id,
                current,
            } => {
                assert_eq!(decoded_id, id);
                let ReloadSavedManagedCurrent::Snapshot(current) = current.into_reload_kind()
                else {
                    panic!("Present serialized as an in-flight marker")
                };
                match *current {
                    Current::Present(present) => {
                        assert_eq!(present.identifier().as_str(), "typed-provider-identifier");
                        assert_eq!(
                            present.state().identifier.as_deref(),
                            Some("typed-provider-identifier"),
                        );
                    }
                    Current::Absent(state) => {
                        panic!("Present serialized as Absent: {state:?}")
                    }
                    Current::CreateInFlight(pending) => {
                        panic!("Present serialized as in flight: {pending:?}")
                    }
                    Current::MissingIdentifier(state) => {
                        panic!("Present serialized without an identifier: {state:?}")
                    }
                }
            }
        }
    }

    #[test]
    fn saved_plan_rejects_in_flight_status_change_in_both_directions() {
        let resource = ResolvedResource::new(Resource::with_provider(
            "mock",
            "test.Resource",
            "pending",
            None,
        ));
        let id = resource.id.clone();
        let pending = crate::fixture_in_flight_create(
            &id,
            "state-owned-token",
            None,
            Sent::AllRequestAttributes,
        );
        let row = ResourceState::from_in_flight(&resource, &pending, None, None, None)
            .expect("in-flight fixture must encode");
        let mut locked_in_flight = StateFile::new();
        locked_in_flight
            .upsert_resource(row)
            .expect("state fixture must be valid");

        let planned_absent = plan_file_with_current(
            id.clone(),
            SavedManagedCurrent::from_current(Current::absent(id.clone())),
        );
        let became_in_flight = planned_absent
            .reload_in_flight_from_locked_state(Some(&locked_in_flight))
            .expect_err("absent to in-flight must invalidate the saved plan");
        assert!(
            became_in_flight.contains("false to true"),
            "{became_in_flight}"
        );

        let planned_in_flight =
            plan_file_with_current(id, SavedManagedCurrent::create_in_flight_marker());
        let ended = planned_in_flight
            .reload_in_flight_from_locked_state(None)
            .expect_err("in-flight to absent must invalidate the saved plan");
        assert!(ended.contains("true to false"), "{ended}");
    }

    #[test]
    fn saved_plan_rejects_new_in_flight_address_missing_from_snapshot() {
        let planned_id =
            ResourceId::with_provider_identity("mock", "test.Resource", "planned", None);
        let unexpected_resource = ResolvedResource::new(Resource::with_provider(
            "mock",
            "test.Resource",
            "unexpected",
            None,
        ));
        let unexpected_id = unexpected_resource.id.clone();
        let pending = crate::fixture_in_flight_create(
            &unexpected_id,
            "state-owned-token",
            None,
            Sent::AllRequestAttributes,
        );
        let row = ResourceState::from_in_flight(&unexpected_resource, &pending, None, None, None)
            .expect("in-flight fixture must encode");
        let mut locked = StateFile::new();
        locked
            .upsert_resource(row)
            .expect("state fixture must be valid");
        let plan = plan_file_with_current(
            planned_id.clone(),
            SavedManagedCurrent::from_current(Current::absent(planned_id)),
        );

        let error = plan
            .reload_in_flight_from_locked_state(Some(&locked))
            .expect_err("a new in-flight address must invalidate the saved plan");
        assert!(error.contains(&unexpected_id.to_string()), "{error}");
        assert!(error.contains("false to true"), "{error}");
    }

    #[test]
    fn saved_plan_reloads_moved_in_flight_row_from_its_source_address() {
        use carina_core::effect::Effect;
        use carina_core::resource::ResolvedResourceId;

        let source = ResourceId::with_provider_identity("mock", "test.Resource", "source", None);
        let destination =
            ResourceId::with_provider_identity("mock", "test.Resource", "destination", None);
        let source_resource = ResolvedResource::new(Resource::from_id(source.clone()));
        let pending = crate::fixture_in_flight_create(
            &source,
            "state-owned-token",
            None,
            Sent::AllRequestAttributes,
        );
        let row = ResourceState::from_in_flight(&source_resource, &pending, None, None, None)
            .expect("in-flight fixture must encode");
        let mut locked = StateFile::new();
        locked
            .upsert_resource(row)
            .expect("state fixture must be valid");
        let mut contents = plan_file_with_current(
            destination.clone(),
            SavedManagedCurrent::create_in_flight_marker(),
        )
        .into_contents();
        contents.plan.add(Effect::Move {
            from: ResolvedResourceId::new(source),
            to: ResolvedResourceId::new(destination.clone()),
        });
        let plan = contents
            .record(&crate::wiring::WiringContext::new_for_test(Vec::new(), &[]))
            .expect("test plan provider evidence must be valid");

        let plan = plan
            .reload_in_flight_from_locked_state(Some(&locked))
            .expect("a moved continuation must reload from its source row");
        let ReloadedCurrentStateEntry::Managed { id, current } = &plan.current_states()[0];
        assert_eq!(id, &destination);
        let Current::CreateInFlight(pending) = current.as_ref() else {
            panic!("moved marker must reload the state-owned continuation");
        };
        assert_eq!(pending.id(), &destination);
        assert_eq!(
            pending.token().expose_for_provider_call(),
            "state-owned-token"
        );
    }
}

impl CurrentStateEntry {
    pub fn managed(id: ResourceId, state: State) -> Self {
        Self::Managed {
            id,
            current: SavedManagedCurrent::from_current(Current::from_state(state)),
        }
    }

    pub fn id(&self) -> &ResourceId {
        match self {
            Self::Managed {
                id,
                current: _current,
            } => id,
        }
    }
}

#[derive(Debug)]
enum PlanSaveError {
    Serialization(carina_core::value::SerializationError),
    ProviderEvidence(ProviderEvidenceError),
}

impl From<carina_core::value::SerializationError> for PlanSaveError {
    fn from(error: carina_core::value::SerializationError) -> Self {
        Self::Serialization(error)
    }
}

impl From<ProviderEvidenceError> for PlanSaveError {
    fn from(error: ProviderEvidenceError) -> Self {
        Self::ProviderEvidence(error)
    }
}

impl std::fmt::Display for PlanSaveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Serialization(error) => error.fmt(formatter),
            Self::ProviderEvidence(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PlanSaveError {}

fn build_plan_file<E>(
    path: &Path,
    parsed: &carina_core::parser::File<E>,
    provider_configs: &ResolvedProviderConfigs,
    backend_config: Option<BackendConfig>,
    state_file: &Option<StateFile>,
    ctx: &crate::wiring::PlanContext,
    wiring: &WiringContext,
) -> Result<PlanFile, PlanSaveError> {
    assert_eq!(
        ctx.data_sources.len(),
        parsed.data_sources.len(),
        "plan data sources must stay paired with their authored origins"
    );
    assert_eq!(
        ctx.unresolved_resources.len(),
        ctx.sorted_resources.len(),
        "plan resources must stay paired with their authored origins"
    );
    let source_path = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string();

    let desired_resources: HashMap<ResourceId, Resource> = ctx
        .sorted_resources
        .iter()
        .map(|resource| (resource.id.clone(), resource.as_inner().clone()))
        .collect();
    let provider_configs = SavedProviderConfigs::from(provider_configs).redact_secrets()?;
    for (binding, attributes) in &ctx.upstream_snapshot {
        for (key, value) in attributes {
            // Export validation rejects secret-valued exports before an
            // upstream state snapshot can be produced. Keep this guard for
            // malformed/tampered in-memory input.
            carina_core::value::reject_secret_in_saved_plan_field(
                value,
                format!("upstream_state {binding}.{key}"),
            )?;
        }
    }
    let upstream_snapshot = ctx.upstream_snapshot.clone();
    let persisted_resources = state_file.as_ref().map(|state_file| {
        state_file
            .resources()
            .iter()
            .map(|resource| {
                (
                    (
                        resource.provider.as_str(),
                        resource.resource_type.as_str(),
                        resource.identity.as_str(),
                        resource.directives.provider_instance.as_deref(),
                    ),
                    resource,
                )
            })
            .collect::<PersistedResourceLookup<'_>>()
    });
    let moved_origins = MovedOrigins::from_plan(&ctx.plan);
    let redact_current_state = |id: &ResourceId,
                                state: &State|
     -> Result<State, carina_core::value::SerializationError> {
        let persisted =
            persisted_resource_for_plan_address(persisted_resources.as_ref(), &moved_origins, id);
        // A moved resource's previous state row still lives at its source
        // address until execution writes the destination row.
        let redacted = match persisted.and_then(ResourceState::persisted_attributes) {
            Some(persisted_attributes) => {
                redact_secrets_in_state_using_persisted(state, persisted_attributes)?
            }
            None => state.clone(),
        };
        let redacted = match desired_resources.get(id) {
            Some(desired) => redact_secrets_in_state_using_resource(&redacted, desired)?,
            None if persisted.is_some() => redacted,
            None => redact_secrets_in_state(&redacted)?,
        };
        Ok(redacted)
    };
    let redacted_managed_states = ctx
        .managed_current
        .iter()
        .filter_map(|(id, current)| {
            let state = match current {
                Current::Absent(state) => Some(state.state()),
                Current::Present(present) => Some(present.state()),
                Current::CreateInFlight(_pending) => None,
                Current::MissingIdentifier(state) => Some(state.state()),
            };
            state.map(|state| redact_current_state(id, state).map(|state| (id.clone(), state)))
        })
        .collect::<Result<HashMap<_, _>, carina_core::value::SerializationError>>()?;
    let redacted_managed_current = ctx
        .managed_current
        .iter()
        .map(|(id, current)| {
            let mut current = current.clone();
            if let Some(attributes) = current.attributes_mut() {
                *attributes = redacted_managed_states[id].attributes.clone();
            }
            (id.clone(), current)
        })
        .collect::<HashMap<_, _>>();
    let redacted_data_source_states = ctx
        .data_source_values
        .read_states()
        .map(|(id, state)| Ok((id.clone(), redact_current_state(id, state)?)))
        .collect::<Result<HashMap<_, _>, carina_core::value::SerializationError>>()?;
    let redacted_sorted_resources = ctx
        .sorted_resources
        .iter()
        .map(|resource| {
            let persisted = persisted_resource_for_plan_address(
                persisted_resources.as_ref(),
                &moved_origins,
                &resource.id,
            );
            match persisted {
                Some(persisted) => match persisted.persisted_attributes() {
                    Some(attributes) => {
                        redact_secrets_in_resource_reusing_persisted_hashes(resource, attributes)
                    }
                    None => redact_secrets_in_resource(resource),
                },
                None => redact_secrets_in_resource(resource),
            }
        })
        .collect::<Result<Vec<_>, carina_core::value::SerializationError>>()?;
    let redacted_desired_resources: HashMap<ResourceId, Resource> = redacted_sorted_resources
        .iter()
        .map(|resource| (resource.id.clone(), resource.clone()))
        .collect();

    let contents = PlanFileContents {
        // carina#4045: bumped 25→26 — saved plans persist the exact loaded
        // provider artifacts used to compute the plan. carina#4060 keeps the
        // format at 26 because it only tightens validation.
        // carina#3993: bumped 24→25 — saved plans persist every data-source
        // outcome so apply never re-derives it from a partial graph.
        // carina#3916: bumped 23→24 — saved plans remove the redundant wait
        // target schema kind now that waits are managed-only.
        // carina#3983: bumped 22→23 — saved plans bind every referenced
        // managed/data-source type to the canonical provider schema used by
        // planning. Apply must verify this snapshot before provider I/O.
        //
        // carina#3909: bumped 20→21 — in-flight create snapshots store only
        // their resource address and a marker. Apply reloads the continuation
        // token from locked state and rejects either direction of status change.
        //
        // carina#3909 preparation: bumped 19→20 — saved current-state entries now carry
        // explicit managed/data-source ownership and managed lifecycle tags.
        // V19 inferred ownership from `data_sources`, which could silently
        // reinterpret an entry whose id was absent from that list.
        //
        // carina#3913: bumped 18→19 — abandoned deposed generations now
        // serialize only their resource address and generation key. The
        // removed field was necessarily an invalid empty provider identifier;
        // reject v18 files so the saved-plan shape stays explicit.
        // carina#3855: bumped 15→16 — unquoted wait RHS identifiers are
        // persisted as typed enum values. A v15 plan may contain the same
        // dotted text as an ordinary String, which cannot be distinguished
        // safely from an intentionally quoted comparison; require replanning.
        //
        // carina#3863 / carina#3865: bumped 14→15 — secret digests now
        // carry their salt, including upgraded legacy placeholders in embedded
        // current states. Saved-plan resource placeholders carry the exact
        // digest that state writeback must reuse. A v14 plan contains
        // context-derived digests and cannot satisfy those contracts.
        //
        // carina#3871: bumped 13→14 — orphan state and host metadata now
        // travel as one typed value until planning finishes. V13
        // `CurrentStateEntry` values may also contain legacy `_`-prefixed
        // host attributes, so they must be regenerated at the state migration
        // boundary instead of entering drift detection.
        //
        // carina#3869: bumped 12→13 — host metadata no longer lives in
        // provider-facing resource attributes. Default-tag provenance moved
        // to `Resource::default_tag_keys`, and resource type comes solely
        // from `Resource::id`. A v12 plan must be regenerated rather than
        // mixing its synthetic-attribute representation with v13 apply.
        //
        // carina#3861 / carina#2674: bumped 11→12 — secrets now use typed
        // placeholders, and saved-plan apply must re-evaluate and verify them.
        // A v11 plan represents secret hashes as ordinary strings and could
        // send those hashes to a provider, so it must never cross this seam.
        //
        // carina#3805: bumped 10→11 — `compositions` stores the pending module
        // constraints required by apply, and `data_source_origins` preserves
        // the authored input paired with each canonical plan-time data source
        // so apply can validate only values that become known at runtime. A
        // v10 plan can enforce neither provider-boundary guarantee.
        //
        // Phase 9: bumped 8→9 — `data_sources` now stores the exact
        // differ/executor view: resolved refresh-time reads and unresolved
        // apply-time reads. Older v8 files may have serialized deferred reads
        // after parser resolution, which loses the metadata needed by
        // `apply --plan`.
        //
        // Phase 7: bumped 6→8 — saved plans no longer support the
        // legacy Replace effect shape and replacement display metadata
        // must survive plan redaction.
        //
        // carina#3486: bumped 5→6 — saved plans now persist
        // `unresolved_resources`, the pre-resolution snapshot needed by
        // apply-time dependency analysis and reference re-resolution.
        //
        // carina#3329: bumped 4→5 — `Effect::Import.identifier` is now
        // a tagged `Value`, not a plain string, so a pre-#3329 v4 plan
        // would otherwise fail to deserialize with an opaque serde
        // error. The version check at apply time produces a clean
        // "re-run plan" message instead.
        //
        // carina#3248: bumped 3→4 — saved plans now persist
        // `compositions` so the saved-plan apply path can rebuild
        // the same `ResolvedBindings` view as the live-apply path.
        // Older plans (version below current) are rejected with a
        // clear message pointing the user at re-running `plan`.
        version: PlanFile::CURRENT_VERSION,
        carina_version: env!("CARGO_PKG_VERSION").to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        source_path,
        state_lineage: state_file.as_ref().map(|s| s.lineage.clone()),
        state_serial: state_file.as_ref().map(|s| s.serial),
        provider_configs,
        backend_config,
        plan: redact_secrets_in_plan_with_desired_and_historical(
            &ctx.plan,
            &redacted_desired_resources,
            &redacted_managed_states,
        )?,
        sorted_resources: redacted_sorted_resources,
        unresolved_resources: ctx
            .unresolved_resources
            .iter()
            .zip(&ctx.sorted_resources)
            .map(|(source, evaluated)| {
                redact_secrets_in_resource_using_evaluated(source.as_resource(), evaluated)
            })
            .collect::<Result<Vec<_>, _>>()?,
        compositions: ctx
            .compositions
            .iter()
            .map(carina_core::value::redact_secrets_in_virtual)
            .collect::<Result<Vec<_>, _>>()?,
        data_sources: ctx
            .data_sources
            .iter()
            .map(carina_core::value::redact_secrets_in_data_source)
            .collect::<Result<Vec<_>, _>>()?,
        data_source_origins: ctx
            .data_sources
            .iter()
            .zip(&parsed.data_sources)
            .map(|(resolved, authored)| {
                let mut authored = authored.clone();
                authored.id = resolved.id.clone();
                carina_core::value::redact_secrets_in_data_source_using_evaluated(
                    &authored, resolved,
                )
            })
            .collect::<Result<Vec<_>, _>>()?,
        data_source_values: ctx
            .data_source_values
            .iter()
            .map(|(data_source, value)| SavedDataSourceValue {
                id: data_source.id.clone(),
                value: match value {
                    DataSourceValue::Read(_) => {
                        DataSourceValue::read(redacted_data_source_states[&data_source.id].clone())
                    }
                    DataSourceValue::DeferredToApply(unresolved) => {
                        DataSourceValue::deferred_to_apply(unresolved.clone())
                    }
                    DataSourceValue::BlockedByPlanError => DataSourceValue::blocked_by_plan_error(),
                },
            })
            .collect(),
        current_states: ctx
            .managed_current
            .keys()
            .map(|id| CurrentStateEntry::Managed {
                id: id.clone(),
                current: SavedManagedCurrent::from_current(redacted_managed_current[id].clone()),
            })
            .collect(),
        upstream_snapshot,
        upstream_sources: parsed
            .upstream_states
            .iter()
            .map(|us| UpstreamSource {
                binding: us.binding.clone(),
                source: us.source.clone(),
            })
            .collect(),
        wait_bindings: parsed
            .wait_bindings
            .iter()
            .map(|wb| PlanWaitBinding {
                binding: wb.binding.as_str().to_string(),
                target: wb.target.as_str().to_string(),
            })
            .collect(),
    };
    contents.record(wiring).map_err(PlanSaveError::from)
}

/// Format a failure from `build_plan_file` into the CLI's
/// "cannot save plan" diagnostic, suggesting the caller drop `flag` (for
/// example `--json` or `--out`) to fall back to display-only output.
fn format_plan_save_error(e: &PlanSaveError, flag: &str) -> String {
    match e {
        PlanSaveError::Serialization(
            carina_core::value::SerializationError::UnknownNotAllowed { reason, .. },
        ) => format!(
            "Cannot save plan: it depends on values that are not yet \
             applied ({reason}). Apply the upstream module(s) first, \
             or rerun without {flag}."
        ),
        PlanSaveError::Serialization(error) => error.to_string(),
        PlanSaveError::ProviderEvidence(error) => format!("Cannot save plan: {error}"),
    }
}

/// A point-in-time fingerprint of the state file captured immediately
/// after `plan` first reads state (T0). `plan` takes no state lock
/// (issue #3111: a lock on a read-only operation is overkill and would
/// serialize concurrent `plan`s), so a concurrent `apply`/`destroy`
/// can mutate state between this snapshot and when the plan is
/// displayed. Comparing this snapshot against a re-read at T1 lets
/// `plan` warn that its output may be stale (TOCTOU drift detection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateSnapshot {
    /// `StateFile::serial` — monotonically increasing per state write.
    serial: u64,
    /// `StateFile::lineage` — changes only if the state was recreated
    /// from scratch (e.g. a destroy + fresh apply, or `state` surgery).
    lineage: String,
}

impl StateSnapshot {
    /// Capture the snapshot from the state read at T0. `None` when no
    /// state exists yet (first-ever plan): there is nothing a
    /// concurrent writer could make stale, so drift detection is moot.
    pub(crate) fn capture(state: Option<&StateFile>) -> Option<Self> {
        state.map(|s| Self {
            serial: s.serial,
            lineage: s.lineage.clone(),
        })
    }
}

/// How the state changed between the T0 snapshot and the T1 re-read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StateDrift {
    /// Same lineage, higher serial: a concurrent `apply`/`destroy`
    /// wrote state while this `plan` was computing its diff.
    SerialAdvanced { from: u64, to: u64 },
    /// Lineage changed: the state was recreated entirely (destroy +
    /// fresh apply, or `carina state` surgery) under this `plan`.
    LineageChanged { from: String, to: String },
    /// State existed at T0 but is gone at T1 (deleted concurrently).
    StateRemoved,
}

impl StateDrift {
    /// User-facing warning line explaining the plan may be stale.
    pub(crate) fn warning(&self, project_dir: &Path) -> String {
        let detail = match self {
            StateDrift::SerialAdvanced { from, to } => {
                format!("state serial advanced {from} -> {to}")
            }
            StateDrift::LineageChanged { from, to } => {
                format!("state lineage changed {from} -> {to}")
            }
            StateDrift::StateRemoved => "state was removed".to_string(),
        };
        format!(
            "Warning: state changed during plan ({detail}); a concurrent \
             apply/destroy ran while this plan was being computed. The \
             plan output may be stale — re-run `{}`.",
            ProjectCommand::new("plan", project_dir)
        )
    }
}

/// Compare the T0 snapshot against the state re-read at T1 (just
/// before plan display) and classify any drift.
///
/// `before` is `None` only when no state existed at T0, in which case
/// nothing could have gone stale — return `None`. A lineage change is
/// reported in preference to a serial change because it is the
/// stronger signal (the entire state was replaced).
pub(crate) fn detect_state_drift(
    before: Option<&StateSnapshot>,
    after: Option<&StateFile>,
) -> Option<StateDrift> {
    let before = before?;
    match after {
        None => Some(StateDrift::StateRemoved),
        Some(after) => {
            if after.lineage != before.lineage {
                Some(StateDrift::LineageChanged {
                    from: before.lineage.clone(),
                    to: after.lineage.clone(),
                })
            } else if after.serial != before.serial {
                Some(StateDrift::SerialAdvanced {
                    from: before.serial,
                    to: after.serial,
                })
            } else {
                None
            }
        }
    }
}

/// Test-only rendezvous between `plan`'s T0 state read and its T1
/// re-read. Production builds never set the env var, so this is a
/// no-op (one failed `getenv`); when set by the drift e2e test it is a
/// file handshake — not a sleep — so the test can place a concurrent
/// writer *deterministically* in the T0..T1 window (a fixed sleep
/// would race the binary's own startup cost and flake). The binary
/// signals "T0 captured" by creating `<dir>/t0_done`, then blocks
/// until the test creates `<dir>/proceed`. The wait is bounded so a
/// broken test cannot hang the binary forever. It has no effect on
/// the concurrency contract.
async fn drift_detection_test_barrier() {
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(10);
    // Matches the test side's t0_done tolerance. Generous because the
    // repo runs process-per-test under heavily parallel nextest, where
    // the test thread can be starved between observing t0_done and
    // creating proceed; a tight bound here would flake. Unbounded is
    // unnecessary (a runaway test is already capped by nextest's slow
    // timeout) and this is test-only anyway, so it cannot hang prod.
    const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

    let Ok(dir) = std::env::var("CARINA_TEST_PLAN_DRIFT_HANDSHAKE_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let _ = std::fs::write(dir.join("t0_done"), b"");
    let proceed = dir.join("proceed");
    let deadline = tokio::time::Instant::now() + MAX_WAIT;
    while !proceed.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run_plan(
    path: &Path,
    out: Option<&Path>,
    image: Option<&ResolvedPlanImageTarget>,
    inline_image: Option<InlineImageRequest>,
    detail: DetailLevel,
    tui: bool,
    refresh: bool,
    json: bool,
    check_iam: bool,
    strict_iam: bool,
    provider_context: &ProviderContext,
) -> Result<bool, AppError> {
    let progress_sink = if json || tui {
        ProgressSink::MachineStdout
    } else {
        ProgressSink::Human
    };
    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let inference_errors = loaded.inference_errors;
    let duplicate_declarations = loaded.duplicate_declarations;
    let parsed = loaded.parsed;

    let base_dir = get_base_dir(path);
    let mut canonical_resources = crate::commands::validate_and_resolve_config_pair(
        parsed,
        loaded.unresolved_parsed,
        base_dir,
        false,
        &inference_errors,
        &duplicate_declarations,
    )?;
    let parsed = canonical_resources.parsed();

    let plan_backend = resolve_backend_for_read(
        base_dir,
        path,
        parsed.backend.as_ref(),
        BackendLockRequirement::RequirePresent,
        ReadBackendWarningPolicy::CommandBoundary { project_dir: path },
    )
    .await?;
    let plan_file_backend_config = plan_backend
        .selected_config()
        .map(carina_state::BackendConfig::to_parser_config);
    let use_locked_backend = plan_backend.migration_pending();
    let drift_note = use_locked_backend.then(|| {
        format!(
            "Backend migration pending: plan read state from the OLD backend recorded in \
             carina-backend.lock. Run `{}` before apply.",
            ProjectCommand::new("init --migrate-state", path)
        )
    });

    // Check for backend configuration and load state
    // Use local backend by default if no backend is configured
    let mut will_create_state_bucket = false;
    let mut state_bucket_name = String::new();
    let mut state_file: Option<StateFile> = None;

    if let Some(config) = plan_backend.selected_config() {
        let bucket_exists = plan_backend
            .bucket_exists()
            .await
            .map_err(AppError::Backend)?;

        if bucket_exists {
            // Try to load state from backend. `plan` is read-only and
            // already warns when an in-memory migration was applied,
            // so it drops the migration token via `into_state()` —
            // carina#3315 routes persistence through the lock-held
            // `apply` / `destroy` / `state refresh` paths.
            state_file = plan_backend
                .read_state()
                .await
                .map_err(AppError::Backend)?
                .map(|loaded| loaded.into_state());
        } else if use_locked_backend {
            return Err(AppError::Config(
                "Locked backend recorded in carina-backend.lock no longer exists. \
                 Restore the old state backend or revert backend.crn to match the lock before \
                 planning this migration."
                    .to_string(),
            ));
        } else {
            // Check if there's a matching s3_bucket resource defined
            let settings = config
                .as_s3()
                .ok_or("Backend does not manage an S3 bucket")?;
            let bucket_name = settings.bucket().to_string();

            let backend_resource_type = plan_backend
                .resource_type()
                .ok_or("Backend does not specify a resource type")?;
            #[rustfmt::skip]
            let has_bucket_resource = parsed.resources.iter().any(|r| { // allow: direct — plan-time reconciliation
                r.id.resource_type == backend_resource_type
                    && r.attributes
                        .get("bucket")
                        .is_some_and(|v| matches!(v, Value::Concrete(ConcreteValue::String(s)) if s.expose_plaintext() == bucket_name))
            });

            if !has_bucket_resource {
                let auto_create = settings.auto_create();

                if auto_create {
                    will_create_state_bucket = true;
                    state_bucket_name = bucket_name;
                } else {
                    let bucket = match settings.bucket_diagnostic_literal() {
                        Some(bucket) => format!("'{bucket}'"),
                        None => "value from env()".to_string(),
                    };
                    return Err(AppError::Config(format!(
                        "Backend bucket {bucket} not found and auto_create is disabled"
                    )));
                }
            }
        }
    } else {
        // Use local backend by default. Plan is read-only; drop any
        // pending-migration token (carina#3315 — lock-held callers
        // persist instead).
        state_file = plan_backend
            .read_state()
            .await
            .map_err(AppError::Backend)?
            .map(|loaded| loaded.into_state());
    }

    // T0 fingerprint; see `StateSnapshot` for the TOCTOU rationale.
    let state_snapshot_t0 = StateSnapshot::capture(state_file.as_ref());
    if let Some(state) = state_file.as_mut() {
        state.remove_legacy_data_source_rows(&parsed.data_sources);
    }

    // Show bootstrap plan if needed
    if will_create_state_bucket {
        let backend_provider = plan_backend
            .provider_name()
            .ok_or("Backend does not specify a provider name")?;
        let backend_resource_type = plan_backend
            .resource_type()
            .ok_or("Backend does not specify a resource type")?;
        progress_sink.line("Bootstrap Plan:".cyan().bold());
        progress_sink.line(format!(
            "  {} {} (state bucket)",
            "+".green(),
            format!(
                "{}.{}.{}",
                backend_provider, backend_resource_type, state_bucket_name
            )
            .green()
        ));
        progress_sink.line(format!(
            "  {} Resource definition will be added to .crn file",
            "→".cyan()
        ));
        progress_sink.blank_line();
    }

    let PreparedPlanIdentityInputs {
        wiring,
        state_blocks:
            crate::wiring::StateBlockResolution {
                claims: state_block_claims,
                targets: resolved_state_block_targets,
            },
        provider_inputs,
    } = prepare_plan_identity_and_provider_inputs(
        &mut canonical_resources,
        &mut state_file,
        base_dir,
        provider_context,
        UpstreamMissingStatePolicy::Lenient,
    )
    .await?;
    if !refresh {
        eprintln!(
            "{}",
            "Warning: using cached managed-resource state (--refresh=false). Data sources are still read, but the plan may not reflect actual managed infrastructure.".yellow()
        );
    }

    // carina#3132: deferred-for expansion runs inside
    // `create_plan_from_parsed_with_upstream_with_ctx` (post-refresh), which also
    // prints post-expansion warnings and returns the still-unresolved
    // loops via `ctx.residual_deferred_for`.
    let ctx = create_plan_from_parsed_with_upstream_with_ctx(
        &wiring,
        canonical_resources,
        provider_inputs.configs(),
        &state_file,
        refresh,
        progress_sink,
        provider_inputs.remote_bindings(),
        &state_block_claims,
        &resolved_state_block_targets,
    )
    .await?;
    let parsed = ctx.parsed();
    // TOCTOU drift detection (#3111). `plan` took no state lock, so a
    // concurrent apply/destroy may have written state while the diff
    // above was being computed. Re-read state now and compare against
    // the T0 fingerprint; warn (do not fail) if it moved — the plan is
    // a prediction, and apply re-locks + recomputes before mutating.
    //
    // Skipped entirely when no state existed at T0 (first-ever plan, or
    // a bootstrap run that will create the backend): there is no
    // baseline, so nothing could have gone stale and even a re-read
    // *failure* is not worth a "may be stale" warning.
    if let Some(snapshot_t0) = state_snapshot_t0.as_ref() {
        drift_detection_test_barrier().await;
        match plan_backend.read_state().await {
            Ok(state_t1) => {
                let state_t1 = state_t1.map(|loaded| loaded.into_state());
                if let Some(drift) = detect_state_drift(Some(snapshot_t0), state_t1.as_ref()) {
                    eprintln!("{}", drift.warning(path).yellow());
                }
            }
            // A failed re-read is not fatal: the plan already computed
            // against the T0 snapshot. Surface it so the user knows
            // drift could not be checked, but let the plan print.
            Err(e) => {
                eprintln!(
                    "{}",
                    format!(
                        "Warning: could not re-read state to check for \
                         concurrent changes ({e}); plan output may be stale."
                    )
                    .yellow()
                );
            }
        }
    }

    render_plan_errors_and_abort(&ctx.plan)?;

    let iam_preflight_result = if check_iam {
        let result =
            crate::commands::iam_preflight::run_iam_preflight(&ctx.plan, &ctx.provider, strict_iam)
                .await?;
        Some(result)
    } else {
        None
    };
    let iam_strict_failed = strict_iam
        && iam_preflight_result
            .as_ref()
            .is_some_and(crate::commands::iam_preflight::should_fail_strict);

    // Build display-safe historical attributes from current and persisted state.
    let historical_display =
        collect_historical_display_attributes(&ctx.plan, &ctx.managed_current, state_file.as_ref());

    // Resolve exports against the same pre-apply binding view used for their
    // display and reject secrets that were hidden behind references during
    // the earlier syntax-level validation pass.
    let export_wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();
    let resources_for_exports: Vec<Resource> = ctx
        .sorted_resources
        .iter()
        .map(|resource| resource.as_inner().clone())
        .collect();
    let checked_exports = check_exports_pre_apply(CheckExportsPreApplyInput {
        export_params: &parsed.export_params,
        resources: &resources_for_exports,
        compositions: &parsed.compositions,
        managed_current: &ctx.managed_current,
        data_source_values: &ctx.data_source_values,
        state_file: state_file.as_ref(),
        plan: &ctx.plan,
        schemas: wiring.schemas(),
        wait_aliases: &export_wait_aliases,
        provider_context,
    })
    .map_err(AppError::Validation)?;
    let current_exports = state_file
        .as_ref()
        .map(|state| state.exports.clone())
        .unwrap_or_default();
    let export_changes = compute_export_diffs(&checked_exports, &current_exports);
    let summary = PlanChangeSummary::new(&ctx.plan, &export_changes, &ctx.residual_deferred_for);
    // Preserve the CLI contract from before plan-image summaries existed:
    // detailed exit code 2 means a managed-resource mutation, not merely
    // displayable work such as reads, waits, exports, or deferred loops.
    let has_changes = ctx.plan.mutation_count() > 0;

    if json {
        if let Some(note) = drift_note.as_ref() {
            eprintln!("{}", note.yellow());
        }
        // `build_plan_file` propagates serialization and provider-evidence
        // failures so the user sees an actionable plan-save diagnostic
        // instead of a panic backtrace.
        let plan_file = build_plan_file(
            path,
            parsed,
            provider_inputs.configs(),
            plan_file_backend_config.clone(),
            &state_file,
            &ctx,
            &wiring,
        )
        .map_err(|e| format_plan_save_error(&e, "--json"))?;
        let json_str = serde_json::to_string_pretty(&plan_file)
            .map_err(|e| format!("Failed to serialize plan: {}", e))?;
        println!("{}", json_str);
        if let Some(result) = iam_preflight_result.as_ref() {
            crate::commands::iam_preflight::emit_warnings(result);
        }
    } else if tui {
        carina_tui::run(
            &ctx.plan,
            wiring.schemas(),
            &historical_display,
            Some(&ctx.prev_explicit),
        )
        .map_err(|e| AppError::Config(format!("TUI error: {}", e)))?;
        if let Some(result) = iam_preflight_result.as_ref() {
            crate::commands::iam_preflight::emit_warnings(result);
        }
    } else {
        print_plan(
            &ctx.plan,
            &summary,
            detail,
            &historical_display,
            Some(wiring.schemas()),
            &ctx.moved_origins,
            Some(&ctx.prev_explicit),
            Some(&ctx.expansion_trace),
        );
        if let Some(note) = drift_note.as_ref() {
            println!();
            println!("{}", note.yellow());
        }
        if let Some(result) = iam_preflight_result.as_ref() {
            crate::commands::iam_preflight::print_warnings(result);
        }
    }

    if iam_strict_failed {
        eprintln!(
            "{}",
            "Error: IAM preflight check failed in strict mode"
                .red()
                .bold()
        );
        return Err(AppError::Validation(
            "IAM preflight check failed in strict mode".to_string(),
        ));
    }

    let plan_output = if out.is_some() {
        let plan_file = build_plan_file(
            path,
            parsed,
            provider_inputs.configs(),
            plan_file_backend_config.clone(),
            &state_file,
            &ctx,
            &wiring,
        )
        .map_err(|e| format_plan_save_error(&e, "--out"))?;
        let json_out = carina_core::utils::pretty_with_newline(&plan_file)
            .map_err(|e| format!("Failed to serialize plan: {}", e))?;
        Some(json_out)
    } else {
        None
    };

    let state = state_file.as_ref().map(|state| StateMetadata {
        lineage: &state.lineage,
        serial: state.serial,
    });
    let rendered_inline_image = write_plan_artifacts(
        out.zip(plan_output.as_deref()),
        (image.is_some() || inline_image.is_some()).then_some(PlanImageArtifactRequest {
            file: image,
            inline: inline_image,
            input: PlanImageInput {
                plan: &ctx.plan,
                desired_resources: &ctx.sorted_resources,
                unresolved_resources: &ctx.unresolved_resources,
                compositions: &ctx.compositions,
                expansion_trace: Some(&ctx.expansion_trace),
                state,
                summary: &summary,
            },
        }),
    )?;

    if let Some(out_path) = out {
        progress_sink.blank_line();
        progress_sink.line(
            format!("Plan saved to {}", out_path.display())
                .green()
                .bold(),
        );
        progress_sink.line(
            format!(
                "To apply this plan, run: `{}`",
                ProjectCommand::new("apply", out_path)
            )
            .cyan(),
        );
    }

    if let Some(image_target) = image {
        eprintln!("Plan image written to {}", image_target.path().display());
    }

    if let Some(rendered_inline_image) = rendered_inline_image {
        // End command-wide cursor hiding before the image so the guard's
        // restore sequence cannot become trailing stdout after the protocol
        // bytes. The later guard/exit restores are claim-once no-ops.
        crate::cursor::restore_cursor();
        write_inline_image(rendered_inline_image, std::io::stdout().lock())?;
    }

    Ok(has_changes)
}

struct PlanImageArtifactRequest<'a> {
    file: Option<&'a ResolvedPlanImageTarget>,
    inline: Option<InlineImageRequest>,
    input: PlanImageInput<'a>,
}

/// Render all requested images before touching either destination, then write
/// the saved plan followed by the file image. The returned terminal image is
/// already protocol-encoded, but is emitted by `run_plan` only after status
/// messages so its escape bytes remain the final command output.
fn write_plan_artifacts(
    out: Option<(&Path, &str)>,
    images: Option<PlanImageArtifactRequest<'_>>,
) -> Result<Option<RenderedInlineImage>, AppError> {
    let (rendered_image, rendered_inline_image) = match images {
        Some(images) => render_plan_outputs(images.file, images.inline, images.input)?,
        None => (None, None),
    };

    if let Some((out_path, contents)) = out {
        fs::write(out_path, contents)
            .map_err(|error| AppError::Config(format!("Failed to write plan file: {error}")))?;
    }

    if let Some(rendered_image) = rendered_image {
        write_plan_image(rendered_image)?;
    }

    Ok(rendered_inline_image)
}

/// The state-aware identity and provider-input preparation shared by normal
/// planning and saved-plan secret verification. It is intentionally the only
/// owner of this ordering: prefix recovery, state-block target resolution,
/// module/anonymous identity reconciliation, then provider/upstream folding.
pub(crate) struct PreparedPlanIdentityInputs {
    pub(crate) wiring: WiringContext,
    pub(crate) state_blocks: crate::wiring::StateBlockResolution,
    pub(crate) provider_inputs: ResolvedProviderInputs,
}

/// Identity and provider inputs prepared with a caller-owned wiring context.
pub(crate) struct PreparedPlanIdentityData {
    pub(crate) state_blocks: crate::wiring::StateBlockResolution,
    pub(crate) provider_inputs: ResolvedProviderInputs,
}

pub(crate) async fn prepare_plan_identity_and_provider_inputs(
    canonical_resources: &mut CanonicalResourcePair<carina_core::parser::InferredExportParam>,
    state_file: &mut Option<StateFile>,
    base_dir: &Path,
    provider_context: &ProviderContext,
    upstream_policy: UpstreamMissingStatePolicy,
) -> Result<PreparedPlanIdentityInputs, AppError> {
    // Factory/schema wiring uses declaration metadata only. Provider values
    // are folded after identities are reconciled against state.
    let parsed = canonical_resources.parsed();
    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir)?;
    let wiring = WiringContext::new(factories, &parsed.providers);
    let PreparedPlanIdentityData {
        state_blocks,
        provider_inputs,
    } = prepare_plan_identity_and_provider_inputs_with_wiring(
        canonical_resources,
        state_file,
        base_dir,
        provider_context,
        upstream_policy,
        &wiring,
    )
    .await?;
    Ok(PreparedPlanIdentityInputs {
        wiring,
        state_blocks,
        provider_inputs,
    })
}

/// Prepare identity and provider inputs while leaving the caller-owned wiring
/// context in place. Saved-plan apply passes only its verified context here.
pub(crate) async fn prepare_plan_identity_and_provider_inputs_with_wiring(
    canonical_resources: &mut CanonicalResourcePair<carina_core::parser::InferredExportParam>,
    state_file: &mut Option<StateFile>,
    base_dir: &Path,
    provider_context: &ProviderContext,
    upstream_policy: UpstreamMissingStatePolicy,
    wiring: &WiringContext,
) -> Result<PreparedPlanIdentityData, AppError> {
    reconcile_canonical_prefixed_names(canonical_resources, state_file);
    let parsed = canonical_resources.parsed();
    let state_blocks = crate::wiring::resolve_state_blocks(
        &parsed.state_blocks,
        state_file,
        &parsed.resources,
        wiring.schemas(),
    );
    if let Some(state) = state_file.as_ref() {
        canonical_resources.reconcile_anonymous_module_instances(
            &|provider, resource_type| {
                state
                    .resources_by_type(provider, resource_type)
                    .into_iter()
                    .map(|resource| resource.identity.to_string())
                    .collect()
            },
            &state_blocks.claims,
        );
    }
    if let Some(state) = state_file.as_mut() {
        reconcile_canonical_anonymous_identifiers_with_ctx(
            wiring,
            canonical_resources,
            state,
            &state_blocks.claims,
        )?;
    }

    let provider_inputs = load_and_resolve_provider_configs(
        canonical_resources.parsed(),
        base_dir,
        provider_context,
        upstream_policy,
        ProviderUpstreamScope::All,
    )
    .await?;

    Ok(PreparedPlanIdentityData {
        state_blocks,
        provider_inputs,
    })
}

/// Complete input needed to check exports and resolve their display-only
/// values against the pre-apply binding view.
///
/// Every field is mandatory so plan and apply cannot accidentally omit a
/// binding layer or the provider-aware builtin context.
pub(crate) struct CheckExportsPreApplyInput<'a> {
    pub export_params: &'a [carina_core::parser::InferredExportParam],
    pub resources: &'a [Resource],
    pub compositions: &'a [carina_core::resource::Composition],
    pub managed_current: &'a HashMap<ResourceId, Current>,
    pub data_source_values: &'a carina_core::data_source_value::DataSourceValues,
    /// Persisted secret shapes that classify provider-returned plaintext before
    /// it enters export resolution.
    pub state_file: Option<&'a StateFile>,
    /// The plan is the only authority for resolving destination addresses back
    /// to the persisted rows that own their historical secret shapes.
    pub plan: &'a Plan,
    pub schemas: &'a carina_core::schema::SchemaRegistry,
    pub wait_aliases: &'a [carina_core::binding_index::WaitAliasSpec],
    pub provider_context: &'a ProviderContext,
}

/// Check export expressions against the pre-apply binding view while retaining
/// their original unresolved form for a later post-apply resolution.
pub(crate) fn check_exports_pre_apply(
    input: CheckExportsPreApplyInput<'_>,
) -> Result<
    carina_core::validation::ExportsCheckedPreApply<carina_core::parser::InferredExportParam>,
    String,
> {
    // carina#3248: build the unified pre-apply bindings view so an
    // export referencing `<module_instance>.<attr>` chains through the
    // composition to the managed sibling literal (carina#3246).
    let mut secret_aware_current = input.managed_current.clone();
    if let Some(state_file) = input.state_file {
        let moved_origins = MovedOrigins::from_plan(input.plan);
        for (id, current) in &mut secret_aware_current {
            let Some(persisted) = moved_origins.find_persisted_row(state_file, id) else {
                continue;
            };
            let persisted_attributes = persisted.persisted_attributes();
            if let (Some(persisted_attributes), Some(attributes)) =
                (persisted_attributes, current.attributes_mut())
            {
                for (key, value) in attributes {
                    *value = carina_core::value::mask_plaintext_using_persisted_attribute(
                        value,
                        persisted_attributes,
                        key,
                    );
                }
            }
        }
    }
    let plan_input_current = carina_core::resource::into_plan_input_current_map(
        secret_aware_current,
        input.schemas,
        input.resources,
    );
    let mut bindings = carina_core::binding_index::ResolvedBindings::pre_apply(
        carina_core::binding_index::PreApplyInputs {
            managed: input.resources,
            compositions: input.compositions,
            managed_current: &plan_input_current,
            data_source_values: input.data_source_values,
            remote_bindings: &HashMap::new(),
            wait_aliases: input.wait_aliases,
        },
    );
    bindings.apply_plan_stability(
        input.resources,
        &plan_input_current,
        input.plan,
        input.schemas,
    );

    carina_core::validation::check_export_values_pre_apply(
        input.export_params,
        &bindings,
        input.provider_context,
    )
}

/// Represents a change to an export value between current state and desired.
#[derive(Clone)]
pub enum ExportChange {
    Added {
        name: String,
        type_expr: Option<carina_core::parser::TypeExpr>,
        new_value: Value,
    },
    Modified {
        name: String,
        type_expr: Option<carina_core::parser::TypeExpr>,
        old_json: PersistedExportValue,
        new_value: Value,
    },
    Removed {
        name: String,
        old_json: PersistedExportValue,
    },
}

impl ExportChange {
    pub fn name(&self) -> &str {
        match self {
            ExportChange::Added { name, .. }
            | ExportChange::Modified { name, .. }
            | ExportChange::Removed { name, .. } => name,
        }
    }
}

/// Compute the set of export changes by comparing desired (resolved) exports
/// against current state-recorded exports.
///
/// `checked_exports` exposes display-only values resolved against current
/// resource states. `current_exports` is the typed persisted-value map from
/// `StateFile.exports`.
pub fn compute_export_diffs(
    checked_exports: &carina_core::validation::ExportsCheckedPreApply<
        carina_core::parser::InferredExportParam,
    >,
    current_exports: &HashMap<String, PersistedExportValue>,
) -> Vec<ExportChange> {
    let mut changes = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for param in checked_exports.display_values() {
        seen.insert(param.name.clone());
        let Some(ref value) = param.value else {
            continue;
        };
        // Unknown sentinels carry no usable type information for the
        // plan display; surface as `None` so the diff prints without
        // a type tag.
        let type_expr = param.type_expr.clone().into_known();
        // Plan display tolerates `Value::Deferred(DeferredValue::Unknown)` in the export rhs
        // (the deferred-for body will resolve it later). Map both the
        // skip variants and the Unknown error to `None`; the diff
        // surfaces as a "value not yet known" change without a JSON.
        let new_json = PersistedExportValue::encode(value).ok();
        match (current_exports.get(&param.name), new_json) {
            (None, _) => changes.push(ExportChange::Added {
                name: param.name.clone(),
                type_expr: type_expr.clone(),
                new_value: value.clone(),
            }),
            (Some(old), Some(new)) if old == &new => {
                // unchanged — skip
            }
            (Some(old), _) => changes.push(ExportChange::Modified {
                name: param.name.clone(),
                type_expr,
                old_json: old.clone(),
                new_value: value.clone(),
            }),
        }
    }

    // Removed: exports in state but not in desired params
    for (name, old) in current_exports {
        if !seen.contains(name) {
            changes.push(ExportChange::Removed {
                name: name.clone(),
                old_json: old.clone(),
            });
        }
    }

    changes.sort_by(|a, b| a.name().cmp(b.name()));
    changes
}

/// Seed a cycle guard with the caller's own base directory so that a chain
/// ending back at the root is detected as a cycle.
pub(crate) fn seed_cycle_guard(base_dir: &Path) -> HashSet<PathBuf> {
    let mut guard = HashSet::new();
    if let Ok(abs) = base_dir.canonicalize() {
        guard.insert(abs);
    }
    guard
}

/// How a missing upstream state file is handled while loading upstreams.
/// `apply` and recursive walks for cycle detection use [`Strict`] (error);
/// `plan` uses [`Lenient`] so a missing state demotes to a warning and
/// the upstream binding is recorded with no values (#2366).
///
/// [`Strict`]: UpstreamMissingStatePolicy::Strict
/// [`Lenient`]: UpstreamMissingStatePolicy::Lenient
#[derive(Clone, Copy)]
pub(crate) enum UpstreamMissingStatePolicy {
    Strict,
    Lenient,
}

/// Which upstream declarations a command needs before provider construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderUpstreamScope {
    /// Load every upstream because resource resolution also consumes them.
    All,
    /// Load only bindings referenced by construction-time provider attributes.
    ProviderReferences,
}

/// Upstream values and the provider configs resolved from the same snapshot.
///
/// Commands that need both resource-reference resolution and provider
/// construction carry this value together so they cannot accidentally load
/// upstream state for one step and construct providers from raw parser data in
/// another.
pub(crate) struct ResolvedProviderInputs {
    configs: ResolvedProviderConfigs,
    remote_bindings: HashMap<String, HashMap<String, Value>>,
}

impl ResolvedProviderInputs {
    pub(crate) fn configs(&self) -> &ResolvedProviderConfigs {
        &self.configs
    }

    pub(crate) fn remote_bindings(&self) -> &HashMap<String, HashMap<String, Value>> {
        &self.remote_bindings
    }
}

/// Load upstream exports and resolve every provider attribute against them.
///
/// This is the sole CLI seam for project-based provider resolution. The
/// command chooses both missing-state policy and load scope; provider
/// construction then requires the returned typed configs.
pub(crate) async fn load_and_resolve_provider_configs<E>(
    parsed: &carina_core::parser::File<E>,
    base_dir: &Path,
    provider_context: &ProviderContext,
    policy: UpstreamMissingStatePolicy,
    scope: ProviderUpstreamScope,
) -> Result<ResolvedProviderInputs, AppError> {
    let mut cycle_guard = seed_cycle_guard(base_dir);
    let remote_bindings = match scope {
        ProviderUpstreamScope::All => {
            load_upstream_states(
                &parsed.upstream_states,
                base_dir,
                provider_context,
                &mut cycle_guard,
                policy,
            )
            .await?
        }
        ProviderUpstreamScope::ProviderReferences => {
            load_provider_referenced_upstream_states(
                parsed,
                base_dir,
                provider_context,
                &mut cycle_guard,
                policy,
            )
            .await?
        }
    };
    let configs = carina_core::parser::resolve_provider_attributes_with_remote(
        parsed,
        &remote_bindings,
        provider_context,
    )
    .map_err(|error| match error {
        carina_core::parser::ProviderConfigResolutionError::Parse(source) => {
            AppError::ProviderAttributeResolution(Box::new(source))
        }
        carina_core::parser::ProviderConfigResolutionError::Invariant(source) => {
            AppError::ProviderConfigInvariant(Box::new(source))
        }
    })?;
    Ok(ResolvedProviderInputs {
        configs,
        remote_bindings,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProviderAttributeRequirement {
    provider: String,
    attribute: String,
}

#[derive(Clone, Debug)]
struct ProviderReferencedUpstream {
    upstream: UpstreamState,
    required_by: ProviderAttributeRequirement,
}

async fn load_provider_referenced_upstream_states<E>(
    parsed: &carina_core::parser::File<E>,
    base_dir: &Path,
    provider_context: &ProviderContext,
    cycle_guard: &mut HashSet<PathBuf>,
    policy: UpstreamMissingStatePolicy,
) -> Result<HashMap<String, HashMap<String, Value>>, AppError> {
    let mut remote_bindings = HashMap::new();
    for referenced in provider_referenced_upstream_states(parsed) {
        let ProviderReferencedUpstream {
            upstream,
            required_by,
        } = referenced;
        let loaded = load_upstream_states(
            std::slice::from_ref(&upstream),
            base_dir,
            provider_context,
            cycle_guard,
            policy,
        )
        .await
        .map_err(|error| AppError::ProviderReferencedUpstream {
            provider: required_by.provider,
            attribute: required_by.attribute,
            error: Box::new(error),
        })?;
        remote_bindings.extend(loaded);
    }
    Ok(remote_bindings)
}

/// Find the upstream bindings needed to resolve construction-time provider
/// attributes, including dependencies reached through local resource attrs.
///
/// The provider resolver recursively substitutes `provider -> resource -> …`
/// references. Mirror that traversal here so provider-only commands fetch the
/// same upstream values without loading unrelated state. `(binding, attr)` is
/// the visited key: this keeps the walk field-sensitive and terminates local
/// reference cycles.
fn provider_referenced_upstream_states<E>(
    parsed: &carina_core::parser::File<E>,
) -> Vec<ProviderReferencedUpstream> {
    fn enqueue_value(
        value: &Value,
        required_by: &ProviderAttributeRequirement,
        pending: &mut VecDeque<(String, Option<String>, ProviderAttributeRequirement)>,
    ) {
        value.visit_resource_refs(&mut |path| {
            pending.push_back((
                path.binding().to_string(),
                Some(path.attribute().to_string()),
                required_by.clone(),
            ));
        });
        value.visit_binding_refs(&mut |binding| {
            pending.push_back((binding.to_string(), None, required_by.clone()));
        });
    }

    let upstream_bindings: HashSet<&str> = parsed
        .upstream_states
        .iter()
        .map(|upstream| upstream.binding.as_str())
        .collect();
    let mut pending = VecDeque::new();
    for provider in &parsed.providers {
        for (attribute, value) in &provider.attributes {
            enqueue_value(
                value,
                &ProviderAttributeRequirement {
                    provider: provider.name.clone(),
                    attribute: attribute.clone(),
                },
                &mut pending,
            );
        }
    }

    let mut visited = HashSet::new();
    let mut referenced_upstreams = HashMap::new();
    while let Some((binding, attribute, required_by)) = pending.pop_front() {
        if !visited.insert((binding.clone(), attribute.clone())) {
            continue;
        }
        if upstream_bindings.contains(binding.as_str()) {
            referenced_upstreams.entry(binding).or_insert(required_by);
            continue;
        }

        let Some(resource) = parsed
            .iter_top_level_resources()
            .find(|resource| resource.binding() == Some(binding.as_str()))
        else {
            continue;
        };
        let attributes = resource.attributes();
        if let Some(attribute) = attribute {
            if let Some(value) = attributes.get(&attribute) {
                enqueue_value(value, &required_by, &mut pending);
            }
        } else {
            for value in attributes.values() {
                enqueue_value(value, &required_by, &mut pending);
            }
        }
    }

    parsed
        .upstream_states
        .iter()
        .filter_map(|upstream| {
            referenced_upstreams
                .get(&upstream.binding)
                .cloned()
                .map(|required_by| ProviderReferencedUpstream {
                    upstream: upstream.clone(),
                    required_by,
                })
        })
        .collect()
}

/// Resolve and read each upstream's published exports by parsing its source
/// directory, deriving its backend, and pulling the state through that backend.
///
/// `cycle_guard` holds canonicalized absolute paths of directories currently
/// being resolved. An upstream whose source canonicalizes to a path already in
/// the guard is a cycle (A → B → A) and produces an error naming the path.
///
/// `policy` controls only the "state file is missing" case and propagates
/// through the recursive cycle-walk so a chained upstream (A → B → C) that
/// is partially unapplied still produces warnings + display markers under
/// `Lenient` instead of a hard error. Cycle detection, source-path
/// resolution failures, upstream `.crn` parse errors, and backend I/O
/// errors remain hard errors regardless — they indicate structural
/// problems the user must fix before plan output can mean anything.
pub(crate) async fn load_upstream_states(
    upstream_states: &[UpstreamState],
    base_dir: &Path,
    provider_context: &ProviderContext,
    cycle_guard: &mut HashSet<PathBuf>,
    policy: UpstreamMissingStatePolicy,
) -> Result<HashMap<String, HashMap<String, Value>>, AppError> {
    let mut result = HashMap::new();

    for us in upstream_states {
        let source_abs = base_dir.join(&us.source).canonicalize().map_err(|e| {
            AppError::Config(format!(
                "upstream_state '{}': cannot resolve source '{}': {}",
                us.binding,
                us.source.display(),
                e
            ))
        })?;

        if !cycle_guard.insert(source_abs.clone()) {
            return Err(AppError::Config(format!(
                "upstream_state '{}': cycle detected at {}",
                us.binding,
                source_abs.display()
            )));
        }

        let backend_result =
            build_upstream_backend(us, &source_abs, provider_context, cycle_guard).await;
        cycle_guard.remove(&source_abs);
        let backend = backend_result?;

        let published_exports = backend
            .read_published_exports()
            .await
            .map_err(AppError::Backend)?;
        match (published_exports, policy) {
            (Some(exports), _) => {
                result.insert(us.binding.clone(), exports.build_remote_bindings());
            }
            (None, UpstreamMissingStatePolicy::Strict) => {
                return Err(AppError::Config(format!(
                    "upstream_state '{}': no state found at {}",
                    us.binding,
                    source_abs.display()
                )));
            }
            (None, UpstreamMissingStatePolicy::Lenient) => {
                let msg = format!(
                    "Warning: upstream_state '{}': no state found at {}; \
                     dependent values will display as `(known after upstream apply: ...)`",
                    us.binding,
                    source_abs.display()
                );
                eprintln!("{}", msg.yellow());
                // Omit the binding entirely. In the typed resolver, a loaded
                // empty export map is complete (`Absent`) while no snapshot is
                // available yet is `Unknown`; representing both as an empty
                // map would make `??` select its default too early.
            }
        }
    }

    Ok(result)
}

/// Resolve an upstream's backend: parse its `.crn`, walk its own upstream
/// chain (cycle detection only — never state I/O — see
/// carina-rs/carina#2608), then build the backend honoring local-path
/// anchoring. Shared between strict and lenient upstream loaders.
async fn build_upstream_backend(
    us: &UpstreamState,
    source_abs: &Path,
    provider_context: &ProviderContext,
    cycle_guard: &mut HashSet<PathBuf>,
) -> Result<ResolvedReadBackend, AppError> {
    let loaded = load_configuration_with_config(
        source_abs,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )
    .map_err(|e| AppError::Config(format!("upstream_state '{}': {}", us.binding, e)))?;

    // Walk the upstream's own `upstream_state` chain so cycles longer
    // than one hop still surface — but do NOT fetch their state from
    // S3 (or any other backend). The downstream only needs this
    // upstream's own exports; pulling B's transitive upstreams forces
    // A's runtime credentials to cover state buckets B happens to use
    // internally, puncturing the encapsulation that `exports.crn` is
    // supposed to provide (carina-rs/carina#2608).
    Box::pin(walk_upstream_cycles_only(
        &loaded.parsed.upstream_states,
        source_abs,
        provider_context,
        cycle_guard,
    ))
    .await?;

    // Anchor local-backend state paths and the backend lock at the upstream's
    // source directory. A pending migration in the upstream must read the old
    // locked backend just like a top-level read command.
    resolve_backend_for_read(
        source_abs,
        source_abs,
        loaded.parsed.backend.as_ref(),
        BackendLockRequirement::AllowMissing,
        ReadBackendWarningPolicy::UpstreamSource {
            source_dir: source_abs,
        },
    )
    .await
    .map_err(|error| error.with_upstream_state_source(source_abs))
}

/// Walk an upstream's transitive `upstream_state` chain *without*
/// performing any state I/O. Detects cycles (as `load_upstream_states`
/// did) but never fetches a state file from a backend — the downstream
/// only needs this upstream's own `exports`, which arrives through the
/// upstream's *own* state file fetched directly by
/// [`load_upstream_states`]. See carina-rs/carina#2608 for the
/// authorization-leak problem the I/O-free walk fixes.
///
/// Returns `Ok(())` on success; the caller does not consume any
/// bindings — there are none.
async fn walk_upstream_cycles_only(
    upstream_states: &[UpstreamState],
    base_dir: &Path,
    provider_context: &ProviderContext,
    cycle_guard: &mut HashSet<PathBuf>,
) -> Result<(), AppError> {
    for us in upstream_states {
        let source_abs = base_dir.join(&us.source).canonicalize().map_err(|e| {
            AppError::Config(format!(
                "upstream_state '{}': cannot resolve source '{}': {}",
                us.binding,
                us.source.display(),
                e
            ))
        })?;

        if !cycle_guard.insert(source_abs.clone()) {
            return Err(AppError::Config(format!(
                "upstream_state '{}': cycle detected at {}",
                us.binding,
                source_abs.display()
            )));
        }

        // Parse the upstream so we can see its own `upstream_state`
        // blocks. We deliberately do NOT touch its backend.
        let loaded = load_configuration_with_config(
            &source_abs,
            provider_context,
            &carina_core::schema::SchemaRegistry::new(),
        )
        .map_err(|e| AppError::Config(format!("upstream_state '{}': {}", us.binding, e)));

        let result = match loaded {
            Ok(loaded) => {
                Box::pin(walk_upstream_cycles_only(
                    &loaded.parsed.upstream_states,
                    &source_abs,
                    provider_context,
                    cycle_guard,
                ))
                .await
            }
            Err(e) => Err(e),
        };

        cycle_guard.remove(&source_abs);
        result?;
    }
    Ok(())
}

#[cfg(test)]
mod load_upstream_states_tests {
    use super::*;
    use std::fs;

    fn write_state(dir: &Path, exports: &[(&str, serde_json::Value)]) {
        let mut state = StateFile::new();
        for (k, v) in exports {
            state.insert_export_json_for_test(*k, v.clone());
        }
        fs::write(
            dir.join(carina_state::LocalBackend::DEFAULT_STATE_FILE),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn provider_resolution_preserves_complete_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers.push(ProviderConfig {
            name: "test".to_string(),
            attributes: indexmap::IndexMap::from([(
                "region".to_string(),
                Value::Deferred(DeferredValue::FunctionCall {
                    name: carina_core::builtins::BuiltinFunction::upper,
                    args: Vec::new(),
                    source_line: 0.into(),
                }),
            )]),
            default_tags: indexmap::IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: indexmap::IndexMap::new(),
            binding: None,
        });

        let error = match load_and_resolve_provider_configs(
            &parsed,
            dir.path(),
            &ProviderContext::default(),
            UpstreamMissingStatePolicy::Strict,
            ProviderUpstreamScope::All,
        )
        .await
        {
            Ok(_) => panic!("invalid provider function call must fail resolution"),
            Err(error) => error,
        };

        assert!(
            matches!(&error, AppError::ProviderAttributeResolution(_)),
            "ParseError must remain typed in AppError: {error:?}"
        );
        let rendered = error.to_string();
        assert_eq!(
            rendered
                .matches("upper() expects 1 argument (string), got 0")
                .count(),
            1,
            "{rendered}"
        );
        assert!(std::error::Error::source(&error).is_none());
    }

    #[tokio::test]
    async fn provider_resolution_preserves_complete_invariant_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers.push(ProviderConfig {
            name: "test".to_string(),
            attributes: indexmap::IndexMap::from([(
                "region".to_string(),
                Value::resource_ref("missing", "region", vec![]),
            )]),
            default_tags: indexmap::IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: indexmap::IndexMap::new(),
            binding: None,
        });

        let error = match load_and_resolve_provider_configs(
            &parsed,
            dir.path(),
            &ProviderContext::default(),
            UpstreamMissingStatePolicy::Strict,
            ProviderUpstreamScope::All,
        )
        .await
        {
            Ok(_) => panic!("unresolved provider config must fail the resolved invariant"),
            Err(error) => error,
        };

        assert!(matches!(&error, AppError::ProviderConfigInvariant(_)));
        let rendered = error.to_string();
        assert_eq!(rendered.matches("missing.region").count(), 1, "{rendered}");
        assert!(std::error::Error::source(&error).is_none());
    }

    #[tokio::test]
    async fn provider_reference_scope_excludes_default_tags() {
        let temp = tempfile::tempdir().unwrap();
        let referenced = temp.path().join("referenced");
        let unrelated = temp.path().join("unrelated");
        fs::create_dir_all(&referenced).unwrap();
        fs::create_dir_all(&unrelated).unwrap();
        for directory in [&referenced, &unrelated] {
            fs::write(
                directory.join("main.crn"),
                r#"backend local { path = "carina.state.json" }"#,
            )
            .unwrap();
        }
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers.push(ProviderConfig {
            name: "test".to_string(),
            attributes: indexmap::IndexMap::new(),
            default_tags: indexmap::IndexMap::from([(
                "Owner".to_string(),
                Value::resource_ref("mgmt", "owner", vec![]),
            )]),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: indexmap::IndexMap::new(),
            binding: None,
        });
        parsed.upstream_states.extend([
            UpstreamState {
                binding: "mgmt".to_string(),
                source: referenced,
            },
            UpstreamState {
                binding: "unrelated".to_string(),
                source: unrelated,
            },
        ]);

        let inputs = load_and_resolve_provider_configs(
            &parsed,
            temp.path(),
            &ProviderContext::default(),
            UpstreamMissingStatePolicy::Strict,
            ProviderUpstreamScope::ProviderReferences,
        )
        .await
        .expect("default_tags must not cause provider-only upstream loading");

        assert!(matches!(
            inputs.configs().as_provider_configs()[0]
                .default_tags
                .get("Owner"),
            Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
        ));
        assert!(inputs.remote_bindings().is_empty());
    }

    #[tokio::test]
    async fn provider_reference_scope_follows_resource_refs_to_upstream_state() {
        let (_temp, downstream, mut parsed) =
            crate::provider_upstream_test_support::provider_upstream_fixture();
        parsed.providers[0].attributes.insert(
            "role_arn".to_string(),
            Value::resource_ref("bridge", "role_arn", vec![]),
        );
        let mut bridge = Resource::with_provider("test", "thing", "bridge", None);
        bridge.binding = Some("bridge".to_string());
        bridge.set_attr(
            "role_arn".to_string(),
            Value::resource_ref("relay", "role_arn", vec![]),
        );
        let mut relay = Resource::with_provider("test", "thing", "relay", None);
        relay.binding = Some("relay".to_string());
        relay.set_attr(
            "role_arn".to_string(),
            Value::resource_ref("mgmt", "role_arn", vec![]),
        );
        parsed.resources.push(bridge); // allow: direct — fixture test inspection
        parsed.resources.push(relay); // allow: direct — fixture test inspection

        let inputs = load_and_resolve_provider_configs(
            &parsed,
            &downstream,
            &ProviderContext::default(),
            UpstreamMissingStatePolicy::Strict,
            ProviderUpstreamScope::ProviderReferences,
        )
        .await
        .expect("provider scope must load upstreams reached through resource attributes");

        assert_eq!(
            inputs.configs().as_provider_configs()[0]
                .attributes
                .get("role_arn"),
            Some(&Value::Concrete(ConcreteValue::String(
                "arn:aws:iam::123456789012:role/upstream".into()
            )))
        );
    }

    #[test]
    fn provider_reference_scope_resource_walk_is_cycle_safe() {
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers.push(ProviderConfig {
            name: "test".to_string(),
            attributes: indexmap::IndexMap::from([(
                "value".to_string(),
                Value::resource_ref("a", "value", vec![]),
            )]),
            default_tags: indexmap::IndexMap::new(),
            source: None,
            version: None,
            revision: None,
            unresolved_attributes: indexmap::IndexMap::new(),
            binding: None,
        });
        for (binding, target) in [("a", "b"), ("b", "a")] {
            let mut resource = Resource::with_provider("test", "thing", binding, None);
            resource.binding = Some(binding.to_string());
            resource.set_attr(
                "value".to_string(),
                Value::resource_ref(target, "value", vec![]),
            );
            parsed.resources.push(resource); // allow: direct — fixture test inspection
        }

        assert!(provider_referenced_upstream_states(&parsed).is_empty());
    }

    #[tokio::test]
    async fn load_upstream_states_reads_exports_from_source_backend() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            r#"backend local { path = "carina.state.json" }"#,
        )
        .unwrap();
        write_state(dir.path(), &[("account_id", serde_json::json!("123"))]);

        let upstream_states = vec![UpstreamState {
            binding: "orgs".to_string(),
            source: dir.path().to_path_buf(),
        }];

        let base_dir = dir.path().parent().unwrap();
        let result = load_upstream_states(
            &upstream_states,
            base_dir,
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .unwrap();

        assert_eq!(
            result["orgs"]["account_id"],
            Value::Concrete(ConcreteValue::String("123".into()))
        );
    }

    #[tokio::test]
    async fn load_upstream_states_uses_locked_backend_while_migration_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            r#"backend local { path = "new.state.json" }"#,
        )
        .unwrap();

        let mut state = StateFile::new();
        state.insert_export_json_for_test("account_id", serde_json::json!("from-old-backend"));
        fs::write(
            dir.path().join("old.state.json"),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();

        let old_backend = carina_core::parser::parse_file(
            r#"backend local { path = "old.state.json" }"#,
            std::path::Path::new("backend.crn"),
            &ProviderContext::default(),
        )
        .unwrap()
        .backend
        .unwrap();
        carina_state::BackendLock::for_config(Some(&old_backend))
            .unwrap()
            .save(dir.path())
            .unwrap();

        let upstream_states = vec![UpstreamState {
            binding: "orgs".to_string(),
            source: dir.path().to_path_buf(),
        }];
        let result = load_upstream_states(
            &upstream_states,
            dir.path().parent().unwrap(),
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .unwrap();

        assert_eq!(
            result["orgs"]["account_id"],
            Value::Concrete(ConcreteValue::String("from-old-backend".into()))
        );
    }

    #[tokio::test]
    async fn load_upstream_states_reads_supported_exports_from_future_state() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("backend.crn"),
            r#"backend local { path = "carina.state.json" }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("main.crn"),
            "// The upstream fixture is intentionally split across files.\n",
        )
        .unwrap();
        let state = serde_json::json!({
            "version": StateFile::CURRENT_VERSION + 1,
            "exports_version": carina_state::PublishedExports::CURRENT_VERSION,
            "serial": 1,
            "lineage": "future-upstream",
            "carina_version": "future",
            "resources": [{ "future_resource_shape": true }],
            "exports": { "account_id": "123456789012" }
        });
        fs::write(
            dir.path()
                .join(carina_state::LocalBackend::DEFAULT_STATE_FILE),
            state.to_string(),
        )
        .unwrap();

        let upstream_states = vec![UpstreamState {
            binding: "orgs".to_string(),
            source: dir.path().to_path_buf(),
        }];
        let result = load_upstream_states(
            &upstream_states,
            dir.path().parent().unwrap(),
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .expect("exports-only upstream read must ignore the future resource schema");

        assert_eq!(
            result["orgs"]["account_id"],
            Value::Concrete(ConcreteValue::String("123456789012".into()))
        );
    }

    #[tokio::test]
    async fn load_upstream_states_errors_on_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        fs::write(
            dir_a.join("main.crn"),
            r#"let b = upstream_state { source = "../b" }"#,
        )
        .unwrap();
        fs::write(
            dir_b.join("main.crn"),
            r#"let a = upstream_state { source = "../a" }"#,
        )
        .unwrap();

        let upstream_states = vec![UpstreamState {
            binding: "b".to_string(),
            source: PathBuf::from("../b"),
        }];

        let mut guard = HashSet::new();
        guard.insert(dir_a.canonicalize().unwrap());

        let err = load_upstream_states(
            &upstream_states,
            &dir_a,
            &ProviderContext::default(),
            &mut guard,
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("cycle"), "got: {}", err);
    }

    #[tokio::test]
    async fn load_upstream_states_errors_when_source_missing() {
        let upstream_states = vec![UpstreamState {
            binding: "orgs".to_string(),
            source: PathBuf::from("/nonexistent/carina/upstream/path"),
        }];
        let err = load_upstream_states(
            &upstream_states,
            Path::new("/"),
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("orgs"),
            "error should name the binding: {}",
            err
        );
    }

    /// Strict policy with a parseable upstream that has *no* `carina.state.json`
    /// must error with `"no state found at <path>"` — this is the contract
    /// `apply` relies on (#2366).
    #[tokio::test]
    async fn load_upstream_states_strict_errors_when_state_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            r#"backend local { path = "carina.state.json" }"#,
        )
        .unwrap();
        // Intentionally do NOT write carina.state.json.

        let upstream_states = vec![UpstreamState {
            binding: "orgs".to_string(),
            source: dir.path().to_path_buf(),
        }];
        let base_dir = dir.path().parent().unwrap();
        let err = load_upstream_states(
            &upstream_states,
            base_dir,
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no state found at"),
            "expected `no state found at`, got: {}",
            msg
        );
        assert!(
            msg.contains("orgs"),
            "error should name the binding: {}",
            msg
        );
    }

    /// Lenient policy with the same setup must NOT error. The unavailable
    /// snapshot is omitted so resolution distinguishes it from a successfully
    /// loaded upstream that exports no keys.
    #[tokio::test]
    async fn load_upstream_states_lenient_omits_unknown_binding_when_state_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.crn"),
            r#"backend local { path = "carina.state.json" }"#,
        )
        .unwrap();

        let upstream_states = vec![UpstreamState {
            binding: "orgs".to_string(),
            source: dir.path().to_path_buf(),
        }];
        let base_dir = dir.path().parent().unwrap();
        let result = load_upstream_states(
            &upstream_states,
            base_dir,
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Lenient,
        )
        .await
        .expect("lenient must not error");
        assert!(
            !result.contains_key("orgs"),
            "an unavailable snapshot must remain unknown, not look like a complete empty map"
        );
    }

    /// carina-rs/carina#2608: when component A reads B's
    /// `upstream_state`, carina must NOT walk into B's own
    /// transitive upstreams' state files. If it does, A's runtime
    /// credentials need read access to state objects A never
    /// references — puncturing the encapsulation that
    /// `exports.crn` is supposed to provide.
    ///
    /// The fixture: A reads B; B internally reads C. A's
    /// `load_upstream_states` should fetch B's state file (which
    /// has its `exports` already serialized), but must NEVER touch
    /// C's state file. We assert the latter by deliberately giving
    /// C a state file at a path that would parse but with sentinel
    /// `exports`, then by *not* asserting anything about C — and by
    /// covering the related path (cycle detection still walks the
    /// chain) with the existing `cycle_detected_*` tests.
    ///
    /// Direct repro of the authorization-leak shape: we delete C's
    /// state file. A pre-fix run loads C and crashes with
    /// `no state found at <C>` under Strict policy; a post-fix
    /// run does not visit C at all.
    #[tokio::test]
    async fn load_upstream_states_does_not_fetch_transitive_upstream_state() {
        let root = tempfile::tempdir().unwrap();
        let a_dir = root.path().join("a");
        let b_dir = root.path().join("b");
        let c_dir = root.path().join("c");
        fs::create_dir_all(&a_dir).unwrap();
        fs::create_dir_all(&b_dir).unwrap();
        fs::create_dir_all(&c_dir).unwrap();

        // C declares a backend; we deliberately do NOT write a
        // state file. Loading C's state would fail with
        // `BackendError` under Strict policy — the post-fix path
        // must skip this entirely.
        fs::write(
            c_dir.join("main.crn"),
            r#"backend local { path = "carina.state.json" }"#,
        )
        .unwrap();

        // B reads C as an upstream and has its own backend with
        // exports serialized into its state file.
        fs::write(
            b_dir.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
let internal = upstream_state {{ source = '{}' }}"#,
                c_dir.display()
            ),
        )
        .unwrap();
        write_state(
            &b_dir,
            &[("oidc_provider_arn", serde_json::json!("arn:..."))],
        );

        // A reads B.
        let upstream_states = vec![UpstreamState {
            binding: "bootstrap".to_string(),
            source: b_dir.clone(),
        }];

        let result = load_upstream_states(
            &upstream_states,
            &a_dir,
            &ProviderContext::default(),
            &mut HashSet::new(),
            UpstreamMissingStatePolicy::Strict,
        )
        .await;

        // The successful return is the assertion: C had no state
        // file, but the loader must not have tried to read it. If
        // the pre-fix transitive-fetch behavior were still active,
        // this would fail with "no state found at <c>".
        let bindings = result.expect(
            "loader must succeed without visiting C; carina-rs/carina#2608 \
             regression if this becomes `no state found at <c>`",
        );
        assert_eq!(
            bindings["bootstrap"]["oidc_provider_arn"],
            Value::Concrete(ConcreteValue::String("arn:...".into())),
            "B's exports must come through unchanged"
        );
    }

    /// Regression guard: cycles longer than one hop are still
    /// detected even though we no longer fetch transitive state.
    /// A → B → A must error, not stack-overflow or deadlock.
    #[tokio::test]
    async fn load_upstream_states_detects_two_hop_cycle_without_fetching_state() {
        let root = tempfile::tempdir().unwrap();
        let a_dir = root.path().join("a");
        let b_dir = root.path().join("b");
        fs::create_dir_all(&a_dir).unwrap();
        fs::create_dir_all(&b_dir).unwrap();

        // A declares an upstream pointing at B; B declares an
        // upstream pointing back at A — a cycle.
        fs::write(
            a_dir.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
let back = upstream_state {{ source = '{}' }}"#,
                b_dir.display()
            ),
        )
        .unwrap();
        fs::write(
            b_dir.join("main.crn"),
            format!(
                r#"backend local {{ path = "carina.state.json" }}
let back = upstream_state {{ source = '{}' }}"#,
                a_dir.display()
            ),
        )
        .unwrap();

        // Pretend a "downstream" reads A; the loader must catch the
        // A → B → A cycle.
        let upstream_states = vec![UpstreamState {
            binding: "a".to_string(),
            source: a_dir.clone(),
        }];

        let mut guard = HashSet::new();
        // Seed the guard so A → B → A detection still triggers
        // when the downstream itself is the seed; mirrors how the
        // CLI seeds with the project root before recursing.
        guard.insert(root.path().canonicalize().unwrap());

        let err = load_upstream_states(
            &upstream_states,
            root.path(),
            &ProviderContext::default(),
            &mut guard,
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .expect_err("two-hop cycle must error");
        let msg = err.to_string();
        assert!(
            msg.contains("cycle detected"),
            "cycle detection must still fire post-#2608 fix, got: {msg}"
        );
    }
}

#[cfg(test)]
mod run_plan_upstream_state_tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn run_plan_resolves_upstream_state_exports() {
        let tmp = tempfile::tempdir().unwrap();
        let dir_a = tmp.path().join("a");
        let dir_b = tmp.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();

        fs::write(
            dir_a.join("main.crn"),
            r#"backend local { path = "carina.state.json" }
exports { region: String = "ap-northeast-1" }"#,
        )
        .unwrap();

        let mut state_a = StateFile::new();
        state_a.insert_export_json_for_test("region", serde_json::json!("ap-northeast-1"));
        fs::write(
            dir_a.join("carina.state.json"),
            serde_json::to_string(&state_a).unwrap(),
        )
        .unwrap();

        fs::write(
            dir_b.join("main.crn"),
            r#"
                let a = upstream_state { source = "../a" }
                exports { region = a.region }
            "#,
        )
        .unwrap();

        crate::commands::ensure_backend_lock(&dir_b, None).unwrap();

        let has_changes = run_plan(
            &dir_b,
            None,
            None,
            None,
            DetailLevel::None,
            false,
            false,
            true,
            false,
            false,
            &ProviderContext::default(),
        )
        .await
        .expect("run_plan should succeed");
        assert!(
            !has_changes,
            "an exports-only plan must preserve main's zero detailed exit code"
        );

        // `run_plan` returns only `has_changes`; reload the downstream via
        // the same loader path to verify the upstream binding value is
        // reachable to downstream references.
        let parsed = carina_core::config_loader::load_configuration_with_config(
            &dir_b,
            &ProviderContext::default(),
            &carina_core::schema::SchemaRegistry::new(),
        )
        .expect("load config")
        .parsed;
        let mut guard = seed_cycle_guard(&dir_b);
        let bindings = load_upstream_states(
            &parsed.upstream_states,
            &dir_b,
            &ProviderContext::default(),
            &mut guard,
            UpstreamMissingStatePolicy::Strict,
        )
        .await
        .expect("upstream bindings");
        assert_eq!(
            bindings["a"]["region"],
            Value::Concrete(ConcreteValue::String("ap-northeast-1".into()))
        );

        let managed_current = HashMap::new();
        let data_source_values =
            carina_core::data_source_value::DataSourceValues::from_data_sources(
                &parsed.data_sources,
                |_| carina_core::data_source_value::DataSourceValue::deferred_to_apply(Vec::new()),
            );
        let export_bindings = carina_core::binding_index::ResolvedBindings::pre_apply(
            carina_core::binding_index::PreApplyInputs {
                managed: &parsed.resources,
                compositions: &parsed.compositions,
                managed_current: &managed_current,
                data_source_values: &data_source_values,
                remote_bindings: &bindings,
                wait_aliases: &[],
            },
        );
        let checked_exports = carina_core::validation::check_export_values_pre_apply(
            &parsed.export_params,
            &export_bindings,
            &ProviderContext::default(),
        )
        .expect("downstream export resolves from upstream state");
        let export_changes = compute_export_diffs(&checked_exports, &HashMap::new());
        assert_eq!(export_changes.len(), 1, "fixture must be exports-only");
        assert_eq!(export_changes[0].name(), "region");
    }

    #[tokio::test]
    async fn run_plan_allows_missing_upstream_referenced_only_by_default_tags() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let missing = tmp.path().join("missing");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&missing).unwrap();
        fs::write(
            missing.join("main.crn"),
            r#"backend local { path = "carina.state.json" }"#,
        )
        .unwrap();
        fs::write(
            project.join("main.crn"),
            r#"
                let mgmt = upstream_state { source = "../missing" }
                provider mock { default_tags = { Owner = mgmt.owner } }
                mock.test.resource { name = "tagged" }
            "#,
        )
        .unwrap();
        crate::commands::ensure_backend_lock(&project, None).unwrap();

        run_plan(
            &project,
            None,
            None,
            None,
            DetailLevel::None,
            false,
            false,
            true,
            false,
            false,
            &ProviderContext::default(),
        )
        .await
        .expect("plan must keep unresolved default tags for per-resource normalization");
    }
}

#[cfg(test)]
mod run_plan_out_tests {
    use super::*;
    use carina_core::parser::{CompareOp, ValidateExpr};
    use carina_core::provider::ProviderRouter;
    use carina_core::resource::{
        AccessPath, Composition, CompositionArgument, DataSource, DeferredValue,
        ModuleConstraintId, PendingModuleConstraint, ResolvedResource, ResourceId, Signature,
        Value,
    };
    use std::fs;

    fn resolved_image_target(path: PathBuf, theme: ImageTheme) -> ResolvedPlanImageTarget {
        let target = PlanImageTarget::try_from(path).unwrap();
        resolve_plan_image_options(Some(&target), false, Some(theme), |_| None, false)
            .unwrap()
            .file()
            .expect("file image requested")
            .clone()
    }

    fn saved_composition_argument(value: Value) -> CompositionArgument {
        serde_json::from_value(serde_json::to_value(value).unwrap()).unwrap()
    }

    fn resource_with_secret(identity: &str, plaintext: &str) -> Resource {
        Resource::with_provider(
            "mock",
            "test.resource",
            carina_core::resource::ResourceIdentity::try_from(identity.to_string()).unwrap(),
            None,
        )
        .with_attribute(
            "comment",
            Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                ConcreteValue::String(plaintext.to_string().into()),
            )))),
        )
    }

    #[test]
    fn saved_plan_effects_share_sorted_resource_secret_placeholders() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("main.crn");
        fs::write(&path, "provider mock {}\n").unwrap();

        let created = resource_with_secret("created", "CREATE_SECRET").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("created".into())),
        );
        let created_effect = resource_with_secret("created", "CREATE_SECRET").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("created-temporary".into())),
        );
        let updated = resource_with_secret("updated", "UPDATE_SECRET").with_attribute(
            "web_acl_arn",
            Value::Concrete(ConcreteValue::String("resolved-at-plan-time".into())),
        );
        let updated_effect = resource_with_secret("updated", "UPDATE_SECRET").with_attribute(
            "web_acl_arn",
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("producer", "identifier"),
            }),
        );
        let persisted_update_marker = PersistedValue::encode_provider_value(
            updated.attributes.get("comment").expect("updated secret"),
            None,
        )
        .expect("persisted update marker");
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(
                carina_state::ResourceState::try_applied(
                    "test.resource",
                    "updated",
                    "mock",
                    "updated-id",
                )
                .expect("fixture identifier is non-empty")
                .with_attribute("comment", persisted_update_marker.as_json().clone()),
            )
            .expect("persisted update row");
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(created_effect),
        ));
        plan.add(Effect::Update {
            identifier: crate::fixture_applied_identifier("updated-id"),
            from: Box::new(
                State::existing(updated.id.clone(), HashMap::new()).with_identifier("updated-id"),
            ),
            to: carina_core::resource::ResolvedResource::new(updated_effect),
            changed_attributes: vec!["comment".to_string(), "web_acl_arn".to_string()],
        });
        let parsed = carina_core::parser::ParsedFile::default();
        let ctx = crate::wiring::PlanContext {
            plan,
            provider: ProviderRouter::new(),
            sorted_resources: ResolvedResource::collect_resolved([
                created.clone(),
                updated.clone(),
            ])
            .unwrap(),
            unresolved_resources: [created, updated]
                .into_iter()
                .map(carina_core::executor::UnresolvedResource::from_pre_resolve)
                .collect(),
            compositions: Vec::new(),
            data_sources: Vec::new(),
            managed_current: HashMap::new(),
            data_source_values: carina_core::data_source_value::DataSourceValues::empty(),
            moved_origins: HashMap::new(),
            upstream_snapshot: HashMap::new(),
            prev_explicit: HashMap::new(),
            residual_deferred_for: Vec::new(),
            expansion_trace: carina_core::resource::ExpansionTrace::new(),
        };
        let provider_configs = carina_core::parser::resolve_provider_attributes_with_remote(
            &parsed,
            &HashMap::new(),
            &ProviderContext::default(),
        )
        .unwrap();

        let saved = build_plan_file(
            &path,
            &parsed,
            &provider_configs,
            None,
            &Some(state_file),
            &ctx,
            &WiringContext::new_for_test(Vec::new(), &[]),
        )
        .expect("saved plan should serialize");
        let Effect::Create(created_effect) = &saved.plan.effects()[0] else {
            panic!("expected create effect")
        };
        let Effect::Update {
            to: updated_effect, ..
        } = &saved.plan.effects()[1]
        else {
            panic!("expected update effect")
        };
        assert_eq!(
            created_effect.attributes["comment"], saved.sorted_resources[0].attributes["comment"],
            "create effect and sorted resource must share one digest",
        );
        assert_eq!(
            created_effect.attributes["name"],
            Value::Concrete(ConcreteValue::String("created-temporary".into())),
            "saved-plan redaction must retain a create-before-destroy temporary name",
        );
        assert_eq!(
            updated_effect.attributes["comment"], saved.sorted_resources[1].attributes["comment"],
            "update effect and sorted resource must share one digest",
        );
        assert!(
            matches!(
                updated_effect.attributes.get("web_acl_arn"),
                Some(Value::Deferred(DeferredValue::ResourceRef { .. }))
            ),
            "saved-plan redaction must retain unresolved cascade-consumer references",
        );
        assert_eq!(
            saved.sorted_resources[1].attributes["comment"],
            persisted_update_marker
                .decode()
                .expect("lift persisted update marker"),
            "both saved-plan copies must use the matching persisted digest",
        );
    }

    #[tokio::test]
    async fn provider_attributes_observe_reconciled_prefixed_resource_names() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(
            dir.join("main.crn"),
            r#"
                provider mock { target = bucket.name }
                let bucket = mock.test.resource { name_prefix = "pref-" }
            "#,
        )
        .unwrap();

        let mut state = StateFile::new();
        let mut bucket = carina_state::ResourceState::try_applied(
            "test.resource",
            "bucket",
            "mock",
            "pref-existing",
        )
        .expect("fixture identifier is non-empty");
        bucket.set_persisted_attribute("name".to_string(), serde_json::json!("pref-existing"));
        bucket
            .prefixes
            .insert("name".to_string(), "pref-".to_string());
        state.upsert_resource(bucket).unwrap();
        fs::write(
            dir.join("carina.state.json"),
            carina_core::utils::pretty_with_newline(&state).unwrap(),
        )
        .unwrap();
        crate::commands::ensure_backend_lock(dir, None).unwrap();

        let plan_path = dir.join("plan.json");
        run_plan(
            dir,
            Some(&plan_path),
            None,
            None,
            DetailLevel::None,
            false,
            false,
            true,
            false,
            false,
            &ProviderContext::default(),
        )
        .await
        .expect("plan should resolve provider attributes after name reconciliation");

        let saved: PlanFile = serde_json::from_slice(&fs::read(plan_path).unwrap()).unwrap();
        assert_eq!(
            saved.provider_configs.as_provider_configs()[0]
                .attributes
                .get("target"),
            Some(&Value::Concrete(ConcreteValue::String(
                "pref-existing".into()
            )))
        );
    }

    // Pin the byte-level shape so plan files written by `plan --out`
    // match the trailing-newline convention used by carina-backend.lock,
    // carina.state.json, and carina-providers.lock (#2583).
    #[tokio::test]
    async fn run_plan_out_file_ends_with_trailing_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(
            dir.join("main.crn"),
            r#"exports { region: String = "ap-northeast-1" }"#,
        )
        .unwrap();

        crate::commands::ensure_backend_lock(dir, None).unwrap();

        let plan_path = dir.join("plan.json");
        run_plan(
            dir,
            Some(&plan_path),
            None,
            None,
            DetailLevel::None,
            false,
            false,
            true,
            false,
            false,
            &ProviderContext::default(),
        )
        .await
        .expect("run_plan should succeed");

        let bytes = fs::read(&plan_path).expect("plan file written");
        assert_eq!(
            bytes.last().copied(),
            Some(b'\n'),
            "plan --out file must end with a trailing newline; got {:?}",
            bytes.last().map(|b| *b as char),
        );
    }

    #[tokio::test]
    async fn run_plan_does_not_write_image_when_out_file_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        fs::write(
            dir.join("main.crn"),
            r#"exports { region: String = "ap-northeast-1" }"#,
        )
        .unwrap();
        crate::commands::ensure_backend_lock(dir, None).unwrap();

        let image_path = dir.join("plan.svg");
        let image = resolved_image_target(image_path.clone(), ImageTheme::Light);
        let missing_parent_out = dir.join("missing/plan.json");
        let result = run_plan(
            dir,
            Some(&missing_parent_out),
            Some(&image),
            None,
            DetailLevel::None,
            false,
            false,
            true,
            false,
            false,
            &ProviderContext::default(),
        )
        .await;

        assert!(result.is_err(), "the invalid --out destination must fail");
        assert!(
            !image_path.exists(),
            "a failed plan command must not leave a fresh image"
        );
    }

    #[test]
    fn png_render_failure_writes_neither_out_nor_image() {
        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("plan.json");
        let image_path = tmp.path().join("plan.png");
        let image_target = resolved_image_target(image_path.clone(), ImageTheme::Light);

        let mut plan = Plan::new();
        for index in 0..300 {
            let identity =
                carina_core::resource::ResourceIdentity::try_from(format!("resource_{index:04}"))
                    .unwrap();
            plan.add(Effect::Create(ResolvedResource::new(Resource::new(
                "test.Resource",
                identity,
            ))));
        }
        let summary = PlanChangeSummary::new(&plan, &[], &[]);
        let desired_resources = Vec::new();
        let unresolved_resources = Vec::new();
        let compositions = Vec::new();
        let expansion_trace = carina_core::resource::ExpansionTrace::new();

        let result = write_plan_artifacts(
            Some((&out_path, "{}\n")),
            Some(PlanImageArtifactRequest {
                file: Some(&image_target),
                inline: None,
                input: PlanImageInput {
                    plan: &plan,
                    desired_resources: &desired_resources,
                    unresolved_resources: &unresolved_resources,
                    compositions: &compositions,
                    expansion_trace: Some(&expansion_trace),
                    state: None,
                    summary: &summary,
                },
            }),
        );

        assert!(
            matches!(
                result,
                Err(AppError::PlanImage(PlanImageError::PngPixelBudget { .. }))
            ),
            "oversized PNG rendering must fail before either artifact is written"
        );
        assert!(!out_path.exists(), "render failure must not write --out");
        assert!(
            !image_path.exists(),
            "render failure must not write --image"
        );
    }

    #[test]
    fn inline_png_render_failure_writes_and_emits_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let out_path = tmp.path().join("plan.json");
        let image_path = tmp.path().join("plan.svg");
        let image_target = resolved_image_target(image_path.clone(), ImageTheme::Light);

        let mut plan = Plan::new();
        for index in 0..300 {
            let identity =
                carina_core::resource::ResourceIdentity::try_from(format!("resource_{index:04}"))
                    .unwrap();
            plan.add(Effect::Create(ResolvedResource::new(Resource::new(
                "test.Resource",
                identity,
            ))));
        }
        let summary = PlanChangeSummary::new(&plan, &[], &[]);
        let desired_resources = Vec::new();
        let unresolved_resources = Vec::new();
        let compositions = Vec::new();
        let expansion_trace = carina_core::resource::ExpansionTrace::new();

        let mut inline_output = Vec::new();
        let result: Result<(), AppError> = (|| {
            let rendered_inline_image = write_plan_artifacts(
                Some((&out_path, "{}\n")),
                Some(PlanImageArtifactRequest {
                    file: Some(&image_target),
                    inline: Some(InlineImageRequest {
                        protocol: InlineImageProtocol::Kitty,
                        terminal: None,
                        theme: Theme::Light,
                    }),
                    input: PlanImageInput {
                        plan: &plan,
                        desired_resources: &desired_resources,
                        unresolved_resources: &unresolved_resources,
                        compositions: &compositions,
                        expansion_trace: Some(&expansion_trace),
                        state: None,
                        summary: &summary,
                    },
                }),
            )?;
            if let Some(rendered_inline_image) = rendered_inline_image {
                write_inline_image(rendered_inline_image, &mut inline_output)?;
            }
            Ok(())
        })();

        assert!(
            matches!(
                result,
                Err(AppError::PlanImage(PlanImageError::PngPixelBudget { .. }))
            ),
            "oversized inline PNG rendering must fail before any artifact is written"
        );
        assert!(!out_path.exists(), "render failure must not write --out");
        assert!(
            !image_path.exists(),
            "inline render failure must not write --image"
        );
        assert!(
            inline_output.is_empty(),
            "inline render failure must not emit terminal escape bytes"
        );
    }

    #[test]
    fn saved_plan_file_persists_deferred_data_source_inputs_from_plan_context() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("main.crn");
        fs::write(&path, "provider mock {}\n").unwrap();

        let mut parsed_data_source = DataSource::with_provider("mock", "iam.Roles", "roles", None);
        parsed_data_source.binding = Some("roles".to_string());
        parsed_data_source.set_attr(
            "name_regex".to_string(),
            Value::Concrete(carina_core::resource::ConcreteValue::String(
                "^target$".into(),
            )),
        );

        let mut deferred_data_source =
            DataSource::with_provider("mock", "iam.Roles", "roles", None);
        deferred_data_source.binding = Some("roles".to_string());
        deferred_data_source.set_attr(
            "name_regex".to_string(),
            Value::Deferred(DeferredValue::ResourceRef {
                path: AccessPath::new("target_role", "role_name"),
            }),
        );
        let data_source_values =
            carina_core::data_source_value::DataSourceValues::from_data_sources(
                std::slice::from_ref(&deferred_data_source),
                |_| carina_core::data_source_value::DataSourceValue::deferred_to_apply(Vec::new()),
            );

        let parsed = carina_core::parser::ParsedFile {
            data_sources: vec![parsed_data_source],
            ..Default::default()
        };
        let ctx = crate::wiring::PlanContext {
            plan: Plan::new(),
            provider: ProviderRouter::new(),
            sorted_resources: Vec::new(),
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: vec![deferred_data_source],
            managed_current: HashMap::new(),
            data_source_values,
            moved_origins: HashMap::new(),
            upstream_snapshot: HashMap::new(),
            prev_explicit: HashMap::new(),
            residual_deferred_for: Vec::new(),
            expansion_trace: carina_core::resource::ExpansionTrace::new(),
        };

        let provider_configs = carina_core::parser::resolve_provider_attributes_with_remote(
            &parsed,
            &HashMap::new(),
            &ProviderContext::default(),
        )
        .unwrap();
        let plan_file = build_plan_file(
            &path,
            &parsed,
            &provider_configs,
            None,
            &None,
            &ctx,
            &WiringContext::new_for_test(Vec::new(), &[]),
        )
        .expect("saved plan should serialize");

        let saved = plan_file.data_sources[0]
            .get_attr("name_regex")
            .expect("saved data source attr");
        assert!(
            matches!(saved, Value::Deferred(DeferredValue::ResourceRef { .. })),
            "saved plans must preserve deferred data-source refs for apply --plan, got {saved:?}"
        );
        assert_eq!(
            plan_file.data_source_origins[0].get_attr("name_regex"),
            Some(&Value::Concrete(
                carina_core::resource::ConcreteValue::String("^target$".into())
            )),
            "saved plans must separately preserve the authored pre-normalization input"
        );
    }

    #[test]
    fn saved_plan_file_persists_prepared_pending_module_constraints() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("main.crn");
        fs::write(&path, "provider mock {}\n").unwrap();

        let mut arguments = indexmap::IndexMap::new();
        arguments.insert(
            "name".to_string(),
            saved_composition_argument(Value::resource_ref("producer", "name", vec![])),
        );
        let prepared = Composition {
            id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual", "checked",
            )),
            signature: Signature {
                arguments,
                attributes: indexmap::IndexMap::new(),
                pending_constraints: vec![PendingModuleConstraint {
                    id: ModuleConstraintId::argument_validation("name", 0),
                    expression: ValidateExpr::Compare {
                        lhs: Box::new(ValidateExpr::Var("name".to_string())),
                        op: CompareOp::Ne,
                        rhs: Box::new(ValidateExpr::String("bad".to_string())),
                    },
                    message: "name must not be bad".to_string(),
                }],
            },
            binding: Some("checked".to_string()),
            dependency_bindings: Default::default(),
            module_name: "checked_module".to_string(),
            instance: "root.checked".to_string(),
            provenance: Default::default(),
            quoted_string_attrs: Default::default(),
        };
        let parsed = carina_core::parser::ParsedFile::default();
        let ctx = crate::wiring::PlanContext {
            plan: Plan::new(),
            provider: ProviderRouter::new(),
            sorted_resources: Vec::new(),
            unresolved_resources: Vec::new(),
            compositions: vec![prepared],
            data_sources: Vec::new(),
            managed_current: HashMap::new(),
            data_source_values: carina_core::data_source_value::DataSourceValues::empty(),
            moved_origins: HashMap::new(),
            upstream_snapshot: HashMap::new(),
            prev_explicit: HashMap::new(),
            residual_deferred_for: Vec::new(),
            expansion_trace: carina_core::resource::ExpansionTrace::new(),
        };

        let provider_configs = carina_core::parser::resolve_provider_attributes_with_remote(
            &parsed,
            &HashMap::new(),
            &ProviderContext::default(),
        )
        .unwrap();
        let plan_file = build_plan_file(
            &path,
            &parsed,
            &provider_configs,
            None,
            &None,
            &ctx,
            &WiringContext::new_for_test(Vec::new(), &[]),
        )
        .expect("saved plan should serialize prepared compositions");

        assert_eq!(plan_file.compositions.len(), 1);
        assert_eq!(
            plan_file.compositions[0]
                .signature
                .pending_constraints
                .len(),
            1,
            "the prepared pending constraint must survive into the plan file"
        );
    }

    #[test]
    fn saved_plan_upstream_secret_error_names_binding_and_key() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("main.crn");
        fs::write(&path, "provider mock {}\n").unwrap();
        let parsed = carina_core::parser::ParsedFile::default();
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
            carina_core::resource::ConcreteValue::String("upstream-secret".into()),
        ))));
        let ctx = crate::wiring::PlanContext {
            plan: Plan::new(),
            provider: ProviderRouter::new(),
            sorted_resources: Vec::new(),
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: Vec::new(),
            managed_current: HashMap::new(),
            data_source_values: carina_core::data_source_value::DataSourceValues::empty(),
            moved_origins: HashMap::new(),
            upstream_snapshot: HashMap::from([(
                "network".to_string(),
                HashMap::from([("password".to_string(), secret)]),
            )]),
            prev_explicit: HashMap::new(),
            residual_deferred_for: Vec::new(),
            expansion_trace: carina_core::resource::ExpansionTrace::new(),
        };
        let provider_configs = carina_core::parser::resolve_provider_attributes_with_remote(
            &parsed,
            &HashMap::new(),
            &ProviderContext::default(),
        )
        .unwrap();

        let error = build_plan_file(
            &path,
            &parsed,
            &provider_configs,
            None,
            &None,
            &ctx,
            &WiringContext::new_for_test(Vec::new(), &[]),
        )
        .expect_err("a secret-bearing upstream snapshot must be rejected")
        .to_string();
        assert!(
            error.contains("upstream_state network.password"),
            "saved-plan error lost the upstream field location: {error}",
        );
    }

    #[test]
    fn build_plan_file_reports_provider_evidence_failure_without_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("main.crn");
        let configured = ProviderConfig {
            name: "aws".to_string(),
            attributes: indexmap::IndexMap::new(),
            default_tags: indexmap::IndexMap::new(),
            source: Some("file:///missing/aws.wasm".parse().unwrap()),
            version: None,
            revision: None,
            unresolved_attributes: indexmap::IndexMap::new(),
            binding: None,
        };
        let parsed = carina_core::parser::InferredFile {
            providers: vec![configured.clone()],
            ..Default::default()
        };
        let provider_configs = carina_core::parser::resolve_provider_attributes_with_remote(
            &parsed,
            &HashMap::new(),
            &ProviderContext::default(),
        )
        .unwrap();
        let ctx = crate::wiring::PlanContext {
            plan: Plan::new(),
            provider: ProviderRouter::new(),
            sorted_resources: Vec::new(),
            unresolved_resources: Vec::new(),
            compositions: Vec::new(),
            data_sources: Vec::new(),
            managed_current: HashMap::new(),
            data_source_values: carina_core::data_source_value::DataSourceValues::empty(),
            moved_origins: HashMap::new(),
            upstream_snapshot: HashMap::new(),
            prev_explicit: HashMap::new(),
            residual_deferred_for: Vec::new(),
            expansion_trace: carina_core::resource::ExpansionTrace::new(),
        };
        let wiring = WiringContext::new_for_test(Vec::new(), &[configured]);

        let error = build_plan_file(
            &path,
            &parsed,
            &provider_configs,
            None,
            &None,
            &ctx,
            &wiring,
        )
        .expect_err("a configured provider without an artifact must fail plan saving");
        assert!(matches!(
            &error,
            PlanSaveError::ProviderEvidence(
                ProviderEvidenceError::MissingProviderArtifact { provider }
            ) if provider == "aws"
        ));
        assert_eq!(
            format_plan_save_error(&error, "--out"),
            "Cannot save plan: invalid saved-plan provider evidence: provider_artifacts is missing expected provider `aws`"
        );
    }
}

#[cfg(test)]
mod export_diff_tests {
    use super::*;
    use carina_core::parser::{InferredExportParam, TypeExpr};
    use carina_core::resource::DeferredValue;

    fn persisted(value: serde_json::Value) -> PersistedExportValue {
        serde_json::from_value(value).expect("test value is valid persisted JSON")
    }

    fn param(name: &str, value: Value) -> InferredExportParam {
        InferredExportParam {
            name: name.to_string(),
            type_expr: TypeExpr::Unknown,
            value: Some(value),
        }
    }

    fn checked(
        params: Vec<InferredExportParam>,
    ) -> carina_core::validation::ExportsCheckedPreApply<InferredExportParam> {
        carina_core::validation::check_export_values_pre_apply(
            &params,
            &carina_core::binding_index::ResolvedBindings::default(),
            &ProviderContext::default(),
        )
        .expect("test exports must pass the secret boundary")
    }

    #[test]
    fn compute_export_diffs_added_when_state_empty() {
        let params = checked(vec![param(
            "count",
            Value::Concrete(ConcreteValue::Int(42)),
        )]);
        let current = HashMap::new();
        let changes = compute_export_diffs(&params, &current);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], ExportChange::Added { .. }));
    }

    #[test]
    fn compute_export_diffs_modified_when_value_differs() {
        let params = checked(vec![param(
            "count",
            Value::Concrete(ConcreteValue::Int(42)),
        )]);
        let mut current = HashMap::new();
        current.insert("count".to_string(), persisted(serde_json::json!(7)));
        let changes = compute_export_diffs(&params, &current);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], ExportChange::Modified { .. }));
    }

    #[test]
    fn compute_export_diffs_unchanged_when_value_matches() {
        let params = checked(vec![param(
            "count",
            Value::Concrete(ConcreteValue::Int(42)),
        )]);
        let mut current = HashMap::new();
        current.insert("count".to_string(), persisted(serde_json::json!(42)));
        let changes = compute_export_diffs(&params, &current);
        assert!(changes.is_empty());
    }

    #[test]
    fn compute_export_diffs_removed_when_param_missing() {
        let params = checked(Vec::new());
        let mut current = HashMap::new();
        current.insert("stale".to_string(), persisted(serde_json::json!("old")));
        let changes = compute_export_diffs(&params, &current);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes[0], ExportChange::Removed { .. }));
    }

    #[test]
    fn compute_export_diffs_mixed_sorted_by_name() {
        let params = checked(vec![
            param("added", Value::Concrete(ConcreteValue::Int(1))),
            param("modified", Value::Concrete(ConcreteValue::Int(2))),
        ]);
        let mut current = HashMap::new();
        current.insert("modified".to_string(), persisted(serde_json::json!(99)));
        current.insert("removed".to_string(), persisted(serde_json::json!("old")));
        let changes = compute_export_diffs(&params, &current);
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].name(), "added");
        assert_eq!(changes[1].name(), "modified");
        assert_eq!(changes[2].name(), "removed");
    }

    #[test]
    fn resolve_export_value_preserves_dotted_string_literal() {
        let resource = Resource::with_provider("test", "r.Vpc", "vpc", None)
            .with_binding("vpc")
            .with_attribute(
                "vpc_id",
                Value::Concrete(ConcreteValue::String("vpc-123".into())),
            );
        let bindings = carina_core::binding_index::ResolvedBindings::pre_apply(
            carina_core::binding_index::PreApplyInputs {
                managed: &[resource],
                compositions: &[],
                managed_current: &HashMap::new(),
                data_source_values: &carina_core::data_source_value::DataSourceValues::empty(),
                remote_bindings: &HashMap::new(),
                wait_aliases: &[],
            },
        );
        let value = Value::Concrete(ConcreteValue::String("vpc.vpc_id".into()));

        let params = vec![param("literal", value.clone())];
        let resolved = carina_core::validation::check_export_values_pre_apply(
            &params,
            &bindings,
            &ProviderContext::default(),
        )
        .expect("plain dotted string is a valid export");
        assert_eq!(resolved.display_values()[0].value.as_ref(), Some(&value));
    }

    #[test]
    fn resolved_resource_reference_exposes_secret_export_for_plan_rejection() {
        let resource = Resource::with_provider("test", "r.Vpc", "vpc", None)
            .with_binding("vpc")
            .with_attribute(
                "password",
                Value::Deferred(DeferredValue::Secret(Box::new(Value::Concrete(
                    ConcreteValue::String("topsecretXYZ".into()),
                )))),
            );
        let bindings = carina_core::binding_index::ResolvedBindings::pre_apply(
            carina_core::binding_index::PreApplyInputs {
                managed: &[resource],
                compositions: &[],
                managed_current: &HashMap::new(),
                data_source_values: &carina_core::data_source_value::DataSourceValues::empty(),
                remote_bindings: &HashMap::new(),
                wait_aliases: &[],
            },
        );
        let params = vec![param(
            "password",
            Value::resource_ref("vpc", "password", vec![]),
        )];
        let error = carina_core::validation::check_export_values_pre_apply(
            &params,
            &bindings,
            &ProviderContext::default(),
        )
        .expect_err("resolved secret reference must be rejected before display");
        assert_eq!(error, "export 'password' cannot hold a secret value");
        assert!(!error.contains("topsecretXYZ"));
    }
}

#[cfg(test)]
mod plan_serialization_error_tests {
    //! RFC #2371 stage 4: serialization-boundary errors are now produced
    //! by `PersistedValue::encode_provider_value` / `redact_secrets_in_*`
    //! directly, surfaced through `build_plan_file`'s typed plan-save error.
    //! Verify the actionable wording and that every effect shape
    //! (top-level attributes, nested List/Map, Replace cascade) propagates
    //! the error.
    use carina_core::resource::{
        AccessPath, ConcreteValue, DeferredValue, Resource, UnknownReason, Value,
    };
    use carina_core::value::{PersistedValue, SerializationError, redact_secrets_in_resource};

    fn upstream_unknown() -> Value {
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamExport {
            path,
        }))
    }

    #[test]
    fn value_to_json_errors_on_top_level_unknown() {
        let err = PersistedValue::encode_provider_value(&upstream_unknown(), None).unwrap_err();
        match err {
            SerializationError::UnknownNotAllowed {
                reason: UnknownReason::UpstreamExport { path },
                ..
            } => {
                assert_eq!(path.to_dot_string(), "network.vpc.vpc_id");
            }
            other => panic!("expected UnknownNotAllowed/UpstreamExport, got: {other:?}"),
        }
    }

    #[test]
    fn value_to_json_errors_on_unknown_inside_list_or_map() {
        let list_val = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            upstream_unknown(),
        ]));
        assert!(matches!(
            PersistedValue::encode_provider_value(&list_val, None).unwrap_err(),
            SerializationError::UnknownNotAllowed { .. }
        ));

        let mut m: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        m.insert("Name".into(), upstream_unknown());
        assert!(matches!(
            PersistedValue::encode_provider_value(&Value::Concrete(ConcreteValue::Map(m)), None,)
                .unwrap_err(),
            SerializationError::UnknownNotAllowed { .. }
        ));
    }

    #[test]
    fn redact_secrets_in_resource_errors_on_unknown_attribute() {
        let mut r = Resource::new("test.resource", "name");
        r.attributes.insert("vpc_id".into(), upstream_unknown());
        let err = redact_secrets_in_resource(&r).unwrap_err();
        assert!(matches!(err, SerializationError::UnknownNotAllowed { .. }));
    }

    /// `for*` placeholders carry no path, but their reason variant
    /// must survive into the error so the caller can render
    /// "for-binding placeholder" rather than a generic message.
    #[test]
    fn value_to_json_preserves_for_variants_in_error() {
        for (variant, label) in [
            (UnknownReason::ForValue, "ForValue"),
            (UnknownReason::ForKey, "ForKey"),
            (UnknownReason::ForIndex, "ForIndex"),
        ] {
            let err = PersistedValue::encode_provider_value(
                &Value::Deferred(DeferredValue::Unknown(variant.clone())),
                None,
            )
            .unwrap_err();
            match err {
                SerializationError::UnknownNotAllowed { reason, .. } => {
                    assert_eq!(reason, variant, "expected {label} round-trip");
                }
                other => panic!("expected UnknownNotAllowed for {label}, got: {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod state_drift_tests {
    use super::*;

    fn state(serial: u64, lineage: &str) -> StateFile {
        let mut s = StateFile::new();
        s.serial = serial;
        s.lineage = lineage.to_string();
        s
    }

    #[test]
    fn no_state_at_t0_means_no_drift() {
        let before = StateSnapshot::capture(None);
        let after = state(5, "abc");
        assert_eq!(detect_state_drift(before.as_ref(), Some(&after)), None);
    }

    #[test]
    fn unchanged_serial_and_lineage_is_no_drift() {
        let s = state(7, "abc");
        let before = StateSnapshot::capture(Some(&s));
        assert_eq!(detect_state_drift(before.as_ref(), Some(&s)), None);
    }

    #[test]
    fn advanced_serial_same_lineage_is_serial_drift() {
        let t0 = state(7, "abc");
        let before = StateSnapshot::capture(Some(&t0));
        let t1 = state(8, "abc");
        assert_eq!(
            detect_state_drift(before.as_ref(), Some(&t1)),
            Some(StateDrift::SerialAdvanced { from: 7, to: 8 })
        );
    }

    #[test]
    fn changed_lineage_is_lineage_drift_even_if_serial_lower() {
        let t0 = state(9, "old-lineage");
        let before = StateSnapshot::capture(Some(&t0));
        // Fresh state after destroy+apply: new lineage, serial reset.
        let t1 = state(0, "new-lineage");
        assert_eq!(
            detect_state_drift(before.as_ref(), Some(&t1)),
            Some(StateDrift::LineageChanged {
                from: "old-lineage".to_string(),
                to: "new-lineage".to_string(),
            })
        );
    }

    #[test]
    fn state_removed_between_t0_and_t1_is_removed_drift() {
        let t0 = state(3, "abc");
        let before = StateSnapshot::capture(Some(&t0));
        assert_eq!(
            detect_state_drift(before.as_ref(), None),
            Some(StateDrift::StateRemoved)
        );
    }

    #[test]
    fn warning_message_names_the_drift_kind() {
        assert!(
            StateDrift::SerialAdvanced { from: 1, to: 2 }
                .warning(Path::new("."))
                .contains("serial advanced 1 -> 2")
        );
        assert!(
            StateDrift::LineageChanged {
                from: "a".into(),
                to: "b".into()
            }
            .warning(Path::new("."))
            .contains("lineage changed a -> b")
        );
        assert!(
            StateDrift::StateRemoved
                .warning(Path::new("."))
                .contains("state was removed")
        );
        // Every variant must steer the user to re-run plan.
        assert!(
            StateDrift::StateRemoved
                .warning(Path::new("."))
                .contains("re-run `carina plan`")
        );
    }
}
