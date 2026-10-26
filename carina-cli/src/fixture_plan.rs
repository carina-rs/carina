//! Build plan output from `tests/fixtures/plan_display` directories.
//!
//! Shared between the plan snapshot tests and the `plan-fixture` example
//! binary. The logic mirrors `--refresh=false` but skips provider plugin
//! loading so fixture rendering works without installed providers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use carina_core::config_loader::{get_base_dir, load_configuration};
use carina_core::deps::sort_resources_by_dependencies;
use carina_core::differ::{
    Orphans, PlanPriorStates, create_plan_with_cascades, into_all_prior_states, provider_states_for,
};
use carina_core::override_aware::{OverrideAwareResources, RebuildBindingsInputs};
use carina_core::plan::Plan;
use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderResult, ProviderRouter};
use carina_core::resource::{Current, ResourceId, State, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, DslTransform, ResourceSchema, SchemaRegistry, StructField,
    TypeIdentity,
};
use carina_state::{PublishedExports, StateFile, check_and_migrate};

use crate::commands::{collapse_errors, validate_and_resolve_errors_with_context};
use crate::wiring::{
    LateAnonymousIdentityInputs, PlanPreparationError, PlanPreprocessor, WiringContext,
    add_deferred_create_effects, compute_anonymous_identifiers_with_ctx,
    expand_same_config_deferred_for, reconcile_anonymous_identifiers_with_ctx,
    reconcile_late_anonymous_identities, reconcile_prefixed_names,
};

/// Fixture root path relative to the `carina-cli` crate manifest.
const FIXTURE_SUBPATH: &str = "tests/fixtures/plan_display";

/// Minimal schema catalog for provider-less plan fixtures.
///
/// These schemas intentionally model the immutable inputs needed for anonymous
/// identity plus the schema-sensitive fields used by fixture diffing. Richer
/// provider-specific formatting/validation fixtures add local schemas below,
/// which take precedence over matching entries in this catalog.
fn fixture_schemas() -> Vec<(String, ResourceSchema)> {
    let create_only_string =
        |name| AttributeSchema::new(name, AttributeType::string()).create_only();
    let aws = |schema| ("aws".to_string(), schema);
    let awscc = |schema| ("awscc".to_string(), schema);
    vec![
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
                .attribute(AttributeSchema::new(
                    "instance_tenancy",
                    fixture_instance_tenancy_type(),
                ))
                .attribute(AttributeSchema::new(
                    "enable_dns_support",
                    AttributeType::bool(),
                ))
                .attribute(AttributeSchema::new(
                    "tags",
                    AttributeType::map(AttributeType::string()),
                )),
        ),
        awscc(ResourceSchema::new("iam.Role").attribute(create_only_string("role_name"))),
        awscc(
            ResourceSchema::new("issue3885.Role")
                .attribute(create_only_string("role_name"))
                .attribute(AttributeSchema::new("arn", AttributeType::string()).read_only()),
        ),
        awscc(
            ResourceSchema::new("iam.RolePolicy")
                .attribute(create_only_string("policy_name"))
                .attribute(create_only_string("role_name"))
                .attribute(AttributeSchema::new(
                    "policy_document",
                    AttributeType::string(),
                )),
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
    ]
}

/// Complete output of fixture-based plan construction.
pub struct FixturePlan {
    pub plan: Plan,
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

    let loaded = load_configuration(&fixture_pathbuf).unwrap();
    let inference_errors = loaded.inference_errors;
    let duplicate_declarations = loaded.duplicate_declarations;
    let unresolved_resources = loaded.unresolved_parsed.resources;
    let mut parsed = loaded.parsed;
    let base_dir = get_base_dir(&fixture_pathbuf);
    // Fixture planning deliberately skips loading provider artifacts. Supply
    // only the enum metadata validation needs here: the full fixture schema
    // catalog would assign anonymous identities earlier than the real plan
    // path and make the harness observe the wrong inputs.
    let validation_wiring = WiringContext::new_with_additional_schemas(
        fixture_provider_config_factories(),
        &parsed.providers,
        fixture_validation_enum_schemas(&fixture_pathbuf),
    );
    let validation_errors = validate_and_resolve_errors_with_context(
        &mut parsed,
        base_dir,
        true,
        &validation_wiring,
        HashMap::new(),
        &inference_errors,
        &duplicate_declarations,
    );
    if !validation_errors.is_empty() {
        let error = collapse_errors(validation_errors);
        panic!("fixture validation failed: {error}");
    }
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

    let wiring = WiringContext::new_with_additional_schemas(
        fixture_provider_factories(&fixture_pathbuf),
        &parsed.providers,
        fixture_schemas(),
    );

    if fixture_pathbuf
        .file_name()
        .is_some_and(|name| name == "moved_claims_precede_heuristics")
    {
        // This fixture writes moved.to as the literal schema-derived anonymous
        // hash name, so compute the desired anonymous id before claims resolve.
        let canonical_resources = carina_core::value::canonicalize_resources_with_schemas(
            &mut parsed.resources,
            wiring.schemas(),
        )
        .unwrap();
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
            &mut parsed.compositions,
            &|provider, resource_type| {
                sf.resources_by_type(provider, resource_type)
                    .into_iter()
                    .map(|r| r.identity.to_string())
                    .collect()
            },
            &state_block_claims,
        );
    }
    if let Some(sf) = state_file.as_mut() {
        reconcile_anonymous_identifiers_with_ctx(
            &wiring,
            &mut parsed.resources,
            &parsed.data_sources,
            sf,
            &state_block_claims,
        )
        .expect("fixture state identities must remain unique");
    }

    // carina#3181: `parsed.resources` is managed-only; data sources live
    // in `parsed.data_sources`. Only managed resources are dependency-
    // sorted.
    let sorted_resources = sort_resources_by_dependencies(&parsed.resources).unwrap();
    let data_sources: Vec<carina_core::resource::DataSource> = parsed.data_sources.clone();
    if let Some(sf) = state_file.as_mut() {
        sf.remove_legacy_data_source_rows(&data_sources);
    }

    let mut managed_current: HashMap<ResourceId, Current> = HashMap::new();
    let mut data_source_states: HashMap<ResourceId, State> = HashMap::new();
    let mut orphans = Orphans::new();
    if let Some(sf) = state_file.as_ref() {
        for resource in &sorted_resources {
            let current = sf.build_state_for_resource(&resource.id);
            managed_current.insert(resource.id.clone(), current);
        }
        data_source_states.extend(
            data_sources
                .iter()
                .map(|source| (source.id.clone(), State::not_found(source.id.clone()))),
        );

        let desired_ids: HashSet<ResourceId> = sorted_resources
            .iter()
            .map(|r| r.id.clone())
            .chain(data_sources.iter().map(|d| d.id.clone()))
            .collect();
        orphans = sf.build_orphans(&desired_ids).into_plan_orphans();
    } else {
        for resource in &sorted_resources {
            managed_current.insert(resource.id.clone(), Current::absent(resource.id.clone()));
        }
        for ds in &data_sources {
            data_source_states.insert(ds.id.clone(), State::not_found(ds.id.clone()));
        }
    }

    let directives_map = state_file
        .as_ref()
        .map(|sf| sf.build_directives())
        .unwrap_or_default();

    // Keep fixture plans aligned with the real CLI plan/apply wiring:
    // state-file attributes are lifted before the differ sees them, so
    // fixture coverage exercises the enum-state lift seam instead of
    // relying on differ cross-shape tolerance.
    let mut saved_attrs = state_file
        .as_ref()
        .map(|sf| sf.build_saved_attrs())
        .unwrap_or_default()
        .lift(wiring.schemas());

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
        let bindings = std::fs::read(&state_path)
            .ok()
            .and_then(|bytes| PublishedExports::from_state_bytes(&bytes).ok())
            .map(|exports| exports.build_remote_bindings())
            .unwrap_or_default();
        remote_bindings.insert(us.binding.clone(), bindings);
    }
    let resolved_provider_configs = carina_core::parser::resolve_provider_attributes_with_remote(
        &parsed,
        &remote_bindings,
        &carina_core::parser::ProviderContext::default(),
    )
    .expect("fixture provider configs must resolve");
    let raw_provider_configs = resolved_provider_configs.as_provider_configs();

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
    let pre_apply_input_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        wiring.schemas(),
        &sorted_resources,
    );
    let mut override_aware_resources = OverrideAwareResources::build_for_plan(
        sorted_resources.clone(),
        state_file.as_ref(),
        carina_core::binding_index::PreApplyInputs {
            managed: &[],
            compositions: &parsed.compositions,
            data_sources: &data_sources,
            managed_current: &pre_apply_input_current,
            data_source_states: &data_source_states,
            remote_bindings: &remote_bindings,
            wait_aliases: &wait_aliases,
        },
        &upstream_binding_names,
    )
    .expect("Failed to build override-aware resources");
    crate::legacy_name_overrides::emit_legacy_override_warnings(
        &override_aware_resources,
        state_file.as_ref(),
    );
    let unresolved_override_aware_resources = OverrideAwareResources::build_for_plan(
        unresolved_resources,
        state_file.as_ref(),
        carina_core::binding_index::PreApplyInputs {
            managed: &[],
            compositions: &parsed.compositions,
            data_sources: &data_sources,
            managed_current: &pre_apply_input_current,
            data_source_states: &data_source_states,
            remote_bindings: &remote_bindings,
            wait_aliases: &wait_aliases,
        },
        &upstream_binding_names,
    )
    .expect("Failed to build unresolved override-aware resources");

    // Resolve data-source input refs for the plan (carina#3181).
    let mut data_sources_for_plan = data_sources.clone();
    carina_core::resolver::resolve_data_source_refs_for_plan(
        &mut data_sources_for_plan,
        override_aware_resources.bindings(),
        &upstream_binding_names,
    )
    .expect("Failed to resolve data source refs with state");

    carina_core::utils::lift_current_state_enum_leaves_for_data_sources(
        &mut data_source_states,
        &data_sources,
        wiring.schemas(),
    );
    let constraint_origin_resources = override_aware_resources
        .paired_unresolved_resources_with_binding_sources(&unresolved_override_aware_resources);
    let prepared_compositions = parsed.compositions.clone();
    let module_gate = carina_core::executor::ModuleConstraintGate::new(&prepared_compositions);
    let wait_bindings = parsed.wait_bindings.clone();
    let mut provider_router = ProviderRouter::new();
    let preparation = {
        // Provider config validation may use `block_in_place` to bridge a
        // synchronous factory hook to async WASM calls. Keep both preparation
        // and normalizer creation inside a compatible runtime.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
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
            PlanPreprocessor::new(&provider_router, &wiring)
                .prepare(
                    &mut override_aware_resources,
                    &constraint_origin_resources,
                    &module_gate,
                    &mut managed_current,
                    &mut data_source_states,
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
        return FixturePlan {
            plan,
            managed_current,
            data_source_states,
            state_file,
            schemas: wiring.schemas().clone(),
            moved_origins: HashMap::new(),
            deferred_for_expressions: parsed.deferred_for_expressions,
            export_params: parsed.export_params,
            resolved_export_params: carina_core::validation::check_export_values_pre_apply(
                &[],
                &carina_core::binding_index::ResolvedBindings::default(),
                &carina_core::parser::ProviderContext::default(),
            )
            .expect("empty exports are checked"),
            prev_explicit,
            expansion_trace: parsed.expansion_trace,
        };
    }
    let resolved_desired_ids = reconcile_late_anonymous_identities(
        &wiring,
        LateAnonymousIdentityInputs {
            resources: &mut override_aware_resources,
            data_sources: &data_sources_for_plan,
            state_file: state_file.as_ref(),
            state_block_claims: &state_block_claims,
            providers: raw_provider_configs,
        },
    )
    .expect("fixture late anonymous identities must resolve uniquely");

    let moved_pairs = crate::wiring::materialize_moved_states(
        &mut managed_current,
        &mut orphans,
        &mut prev_explicit,
        &mut saved_attrs,
        &parsed.state_blocks,
        &state_file,
    )
    .expect("fixture move routes must match their loaded current states");
    crate::wiring::validate_plan_time_state_block_collisions(
        &resolved_desired_ids,
        &moved_pairs,
        &resolved_state_block_targets,
        &state_file,
    )
    .expect("state block collision validation failed");

    let reconciled_provider_states = provider_states_for(
        &managed_current,
        &orphans,
        override_aware_resources.resources(),
    );
    let reconciled_input_current = carina_core::resource::into_plan_input_current_map(
        reconciled_provider_states,
        wiring.schemas(),
        override_aware_resources.resources(),
    );
    override_aware_resources.rebuild_bindings_after_identity_reconciliation(
        RebuildBindingsInputs {
            compositions: &parsed.compositions,
            data_sources: &data_sources,
            managed_current: &reconciled_input_current,
            data_source_states: &data_source_states,
            remote_bindings: &remote_bindings,
            wait_aliases: &wait_aliases,
        },
    );

    let deferred_for_expansion = expand_same_config_deferred_for(
        &parsed,
        &sorted_resources,
        &managed_current,
        &data_source_states,
        wiring.schemas(),
        &remote_bindings,
        &wait_aliases,
        &HashSet::new(),
        &HashSet::new(),
    )
    .expect("fixture deferred-for expansion failed");
    let managed_bindings: HashSet<String> = sorted_resources
        .iter()
        .filter_map(|resource| resource.binding.clone())
        .collect();
    let deferred_create_targets: Vec<_> = deferred_for_expansion
        .deferred_create_targets
        .into_iter()
        .filter(|target| managed_bindings.contains(&target.upstream_binding))
        .collect();
    let mut residual_deferred_for = deferred_for_expansion.residual_deferred_for;
    residual_deferred_for.extend(
        parsed
            .deferred_for_expressions
            .iter()
            .filter(|deferred| !managed_bindings.contains(&deferred.iterable_binding))
            .cloned(),
    );
    let resolved_resources = carina_core::resource::ResolvedResource::collect_resolved(
        override_aware_resources.resources().iter().cloned(),
    )
    .expect("fixture resources must have assigned identities");
    let unresolved_resources = carina_core::resource::ResolvedResource::collect_resolved(
        override_aware_resources.paired_unresolved_resources(),
    )
    .expect("fixture unresolved resources must have paired identities");
    let plan_input_current = carina_core::resource::into_plan_input_current_map(
        managed_current.clone(),
        wiring.schemas(),
        override_aware_resources.resources(),
    );
    let mut plan = create_plan_with_cascades(
        &resolved_resources,
        &unresolved_resources,
        &data_sources_for_plan,
        &provider_router,
        PlanPriorStates::new(&plan_input_current, &data_source_states, &orphans),
        &directives_map,
        wiring.schemas(),
        &saved_attrs,
        &prev_explicit,
        &wait_bindings,
    )
    .expect("fixture providers must return valid satisfier hints");
    crate::wiring::add_deposed_delete_effects(&mut plan, &state_file, &moved_pairs);

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
            data_sources: &data_sources_for_plan,
            managed_current: &managed_current,
            data_source_states: &data_source_states,
            state_file: state_file.as_ref(),
            plan: &plan,
            schemas: wiring.schemas(),
            wait_aliases: &export_wait_aliases,
            provider_context: &provider_context,
        },
    )
    .expect("fixture exports must resolve without secrets");

    FixturePlan {
        plan,
        managed_current,
        data_source_states,
        state_file,
        schemas: wiring.schemas().clone(),
        moved_origins,
        deferred_for_expressions: residual_deferred_for,
        export_params: parsed.export_params,
        resolved_export_params,
        prev_explicit,
        expansion_trace: parsed.expansion_trace,
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
            Some("replace_create_only") | Some("replace_with_non_forcing_diffs") => {
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

fn fixture_validation_enum_schemas(fixture_path: &Path) -> Vec<(String, ResourceSchema)> {
    match fixture_path.file_name().and_then(|name| name.to_str()) {
        Some("dynamic_enum_az_no_diff") => vec![(
            "fixture".to_string(),
            ResourceSchema::new("network.Subnet").attribute(
                AttributeSchema::new("availability_zone", fixture_zone_name_type()).required(),
            ),
        )],
        Some("enum_display" | "no_changes_enum") => vec![(
            "awscc".to_string(),
            ResourceSchema::new("ec2.Vpc")
                .attribute(
                    AttributeSchema::new("cidr_block", AttributeType::string())
                        .required()
                        .create_only(),
                )
                .attribute(
                    AttributeSchema::new("instance_tenancy", fixture_instance_tenancy_type())
                        .required(),
                ),
        )],
        _ => Vec::new(),
    }
}

fn fixture_region_attribute_types(provider: &str) -> HashMap<String, AttributeType> {
    HashMap::from([(
        "region".to_string(),
        AttributeType::enum_(
            TypeIdentity::new(Some(provider), Vec::<String>::new(), "Region"),
            None,
            vec![],
            None,
            Some(DslTransform::HyphenToUnderscore),
        ),
    )])
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

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        fixture_region_attribute_types(self.provider)
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
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

    fn schemas(&self) -> Vec<ResourceSchema> {
        Vec::new()
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

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
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

    fn schemas(&self) -> Vec<ResourceSchema> {
        let constrained =
            AttributeType::refined_string(None, Some("^good-".to_string()), None, None);
        vec![
            ResourceSchema::new("test.Source")
                .attribute(AttributeSchema::new("value", AttributeType::string()).required()),
            ResourceSchema::new("test.Consumer")
                .attribute(AttributeSchema::new("target", constrained).required()),
        ]
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

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        fixture_region_attribute_types(self.name())
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
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

    fn schemas(&self) -> Vec<ResourceSchema> {
        let statement = AttributeType::list(AttributeType::struct_(
            "Statement",
            vec![
                StructField::new("sid", AttributeType::string()),
                StructField::new("effect", AttributeType::string()),
                StructField::new("action", AttributeType::list(AttributeType::string())),
                StructField::new("resource", AttributeType::list(AttributeType::string())),
            ],
        ));
        vec![
            ResourceSchema::new("iam.RolePolicy")
                .attribute(
                    AttributeSchema::new("policy_name", AttributeType::string())
                        .required()
                        .create_only(),
                )
                .attribute(
                    AttributeSchema::new("role_name", AttributeType::string())
                        .required()
                        .create_only(),
                )
                .attribute(AttributeSchema::new("statement", statement).required()),
        ]
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
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
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
                )
                .attribute(AttributeSchema::new("description", AttributeType::string()))
                .attribute(AttributeSchema::new("metric_name", AttributeType::string()))
                .attribute(AttributeSchema::new("scope", AttributeType::string())),
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
        fixture_region_attribute_types(self.name())
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
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
        fixture_region_attribute_types(self.name())
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
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

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![
            ResourceSchema::new("ec2.Vpc")
                .attribute(
                    AttributeSchema::new("cidr_block", AttributeType::string())
                        .required()
                        .create_only(),
                )
                .attribute(
                    AttributeSchema::new("instance_tenancy", fixture_instance_tenancy_type())
                        .required(),
                ),
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
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
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

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![ResourceSchema::new("network.Subnet").attribute(
            AttributeSchema::new("availability_zone", fixture_zone_name_type()).required(),
        )]
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
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> Result<(), String> {
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

/// Collect `Delete`-effect attributes keyed by resource id plus generation.
pub fn delete_instance_attributes_from_plan(
    plan: &Plan,
    managed_current: &HashMap<ResourceId, Current>,
    state_file: Option<&StateFile>,
) -> carina_core::detail_rows::HistoricalDisplayAttributes {
    crate::commands::plan::collect_historical_display_attributes(plan, managed_current, state_file)
}
