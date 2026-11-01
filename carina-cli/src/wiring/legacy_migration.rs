use std::collections::BTreeSet;

use carina_core::effect::ResolvedMovePairs;
use carina_core::parser::{DeferredForExpression, ModuleResolvedFile};
use carina_core::resolved_graph::{
    loop_group_bindings, resolve_expanded_graph_for_legacy_classification,
};
use carina_core::resource::{DataSource, Resource};
use carina_state::{
    LegacyDependencyClassification, LegacyMigrationInput, StateFile, UnreportedMigratedState,
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

pub(crate) fn classify_legacy_state<E>(
    parsed: &mut ModuleResolvedFile<E>,
    configured: MaterializedConfiguration<'_>,
    moves: &ResolvedMovePairs,
    state: &StateFile,
) -> Result<LegacyDependencyClassification, AppError> {
    let (resources, data_sources, offline_unmaterialized) = configured.parts();
    let graph = resolve_expanded_graph_for_legacy_classification(parsed, resources, data_sources)?;
    let input = if offline_unmaterialized.is_empty() {
        LegacyMigrationInput::planned(&graph, resources, moves)
    } else {
        LegacyMigrationInput::offline(&graph, resources, moves, offline_unmaterialized)
    };
    state
        .classify_legacy_dependencies(&input)
        .map_err(|error| AppError::Backend(error.into()))
}

pub(crate) fn report_legacy_migration(migration: UnreportedMigratedState) -> StateFile {
    migration.report(|warning| {
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
    })
}
