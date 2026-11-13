use std::collections::BTreeSet;

use carina_core::effect::ResolvedMovePairs;
use carina_core::parser::ExpandedFile;
use carina_core::resolved_graph::{
    ConfiguredGraphInput, LegacyClassificationGraph, ResolvedExpandedGraph, loop_group_bindings,
    resolve_expanded_graph_for_legacy_classification,
};
use carina_state::{
    LegacyClassificationOutcome, LegacyDependencyClassification, StateFile,
    UnvalidatedLegacyMigration,
};

use crate::error::AppError;
use crate::output_style::{ColorMode, Colorize};

/// Configuration and materialization evidence travel as one owned value.
/// Commands can inspect it, but only the materializer can construct Planned.
pub(crate) struct MaterializedConfiguration<E> {
    parsed: ExpandedFile<E>,
    materialization: ConfigurationMaterialization,
}

#[derive(Clone)]
enum ConfigurationMaterialization {
    Planned(CompleteDeferredForMaterialization),
    Offline(ResidualDeferredLoops),
}

/// Minted only after the materializer has checked its final loop collection.
#[derive(Clone)]
struct CompleteDeferredForMaterialization;

#[derive(Clone)]
struct ResidualDeferredLoops {
    bindings: BTreeSet<String>,
}

impl ResidualDeferredLoops {
    fn from_loops(loops: &carina_core::parser::ResolvedDeferredForExpressions) -> Self {
        Self {
            bindings: loops
                .iter()
                .flat_map(|deferred| match deferred.target() {
                    carina_core::parser::DeferredForTarget::Bound(target) => {
                        let binding = target.binding_address();
                        std::iter::once(binding.to_string())
                            .chain(
                                loop_group_bindings(binding)
                                    .into_iter()
                                    .map(|binding| binding.to_string()),
                            )
                            .collect::<Vec<_>>()
                    }
                    carina_core::parser::DeferredForTarget::Unbound { .. } => Vec::new(),
                })
                .collect(),
        }
    }
}

/// Private materializer result. Completion and residual loops are mutually
/// exclusive, and neither payload can be constructed by a command.
#[derive(Clone)]
pub(super) struct DeferredForExpansionWitness(ConfigurationMaterialization);

impl DeferredForExpansionWitness {
    pub(super) fn after_expansion(
        loops: &carina_core::parser::ResolvedDeferredForExpressions,
    ) -> Self {
        let residual = ResidualDeferredLoops::from_loops(loops);
        Self(if loops.is_empty() {
            ConfigurationMaterialization::Planned(CompleteDeferredForMaterialization)
        } else {
            ConfigurationMaterialization::Offline(residual)
        })
    }
}

impl<E> MaterializedConfiguration<E> {
    pub(super) fn after_expansion(
        parsed: ExpandedFile<E>,
        witness: DeferredForExpansionWitness,
    ) -> Self {
        Self {
            parsed,
            materialization: witness.0,
        }
    }

    pub(super) fn offline(parsed: ExpandedFile<E>) -> Self {
        let residual_loops = ResidualDeferredLoops::from_loops(parsed.deferred_for_expressions());
        Self {
            parsed,
            materialization: ConfigurationMaterialization::Offline(residual_loops),
        }
    }
}

impl<E> std::ops::Deref for MaterializedConfiguration<E> {
    type Target = ExpandedFile<E>;

    fn deref(&self) -> &Self::Target {
        &self.parsed
    }
}

type ExpandedLegacyClassificationGraph<E> = LegacyClassificationGraph<
    E,
    carina_core::parser::ResolvedDeferredForPhase<
        carina_core::resource::NoPlaceholder,
        carina_core::resource::LoopSlot,
        carina_core::resource::AbsoluteBindingAddress,
    >,
>;

/// No writable state or executable graph can leave this owner until warnings
/// have been reported and the same graph has passed strict wait validation.
pub(crate) enum ClassifiedLegacyGraph<E> {
    Current(Box<ExpandedLegacyClassificationGraph<E>>),
    Migrating(Box<UnvalidatedLegacyMigration<ExpandedLegacyClassificationGraph<E>>>),
}

pub(crate) struct ValidatedLegacyGraph<E> {
    pub graph: ResolvedExpandedGraph<
        E,
        carina_core::parser::ResolvedDeferredForPhase<
            carina_core::resource::NoPlaceholder,
            carina_core::resource::LoopSlot,
            carina_core::resource::AbsoluteBindingAddress,
        >,
    >,
    pub classification: Option<LegacyDependencyClassification>,
    pub migrated_state: Option<StateFile>,
}

impl<E> ClassifiedLegacyGraph<E> {
    pub(crate) fn migrated_rows(&self) -> usize {
        match self {
            Self::Current(_) => 0,
            Self::Migrating(migration) => migration.migrated_rows(),
        }
    }

    pub(crate) fn warning_count(&self) -> usize {
        match self {
            Self::Current(_) => 0,
            Self::Migrating(migration) => migration.warnings().len(),
        }
    }

    pub(crate) fn report_and_validate(
        self,
        color_mode: ColorMode,
    ) -> Result<ValidatedLegacyGraph<E>, AppError> {
        match self {
            Self::Current(graph) => Ok(ValidatedLegacyGraph {
                graph: graph.validate()?,
                classification: None,
                migrated_state: None,
            }),
            Self::Migrating(migration) => {
                let validated = migration
                    .report_and_validate(|warning| report_legacy_warning(color_mode, warning))
                    .map_err(|error| AppError::Backend(error.into()))?;
                Ok(ValidatedLegacyGraph {
                    graph: validated.graph,
                    classification: Some(validated.classification),
                    migrated_state: Some(validated.state),
                })
            }
        }
    }
}

pub(crate) fn classify_legacy_state<E>(
    configuration: MaterializedConfiguration<E>,
    moves: &ResolvedMovePairs,
    state: Option<&StateFile>,
) -> Result<LegacyClassificationOutcome<ClassifiedLegacyGraph<E>>, AppError> {
    let MaterializedConfiguration {
        parsed,
        materialization,
    } = configuration;
    let offline_unmaterialized = match materialization {
        ConfigurationMaterialization::Planned(_complete) => BTreeSet::new(),
        ConfigurationMaterialization::Offline(residual_loops) => residual_loops.bindings,
    };
    let desired_ids = parsed
        .resources
        .iter()
        .map(|resource| resource.id.clone())
        .collect();
    let prior_orphans = state
        .map(|state| state.build_orphans(&desired_ids).into_plan_orphans())
        .unwrap_or_default();
    let graph = resolve_expanded_graph_for_legacy_classification(
        ConfiguredGraphInput::from(parsed).with_prior_orphans(prior_orphans),
    )?;
    if let Some(state) = state.filter(|state| state.requires_legacy_dependency_migration()) {
        let outcome = state
            .clone()
            .classify_legacy_graph(graph, moves, offline_unmaterialized)
            .map_err(|error| AppError::Backend(error.into()))?;
        match outcome {
            LegacyClassificationOutcome::Classified(migration) => {
                Ok(LegacyClassificationOutcome::Classified(
                    ClassifiedLegacyGraph::Migrating(Box::new(migration)),
                ))
            }
            LegacyClassificationOutcome::RequiresReads(rows) => {
                Ok(LegacyClassificationOutcome::RequiresReads(rows))
            }
        }
    } else {
        Ok(LegacyClassificationOutcome::Classified(
            ClassifiedLegacyGraph::Current(Box::new(graph)),
        ))
    }
}

pub(crate) fn replay_legacy_migration(
    color_mode: ColorMode,
    state: StateFile,
    classification: LegacyDependencyClassification,
) -> Result<StateFile, AppError> {
    state
        .replay_legacy_graph(classification)
        .report_and_validate(|warning| report_legacy_warning(color_mode, warning))
        .map(|validated| validated.state)
        .map_err(|error| AppError::Backend(error.into()))
}

fn report_legacy_warning(
    color_mode: ColorMode,
    warning: &carina_state::LegacyDestroyOrderingWarning,
) {
    let generation = warning
        .generation
        .as_ref()
        .map(|key| format!(" (deposed generation {})", key.as_str()))
        .unwrap_or_default();
    for legacy_name in &warning.legacy_names {
        eprintln!(
                "{}",
                format!(
                    "Warning: destroy ordering for {}{} is partially unknown: legacy dependency {:?} no longer resolves.",
                    warning.row, generation, legacy_name
                )
                .yellow(color_mode)
            );
    }
}
