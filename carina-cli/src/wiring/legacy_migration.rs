use std::collections::BTreeSet;

use carina_core::effect::ResolvedMovePairs;
use carina_core::parser::ModuleResolvedFile;
use carina_core::resolved_graph::{
    LegacyClassificationGraph, ResolvedExpandedGraph, loop_group_bindings,
    resolve_expanded_graph_for_legacy_classification,
};
use carina_state::{
    LegacyClassificationOutcome, LegacyDependencyClassification, StateFile,
    UnvalidatedLegacyMigration,
};
use colored::Colorize;

use crate::error::AppError;

/// Configuration and materialization evidence travel as one owned value.
/// Commands can inspect it, but only the materializer can construct Planned.
pub(crate) struct MaterializedConfiguration<E> {
    parsed: ModuleResolvedFile<E>,
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
                .flat_map(|deferred| {
                    let target = deferred.target.scheduler_key();
                    std::iter::once(target.to_string()).chain(loop_group_bindings(target))
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
        parsed: ModuleResolvedFile<E>,
        witness: DeferredForExpansionWitness,
    ) -> Self {
        Self {
            parsed,
            materialization: witness.0,
        }
    }

    pub(super) fn offline(parsed: ModuleResolvedFile<E>) -> Self {
        let residual_loops = ResidualDeferredLoops::from_loops(parsed.deferred_for_expressions());
        Self {
            parsed,
            materialization: ConfigurationMaterialization::Offline(residual_loops),
        }
    }
}

impl<E> std::ops::Deref for MaterializedConfiguration<E> {
    type Target = ModuleResolvedFile<E>;

    fn deref(&self) -> &Self::Target {
        &self.parsed
    }
}

/// No writable state or executable graph can leave this owner until warnings
/// have been reported and the same graph has passed strict wait validation.
pub(crate) enum ClassifiedLegacyGraph<E> {
    Current(LegacyClassificationGraph<E, carina_core::parser::ResolvedDeferredForPhase>),
    Migrating(
        UnvalidatedLegacyMigration<
            LegacyClassificationGraph<E, carina_core::parser::ResolvedDeferredForPhase>,
        >,
    ),
}

pub(crate) struct ValidatedLegacyGraph<E> {
    pub graph: ResolvedExpandedGraph<E, carina_core::parser::ResolvedDeferredForPhase>,
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

    pub(crate) fn report_and_validate(self) -> Result<ValidatedLegacyGraph<E>, AppError> {
        match self {
            Self::Current(graph) => Ok(ValidatedLegacyGraph {
                graph: graph.validate()?,
                classification: None,
                migrated_state: None,
            }),
            Self::Migrating(migration) => {
                let validated = migration
                    .report_and_validate(report_legacy_warning)
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
    let graph = resolve_expanded_graph_for_legacy_classification(parsed.into())?;
    if let Some(state) = state.filter(|state| state.requires_legacy_dependency_migration()) {
        let outcome = state
            .clone()
            .classify_legacy_graph(graph, moves, offline_unmaterialized)
            .map_err(|error| AppError::Backend(error.into()))?;
        match outcome {
            LegacyClassificationOutcome::Classified(migration) => {
                Ok(LegacyClassificationOutcome::Classified(
                    ClassifiedLegacyGraph::Migrating(migration),
                ))
            }
            LegacyClassificationOutcome::RequiresReads(rows) => {
                Ok(LegacyClassificationOutcome::RequiresReads(rows))
            }
        }
    } else {
        Ok(LegacyClassificationOutcome::Classified(
            ClassifiedLegacyGraph::Current(graph),
        ))
    }
}

pub(crate) fn replay_legacy_migration(
    state: StateFile,
    classification: LegacyDependencyClassification,
) -> Result<StateFile, AppError> {
    state
        .replay_legacy_graph(classification)
        .report_and_validate(report_legacy_warning)
        .map(|validated| validated.state)
        .map_err(|error| AppError::Backend(error.into()))
}

fn report_legacy_warning(warning: &carina_state::LegacyDestroyOrderingWarning) {
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
                .yellow()
            );
    }
}
