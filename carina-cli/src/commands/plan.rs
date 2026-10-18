use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use colored::Colorize;
use serde::{Deserialize, Serialize};

use carina_core::config_loader::{get_base_dir, load_configuration_with_config};
use carina_core::detail_rows::HistoricalDisplayAttributes;
use carina_core::effect::{Effect, EffectGeneration};
#[cfg(test)]
use carina_core::parser::ProviderConfig;
use carina_core::parser::{
    BackendConfig, ProviderContext, ResolvedProviderConfigs, SavedProviderConfigs, UpstreamState,
};
use carina_core::plan::Plan;
#[cfg(test)]
use carina_core::resource::DeferredValue;
use carina_core::resource::{ConcreteValue, Resource, ResourceId, State, Value};
use carina_core::value::{
    redact_secrets_in_plan_with_desired_and_historical, redact_secrets_in_resource,
    redact_secrets_in_resource_reusing_persisted_hashes,
    redact_secrets_in_resource_using_evaluated, redact_secrets_in_state,
    redact_secrets_in_state_using_persisted, redact_secrets_in_state_using_resource,
};
use carina_state::{BackendConfig as StateBackendConfig, StateBackend, StateFile, create_backend};

use super::{BackendDriftStatus, drift_warning, inspect_backend_drift};
use crate::DetailLevel;
use crate::commands::shared::plan_errors::render_plan_errors_and_abort;
use crate::display::{print_plan, refresh_plan_separator};
use crate::error::AppError;
use crate::wiring::{
    WiringContext, build_factories_from_providers, create_plan_from_parsed_with_upstream_with_ctx,
    reconcile_anonymous_identifiers_with_ctx, reconcile_prefixed_names,
};
use carina_core::hint::ProjectCommand;

/// Saved plan file for `plan --out` / `apply <plan-file>`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanFile {
    /// Plan file format version.
    ///
    /// `PlanFileHeader` in `commands/apply/mod.rs` relies on this field and
    /// `timestamp` to recognize saved plans before decoding format-specific
    /// fields. Their serialized names must remain stable across plan formats;
    /// the version must also remain a `u32` (guarded by a test).
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
    #[serde(default)]
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
    #[serde(default)]
    pub data_sources: Vec<carina_core::resource::DataSource>,
    /// Authored, pre-normalization counterparts of `data_sources`, paired by
    /// index and rewritten to the resolved IDs above. Apply uses these to
    /// derive which input attributes became known at runtime without
    /// rechecking canonicalized literals as authored values.
    #[serde(default)]
    pub data_source_origins: Vec<carina_core::resource::DataSource>,
    /// Current states (for binding_map + state saving)
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
    #[serde(default)]
    pub wait_bindings: Vec<PlanWaitBinding>,
}

impl PlanFile {
    pub const CURRENT_VERSION: u32 = 16;

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
            let CurrentStateEntry { id: _id, state } = entry;
            state_contains(state)
        }

        let PlanFile {
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
            current_states,
            upstream_snapshot,
            upstream_sources: _upstream_sources,
            wait_bindings: _wait_bindings,
        } = self;

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
        for (index, entry) in self.current_states.iter().enumerate() {
            require_identity(&entry.id, &format!("current_states[{index}].id"))?;
            require_identity(
                &entry.state.id,
                &format!("current_states[{index}].state.id"),
            )?;
        }

        for (index, effect) in self.plan.effects().iter().enumerate() {
            let location = format!("plan.effects[{index}]");
            match effect {
                Effect::Read { resource } => require_identity(&resource.id, &location)?,
                Effect::Create(resource) => require_identity(&resource.id, &location)?,
                Effect::Update { from, to, .. } => {
                    require_identity(&from.id, &format!("{location}.from"))?;
                    require_identity(&to.id, &format!("{location}.to"))?;
                }
                Effect::Delete { id, .. }
                | Effect::Import { id, .. }
                | Effect::Remove { id }
                | Effect::Wait { target_id: id, .. } => require_identity(id, &location)?,
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

pub(crate) fn collect_historical_display_attributes(
    plan: &Plan,
    current_states: &HashMap<ResourceId, State>,
    state_file: Option<&StateFile>,
) -> HistoricalDisplayAttributes {
    let mut display = HistoricalDisplayAttributes::for_plan(plan);
    let moved_origins = MovedOrigins::from_plan(plan);

    for effect in plan.effects() {
        for key in effect.deleted_resource_attribute_keys() {
            match key.generation() {
                carina_core::effect::EffectGeneration::Current => {
                    if let Some(state) = current_states.get(key.id()) {
                        let persisted_id = moved_origins.origin_for(key.id());
                        let persisted = state_file
                            .and_then(|file| file.find_resource_for_id(persisted_id))
                            .map(|resource| resource.persisted_attributes());
                        display.set_delete_from_provider_and_persisted(
                            key,
                            &state.attributes,
                            persisted,
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
                .map(|resource| resource.persisted_attributes());
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
        );
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
            from: Box::new(from.clone()),
            to,
            changed_attributes: vec!["comment".to_string()],
        });
        let current_states = HashMap::from([(id, from)]);

        // Exercise the same collector and row builder used by CLI and TUI
        // production paths. There is deliberately no persisted state shape:
        // the desired Secret shape must still mask the provider plaintext.
        let historical = collect_historical_display_attributes(&plan, &current_states, None);
        let app = carina_tui::App::new(
            &plan,
            &carina_core::schema::SchemaRegistry::new(),
            &historical,
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

/// Entry for serializing current resource states
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentStateEntry {
    pub id: ResourceId,
    pub state: State,
}

fn build_plan_file<E>(
    path: &Path,
    parsed: &carina_core::parser::File<E>,
    provider_configs: &ResolvedProviderConfigs,
    backend_config: Option<BackendConfig>,
    state_file: &Option<StateFile>,
    ctx: &crate::wiring::PlanContext,
) -> Result<PlanFile, carina_core::value::SerializationError> {
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
        .map(|resource| (resource.id.clone(), resource.clone()))
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
    let redacted_current_states = ctx
        .current_states
        .iter()
        .map(|(id, state)| {
            let persisted = persisted_resource_for_plan_address(
                persisted_resources.as_ref(),
                &moved_origins,
                id,
            );
            // A moved resource's previous state row still lives at its source
            // address until execution writes the destination row.
            let redacted = match persisted {
                Some(persisted) => {
                    let persisted_attributes = persisted.persisted_attributes();
                    redact_secrets_in_state_using_persisted(state, persisted_attributes)?
                }
                None => state.clone(),
            };
            let redacted = match desired_resources.get(id) {
                Some(desired) => redact_secrets_in_state_using_resource(&redacted, desired)?,
                None if persisted.is_some() => redacted,
                None => redact_secrets_in_state(&redacted)?,
            };
            Ok((id.clone(), redacted))
        })
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
                Some(persisted) => redact_secrets_in_resource_reusing_persisted_hashes(
                    resource,
                    persisted.persisted_attributes(),
                ),
                None => redact_secrets_in_resource(resource),
            }
        })
        .collect::<Result<Vec<_>, carina_core::value::SerializationError>>()?;
    let redacted_desired_resources: HashMap<ResourceId, Resource> = redacted_sorted_resources
        .iter()
        .map(|resource| (resource.id.clone(), resource.clone()))
        .collect();

    Ok(PlanFile {
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
            &redacted_current_states,
        )?,
        sorted_resources: redacted_sorted_resources,
        unresolved_resources: ctx
            .unresolved_resources
            .iter()
            .zip(&ctx.sorted_resources)
            .map(|(source, evaluated)| {
                redact_secrets_in_resource_using_evaluated(source, evaluated)
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
        current_states: ctx
            .current_states
            .keys()
            .map(|id| CurrentStateEntry {
                id: id.clone(),
                state: redacted_current_states[id].clone(),
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
    })
}

/// Format a `SerializationError` from `build_plan_file` into the
/// CLI's "cannot save plan" diagnostic, suggesting the caller drop
/// `flag` (e.g. `--json`, `--out`) to fall back to display-only.
fn format_plan_save_error(e: &carina_core::value::SerializationError, flag: &str) -> String {
    match e {
        carina_core::value::SerializationError::UnknownNotAllowed { reason, .. } => format!(
            "Cannot save plan: it depends on values that are not yet \
             applied ({reason}). Apply the upstream module(s) first, \
             or rerun without {flag}."
        ),
        _ => e.to_string(),
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
    detail: DetailLevel,
    tui: bool,
    refresh: bool,
    json: bool,
    check_iam: bool,
    strict_iam: bool,
    provider_context: &ProviderContext,
) -> Result<bool, AppError> {
    let loaded = load_configuration_with_config(
        path,
        provider_context,
        &carina_core::schema::SchemaRegistry::new(),
    )?;
    let inference_errors = loaded.inference_errors;
    let duplicate_declarations = loaded.duplicate_declarations;
    let mut parsed = loaded.parsed;
    let unresolved_parsed = loaded.unresolved_parsed;

    let base_dir = get_base_dir(path);
    crate::commands::validate_and_resolve_with_config(
        &mut parsed,
        base_dir,
        false,
        &inference_errors,
        &duplicate_declarations,
    )?;

    let mut plan_backend_config = parsed
        .backend
        .as_ref()
        .map(StateBackendConfig::try_from)
        .transpose()
        .map_err(carina_state::BackendError::from)?;
    let mut plan_file_backend_config = parsed.backend.clone();
    let mut use_locked_backend = false;
    let drift_note = match inspect_backend_drift(base_dir, parsed.backend.as_ref())? {
        BackendDriftStatus::Fresh => {
            return Err(AppError::Config(format!(
                "Backend lock file not found. Run `{}` to initialize the project.",
                ProjectCommand::new("init", path)
            )));
        }
        BackendDriftStatus::Unchanged => None,
        BackendDriftStatus::Drifted {
            existing,
            configured,
        } => {
            let warning = drift_warning(&existing, &configured, path);
            eprintln!("{}", warning.yellow());
            let locked_config = existing.to_state_config()?;
            plan_file_backend_config = Some(locked_config.to_parser_config());
            plan_backend_config = Some(locked_config);
            use_locked_backend = true;
            Some(format!(
                "Backend migration pending: plan read state from the OLD backend recorded in \
                 carina-backend.lock. Run `{}` before apply.",
                ProjectCommand::new("init --migrate-state", path)
            ))
        }
    };

    // Check for backend configuration and load state
    // Use local backend by default if no backend is configured
    let mut will_create_state_bucket = false;
    let mut state_bucket_name = String::new();
    let mut state_file: Option<StateFile> = None;

    let plan_backend: Box<dyn StateBackend> = if let Some(config) = plan_backend_config.as_ref() {
        let backend = create_backend(Some(config), base_dir)
            .await
            .map_err(AppError::Backend)?;

        let bucket_exists = backend.bucket_exists().await.map_err(AppError::Backend)?;

        if bucket_exists {
            // Try to load state from backend. `plan` is read-only and
            // already warns when an in-memory migration was applied,
            // so it drops the migration token via `into_state()` —
            // carina#3315 routes persistence through the lock-held
            // `apply` / `destroy` / `state refresh` paths.
            state_file = backend
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

            let backend_resource_type = backend
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
        backend
    } else {
        // Use local backend by default. Plan is read-only; drop any
        // pending-migration token (carina#3315 — lock-held callers
        // persist instead).
        let backend = create_backend(None, base_dir)
            .await
            .map_err(AppError::Backend)?;
        state_file = backend
            .read_state()
            .await
            .map_err(AppError::Backend)?
            .map(|loaded| loaded.into_state());
        backend
    };

    // T0 fingerprint; see `StateSnapshot` for the TOCTOU rationale.
    let state_snapshot_t0 = StateSnapshot::capture(state_file.as_ref());

    // Show bootstrap plan if needed
    if will_create_state_bucket {
        let backend_provider = plan_backend
            .provider_name()
            .ok_or("Backend does not specify a provider name")?;
        let backend_resource_type = plan_backend
            .resource_type()
            .ok_or("Backend does not specify a resource type")?;
        println!("{}", "Bootstrap Plan:".cyan().bold());
        println!(
            "  {} {} (state bucket)",
            "+".green(),
            format!(
                "{}.{}.{}",
                backend_provider, backend_resource_type, state_bucket_name
            )
            .green()
        );
        println!(
            "  {} Resource definition will be added to .crn file",
            "→".cyan()
        );
        println!();
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
        &mut parsed,
        &mut state_file,
        base_dir,
        provider_context,
        UpstreamMissingStatePolicy::Lenient,
    )
    .await?;
    if !refresh {
        eprintln!(
            "{}",
            "Warning: using cached state (--refresh=false). Plan may not reflect actual infrastructure.".yellow()
        );
    }

    // carina#3132: deferred-for expansion runs inside
    // `create_plan_from_parsed_with_upstream_with_ctx` (post-refresh), which also
    // prints post-expansion warnings and returns the still-unresolved
    // loops via `ctx.residual_deferred_for`.
    let ctx = create_plan_from_parsed_with_upstream_with_ctx(
        &wiring,
        &parsed,
        provider_inputs.configs(),
        &unresolved_parsed.resources,
        &state_file,
        refresh,
        provider_inputs.remote_bindings(),
        &state_block_claims,
        &resolved_state_block_targets,
        base_dir,
    )
    .await?;
    let has_changes = ctx.plan.mutation_count() > 0;

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
        collect_historical_display_attributes(&ctx.plan, &ctx.current_states, state_file.as_ref());

    // Resolve exports against the same pre-apply binding view used for their
    // display and reject secrets that were hidden behind references during
    // the earlier syntax-level validation pass.
    let export_wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();
    let checked_exports = check_exports_pre_apply(CheckExportsPreApplyInput {
        export_params: &parsed.export_params,
        resources: &ctx.sorted_resources,
        compositions: &parsed.compositions,
        data_sources: &parsed.data_sources,
        current_states: &ctx.current_states,
        state_file: state_file.as_ref(),
        plan: &ctx.plan,
        schemas: wiring.schemas(),
        wait_aliases: &export_wait_aliases,
        provider_context,
    })
    .map_err(AppError::Validation)?;

    if json {
        if let Some(note) = drift_note.as_ref() {
            eprintln!("{}", note.yellow());
        }
        // `build_plan_file` propagates `SerializationError` so the user
        // sees an actionable diagnostic (which upstream / for-binding
        // is unresolved) instead of a panic backtrace.
        let plan_file = build_plan_file(
            path,
            &parsed,
            provider_inputs.configs(),
            plan_file_backend_config.clone(),
            &state_file,
            &ctx,
        )
        .map_err(|e| format_plan_save_error(&e, "--json"))?;
        let json_str = serde_json::to_string_pretty(&plan_file)
            .map_err(|e| format!("Failed to serialize plan: {}", e))?;
        println!("{}", json_str);
        if let Some(result) = iam_preflight_result.as_ref() {
            crate::commands::iam_preflight::emit_warnings(result);
        }
    } else if tui {
        carina_tui::run(&ctx.plan, wiring.schemas(), &historical_display)
            .map_err(|e| AppError::Config(format!("TUI error: {}", e)))?;
        if let Some(result) = iam_preflight_result.as_ref() {
            crate::commands::iam_preflight::emit_warnings(result);
        }
    } else {
        let current_exports = state_file
            .as_ref()
            .map(|s| s.exports.clone())
            .unwrap_or_default();
        let export_changes = compute_export_diffs(&checked_exports, &current_exports);
        // Separate the refresh-progress block (printed above when `refresh`)
        // from the plan's terminal section so they don't read as a run-on
        // (#3148).
        print!("{}", refresh_plan_separator(refresh));
        print_plan(
            &ctx.plan,
            detail,
            &historical_display,
            Some(wiring.schemas()),
            &ctx.moved_origins,
            &export_changes,
            &ctx.residual_deferred_for,
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

    // Save plan to file if --out was specified
    if let Some(out_path) = out {
        let plan_file = build_plan_file(
            path,
            &parsed,
            provider_inputs.configs(),
            plan_file_backend_config.clone(),
            &state_file,
            &ctx,
        )
        .map_err(|e| format_plan_save_error(&e, "--out"))?;
        let json_out = carina_core::utils::pretty_with_newline(&plan_file)
            .map_err(|e| format!("Failed to serialize plan: {}", e))?;
        fs::write(out_path, json_out).map_err(|e| format!("Failed to write plan file: {}", e))?;

        println!();
        println!(
            "{}",
            format!("Plan saved to {}", out_path.display())
                .green()
                .bold()
        );
        println!(
            "{}",
            format!(
                "To apply this plan, run: `{}`",
                ProjectCommand::new("apply", out_path)
            )
            .cyan()
        );
    }

    Ok(has_changes)
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

pub(crate) async fn prepare_plan_identity_and_provider_inputs(
    parsed: &mut carina_core::parser::InferredFile,
    state_file: &mut Option<StateFile>,
    base_dir: &Path,
    provider_context: &ProviderContext,
    upstream_policy: UpstreamMissingStatePolicy,
) -> Result<PreparedPlanIdentityInputs, AppError> {
    // Factory/schema wiring uses declaration metadata only. Provider values
    // are folded after identities are reconciled against state.
    let (factories, _) = build_factories_from_providers(&parsed.providers, base_dir)?;
    let wiring = WiringContext::new(factories, &parsed.providers);
    reconcile_prefixed_names(&mut parsed.resources, state_file);
    let state_blocks = crate::wiring::resolve_state_blocks(
        &parsed.state_blocks,
        state_file,
        &parsed.resources,
        wiring.schemas(),
    );
    if let Some(state) = state_file.as_ref() {
        carina_core::module_resolver::reconcile_anonymous_module_instances(
            &mut parsed.resources,
            &mut parsed.compositions,
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
        reconcile_anonymous_identifiers_with_ctx(
            &wiring,
            &mut parsed.resources,
            state,
            &state_blocks.claims,
        )?;
    }

    let provider_inputs = load_and_resolve_provider_configs(
        parsed,
        base_dir,
        provider_context,
        upstream_policy,
        ProviderUpstreamScope::All,
    )
    .await?;

    Ok(PreparedPlanIdentityInputs {
        wiring,
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
    pub data_sources: &'a [carina_core::resource::DataSource],
    pub current_states: &'a HashMap<ResourceId, State>,
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
    let mut secret_aware_states = input.current_states.clone();
    if let Some(state_file) = input.state_file {
        let moved_origins = MovedOrigins::from_plan(input.plan);
        for (id, state) in &mut secret_aware_states {
            let Some(persisted) = moved_origins.find_persisted_row(state_file, id) else {
                continue;
            };
            let persisted_attributes = persisted.persisted_attributes();
            for (key, value) in &mut state.attributes {
                *value = carina_core::value::mask_plaintext_using_persisted_attribute(
                    value,
                    persisted_attributes,
                    key,
                );
            }
        }
    }
    let plan_input_states = carina_core::resource::into_plan_input_map(
        secret_aware_states,
        input.schemas,
        input.resources,
    );
    let bindings = carina_core::binding_index::ResolvedBindings::pre_apply(
        carina_core::binding_index::PreApplyInputs {
            managed: input.resources,
            compositions: input.compositions,
            data_sources: input.data_sources,
            current_states: &plan_input_states,
            remote_bindings: &HashMap::new(),
            wait_aliases: input.wait_aliases,
        },
    );

    carina_core::validation::check_export_values_pre_apply(
        input.export_params,
        &bindings,
        input.provider_context,
    )
}

/// Represents a change to an export value between current state and desired.
pub enum ExportChange {
    Added {
        name: String,
        type_expr: Option<carina_core::parser::TypeExpr>,
        new_value: Value,
    },
    Modified {
        name: String,
        type_expr: Option<carina_core::parser::TypeExpr>,
        old_json: serde_json::Value,
        new_value: Value,
    },
    Removed {
        name: String,
        old_json: serde_json::Value,
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
/// resource states. `current_exports` is the JSON-serialized map from
/// `StateFile.exports`.
pub fn compute_export_diffs(
    checked_exports: &carina_core::validation::ExportsCheckedPreApply<
        carina_core::parser::InferredExportParam,
    >,
    current_exports: &HashMap<String, serde_json::Value>,
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
        let new_json = crate::commands::shared::state_writeback::dsl_value_to_json(value).ok();
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
        .map_err(|source| AppError::ProviderReferencedUpstream {
            provider: required_by.provider,
            attribute: required_by.attribute,
            source: Box::new(source),
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

        // Upstream-state reads are read-only; drop any pending-migration
        // token (carina#3315 — only the owning directory's lock-held
        // apply / destroy / refresh persists).
        let state_file = backend
            .read_state()
            .await
            .map_err(AppError::Backend)?
            .map(|loaded| loaded.into_state());
        let bindings = match (state_file, policy) {
            (Some(sf), _) => sf.build_remote_bindings(),
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
                HashMap::new()
            }
        };
        result.insert(us.binding.clone(), bindings);
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
) -> Result<Box<dyn StateBackend>, AppError> {
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

    // Anchor local-backend state paths at the upstream's source directory
    // so `path = "foo.json"` resolves relative to the upstream, not the
    // downstream process's CWD.
    let upstream_backend_config = loaded
        .parsed
        .backend
        .as_ref()
        .map(StateBackendConfig::try_from)
        .transpose()
        .map_err(carina_state::BackendError::from)?;
    let backend: Box<dyn StateBackend> =
        create_backend(upstream_backend_config.as_ref(), source_abs)
            .await
            .map_err(AppError::Backend)?;

    Ok(backend)
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
    use std::error::Error as _;
    use std::fs;

    fn write_state(dir: &Path, exports: &[(&str, serde_json::Value)]) {
        let mut state = StateFile::new();
        for (k, v) in exports {
            state.exports.insert(k.to_string(), v.clone());
        }
        fs::write(
            dir.join(carina_state::LocalBackend::DEFAULT_STATE_FILE),
            serde_json::to_string(&state).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn provider_resolution_preserves_parse_error_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut parsed = carina_core::parser::InferredFile::default();
        parsed.providers.push(ProviderConfig {
            name: "test".to_string(),
            attributes: indexmap::IndexMap::from([(
                "region".to_string(),
                Value::Deferred(DeferredValue::FunctionCall {
                    name: "no_such_provider_function".to_string(),
                    args: Vec::new(),
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
            Ok(_) => panic!("unknown provider function must fail resolution"),
            Err(error) => error,
        };

        let source = error.source().expect("AppError must retain a source");
        assert!(
            source
                .downcast_ref::<Box<carina_core::parser::ParseError>>()
                .is_some(),
            "ParseError must remain in the AppError source chain: {error:?}"
        );
    }

    #[tokio::test]
    async fn provider_resolution_preserves_invariant_error_source() {
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
        let source = error.source().expect("AppError must retain a source");
        assert!(
            source
                .downcast_ref::<Box<carina_core::parser::ProviderConfigInvariantError>>()
                .is_some(),
            "ProviderConfigInvariantError must remain in the AppError source chain: {error:?}"
        );
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

    /// Lenient policy with the same setup must NOT error — it warns and
    /// returns an empty binding so downstream display can stamp the marker.
    #[tokio::test]
    async fn load_upstream_states_lenient_returns_empty_when_state_file_missing() {
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
            result.contains_key("orgs"),
            "binding name must be present so display can stamp the marker"
        );
        assert!(
            result["orgs"].is_empty(),
            "no exports available, so the inner map must be empty"
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
        state_a
            .exports
            .insert("region".to_string(), serde_json::json!("ap-northeast-1"));
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

        run_plan(
            &dir_b,
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
    use carina_core::parser::{CompareOp, TypeExpr, ValidateExpr};
    use carina_core::provider::ProviderRouter;
    use carina_core::resource::{
        AccessPath, Composition, CompositionArgument, DataSource, DeferredValue,
        ModuleConstraintId, PendingModuleConstraint, ResourceId, Signature, Value,
    };
    use std::fs;

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
        let persisted_update_marker = carina_core::value::value_to_json(
            updated.attributes.get("comment").expect("updated secret"),
        )
        .expect("persisted update marker");
        let mut state_file = StateFile::new();
        state_file
            .upsert_resource(
                carina_state::ResourceState::new("test.resource", "updated", "mock")
                    .with_identifier("updated-id")
                    .with_attribute("comment", persisted_update_marker.clone()),
            )
            .expect("persisted update row");
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            carina_core::resource::ResolvedResource::new(created_effect),
        ));
        plan.add(Effect::Update {
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
            sorted_resources: vec![created.clone(), updated.clone()],
            unresolved_resources: vec![created, updated],
            compositions: Vec::new(),
            data_sources: Vec::new(),
            current_states: HashMap::new(),
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
            carina_core::value::persisted_json_to_dsl_value(&persisted_update_marker)
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
        let mut bucket = carina_state::ResourceState::new("test.resource", "bucket", "mock")
            .with_identifier("pref-existing");
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
            current_states: HashMap::new(),
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
        let plan_file = build_plan_file(&path, &parsed, &provider_configs, None, &None, &ctx)
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
            CompositionArgument::from_value(
                Value::resource_ref("producer", "name", vec![]),
                TypeExpr::String,
            ),
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
            current_states: HashMap::new(),
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
        let plan_file = build_plan_file(&path, &parsed, &provider_configs, None, &None, &ctx)
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
            current_states: HashMap::new(),
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

        let error = build_plan_file(&path, &parsed, &provider_configs, None, &None, &ctx)
            .expect_err("a secret-bearing upstream snapshot must be rejected")
            .to_string();
        assert!(
            error.contains("upstream_state network.password"),
            "saved-plan error lost the upstream field location: {error}",
        );
    }
}

#[cfg(test)]
mod export_diff_tests {
    use super::*;
    use carina_core::parser::{InferredExportParam, TypeExpr};
    use carina_core::resource::DeferredValue;

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
        current.insert("count".to_string(), serde_json::json!(7));
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
        current.insert("count".to_string(), serde_json::json!(42));
        let changes = compute_export_diffs(&params, &current);
        assert!(changes.is_empty());
    }

    #[test]
    fn compute_export_diffs_removed_when_param_missing() {
        let params = checked(Vec::new());
        let mut current = HashMap::new();
        current.insert("stale".to_string(), serde_json::json!("old"));
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
        current.insert("modified".to_string(), serde_json::json!(99));
        current.insert("removed".to_string(), serde_json::json!("old"));
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
                data_sources: &[],
                current_states: &HashMap::new(),
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
                data_sources: &[],
                current_states: &HashMap::new(),
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
    //! by `value_to_json` / `redact_secrets_in_*` directly, surfaced
    //! through `build_plan_file`'s `SerializationError` return. Verify
    //! the actionable wording and that every effect shape (top-level
    //! attributes, nested List/Map, Replace cascade) propagates the
    //! error.
    use carina_core::resource::{
        AccessPath, ConcreteValue, DeferredValue, Resource, UnknownReason, Value,
    };
    use carina_core::value::{SerializationError, redact_secrets_in_resource, value_to_json};

    fn upstream_unknown() -> Value {
        let path = AccessPath::with_fields("network", "vpc", vec!["vpc_id".into()]);
        Value::Deferred(DeferredValue::Unknown(UnknownReason::UpstreamRef { path }))
    }

    #[test]
    fn value_to_json_errors_on_top_level_unknown() {
        let err = value_to_json(&upstream_unknown()).unwrap_err();
        match err {
            SerializationError::UnknownNotAllowed {
                reason: UnknownReason::UpstreamRef { path },
                ..
            } => {
                assert_eq!(path.to_dot_string(), "network.vpc.vpc_id");
            }
            other => panic!("expected UnknownNotAllowed/UpstreamRef, got: {other:?}"),
        }
    }

    #[test]
    fn value_to_json_errors_on_unknown_inside_list_or_map() {
        let list_val = Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String("a".into())),
            upstream_unknown(),
        ]));
        assert!(matches!(
            value_to_json(&list_val).unwrap_err(),
            SerializationError::UnknownNotAllowed { .. }
        ));

        let mut m: indexmap::IndexMap<String, Value> = indexmap::IndexMap::new();
        m.insert("Name".into(), upstream_unknown());
        assert!(matches!(
            value_to_json(&Value::Concrete(ConcreteValue::Map(m))).unwrap_err(),
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
            let err = value_to_json(&Value::Deferred(DeferredValue::Unknown(variant.clone())))
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
