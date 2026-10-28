//! Effect - Representing side effects as values
//!
//! An Effect describes "what to do" without actually performing the side effect.
//! Side effects only occur when they are executed via a Provider.

/// Dependency analysis over a slice of [`Effect`]s.
///
/// Centralizes the apply/destroy scheduling graph so the parallel scheduler,
/// the phased scheduler, and the destroy CLI driver all see the same edges.
pub mod deps;

use std::{
    collections::{BTreeSet, HashSet},
    ops::Deref,
};

use serde::{Deserialize, Serialize};

use crate::non_empty::NonEmptyVec;
use crate::parser::{DeferredForExpression, MovedAddresses, StateBlockAddress};
use crate::provider::AppliedIdentifier;
#[cfg(test)]
use crate::resource::{DataSource, DeferredValue, Resource, Value};
use crate::resource::{
    DeposedKey, Directives, ModuleSource, ResolvedDataSource, ResolvedResource, ResolvedResourceId,
    ResourceId, ResourceIdentity, SavedModuleSource, State,
};
use crate::wait::ManagedWaitTarget;
use crate::wait::predicate::WaitPredicate;

#[cfg(test)]
pub(crate) use crate::provider::fixture_applied_identifier;

#[cfg(test)]
fn fixture_managed_wait_target(id: ResourceId) -> ManagedWaitTarget {
    ManagedWaitTarget::new(&ResolvedResource::from_id(ResolvedResourceId::new(id)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanOp {
    Create,
    Read,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleEdge {
    /// This effect must wait for the resource identity to complete.
    DependsOn(ResourceIdentity),
    /// This effect must wait for the resource identity to complete, and the
    /// plan is invalid if no current effect owns that identity.
    RequiredDependsOn(ResourceIdentity),
    /// This effect must complete before the resource identity can proceed.
    BlockedBy(ResourceIdentity),
    /// This effect must complete before the resource identity can proceed, but
    /// only when that identity resolves to a delete effect.
    BlockedByIfDelete(ResourceIdentity),
}

/// Generation addressed by an effect.
///
/// Create and Update effects always address [`EffectGeneration::Current`].
/// Delete carries this explicitly so a delete of a deposed generation cannot
/// be mistaken for a delete of the current instance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EffectGeneration {
    #[default]
    Current,
    Deposed(DeposedKey),
}

impl EffectGeneration {
    pub fn is_current(&self) -> bool {
        matches!(self, Self::Current)
    }
}

/// Scheduler key for the exact instance generation an effect addresses.
///
/// This deliberately keys on user identity plus generation only. It is for
/// same-address scheduling relationships, not provider result accounting.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EffectInstanceKey {
    identity: ResourceIdentity,
    generation: EffectGeneration,
}

impl EffectInstanceKey {
    pub fn new(identity: ResourceIdentity, generation: EffectGeneration) -> Self {
        Self {
            identity,
            generation,
        }
    }

    pub fn current(identity: ResourceIdentity) -> Self {
        Self::new(identity, EffectGeneration::Current)
    }

    pub fn identity(&self) -> &ResourceIdentity {
        &self.identity
    }

    pub fn generation(&self) -> &EffectGeneration {
        &self.generation
    }
}

/// Delete-result key for the exact resource instance generation that was
/// destroyed.
///
/// Unlike [`EffectInstanceKey`], this carries the full [`ResourceId`] so result
/// accounting cannot conflate resources whose identity strings collide across
/// providers, types, or provider instances.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeletedInstanceKey {
    id: ResourceId,
    generation: EffectGeneration,
    identifier: String,
}

impl DeletedInstanceKey {
    pub fn new(
        id: ResourceId,
        generation: EffectGeneration,
        identifier: impl Into<String>,
    ) -> Self {
        Self {
            id,
            generation,
            identifier: identifier.into(),
        }
    }

    pub fn current(id: ResourceId, identifier: impl Into<String>) -> Self {
        Self::new(id, EffectGeneration::Current, identifier)
    }

    pub fn id(&self) -> &ResourceId {
        &self.id
    }

    pub fn generation(&self) -> &EffectGeneration {
        &self.generation
    }

    pub fn identifier(&self) -> &str {
        &self.identifier
    }
}

/// Temporary name used during create-before-destroy replacement.
///
/// When a resource with a unique name constraint is replaced with create-before-destroy,
/// the new resource is created with a temporary name to avoid conflicts with the old resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporaryName {
    /// The attribute that holds the name (e.g., "bucket_name")
    pub attribute: String,
    /// The original (desired) name value (e.g., "my-bucket")
    pub original_value: String,
    /// The generated temporary name (e.g., "my-bucket-a1b2c3d4")
    pub temporary_value: String,
    /// Whether the unique-name attribute can be updated after creation (not create-only)
    pub can_rename: bool,
}

/// Delete payload absorbed into [`Effect::DeferredReplace`].
///
/// This carries the current-generation delete fields from [`Effect::Delete`].
/// Keeping the delete half in a named struct lets the deletes slot grow new
/// fields later without re-shaping `DeferredReplace` itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeferredReplaceDelete {
    pub id: ResolvedResourceId,
    pub identifier: AppliedIdentifier,
    #[serde(default)]
    pub directives: Directives,
    #[serde(default)]
    pub binding: Option<String>,
    #[serde(default)]
    pub dependencies: HashSet<String>,
    #[serde(default)]
    pub explicit_dependencies: HashSet<String>,
    /// Consumer effects that must complete before this delete may run.
    ///
    /// Identities come from [`Effect::identity`] on consumer effects
    /// (typically `Update`, possibly `Replace` in future).
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub blocked_by_updates: HashSet<ResourceIdentity>,
}

impl DeferredReplaceDelete {
    pub fn to_delete_effect(&self) -> Effect {
        Effect::Delete {
            id: self.id.clone(),
            identifier: self.identifier.clone(),
            generation: EffectGeneration::Current,
            directives: self.directives.clone(),
            binding: self.binding.clone(),
            dependencies: self.dependencies.clone(),
            explicit_dependencies: self.explicit_dependencies.clone(),
            blocked_by_updates: self.blocked_by_updates.clone(),
        }
    }
}

/// Error returned when constructing [`NonEmptyDeletes`] from an empty vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyDeletes;

impl std::fmt::Display for EmptyDeletes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DeferredReplace requires at least one absorbed delete")
    }
}

impl std::error::Error for EmptyDeletes {}

/// Non-empty absorbed delete list for [`Effect::DeferredReplace`].
///
/// A DeferredReplace represents a known delete side paired with an apply-time
/// create side. An empty delete list would be a plain DeferredCreate, so this
/// wrapper makes that planner invariant explicit in the Effect model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    try_from = "Vec<DeferredReplaceDelete>",
    into = "Vec<DeferredReplaceDelete>"
)]
pub struct NonEmptyDeletes(NonEmptyVec<DeferredReplaceDelete>);

impl NonEmptyDeletes {
    /// Construct a non-empty absorbed delete list.
    pub fn try_new(deletes: Vec<DeferredReplaceDelete>) -> Result<Self, EmptyDeletes> {
        NonEmptyVec::from_vec(deletes).map(Self).ok_or(EmptyDeletes)
    }
}

impl Deref for NonEmptyDeletes {
    type Target = [DeferredReplaceDelete];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
    }
}

impl<'a> IntoIterator for &'a NonEmptyDeletes {
    type Item = &'a DeferredReplaceDelete;
    type IntoIter = std::slice::Iter<'a, DeferredReplaceDelete>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl TryFrom<Vec<DeferredReplaceDelete>> for NonEmptyDeletes {
    type Error = EmptyDeletes;

    fn try_from(deletes: Vec<DeferredReplaceDelete>) -> Result<Self, Self::Error> {
        Self::try_new(deletes)
    }
}

impl From<NonEmptyDeletes> for Vec<DeferredReplaceDelete> {
    fn from(deletes: NonEmptyDeletes) -> Self {
        deletes.0.into_vec()
    }
}

/// Payload for [`Effect::DeferredReplace`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "S: Serialize", deserialize = "S: Deserialize<'de>"))]
pub struct DeferredReplacePayload<S = ModuleSource> {
    /// Pre-apply iterations being destroyed.
    pub deletes: NonEmptyDeletes,
    /// Synthetic id used for plan-tree display and progress.
    pub id: ResolvedResourceId,
    /// The for-expression body, replayed against the upstream state.
    pub template: Box<DeferredForExpression<S>>,
}

fn deferred_replace_delete_dependencies(deletes: &[DeferredReplaceDelete]) -> BTreeSet<String> {
    deletes
        .iter()
        .flat_map(|delete| delete.dependencies.iter().cloned())
        .collect()
}

fn deferred_replace_delete_explicit_dependencies(
    deletes: &[DeferredReplaceDelete],
) -> HashSet<String> {
    deletes
        .iter()
        .flat_map(|delete| delete.explicit_dependencies.iter().cloned())
        .collect()
}

/// Non-empty create-only attribute list for replacement display metadata.
///
/// An empty list would render a destroy-and-recreate plan with no visible
/// reason: the unexplained-replacement bug class (carina#3471) this type makes
/// unrepresentable.
///
/// A replace must name at least one create-only attribute that forced it.
/// Constructing this type from a possibly empty `Vec<String>` requires the
/// fallible [`ChangedCreateOnly::new`] constructor:
///
/// ```compile_fail
/// use carina_core::effect::ChangedCreateOnly;
///
/// let attrs: Vec<String> = Vec::new();
/// let _changed = ChangedCreateOnly(attrs);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<String>", into = "Vec<String>")]
pub struct ChangedCreateOnly(NonEmptyVec<String>);

impl ChangedCreateOnly {
    pub fn new(attrs: Vec<String>) -> Option<Self> {
        NonEmptyVec::from_vec(attrs).map(Self)
    }

    pub fn push(&mut self, attr: String) {
        self.0.push(attr);
    }

    pub fn contains(&self, attr: &str) -> bool {
        self.0.iter().any(|a| a == attr)
    }
}

impl Deref for ChangedCreateOnly {
    type Target = [String];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
    }
}

impl TryFrom<Vec<String>> for ChangedCreateOnly {
    type Error = String;

    fn try_from(attrs: Vec<String>) -> Result<Self, Self::Error> {
        Self::new(attrs).ok_or_else(|| {
            "Replacement requires at least one changed create-only attribute".to_string()
        })
    }
}

impl From<ChangedCreateOnly> for Vec<String> {
    fn from(attrs: ChangedCreateOnly) -> Self {
        attrs.0.into_vec()
    }
}

/// Capability to rebind persisted state along a validated move edge.
///
/// The route pair is private: callers obtain this value from an
/// [`Effect::Move`] rather than supplying unchecked `from` and `to` ids to
/// the state-rebinding APIs.
#[derive(Debug, Clone, Copy)]
pub struct MoveAuthority<'a> {
    from: &'a ResolvedResourceId,
    to: &'a ResolvedResourceId,
}

impl MoveAuthority<'_> {
    pub fn from(&self) -> &ResourceId {
        self.from.as_inner()
    }

    pub fn to(&self) -> &ResourceId {
        self.to.as_inner()
    }

    /// Project the destination to the routing-free address that an in-flight
    /// continuation may adopt. Provider-instance routing is deliberately
    /// absent, so rebinding cannot rewrite the route that issued the create.
    pub(crate) fn destination_address(&self) -> StateBlockAddress {
        StateBlockAddress::new(
            self.to.provider().to_string(),
            self.to.resource_type().to_string(),
            self.to.identity().clone(),
        )
    }
}

/// A move whose source and destination identities have both crossed the
/// resolved-resource boundary. This is the pre-plan form used while state is
/// being transferred; it mints the same route authority as [`Effect::Move`]
/// without narrowing a general effect at runtime.
///
/// Arbitrary resolved IDs are not sufficient to mint move authority:
///
/// ```compile_fail
/// use carina_core::effect::ResolvedMove;
/// use carina_core::resource::{ResolvedResourceId, ResourceId};
///
/// fn forge(from: ResourceId, to: ResourceId) -> ResolvedMove {
///     ResolvedMove::new(
///         ResolvedResourceId::new(from),
///         ResolvedResourceId::new(to),
///     )
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMove {
    from: ResolvedResourceId,
    to: ResolvedResourceId,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "resolved move route {resolved_from} -> {resolved_to} does not match moved block {declared_from} -> {declared_to}"
)]
pub struct ResolvedMoveMismatch {
    resolved_from: Box<ResolvedResourceId>,
    resolved_to: Box<ResolvedResourceId>,
    declared_from: Box<StateBlockAddress>,
    declared_to: Box<StateBlockAddress>,
}

impl ResolvedMove {
    /// Validate routed source and destination ids against one parsed `moved`
    /// block, then mint the capability used by state-rebinding APIs.
    ///
    /// Provider-instance routing is deliberately inherited from the resolved
    /// ids because the DSL addresses do not carry it. Provider, resource type,
    /// and identity must agree with the block on both sides.
    pub fn from_moved_block(
        addresses: &MovedAddresses,
        from: ResolvedResourceId,
        to: ResolvedResourceId,
    ) -> Result<Self, ResolvedMoveMismatch> {
        let declared_from = addresses.from();
        let declared_to = addresses.to();
        if !Self::matches_address(&from, &declared_from)
            || !Self::matches_address(&to, &declared_to)
        {
            return Err(ResolvedMoveMismatch {
                resolved_from: Box::new(from),
                resolved_to: Box::new(to),
                declared_from: Box::new(declared_from),
                declared_to: Box::new(declared_to),
            });
        }
        Ok(Self { from, to })
    }

    pub(crate) fn from_detected_rename(from: ResolvedResourceId, to: ResolvedResourceId) -> Self {
        Self { from, to }
    }

    fn matches_address(resolved: &ResolvedResourceId, declared: &StateBlockAddress) -> bool {
        declared.matches_resource_id(resolved.as_inner())
    }

    pub fn from(&self) -> &ResolvedResourceId {
        &self.from
    }

    pub fn to(&self) -> &ResolvedResourceId {
        &self.to
    }

    pub fn authority(&self) -> MoveAuthority<'_> {
        MoveAuthority {
            from: &self.from,
            to: &self.to,
        }
    }

    pub fn into_parts(self) -> (ResolvedResourceId, ResolvedResourceId) {
        (self.from, self.to)
    }

    pub fn into_effect(self) -> Effect {
        Effect::Move {
            from: self.from,
            to: self.to,
        }
    }
}

/// Effect representing an operation on a resource
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "S: Serialize + Clone",
    deserialize = "S: Deserialize<'de>"
))]
pub enum EffectWithSource<S> {
    /// Read the current state of a resource (data source)
    Read { resource: ResolvedDataSource<S> },

    /// Create a new resource
    Create(ResolvedResource<S>),

    /// Resume a create continuation loaded from locked state. The token is
    /// deliberately absent from the serializable plan and is joined from
    /// `Current::CreateInFlight` immediately before provider dispatch.
    ResumeCreate {
        resource: ResolvedResource<S>,
        started_at: crate::provider::Timestamp,
    },

    /// Update an existing resource
    Update {
        from: Box<State>,
        /// Non-empty provider identifier validated while constructing the plan.
        identifier: AppliedIdentifier,
        to: ResolvedResource<S>,
        /// Attribute names that changed (including removed attributes)
        changed_attributes: Vec<String>,
    },

    /// Delete a resource
    Delete {
        id: ResolvedResourceId,
        identifier: AppliedIdentifier,
        #[serde(default, skip_serializing_if = "EffectGeneration::is_current")]
        generation: EffectGeneration,
        #[serde(default)]
        directives: Directives,
        /// The binding name of the deleted resource (for plan tree display)
        #[serde(default)]
        binding: Option<String>,
        /// Binding names this resource depended on (for plan tree display).
        /// Includes both value-reference and explicit `directives.depends_on`
        /// edges — the union the executor needs for ordering.
        #[serde(default)]
        dependencies: HashSet<String>,
        /// Subset of `dependencies` that came from
        /// `directives { depends_on = [...] }` rather than value
        /// references. Captured at Delete construction time because the
        /// originating resource is gone by the time the executor runs
        /// (#2871). Empty for legacy state files.
        #[serde(default)]
        explicit_dependencies: HashSet<String>,
        /// Consumer effects that must complete before this delete may run.
        ///
        /// Identities come from [`Effect::identity`] on consumer effects
        /// (typically `Update`, possibly `Replace` in future).
        #[serde(default, skip_serializing_if = "HashSet::is_empty")]
        blocked_by_updates: HashSet<ResourceIdentity>,
    },

    /// Import an existing resource into state (via provider read)
    Import {
        /// Target resource address
        id: ResolvedResourceId,
        /// Cloud provider identifier (e.g., `"vpc-0abc123def456"`).
        ///
        /// Carried as a [`Value`] so an interpolation like
        /// `"${upstream.attr}|literal"` whose `upstream.attr` is still
        /// deferred at plan-time remains a
        /// `Value::Deferred(DeferredValue::Interpolation)` for display
        /// rather than being silently substituted to empty (carina#3329).
        /// The executor calls `assert_fully_resolved` before passing the
        /// identifier to the provider, so by apply time this is always
        /// a `Value::Concrete(ConcreteValue::String)`.
        identifier: crate::resource::Value,
    },

    /// Remove a resource from state without destroying it
    Remove {
        /// Resource address to remove from state
        id: ResolvedResourceId,
    },

    /// Move/rename a resource in state without destroy/recreate
    Move {
        /// Old resource address
        from: ResolvedResourceId,
        /// New resource address
        to: ResolvedResourceId,
    },

    /// Wait for `target` to satisfy `until` by polling `read()`.
    ///
    /// Emitted by the differ for a `let <binding> = wait <target> { ... }`
    /// declaration. The executor (not the provider) drives the polling
    /// loop on top of the existing `Provider::read` trait method, so
    /// providers — including WASM plugins — need no contract change.
    ///
    /// Wait effects do **not** persist to `carina.state.json`; they are
    /// re-evaluated on every plan/apply. See
    /// `notes/specs/2026-05-09-wait-construct-design.md` §State file.
    Wait {
        /// The wait's own identity (e.g. `"cert_issued"`).
        identity: ResourceIdentity,
        /// Managed target selected by the differ (`wait cert { ... }` →
        /// `cert`'s resolved resource id).
        target_id: ManagedWaitTarget,
        /// Typed predicate evaluated against each `read()` snapshot.
        until: WaitPredicate,
        /// Surface form of the `until` expression as the user wrote it
        /// (e.g. `"cert.status == aws.acm.Certificate.Status.Issued"`).
        /// Carried so plan-display never re-stringifies the parsed AST —
        /// same pattern as replacement-display cascade reference hints.
        until_surface: String,
        /// Hard cap on total wait time. Resolved by the differ from the
        /// user-provided override or the target schema's default.
        #[serde(with = "crate::resource::duration_secs")]
        timeout: std::time::Duration,
        /// Poll cadence between `read()` calls. Resolved from the target
        /// schema's default; not user-visible in MVP.
        #[serde(with = "crate::resource::duration_secs")]
        interval: std::time::Duration,
        /// Additional bindings the wait must wait for before polling,
        /// declared via `depends_on = [...]` in the wait block. The
        /// scheduler treats these like `directives.depends_on` on a
        /// resource — extra ordering edges that aren't expressed via
        /// value references.
        #[serde(default)]
        explicit_dependencies: HashSet<String>,
    },

    /// Re-expand a `for opt in <upstream>.<collection> { ... }`
    /// expression against the post-apply upstream state, emitting
    /// fresh `Create` effects for the synthesised children.
    ///
    /// Emitted by the planner when the iterable's plan-time value is
    /// unresolved. State-only: does not call the provider.
    DeferredCreate {
        /// Synthetic id used for plan-tree display and progress.
        id: ResolvedResourceId,
        /// The for-expression body, replayed against the upstream state.
        template: Box<DeferredForExpression<S>>,
    },

    /// A deferred-for replacement whose delete side is known at plan time
    /// and whose create side is materialized after the upstream binding is
    /// available at apply time.
    DeferredReplace(Box<DeferredReplacePayload<S>>),
}

pub type Effect = EffectWithSource<ModuleSource>;
pub type SavedEffect = EffectWithSource<SavedModuleSource>;

impl From<Effect> for SavedEffect {
    fn from(effect: Effect) -> Self {
        match effect {
            Effect::Read { resource } => Self::Read {
                resource: resource.into(),
            },
            Effect::Create(resource) => Self::Create(resource.into()),
            Effect::ResumeCreate {
                resource,
                started_at,
            } => Self::ResumeCreate {
                resource: resource.into(),
                started_at,
            },
            Effect::Update {
                from,
                identifier,
                to,
                changed_attributes,
            } => Self::Update {
                from,
                identifier,
                to: to.into(),
                changed_attributes,
            },
            Effect::Delete {
                id,
                identifier,
                generation,
                directives,
                binding,
                dependencies,
                explicit_dependencies,
                blocked_by_updates,
            } => Self::Delete {
                id,
                identifier,
                generation,
                directives,
                binding,
                dependencies,
                explicit_dependencies,
                blocked_by_updates,
            },
            Effect::Import { id, identifier } => Self::Import { id, identifier },
            Effect::Remove { id } => Self::Remove { id },
            Effect::Move { from, to } => Self::Move { from, to },
            Effect::Wait {
                identity,
                target_id,
                until,
                until_surface,
                timeout,
                interval,
                explicit_dependencies,
            } => Self::Wait {
                identity,
                target_id,
                until,
                until_surface,
                timeout,
                interval,
                explicit_dependencies,
            },
            Effect::DeferredCreate { id, template } => Self::DeferredCreate {
                id,
                template: Box::new((*template).into()),
            },
            Effect::DeferredReplace(payload) => {
                let DeferredReplacePayload {
                    deletes,
                    id,
                    template,
                } = *payload;
                Self::DeferredReplace(Box::new(DeferredReplacePayload {
                    deletes,
                    id,
                    template: Box::new((*template).into()),
                }))
            }
        }
    }
}

impl EffectWithSource<SavedModuleSource> {
    pub fn into_live(self) -> Effect {
        match self {
            Self::Read { resource } => Effect::Read {
                resource: resource.into_live(),
            },
            Self::Create(resource) => Effect::Create(resource.into_live()),
            Self::ResumeCreate {
                resource,
                started_at,
            } => Effect::ResumeCreate {
                resource: resource.into_live(),
                started_at,
            },
            Self::Update {
                from,
                identifier,
                to,
                changed_attributes,
            } => Effect::Update {
                from,
                identifier,
                to: to.into_live(),
                changed_attributes,
            },
            Self::Delete {
                id,
                identifier,
                generation,
                directives,
                binding,
                dependencies,
                explicit_dependencies,
                blocked_by_updates,
            } => Effect::Delete {
                id,
                identifier,
                generation,
                directives,
                binding,
                dependencies,
                explicit_dependencies,
                blocked_by_updates,
            },
            Self::Import { id, identifier } => Effect::Import { id, identifier },
            Self::Remove { id } => Effect::Remove { id },
            Self::Move { from, to } => Effect::Move { from, to },
            Self::Wait {
                identity,
                target_id,
                until,
                until_surface,
                timeout,
                interval,
                explicit_dependencies,
            } => Effect::Wait {
                identity,
                target_id,
                until,
                until_surface,
                timeout,
                interval,
                explicit_dependencies,
            },
            Self::DeferredCreate { id, template } => Effect::DeferredCreate {
                id,
                template: Box::new((*template).into_live()),
            },
            Self::DeferredReplace(payload) => {
                let DeferredReplacePayload {
                    deletes,
                    id,
                    template,
                } = *payload;
                Effect::DeferredReplace(Box::new(DeferredReplacePayload {
                    deletes,
                    id,
                    template: Box::new((*template).into_live()),
                }))
            }
        }
    }
}

impl AsRef<Effect> for Effect {
    fn as_ref(&self) -> &Effect {
        self
    }
}

/// A type-level narrowing of [`Effect`] to the variants the basic
/// executor (`execute_basic_effect`) actually handles: `Create`,
/// `Update`, and `Delete`.
///
/// The basic executor was previously typed on `&Effect` and used a
/// `_ => unreachable!("execute_basic_effect called with non-basic
/// effect")` arm to reject `Read`/`Import`/`Remove`/`Move`/
/// `Wait`. Callers' filters were the *only* thing keeping non-basic
/// effects out; a single missed filter (#3164) panicked apply at
/// runtime and left the state lock acquired.
///
/// `BasicEffect` makes that contract live in the type system instead.
/// The only way to obtain one is [`Effect::as_basic`], which returns
/// `None` for every non-basic variant. `execute_basic_effect` takes
/// `BasicEffect<'a>` directly and exhaustively matches its three arms —
/// no `unreachable!()` is needed, and adding a new `Effect` variant
/// won't compile until the call sites decide whether the variant is
/// "basic" or routed elsewhere.
///
/// Variants borrow from the source `&'a Effect` so the basic executor
/// can still forward the original effect into `ExecutionEvent::*`
/// observer calls.
#[derive(Debug)]
pub enum BasicEffect<'a> {
    Create {
        effect: &'a Effect,
        resource: &'a ResolvedResource,
    },
    ResumeCreate {
        effect: &'a Effect,
        resource: &'a ResolvedResource,
    },
    Update {
        effect: &'a Effect,
        from: &'a State,
        identifier: &'a AppliedIdentifier,
        to: &'a ResolvedResource,
        changed_attributes: &'a [String],
    },
    Delete {
        effect: &'a Effect,
        id: &'a ResourceId,
        identifier: &'a AppliedIdentifier,
        generation: &'a EffectGeneration,
        directives: &'a Directives,
    },
}

impl<'a> BasicEffect<'a> {
    /// Returns the source `&Effect` this `BasicEffect` was narrowed
    /// from. Used by `execute_basic_effect` to forward the original
    /// effect to `ExecutionEvent::*` observer calls without storing it
    /// twice.
    pub fn as_effect(&self) -> &'a Effect {
        match *self {
            BasicEffect::Create { effect, .. }
            | BasicEffect::ResumeCreate { effect, .. }
            | BasicEffect::Update { effect, .. }
            | BasicEffect::Delete { effect, .. } => effect,
        }
    }
}

impl EffectWithSource<ModuleSource> {
    /// Mint route-rebinding authority only from a resolved move effect.
    pub fn move_authority(&self) -> Option<MoveAuthority<'_>> {
        match self {
            Self::Move { from, to } => Some(MoveAuthority { from, to }),
            _ => None,
        }
    }

    pub const fn replace_display_glyph(create_before_destroy: bool) -> &'static str {
        if create_before_destroy { "+/-" } else { "-/+" }
    }

    #[cfg(test)]
    pub fn all_display_glyphs() -> Vec<&'static str> {
        Self::display_glyph_effects()
            .iter()
            .map(Self::display_glyph)
            .collect()
    }

    #[cfg(test)]
    fn display_glyph_effects() -> Vec<Self> {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let id = ResourceId::with_identity("test", "x");

        macro_rules! effect_cases {
            ($(($effect:expr, $pattern:pat)),+ $(,)?) => {{
                let effects = vec![$($effect),+];

                fn _check_exhaustive(effect: &Effect) {
                    match effect {
                        $($pattern => {})+
                    }
                }

                for effect in &effects {
                    _check_exhaustive(effect);
                }
                assert_eq!(
                    effects.len(),
                    11,
                    "display_glyph_effects must list every Effect variant exactly once"
                );
                effects
            }};
        }

        effect_cases![
            (
                Effect::Read {
                    resource: ResolvedDataSource::new(DataSource::new("test", "x")),
                },
                Effect::Read { .. }
            ),
            (
                Effect::Create(ResolvedResource::new(Resource::new("test", "x"))),
                Effect::Create(_)
            ),
            (
                Effect::ResumeCreate {
                    resource: ResolvedResource::new(Resource::new("test", "x")),
                    started_at: crate::provider::Timestamp::try_from("2026-10-23T08:00:00Z")
                        .expect("valid timestamp"),
                },
                Effect::ResumeCreate { .. }
            ),
            (
                Effect::Update {
                    from: Box::new(
                        State::existing(id.clone(), Default::default()).with_identifier("x-1"),
                    ),
                    identifier: fixture_applied_identifier("x-1"),
                    to: ResolvedResource::new(Resource::new("test", "x")),
                    changed_attributes: vec![],
                },
                Effect::Update { .. }
            ),
            (
                Effect::Delete {
                    id: crate::resource::ResolvedResourceId::new(id.clone()),
                    identifier: fixture_applied_identifier("x-1"),
                    generation: EffectGeneration::Current,
                    directives: Directives::default(),
                    binding: None,
                    dependencies: HashSet::new(),
                    explicit_dependencies: HashSet::new(),
                    blocked_by_updates: HashSet::new(),
                },
                Effect::Delete { .. }
            ),
            (
                Effect::Import {
                    id: crate::resource::ResolvedResourceId::new(id.clone()),
                    identifier: Value::Concrete(ConcreteValue::String("x-1".into())),
                },
                Effect::Import { .. }
            ),
            (
                Effect::Remove {
                    id: crate::resource::ResolvedResourceId::new(id.clone())
                },
                Effect::Remove { .. }
            ),
            (
                Effect::Move {
                    from: crate::resource::ResolvedResourceId::new(id.clone()),
                    to: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                        "test", "y"
                    )),
                },
                Effect::Move { .. }
            ),
            (
                Effect::Wait {
                    identity: ResourceIdentity::new("w"),
                    target_id: fixture_managed_wait_target(id.clone()),
                    until: WaitPredicate::equals(
                        AttrPath::single("status"),
                        Value::Concrete(ConcreteValue::String("ready".into())),
                    )
                    .unwrap(),
                    until_surface: "status == 'ready'".to_string(),
                    timeout: Duration::from_secs(60),
                    interval: Duration::from_secs(1),
                    explicit_dependencies: HashSet::new(),
                },
                Effect::Wait { .. }
            ),
            (
                Effect::DeferredCreate {
                    id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                        "route53.Record",
                        "validation_records"
                    )),
                    template: Box::new(crate::parser::DeferredForExpression {
                        file: Some("main.crn".to_string()),
                        line: 12,
                        header: "for opt in cert.domain_validation_options".to_string(),
                        resource_type: "route53.Record".to_string(),
                        attributes: vec![],
                        binding_name: "validation_records".to_string(),
                        iterable: Value::resource_ref(
                            "cert",
                            "domain_validation_options",
                            Vec::new(),
                        ),
                        binding: crate::parser::ForBinding::Simple("opt".to_string()),
                        template_resource: Resource::new("route53.Record", "validation_records"),
                    }),
                },
                Effect::DeferredCreate { .. }
            ),
            (
                Effect::DeferredReplace(Box::new(DeferredReplacePayload {
                    deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                            "route53.Record",
                            "validation_records[0]"
                        )),
                        identifier: fixture_applied_identifier("record-0"),
                        directives: Directives::default(),
                        binding: Some("validation_records[0]".to_string()),
                        dependencies: HashSet::new(),
                        explicit_dependencies: HashSet::new(),
                        blocked_by_updates: HashSet::new(),
                    }])
                    .expect("test fixture has one delete"),
                    id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                        "route53.Record",
                        "validation_records"
                    )),
                    template: Box::new(crate::parser::DeferredForExpression {
                        file: Some("main.crn".to_string()),
                        line: 12,
                        header: "for opt in cert.domain_validation_options".to_string(),
                        resource_type: "route53.Record".to_string(),
                        attributes: vec![],
                        binding_name: "validation_records".to_string(),
                        iterable: Value::resource_ref(
                            "cert",
                            "domain_validation_options",
                            Vec::new(),
                        ),
                        binding: crate::parser::ForBinding::Simple("opt".to_string()),
                        template_resource: Resource::new("route53.Record", "validation_records"),
                    }),
                })),
                Effect::DeferredReplace(_)
            ),
        ]
    }

    /// Plain display glyph for this effect.
    ///
    /// Color and text styling stay in each UI sink; this method owns the
    /// operation-to-glyph mapping so CLI, TUI, and compact plan formatting
    /// cannot drift independently.
    pub fn display_glyph(&self) -> &'static str {
        match self {
            Effect::Create(_) => "+",
            Effect::ResumeCreate { .. } => "↻",
            Effect::Update { .. } => "~",
            Effect::Delete { .. } => "-",
            Effect::Read { .. } => "<=",
            Effect::Import { .. } => "<-",
            Effect::Remove { .. } => "~",
            Effect::Move { .. } => "->",
            Effect::Wait { .. } => ">",
            Effect::DeferredCreate { .. } => "+",
            Effect::DeferredReplace(_) => "+/-",
        }
    }

    /// Narrow this effect to a [`BasicEffect`] if it is one of the
    /// variants the basic executor handles (`Create`, `Update`,
    /// `Delete`). Returns `None` for `Read`, `Import`,
    /// `Remove`, `Move`, and `Wait` — those route through other
    /// executor paths or are state-only (applied by the CLI's
    /// `execute_state_only_effects` step).
    ///
    /// This is the *only* way to construct a `BasicEffect`, so the
    /// basic executor's "this is a Create/Update/Delete" contract
    /// lives in the type system rather than in caller-side filters.
    /// See [`BasicEffect`] for the rationale (#3164).
    ///
    /// Guard: `BasicEffect` has no `From<&Effect>` or other public
    /// constructor — callers must route through `as_basic()` and
    /// handle the `None` case. A bare `&Effect` does not coerce to
    /// `BasicEffect`:
    ///
    /// ```compile_fail
    /// use carina_core::effect::{BasicEffect, Effect};
    /// use carina_core::resource::{ResolvedResource, Resource};
    /// let effect = Effect::Create(ResolvedResource::new(Resource::new("test", "x")));
    /// // Was: a missed filter could pass a non-basic `&Effect` straight
    /// // into `execute_basic_effect` and trip `unreachable!()` at apply
    /// // time (carina#3164). The conversion no longer exists.
    /// let _: BasicEffect = (&effect).into();
    /// ```
    pub fn as_basic(&self) -> Option<BasicEffect<'_>> {
        match self {
            Effect::Create(resource) => Some(BasicEffect::Create {
                effect: self,
                resource,
            }),
            Effect::ResumeCreate {
                resource,
                started_at: _started_at,
            } => Some(BasicEffect::ResumeCreate {
                effect: self,
                resource,
            }),
            Effect::Update {
                from,
                identifier,
                to,
                changed_attributes,
            } => Some(BasicEffect::Update {
                effect: self,
                from,
                identifier,
                to,
                changed_attributes,
            }),
            Effect::Delete {
                id,
                identifier,
                generation,
                directives,
                ..
            } => Some(BasicEffect::Delete {
                effect: self,
                id,
                identifier,
                generation,
                directives,
            }),
            Effect::Read { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate { .. }
            | Effect::DeferredReplace(_) => None,
        }
    }

    /// Returns true iff this effect polls an external state and
    /// could in principle hang forever — meaning that if it is the
    /// only kind left in flight while no other effect can dispatch,
    /// the executor must intervene rather than wait.
    pub fn is_wait(&self) -> bool {
        matches!(self, Effect::Wait { .. })
    }

    /// Returns the kind of Effect as a string (for display)
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::Read { .. } => "read",
            Effect::Create(_) => "create",
            Effect::ResumeCreate { .. } => "resume_create",
            Effect::Update { .. } => "update",
            Effect::Delete { .. } => "delete",
            Effect::Import { .. } => "import",
            Effect::Remove { .. } => "remove",
            Effect::Move { .. } => "move",
            Effect::Wait { .. } => "wait",
            Effect::DeferredCreate { .. } => "deferred_create",
            Effect::DeferredReplace(_) => "deferred_replace",
        }
    }

    /// Returns whether this Effect causes a mutation
    pub fn is_mutating(&self) -> bool {
        match self {
            Effect::Read { .. } => false,
            Effect::Create(_) => true,
            Effect::ResumeCreate { .. } => true,
            Effect::Update { .. } => true,
            Effect::Delete { .. } => true,
            Effect::Import { .. } => true,
            Effect::Remove { .. } => true,
            Effect::Move { .. } => true,
            Effect::Wait { .. } => false,
            Effect::DeferredCreate { .. } => true,
            Effect::DeferredReplace(_) => true,
        }
    }

    /// Returns whether this is a state-only operation.
    pub fn is_state_operation(&self) -> bool {
        match self {
            Effect::Read { .. } => false,
            Effect::Create(_) => false,
            Effect::ResumeCreate { .. } => false,
            Effect::Update { .. } => false,
            Effect::Delete { .. } => false,
            Effect::Import { .. } => true,
            Effect::Remove { .. } => true,
            Effect::Move { .. } => true,
            Effect::Wait { .. } => false,
            Effect::DeferredCreate { .. } => false,
            Effect::DeferredReplace(_) => true,
        }
    }

    /// Effects that do not call the provider and do not directly mutate state,
    /// but produce new effects for the scheduler to dispatch.
    pub fn is_scheduler_meta(&self) -> bool {
        match self {
            Effect::Read { .. } => false,
            Effect::Create(_) => false,
            Effect::ResumeCreate { .. } => false,
            Effect::Update { .. } => false,
            Effect::Delete { .. } => false,
            Effect::Import { .. } => false,
            Effect::Remove { .. } => false,
            Effect::Move { .. } => false,
            Effect::Wait { .. } => false,
            Effect::DeferredCreate { .. } => true,
            Effect::DeferredReplace(_) => true,
        }
    }

    /// Returns the resource ID for this effect
    pub fn resource_id(&self) -> &ResourceId {
        match self {
            Effect::Read { resource } => &resource.id,
            Effect::Create(r) => &r.id,
            Effect::ResumeCreate { resource, .. } => &resource.id,
            Effect::Update { to, .. } => &to.id,
            Effect::Delete { id, .. } => id,
            Effect::Import { id, .. } => id,
            Effect::Remove { id, .. } => id,
            Effect::Move { to, .. } => to,
            Effect::Wait { target_id, .. } => target_id.id(),
            Effect::DeferredCreate { id, .. } => id,
            Effect::DeferredReplace(payload) => &payload.id,
        }
    }

    /// Returns the scheduler identity for this effect.
    ///
    /// `Effect::identity()` is total because all scheduler-bound payloads went
    /// through the `Resolved*` checked constructors after the resolver boundary.
    /// `Wait` is keyed by the wait effect's own identity, not by its target.
    pub fn identity(&self) -> ResourceIdentity {
        match self {
            Effect::Read { resource } => resource.identity().clone(),
            Effect::Create(resource) => resource.identity().clone(),
            Effect::ResumeCreate { resource, .. } => resource.identity().clone(),
            Effect::Update { to, .. } => to.identity().clone(),
            Effect::Delete { id, .. }
            | Effect::Import { id, .. }
            | Effect::Remove { id }
            | Effect::DeferredCreate { id, .. } => id.identity().clone(),
            Effect::Move { to, .. } => to.identity().clone(),
            Effect::Wait { identity, .. } => identity.clone(),
            Effect::DeferredReplace(payload) => payload.id.identity().clone(),
        }
    }

    /// Returns the exact instance generation this effect addresses.
    ///
    /// Non-delete effects always operate on the current generation. Delete
    /// effects carry their generation explicitly.
    pub fn instance_key(&self) -> EffectInstanceKey {
        match self {
            Effect::Delete { id, generation, .. } => {
                EffectInstanceKey::new(id.identity().clone(), generation.clone())
            }
            _ => EffectInstanceKey::current(self.identity()),
        }
    }

    /// Returns a read-only [`ResourceRef`](crate::parser::ResourceRef)
    /// view of the resource for this effect, if it has one. Delete,
    /// Import, Remove, Move, and Wait effects have no resource.
    ///
    /// carina#3181 / #3308: the underlying payloads are typestate
    /// structs — `Create`/`Update` carry a [`Resource`], `Read` carries
    /// a [`DataSource`]. Callers that need a concrete
    /// type match the variant directly; this helper covers the
    /// shared id/attributes/binding/dependency_bindings accessors
    /// through the borrowing `ResourceRef` enum.
    pub fn as_resource_ref(&self) -> Option<crate::parser::ResourceRef<'_>> {
        match self {
            Effect::Create(resource) => Some(crate::parser::ResourceRef::Resource(resource)),
            Effect::ResumeCreate { resource, .. } => {
                Some(crate::parser::ResourceRef::Resource(resource))
            }
            Effect::Update { to, .. } => Some(crate::parser::ResourceRef::Resource(to)),
            Effect::Read { resource } => Some(crate::parser::ResourceRef::DataSource(resource)),
            Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate { .. }
            | Effect::DeferredReplace(_) => None,
        }
    }

    /// Returns the binding name for this effect's resource, if it has one.
    pub fn binding_name(&self) -> Option<String> {
        match self {
            Effect::Read { resource } => resource.binding.clone(),
            Effect::Create(resource) => resource.binding.clone(),
            Effect::ResumeCreate { resource, .. } => resource.binding.clone(),
            Effect::Update { to, .. } => to.binding.clone(),
            Effect::Delete { binding, .. } => binding.clone(),
            Effect::Import { .. } => None,
            Effect::Remove { .. } => None,
            Effect::Move { .. } => None,
            Effect::Wait { identity, .. } => Some(identity.to_string()),
            // INVARIANT: DeferredCreate has no binding identity until the
            // upstream resolves at apply time (the iterable's cardinality is
            // unknown), so binding_name() is None. DeferredReplace already
            // owns the deletes' pre-apply bindings, so its template's
            // binding_name is the stable identity. A consumer iterating
            // `plan.effects().filter_map(|e| e.binding_name())` will see
            // DeferredReplaces and skip DeferredCreates — this is intentional.
            Effect::DeferredCreate { .. } => None,
            Effect::DeferredReplace(payload) => Some(payload.template.binding_name.clone()),
        }
    }

    /// Returns the ResourceIds whose state rows this effect requires be
    /// cleared after a successful apply.
    ///
    /// This exhaustive match is the writeback contract for every effect
    /// variant. Adding a new variant must answer what, if anything, it cleans
    /// up in state instead of relying on downstream `_ => {}` catch-alls.
    pub fn writeback_cleanup_ids(
        &self,
        successfully_deleted: &HashSet<DeletedInstanceKey>,
    ) -> Vec<ResolvedResourceId> {
        match self {
            Effect::Read { .. } => Vec::new(),
            Effect::Create(_) => Vec::new(),
            Effect::ResumeCreate { .. } => Vec::new(),
            Effect::Update { .. } => Vec::new(),
            Effect::Delete {
                id,
                identifier,
                generation: EffectGeneration::Current,
                ..
            } => {
                if successfully_deleted.contains(&DeletedInstanceKey::current(
                    id.clone().into_inner(),
                    identifier.to_string(),
                )) {
                    vec![id.clone()]
                } else {
                    Vec::new()
                }
            }
            Effect::Delete {
                generation: EffectGeneration::Deposed(_),
                ..
            } => Vec::new(),
            Effect::Import { .. } => Vec::new(),
            Effect::Remove { id } => vec![id.clone()],
            Effect::Move { from, .. } => vec![from.clone()],
            Effect::Wait { .. } => Vec::new(),
            Effect::DeferredCreate { .. } => Vec::new(),
            // Absorbed deletes are replacement outcomes, not ordinary
            // cleanups. Apply writeback classifies them exhaustively in
            // `classify_replacement_outcome`, where the create result is
            // available alongside the delete result.
            Effect::DeferredReplace(_) => Vec::new(),
        }
    }

    /// Returns the exact delete instances whose pre-apply attributes should
    /// render in strike-through deleted form in plan display.
    pub fn deleted_resource_attribute_keys(&self) -> Vec<DeletedInstanceKey> {
        match self {
            Effect::Read { .. } => Vec::new(),
            Effect::Create(_) => Vec::new(),
            Effect::ResumeCreate { .. } => Vec::new(),
            Effect::Update { .. } => Vec::new(),
            Effect::Delete {
                id,
                identifier,
                generation,
                ..
            } => {
                vec![DeletedInstanceKey::new(
                    id.clone().into_inner(),
                    generation.clone(),
                    identifier.to_string(),
                )]
            }
            Effect::Import { .. } => Vec::new(),
            Effect::Remove { .. } => Vec::new(),
            Effect::Move { .. } => Vec::new(),
            Effect::Wait { .. } => Vec::new(),
            Effect::DeferredCreate { .. } => Vec::new(),
            Effect::DeferredReplace(payload) => payload
                .deletes
                .iter()
                .map(|delete| {
                    DeletedInstanceKey::new(
                        delete.id.clone().into_inner(),
                        EffectGeneration::Current,
                        delete.identifier.to_string(),
                    )
                })
                .collect(),
        }
    }

    /// Returns the binding names this effect depends on **via explicit
    /// `directives { depends_on = [...] }` declarations**, as a snapshot
    /// (cloned).
    ///
    /// For variants carrying a `Resource` (Create, Update, Read), the
    /// answer is derived live from
    /// `resource.directives.depends_on`. For Delete the answer comes
    /// from a stored `explicit_dependencies` set captured by the differ
    /// at construction time, because the originating resource is gone
    /// by the time the executor runs (#2871).
    ///
    /// State-only effects (Import, Remove, Move) return an empty set —
    /// they are scheduling primitives, not resource-state operations.
    pub fn explicit_dependencies(&self) -> HashSet<String> {
        match self {
            Effect::Read { resource } => resource.directives.depends_on.iter().cloned().collect(),
            Effect::Create(resource) => resource.directives.depends_on.iter().cloned().collect(),
            Effect::ResumeCreate { resource, .. } => {
                resource.directives.depends_on.iter().cloned().collect()
            }
            Effect::Update { to, .. } => to.directives.depends_on.iter().cloned().collect(),
            Effect::Delete {
                explicit_dependencies,
                ..
            } => explicit_dependencies.clone(),
            Effect::Import { .. } => HashSet::new(),
            Effect::Remove { .. } => HashSet::new(),
            Effect::Move { .. } => HashSet::new(),
            Effect::Wait {
                explicit_dependencies,
                ..
            } => explicit_dependencies.clone(),
            Effect::DeferredCreate { .. } => HashSet::new(),
            Effect::DeferredReplace(payload) => {
                deferred_replace_delete_explicit_dependencies(&payload.deletes)
            }
        }
    }

    /// Bindings whose failure must prevent this effect from being dispatched.
    ///
    /// For [`Effect::Wait`] this is the target's resolved identity plus the wait's
    /// explicit dependencies, with the target binding first. For other
    /// resource-carrying variants this is value-reference bindings plus
    /// explicit dependencies. State-only effects that do not carry dependency
    /// metadata return an empty list.
    ///
    /// This concentrates the "what blocks me?" rule in one place so callers
    /// cannot forget Wait's implicit target binding.
    pub fn blocking_bindings(&self) -> Vec<String> {
        match self {
            Effect::Wait {
                target_id,
                explicit_dependencies,
                ..
            } => {
                let target_binding = target_id.id().identity_str();
                let mut out = Vec::with_capacity(1 + explicit_dependencies.len());
                out.push(target_binding.to_string());
                out.extend(
                    explicit_dependencies
                        .iter()
                        .filter(|dep| dep.as_str() != target_binding)
                        .cloned()
                        .collect::<BTreeSet<_>>(),
                );
                out
            }
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. } => {
                let mut deps = BTreeSet::new();
                if let Some(resource) = self.as_resource_ref() {
                    deps.extend(crate::deps::get_resource_value_ref_dependencies(resource));
                }
                deps.extend(self.explicit_dependencies());
                deps.into_iter().collect()
            }
            Effect::DeferredCreate { template, .. } => {
                template.root_bindings().into_iter().collect()
            }
            Effect::DeferredReplace(payload) => {
                let mut deps = deferred_replace_delete_dependencies(&payload.deletes);
                deps.extend(payload.template.root_bindings());
                deps.into_iter().collect()
            }
        }
    }

    /// Scheduling edges this effect contributes when running `apply`.
    ///
    /// Returned edges describe ordering constraints relative to other
    /// bindings; resolving them to concrete effect indices is the
    /// responsibility of [`crate::effect::deps::build_effect_dependency_analysis`].
    pub fn apply_edges(&self) -> Vec<ScheduleEdge> {
        match self {
            Effect::Delete {
                dependencies,
                blocked_by_updates,
                ..
            } => {
                let mut edges: Vec<ScheduleEdge> = dependencies
                    .iter()
                    .cloned()
                    .map(ResourceIdentity::new)
                    .map(ScheduleEdge::BlockedByIfDelete)
                    .collect();
                edges.extend(
                    blocked_by_updates
                        .iter()
                        .cloned()
                        .map(ScheduleEdge::RequiredDependsOn),
                );
                edges
            }
            Effect::Wait { .. } | Effect::DeferredCreate { .. } | Effect::DeferredReplace(_) => {
                self.blocking_bindings()
                    .into_iter()
                    .map(ResourceIdentity::new)
                    .map(ScheduleEdge::DependsOn)
                    .collect()
            }
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. } => Vec::new(),
        }
    }

    /// Scheduling edges this effect contributes when running `destroy`.
    ///
    /// Differs from [`Self::apply_edges`] in that meta effects (`Wait`,
    /// deferred variants) contribute no edges because they are not present
    /// in destroy plans.
    pub fn destroy_edges(&self) -> Vec<ScheduleEdge> {
        match self {
            Effect::Delete { dependencies, .. } => dependencies
                .iter()
                .cloned()
                .map(ResourceIdentity::new)
                .map(ScheduleEdge::BlockedBy)
                .collect(),
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate { .. }
            | Effect::DeferredReplace(_) => Vec::new(),
        }
    }
}

/// Render an [`Effect::Import`] identifier for plan display.
///
/// Three shapes matter:
///
/// 1. **Concrete `String` / `EnumIdentifier`**: print the bare
///    identifier text (no DSL string quoting — operators read this as
///    the cloud-API identifier, not as a DSL literal).
/// 2. **`Value::Deferred(Interpolation)`**: walk the parts and emit
///    each one inline — `Literal` segments verbatim, `Expr` segments
///    through [`crate::value::format_value_with_key`]. Concrete `Expr`
///    parts therefore render as bare text and a deferred upstream ref
///    renders as `(known after upstream apply: …)` *without* the
///    surrounding `${…}` syntax, so the operator sees the full
///    composite identifier exactly as it will look after apply with
///    the deferred slot called out.
/// 3. **Other deferred shapes**: fall through to
///    [`crate::value::format_value_with_key`].
///
/// Carina#3329.
pub fn format_import_identifier(identifier: &crate::resource::Value) -> String {
    use crate::resource::{ConcreteValue, DeferredValue, InterpolationPart, Value};
    match identifier {
        Value::Concrete(ConcreteValue::String(s)) => s.expose_plaintext().to_owned(),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => s.to_string(),
        Value::Deferred(DeferredValue::Interpolation(parts)) => {
            let mut out = String::new();
            for part in parts {
                match part {
                    InterpolationPart::Literal(s) => out.push_str(s.expose_plaintext()),
                    // Recurse so a nested `Value::Deferred(Interpolation)`
                    // produced by canonicalization stays unquoted and
                    // un-`${…}`-wrapped at every level. Falling through
                    // to `format_value_with_key` here would re-introduce
                    // the wrapping the outer level was designed to
                    // strip.
                    InterpolationPart::Expr(v) => out.push_str(&format_import_identifier(v)),
                }
            }
            out
        }
        other => crate::value::format_value_with_key(other, Some("id")),
    }
}

/// Resolve an [`Effect::Import`] identifier to the concrete string the
/// provider's `read()` needs, or return a structured error describing
/// which deferred segment prevented resolution.
///
/// Centralizing the check means a future apply-side caller cannot
/// silently ship a `Value::Deferred(…)` to the provider by rolling its
/// own incomplete `match`: the only public path from a `Value` import
/// identifier to a provider-ready `&str` goes through this helper.
/// Plan-time display still calls
/// [`crate::value::format_value_with_key`] on the same field, so a
/// deferred upstream-state ref renders as
/// `(known after upstream apply: …)` rather than being silently
/// substituted to empty — see carina#3329.
pub fn resolve_import_identifier(identifier: &crate::resource::Value) -> Result<&str, String> {
    use crate::resource::{ConcreteValue, DeferredValue, Value};
    match identifier {
        Value::Concrete(ConcreteValue::String(s)) => Ok(s.expose_plaintext()),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => Ok(s.as_str()),
        Value::Deferred(DeferredValue::Secret(inner)) => resolve_import_identifier(inner),
        other => Err(format!(
            "import identifier did not resolve to a concrete string at apply time \
             (got {}). A referenced upstream value has not been published yet — \
             apply the upstream stack first, then re-run apply here.",
            crate::value::format_value_with_key(other, Some("id"))
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const EFFECT_VARIANT_COUNT: usize = 10;

    fn deserialize_effect(json: &str) -> Effect {
        serde_json::from_str::<SavedEffect>(json)
            .expect("deserialize saved effect")
            .into_live()
    }

    fn deserialize_effect_value(value: serde_json::Value) -> Effect {
        serde_json::from_value::<SavedEffect>(value)
            .expect("deserialize saved effect")
            .into_live()
    }

    #[test]
    fn resolved_move_requires_routes_matching_the_moved_block() {
        let addresses = MovedAddresses::new("awscc", "s3.Bucket", "old", "new");
        let from = ResolvedResourceId::with_provider_identity(
            "awscc",
            "s3.Bucket",
            "old",
            Some("source".to_string()),
        );
        let to = ResolvedResourceId::with_provider_identity(
            "awscc",
            "s3.Bucket",
            "new",
            Some("destination".to_string()),
        );

        let validated = ResolvedMove::from_moved_block(&addresses, from.clone(), to)
            .expect("routing instances may differ while the declared addresses agree");
        assert_eq!(validated.from().provider_instance(), Some("source"));
        assert_eq!(validated.to().provider_instance(), Some("destination"));

        let wrong_to =
            ResolvedResourceId::with_provider_identity("awscc", "s3.Bucket", "another", None);
        assert!(ResolvedMove::from_moved_block(&addresses, from, wrong_to).is_err());
    }

    fn deferred_for_template() -> crate::parser::DeferredForExpression {
        crate::parser::DeferredForExpression {
            file: Some("main.crn".to_string()),
            line: 12,
            header: "for opt in cert.domain_validation_options".to_string(),
            resource_type: "route53.Record".to_string(),
            attributes: vec![],
            binding_name: "validation_records".to_string(),
            iterable: Value::resource_ref("cert", "domain_validation_options", Vec::new()),
            binding: crate::parser::ForBinding::Simple("opt".to_string()),
            template_resource: Resource::new("route53.Record", "validation_records"),
        }
    }

    fn deferred_create_effect() -> Effect {
        Effect::DeferredCreate {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.Record",
                "validation_records",
            )),
            template: Box::new(deferred_for_template()),
        }
    }

    fn deferred_replace_effect() -> Effect {
        let mut template = deferred_for_template();
        template.file = None;
        template.line = 0;
        Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.Record",
                    "validation_records[0]",
                )),
                identifier: fixture_applied_identifier("old-record-id"),
                directives: Directives::default(),
                binding: Some("validation_records[0]".to_string()),
                dependencies: HashSet::from(["cert".to_string()]),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            }])
            .expect("test fixture has one delete"),
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "validation_records",
            )),
            template: Box::new(template),
        }))
    }

    fn every_effect_variant() -> Vec<(&'static str, Effect)> {
        use crate::resource::{ConcreteValue, State, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let rid = ResourceId::with_identity("test", "x");
        vec![
            (
                "Read",
                Effect::Read {
                    resource: ResolvedDataSource::new(DataSource::new("test", "x")),
                },
            ),
            (
                "Create",
                Effect::Create(ResolvedResource::new(Resource::new("test", "x"))),
            ),
            (
                "Update",
                Effect::Update {
                    from: Box::new(
                        State::existing(rid.clone(), Default::default()).with_identifier("x-1"),
                    ),
                    identifier: fixture_applied_identifier("x-1"),
                    to: ResolvedResource::new(Resource::new("test", "x")),
                    changed_attributes: vec![],
                },
            ),
            (
                "Delete",
                Effect::Delete {
                    id: crate::resource::ResolvedResourceId::new(rid.clone()),
                    identifier: fixture_applied_identifier("x-1"),
                    generation: EffectGeneration::Current,
                    directives: Directives::default(),
                    binding: None,
                    dependencies: HashSet::new(),
                    explicit_dependencies: HashSet::new(),
                    blocked_by_updates: HashSet::new(),
                },
            ),
            (
                "Import",
                Effect::Import {
                    id: crate::resource::ResolvedResourceId::new(rid.clone()),
                    identifier: Value::Concrete(ConcreteValue::String("x-1".into())),
                },
            ),
            (
                "Remove",
                Effect::Remove {
                    id: crate::resource::ResolvedResourceId::new(rid.clone()),
                },
            ),
            (
                "Move",
                Effect::Move {
                    from: crate::resource::ResolvedResourceId::new(rid.clone()),
                    to: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                        "test", "y",
                    )),
                },
            ),
            (
                "Wait",
                Effect::Wait {
                    identity: ResourceIdentity::new("w"),
                    target_id: fixture_managed_wait_target(rid),
                    until: WaitPredicate::equals(
                        AttrPath::single("status"),
                        Value::Concrete(ConcreteValue::String("ready".into())),
                    )
                    .unwrap(),
                    until_surface: "status == 'ready'".to_string(),
                    timeout: Duration::from_secs(60),
                    interval: Duration::from_secs(1),
                    explicit_dependencies: HashSet::new(),
                },
            ),
            ("DeferredCreate", deferred_create_effect()),
            ("DeferredReplace", deferred_replace_effect()),
        ]
    }

    #[test]
    fn every_effect_variant_covers_all_effect_variants() {
        assert_eq!(every_effect_variant().len(), EFFECT_VARIANT_COUNT);
    }

    #[test]
    fn plan_op_supports_debug_eq_and_hash() {
        let op = PlanOp::Create;
        assert_eq!(op, PlanOp::Create);
        assert_eq!(format!("{op:?}"), "Create");

        let mut ops = HashSet::new();
        ops.insert(op);
        assert!(ops.contains(&PlanOp::Create));
    }

    #[test]
    fn format_import_identifier_recurses_into_nested_interpolation() {
        // Carina#3329 (round-2): if canonicalization or a future
        // resolver pass produces a `Value::Deferred(Interpolation)`
        // *inside* an `Expr` part of the outer interpolation, the
        // helper must stay in its inline-string mode at every level
        // rather than falling back to `format_value_with_key` (which
        // would wrap the nested value as a DSL string literal with
        // surrounding `"…"` quotes and `${…}` syntax).
        use crate::resource::{
            ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
        };
        let inner = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                UnknownReason::UpstreamAllExports {
                    binding: "u".into(),
                },
            ))),
            InterpolationPart::Literal("-suffix".into()),
        ]));
        let outer = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Literal("prefix-".into()),
            InterpolationPart::Expr(inner),
            InterpolationPart::Literal("-tail".into()),
        ]));
        let rendered = format_import_identifier(&outer);
        assert_eq!(
            rendered, "prefix-(known after upstream apply: u)-suffix-tail",
            "nested interpolation must render inline with no `${{…}}` wrapping or quoting"
        );
        // Sanity: a bare concrete still renders bare.
        assert_eq!(
            format_import_identifier(&Value::Concrete(ConcreteValue::String("plain".into()))),
            "plain"
        );
    }

    #[test]
    fn resolve_import_identifier_accepts_concrete_string() {
        use crate::resource::{ConcreteValue, DeferredValue, Value};
        let v = Value::Concrete(ConcreteValue::String("vpc-0abc".into()));
        assert_eq!(resolve_import_identifier(&v).unwrap(), "vpc-0abc");
        let secret = Value::Deferred(DeferredValue::Secret(Box::new(v)));
        assert_eq!(resolve_import_identifier(&secret).unwrap(), "vpc-0abc");
    }

    #[test]
    fn resolve_import_identifier_rejects_deferred_interpolation() {
        // carina#3329: an apply-time `Effect::Import.identifier` carrying
        // an unresolved interpolation must surface a structured error,
        // not be silently shipped to the provider as the
        // partially-substituted literal. Pre-#3329 the field was a
        // `String` so the same shape would land at the provider as
        // `|literal|literal` with no way to detect the dropped
        // interpolation; the helper now gates that path through the
        // `Value` type.
        use crate::resource::{
            ConcreteValue, DeferredValue, InterpolationPart, UnknownReason, Value,
        };
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                UnknownReason::UpstreamAllExports {
                    binding: "upstream".into(),
                },
            ))),
            InterpolationPart::Literal("|tail".into()),
        ]));
        let err = resolve_import_identifier(&v).unwrap_err();
        assert!(
            err.contains("did not resolve to a concrete string"),
            "unexpected error message: {err}"
        );
        // Sanity: still passes for a concrete String/EnumIdentifier.
        let s = Value::Concrete(ConcreteValue::enum_identifier("ENUM_X"));
        assert_eq!(resolve_import_identifier(&s).unwrap(), "ENUM_X");
    }

    #[test]
    fn deferred_create_blocking_bindings_is_upstream_only() {
        let effect = deferred_create_effect();
        assert_eq!(effect.blocking_bindings(), vec!["cert".to_string()]);
    }

    #[test]
    fn deferred_create_blocking_bindings_include_both_coalesce_operands() {
        let mut template = deferred_for_template();
        template.iterable = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::resource_ref("primary", "items", Vec::new())),
            default: Box::new(Value::resource_ref("fallback", "items", Vec::new())),
        });
        let effect = Effect::DeferredCreate {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.Record",
                "validation_records",
            )),
            template: Box::new(template),
        };

        assert_eq!(
            effect.blocking_bindings(),
            vec!["fallback".to_string(), "primary".to_string()],
        );
    }

    #[test]
    fn deferred_replace_blocking_bindings_includes_upstream_and_delete_deps() {
        let mut template = deferred_for_template();
        template.file = None;
        template.line = 0;
        let effect = Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.Record",
                    "validation_records[0]",
                )),
                identifier: fixture_applied_identifier("old-record-id"),
                directives: Directives::default(),
                binding: Some("validation_records[0]".to_string()),
                dependencies: HashSet::from(["foo".to_string()]),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            }])
            .expect("fixture has one delete"),
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "validation_records",
            )),
            template: Box::new(template),
        }));

        assert_eq!(
            effect.blocking_bindings(),
            vec!["cert".to_string(), "foo".to_string()]
        );
    }

    #[test]
    fn deferred_replace_explicit_dependencies_unions_delete_explicit_deps() {
        let mut template = deferred_for_template();
        template.file = None;
        template.line = 0;
        let effect = Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.Record",
                    "validation_records[0]",
                )),
                identifier: fixture_applied_identifier("old-record-id"),
                directives: Directives::default(),
                binding: Some("validation_records[0]".to_string()),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::from(["foo".to_string()]),
                blocked_by_updates: HashSet::new(),
            }])
            .expect("fixture has one delete"),
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "validation_records",
            )),
            template: Box::new(template),
        }));

        assert_eq!(
            effect.explicit_dependencies(),
            HashSet::from(["foo".to_string()])
        );
    }

    #[test]
    fn deferred_create_as_basic_returns_none() {
        let effect = deferred_create_effect();
        assert!(effect.as_basic().is_none());
    }

    #[test]
    fn deferred_create_resource_id_returns_synthetic_id() {
        let effect = deferred_create_effect();
        assert_eq!(
            effect.resource_id(),
            &ResourceId::with_identity("route53.Record", "validation_records")
        );
    }

    #[test]
    fn deferred_create_serde_roundtrip() {
        let original = deferred_create_effect();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded = deserialize_effect(&json);
        match decoded {
            Effect::DeferredCreate { template, .. } => {
                assert_eq!(template.file, None);
                assert_eq!(template.line, 0);
                assert_eq!(template.header, "for opt in cert.domain_validation_options");
                assert_eq!(template.resource_type, "route53.Record");
                assert_eq!(template.binding_name, "validation_records");
                assert_eq!(
                    template.reference_candidates(),
                    BTreeSet::from(["cert.domain_validation_options".to_string()])
                );
            }
            other => panic!("expected DeferredCreate, got {other:?}"),
        }
    }

    #[test]
    fn deferred_replace_serde_roundtrip() {
        let original = deferred_replace_effect();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded = deserialize_effect(&json);
        assert_eq!(decoded, original);
    }

    #[test]
    fn delete_generation_serde_roundtrip_preserves_deposed_generation() {
        let original = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "s3.Bucket",
                "old-bucket",
            )),
            identifier: fixture_applied_identifier("old-bucket"),
            generation: EffectGeneration::Deposed(DeposedKey::new_unique()),
            directives: Directives::default(),
            binding: Some("bucket".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };

        let json = serde_json::to_string(&original).expect("serialize");
        assert!(json.contains("\"generation\""), "got: {json}");
        let decoded = deserialize_effect(&json);

        assert_eq!(decoded, original);
    }

    #[test]
    fn delete_generation_defaults_to_current_for_pre_field_saved_plans() {
        let legacy = serde_json::json!({
            "Delete": {
                "id": {"provider": "aws", "resource_type": "s3.Bucket", "name": "b"},
                "identifier": "x",
                "directives": {},
                "binding": null,
                "dependencies": [],
                "explicit_dependencies": [],
                "blocked_by_updates": [],
            }
        });

        let effect = deserialize_effect_value(legacy);
        match effect {
            Effect::Delete { generation, .. } => {
                assert_eq!(generation, EffectGeneration::Current);
            }
            other => panic!("expected Delete, got {other:?}"),
        }
    }

    #[test]
    fn is_scheduler_meta_only_true_for_deferred_variants() {
        for (label, effect) in every_effect_variant() {
            assert_eq!(
                effect.is_scheduler_meta(),
                matches!(label, "DeferredCreate" | "DeferredReplace"),
                "{label} scheduler-meta classification mismatch",
            );
        }
    }

    #[test]
    fn is_state_operation_includes_state_only_variants() {
        for (label, effect) in every_effect_variant() {
            assert_eq!(
                effect.is_state_operation(),
                matches!(label, "Import" | "Remove" | "Move" | "DeferredReplace"),
                "{label} state-operation classification mismatch",
            );
        }
    }

    #[test]
    fn read_is_not_mutating() {
        let resource = DataSource::new("test", "example");
        let effect = Effect::Read {
            resource: ResolvedDataSource::new(resource),
        };
        assert!(!effect.is_mutating());
    }

    #[test]
    fn create_is_mutating() {
        let resource = Resource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Create(ResolvedResource::new(resource));
        assert!(effect.is_mutating());
    }

    #[test]
    fn resource_id_returns_correct_id() {
        let resource = DataSource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Read {
            resource: ResolvedDataSource::new(resource.clone()),
        };
        assert_eq!(effect.resource_id(), &resource.id);
    }

    #[test]
    fn resource_returns_some_for_create() {
        let resource = Resource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Create(ResolvedResource::new(resource.clone()));
        assert_eq!(effect.as_resource_ref().unwrap().id(), &resource.id);
    }

    #[test]
    fn resource_returns_none_for_delete() {
        let effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity("test", "a")),
            identifier: fixture_applied_identifier("id-123"),
            generation: EffectGeneration::Current,
            directives: Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };
        assert!(effect.as_resource_ref().is_none());
    }

    #[test]
    fn binding_name_returns_binding() {
        let resource = Resource::new("test", "my_binding").with_binding("my_binding");
        let effect = Effect::Create(ResolvedResource::new(resource));
        assert_eq!(effect.binding_name(), Some("my_binding".to_string()));
    }

    #[test]
    fn binding_name_returns_none_without_binding() {
        use crate::resource::{ConcreteValue, Value};
        let resource = Resource::new("test", "no_binding").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("test".into())),
        );
        let effect = Effect::Create(ResolvedResource::new(resource));
        assert_eq!(effect.binding_name(), None);
    }

    #[test]
    fn effect_serde_round_trip() {
        use crate::resource::{ConcreteValue, Value};
        use std::collections::HashMap;

        let effects = vec![
            Effect::Create(ResolvedResource::new(Resource::new(
                "s3.Bucket",
                "my-bucket",
            ))),
            Effect::Read {
                resource: ResolvedDataSource::new(DataSource::new("s3.Bucket", "existing")),
            },
            Effect::Update {
                from: Box::new(
                    State::existing(
                        ResourceId::with_identity("s3.Bucket", "my-bucket"),
                        HashMap::from([(
                            "versioning".to_string(),
                            Value::Concrete(ConcreteValue::String("Disabled".into())),
                        )]),
                    )
                    .with_identifier("my-bucket"),
                ),
                identifier: fixture_applied_identifier("my-bucket"),
                to: ResolvedResource::new(Resource::new("s3.Bucket", "my-bucket").with_attribute(
                    "versioning",
                    Value::Concrete(ConcreteValue::String("Enabled".into())),
                )),
                changed_attributes: vec!["versioning".to_string()],
            },
            Effect::Delete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "s3.Bucket",
                    "old-bucket",
                )),
                identifier: fixture_applied_identifier("old-bucket"),
                generation: EffectGeneration::Current,
                directives: Directives::default(),
                binding: None,
                dependencies: HashSet::new(),
                explicit_dependencies: std::collections::HashSet::new(),
                blocked_by_updates: HashSet::new(),
            },
        ];

        for effect in effects {
            let json = serde_json::to_string(&effect).unwrap();
            let deserialized = deserialize_effect(&json);
            assert_eq!(effect, deserialized, "Round-trip failed for {:?}", effect);
        }
    }

    #[test]
    fn changed_create_only_constructor_rejects_empty() {
        assert!(ChangedCreateOnly::new(Vec::new()).is_none());
        assert_eq!(
            &ChangedCreateOnly::new(vec!["cidr_block".to_string()]).unwrap()[..],
            ["cidr_block".to_string()]
        );
    }

    #[test]
    fn changed_create_only_push_preserves_non_empty() {
        let mut changed = ChangedCreateOnly::new(vec!["cidr_block".to_string()]).unwrap();
        changed.push("vpc_id".to_string());
        assert_eq!(
            &changed[..],
            ["cidr_block".to_string(), "vpc_id".to_string()]
        );
    }

    #[test]
    fn explicit_dependencies_derived_from_resource_directives() {
        use crate::resource::Directives;
        let mut bucket = Resource::new("s3.Bucket", "b");
        bucket.directives = Directives {
            depends_on: vec!["role".to_string(), "kms".to_string()],
            ..Directives::default()
        };
        let create = Effect::Create(ResolvedResource::new(bucket.clone()));
        let got = create.explicit_dependencies();
        assert!(got.contains("role") && got.contains("kms"), "got {:?}", got);
    }

    #[test]
    fn explicit_dependencies_for_delete_uses_stored_set() {
        let effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "s3.Bucket",
                "b",
            )),
            identifier: fixture_applied_identifier("x"),
            generation: EffectGeneration::Current,
            directives: Directives::default(),
            binding: Some("bucket".to_string()),
            dependencies: HashSet::from(["role".to_string(), "kms".to_string()]),
            explicit_dependencies: HashSet::from(["role".to_string()]),
            blocked_by_updates: HashSet::new(),
        };
        let got = effect.explicit_dependencies();
        assert!(got.contains("role"));
        assert!(!got.contains("kms"), "kms is value-ref-only; got {:?}", got);
    }

    #[test]
    fn explicit_dependencies_empty_for_state_only_effects() {
        let imp = Effect::Import {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "s3.Bucket",
                "b",
            )),
            identifier: crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
                "x".into(),
            )),
        };
        let rem = Effect::Remove {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "s3.Bucket",
                "b",
            )),
        };
        let mov = Effect::Move {
            from: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "s3.Bucket",
                "old",
            )),
            to: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "s3.Bucket",
                "new",
            )),
        };
        for e in [imp, rem, mov] {
            assert!(
                e.explicit_dependencies().is_empty(),
                "expected empty for state-only, got {:?}",
                e.explicit_dependencies()
            );
        }
    }

    #[test]
    fn delete_legacy_state_without_explicit_deps_deserialises_to_empty() {
        // Pre-#2871 state files have no `explicit_dependencies` field,
        // and Phase 2 state files may omit `blocked_by_updates`.
        // `#[serde(default)]` must populate them as empty HashSets so
        // round-tripping legacy state never fails.
        let legacy = serde_json::json!({
            "Delete": {
                "id": {"provider": "aws", "resource_type": "s3.Bucket", "name": "b"},
                "identifier": "x",
                "directives": {},
                "binding": null,
                "dependencies": ["role"],
            }
        });
        let effect = deserialize_effect_value(legacy);
        if let Effect::Delete {
            explicit_dependencies,
            blocked_by_updates,
            ..
        } = &effect
        {
            assert!(explicit_dependencies.is_empty());
            assert!(blocked_by_updates.is_empty());
        } else {
            panic!("expected Delete, got {:?}", effect);
        }
    }

    #[test]
    fn wait_variant_constructs() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let _ = Effect::Wait {
            identity: ResourceIdentity::new("cert_issued"),
            target_id: fixture_managed_wait_target(ResourceId::with_identity(
                "acm.Certificate",
                "cert",
            )),
            until: WaitPredicate::equals(
                AttrPath::single("status"),
                Value::Concrete(ConcreteValue::String("ISSUED".into())),
            )
            .unwrap(),
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(75 * 60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
    }

    #[test]
    fn wait_binding_name_returns_wait_binding() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let e = Effect::Wait {
            identity: ResourceIdentity::new("cert_issued"),
            target_id: fixture_managed_wait_target(ResourceId::with_identity(
                "acm.Certificate",
                "cert",
            )),
            until: WaitPredicate::equals(
                AttrPath::single("status"),
                Value::Concrete(ConcreteValue::String("ISSUED".into())),
            )
            .unwrap(),
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        assert_eq!(e.binding_name(), Some("cert_issued".to_string()));
    }

    #[test]
    fn wait_is_not_mutating() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let e = Effect::Wait {
            identity: ResourceIdentity::new("cert_issued"),
            target_id: fixture_managed_wait_target(ResourceId::with_identity(
                "acm.Certificate",
                "cert",
            )),
            until: WaitPredicate::equals(
                AttrPath::single("status"),
                Value::Concrete(ConcreteValue::String("ISSUED".into())),
            )
            .unwrap(),
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        assert!(!e.is_mutating());
        assert_eq!(e.kind(), "wait");
    }

    #[test]
    fn wait_serde_round_trip() {
        use crate::resource::{ConcreteValue, ResolvedResource, Resource, Value};
        use crate::wait::ManagedWaitTarget;
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let target = ResolvedResource::new(Resource::new("acm.Certificate", "cert"));
        let original = Effect::Wait {
            identity: ResourceIdentity::new("cert_issued"),
            target_id: ManagedWaitTarget::new(&target),
            until: WaitPredicate::equals(
                AttrPath::single("status"),
                Value::Concrete(ConcreteValue::String("ISSUED".into())),
            )
            .unwrap(),
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(4500),
            interval: Duration::from_secs(5),
            explicit_dependencies: std::collections::HashSet::new(),
        };
        let json = serde_json::to_string(&original).expect("serialize");
        // Duration must round-trip as plain integer seconds (matches the
        // project's "no { secs, nanos }" rule from #2824).
        assert!(
            json.contains("\"timeout\":4500"),
            "expected `\"timeout\":4500` in JSON, got: {}",
            json
        );
        assert!(json.contains("\"identity\":\"cert_issued\""), "got: {json}");
        assert!(!json.contains("target_schema_kind"), "got: {json}");
        assert!(json.contains("\"target_id\":"), "got: {json}");
        let wire: serde_json::Value = serde_json::from_str(&json).expect("decode JSON value");
        assert_eq!(
            wire["Wait"]["target_id"],
            serde_json::to_value(target.resolved_id()).expect("serialize resolved target id"),
            "ManagedWaitTarget must be transparent on the wire"
        );
        let decoded = deserialize_effect(&json);
        assert_eq!(decoded, original);
    }

    /// `as_basic()` must return `Some` for the three variants the
    /// basic executor handles, and `None` for every other variant.
    /// This is the carina#3164 type-level contract: filters used to
    /// be caller-side, and a missed filter (Move slipping into the
    /// basic executor) panicked apply.
    #[test]
    fn as_basic_narrows_to_create_update_and_delete_only() {
        use crate::resource::State as ResState;

        let rid = ResourceId::with_identity("test", "x");

        // Basic variants must narrow.
        let create = Effect::Create(ResolvedResource::new(Resource::new("test", "x")));
        assert!(matches!(
            create.as_basic(),
            Some(BasicEffect::Create { .. })
        ));

        let update = Effect::Update {
            from: Box::new(
                ResState::existing(rid.clone(), Default::default()).with_identifier("x-1"),
            ),
            identifier: fixture_applied_identifier("x-1"),
            to: ResolvedResource::new(Resource::new("test", "x")),
            changed_attributes: vec![],
        };
        assert!(matches!(
            update.as_basic(),
            Some(BasicEffect::Update { .. })
        ));

        let delete = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(rid.clone()),
            identifier: fixture_applied_identifier("x-1"),
            generation: EffectGeneration::Current,
            directives: Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };
        assert!(matches!(
            delete.as_basic(),
            Some(BasicEffect::Delete { .. })
        ));

        // Non-basic variants must not. If a new variant is added and
        // someone forgets to extend `as_basic`, the exhaustive match
        // inside `as_basic` is what catches it at compile time; this
        // test catches misclassification of an existing variant.
        let read = Effect::Read {
            resource: ResolvedDataSource::new(DataSource::new("test", "x")),
        };
        let import = Effect::Import {
            id: crate::resource::ResolvedResourceId::new(rid.clone()),
            identifier: crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
                "x-1".into(),
            )),
        };
        let remove = Effect::Remove {
            id: crate::resource::ResolvedResourceId::new(rid.clone()),
        };
        let mov = Effect::Move {
            from: crate::resource::ResolvedResourceId::new(rid.clone()),
            to: crate::resource::ResolvedResourceId::new(ResourceId::with_identity("test", "y")),
        };
        let wait = Effect::Wait {
            identity: ResourceIdentity::new("w"),
            target_id: fixture_managed_wait_target(rid.clone()),
            until: WaitPredicate::equals(
                crate::wait::predicate::AttrPath::single("status"),
                crate::resource::Value::Concrete(crate::resource::ConcreteValue::String(
                    "ready".into(),
                )),
            )
            .unwrap(),
            until_surface: "status == 'ready'".to_string(),
            timeout: std::time::Duration::from_secs(60),
            interval: std::time::Duration::from_secs(1),
            explicit_dependencies: HashSet::new(),
        };
        for (label, e) in [
            ("Read", read),
            ("Import", import),
            ("Remove", remove),
            ("Move", mov),
            ("Wait", wait),
        ] {
            assert!(
                e.as_basic().is_none(),
                "{label} must not narrow to BasicEffect"
            );
        }
    }
}
