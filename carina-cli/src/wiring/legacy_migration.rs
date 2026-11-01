use std::collections::BTreeSet;

use carina_core::effect::ResolvedMovePairs;
use carina_core::parser::{DeferredForExpression, ModuleResolvedFile};
use carina_core::resolved_graph::{
    LegacyClassificationGraph, ResolvedExpandedGraph, loop_group_bindings,
    resolve_expanded_graph_for_legacy_classification,
};
use carina_core::resource::{DataSource, Resource};
use carina_state::{
    LegacyDependencyClassification, StateFile, UnreportedMigratedState, UnvalidatedLegacyMigration,
};
use colored::Colorize;

use crate::error::AppError;

pub(crate) enum MaterializedConfiguration<'a> {
    Planned {
        resources: &'a [Resource],
        data_sources: &'a [DataSource],
    },
    Offline {
        resources: &'a [Resource],
        data_sources: &'a [DataSource],
        residual_loops: &'a [DeferredForExpression],
    },
}

impl MaterializedConfiguration<'_> {
    fn parts(&self) -> (&[Resource], &[DataSource], BTreeSet<String>) {
        match self {
            Self::Planned {
                resources,
                data_sources,
            } => (resources, data_sources, BTreeSet::new()),
            Self::Offline {
                resources,
                data_sources,
                residual_loops,
            } => {
                let unmaterialized = residual_loops
                    .iter()
                    .flat_map(|deferred| {
                        let target = deferred.target.scheduler_key();
                        std::iter::once(target.to_string()).chain(loop_group_bindings(target))
                    })
                    .collect();
                (resources, data_sources, unmaterialized)
            }
        }
    }
}

/// No writable state or executable graph can leave this owner until warnings
/// have been reported and the same graph has passed strict wait validation.
pub(crate) enum ClassifiedLegacyGraph<E> {
    Current(LegacyClassificationGraph<E, carina_core::parser::ResolvedDeferredForPhase>),
    Migrating(UnvalidatedLegacyMigration<E, carina_core::parser::ResolvedDeferredForPhase>),
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
                let validated = migration.report_and_validate(report_legacy_warning)?;
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
    parsed: ModuleResolvedFile<E>,
    configured: MaterializedConfiguration<'_>,
    moves: &ResolvedMovePairs,
    state: Option<&StateFile>,
) -> Result<ClassifiedLegacyGraph<E>, AppError> {
    let (resources, data_sources, offline_unmaterialized) = configured.parts();
    let graph = resolve_expanded_graph_for_legacy_classification(parsed, resources, data_sources)?;
    if let Some(state) = state.filter(|state| state.requires_legacy_dependency_migration()) {
        let migration = state
            .clone()
            .classify_legacy_graph(graph, moves, offline_unmaterialized)
            .map_err(|error| AppError::Backend(error.into()))?;
        Ok(ClassifiedLegacyGraph::Migrating(migration))
    } else {
        Ok(ClassifiedLegacyGraph::Current(graph))
    }
}

pub(crate) fn report_legacy_migration(migration: UnreportedMigratedState) -> StateFile {
    migration.report(report_legacy_warning)
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
