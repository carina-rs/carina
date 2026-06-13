//! Build plan output from `tests/fixtures/plan_display` directories.
//!
//! Shared between the plan snapshot tests and the `plan-fixture` example
//! binary. The logic mirrors `--refresh=false` but skips provider plugin
//! loading so fixture rendering works without installed providers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use carina_core::config_loader::{get_base_dir, load_configuration};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{cascade_dependent_updates, create_plan};
use carina_core::executor::normalized::apply_desired_normalization_slice;
use carina_core::plan::Plan;
use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult};
use carina_core::resolver::resolve_refs_for_plan;
use carina_core::resource::{ResourceId, State, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, DslTransform, ResourceSchema, SchemaRegistry, TypeIdentity,
};
use carina_state::{StateFile, check_and_migrate};

use crate::commands::validate_and_resolve;
use crate::wiring::{
    WiringContext, compute_anonymous_identifiers_with_ctx, normalize_state_with_ctx,
    reconcile_anonymous_identifiers_with_ctx, reconcile_prefixed_names,
    resolve_enum_aliases_in_states,
};

/// Fixture root path relative to the `carina-cli` crate manifest.
const FIXTURE_SUBPATH: &str = "tests/fixtures/plan_display";

/// Complete output of fixture-based plan construction.
pub struct FixturePlan {
    pub plan: Plan,
    pub current_states: HashMap<ResourceId, State>,
    pub schemas: SchemaRegistry,
    pub moved_origins: HashMap<ResourceId, ResourceId>,
    pub deferred_for_expressions: Vec<carina_core::parser::DeferredForExpression>,
    pub export_params: Vec<carina_core::parser::InferredExportParam>,
    pub resolved_export_params: Vec<carina_core::parser::InferredExportParam>,
    /// Per-resource user-authoring trees lifted from the fixture's
    /// `carina.state.json`. Forwarded to `format_plan` so server-side
    /// default fields the user never wrote do not surface in plan
    /// output (refs awscc#206).
    pub prev_explicit: HashMap<ResourceId, carina_core::explicit::ExplicitFields>,
    /// Plan-scoped lineage of leaf nodes back to their composition
    /// call sites — surfaced here so fixture-based snapshot tests
    /// can render the composition-group header the same way the
    /// real `carina plan` command does (carina#3322).
    pub expansion_trace: carina_core::resource::ExpansionTrace,
}

/// Build a plan from a fixture directory name (e.g. "all_create"). Resolves
/// the fixture under `CARGO_MANIFEST_DIR/tests/fixtures/plan_display/<name>`.
pub fn build_plan_from_fixture_name(fixture_dir: &str) -> FixturePlan {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixture_path = PathBuf::from(format!(
        "{}/{}/{}",
        manifest_dir, FIXTURE_SUBPATH, fixture_dir
    ));
    build_plan_from_fixture_path(&fixture_path)
}

/// Build a plan from an absolute fixture path. Loads the configuration,
/// validates with `skip_resource_validation=true` (no provider plugin load),
/// and produces a plan equivalent to `carina plan --refresh=false`.
pub fn build_plan_from_fixture_path(fixture_path: &Path) -> FixturePlan {
    let fixture_pathbuf = fixture_path.to_path_buf();
    let state_path = fixture_path.join(carina_state::LocalBackend::DEFAULT_STATE_FILE);

    let mut parsed = load_configuration(&fixture_pathbuf).unwrap().parsed;
    let base_dir = get_base_dir(&fixture_pathbuf);
    validate_and_resolve(&mut parsed, base_dir, true).unwrap();

    let mut state_file: Option<StateFile> = if state_path.exists() {
        let json = std::fs::read_to_string(&state_path).unwrap();
        // Go through check_and_migrate so v5 fixtures (with the legacy
        // `desired_keys` array) are lifted into v6 `explicit` trees on
        // load. Direct `serde_json::from_str` would silently drop the
        // legacy field and leave `explicit` empty, breaking the
        // moved_prev_keys snapshot test.
        Some(check_and_migrate(&json).unwrap().into_state())
    } else {
        None
    };

    let wiring = WiringContext::new(fixture_provider_factories(&fixture_pathbuf));
    if fixture_pathbuf
        .file_name()
        .is_some_and(|name| name == "moved_claims_precede_heuristics")
    {
        // This fixture writes moved.to as the literal schema-derived anonymous
        // hash name, so compute the desired anonymous id before claims resolve.
        let canonical_resources = carina_core::value::canonicalize_resources_with_schemas(
            &mut parsed.resources,
            wiring.schemas(),
        );
        let errors =
            compute_anonymous_identifiers_with_ctx(&wiring, canonical_resources, &parsed.providers);
        assert!(errors.is_empty(), "{errors:?}");
    }
    reconcile_prefixed_names(&mut parsed.resources, &state_file);
    let crate::wiring::StateBlockResolution {
        claims: state_block_claims,
        targets: resolved_state_block_targets,
    } = crate::wiring::resolve_state_blocks(
        &parsed.state_blocks,
        &state_file,
        &parsed.resources,
        wiring.schemas(),
    );
    if let Some(sf) = state_file.as_ref() {
        carina_core::module_resolver::reconcile_anonymous_module_instances(
            &mut parsed.resources,
            &|provider, resource_type| {
                sf.resources_by_type(provider, resource_type)
                    .into_iter()
                    .map(|r| r.name.clone())
                    .collect()
            },
            &state_block_claims,
        );
    }
    if let Some(sf) = state_file.as_mut() {
        reconcile_anonymous_identifiers_with_ctx(
            &wiring,
            &mut parsed.resources,
            sf,
            &state_block_claims,
        );
    }

    // carina#3181: `parsed.resources` is managed-only; data sources live
    // in `parsed.data_sources`. Only managed resources are dependency-
    // sorted.
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources).unwrap();
    let data_sources: Vec<carina_core::resource::DataSource> = parsed.data_sources.clone();

    let mut current_states: HashMap<ResourceId, State> = HashMap::new();
    if let Some(sf) = state_file.as_ref() {
        for resource in &sorted_resources {
            let state = sf.build_state_for_resource(&resource.id);
            current_states.insert(resource.id.clone(), state);
        }
        for ds in &data_sources {
            let state = sf.build_state_for_resource(&ds.id);
            current_states.insert(ds.id.clone(), state);
        }

        let desired_ids: HashSet<ResourceId> = sorted_resources
            .iter()
            .map(|r| r.id.clone())
            .chain(data_sources.iter().map(|d| d.id.clone()))
            .collect();
        for (id, state) in sf.build_orphan_states(&desired_ids) {
            current_states.entry(id).or_insert(state);
        }
    } else {
        for resource in &sorted_resources {
            current_states.insert(resource.id.clone(), State::not_found(resource.id.clone()));
        }
        for ds in &data_sources {
            current_states.insert(ds.id.clone(), State::not_found(ds.id.clone()));
        }
    }

    let directives_map = state_file
        .as_ref()
        .map(|sf| sf.build_directives())
        .unwrap_or_default();

    let mut saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default();

    // Keep fixture plans aligned with the real CLI plan/apply wiring:
    // state-file attributes are lifted before the differ sees them, so
    // fixture coverage exercises the enum-state lift seam instead of
    // relying on differ cross-shape tolerance.
    carina_core::utils::lift_saved_state_enum_leaves(
        &mut saved_attrs,
        &sorted_resources,
        wiring.schemas(),
    );

    let mut prev_explicit = state_file
        .as_ref()
        .map(|sf| sf.build_explicit())
        .unwrap_or_default();

    // Insert every upstream binding — even when the state file is
    // unreadable — so `resolve_refs_for_plan` knows the binding name
    // and can stamp surviving refs as `(known after upstream apply: <ref>)`
    // instead of leaving the raw dot-form (#2366).
    let mut remote_bindings: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for us in &parsed.upstream_states {
        let dir = if us.source.is_absolute() {
            us.source.clone()
        } else {
            base_dir.join(&us.source)
        };
        let state_path = dir.join(carina_state::LocalBackend::DEFAULT_STATE_FILE);
        let bindings = std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|content| check_and_migrate(&content).ok())
            .map(|migrated| migrated.state.build_remote_bindings())
            .unwrap_or_default();
        remote_bindings.insert(us.binding.clone(), bindings);
    }

    // Plan-only resolution: stamps surviving upstream refs for display
    // as `(known after upstream apply: <ref>)` (#2366). The fixture
    // loader is already lenient about missing upstream state files (see
    // the loop above that silently skips them).
    let mut resources = sorted_resources.clone();
    let wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();
    // carina#3248: build unified pre-apply bindings (managed +
    // composition + data sources) so composition-rooted refs in the fixture
    // resolve through the composition layer to the managed sibling.
    let upstream_binding_names: std::collections::HashSet<&str> =
        remote_bindings.keys().map(String::as_str).collect();
    let plan_bindings = carina_core::binding_index::ResolvedBindings::pre_apply(
        carina_core::binding_index::PreApplyInputs {
            managed: &resources,
            compositions: &parsed.compositions,
            data_sources: &data_sources,
            current_states: &current_states,
            remote_bindings: &remote_bindings,
            wait_aliases: &wait_aliases,
        },
    );
    resolve_refs_for_plan(&mut resources, &plan_bindings, &upstream_binding_names)
        .expect("Failed to resolve refs with state");

    // Resolve data-source input refs for the plan (carina#3181).
    let mut data_sources_for_plan = data_sources.clone();
    carina_core::resolver::resolve_data_source_refs_for_plan(
        &mut data_sources_for_plan,
        &plan_bindings,
        &upstream_binding_names,
    )
    .expect("Failed to resolve data source refs with state");

    carina_core::value::canonicalize_data_sources_with_schemas(
        &mut data_sources_for_plan,
        wiring.schemas(),
    );
    carina_core::utils::lift_current_state_enum_leaves(
        &mut current_states,
        &sorted_resources,
        wiring.schemas(),
    );
    carina_core::utils::lift_current_state_enum_leaves_for_data_sources(
        &mut current_states,
        &data_sources,
        wiring.schemas(),
    );
    carina_core::value::canonicalize_states_with_schemas(&mut current_states, wiring.schemas());

    normalize_state_with_ctx(&wiring, &mut current_states);

    {
        use carina_core::provider::ProviderRouter;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("failed to build tokio runtime for desired normalization");
        let mut router = ProviderRouter::new();
        for factory in wiring.factories() {
            let attrs = indexmap::IndexMap::new();
            router.add_normalizer(rt.block_on(factory.create_normalizer(None, &attrs)));
        }
        rt.block_on(apply_desired_normalization_slice(
            &mut resources,
            &parsed.providers,
            &router,
            wiring.factories(),
            wiring.schemas(),
        ));
    }

    resolve_enum_aliases_in_states(&wiring, &mut current_states);

    let orphan_dependencies = if let Some(sf) = state_file.as_ref() {
        let desired_ids: HashSet<ResourceId> =
            sorted_resources.iter().map(|r| r.id.clone()).collect();
        sf.build_orphan_dependencies(&desired_ids)
    } else {
        HashMap::new()
    };

    let moved_pairs = crate::wiring::materialize_moved_states(
        &mut current_states,
        &mut prev_explicit,
        &mut saved_attrs,
        &parsed.state_blocks,
        &state_file,
    );
    crate::wiring::validate_plan_time_state_block_collisions(
        &sorted_resources,
        &moved_pairs,
        &resolved_state_block_targets,
        &state_file,
    )
    .expect("state block collision validation failed");

    // carina#3358: resolve `until` predicate enum aliases before the
    // differ lowers the wait, the same step the plan/apply pipelines run.
    // The fixture harness uses an empty factory set so this is a no-op
    // here, but keeping the sibling call consistent means a fixture that
    // ever gains real factories cannot silently regress.
    let mut wait_bindings = parsed.wait_bindings.clone();
    crate::wiring::resolve_enum_aliases_in_wait_bindings(
        &wiring,
        &mut wait_bindings,
        &resources,
        &data_sources_for_plan,
    );
    let mut plan = create_plan(
        &resources,
        &data_sources_for_plan,
        &current_states,
        &directives_map,
        wiring.schemas(),
        &saved_attrs,
        &prev_explicit,
        &orphan_dependencies,
        &wait_bindings,
    );

    cascade_dependent_updates(
        &mut plan,
        &sorted_resources,
        &current_states,
        wiring.schemas(),
    );

    crate::wiring::add_state_block_effects(
        &mut plan,
        &parsed.state_blocks,
        &state_file,
        &moved_pairs,
        wiring.schemas(),
        &plan_bindings,
        &upstream_binding_names,
    );

    let moved_origins: HashMap<ResourceId, ResourceId> = moved_pairs
        .iter()
        .map(|(from, to)| (to.clone(), from.clone()))
        .collect();
    let export_wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();
    let resolved_export_params = crate::commands::plan::resolve_export_values_for_display(
        &parsed.export_params,
        &resources,
        &parsed.compositions,
        &data_sources_for_plan,
        &current_states,
        &export_wait_aliases,
    );

    FixturePlan {
        plan,
        current_states,
        schemas: wiring.schemas().clone(),
        moved_origins,
        deferred_for_expressions: parsed.deferred_for_expressions,
        export_params: parsed.export_params,
        resolved_export_params,
        prev_explicit,
        expansion_trace: parsed.expansion_trace,
    }
}

fn fixture_provider_factories(fixture_path: &Path) -> Vec<Box<dyn ProviderFactory>> {
    match fixture_path.file_name().and_then(|name| name.to_str()) {
        Some("dynamic_enum_az_no_diff") => vec![Box::new(DynamicEnumFixtureFactory)],
        Some("enum_display") => vec![Box::new(EnumDisplayFixtureFactory)],
        Some("moved_claims_precede_heuristics") => {
            vec![Box::new(MovedClaimsPrecedeHeuristicsFixtureFactory)]
        }
        Some("replace_create_only") => vec![Box::new(ReplaceCreateOnlyFixtureFactory)],
        Some("route53_hosted_zone_name_strip_suffix_no_diff") => {
            vec![Box::new(Route53HostedZoneFixtureFactory)]
        }
        _ => vec![],
    }
}

struct ReplaceCreateOnlyFixtureFactory;

impl ProviderFactory for ReplaceCreateOnlyFixtureFactory {
    fn name(&self) -> &str {
        "test"
    }

    fn display_name(&self) -> &str {
        "Test fixture provider"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(
        &self,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![
            ResourceSchema::new("test.Widget")
                .attribute(
                    AttributeSchema::new("external_name", AttributeType::string()).create_only(),
                )
                .attribute(
                    AttributeSchema::new("legacy_token", AttributeType::string())
                        .create_only()
                        .removable(),
                ),
        ]
    }
}

struct MovedClaimsPrecedeHeuristicsFixtureFactory;

impl ProviderFactory for MovedClaimsPrecedeHeuristicsFixtureFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWS Cloud Control fixture provider"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(
        &self,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![ResourceSchema::new("test.Widget").attribute(
            AttributeSchema::new("external_name", AttributeType::string()).create_only(),
        )]
    }
}

struct EnumDisplayFixtureFactory;

impl ProviderFactory for EnumDisplayFixtureFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWS Cloud Control fixture provider"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::from([(
            "region".to_string(),
            AttributeType::enum_(
                TypeIdentity::new(Some("awscc"), Vec::<String>::new(), "Region"),
                None,
                vec![],
                None,
                Some(DslTransform::HyphenToUnderscore),
            ),
        )])
    }

    fn validate_config(
        &self,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        let tenancy = AttributeType::enum_(
            TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
            Some(vec![
                "default".to_string(),
                "dedicated".to_string(),
                "host".to_string(),
            ]),
            vec![],
            None,
            None,
        );
        vec![
            ResourceSchema::new("ec2.Vpc")
                .attribute(AttributeSchema::new("cidr_block", AttributeType::string()).required())
                .attribute(AttributeSchema::new("instance_tenancy", tenancy).required()),
        ]
    }
}

struct DynamicEnumFixtureFactory;

impl ProviderFactory for DynamicEnumFixtureFactory {
    fn name(&self) -> &str {
        "fixture"
    }

    fn display_name(&self) -> &str {
        "Fixture provider"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(
        &self,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "test".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        let zone_name = AttributeType::enum_(
            TypeIdentity::new(
                Some("fixture"),
                vec!["network".to_string(), "Subnet".to_string()],
                "ZoneName",
            ),
            None,
            vec![],
            None,
            Some(carina_core::schema::DslTransform::HyphenToUnderscore),
        );
        vec![
            ResourceSchema::new("network.Subnet")
                .attribute(AttributeSchema::new("availability_zone", zone_name).required()),
        ]
    }
}

struct Route53HostedZoneFixtureFactory;

impl ProviderFactory for Route53HostedZoneFixtureFactory {
    fn name(&self) -> &str {
        "fixture"
    }

    fn display_name(&self) -> &str {
        "Fixture provider"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(
        &self,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "test".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &indexmap::IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        let zone_name = AttributeType::refined_string(
            Some(TypeIdentity::new(
                Some("fixture"),
                vec!["route53".to_string(), "HostedZone".to_string()],
                "Name",
            )),
            None,
            Some((None, Some(1024))),
            Some(DslTransform::StripSuffix(".".to_string())),
        );
        vec![
            ResourceSchema::new("route53.HostedZone")
                .attribute(AttributeSchema::new("name", zone_name).required()),
        ]
    }
}

/// Collect `Delete`-effect attributes from `current_states` for display.
pub fn delete_attributes_from_plan(
    plan: &Plan,
    current_states: &HashMap<ResourceId, State>,
) -> HashMap<ResourceId, HashMap<String, Value>> {
    plan.effects()
        .iter()
        .filter_map(|e| {
            if let carina_core::effect::Effect::Delete { id, .. } = e {
                current_states
                    .get(id)
                    .map(|s| (id.clone(), s.attributes.clone()))
            } else {
                None
            }
        })
        .collect()
}

/// Collect the attributes of all live states (for destroy rendering).
pub fn delete_attributes_from_states(
    current_states: &HashMap<ResourceId, State>,
) -> HashMap<ResourceId, HashMap<String, Value>> {
    current_states
        .iter()
        .filter(|(_, state)| state.exists)
        .map(|(id, state)| (id.clone(), state.attributes.clone()))
        .collect()
}
