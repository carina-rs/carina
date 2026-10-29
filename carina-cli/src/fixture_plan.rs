//! Build plan output from `tests/fixtures/plan_display` directories.
//!
//! Shared between the plan snapshot tests and the `plan-fixture` example
//! binary. It builds from fixture state without loading provider plugins or
//! performing live provider reads, so rendering works without installed
//! providers.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use carina_core::config_loader::{get_base_dir, load_configuration};
use carina_core::data_source_value::{DataSourceValue, DataSourceValues};
use carina_core::differ::{
    Orphans, PlanPriorStates, create_plan_with_cascades, into_all_prior_states, provider_states_for,
};
use carina_core::executor::UnresolvedResource;
use carina_core::plan::Plan;
use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult, ProviderRouter};
use carina_core::resource::{Current, ResolvedResource, ResourceId, State, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, DslTransform, ResourceSchema, SchemaRegistry, StructField,
    TypeIdentity, ValidatedResourceSchema,
};
use carina_state::{PublishedExports, StateFile, check_and_migrate};

use crate::commands::plan::compute_export_diffs;
use crate::plan_summary::PlanChangeSummary;
use crate::wiring::{
    LoadedProviderFactory, OverrideAwareBuildMode, OverrideAwareResources, PlanPreparationError,
    PlanPreprocessor, ProviderLoad, RebuildBindingsInputs, ValidatedConfig,
    add_deferred_create_effects,
};

/// Fixture root path relative to the `carina-cli` crate manifest.
const FIXTURE_SUBPATH: &str = "tests/fixtures/plan_display";

/// Minimal schema catalog for provider-less plan fixtures.
///
/// These schemas intentionally model the immutable inputs needed for anonymous
/// identity plus the schema-sensitive fields used by fixture diffing. Richer
/// provider-specific formatting/validation fixtures add local schemas below,
/// which take precedence over matching entries in this catalog.
fn fixture_schemas() -> Vec<(String, ValidatedResourceSchema)> {
    let create_only_string = |name| {
        AttributeSchema::optional(name, AttributeType::string())
            .create_only()
            .build()
    };
    let aws = |schema| ("aws".to_string(), schema);
    let awscc = |schema| ("awscc".to_string(), schema);
    let schemas = vec![
        aws(ResourceSchema::new("acm.Certificate").attribute(create_only_string("domain_name"))),
        aws(ResourceSchema::new("acm.CertificateValidation")
            .attribute(create_only_string("certificate_arn"))
            .attribute(create_only_string("validation_record_id"))),
        aws(ResourceSchema::new("iam.Role").attribute(create_only_string("role_name"))),
        aws(ResourceSchema::new("route53.RecordSet")
            .attribute(create_only_string("hosted_zone_id"))
            .attribute(create_only_string("name"))
            .attribute(create_only_string("type"))),
        aws(ResourceSchema::new("s3.Bucket").attribute(create_only_string("bucket_name"))),
        awscc(ResourceSchema::new("ec2.RouteTable").attribute(create_only_string("vpc_id"))),
        awscc(
            ResourceSchema::new("ec2.SecurityGroup")
                .attribute(create_only_string("group_name"))
                .attribute(create_only_string("vpc_id")),
        ),
        awscc(
            ResourceSchema::new("ec2.Subnet")
                .attribute(create_only_string("availability_zone"))
                .attribute(create_only_string("cidr_block"))
                .attribute(create_only_string("vpc_id")),
        ),
        awscc(
            ResourceSchema::new("ec2.Vpc")
                .attribute(create_only_string("cidr_block"))
                .attribute(
                    AttributeSchema::optional("instance_tenancy", fixture_instance_tenancy_type())
                        .build(),
                )
                .attribute(
                    AttributeSchema::optional("enable_dns_support", AttributeType::bool()).build(),
                )
                .attribute(
                    AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                        .build(),
                ),
        ),
        awscc(
            ResourceSchema::new("elasticloadbalancingv2.Listener")
                .attribute(
                    AttributeSchema::optional(
                        "default_actions",
                        AttributeType::list(AttributeType::struct_(
                            "Action",
                            vec![
                                StructField::optional(
                                    "authenticate_oidc_config",
                                    AttributeType::struct_(
                                        "AuthenticateOidcConfig",
                                        vec![
                                            StructField::optional(
                                                "issuer",
                                                AttributeType::string(),
                                            )
                                            .build(),
                                            StructField::optional(
                                                "client_secret",
                                                AttributeType::string(),
                                            )
                                            .write_only()
                                            .build(),
                                        ],
                                    ),
                                )
                                .build(),
                            ],
                        )),
                    )
                    .build(),
                )
                .attribute(AttributeSchema::optional("ssl_policy", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("port", AttributeType::int())
                        .create_only()
                        .build(),
                ),
        ),
        awscc(ResourceSchema::new("iam.Role").attribute(create_only_string("role_name"))),
        awscc(
            ResourceSchema::new("issue3885.Role")
                .attribute(create_only_string("role_name"))
                .attribute(
                    AttributeSchema::provider_populated("arn", AttributeType::string()).build(),
                ),
        ),
        awscc(
            ResourceSchema::new("iam.RolePolicy")
                .attribute(create_only_string("policy_name"))
                .attribute(create_only_string("role_name"))
                .attribute(
                    AttributeSchema::optional("policy_document", AttributeType::string()).build(),
                ),
        ),
        awscc(ResourceSchema::new("logs.LogGroup").attribute(create_only_string("log_group_name"))),
        awscc(ResourceSchema::new("s3.Bucket").attribute(create_only_string("bucket_name"))),
        awscc(
            ResourceSchema::new("sso.Assignment")
                .attribute(create_only_string("instance_arn"))
                .attribute(create_only_string("permission_set_arn"))
                .attribute(create_only_string("principal_id"))
                .attribute(create_only_string("principal_type"))
                .attribute(create_only_string("target_id"))
                .attribute(create_only_string("target_type")),
        ),
        awscc(ResourceSchema::new("test.Widget").attribute(create_only_string("external_name"))),
    ];
    schemas
        .into_iter()
        .map(|(provider, schema)| {
            (
                provider,
                schema.validate().expect("fixture schema must be valid"),
            )
        })
        .collect()
}

/// Complete output of fixture-based plan construction.
pub struct FixturePlan {
    pub plan: Plan,
    /// Resolved desired resources, including unchanged resources omitted from
    /// `plan.effects()`. Plan-image tests use these to retain only unchanged
    /// connectors between changed nodes.
    pub desired_resources: Vec<ResolvedResource>,
    /// Pre-resolution resource snapshots paired to `desired_resources`.
    pub unresolved_resources: Vec<UnresolvedResource>,
    pub compositions: Vec<carina_core::resource::LiveComposition>,
    pub managed_current: HashMap<ResourceId, Current>,
    pub data_source_states: HashMap<ResourceId, State>,
    pub state_file: Option<StateFile>,
    pub schemas: SchemaRegistry,
    pub moved_origins: HashMap<ResourceId, ResourceId>,
    pub deferred_for_expressions: Vec<carina_core::parser::DeferredForExpression>,
    pub export_params: Vec<carina_core::parser::InferredExportParam>,
    pub resolved_export_params:
        carina_core::validation::ExportsCheckedPreApply<carina_core::parser::InferredExportParam>,
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
#[cfg(test)]
pub fn build_plan_from_fixture_name(fixture_dir: &str) -> FixturePlan {
    build_plan_from_fixture_name_using(fixture_dir, LoadedProviderFactory::for_test)
}

/// Build a fixture plan with caller-supplied, identity-carrying factories.
pub fn build_plan_from_fixture_name_using<F>(fixture_dir: &str, load_factories: F) -> FixturePlan
where
    F: Fn(Vec<Box<dyn ProviderFactory>>) -> Vec<LoadedProviderFactory>,
{
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let fixture_path = PathBuf::from(format!(
        "{}/{}/{}",
        manifest_dir, FIXTURE_SUBPATH, fixture_dir
    ));
    build_plan_from_fixture_path_using(&fixture_path, load_factories)
}

/// Build the exact non-destroy display summary for a fixture, including its
/// state-aware export diff and residual deferred-for entries.
pub fn build_fixture_plan_change_summary(fixture: &FixturePlan) -> PlanChangeSummary {
    let current_exports = fixture
        .state_file
        .as_ref()
        .map(|state| state.exports.clone())
        .unwrap_or_default();
    let export_changes = compute_export_diffs(&fixture.resolved_export_params, &current_exports);
    PlanChangeSummary::new(
        &fixture.plan,
        &export_changes,
        &fixture.deferred_for_expressions,
    )
}

/// Build a plan from an absolute fixture path. Loads the configuration,
/// validates with `skip_resource_validation=true` (no provider plugin load),
/// and produces a provider-less fixture plan from the persisted state.
#[cfg(test)]
pub fn build_plan_from_fixture_path(fixture_path: &Path) -> FixturePlan {
    build_plan_from_fixture_path_using(fixture_path, LoadedProviderFactory::for_test)
}

/// Build a fixture plan with caller-supplied, identity-carrying factories.
pub fn build_plan_from_fixture_path_using<F>(fixture_path: &Path, load_factories: F) -> FixturePlan
where
    F: Fn(Vec<Box<dyn ProviderFactory>>) -> Vec<LoadedProviderFactory>,
{
    let fixture_pathbuf = fixture_path.to_path_buf();
    let state_path = fixture_path.join(carina_state::LocalBackend::DEFAULT_STATE_FILE);

    let loaded = load_configuration(&fixture_pathbuf).unwrap();
    let inference_errors = loaded.inference_errors;
    let duplicate_declarations = loaded.duplicate_declarations;
    let unresolved_parsed = loaded.unresolved_parsed;
    let parsed = loaded.parsed;
    let base_dir = get_base_dir(&fixture_pathbuf);
    // Fixture planning deliberately skips loading provider artifacts. Supply
    // only the enum metadata validation needs here: the full fixture schema
    // catalog would assign anonymous identities earlier than the real plan
    // path and make the harness observe the wrong inputs.
    let validation_providers = ProviderLoad::for_fixture_with_additional_schemas(
        load_factories(fixture_provider_config_factories()),
        &parsed.providers,
        fixture_validation_enum_schemas(&fixture_pathbuf),
    );
    let validation_config = validation_providers
        .validate_config_pair(
            parsed,
            unresolved_parsed,
            base_dir,
            true,
            &inference_errors,
            &duplicate_declarations,
        )
        .unwrap_or_else(|error| panic!("fixture validation failed: {error}"));
    let canonical_resources = validation_config.into_resources_for_fixture();
    let provider_configs = canonical_resources.parsed().providers.clone();
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

    // This fixture intentionally uses a second, fuller schema context for
    // planning; the test-only constructor marks that separately assembled
    // context as the fixture's validated execution wiring.
    let mut validated_config = ValidatedConfig::for_fixture_with_additional_schemas(
        canonical_resources,
        load_factories(fixture_provider_factories(&fixture_pathbuf)),
        &provider_configs,
        fixture_schemas(),
    );
    if fixture_pathbuf
        .file_name()
        .is_some_and(|name| name == "moved_claims_precede_heuristics")
    {
        // This fixture writes moved.to as the literal schema-derived anonymous
        // hash name, so compute the desired anonymous id before claims resolve.
        let errors = validated_config
            .compute_anonymous_identifiers_for_fixture(&provider_configs)
            .unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    }
    let crate::wiring::StateBlockResolution {
        claims: state_block_claims,
        targets: resolved_state_block_targets,
    } = validated_config.reconcile_identities_with_state(&state_file);
    let (wiring, canonical_resources) = validated_config.context_and_resources();
    let parsed = canonical_resources.parsed();

    // carina#3181: `parsed.resources` is managed-only; data sources live
    // in `parsed.data_sources`. Only managed resources are dependency-
    // sorted.
    let data_sources: Vec<carina_core::resource::DataSource> = parsed.data_sources.clone();
    if let Some(sf) = state_file.as_mut() {
        sf.remove_legacy_data_source_rows(&data_sources);
    }
    let expansion_prelude = crate::wiring::DeferredForExpansionPrelude::new(
        canonical_resources,
        &state_file,
        wiring.schemas(),
    )
    .expect("fixture deferred-for prelude must resolve");

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
        let bindings = std::fs::read(&state_path)
            .ok()
            .and_then(|bytes| PublishedExports::from_state_bytes(&bytes).ok())
            .map(|exports| exports.build_remote_bindings())
            .unwrap_or_default();
        remote_bindings.insert(us.binding.clone(), bindings);
    }
    let resolved_provider_configs = carina_core::parser::resolve_provider_attributes_with_remote(
        parsed,
        &remote_bindings,
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("fixture provider configs must resolve");
    let raw_provider_configs = resolved_provider_configs.as_provider_configs();
    let expansion_prelude = expansion_prelude
        .detect_moves(
            wiring,
            &data_sources,
            raw_provider_configs,
            &state_block_claims,
        )
        .expect("fixture automatic moves must resolve");
    let sorted_resources = expansion_prelude.sorted_resources();

    let mut managed_current: HashMap<ResourceId, Current> = HashMap::new();
    let mut data_source_states: HashMap<ResourceId, State> = HashMap::new();
    let mut orphans = Orphans::new();
    if state_file.is_some() {
        for resource in sorted_resources {
            let (loaded_id, current) = expansion_prelude.build_current(&resource.id);
            managed_current.insert(loaded_id, current);
        }
        data_source_states.extend(
            data_sources
                .iter()
                .map(|source| (source.id.clone(), State::not_found(source.id.clone()))),
        );

        orphans = expansion_prelude
            .loaded_orphans(
                data_sources
                    .iter()
                    .map(|data_source| data_source.id.clone()),
            )
            .into_plan_orphans();
    } else {
        for resource in sorted_resources {
            managed_current.insert(resource.id.clone(), Current::absent(resource.id.clone()));
        }
        for ds in &data_sources {
            data_source_states.insert(ds.id.clone(), State::not_found(ds.id.clone()));
        }
    }
    let mut data_source_values =
        DataSourceValues::from_data_sources(&data_sources, |data_source| {
            DataSourceValue::read(
                data_source_states
                    .get(&data_source.id)
                    .cloned()
                    .expect("fixture initialized every data-source state"),
            )
        });

    let directives_map = state_file
        .as_ref()
        .map(|sf| sf.build_directives())
        .unwrap_or_default();

    // carina#3248: build unified pre-apply bindings (managed +
    // composition + data sources) so composition-rooted refs in the fixture
    // resolve through the composition layer to the managed sibling.
    let upstream_binding_names_owned: HashSet<String> = parsed
        .upstream_binding_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let upstream_binding_names: HashSet<&str> = upstream_binding_names_owned
        .iter()
        .map(String::as_str)
        .collect();
    let pre_apply_input_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        wiring.schemas(),
        sorted_resources,
    );
    let managed_stability = crate::wiring::derive_managed_binding_stability(
        parsed,
        sorted_resources,
        &managed_current,
        &data_source_values,
        wiring.schemas(),
        expansion_prelude.saved_attrs(),
        expansion_prelude.prev_explicit(),
        &remote_bindings,
        expansion_prelude.wait_aliases(),
    )
    .expect("fixture managed binding stability must resolve");
    let (mut override_aware_resources, unresolved_override_aware_resources) =
        OverrideAwareResources::build_pair_with_stability(
            canonical_resources,
            state_file.as_ref(),
            carina_core::binding_index::LivePreApplyInputs {
                managed: &[],
                compositions: &parsed.compositions,
                managed_current: &pre_apply_input_current,
                data_source_values: &data_source_values,
                remote_bindings: &remote_bindings,
                wait_aliases: expansion_prelude.wait_aliases(),
            },
            &managed_stability,
            OverrideAwareBuildMode::Plan {
                unresolved_upstream_bindings: &upstream_binding_names,
            },
        )
        .expect("Failed to build override-aware resources");
    crate::legacy_name_overrides::emit_legacy_override_warnings(
        &override_aware_resources,
        state_file.as_ref(),
    );

    // Resolve data-source input refs for the plan (carina#3181).
    let mut data_sources_for_plan = data_sources.clone();
    carina_core::resolver::resolve_data_source_refs_for_plan(
        &mut data_sources_for_plan,
        override_aware_resources.bindings(),
        &upstream_binding_names,
    )
    .expect("Failed to resolve data source refs with state");

    crate::wiring::lift_data_source_value_enum_leaves(
        &mut data_source_values,
        &data_sources,
        wiring.schemas(),
    );
    let constraint_origin_resources = override_aware_resources
        .paired_unresolved_resources_with_binding_sources(&unresolved_override_aware_resources);
    let prepared_compositions = parsed.compositions.clone();
    let module_gate =
        carina_core::executor::ModuleConstraintGate::from_live_compositions(&prepared_compositions);
    let wait_bindings = parsed.wait_bindings.clone();
    let mut provider_router = ProviderRouter::new();
    let preparation = {
        // Provider config validation may use `block_in_place` to bridge a
        // synchronous factory hook to async WASM calls. Keep both preparation
        // and normalizer creation inside a compatible runtime.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for plan preprocessing");
        for factory in wiring.factories() {
            let resolved_config = resolved_provider_configs.provider_or_implicit(factory.name());
            let config = rt
                .block_on(async {
                    carina_core::executor::prepare_provider_ready_config(
                        factory.as_ref(),
                        resolved_config.as_ref(),
                    )
                })
                .expect("fixture provider config unexpectedly failed validation");
            provider_router.add_normalizer(
                factory.name(),
                rt.block_on(factory.create_normalizer(None, &config))
                    .expect("fixture normalizer creation unexpectedly failed"),
            );
            provider_router.add_provider(
                factory.name().to_string(),
                Box::new(carina_provider_mock::MockProvider::new()),
            );
        }
        rt.block_on(async {
            PlanPreprocessor::new(&provider_router, wiring)
                .prepare(
                    &mut override_aware_resources,
                    &constraint_origin_resources,
                    &module_gate,
                    &mut managed_current,
                    &mut data_source_values,
                    &mut orphans,
                    raw_provider_configs,
                    &mut data_sources_for_plan,
                    &data_sources,
                )
                .await
        })
    };
    if let Err(error) = preparation {
        let errors = match error {
            PlanPreparationError::Plan(errors) => errors,
            PlanPreparationError::Provider(error) => {
                panic!("fixture no-op normalizer unexpectedly failed: {error}")
            }
        };
        let mut plan = Plan::new();
        for error in errors {
            plan.add_error(error);
        }
        managed_current = into_all_prior_states(orphans, managed_current);
        let prev_explicit = expansion_prelude.prev_explicit().clone();
        drop(expansion_prelude);
        return FixturePlan {
            plan,
            // Error plans are never rendered, so they intentionally do not
            // expose a partial resolved-resource view to fixture callers.
            desired_resources: Vec::new(),
            unresolved_resources: Vec::new(),
            compositions: parsed.compositions.clone(),
            managed_current,
            data_source_states: data_source_values.read_states_map(),
            state_file,
            schemas: wiring.schemas().clone(),
            moved_origins: HashMap::new(),
            deferred_for_expressions: parsed.deferred_for_expressions.clone(),
            export_params: parsed.export_params.clone(),
            resolved_export_params: carina_core::validation::check_export_values_pre_apply(
                &[],
                &carina_core::binding_index::ResolvedBindings::default(),
                &carina_core::parser::ProviderContext::default(),
            )
            .expect("empty exports are checked"),
            prev_explicit,
            expansion_trace: parsed.expansion_trace.clone(),
        };
    }
    let managed_bindings: HashSet<String> = expansion_prelude
        .sorted_resources()
        .iter()
        .filter_map(|resource| resource.binding.clone())
        .collect();
    let non_managed_deferred_for = parsed
        .deferred_for_expressions
        .iter()
        .filter(|deferred| {
            !deferred
                .root_bindings()
                .iter()
                .any(|binding| managed_bindings.contains(binding))
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut reconciled_expansion = expansion_prelude
        .bind_projected_phase1_state(managed_current, orphans, HashSet::new())
        .reconcile_addresses(parsed, crate::wiring::DeferredForWarningPolicy::Print)
        .expect("fixture move routes must match their loaded current states");
    let resolved_desired_ids = reconciled_expansion
        .reconcile_late_anonymous_identities(
            wiring,
            &mut override_aware_resources,
            &data_sources_for_plan,
            state_file.as_ref(),
            &state_block_claims,
            raw_provider_configs,
        )
        .expect("fixture late anonymous identities must resolve uniquely");
    crate::wiring::validate_plan_time_state_block_collisions(
        &resolved_desired_ids,
        reconciled_expansion.moved_pairs(),
        &resolved_state_block_targets,
        &state_file,
    )
    .expect("state block collision validation failed");

    let reconciled_provider_states = provider_states_for(
        reconciled_expansion.managed_current(),
        reconciled_expansion.orphans(),
        override_aware_resources.resources(),
    );
    let reconciled_input_current = carina_core::resource::into_plan_input_current_map(
        reconciled_provider_states,
        wiring.schemas(),
        override_aware_resources.resources(),
    );
    let stability_resources = carina_core::resource::ResolvedResource::collect_resolved(
        override_aware_resources.resources().iter().cloned(),
    )
    .expect("fixture managed resources must have identities");
    let managed_stability = carina_core::differ::managed_binding_stability(
        &stability_resources,
        &reconciled_input_current,
        wiring.schemas(),
        reconciled_expansion.saved_attrs(),
        reconciled_expansion.prev_explicit(),
    );
    override_aware_resources.rebuild_bindings_after_identity_reconciliation_with_stability(
        RebuildBindingsInputs {
            compositions: &parsed.compositions,
            managed_current: &reconciled_input_current,
            data_source_values: &data_source_values,
            remote_bindings: &remote_bindings,
            wait_aliases: reconciled_expansion.wait_aliases(),
        },
        &managed_stability,
    );

    let prepared_expansion = reconciled_expansion.seal(data_source_values);
    let (validated_expansion, deferred_for_expansion) = validated_config
        .expand_deferred_for(
            &prepared_expansion,
            &remote_bindings,
            crate::wiring::DeferredForWarningPolicy::Print,
        )
        .expect("fixture deferred-for expansion failed");
    let crate::wiring::PreparedDeferredForExpansionParts {
        addressed,
        data_source_values,
        wait_aliases: _,
    } = prepared_expansion.into_parts();
    let crate::wiring::AddressedStateParts {
        mut managed_current,
        orphans,
        saved_attrs,
        prev_explicit,
        moved_pairs,
    } = addressed.into_parts();
    let (wiring, expanded_canonical_resources) = validated_expansion.context_and_resources();
    let parsed = expanded_canonical_resources.parsed();
    let deferred_create_targets: Vec<_> = deferred_for_expansion
        .deferred_create_targets
        .into_iter()
        .filter(|target| {
            target
                .template
                .root_bindings()
                .iter()
                .any(|binding| managed_bindings.contains(binding))
        })
        .collect();
    let mut residual_deferred_for = deferred_for_expansion.residual_deferred_for;
    residual_deferred_for.extend(non_managed_deferred_for);
    let image_compositions = parsed.compositions.clone();
    let resolved_resources =
        ResolvedResource::collect_resolved(override_aware_resources.resources().iter().cloned())
            .expect("fixture resources must have assigned identities");
    let resolved_unresolved_resources =
        ResolvedResource::collect_resolved(override_aware_resources.paired_unresolved_resources())
            .expect("fixture unresolved resources must have paired identities");
    let plan_input_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        wiring.schemas(),
        override_aware_resources.resources(),
    );
    let mut plan = create_plan_with_cascades(
        &resolved_resources,
        &resolved_unresolved_resources,
        &data_sources_for_plan,
        &provider_router,
        PlanPriorStates::new(&plan_input_current, &orphans),
        &directives_map,
        wiring.schemas(),
        &saved_attrs,
        &prev_explicit,
        &wait_bindings,
    )
    .expect("fixture providers must return valid satisfier hints");
    crate::wiring::add_deposed_delete_effects(&mut plan, &state_file, &moved_pairs);
    override_aware_resources.apply_plan_stability(&plan_input_current, &plan, wiring.schemas());

    // Keep this after both identifier-error producers. The
    // `removed_missing_identifier` fixture pins the fixture planner's copy of
    // the same recovery ordering used by plan and live apply.
    crate::wiring::add_state_block_effects(
        &mut plan,
        &parsed.state_blocks,
        &state_file,
        &moved_pairs,
        override_aware_resources.resources(),
        override_aware_resources.bindings(),
        &upstream_binding_names,
    );
    add_deferred_create_effects(&mut plan, &deferred_create_targets);
    managed_current = into_all_prior_states(orphans, managed_current);

    let moved_origins: HashMap<ResourceId, ResourceId> = moved_pairs
        .iter()
        .map(|(from, to)| (to.as_inner().clone(), from.as_inner().clone()))
        .collect();
    let export_wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();
    let provider_context = carina_core::parser::ProviderContext::default();
    let resolved_export_params = crate::commands::plan::check_exports_pre_apply(
        crate::commands::plan::CheckExportsPreApplyInput {
            export_params: &parsed.export_params,
            resources: override_aware_resources.resources(),
            compositions: &parsed.compositions,
            managed_current: &managed_current,
            data_source_values: &data_source_values,
            state_file: state_file.as_ref(),
            plan: &plan,
            schemas: wiring.schemas(),
            wait_aliases: &export_wait_aliases,
            provider_context: &provider_context,
        },
    )
    .expect("fixture exports must resolve without secrets");

    let unresolved_resources = resolved_unresolved_resources
        .into_iter()
        .map(ResolvedResource::into_inner)
        .map(UnresolvedResource::from_pre_resolve)
        .collect();
    FixturePlan {
        plan,
        desired_resources: resolved_resources,
        unresolved_resources,
        compositions: image_compositions,
        managed_current,
        data_source_states: data_source_values.into_read_states(),
        state_file,
        schemas: wiring.schemas().clone(),
        moved_origins,
        deferred_for_expressions: residual_deferred_for,
        export_params: parsed.export_params.clone(),
        resolved_export_params,
        prev_explicit,
        expansion_trace: parsed.expansion_trace.clone(),
    }
}

fn fixture_provider_factories(fixture_path: &Path) -> Vec<Box<dyn ProviderFactory>> {
    let mut factories: Vec<Box<dyn ProviderFactory>> =
        match fixture_path.file_name().and_then(|name| name.to_str()) {
            Some("dynamic_enum_az_no_diff") => vec![Box::new(DynamicEnumFixtureFactory)],
            Some("enum_display") => vec![Box::new(EnumDisplayFixtureFactory)],
            Some("list_diff_best_first_mid_insert") => {
                vec![Box::new(BestFirstPairingFixtureFactory)]
            }
            Some("moved_claims_precede_heuristics") => {
                vec![Box::new(MovedClaimsPrecedeHeuristicsFixtureFactory)]
            }
            Some("plan_image_anonymous") => vec![Box::new(PlanImageFixtureFactory)],
            Some("plan_image_module_cascade" | "plan_image_reference_layout") => {
                vec![Box::new(PlanImageFixtureFactory)]
            }
            Some("anonymous_create_only_rekey")
            | Some("replace_create_only")
            | Some("replace_with_non_forcing_diffs") => {
                vec![Box::new(ReplaceCreateOnlyFixtureFactory)]
            }
            Some("route53_hosted_zone_name_strip_suffix_no_diff") => {
                vec![Box::new(Route53HostedZoneFixtureFactory)]
            }
            Some("resolved_value_constraint_multifile") => {
                vec![Box::new(ResolvedValueConstraintFixtureFactory)]
            }
            _ => vec![],
        };

    // Most display fixtures intentionally omit real provider artifacts. Their
    // provider blocks still use typed Region enum syntax, so model that small
    // piece of provider metadata instead of allowing an unresolved identifier
    // through the harness. A fixture-specific factory wins when present.
    for provider in ["aws", "awscc"] {
        if !factories.iter().any(|factory| factory.name() == provider) {
            factories.push(Box::new(FixtureProviderConfigFactory { provider }));
        }
    }
    factories
}

fn fixture_provider_config_factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![
        Box::new(FixtureProviderConfigFactory { provider: "aws" }),
        Box::new(FixtureProviderConfigFactory { provider: "awscc" }),
    ]
}

fn fixture_validation_enum_schemas(fixture_path: &Path) -> Vec<(String, ValidatedResourceSchema)> {
    let schemas = match fixture_path.file_name().and_then(|name| name.to_str()) {
        Some("dynamic_enum_az_no_diff") => vec![(
            "fixture".to_string(),
            ResourceSchema::new("network.Subnet").attribute(
                AttributeSchema::required("availability_zone", fixture_zone_name_type()).build(),
            ),
        )],
        Some("enum_display" | "no_changes_enum") => vec![(
            "awscc".to_string(),
            ResourceSchema::new("ec2.Vpc")
                .attribute(
                    AttributeSchema::required("cidr_block", AttributeType::string())
                        .create_only()
                        .build(),
                )
                .attribute(
                    AttributeSchema::required("instance_tenancy", fixture_instance_tenancy_type())
                        .build(),
                ),
        )],
        _ => Vec::new(),
    };
    schemas
        .into_iter()
        .map(|(provider, schema)| {
            (
                provider,
                schema
                    .validate()
                    .expect("fixture validation schema must be valid"),
            )
        })
        .collect()
}

fn fixture_region_attribute_types(provider: &str) -> carina_core::schema::ValidatedConfigSchema {
    carina_core::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
        "region".to_string(),
        AttributeType::enum_(
            TypeIdentity::new(Some(provider), Vec::<String>::new(), "Region"),
            None,
            vec![],
            None,
            Some(DslTransform::HyphenToUnderscore),
        ),
    )]))
    .expect("fixture provider config schema must be valid")
}

fn fixture_instance_tenancy_type() -> AttributeType {
    AttributeType::enum_(
        TypeIdentity::new(Some("awscc"), ["ec2", "Vpc"], "InstanceTenancy"),
        Some(vec![
            "default".to_string(),
            "dedicated".to_string(),
            "host".to_string(),
        ]),
        vec![],
        None,
        None,
    )
}

fn fixture_zone_name_type() -> AttributeType {
    AttributeType::enum_(
        TypeIdentity::new(
            Some("fixture"),
            vec!["network".to_string(), "Subnet".to_string()],
            "ZoneName",
        ),
        None,
        vec![],
        None,
        Some(DslTransform::HyphenToUnderscore),
    )
}

struct FixtureProviderConfigFactory {
    provider: &'static str,
}

impl ProviderFactory for FixtureProviderConfigFactory {
    fn name(&self) -> &str {
        self.provider
    }

    fn display_name(&self) -> &str {
        "Fixture provider config schema"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        fixture_region_attribute_types(self.provider)
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = Vec::new();
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

struct ResolvedValueConstraintFixtureFactory;

impl ProviderFactory for ResolvedValueConstraintFixtureFactory {
    fn name(&self) -> &str {
        "fixture"
    }

    fn display_name(&self) -> &str {
        "Resolved value constraint fixture provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "test".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            let constrained =
                AttributeType::refined_string(None, Some("^good-".to_string()), None, None);
            vec![
                ResourceSchema::new("test.Source")
                    .attribute(AttributeSchema::required("value", AttributeType::string()).build()),
                ResourceSchema::new("test.Consumer")
                    .attribute(AttributeSchema::required("target", constrained).build()),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

struct BestFirstPairingFixtureFactory;

impl ProviderFactory for BestFirstPairingFixtureFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWS Cloud Control fixture provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        fixture_region_attribute_types(self.name())
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            let statement = AttributeType::list(AttributeType::struct_(
                "Statement",
                vec![
                    StructField::optional("sid", AttributeType::string()).build(),
                    StructField::optional("effect", AttributeType::string()).build(),
                    StructField::optional("action", AttributeType::list(AttributeType::string()))
                        .build(),
                    StructField::optional("resource", AttributeType::list(AttributeType::string()))
                        .build(),
                ],
            ));
            vec![
                ResourceSchema::new("iam.RolePolicy")
                    .attribute(
                        AttributeSchema::required("policy_name", AttributeType::string())
                            .create_only()
                            .build(),
                    )
                    .attribute(
                        AttributeSchema::required("role_name", AttributeType::string())
                            .create_only()
                            .build(),
                    )
                    .attribute(AttributeSchema::required("statement", statement).build()),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
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

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> =
            vec![replace_create_only_widget_schema()];
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

fn replace_create_only_widget_schema() -> ResourceSchema {
    ResourceSchema::new("test.Widget")
        .attribute(
            AttributeSchema::optional("external_name", AttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            AttributeSchema::optional("legacy_token", AttributeType::string())
                .create_only()
                .removable()
                .build(),
        )
        .attribute(AttributeSchema::optional("description", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("metric_name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("scope", AttributeType::string()).build())
}

struct PlanImageFixtureFactory;

impl ProviderFactory for PlanImageFixtureFactory {
    fn name(&self) -> &str {
        "test"
    }

    fn display_name(&self) -> &str {
        "Plan image fixture provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "test-region".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![
                replace_create_only_widget_schema()
                    .attribute(
                        AttributeSchema::provider_populated("output_id", AttributeType::string())
                            .build(),
                    )
                    .with_unique_name_attribute("external_name"),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
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

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        fixture_region_attribute_types(self.name())
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![
                ResourceSchema::new("test.Widget").attribute(
                    AttributeSchema::optional("external_name", AttributeType::string())
                        .create_only()
                        .build(),
                ),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
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

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        fixture_region_attribute_types(self.name())
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![
                ResourceSchema::new("ec2.Vpc")
                    .attribute(
                        AttributeSchema::required("cidr_block", AttributeType::string())
                            .create_only()
                            .build(),
                    )
                    .attribute(
                        AttributeSchema::required(
                            "instance_tenancy",
                            fixture_instance_tenancy_type(),
                        )
                        .build(),
                    ),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
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

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "test".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![ResourceSchema::new("network.Subnet").attribute(
                AttributeSchema::required("availability_zone", fixture_zone_name_type()).build(),
            )]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
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

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &indexmap::IndexMap<String, Value>) -> String {
        "test".to_string()
    }

    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
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
                    .attribute(AttributeSchema::required("name", zone_name).build()),
            ]
        };
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

/// Collect `Delete`-effect attributes keyed by resource id plus generation.
pub fn delete_instance_attributes_from_plan(
    plan: &Plan,
    managed_current: &HashMap<ResourceId, Current>,
    state_file: Option<&StateFile>,
) -> carina_core::detail_rows::HistoricalDisplayAttributes {
    crate::commands::plan::collect_historical_display_attributes(plan, managed_current, state_file)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn providerless_fixture_plan_canonicalizes_block_names_in_both_snapshots() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(
            temp.path().join("main.crn"),
            r#"provider mock {}

let item = mock.test.resource {
  name = "fixture-block"
  rule { action = "allow" }
}
"#,
        )
        .expect("write fixture");

        let fixture = build_plan_from_fixture_path(temp.path());
        let desired = fixture
            .desired_resources
            .iter()
            .find(|resource| resource.id.resource_type == "test.resource")
            .expect("desired fixture resource")
            .as_resource();
        assert!(desired.attributes.contains_key("rules"));
        assert!(!desired.attributes.contains_key("rule"));

        let unresolved = fixture
            .unresolved_resources
            .iter()
            .find(|resource| resource.as_resource().id.resource_type == "test.resource")
            .expect("unresolved fixture resource")
            .as_resource();
        assert!(unresolved.attributes.contains_key("rules"));
        assert!(!unresolved.attributes.contains_key("rule"));
    }
}
