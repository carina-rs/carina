//! Build plan output from `tests/fixtures/plan_display` directories.
//!
//! Shared between the plan snapshot tests and the `plan-fixture` example
//! binary. It builds from fixture state without loading provider plugins or
//! performing live provider reads, so rendering works without installed
//! providers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use carina_core::config_loader::{get_base_dir, load_configuration};
use carina_core::data_source_value::{DataSourceValue, DataSourceValues};
use carina_core::differ::Orphans;
use carina_core::executor::UnresolvedResource;
use carina_core::plan::Plan;
use carina_core::provider::{
    BoxFuture, InitializedProvider, ProviderFactory, ProviderInitialization, ProviderInstanceKey,
    ProviderResult, ProviderRouter,
};
use carina_core::resource::{Current, ResolvedResource, ResourceId, State, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, DslTransform, ResourceSchema, SchemaRegistry, StructField,
    TypeIdentity, ValidatedResourceSchema,
};
use carina_state::{PublishedExports, StateFile, check_and_migrate};

use crate::commands::plan::compute_export_diffs;
use crate::commands::shared::state_writeback::persisted_directive_changes;
use crate::moved_hint::{MovedBlockHints, detect_moved_block_hints};
use crate::plan_summary::PlanChangeSummary;
use crate::wiring::{LoadedProviderFactory, ProviderLoad, ValidatedConfig};

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
    let identity_string = |name| {
        AttributeSchema::optional(name, AttributeType::string())
            .create_only()
            .identity()
            .build()
    };
    let aws = |schema| ("aws".to_string(), schema);
    let awscc = |schema| ("awscc".to_string(), schema);
    let schemas = vec![
        aws(ResourceSchema::new("acm.Certificate")
            .attribute(create_only_string("domain_name"))
            .attribute(
                AttributeSchema::provider_populated(
                    "domain_validation_options",
                    AttributeType::list(AttributeType::struct_(
                        "DomainValidation",
                        vec![
                            StructField::provider_populated("domain_name", AttributeType::string())
                                .build(),
                            StructField::provider_populated(
                                "resource_record",
                                AttributeType::struct_(
                                    "ResourceRecord",
                                    vec![
                                        StructField::provider_populated(
                                            "name",
                                            AttributeType::string(),
                                        )
                                        .build(),
                                        // The real provider uses a string enum here. The
                                        // provider-less fixture catalog keeps the nested
                                        // value as a string; RecordSet.type below carries
                                        // the enum needed at the create-only match seam.
                                        StructField::provider_populated(
                                            "type",
                                            AttributeType::string(),
                                        )
                                        .build(),
                                        StructField::provider_populated(
                                            "value",
                                            AttributeType::string(),
                                        )
                                        .build(),
                                    ],
                                ),
                            )
                            .build(),
                        ],
                    )),
                )
                .build(),
            )),
        aws(ResourceSchema::new("acm.CertificateValidation")
            .attribute(create_only_string("certificate_arn"))
            .attribute(create_only_string("validation_record_id"))),
        aws(ResourceSchema::new("iam.Role")
            .attribute(identity_string("role_name"))
            .with_unique_name_attribute("role_name")),
        aws(ResourceSchema::new("route53.RecordSet")
            .attribute(create_only_string("hosted_zone_id"))
            .attribute(create_only_string("name"))
            .attribute(
                AttributeSchema::optional("type", fixture_route53_record_type())
                    .create_only()
                    .build(),
            )),
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
        awscc(
            ResourceSchema::new("iam.Role")
                .attribute(identity_string("role_name"))
                .with_unique_name_attribute("role_name"),
        ),
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
        awscc(
            ResourceSchema::new("logs.LogGroup")
                .attribute(identity_string("log_group_name"))
                .with_unique_name_attribute("log_group_name"),
        ),
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
        awscc(
            ResourceSchema::new("test.Widget")
                .attribute(create_only_string("external_name"))
                .attribute(
                    AttributeSchema::optional("b", AttributeType::list(AttributeType::string()))
                        .build(),
                ),
        ),
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
    pub moved_hints: MovedBlockHints,
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
    pub deferred_for_expressions: carina_core::parser::ResolvedDeferredForExpressions,
    pub export_params: Vec<carina_core::parser::ExpandedInferredExportParam>,
    pub resolved_export_params: carina_core::validation::ExportsCheckedPreApply<
        carina_core::parser::ExpandedInferredExportParam,
    >,
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

/// Build an explicitly scoped provider router for rendering a hand-built plan.
pub fn provider_router_for_plan<S>(plan: &Plan<S>) -> ProviderRouter {
    let instances = plan
        .effects()
        .iter()
        .map(|effect| ProviderInstanceKey::from(effect.resource_id()))
        .collect::<std::collections::BTreeSet<_>>();
    let mut provider = ProviderRouter::new();
    for instance in instances {
        provider.add_provider_instance(
            instance,
            InitializedProvider::unavailable(Box::new(carina_provider_mock::MockProvider::new())),
        );
    }
    provider
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
    let directive_changes = persisted_directive_changes(
        fixture.state_file.as_ref(),
        &fixture.desired_resources,
        &fixture.plan,
    );
    PlanChangeSummary::new(
        &fixture.plan,
        &export_changes,
        &fixture.deferred_for_expressions,
        &directive_changes,
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
    if fixture_pathbuf.file_name().is_some_and(|name| {
        name == "moved_claims_precede_heuristics"
            || name == "anonymous_old_identity_name_override"
            || name == "anonymous_old_identity_update"
            || name == "anonymous_loop_insert_before"
            || name == "anonymous_loop_reorder"
            || name == "anonymous_loop_delete_earlier"
            || name == "anonymous_loop_sibling_files"
            || name == "anonymous_loop_same_tail"
            || name == "anonymous_if_legacy"
            || name == "anonymous_loop_deferred_map"
            || name == "anonymous_loop_module_call"
            || name == "anonymous_loop_claimed_legacy"
            || name == "anonymous_loop_new_id_exists"
    }) {
        // These fixtures exercise pre-refresh anonymous reconciliation, so
        // compute their desired addresses before claims and move detection.
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
        remote_bindings.insert(us.binding.to_string(), bindings);
    }
    let resolved_provider_configs = carina_core::validation::deferred_types::with_session_for_test(
        &carina_core::parser::ProviderContext::default(),
        |session| {
            carina_core::parser::resolve_provider_attributes_with_remote(
                parsed,
                &remote_bindings,
                session,
            )
        },
    )
    .expect("fixture provider configs must resolve");
    let raw_provider_configs = resolved_provider_configs.as_provider_configs();
    // Fixture planning still follows the production comparison path: every
    // resource route has an initialized provider and an explicit scope.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime for fixture providers");
    let mut provider_router = ProviderRouter::new();
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
            carina_core::provider::InitializedProvider::unavailable(Box::new(
                carina_provider_mock::MockProvider::new(),
            )),
        );
    }
    // Built-in fixture providers (notably `mock`) do not have a loaded
    // factory, so the loop above cannot register their routes. Register every
    // exact route named by provider configuration or fixture resources; this
    // also keeps named instances aligned with the ResourceId used later by
    // comparison resolution.
    let fixture_routes = raw_provider_configs
        .iter()
        .map(ProviderInstanceKey::from)
        .chain(
            parsed
                .resources
                .iter()
                .map(|resource| ProviderInstanceKey::from(&resource.id)),
        )
        .chain(
            data_sources
                .iter()
                .map(|resource| ProviderInstanceKey::from(&resource.id)),
        )
        .collect::<BTreeSet<_>>();
    for route in fixture_routes {
        provider_router.add_provider_instance(
            route,
            InitializedProvider::unavailable(Box::new(carina_provider_mock::MockProvider::new())),
        );
    }
    let expansion_prelude = expansion_prelude
        .detect_moves(
            wiring,
            &provider_router,
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
    let data_source_values = DataSourceValues::from_data_sources(&data_sources, |data_source| {
        DataSourceValue::read(
            data_source_states
                .get(&data_source.id)
                .cloned()
                .expect("fixture initialized every data-source state"),
        )
    });

    // Fixtures have no provider/upstream I/O phase. Only loops rooted in the
    // loaded managed-state view can materialize here; keep every other loop as
    // a display residual instead of turning it into an executable deferred
    // create. This stays fixture-only because production must retain those
    // deferred creates for apply-time expansion.
    let managed_bindings: HashSet<carina_core::resource::AbsoluteBindingAddress> =
        expansion_prelude
            .sorted_resources()
            .iter()
            .filter_map(|resource| resource.binding.clone())
            .collect();
    let (non_managed_deferred_for, _managed_deferred_for) = parsed
        .deferred_for_expressions()
        .clone()
        .partition(|deferred| {
            !deferred
                .root_bindings()
                .iter()
                .any(|binding| managed_bindings.contains(binding))
        });
    let reconciled_expansion = expansion_prelude
        .bind_projected_phase1_state(managed_current, orphans, HashSet::new())
        .reconcile_addresses(parsed, crate::wiring::DeferredForWarningPolicy::Print)
        .expect("fixture move routes must match their loaded current states");
    let prepared_expansion = reconciled_expansion.seal(data_source_values);
    let mut validated_expansion = validated_config
        .expand_deferred_for_plan(
            prepared_expansion,
            &remote_bindings,
            crate::wiring::DeferredForWarningPolicy::Print,
            &provider_router,
        )
        .expect("fixture deferred-for expansion failed");
    validated_expansion.retain_fixture_deferred_for(&managed_bindings, non_managed_deferred_for);
    let parsed = validated_expansion.resources().parsed();
    let export_params = parsed.export_params.clone();
    let expansion_trace = parsed.expansion_trace.clone();
    let export_wait_aliases: Vec<carina_core::binding_index::WaitAliasSpec> = parsed
        .wait_bindings
        .iter()
        .map(carina_core::binding_index::WaitAliasSpec::from)
        .collect();
    let schemas = validated_expansion.context().schemas().clone();
    let post_expansion = {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("failed to build tokio runtime for post-expansion plan construction");
        let validated_expansion = match state_file.as_ref() {
            Some(state_file) => rt
                .block_on(
                    validated_expansion
                        .settle_expanded_children_from_cache(&provider_router, state_file),
                )
                .expect("fixture expanded-child cached-state restore must succeed"),
            None => validated_expansion.settle_expanded_children_absent(),
        };
        rt.block_on(
            validated_expansion.finish_post_expansion_plan::<crate::wiring::PlanMode>(
                crate::wiring::PostExpansionPlanEnvironment {
                    data_sources: &data_sources,
                    provider: &provider_router,
                    normalizer: &provider_router,
                    provider_configs: raw_provider_configs,
                    remote_bindings: &remote_bindings,
                    state_file: &state_file,
                    state_block_claims: &state_block_claims,
                    resolved_state_block_targets: &resolved_state_block_targets,
                    preparation_errors: None,
                },
            ),
        )
        .expect("fixture post-expansion plan construction must succeed")
    };
    match post_expansion {
        crate::wiring::PostExpansionOutcome::Diagnostics(diagnostics) => {
            let crate::wiring::PostExpansionArtifacts {
                expanded_resources: validated_expansion,
                residual_deferred_for,
                plan,
                compositions,
                data_sources: _,
                managed_current,
                data_source_values,
                moved_origins,
                prev_explicit,
                wait_aliases: _,
            } = diagnostics.into_artifacts();
            let moved_hints =
                detect_moved_block_hints(&plan, &managed_current, &schemas, &provider_router)
                    .expect("fixture moved-hint comparisons must resolve");
            FixturePlan {
                plan,
                moved_hints,
                // Error plans abort before resolved-resource and export consumers.
                desired_resources: Vec::new(),
                unresolved_resources: Vec::new(),
                compositions,
                managed_current,
                data_source_states: data_source_values.read_states_map(),
                state_file,
                schemas,
                moved_origins,
                deferred_for_expressions: residual_deferred_for,
                export_params,
                resolved_export_params:
                    carina_core::validation::deferred_types::with_session_for_test(
                        validated_expansion.context().provider_context(),
                        |session| {
                            carina_core::validation::check_export_values_pre_apply(
                                &[],
                                &carina_core::binding_index::ResolvedBindings::default(),
                                session,
                            )
                        },
                    )
                    .expect("empty exports are checked"),
                prev_explicit,
                expansion_trace,
            }
        }
        crate::wiring::PostExpansionOutcome::Planned(crate::wiring::PostExpansionPlan {
            artifacts,
            resources:
                crate::wiring::PostExpansionResolvedResources {
                    resolved: resolved_resources,
                    unresolved: unresolved_resources,
                },
        }) => {
            let crate::wiring::PostExpansionArtifacts {
                expanded_resources: validated_expansion,
                residual_deferred_for,
                plan,
                compositions,
                data_sources: _,
                managed_current,
                data_source_values,
                moved_origins,
                prev_explicit,
                wait_aliases: _,
            } = artifacts;
            let moved_hints =
                detect_moved_block_hints(&plan, &managed_current, &schemas, &provider_router)
                    .expect("fixture moved-hint comparisons must resolve");
            let resources_for_exports = resolved_resources
                .iter()
                .map(|resource| resource.as_inner().clone())
                .collect::<Vec<_>>();
            let resolved_export_params = crate::commands::plan::check_exports_pre_apply(
                crate::commands::plan::CheckExportsPreApplyInput {
                    export_params: &export_params,
                    resources: &resources_for_exports,
                    compositions: &compositions,
                    managed_current: &managed_current,
                    data_source_values: &data_source_values,
                    state_file: state_file.as_ref(),
                    plan: &plan,
                    schemas: &schemas,
                    wait_aliases: &export_wait_aliases,
                    provider_context: validated_expansion.context().provider_context(),
                },
            )
            .expect("fixture exports must resolve without secrets");

            FixturePlan {
                plan,
                moved_hints,
                desired_resources: resolved_resources,
                unresolved_resources,
                compositions,
                managed_current,
                data_source_states: data_source_values.into_read_states(),
                state_file,
                schemas,
                moved_origins,
                deferred_for_expressions: residual_deferred_for,
                export_params,
                resolved_export_params,
                prev_explicit,
                expansion_trace,
            }
        }
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
            | Some("anonymous_old_identity_name_override")
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
    let mut schemas = match fixture_path.file_name().and_then(|name| name.to_str()) {
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
    schemas.push((
        "aws".to_string(),
        ResourceSchema::new("route53.RecordSet").attribute(
            AttributeSchema::optional("type", fixture_route53_record_type())
                .create_only()
                .build(),
        ),
    ));
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

fn fixture_route53_record_type() -> AttributeType {
    AttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["route53"], "RecordType"),
        Some(vec!["CNAME".to_string()]),
        vec![("CNAME".to_string(), "cname".to_string())],
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
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
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
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
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
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
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
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
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
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
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
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
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
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
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
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
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
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { unreachable!("plan fixture does not instantiate providers") })
    }

    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
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
