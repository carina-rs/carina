//! Final plan preparation owns the CLI's state-release capability.
//!
//! The private field has no production constructor outside this module. The
//! pipeline below is the only place that wraps final plan values.
use super::*;
use carina_core::differ::{ComparisonContext, ReferenceEquality};
use carina_core::provider::PersistedAttributes;
use carina_core::validation::admission::final_input::FinalDesiredValues;
use carina_state::{ResourceState, ResourceStateEncodingError, ResourceStateWrite};

pub(crate) struct ReleaseWitness {
    release_values: FinalDesiredValues,
}

impl ReleaseWitness {
    #[cfg(test)]
    pub(crate) fn for_test(
        resources: impl IntoIterator<Item = (ResourceId, IndexMap<String, Value>)>,
    ) -> Self {
        Self {
            release_values: FinalDesiredValues::for_test(resources),
        }
    }
}

/// The single CLI-to-state adapter that can unwrap a final desired capability.
/// Its contents are borrowed only for the immediately following state write.
pub(crate) fn write_resource_state<S: Clone, R: ReferenceEquality>(
    resource: &ResolvedResource<S>,
    source: ResourceStateWrite<'_>,
    existing: Option<&ResourceState>,
    schema: Option<&carina_core::schema::CompiledResourceSchema>,
    desired: Option<&ReleaseWitness>,
    preferred_hashes: Option<&PersistedAttributes>,
    comparison: &ComparisonContext<'_, R>,
) -> Result<ResourceState, ResourceStateEncodingError> {
    ResourceState::from_write_with_final_desired(
        resource,
        source,
        existing,
        schema,
        desired.map(|witness| &witness.release_values),
        preferred_hashes,
        comparison,
    )
}

/// The sole final desired preparation body for plan, apply and refresh.
/// Reference evidence is prepared with the plan so state equality uses the
/// same resource scope, canonical operands and comparison options.
pub(super) async fn prepare_post_expansion_inputs<M: PostExpansionMode, E>(
    input: PostExpansionPlanInput<'_, E, impl ChildrenSettledFor<M>>,
    row_request: RowRequest,
) -> Result<PostExpansionOutcome<M, PostExpansionConfiguration<E>>, AppError> {
    crate::wiring::settle_type_checks_with_warning_policy(
        carina_core::validation::deferred_types::TypeCheckSession::run_async(
            input.context.provider_context(),
            async |session| {
                let PostExpansionPlanInput {
                    context,
                    expansion,
                    environment,
                } = input;
                let PostExpansionPlanEnvironment {
                    data_sources,
                    provider,
                    normalizer,
                    provider_configs,
                    remote_bindings,
                    state_file,
                    state_block_claims,
                    resolved_state_block_targets,
                    preparation_errors,
                } = environment;
                let PostExpansionPlanView { data, children } = expansion;
                children.consume();
                let PostExpansionPlanData {
                    mut reference_preparation,
                    expanded_resources,
                    sorted_resources,
                    mut addressed,
                    mut data_source_values,
                    wait_aliases,
                    residual_deferred_for,
                    deferred_create_targets,
                } = data;
                // awscc#251 (follow-up to #3055): data sources do not flow through
                // `into_plan_input_current_map`, so lift them explicitly at this seam.
                lift_data_source_value_enum_leaves(
                    &mut data_source_values,
                    data_sources,
                    context.schemas(),
                );
                let upstream_binding_names = expanded_resources.parsed().upstream_binding_names();
                let managed_stability = derive_managed_binding_stability(
                    expanded_resources.parsed(),
                    &sorted_resources,
                    ManagedBindingStabilityContext {
                        managed_current: &addressed.managed_current,
                        data_source_values: &data_source_values,
                        schemas: context.schemas(),
                        saved_attrs: &addressed.saved_attrs,
                        prev_explicit: &addressed.prev_explicit,
                        remote_bindings,
                        wait_aliases: &wait_aliases,
                        provider,
                        reference_preparation: &mut reference_preparation,
                        session,
                    },
                )
                .await?;
                let pre_apply_current = carina_core::resource::into_plan_input_current_map(
                    addressed.managed_current.clone(),
                    context.schemas(),
                    &sorted_resources,
                );
                let projected_state = addressed.projected_state(state_file.as_ref());
                let (mut override_aware_resources, unresolved_override_aware_resources) =
                    OverrideAwareResources::build_pair_with_stability(
                        expanded_resources.as_pair(),
                        &projected_state,
                        LivePreApplyInputs {
                            managed: &[],
                            compositions: &expanded_resources.parsed().compositions,
                            managed_current: &pre_apply_current,
                            data_source_values: &data_source_values,
                            remote_bindings,
                            wait_aliases: &wait_aliases,
                        },
                        &managed_stability,
                        M::override_aware_build_mode(&upstream_binding_names),
                        session,
                    )?;
                M::warning_policy().publish(|| {
                    crate::legacy_name_overrides::emit_legacy_override_warnings(
                        &override_aware_resources,
                        &projected_state,
                    );
                });

                // Refresh-time reads get concrete inputs. Apply-time deferred reads retain
                // structural refs so saved-plan apply can rebuild their dependency graph.
                let mut data_sources_for_plan = prepare_data_sources_for_plan(
                    &data_source_values,
                    override_aware_resources.bindings(),
                    M::data_source_upstream_bindings(&upstream_binding_names),
                    session,
                )?;
                let constraint_origin_resources = override_aware_resources
                    .paired_unresolved_resources_with_binding_sources(
                        &unresolved_override_aware_resources,
                    );
                let mut compositions = expanded_resources.parsed().compositions.clone();
                let module_gate =
                    carina_core::executor::ModuleConstraintGate::from_live_compositions(
                        &compositions,
                    );
                let wait_bindings = expanded_resources.parsed().wait_bindings.clone();
                let preparation = if let Some(errors) = preparation_errors {
                    Err(PlanPreparationError::Plan(errors))
                } else {
                    PlanPreprocessor::new(normalizer, context)
                        .prepare(
                            &mut override_aware_resources,
                            &constraint_origin_resources,
                            &module_gate,
                            &mut addressed.managed_current,
                            &mut data_source_values,
                            &mut addressed.orphans,
                            provider_configs,
                            &mut data_sources_for_plan,
                            data_sources,
                        )
                        .await
                };
                match preparation {
                    Ok(()) => {}
                    Err(error) => {
                        let errors = match error {
                            PlanPreparationError::Plan(errors) => errors,
                            PlanPreparationError::Provider(error) => {
                                return Err(AppError::Provider(error));
                            }
                            PlanPreparationError::ModuleConstraint(error) => {
                                return Err(AppError::ModuleConstraint(error));
                            }
                            error @ PlanPreparationError::Knowledge { .. } => {
                                return Err(error.into());
                            }
                        };
                        let mut plan = Plan::new();
                        for error in errors.as_slice().iter().cloned() {
                            plan.add_error(error);
                        }
                        let moved_origins = addressed
                            .moved_pairs
                            .iter()
                            .map(|(from, to)| (to.as_inner().clone(), from.as_inner().clone()))
                            .collect();
                        let artifacts = PostExpansionArtifacts {
                            expanded_resources: PostExpansionConfiguration::for_diagnostics(
                                expanded_resources.into_configuration(),
                            ),
                            residual_deferred_for,
                            compositions,
                            data_sources: data_sources_for_plan,
                            managed_current: into_all_prior_states(
                                addressed.orphans,
                                addressed.managed_current,
                            ),
                            data_source_values,
                            moved_origins,
                            prev_explicit: addressed.prev_explicit,
                            wait_aliases,
                        };
                        return Ok(PostExpansionOutcome::Diagnostics(
                            PostExpansionDiagnostics::new(artifacts, Box::new(plan), errors),
                        ));
                    }
                };

                // Anonymous identities that depend on normalized ResourceRefs become
                // concrete only here. State transfer and collision validation therefore
                // share this same addressed-state owner.
                let resolved_desired_ids = reconcile_late_anonymous_identities(
                    context,
                    LateAnonymousIdentityInputs {
                        resources: &mut override_aware_resources,
                        addressed: &mut addressed,
                        data_sources: &data_sources_for_plan,
                        state_file: state_file.as_ref(),
                        state_block_claims,
                        providers: provider_configs,
                        comparison_provider: provider,
                    },
                )?;
                validate_plan_time_state_block_collisions(
                    &resolved_desired_ids,
                    &addressed.moved_pairs,
                    resolved_state_block_targets,
                    state_file,
                )?;

                let configuration = expanded_resources
                    .into_plan_configuration(&override_aware_resources, &data_sources_for_plan);
                let graph = resolve_post_expansion_graph(configuration, state_file.as_ref())?;
                let (expanded_resources, expanded_graph) = graph.into_parts();
                compositions = expanded_resources.parsed().compositions.clone();
                let deferred_create_targets =
                    deferred_create_targets.resolve_graph(&expanded_graph)?;

                let reconciled_provider_states = provider_states_for(
                    &addressed.managed_current,
                    &addressed.orphans,
                    override_aware_resources.resources(),
                );
                let reconciled_input_current = carina_core::resource::into_plan_input_current_map(
                    reconciled_provider_states,
                    context.schemas(),
                    override_aware_resources.resources(),
                );
                let stability_resources = override_aware_resources
                    .resources()
                    .iter()
                    .cloned()
                    .map(|resource| expanded_graph.resolve_resource(resource, context.schemas()))
                    .collect::<Result<Vec<_>, _>>()?;
                let managed_stability = reference_preparation
                    .stability(
                        &stability_resources,
                        &reconciled_input_current,
                        context.schemas(),
                        &addressed.saved_attrs,
                        &addressed.prev_explicit,
                        provider,
                    )
                    .await?
                    .value;
                override_aware_resources
                    .rebuild_bindings_after_identity_reconciliation_with_stability(
                        RebuildBindingsInputs {
                            compositions: &compositions,
                            managed_current: &reconciled_input_current,
                            data_source_values: &data_source_values,
                            remote_bindings,
                            wait_aliases: &wait_aliases,
                        },
                        &managed_stability,
                    );

                // Update ordering looks directives up only for Update effects. Orphan
                // directives stay paired with their typed orphan state for Delete effects.
                let directives_map = addressed
                    .projected_state(state_file.as_ref())
                    .build_directives();
                let paired_unresolved_resources = override_aware_resources
                    .paired_unresolved_resources_with_binding_sources(
                        &unresolved_override_aware_resources,
                    );
                let resolved_resources = override_aware_resources
                    .resources()
                    .iter()
                    .cloned()
                    .zip(paired_unresolved_resources.iter().cloned())
                    .map(|(evaluated, authored)| {
                        expanded_graph.resolve_resource_with_dependency_source(
                            evaluated,
                            authored,
                            context.schemas(),
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let resolved_unresolved_resources = paired_unresolved_resources
                    .iter()
                    .cloned()
                    .map(|resource| expanded_graph.resolve_resource(resource, context.schemas()))
                    .collect::<Result<Vec<_>, _>>()?;
                let resolved_data_sources_for_plan = data_sources_for_plan
                    .iter()
                    .cloned()
                    .map(|data_source| expanded_graph.resolve_data_source(data_source))
                    .collect::<Result<Vec<_>, _>>()?;
                let resolved_wait_bindings = wait_bindings
                    .iter()
                    .map(|wait| expanded_graph.resolve_wait_binding(wait))
                    .collect::<Result<Vec<_>, _>>()?;
                let parsed = expanded_resources.parsed();
                let upstream_binding_names = parsed.upstream_binding_names();
                let no_unresolved_upstream_bindings = HashSet::new();
                let state_block_upstream_bindings = M::state_block_upstream_bindings(
                    &upstream_binding_names,
                    &no_unresolved_upstream_bindings,
                );
                let plan_input_current = carina_core::resource::into_plan_input_current_map(
                    addressed.managed_current.clone(),
                    context.schemas(),
                    override_aware_resources.resources(),
                );
                let outcome_unknown_import_replacements = outcome_unknown_import_replacements(
                    &parsed.state_blocks,
                    state_file,
                    override_aware_resources.resources(),
                );
                let final_inputs =
                    carina_core::validation::admission::final_input::FinalPlanInputs::admit(
                        resolved_resources,
                        resolved_data_sources_for_plan,
                        context.schemas(),
                        override_aware_resources.bindings(),
                        session,
                    )?;
                let dependents =
                    CascadeDependents::new(&resolved_unresolved_resources, deferred_create_targets);
                let prepared = reference_preparation
                    .plan_with_cascades(
                        &final_inputs,
                        &dependents,
                        provider,
                        PlanPriorStates::new(&plan_input_current, &addressed.orphans),
                        &directives_map,
                        context.schemas(),
                        &addressed.saved_attrs,
                        &addressed.prev_explicit,
                        &resolved_wait_bindings,
                        &outcome_unknown_import_replacements,
                    )
                    .await?;
                let mut state_block_result = Ok(());
                let plan = prepared
                    .value
                    .finish(resolved_desired_ids.ids.iter(), |plan| {
                        add_deposed_delete_effects(plan, state_file, &addressed.moved_pairs);
                        plan.block_deletes_on_prior_consumer_updates(&directives_map);
                        override_aware_resources.apply_plan_stability(
                            &plan_input_current,
                            plan,
                            context.schemas(),
                        );

                        // State blocks run after differ/deposed identifier errors so `removed`
                        // remains the explicit recovery that can suppress those diagnostics.
                        state_block_result = add_state_block_effects(
                            plan,
                            &parsed.state_blocks,
                            state_file,
                            &addressed.moved_pairs,
                            override_aware_resources.resources(),
                            StateBlockEvaluationContext {
                                removed: &resolved_state_block_targets.removed_from,
                                bindings: override_aware_resources.bindings(),
                                unresolved_upstream_bindings: state_block_upstream_bindings,
                                session,
                            },
                        );
                    });
                state_block_result?;

                let moved_origins = addressed
                    .moved_pairs
                    .iter()
                    .map(|(from, to)| (to.as_inner().clone(), from.as_inner().clone()))
                    .collect();
                let prev_explicit = addressed.prev_explicit;
                let managed_current =
                    into_all_prior_states(addressed.orphans, addressed.managed_current);
                let input_sources = paired_unresolved_resources
                    .iter()
                    .cloned()
                    .map(UnresolvedResource::from_pre_resolve)
                    .collect::<Vec<_>>();
                let input_types = carina_core::input_rules::InputTypeContext::for_plan(
                    &input_sources,
                    &data_sources_for_plan,
                    carina_core::input_rules::InputCompositions::Live(&compositions),
                    &wait_aliases,
                    provider_configs,
                    context.schemas(),
                    plan.effects().iter().map(|effect| effect.resource_id()),
                )?
                .with_provider_context(context.provider_context());
                let final_desired = ReleaseWitness {
                    release_values: final_inputs.desired_values(&prepared.references),
                };
                let outcome = prepare_plan_outcome(
                    plan,
                    prepared.references,
                    &mut reference_preparation,
                    row_request,
                    &managed_current,
                    state_file,
                    context.schemas(),
                    &input_types,
                    &prev_explicit,
                    provider,
                )
                .await?;
                let artifacts = PostExpansionArtifacts {
                    expanded_resources,
                    residual_deferred_for,
                    compositions,
                    data_sources: data_sources_for_plan,
                    managed_current,
                    data_source_values,
                    moved_origins,
                    prev_explicit,
                    wait_aliases,
                };
                let plan = match outcome {
                    PlanOutcome::Planned(plan) => plan,
                    PlanOutcome::Diagnostics(DiagnosticPlan { plan, errors }) => {
                        return Ok(PostExpansionOutcome::Diagnostics(
                            PostExpansionDiagnostics::new(artifacts, plan, errors),
                        ));
                    }
                };
                let resources = M::build_resources(PostExpansionResourceInputs {
                    override_aware: &override_aware_resources,
                    unresolved_override_aware: &unresolved_override_aware_resources,
                    resolved: final_inputs.into_resources(),
                    final_desired,
                    paired_unresolved: paired_unresolved_resources,
                });

                Ok(PostExpansionOutcome::Planned(PostExpansionPlan {
                    plan,
                    artifacts,
                    resources,
                }))
            },
        )
        .await,
        AppError::Validation,
        M::warning_policy(),
    )
}

#[cfg(test)]
mod tests;
