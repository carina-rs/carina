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
//! - `deferred_dispatch`: DeferredCreate and DeferredReplace materialization/dispatch

pub(crate) mod basic;
mod deferred_dispatch;
pub mod normalized;
#[cfg(test)]
mod normalized_tests;
mod parallel;
pub(super) mod scheduler;
pub(crate) mod wait;

pub use crate::effect::deps::UnresolvedResource;

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::time::Duration;

use crate::binding_index::ResolvedBindings;
use crate::effect::Effect;
use crate::parser::ProviderConfig;
use crate::plan::NameOverride;
use crate::provider::{PartialReadDiagnostic, Provider, ProviderNormalizer};
use crate::resource::{ResolvedResource, Resource, ResourceId, State};
use crate::value::SerializationError;
use crate::wait::WaitObservation;

use parallel::execute_effects_sequential;
use tokio_util::sync::CancellationToken;

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
    pub partial_count: usize,
    pub partial_diagnostics: Vec<(ResourceId, PartialReadDiagnostic)>,
    pub skip_count: usize,
    pub applied_states: std::collections::HashMap<ResourceId, State>,
    pub runtime_synthesized_resources: Vec<Resource>,
    pub successfully_deleted: HashSet<ResourceId>,
    pub permanent_name_overrides: HashMap<ResourceId, HashMap<String, NameOverride>>,
    pub current_states: HashMap<ResourceId, State>,
    pub bindings: ResolvedBindings,
    pub failed_refreshes: HashSet<ResourceId>,
}

/// Outcome of executing a plan: either it ran to completion, or a
/// cancel request was observed and the run unwound after in-flight
/// effects finished. Both variants carry the `ExecutionResult` of
/// whatever the run produced; callers must destructure to decide
/// whether to surface an `Interrupted` error after persisting state.
///
/// The enum intentionally provides no `?` shortcut and no `From` /
/// `Into` to `Result<ExecutionResult, _>`. A future caller cannot
/// silently drop the `Cancelled` arm — the compiler forces explicit
/// handling. See [`ExecutionOutcomeCannotBeQuestionMarked`] for the
/// compile-fail evidence.
pub enum ExecutionOutcome {
    Completed(ExecutionResult),
    Cancelled(ExecutionResult),
}

/// Marker type whose `compile_fail` doctest is the type-safety evidence
/// that `ExecutionOutcome` cannot be `?`-ed into a `Result`. The marker
/// is hidden from rustdoc — it exists only so `cargo test --doc
/// ExecutionOutcomeCannotBeQuestionMarked` runs the guard.
///
/// ```compile_fail
/// use carina_core::executor::{ExecutionOutcome, ExecutionResult};
///
/// fn must_not_compile(outcome: ExecutionOutcome) -> Result<ExecutionResult, String> {
///     let result = outcome?;
///     Ok(result)
/// }
/// ```
#[doc(hidden)]
pub struct ExecutionOutcomeCannotBeQuestionMarked;

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
    EffectPartiallySucceeded {
        effect: &'a Effect,
        state: &'a State,
        diagnostic: &'a PartialReadDiagnostic,
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
        observation: WaitObservation<'a>,
        elapsed: Duration,
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
/// - Binding map updates after each effect
/// - Failure propagation (failed_bindings)
/// - Dependency skip
/// - Pending state refreshes
pub async fn execute_plan(
    provider: &dyn Provider,
    mut input: ExecutionInput<'_>,
    observer: &dyn ExecutionObserver,
    cancel: CancellationToken,
) -> ExecutionOutcome {
    let (result, was_cancelled) =
        execute_effects_sequential(provider, &mut input, observer, &cancel).await;
    if was_cancelled {
        ExecutionOutcome::Cancelled(result)
    } else {
        ExecutionOutcome::Completed(result)
    }
}

/// Prove an already-normalized desired resource is fully resolved before
/// direct provider dispatch outside the normal plan executor.
pub fn resolve_normalized_for_provider(
    resource: normalized::NormalizedResource,
) -> Result<ResolvedResource, SerializationError> {
    basic::resolved_normalized_resource(resource)
}

#[cfg(test)]
mod tests;
