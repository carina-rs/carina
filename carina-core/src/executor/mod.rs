//! Plan executor - Executes a Plan by dispatching Effects to a Provider.
//!
//! This module contains the core execution logic extracted from the CLI apply command.
//! It uses an `ExecutionObserver` trait for UI separation, allowing the CLI to provide
//! colored progress output while keeping the execution logic testable.
//!
//! ## Module structure
//!
//! - `basic`: Single-effect execution (Create/Update/Delete), resource resolution, binding map
//! - `parallel`: Dependency computation and fine-grained parallel scheduling
//! - `replace`: Replace effect orchestration (CBD and DBD)
//! - `phased`: Interdependent Replace ordering (4-phase execution)

mod basic;
pub mod normalized;
#[cfg(test)]
mod normalized_tests;
mod parallel;
mod phased;
mod replace;
pub(crate) mod wait;

pub use parallel::UnresolvedResource;
pub use replace::compute_full_diff_patch;

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::time::Duration;

use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::parser::ProviderConfig;
use crate::provider::{Provider, ProviderNormalizer};
use crate::resource::{ResourceId, State, Value};

use parallel::execute_effects_sequential;
use phased::{execute_effects_phased, has_interdependent_replaces};

pub const TEST_UNCAPPED: NonZeroUsize = NonZeroUsize::new(usize::MAX).unwrap();

/// Input data required to execute a plan.
pub struct ExecutionInput<'a> {
    pub plan: &'a crate::plan::Plan,
    pub unresolved_resources: &'a HashMap<ResourceId, UnresolvedResource>,
    /// Virtual resources (module attribute containers). carina#3181:
    /// compositions are a distinct typestate from managed resources, so the
    /// executor's dependency walk receives them as their own slice. A
    /// managed resource that depends on `<module-instance>.<attr>` —
    /// where the module-instance binding is a composition — has that edge
    /// followed through the composition's own attribute refs (#2543).
    pub compositions: &'a [crate::resource::Composition],
    pub bindings: ResolvedBindings,
    pub current_states: HashMap<ResourceId, State>,
    /// The same provider normalizer that ran at plan time
    /// (`PlanPreprocessor`). Apply-time reference re-resolution rebuilds
    /// attributes from the un-normalized source, so the executor must
    /// re-apply this after each resolution and before constructing the
    /// provider request — otherwise plan-time normalization is silently
    /// undone (carina#3060).
    pub normalizer: &'a dyn ProviderNormalizer,
    /// Provider configs whose default_tags participate in desired-side
    /// normalization before provider patch construction.
    pub provider_configs: &'a [ProviderConfig],
    /// Provider factories, looked up per-resource by `id.provider`
    /// (same `find_factory` dispatch the plan path uses) to re-apply
    /// enum-alias resolution (`get_enum_alias_reverse`, e.g.
    /// `IpProtocol.all` → `"-1"`) on the apply path. carina#3063:
    /// plan-time pipeline stage 3 — like `normalize_desired`, undone by
    /// apply-time re-resolution. A multi-provider plan needs the slice,
    /// not a single factory.
    pub factories: &'a [Box<dyn crate::provider::ProviderFactory>],
    /// Schema registry used to re-apply `Union[String, list(String)]`
    /// canonicalization (`canonicalize_resources_with_schemas`) — plan
    /// pipeline stage 1, also undone by apply-time re-resolution
    /// (carina#3063).
    pub schemas: &'a crate::schema::SchemaRegistry,
    /// Maximum concurrent provider operations.
    pub parallelism: NonZeroUsize,
}

/// Result of executing a plan's effects.
pub struct ExecutionResult {
    pub success_count: usize,
    pub failure_count: usize,
    pub skip_count: usize,
    pub applied_states: HashMap<ResourceId, State>,
    pub successfully_deleted: HashSet<ResourceId>,
    pub permanent_name_overrides: HashMap<ResourceId, HashMap<String, String>>,
    pub current_states: HashMap<ResourceId, State>,
    pub failed_refreshes: HashSet<ResourceId>,
}

/// Progress information for effect execution.
#[derive(Debug, Clone, Copy)]
pub struct ProgressInfo {
    /// Number of effects completed so far (including this one).
    pub completed: usize,
    /// Total number of actionable effects (excluding Read).
    pub total: usize,
}

/// Events emitted during plan execution.
pub enum ExecutionEvent<'a> {
    /// An effect is waiting for dependencies to complete before it can start.
    Waiting {
        effect: &'a Effect,
        /// Binding names of the dependencies that have not yet completed.
        pending_dependencies: Vec<String>,
    },
    EffectStarted {
        effect: &'a Effect,
    },
    EffectSucceeded {
        effect: &'a Effect,
        state: Option<&'a State>,
        duration: Duration,
        progress: ProgressInfo,
    },
    EffectFailed {
        effect: &'a Effect,
        error: &'a str,
        duration: Duration,
        progress: ProgressInfo,
    },
    EffectSkipped {
        effect: &'a Effect,
        reason: &'a str,
        progress: ProgressInfo,
    },
    /// Heartbeat emitted while a wait poll loop is still alive.
    ///
    /// Emitted at `max(30s, interval * 5)` cadence with the elapsed time and
    /// last observed attributes so operators can see what the wait is reading.
    WaitPolling {
        binding: &'a str,
        target_id: &'a ResourceId,
        elapsed: Duration,
        last_attrs: &'a HashMap<String, Value>,
    },
    CascadeUpdateSucceeded {
        id: &'a ResourceId,
    },
    CascadeUpdateFailed {
        id: &'a ResourceId,
        error: &'a str,
    },
    RenameSucceeded {
        id: &'a ResourceId,
        from: &'a str,
        to: &'a str,
    },
    RenameFailed {
        id: &'a ResourceId,
        error: &'a str,
    },
    RefreshStarted,
    RefreshSucceeded {
        id: &'a ResourceId,
    },
    RefreshFailed {
        id: &'a ResourceId,
        error: &'a str,
    },
}

/// Observer trait for UI separation during plan execution.
///
/// Implementations must handle concurrent calls from parallel effect execution.
/// Use interior mutability (e.g., `Mutex`) if mutable state is needed.
pub trait ExecutionObserver: Send + Sync {
    fn on_event(&self, event: &ExecutionEvent);
}

/// Execute a plan by dispatching effects to a provider.
///
/// This function contains the core execution logic, including:
/// - Reference resolution via the canonical `ResolvedBindings` view
/// - 3-phase Replace ordering for interdependent replaces
/// - Binding map updates after each effect
/// - Failure propagation (failed_bindings)
/// - Dependency skip
/// - Pending state refreshes
pub async fn execute_plan(
    provider: &dyn Provider,
    mut input: ExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
) -> ExecutionResult {
    if has_interdependent_replaces(input.plan.effects()) {
        execute_effects_phased(provider, &mut input, observer).await
    } else {
        execute_effects_sequential(provider, &mut input, observer).await
    }
}

#[cfg(test)]
mod tests;
