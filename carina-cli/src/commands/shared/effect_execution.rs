//! Helpers for executing import and state-only effects with user feedback.

use carina_core::effect::Effect;
use carina_core::executor::ExecutionResult;
use carina_core::plan::Plan;
use carina_core::provider::Provider;
use colored::Colorize;

/// Execute import effects by reading the resource from the provider.
///
/// For each Import effect, calls provider.read() with the given identifier
/// to fetch the current state and stores the result in applied_states
/// so that finalize_apply can persist it.
pub(crate) async fn execute_import_effects(
    plan: &Plan,
    provider: &dyn Provider,
    result: &mut ExecutionResult,
) {
    for effect in plan.effects() {
        if let Effect::Import { id, identifier } = effect {
            println!("  {} Importing {} (id: {})...", "<-".cyan(), id, identifier);
            match provider.read(id, Some(identifier)).await {
                Ok(state) => {
                    if state.exists {
                        println!("  {} Imported {}", "✓".green(), id);
                        result.applied_states.insert(id.clone(), state);
                        result.success_count += 1;
                    } else {
                        println!(
                            "  {} Import failed: resource {} with id {} not found",
                            "✗".red(),
                            id,
                            identifier
                        );
                        result.failure_count += 1;
                    }
                }
                Err(e) => {
                    println!("  {} Import failed for {}: {}", "✗".red(), id, e);
                    result.failure_count += 1;
                }
            }
        }
    }
}

/// Execute state-only effects (remove, move) with user feedback.
///
/// These effects only modify state and don't call the provider.
pub(crate) fn execute_state_only_effects(plan: &Plan, result: &mut ExecutionResult) {
    for effect in plan.effects() {
        match effect {
            Effect::Remove { id } => {
                println!("  {} Removing {} from state", "x".red(), id);
                result.success_count += 1;
            }
            Effect::Move { from, to } => {
                println!("  {} Moving {} -> {}", "->".yellow(), from, to);
                result.success_count += 1;
            }
            _ => {}
        }
    }
}
