//! Effect - Representing side effects as values
//!
//! An Effect describes "what to do" without actually performing the side effect.
//! Side effects only occur when they are executed via a Provider.

/// Dependency analysis over a slice of [`Effect`]s.
///
/// Centralizes the apply/destroy scheduling graph so the parallel scheduler,
/// the phased scheduler, and the destroy CLI driver all see the same edges.
pub mod deps;

#[cfg(test)]
use crate::resource::Resource;
use std::{
    collections::{BTreeSet, HashSet},
    ops::Deref,
};

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};

use crate::non_empty::NonEmptyVec;
use crate::parser::{
    CompiledDeferredForExpression as DeferredForExpression, MovedAddresses, StateBlock,
    StateBlockAddress,
};
use crate::provider::AppliedIdentifier;
#[cfg(test)]
use crate::resource::DataSource;
use crate::resource::{
    AbsoluteBindingAddress, DeposedKey, Directives, LoopElementOrigin, ModuleSource,
    PersistedDestroyDependencies, ResolvedDataSource, ResolvedDependencies, ResolvedResource,
    ResolvedResourceId, ResourceId, ResourceIdentity, SavedModuleSource, SchemaUnresolvedResource,
    State, Value,
};
use crate::schema::FieldName;
use crate::wait::ManagedWaitTarget;
use crate::wait::predicate::WaitPredicate;

#[cfg(test)]
pub(crate) use crate::provider::fixture_applied_identifier;

#[cfg(test)]
fn fixture_managed_wait_target(id: ResourceId) -> ManagedWaitTarget {
    ManagedWaitTarget::new(&ResolvedResource::from_id(ResolvedResourceId::new(id)))
}

#[cfg(test)]
fn absolute_binding(binding: &str) -> AbsoluteBindingAddress {
    crate::resource::RootScope::new().resolve(crate::resource::RelativeBindingName::new(binding))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanOp {
    Create,
    Read,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScheduleEdge {
    /// This effect must wait for the typed scheduler key to complete.
    DependsOn(SchedulerKey),
    /// This effect must wait for the resource identity to complete, and the
    /// plan is invalid if no current effect owns that identity.
    RequiredDependsOn(ResolvedResourceId),
    /// This effect must complete before the typed scheduler key can proceed.
    BlockedBy(SchedulerKey),
    /// This effect must complete before the bound resource can proceed, but
    /// only when that binding resolves to a delete effect.
    BlockedByIfDelete(ResolvedResourceId),
}

/// Equality key used by the scheduler's cross-phase failure propagation.
/// Binding addresses, provider resource identities, and unbound loop targets
/// stay distinct even when their display spellings happen to match.
pub(crate) type SchedulerKey = ResolvedResourceId;

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

/// Typed evidence that an import effect replaces the same full resource ID's
/// protected create-outcome-unknown current instance.
///
/// The private field prevents callers from turning an arbitrary
/// [`ResourceId`] into differ authorization. The only minting function is
/// [`EffectWithSource::import_replacing_outcome_unknown`], which returns this
/// witness together with the matching [`Effect::Import`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutcomeUnknownImportReplacement {
    id: ResolvedResourceId,
}

impl OutcomeUnknownImportReplacement {
    pub fn resource_id(&self) -> &ResourceId {
        self.id.as_inner()
    }
}

/// Scheduler key for the exact instance generation an effect addresses.
///
/// This deliberately keys on user identity plus generation only. It is for
/// same-address scheduling relationships, not provider result accounting.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EffectInstanceKey {
    id: ResolvedResourceId,
    generation: EffectGeneration,
}

impl EffectInstanceKey {
    pub fn new(id: ResolvedResourceId, generation: EffectGeneration) -> Self {
        Self { id, generation }
    }

    pub fn current(id: ResolvedResourceId) -> Self {
        Self::new(id, EffectGeneration::Current)
    }

    pub fn id(&self) -> &ResolvedResourceId {
        &self.id
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
    pub binding: Option<AbsoluteBindingAddress>,
    #[serde(
        default,
        deserialize_with = "crate::resource::deserialize_persisted_destroy_dependencies"
    )]
    pub dependencies: PersistedDestroyDependencies,
    #[serde(default)]
    pub explicit_dependencies: ResolvedDependencies,
    /// Consumer effects that must complete before this delete may run.
    ///
    /// Identities come from [`Effect::identity`] on consumer effects
    /// (typically `Update`, possibly `Replace` in future).
    #[serde(default, skip_serializing_if = "ResolvedDependencies::is_empty")]
    pub blocked_by_updates: ResolvedDependencies,
}

/// Destroy-ordering edges known to belong to an old resource generation.
///
/// The private field deliberately prevents a desired replacement resource's
/// dependencies from being substituted for the generation being deposed.
/// Non-empty values can only be recovered from refreshed prior state or a
/// planned Delete, both of which already carry projected state-row edges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriorInstanceDestroyDependencies(PersistedDestroyDependencies);

impl PriorInstanceDestroyDependencies {
    pub fn from_refreshed_state(state: &State) -> Self {
        Self(state.dependency_bindings.clone())
    }
    pub fn into_persisted(self) -> PersistedDestroyDependencies {
        self.0
    }
}

impl DeferredReplaceDelete {
    pub fn prior_instance_destroy_dependencies(&self) -> PriorInstanceDestroyDependencies {
        PriorInstanceDestroyDependencies(self.dependencies.clone())
    }

    pub fn to_delete_effect<S>(&self) -> Effect<S> {
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

/// Payload for [`Effect::DeferredCreate`].
///
/// The scheduler id is projected from `template` and exists only on the wire,
/// where deserialization validates it before discarding the duplicate value.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredCreatePayload<S = ModuleSource> {
    pub dependency_template: crate::resource::DeferredDependencyTemplate,
    /// The for-expression body, replayed against the upstream state.
    pub template: Box<DeferredForExpression<S>>,
}

impl<S> DeferredCreatePayload<S> {
    pub fn new(
        template: Box<DeferredForExpression<S>>,
        dependency_template: crate::resource::DeferredDependencyTemplate,
    ) -> Self {
        Self {
            template,
            dependency_template,
        }
    }

    pub fn id(&self) -> &ResolvedResourceId {
        self.template.deferred_effect_id()
    }
}

impl<S: Serialize + Clone> Serialize for DeferredCreatePayload<S> {
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("DeferredCreatePayload", 3)?;
        state.serialize_field("id", &self.template.deferred_effect_wire_id())?;
        state.serialize_field("template", &self.template)?;
        state.serialize_field("dependency_template", &self.dependency_template)?;
        state.end()
    }
}

impl<'de, S: serde::de::DeserializeOwned> Deserialize<'de> for DeferredCreatePayload<S> {
    fn deserialize<De>(deserializer: De) -> Result<Self, De::Error>
    where
        De: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(bound(deserialize = "S: serde::de::DeserializeOwned"))]
        struct Wire<S> {
            id: ResolvedResourceId,
            template: Box<DeferredForExpression<S>>,
            dependency_template: crate::resource::DeferredDependencyTemplate,
        }

        let Wire {
            id,
            template,
            dependency_template,
        } = Wire::deserialize(deserializer)?;
        validate_deferred_effect_id::<De::Error, S>(&id, &template)?;
        Ok(Self {
            template,
            dependency_template,
        })
    }
}

/// Payload for [`Effect::DeferredReplace`].
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredReplacePayload<S = ModuleSource> {
    pub dependency_template: crate::resource::DeferredDependencyTemplate,
    /// Pre-apply iterations being destroyed.
    pub deletes: NonEmptyDeletes,
    /// The for-expression body, replayed against the upstream state.
    pub template: Box<DeferredForExpression<S>>,
}

impl<S> DeferredReplacePayload<S> {
    pub fn id(&self) -> &ResolvedResourceId {
        self.template.deferred_effect_id()
    }
}

impl<S: Serialize + Clone> Serialize for DeferredReplacePayload<S> {
    fn serialize<Ser>(&self, serializer: Ser) -> Result<Ser::Ok, Ser::Error>
    where
        Ser: serde::Serializer,
    {
        let mut state = serializer.serialize_struct("DeferredReplacePayload", 4)?;
        state.serialize_field("deletes", &self.deletes)?;
        state.serialize_field("id", &self.template.deferred_effect_wire_id())?;
        state.serialize_field("template", &self.template)?;
        state.serialize_field("dependency_template", &self.dependency_template)?;
        state.end()
    }
}

impl<'de, S: serde::de::DeserializeOwned> Deserialize<'de> for DeferredReplacePayload<S> {
    fn deserialize<De>(deserializer: De) -> Result<Self, De::Error>
    where
        De: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(bound(deserialize = "S: serde::de::DeserializeOwned"))]
        struct Wire<S> {
            deletes: NonEmptyDeletes,
            id: ResolvedResourceId,
            template: Box<DeferredForExpression<S>>,
            dependency_template: crate::resource::DeferredDependencyTemplate,
        }

        let Wire {
            deletes,
            id,
            template,
            dependency_template,
        } = Wire::deserialize(deserializer)?;
        validate_deferred_effect_id::<De::Error, S>(&id, &template)?;
        Ok(Self {
            deletes,
            template,
            dependency_template,
        })
    }
}

fn validate_deferred_effect_id<E: serde::de::Error, S>(
    id: &ResolvedResourceId,
    template: &DeferredForExpression<S>,
) -> Result<(), E> {
    let expected = template.deferred_effect_id();
    if id.provider() != expected.provider() {
        return Err(E::custom(
            "Invalid saved plan: deferred effect provider does not match deferred template resource provider",
        ));
    }
    if id.resource_type() != expected.resource_type() {
        return Err(E::custom(
            "Invalid saved plan: deferred effect resource type does not match deferred template resource type",
        ));
    }
    if id.provider_instance() != expected.provider_instance() {
        return Err(E::custom(
            "Invalid saved plan: deferred effect provider instance does not match deferred template resource provider instance",
        ));
    }
    if id.identity_str() != template.target().scheduler_key() {
        return Err(E::custom(format!(
            "Invalid saved plan: deferred effect id does not match deferred target scheduler key `{}`",
            template.target().scheduler_key()
        )));
    }
    Ok(())
}

/// One keyed list entry released from Carina's authored-value projection.
///
/// `retained_value` is the provider value that remains after omission. It is
/// carried in the plan so every display frontend can explain the state-only
/// action without re-reading provider state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleasedListKey {
    key: String,
    retained_value: Value,
}

impl ReleasedListKey {
    pub fn new(key: impl Into<String>, retained_value: Value) -> Self {
        Self {
            key: key.into(),
            retained_value,
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn retained_value(&self) -> &Value {
        &self.retained_value
    }
}

/// Invalid payload for [`Effect::ReleaseListKeys`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidReleasedListKeys {
    #[error("a keyed partial-update release must contain at least one key")]
    Empty,
    #[error("a keyed partial-update release contains duplicate key '{0}'")]
    Duplicate(String),
}

/// Non-empty, duplicate-free keys released by one state-only effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<ReleasedListKey>", into = "Vec<ReleasedListKey>")]
pub struct ReleasedListKeys(NonEmptyVec<ReleasedListKey>);

impl ReleasedListKeys {
    pub fn try_new(keys: Vec<ReleasedListKey>) -> Result<Self, InvalidReleasedListKeys> {
        let mut seen = HashSet::new();
        for key in &keys {
            if !seen.insert(key.key.clone()) {
                return Err(InvalidReleasedListKeys::Duplicate(key.key.clone()));
            }
        }
        NonEmptyVec::from_vec(keys)
            .map(Self)
            .ok_or(InvalidReleasedListKeys::Empty)
    }
}

impl Deref for ReleasedListKeys {
    type Target = [ReleasedListKey];

    fn deref(&self) -> &Self::Target {
        self.0.as_slice()
    }
}

impl TryFrom<Vec<ReleasedListKey>> for ReleasedListKeys {
    type Error = InvalidReleasedListKeys;

    fn try_from(keys: Vec<ReleasedListKey>) -> Result<Self, Self::Error> {
        Self::try_new(keys)
    }
}

impl From<ReleasedListKeys> for Vec<ReleasedListKey> {
    fn from(keys: ReleasedListKeys) -> Self {
        keys.0.into_vec()
    }
}

fn deferred_replace_delete_dependencies(deletes: &[DeferredReplaceDelete]) -> ResolvedDependencies {
    deletes
        .iter()
        .flat_map(|delete| delete.dependencies.iter().cloned())
        .collect()
}

fn deferred_replace_delete_explicit_dependencies(
    deletes: &[DeferredReplaceDelete],
) -> ResolvedDependencies {
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

    /// Project the source to the routing-free address used when a saved plan
    /// restores a post-move snapshot before replaying this exact move edge.
    pub(crate) fn source_address(&self) -> StateBlockAddress {
        StateBlockAddress::new(
            self.from.provider().to_string(),
            self.from.resource_type().to_string(),
            self.from.identity().clone(),
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
    /// Persisted row that originally owned the state being moved.
    origin: ResolvedResourceId,
    /// Effective source address after any move already applied in this plan.
    projected_from: ResolvedResourceId,
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
        Ok(Self {
            origin: from.clone(),
            projected_from: from,
            to,
        })
    }

    /// Build an automatically detected move while retaining its persisted
    /// origin and current in-memory projection as separate addresses.
    pub(crate) fn from_detected_rename(
        origin: ResolvedResourceId,
        projected_from: ResolvedResourceId,
        to: ResolvedResourceId,
    ) -> Self {
        Self {
            origin,
            projected_from,
            to,
        }
    }

    fn matches_address(resolved: &ResolvedResourceId, declared: &StateBlockAddress) -> bool {
        declared.matches_resource_id(resolved.as_inner())
    }

    /// Persisted row that owns this move.
    ///
    /// Effects and saved move pairs use this address even when state has
    /// already been projected through an earlier move in the same plan.
    pub fn from(&self) -> &ResolvedResourceId {
        &self.origin
    }

    /// Effective source address from which in-memory state must be transferred.
    ///
    /// This differs from [`Self::from`] when a persisted row has already been
    /// projected through an earlier move in the same plan.
    pub fn projected_from(&self) -> &ResolvedResourceId {
        &self.projected_from
    }

    pub fn to(&self) -> &ResolvedResourceId {
        &self.to
    }

    pub fn authority(&self) -> MoveAuthority<'_> {
        MoveAuthority {
            from: &self.projected_from,
            to: &self.to,
        }
    }

    /// Consume the move as the persisted origin-to-final-destination pair.
    pub fn into_parts(self) -> (ResolvedResourceId, ResolvedResourceId) {
        (self.origin, self.to)
    }

    pub fn into_effect(self) -> Effect {
        Effect::Move {
            from: self.origin,
            to: self.to,
            origin: None,
        }
    }
}

/// Origin-to-destination pairs produced by resolved address reconciliation.
///
/// The private representation prevents migration callers from substituting a
/// loose tuple slice. Even the no-move case must come from either the
/// addressed move resolver or a complete saved plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMovePairs(Vec<(ResolvedResourceId, ResolvedResourceId)>);

/// Address resolver output; a caller cannot substitute a literal collection.
pub struct AddressedMoves(Vec<ResolvedExplicitMove>);

pub struct ResolvedExplicitMove {
    pub resolved_move: ResolvedMove,
    pub target_is_desired: bool,
}

impl AddressedMoves {
    /// Resolve every declared move, retaining destination membership beside
    /// the addresses checked against the declaration.
    pub fn resolve<E: From<ResolvedMoveMismatch>>(
        blocks: &[StateBlock],
        mut addresses: impl FnMut(
            &MovedAddresses,
        )
            -> Result<Option<(ResolvedResourceId, ResolvedResourceId, bool)>, E>,
    ) -> Result<Self, E> {
        let mut moves = Vec::new();
        for block in blocks {
            let StateBlock::Moved {
                addresses: declared,
            } = block
            else {
                continue;
            };
            if let Some((from, to, target_is_desired)) = addresses(declared)? {
                moves.push(ResolvedExplicitMove {
                    resolved_move: ResolvedMove::from_moved_block(declared, from, to)?,
                    target_is_desired,
                });
            }
        }
        Ok(Self(moves))
    }

    /// State refresh resolves the declarations under an address-preserving
    /// policy: no declared move is applied to the persisted state.
    pub fn preserve_addresses(blocks: &[StateBlock]) -> Self {
        Self::resolve(blocks, |_| Ok::<_, ResolvedMoveMismatch>(None))
            .expect("address preservation cannot produce a mismatched move")
    }

    pub fn iter(&self) -> impl Iterator<Item = &ResolvedExplicitMove> {
        self.0.iter()
    }
}

/// The complete move-effect scope of a plan. Only the plan can mint it.
pub struct SavedMoveEffects<'a, S> {
    effects: &'a [Effect<S>],
}

impl<'a, S> SavedMoveEffects<'a, S> {
    pub(crate) fn from_effects(effects: &'a [Effect<S>]) -> Self {
        Self { effects }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "resolved move {from} -> {to} conflicts with existing move {existing_from} -> {existing_to}"
)]
pub struct ResolvedMovePairConflict {
    from: Box<ResolvedResourceId>,
    to: Box<ResolvedResourceId>,
    existing_from: Box<ResolvedResourceId>,
    existing_to: Box<ResolvedResourceId>,
}

impl ResolvedMovePairConflict {
    pub fn into_parts(
        self,
    ) -> (
        ResolvedResourceId,
        ResolvedResourceId,
        ResolvedResourceId,
        ResolvedResourceId,
    ) {
        (*self.from, *self.to, *self.existing_from, *self.existing_to)
    }
}

impl ResolvedMovePairs {
    /// Consume the resolved moves emitted by address reconciliation.
    ///
    /// An empty literal does not prove that address resolution ran:
    ///
    /// ```compile_fail,E0308
    /// use carina_core::effect::ResolvedMovePairs;
    /// let _ = ResolvedMovePairs::from_addressed_moves(Vec::new());
    /// ```
    ///
    /// Address-preserving refresh obtains its empty scope from the resolver:
    ///
    /// ```
    /// use carina_core::{
    ///     config_loader::parse_directory,
    ///     effect::{AddressedMoves, ResolvedMovePairs},
    ///     module_resolver::resolve_modules_providerless,
    ///     parser::ProviderContext,
    ///     schema::SchemaRegistry,
    /// };
    /// let project = tempfile::tempdir().unwrap();
    /// std::fs::write(project.path().join("main.crn"), "").unwrap();
    /// let parsed = parse_directory(project.path(), &ProviderContext::default()).unwrap();
    /// let checks = carina_core::validation::deferred_types::TypeCheckSession::run(
    ///     &ProviderContext::default(),
    ///     |session| resolve_modules_providerless(parsed, project.path(), session)
    ///         .unwrap()
    ///         .into_expanded(&SchemaRegistry::new())
    ///         .unwrap(),
    /// );
    /// let mut diagnostics = Vec::new();
    /// let configuration = checks.publish(&mut diagnostics);
    /// assert!(diagnostics.is_empty());
    /// let addressed = AddressedMoves::preserve_addresses(&configuration.state_blocks);
    /// let pairs = ResolvedMovePairs::from_addressed_moves(addressed);
    /// assert!(pairs.is_empty());
    /// ```
    pub fn from_addressed_moves(moves: AddressedMoves) -> Self {
        Self(
            moves
                .0
                .into_iter()
                .map(|resolved| resolved.resolved_move.into_parts())
                .collect(),
        )
    }

    /// Recover the move scope recorded by a complete saved plan.
    ///
    /// An unwitnessed plan is not a move-effect scope:
    ///
    /// ```compile_fail,E0308
    /// use carina_core::{effect::ResolvedMovePairs, plan::Plan};
    /// let plan: Plan = Plan::new();
    /// let _ = ResolvedMovePairs::from_saved_plan(&plan);
    /// ```
    ///
    /// The plan supplies the witness for its complete move-effect set:
    ///
    /// ```
    /// use carina_core::{effect::ResolvedMovePairs, plan::Plan};
    /// let plan: Plan = Plan::new();
    /// let pairs = ResolvedMovePairs::from_saved_plan(plan.move_effects());
    /// assert!(pairs.is_empty());
    /// ```
    pub fn from_saved_plan<S>(moves: SavedMoveEffects<'_, S>) -> Self {
        Self(
            moves
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    EffectWithSource::Move { from, to, .. } => Some((from.clone(), to.clone())),
                    _ => None,
                })
                .collect(),
        )
    }

    pub fn as_slice(&self) -> &[(ResolvedResourceId, ResolvedResourceId)] {
        &self.0
    }

    pub fn iter(&self) -> impl Iterator<Item = &(ResolvedResourceId, ResolvedResourceId)> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn conflicting_move(
        &self,
        resolved_move: &ResolvedMove,
        compose_forward: bool,
    ) -> Option<&(ResolvedResourceId, ResolvedResourceId)> {
        self.0.iter().find(|(existing_from, existing_to)| {
            let continues_forward = existing_to == resolved_move.projected_from()
                && existing_from == resolved_move.from()
                && existing_from != resolved_move.to()
                && resolved_move.projected_from() != resolved_move.to();
            let overlaps = existing_from == resolved_move.from()
                || existing_from == resolved_move.projected_from()
                || existing_from == resolved_move.to()
                || existing_to == resolved_move.from()
                || existing_to == resolved_move.projected_from()
                || existing_to == resolved_move.to();
            overlaps && !(compose_forward && continues_forward)
        })
    }

    fn conflict(
        &self,
        resolved_move: &ResolvedMove,
        compose_forward: bool,
    ) -> Option<ResolvedMovePairConflict> {
        self.conflicting_move(resolved_move, compose_forward)
            .map(|(existing_from, existing_to)| ResolvedMovePairConflict {
                from: Box::new(resolved_move.from().clone()),
                to: Box::new(resolved_move.to().clone()),
                existing_from: Box::new(existing_from.clone()),
                existing_to: Box::new(existing_to.clone()),
            })
    }

    /// Compose a detected continuation onto its persisted origin, or append
    /// an independent resolved move.
    pub fn compose_forward(
        &mut self,
        resolved_move: ResolvedMove,
    ) -> Result<(), ResolvedMovePairConflict> {
        if let Some(conflict) = self.conflict(&resolved_move, true) {
            return Err(conflict);
        }
        if let Some((_origin, prior_target)) = self.0.iter_mut().find(|(origin, prior_target)| {
            prior_target == resolved_move.projected_from() && origin == resolved_move.from()
        }) {
            *prior_target = resolved_move.to().clone();
        } else {
            self.0.push(resolved_move.into_parts());
        }
        Ok(())
    }

    /// Append one independent resolved move, rejecting every address overlap.
    pub fn append_rejecting_overlap(
        &mut self,
        resolved_move: ResolvedMove,
    ) -> Result<(), ResolvedMovePairConflict> {
        if let Some(conflict) = self.conflict(&resolved_move, false) {
            return Err(conflict);
        }
        self.0.push(resolved_move.into_parts());
        Ok(())
    }
}

impl Deref for ResolvedMovePairs {
    type Target = [(ResolvedResourceId, ResolvedResourceId)];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

/// Effect representing an operation on a resource
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    serialize = "S: Serialize + Clone",
    deserialize = "S: serde::de::DeserializeOwned"
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
        binding: Option<AbsoluteBindingAddress>,
        /// Exact resource addresses this resource depended on.
        /// Includes both value-reference and explicit `directives.depends_on`
        /// edges — the union the executor needs for ordering.
        #[serde(
            default,
            deserialize_with = "crate::resource::deserialize_persisted_destroy_dependencies"
        )]
        dependencies: PersistedDestroyDependencies,
        /// Subset of `dependencies` that came from
        /// `directives { depends_on = [...] }` rather than value
        /// references. Captured at Delete construction time because the
        /// originating resource is gone by the time the executor runs
        /// (#2871). Empty for legacy state files.
        #[serde(default)]
        explicit_dependencies: ResolvedDependencies,
        /// Consumer effects that must complete before this delete may run.
        ///
        /// Addresses come from consumer effects
        /// (typically `Update`, possibly `Replace` in future).
        #[serde(default, skip_serializing_if = "ResolvedDependencies::is_empty")]
        blocked_by_updates: ResolvedDependencies,
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
        /// Authored unbound-loop element that owns the destination.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<LoopElementOrigin>,
    },

    /// Stop treating omitted keys of a keyed partial-update list as authored.
    ///
    /// This effect is applied only to locked local state. It is deliberately
    /// excluded from [`BasicEffect`] so it cannot enter a provider call path.
    ReleaseListKeys {
        id: ResolvedResourceId,
        attribute: String,
        key_field: FieldName,
        keys: ReleasedListKeys,
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
        id: ResolvedResourceId,
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
        explicit_dependencies: ResolvedDependencies,
    },

    /// Re-expand a `for opt in <upstream>.<collection> { ... }`
    /// expression against the post-apply upstream state, emitting
    /// fresh `Create` effects for the synthesised children.
    ///
    /// Emitted by the planner when the iterable's plan-time value is
    /// unresolved. State-only: does not call the provider.
    DeferredCreate(DeferredCreatePayload<S>),

    /// A deferred-for replacement whose delete side is known at plan time
    /// and whose create side is materialized after the upstream binding is
    /// available at apply time.
    DeferredReplace(Box<DeferredReplacePayload<S>>),
}

pub type Effect<S = ModuleSource> = EffectWithSource<S>;
pub type SavedEffect = EffectWithSource<SavedModuleSource>;

/// Borrowed view of a graph-resolved resource carried by an effect.
pub type EffectResourceRef<'a, S = ModuleSource> = crate::parser::ResourceRef<
    'a,
    S,
    crate::resource::NoPlaceholder,
    crate::resource::LoopSlot,
    crate::parser::ResolvedDeferredForPhase<
        crate::resource::NoPlaceholder,
        crate::resource::LoopSlot,
    >,
    AbsoluteBindingAddress,
    (),
>;

/// Saved-plan effect payload before authored resource directives have been
/// resolved against the fingerprint-checked provider schemas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UnresolvedSavedEffect {
    Read {
        resource: ResolvedDataSource<SavedModuleSource>,
    },
    Create(SchemaUnresolvedResource<SavedModuleSource>),
    ResumeCreate {
        resource: SchemaUnresolvedResource<SavedModuleSource>,
        started_at: crate::provider::Timestamp,
    },
    Update {
        from: Box<State>,
        identifier: AppliedIdentifier,
        to: SchemaUnresolvedResource<SavedModuleSource>,
        changed_attributes: Vec<String>,
    },
    Delete {
        id: ResolvedResourceId,
        identifier: AppliedIdentifier,
        #[serde(default, skip_serializing_if = "EffectGeneration::is_current")]
        generation: EffectGeneration,
        #[serde(default)]
        directives: Directives,
        #[serde(default)]
        binding: Option<AbsoluteBindingAddress>,
        #[serde(
            default,
            deserialize_with = "crate::resource::deserialize_persisted_destroy_dependencies"
        )]
        dependencies: PersistedDestroyDependencies,
        #[serde(default)]
        explicit_dependencies: ResolvedDependencies,
        #[serde(default, skip_serializing_if = "ResolvedDependencies::is_empty")]
        blocked_by_updates: ResolvedDependencies,
    },
    Import {
        id: ResolvedResourceId,
        identifier: crate::resource::Value,
    },
    Remove {
        id: ResolvedResourceId,
    },
    Move {
        from: ResolvedResourceId,
        to: ResolvedResourceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<LoopElementOrigin>,
    },
    ReleaseListKeys {
        id: ResolvedResourceId,
        attribute: String,
        key_field: FieldName,
        keys: ReleasedListKeys,
    },
    Wait {
        id: ResolvedResourceId,
        target_id: ManagedWaitTarget,
        until: WaitPredicate,
        until_surface: String,
        #[serde(with = "crate::resource::duration_secs")]
        timeout: std::time::Duration,
        #[serde(with = "crate::resource::duration_secs")]
        interval: std::time::Duration,
        #[serde(default)]
        explicit_dependencies: ResolvedDependencies,
    },
    DeferredCreate(DeferredCreatePayload<SavedModuleSource>),
    DeferredReplace(Box<DeferredReplacePayload<SavedModuleSource>>),
}

impl From<SavedEffect> for UnresolvedSavedEffect {
    fn from(effect: SavedEffect) -> Self {
        match effect {
            Effect::Read { resource } => Self::Read { resource },
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
            Effect::Move { from, to, origin } => Self::Move { from, to, origin },
            Effect::ReleaseListKeys {
                id,
                attribute,
                key_field,
                keys,
            } => Self::ReleaseListKeys {
                id,
                attribute,
                key_field,
                keys,
            },
            Effect::Wait {
                id,
                target_id,
                until,
                until_surface,
                timeout,
                interval,
                explicit_dependencies,
            } => Self::Wait {
                id,
                target_id,
                until,
                until_surface,
                timeout,
                interval,
                explicit_dependencies,
            },
            Effect::DeferredCreate(payload) => Self::DeferredCreate(payload),
            Effect::DeferredReplace(payload) => Self::DeferredReplace(payload),
        }
    }
}

impl UnresolvedSavedEffect {
    pub fn move_authority(&self) -> Option<MoveAuthority<'_>> {
        match self {
            Self::Move { from, to, .. } => Some(MoveAuthority { from, to }),
            Self::Read { .. }
            | Self::Create(_)
            | Self::ResumeCreate { .. }
            | Self::Update { .. }
            | Self::Delete { .. }
            | Self::Import { .. }
            | Self::Remove { .. }
            | Self::ReleaseListKeys { .. }
            | Self::Wait { .. }
            | Self::DeferredCreate(_)
            | Self::DeferredReplace(_) => None,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Create(_) => "create",
            Self::ResumeCreate { .. } => "resume_create",
            Self::Update { .. } => "update",
            Self::Delete { .. } => "delete",
            Self::Import { .. } => "import",
            Self::Remove { .. } => "remove",
            Self::Move { .. } => "move",
            Self::ReleaseListKeys { .. } => "release_list_keys",
            Self::Wait { .. } => "wait",
            Self::DeferredCreate(_) => "deferred_create",
            Self::DeferredReplace(_) => "deferred_replace",
        }
    }

    /// Returns the effect's resource ID. For a bound deferred loop this uses
    /// the derived template identity (`children[?]`), not the bare binding
    /// spelling retained only by the legacy v36 JSON projection.
    pub fn resource_id(&self) -> &ResourceId {
        match self {
            Self::Read { resource } => &resource.id,
            Self::Create(resource) | Self::ResumeCreate { resource, .. } => resource.id(),
            Self::Update { to, .. } => to.id(),
            Self::Delete { id, .. }
            | Self::Import { id, .. }
            | Self::Remove { id }
            | Self::Move { to: id, .. }
            | Self::ReleaseListKeys { id, .. } => id,
            Self::DeferredCreate(payload) => payload.id(),
            Self::Wait { target_id, .. } => target_id.id(),
            Self::DeferredReplace(payload) => payload.id(),
        }
    }

    pub fn resolve_with_schemas(
        self,
        schemas: &crate::schema::SchemaRegistry,
    ) -> Result<SavedEffect, crate::resource::ResolvedResourceError> {
        Ok(match self {
            Self::Read { resource } => Effect::Read { resource },
            Self::Create(resource) => Effect::Create(resource.resolve_with_schemas(schemas)?),
            Self::ResumeCreate {
                resource,
                started_at,
            } => Effect::ResumeCreate {
                resource: resource.resolve_with_schemas(schemas)?,
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
                to: to.resolve_with_schemas(schemas)?,
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
            Self::Move { from, to, origin } => Effect::Move { from, to, origin },
            Self::ReleaseListKeys {
                id,
                attribute,
                key_field,
                keys,
            } => Effect::ReleaseListKeys {
                id,
                attribute,
                key_field,
                keys,
            },
            Self::Wait {
                id,
                target_id,
                until,
                until_surface,
                timeout,
                interval,
                explicit_dependencies,
            } => Effect::Wait {
                id,
                target_id,
                until,
                until_surface,
                timeout,
                interval,
                explicit_dependencies,
            },
            Self::DeferredCreate(mut payload) => {
                payload.template.resolve_template_ignore_changes(schemas)?;
                Effect::DeferredCreate(payload)
            }
            Self::DeferredReplace(mut payload) => {
                payload.template.resolve_template_ignore_changes(schemas)?;
                Effect::DeferredReplace(payload)
            }
        })
    }
}

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
            Effect::Move { from, to, origin } => Self::Move { from, to, origin },
            Effect::ReleaseListKeys {
                id,
                attribute,
                key_field,
                keys,
            } => Self::ReleaseListKeys {
                id,
                attribute,
                key_field,
                keys,
            },
            Effect::Wait {
                id,
                target_id,
                until,
                until_surface,
                timeout,
                interval,
                explicit_dependencies,
            } => Self::Wait {
                id,
                target_id,
                until,
                until_surface,
                timeout,
                interval,
                explicit_dependencies,
            },
            Effect::DeferredCreate(payload) => Self::DeferredCreate(DeferredCreatePayload {
                dependency_template: payload.dependency_template,
                template: Box::new((*payload.template).into()),
            }),
            Effect::DeferredReplace(payload) => {
                let DeferredReplacePayload {
                    deletes,
                    template,
                    dependency_template,
                } = *payload;
                Self::DeferredReplace(Box::new(DeferredReplacePayload {
                    deletes,
                    dependency_template,
                    template: Box::new((*template).into()),
                }))
            }
        }
    }
}

impl<S> AsRef<EffectWithSource<S>> for EffectWithSource<S> {
    fn as_ref(&self) -> &EffectWithSource<S> {
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
/// `ReleaseListKeys`/`Wait`. Callers' filters were the *only* thing keeping non-basic
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
pub enum BasicEffect<'a, S = ModuleSource> {
    Create {
        effect: &'a EffectWithSource<S>,
        resource: &'a ResolvedResource<S>,
    },
    ResumeCreate {
        effect: &'a EffectWithSource<S>,
        resource: &'a ResolvedResource<S>,
    },
    Update {
        effect: &'a EffectWithSource<S>,
        from: &'a State,
        identifier: &'a AppliedIdentifier,
        to: &'a ResolvedResource<S>,
        changed_attributes: &'a [String],
    },
    Delete {
        effect: &'a EffectWithSource<S>,
        id: &'a ResourceId,
        identifier: &'a AppliedIdentifier,
        generation: &'a EffectGeneration,
        directives: &'a Directives,
    },
}

/// Type-level capability for applying a keyed-list release to locked state.
///
/// The fields are private and this value can only be minted by
/// [`Effect::as_release_list_keys`]. State writeback accepts this type rather
/// than `&Effect`, while provider executors accept [`BasicEffect`]; the two
/// paths are disjoint by construction.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseListKeysEffect<'a> {
    id: &'a ResolvedResourceId,
    attribute: &'a str,
    key_field: &'a FieldName,
    keys: &'a ReleasedListKeys,
}

impl<'a> ReleaseListKeysEffect<'a> {
    pub fn id(self) -> &'a ResolvedResourceId {
        self.id
    }

    pub fn attribute(self) -> &'a str {
        self.attribute
    }

    pub fn keys(self) -> &'a ReleasedListKeys {
        self.keys
    }

    pub fn key_field(self) -> &'a FieldName {
        self.key_field
    }
}

impl<'a, S> BasicEffect<'a, S> {
    /// Returns the source `&Effect` this `BasicEffect` was narrowed
    /// from. Used by `execute_basic_effect` to forward the original
    /// effect to `ExecutionEvent::*` observer calls without storing it
    /// twice.
    pub fn as_effect(&self) -> &'a EffectWithSource<S> {
        match *self {
            BasicEffect::Create { effect, .. }
            | BasicEffect::ResumeCreate { effect, .. }
            | BasicEffect::Update { effect, .. }
            | BasicEffect::Delete { effect, .. } => effect,
        }
    }
}

impl<S> EffectWithSource<S> {
    /// Recover the projected destroy dependencies carried by a Delete's old
    /// generation. Other effect variants have no prior instance to depose.
    pub fn prior_instance_destroy_dependencies(&self) -> Option<PriorInstanceDestroyDependencies> {
        let Self::Delete { dependencies, .. } = self else {
            return None;
        };
        Some(PriorInstanceDestroyDependencies(dependencies.clone()))
    }

    /// Loop element shown beside this effect in human-facing plan renderers.
    pub fn loop_origin(&self) -> Option<&LoopElementOrigin> {
        match self {
            Self::Create(resource) | Self::ResumeCreate { resource, .. } => {
                resource.loop_provenance.display_origin()
            }
            Self::Update { to, .. } => to.loop_provenance.display_origin(),
            Self::Move { origin, .. } => origin.as_ref(),
            Self::Read { .. }
            | Self::Delete { .. }
            | Self::Import { .. }
            | Self::Remove { .. }
            | Self::ReleaseListKeys { .. }
            | Self::Wait { .. }
            | Self::DeferredCreate(_)
            | Self::DeferredReplace(_) => None,
        }
    }

    /// Mint route-rebinding authority only from a resolved move effect.
    pub fn move_authority(&self) -> Option<MoveAuthority<'_>> {
        match self {
            Self::Move { from, to, .. } => Some(MoveAuthority { from, to }),
            Self::Read { .. }
            | Self::Create(_)
            | Self::ResumeCreate { .. }
            | Self::Update { .. }
            | Self::Delete { .. }
            | Self::Import { .. }
            | Self::Remove { .. }
            | Self::ReleaseListKeys { .. }
            | Self::Wait { .. }
            | Self::DeferredCreate(_)
            | Self::DeferredReplace(_) => None,
        }
    }

    /// Narrow this effect to the capability accepted by keyed-list state
    /// writeback. No provider handle is present on that path.
    pub fn as_release_list_keys(&self) -> Option<ReleaseListKeysEffect<'_>> {
        match self {
            Self::ReleaseListKeys {
                id,
                attribute,
                key_field,
                keys,
            } => Some(ReleaseListKeysEffect {
                id,
                attribute,
                key_field,
                keys,
            }),
            Self::Read { .. }
            | Self::Create(_)
            | Self::ResumeCreate { .. }
            | Self::Update { .. }
            | Self::Delete { .. }
            | Self::Import { .. }
            | Self::Remove { .. }
            | Self::Move { .. }
            | Self::Wait { .. }
            | Self::DeferredCreate(_)
            | Self::DeferredReplace(_) => None,
        }
    }
}

impl EffectWithSource<ModuleSource> {
    /// Construct an import and the only typed authorization for suppressing
    /// the matching create-outcome-unknown planning error.
    pub fn import_replacing_outcome_unknown(
        id: ResolvedResourceId,
        identifier: crate::resource::Value,
    ) -> (Self, OutcomeUnknownImportReplacement) {
        let replacement = OutcomeUnknownImportReplacement { id: id.clone() };
        (Self::Import { id, identifier }, replacement)
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
        let deferred_template = || {
            crate::parser::DeferredForExpression::new(
                Some("main.crn".to_string()),
                12,
                "for opt in cert.domain_validation_options".to_string(),
                crate::parser::DeferredForTarget::bound(absolute_binding("validation_records"))
                    .expect("fixture binding is valid"),
                Value::root_resource_ref("cert", "domain_validation_options", Vec::new()),
                crate::parser::ForBinding::Simple("opt".to_string()),
                Resource::new("route53.Record", "validation_records"),
            )
        };

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
                    12,
                    "display_glyph_effects must list every Effect variant exactly once"
                );
                effects
            }};
        }

        effect_cases![
            (
                Effect::Read {
                    resource: ResolvedDataSource::new(
                        DataSource::new("test", "x").with_dependency_phase(()),
                    ),
                },
                Effect::Read { .. }
            ),
            (
                Effect::Create(ResolvedResource::new(
                    Resource::new("test", "x").with_dependency_phase(()),
                )),
                Effect::Create(_)
            ),
            (
                Effect::ResumeCreate {
                    resource: ResolvedResource::new(
                        Resource::new("test", "x").with_dependency_phase(()),
                    ),
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
                    to: ResolvedResource::new(Resource::new("test", "x").with_dependency_phase(()),),
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
                    dependencies: Default::default(),
                    explicit_dependencies: Default::default(),
                    blocked_by_updates: Default::default(),
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
                    origin: None,
                },
                Effect::Move { .. }
            ),
            (
                Effect::ReleaseListKeys {
                    id: crate::resource::ResolvedResourceId::new(id.clone()),
                    attribute: "settings".to_string(),
                    key_field: FieldName::declared("key"),
                    keys: ReleasedListKeys::try_new(vec![ReleasedListKey::new(
                        "enabled",
                        Value::Concrete(ConcreteValue::String("true".into())),
                    )])
                    .expect("test release has one key"),
                },
                Effect::ReleaseListKeys { .. }
            ),
            (
                Effect::Wait {
                    id: ResolvedResourceId::with_identity("__wait", "w"),
                    target_id: fixture_managed_wait_target(id.clone()),
                    until: WaitPredicate::equals(
                        AttrPath::single("status"),
                        Value::Concrete(ConcreteValue::String("ready".into())),
                    )
                    .unwrap(),
                    until_surface: "status == 'ready'".to_string(),
                    timeout: Duration::from_secs(60),
                    interval: Duration::from_secs(1),
                    explicit_dependencies: Default::default(),
                },
                Effect::Wait { .. }
            ),
            (
                Effect::DeferredCreate(crate::test_support::compiled_loop(Box::new(
                    deferred_template(),
                ))),
                Effect::DeferredCreate(_)
            ),
            (
                Effect::DeferredReplace(Box::new(crate::test_support::compiled_replace(
                    NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                        id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                            "route53.Record",
                            "validation_records[0]"
                        )),
                        identifier: fixture_applied_identifier("record-0"),
                        directives: Directives::default(),
                        binding: Some(absolute_binding("validation_records[0]")),
                        dependencies: Default::default(),
                        explicit_dependencies: Default::default(),
                        blocked_by_updates: Default::default(),
                    }])
                    .expect("test fixture has one delete"),
                    Box::new(deferred_template())
                ))),
                Effect::DeferredReplace(_)
            ),
        ]
    }
}

impl<S> EffectWithSource<S> {
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
            Effect::ReleaseListKeys { .. } => "!",
            Effect::Wait { .. } => ">",
            Effect::DeferredCreate(_) => "+",
            Effect::DeferredReplace(_) => "+/-",
        }
    }

    /// Narrow this effect to a [`BasicEffect`] if it is one of the
    /// variants the basic executor handles (`Create`, `Update`,
    /// `Delete`). Returns `None` for `Read`, `Import`,
    /// `Remove`, `Move`, `ReleaseListKeys`, and `Wait` — those route through other
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
    /// ```compile_fail,E0277
    /// use carina_core::effect::{BasicEffect, Effect};
    /// use carina_core::resource::{ResolvedResource, Resource};
    /// let effect: Effect = Effect::Create(ResolvedResource::new(Resource::new("test", "x").into_value_snapshot()));
    /// // Was: a missed filter could pass a non-basic `&Effect` straight
    /// // into `execute_basic_effect` and trip `unreachable!()` at apply
    /// // time (carina#3164). The conversion no longer exists.
    /// let _: BasicEffect = (&effect).into();
    /// ```
    pub fn as_basic(&self) -> Option<BasicEffect<'_, S>> {
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
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate(_)
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
            Effect::ReleaseListKeys { .. } => "release_list_keys",
            Effect::Wait { .. } => "wait",
            Effect::DeferredCreate(_) => "deferred_create",
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
            Effect::ReleaseListKeys { .. } => true,
            Effect::Wait { .. } => false,
            Effect::DeferredCreate(_) => true,
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
            Effect::ReleaseListKeys { .. } => true,
            Effect::Wait { .. } => false,
            Effect::DeferredCreate(_) => false,
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
            Effect::ReleaseListKeys { .. } => false,
            Effect::Wait { .. } => false,
            Effect::DeferredCreate(_) => true,
            Effect::DeferredReplace(_) => true,
        }
    }

    /// Returns the resource ID for this effect.
    ///
    /// A bound deferred loop uses its derived template identity (for example,
    /// `children[?]`), not its `children` binding address. An unbound deferred
    /// loop uses its source-derived loop identity. These IDs describe the
    /// deferred effect, not a concrete expanded child resource.
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
            Effect::ReleaseListKeys { id, .. } => id,
            Effect::Wait { target_id, .. } => target_id.id(),
            Effect::DeferredCreate(payload) => payload.id(),
            Effect::DeferredReplace(payload) => payload.id(),
        }
    }

    /// Returns the effect's resource identity, not its typed scheduling key.
    ///
    /// `Effect::identity()` is total because all scheduler-bound payloads went
    /// through the `Resolved*` checked constructors after the resolver boundary.
    /// `Wait` returns the wait effect's own identity, not its target's.
    /// Bound deferred loops return the template identity (`children[?]`),
    /// while dependency scheduling uses the distinct `children` binding key.
    /// Unbound deferred loops return their source-derived loop identity.
    pub fn scheduler_id(&self) -> ResolvedResourceId {
        match self {
            Effect::Read { resource } => ResolvedResourceId::new(resource.id.clone()),
            Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => {
                resource.resolved_id()
            }
            Effect::Update { to, .. } => to.resolved_id(),
            Effect::Delete { id, .. }
            | Effect::Import { id, .. }
            | Effect::Remove { id }
            | Effect::ReleaseListKeys { id, .. }
            | Effect::Move { to: id, .. }
            | Effect::Wait { id, .. } => id.clone(),
            Effect::DeferredCreate(payload) => payload.id().clone(),
            Effect::DeferredReplace(payload) => payload.id().clone(),
        }
    }

    pub fn identity(&self) -> ResourceIdentity {
        self.scheduler_id().identity().clone()
    }

    pub fn instance_key(&self) -> EffectInstanceKey {
        let generation = match self {
            Effect::Delete { generation, .. } => generation.clone(),
            _ => EffectGeneration::Current,
        };
        EffectInstanceKey::new(self.scheduler_id(), generation)
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
    pub fn as_resource_ref(&self) -> Option<EffectResourceRef<'_, S>> {
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
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate(_)
            | Effect::DeferredReplace(_) => None,
        }
    }

    /// Returns the typed binding address for this effect, if it has one.
    pub fn binding_address(&self) -> Option<std::borrow::Cow<'_, AbsoluteBindingAddress>> {
        use std::borrow::Cow;
        let binding = match self {
            Effect::Read { resource } => resource.binding.as_ref(),
            Effect::Create(resource) => resource.binding.as_ref(),
            Effect::ResumeCreate { resource, .. } => resource.binding.as_ref(),
            Effect::Update { to, .. } => to.binding.as_ref(),
            Effect::Delete { binding, .. } => binding.as_ref(),
            // A wait's synthetic scheduler identity is minted from its typed
            // binding by the graph. This address is for display/provenance;
            // scheduling continues to use the complete resolved ID.
            Effect::Wait { id, .. } => return Some(Cow::Owned(id.binding_address())),
            Effect::DeferredReplace(payload) => payload.template.target().bound_binding_address(),
            Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::DeferredCreate(_) => None,
        };
        binding.map(Cow::Borrowed)
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
            Effect::ReleaseListKeys { .. } => Vec::new(),
            Effect::Wait { .. } => Vec::new(),
            Effect::DeferredCreate(_) => Vec::new(),
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
            Effect::ReleaseListKeys { .. } => Vec::new(),
            Effect::Wait { .. } => Vec::new(),
            Effect::DeferredCreate(_) => Vec::new(),
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

    /// Returns the exact addresses this effect depends on **via explicit
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
    /// State-only effects (Import, Remove, Move, ReleaseListKeys) return an empty set —
    /// they are scheduling primitives, not resource-state operations.
    pub fn explicit_dependencies(&self) -> ResolvedDependencies {
        match self {
            Effect::Read { resource } => resource.explicit_dependencies().clone(),
            Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => {
                resource.explicit_dependencies().clone()
            }
            Effect::Update { to, .. } => to.explicit_dependencies().clone(),
            Effect::Delete {
                explicit_dependencies,
                ..
            }
            | Effect::Wait {
                explicit_dependencies,
                ..
            } => explicit_dependencies.clone(),
            Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. } => Default::default(),
            Effect::DeferredCreate(_) => Default::default(),
            Effect::DeferredReplace(payload) => {
                deferred_replace_delete_explicit_dependencies(&payload.deletes)
            }
        }
    }

    pub(crate) fn blocking_keys(&self) -> ResolvedDependencies {
        match self {
            Effect::Read { resource } => resource.all_dependencies().cloned().collect(),
            Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => {
                resource.all_dependencies().cloned().collect()
            }
            Effect::Update { to, .. } => to.all_dependencies().cloned().collect(),
            Effect::Wait {
                target_id,
                explicit_dependencies,
                ..
            } => std::iter::once(target_id.id().clone())
                .chain(explicit_dependencies.iter().cloned())
                .collect(),
            Effect::Delete {
                dependencies,
                explicit_dependencies,
                ..
            } => dependencies
                .iter()
                .chain(explicit_dependencies.iter())
                .cloned()
                .collect(),
            Effect::DeferredCreate(payload) => payload.dependency_template.dependencies(),
            Effect::DeferredReplace(payload) => {
                let mut dependencies = deferred_replace_delete_dependencies(&payload.deletes);
                dependencies.extend(payload.dependency_template.dependencies().iter().cloned());
                dependencies
            }
            Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. } => Default::default(),
        }
    }

    pub(crate) fn known_dependency_reads(
        &self,
        dependency: &ResolvedResourceId,
    ) -> Option<&BTreeSet<String>> {
        match self {
            Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => {
                resource.known_dependency_reads(dependency)
            }
            Effect::Update { to, .. } => to.known_dependency_reads(dependency),
            Effect::Read { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate(_)
            | Effect::DeferredReplace(_) => None,
        }
    }

    pub(crate) fn scheduling_keys(&self) -> Vec<SchedulerKey> {
        if matches!(self, Effect::ReleaseListKeys { .. }) {
            Vec::new()
        } else {
            vec![self.scheduler_id()]
        }
    }

    /// Scheduling edges this effect contributes when running `apply`.
    ///
    /// Returned edges describe ordering constraints relative to other
    /// bindings; resolving them to concrete effect indices is the
    /// responsibility of [`crate::effect::deps::build_effect_dependency_analysis`].
    pub(crate) fn apply_edges(&self) -> Vec<ScheduleEdge> {
        match self {
            Effect::Delete {
                dependencies,
                blocked_by_updates,
                ..
            } => {
                let mut edges: Vec<ScheduleEdge> = dependencies
                    .iter()
                    .cloned()
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
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate(_)
            | Effect::DeferredReplace(_) => self
                .blocking_keys()
                .into_iter()
                .map(ScheduleEdge::DependsOn)
                .collect(),
            Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. } => Vec::new(),
        }
    }

    /// Scheduling edges this effect contributes when running `destroy`.
    ///
    /// Differs from [`Self::apply_edges`] in that meta effects (`Wait`,
    /// deferred variants) contribute no edges because they are not present
    /// in destroy plans.
    pub(crate) fn destroy_edges(&self) -> Vec<ScheduleEdge> {
        match self {
            Effect::Delete { dependencies, .. } => dependencies
                .iter()
                .cloned()
                .map(ScheduleEdge::BlockedBy)
                .collect(),
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate(_)
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

/// An import identifier that remained unresolved at the provider boundary.
#[derive(Debug, thiserror::Error)]
#[error(
    "import identifier did not resolve to a concrete string at apply time (got {got}). A referenced upstream value has not been published yet — apply the upstream stack first, then re-run apply here."
)]
pub struct ImportIdentifierError {
    got: String,
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
pub fn resolve_import_identifier(
    identifier: &crate::resource::Value,
) -> Result<&str, ImportIdentifierError> {
    use crate::resource::{ConcreteValue, DeferredValue, Value};
    match identifier {
        Value::Concrete(ConcreteValue::String(s)) => Ok(s.expose_plaintext()),
        Value::Concrete(ConcreteValue::EnumIdentifier(s)) => Ok(s.as_str()),
        Value::Deferred(DeferredValue::Secret(inner)) => resolve_import_identifier(inner),
        other => Err(ImportIdentifierError {
            got: crate::value::format_value_with_key(other, Some("id")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use crate::schema::PatternResourceSchema;

    use super::*;
    use crate::resource::ConcreteValue;
    use std::collections::HashSet;

    type Value = crate::resource::ExpandedValue;
    type DeferredValue = crate::resource::ExpandedDeferredValue;

    const EFFECT_VARIANT_COUNT: usize = 12;

    fn test_id(identity: &str) -> ResolvedResourceId {
        ResolvedResourceId::with_identity(
            "test",
            ResourceIdentity::try_from(identity.to_string())
                .expect("test identity must be non-empty"),
        )
    }

    fn test_dependencies(names: &[&str]) -> ResolvedDependencies {
        names.iter().map(|name| test_id(name)).collect()
    }

    fn deserialize_effect(json: &str) -> SavedEffect {
        serde_json::from_str::<SavedEffect>(json).expect("deserialize saved effect")
    }

    fn deserialize_effect_value(value: serde_json::Value) -> SavedEffect {
        serde_json::from_value::<SavedEffect>(value).expect("deserialize saved effect")
    }

    #[test]
    fn outcome_unknown_import_replacement_witness_is_tied_to_the_import_effect() {
        let id = ResolvedResourceId::with_provider_identity(
            "mock",
            "test.Resource",
            "shared",
            Some("west".to_string()),
        );
        let (effect, replacement) = Effect::import_replacing_outcome_unknown(
            id.clone(),
            Value::Concrete(ConcreteValue::String("provider-id".into())),
        );

        assert!(matches!(
            effect,
            Effect::Import { id: effect_id, .. } if effect_id == id
        ));
        assert_eq!(replacement.resource_id(), id.as_inner());
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
        deferred_for_template_at(Some("main.crn".to_string()), 12)
    }

    fn deferred_for_template_at(
        file: Option<String>,
        line: usize,
    ) -> crate::parser::DeferredForExpression {
        crate::parser::DeferredForExpression::new(
            file,
            line,
            "for opt in cert.domain_validation_options".to_string(),
            crate::parser::DeferredForTarget::bound(absolute_binding("validation_records"))
                .expect("fixture binding is valid"),
            Value::root_resource_ref("cert", "domain_validation_options", Vec::new()),
            crate::parser::ForBinding::Simple("opt".to_string()),
            Resource::new("route53.Record", "validation_records"),
        )
    }

    fn deferred_for_producer() -> Resource {
        Resource::with_provider("mock", "test", "cert", None)
            .with_absolute_binding(absolute_binding("cert"))
    }

    fn compiled_deferred_for_template(
        template: crate::parser::DeferredForExpression,
    ) -> DeferredCreatePayload {
        crate::test_support::compiled_loop_with_producers(
            Box::new(template),
            vec![deferred_for_producer()],
        )
    }

    fn deferred_create_effect() -> Effect {
        Effect::DeferredCreate(compiled_deferred_for_template(deferred_for_template()))
    }

    fn deferred_replace_effect() -> Effect {
        let payload = compiled_deferred_for_template(deferred_for_template_at(None, 0));
        Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.Record",
                    "validation_records[0]",
                )),
                identifier: fixture_applied_identifier("old-record-id"),
                directives: Directives::default(),
                binding: Some(absolute_binding("validation_records[0]")),
                dependencies: crate::test_support::persisted([ResolvedResourceId::new(
                    deferred_for_producer().id,
                )]),
                explicit_dependencies: Default::default(),
                blocked_by_updates: Default::default(),
            }])
            .expect("test fixture has one delete"),
            template: payload.template,
            dependency_template: payload.dependency_template,
        }))
    }

    #[test]
    fn deferred_effect_wire_matches_stored_id_shape_byte_for_byte() {
        #[derive(Serialize)]
        enum StoredEffect {
            DeferredCreate {
                id: ResolvedResourceId,
                template: Box<crate::parser::CompiledDeferredForExpression>,
                dependency_template: Box<crate::resource::DeferredDependencyTemplate>,
            },
            DeferredReplace(Box<StoredDeferredReplacePayload>),
        }

        #[derive(Serialize)]
        struct StoredDeferredReplacePayload {
            deletes: NonEmptyDeletes,
            id: ResolvedResourceId,
            template: Box<crate::parser::CompiledDeferredForExpression>,
            dependency_template: crate::resource::DeferredDependencyTemplate,
        }

        let create_payload = compiled_deferred_for_template(deferred_for_template());
        let create_template = create_payload.template.clone();
        let create_id = ResolvedResourceId::with_provider_identity(
            "",
            "route53.Record",
            "validation_records",
            None,
        );
        let current_create = Effect::DeferredCreate(create_payload.clone());
        let stored_create = StoredEffect::DeferredCreate {
            id: create_id,
            template: create_template,
            dependency_template: Box::new(create_payload.dependency_template),
        };
        assert_eq!(
            serde_json::to_vec(&current_create).expect("serialize current deferred create"),
            serde_json::to_vec(&stored_create).expect("serialize stored deferred create"),
        );

        let Effect::DeferredReplace(current_replace) = deferred_replace_effect() else {
            unreachable!("fixture constructs DeferredReplace")
        };
        let stored_replace =
            StoredEffect::DeferredReplace(Box::new(StoredDeferredReplacePayload {
                deletes: current_replace.deletes.clone(),
                id: ResolvedResourceId::with_provider_identity(
                    "",
                    "route53.Record",
                    "validation_records",
                    None,
                ),
                template: current_replace.template.clone(),
                dependency_template: current_replace.dependency_template.clone(),
            }));
        let current_replace = Effect::DeferredReplace(current_replace);
        assert_eq!(
            serde_json::to_vec(&current_replace).expect("serialize current deferred replace"),
            serde_json::to_vec(&stored_replace).expect("serialize stored deferred replace"),
        );
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
                    resource: ResolvedDataSource::new(
                        DataSource::new("test", "x").with_dependency_phase(()),
                    ),
                },
            ),
            (
                "Create",
                Effect::Create(ResolvedResource::new(
                    Resource::new("test", "x").with_dependency_phase(()),
                )),
            ),
            (
                "ResumeCreate",
                Effect::ResumeCreate {
                    resource: ResolvedResource::new(
                        Resource::new("test", "x").with_dependency_phase(()),
                    ),
                    started_at: crate::provider::Timestamp::try_from("2026-10-30T00:00:00Z")
                        .expect("valid timestamp"),
                },
            ),
            (
                "Update",
                Effect::Update {
                    from: Box::new(
                        State::existing(rid.clone(), Default::default()).with_identifier("x-1"),
                    ),
                    identifier: fixture_applied_identifier("x-1"),
                    to: ResolvedResource::new(Resource::new("test", "x").with_dependency_phase(())),
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
                    dependencies: Default::default(),
                    explicit_dependencies: Default::default(),
                    blocked_by_updates: Default::default(),
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
                    origin: None,
                },
            ),
            (
                "ReleaseListKeys",
                Effect::ReleaseListKeys {
                    id: ResolvedResourceId::new(rid.clone()),
                    attribute: "settings".to_string(),
                    key_field: FieldName::declared("key"),
                    keys: ReleasedListKeys::try_new(vec![ReleasedListKey::new(
                        "enabled",
                        Value::Concrete(ConcreteValue::String("true".into())),
                    )])
                    .expect("fixture has one release key"),
                },
            ),
            (
                "Wait",
                Effect::Wait {
                    id: ResolvedResourceId::with_identity("__wait", "w"),
                    target_id: fixture_managed_wait_target(rid.clone()),
                    until: WaitPredicate::equals(
                        AttrPath::single("status"),
                        Value::Concrete(ConcreteValue::String("ready".into())),
                    )
                    .unwrap(),
                    until_surface: "status == 'ready'".to_string(),
                    timeout: Duration::from_secs(60),
                    interval: Duration::from_secs(1),
                    explicit_dependencies: Default::default(),
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
            ApplyTimeUnknown, ConcreteValue, DeferredValue, InterpolationPart, Value,
        };
        let inner = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                ApplyTimeUnknown::UpstreamAllExports {
                    binding: absolute_binding("u"),
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
            ApplyTimeUnknown, ConcreteValue, DeferredValue, InterpolationPart, Value,
        };
        let v = Value::Deferred(DeferredValue::Interpolation(vec![
            InterpolationPart::Expr(Value::Deferred(DeferredValue::Unknown(
                ApplyTimeUnknown::UpstreamAllExports {
                    binding: absolute_binding("upstream"),
                },
            ))),
            InterpolationPart::Literal("|tail".into()),
        ]));
        let err = resolve_import_identifier(&v).unwrap_err();
        assert!(
            err.to_string()
                .contains("did not resolve to a concrete string"),
            "unexpected error message: {err}"
        );
        // Sanity: still passes for a concrete String/EnumIdentifier.
        let s = Value::Concrete(ConcreteValue::enum_identifier("ENUM_X"));
        assert_eq!(resolve_import_identifier(&s).unwrap(), "ENUM_X");
    }

    #[test]
    fn deferred_create_blocking_dependencies_is_upstream_only() {
        let effect = deferred_create_effect();
        assert_eq!(
            effect.blocking_keys(),
            ResolvedDependencies::from_iter([ResolvedResourceId::new(deferred_for_producer().id)]),
        );
    }

    #[test]
    fn deferred_create_blocking_dependencies_include_both_coalesce_operands() {
        let mut template = deferred_for_template();
        template.iterable = Value::Deferred(DeferredValue::Coalesce {
            value: Box::new(Value::root_resource_ref("primary", "items", Vec::new())),
            default: Box::new(Value::root_resource_ref("fallback", "items", Vec::new())),
        });
        let effect = Effect::DeferredCreate(crate::test_support::compiled_loop(Box::new(template)));

        assert_eq!(
            effect.blocking_keys(),
            test_dependencies(&["fallback", "primary"]),
        );
    }

    #[test]
    fn deferred_replace_blocking_dependencies_includes_upstream_and_delete_deps() {
        let template = deferred_for_template_at(None, 0);
        let effect = Effect::DeferredReplace(Box::new(crate::test_support::compiled_replace(
            NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.Record",
                    "validation_records[0]",
                )),
                identifier: fixture_applied_identifier("old-record-id"),
                directives: Directives::default(),
                binding: Some(absolute_binding("validation_records[0]")),
                dependencies: crate::test_support::persisted([ResolvedResourceId::with_identity(
                    "test", "foo",
                )]),
                explicit_dependencies: Default::default(),
                blocked_by_updates: Default::default(),
            }])
            .expect("fixture has one delete"),
            Box::new(template),
        )));

        assert_eq!(effect.blocking_keys(), test_dependencies(&["cert", "foo"]));
    }

    #[test]
    fn deferred_replace_explicit_dependencies_unions_delete_explicit_deps() {
        let template = deferred_for_template_at(None, 0);
        let effect = Effect::DeferredReplace(Box::new(crate::test_support::compiled_replace(
            NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.Record",
                    "validation_records[0]",
                )),
                identifier: fixture_applied_identifier("old-record-id"),
                directives: Directives::default(),
                binding: Some(absolute_binding("validation_records[0]")),
                dependencies: Default::default(),
                explicit_dependencies: test_dependencies(&["foo"]),
                blocked_by_updates: Default::default(),
            }])
            .expect("fixture has one delete"),
            Box::new(template),
        )));

        assert_eq!(effect.explicit_dependencies(), test_dependencies(&["foo"]));
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
            &ResourceId::with_identity("route53.Record", "validation_records[?]")
        );
    }

    #[test]
    fn deferred_create_serde_roundtrip() {
        let original = deferred_create_effect();
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded = deserialize_effect(&json);
        match decoded {
            Effect::DeferredCreate(payload) => {
                let template = payload.template;
                assert_eq!(template.file(), Some("main.crn"));
                assert_eq!(template.line(), 12);
                assert_eq!(template.header, "for opt in cert.domain_validation_options");
                assert_eq!(template.resource_type(), "route53.Record");
                assert_eq!(
                    template.target().bound_binding(),
                    Some("validation_records")
                );
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
        assert_eq!(decoded, SavedEffect::from(original));
    }

    #[test]
    fn deferred_ignore_changes_error_reports_materialized_template_address() {
        use crate::resource::{ParsedIgnorePath, ParsedIgnorePathSegment};

        let mut effect = deferred_create_effect();
        let Effect::DeferredCreate(payload) = &mut effect else {
            unreachable!("fixture constructs DeferredCreate")
        };
        payload.template.template_directives_mut().ignore_changes =
            vec![ParsedIgnorePath::new(vec![
                ParsedIgnorePathSegment::Attribute("missing".to_string()),
            ])];

        let json = serde_json::to_string(&effect).expect("serialize");
        let saved = serde_json::from_str::<UnresolvedSavedEffect>(&json)
            .expect("deserialize unresolved saved effect");
        let mut schemas = crate::schema::SchemaRegistry::new();
        schemas.insert(
            "route53",
            PatternResourceSchema::new("Record")
                .validate()
                .expect("valid test schema"),
        );

        let error = saved
            .resolve_with_schemas(&schemas)
            .expect_err("ignore_changes path is absent from the schema");
        assert!(
            error
                .to_string()
                .contains("route53.Record.validation_records[?]"),
            "unexpected error: {error}",
        );
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
            binding: Some(absolute_binding("bucket")),
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
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
                matches!(
                    label,
                    "Import" | "Remove" | "Move" | "ReleaseListKeys" | "DeferredReplace"
                ),
                "{label} state-operation classification mismatch",
            );
        }
    }

    #[test]
    fn read_is_not_mutating() {
        let resource: DataSource = DataSource::new("test", "example");
        let effect = Effect::Read {
            resource: ResolvedDataSource::new(resource.with_dependency_phase(())),
        };
        assert!(!effect.is_mutating());
    }

    #[test]
    fn release_list_keys_saved_effect_round_trips() {
        let effect = every_effect_variant()
            .into_iter()
            .find_map(|(label, effect)| (label == "ReleaseListKeys").then_some(effect))
            .expect("release fixture");
        let json = serde_json::to_string(&SavedEffect::from(effect.clone()))
            .expect("serialize release effect");
        let decoded = deserialize_effect(&json);
        assert_eq!(decoded, SavedEffect::from(effect));
        let Effect::ReleaseListKeys {
            attribute,
            key_field,
            keys,
            ..
        } = decoded
        else {
            panic!("expected release effect")
        };
        assert_eq!(attribute, "settings");
        assert_eq!(key_field.as_str(), "key");
        assert_eq!(keys[0].key(), "enabled");
    }

    #[test]
    fn create_is_mutating() {
        let resource: Resource = Resource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Create(ResolvedResource::new(resource.into_value_snapshot()));
        assert!(effect.is_mutating());
    }

    #[test]
    fn resource_id_returns_correct_id() {
        let resource: DataSource = DataSource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Read {
            resource: ResolvedDataSource::new(resource.clone().with_dependency_phase(())),
        };
        assert_eq!(effect.resource_id(), &resource.id);
    }

    #[test]
    fn resource_returns_some_for_create() {
        let resource: Resource = Resource::new("s3.Bucket", "my-bucket");
        let effect = Effect::Create(ResolvedResource::new(
            resource.clone().with_dependency_phase(()),
        ));
        assert_eq!(effect.as_resource_ref().unwrap().id(), &resource.id);
    }

    #[test]
    fn resource_returns_none_for_delete() {
        let effect: Effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity("test", "a")),
            identifier: fixture_applied_identifier("id-123"),
            generation: EffectGeneration::Current,
            directives: Directives::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        assert!(effect.as_resource_ref().is_none());
    }

    #[test]
    fn binding_address_returns_binding() {
        let resource: Resource =
            Resource::new("test", "my_binding").with_root_binding("my_binding");
        let effect = Effect::Create(ResolvedResource::new(resource.into_value_snapshot()));
        assert_eq!(
            effect.binding_address().as_deref(),
            Some(&absolute_binding("my_binding"))
        );
    }

    #[test]
    fn binding_address_returns_none_without_binding() {
        use crate::resource::{ConcreteValue, Value};
        let resource: Resource = Resource::new("test", "no_binding").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("test".into())),
        );
        let effect = Effect::Create(ResolvedResource::new(resource.into_value_snapshot()));
        assert_eq!(effect.binding_address(), None);
    }

    #[test]
    fn effect_serde_round_trip() {
        use crate::resource::{ConcreteValue, Value};
        use std::collections::HashMap;

        let effects: Vec<Effect> = vec![
            Effect::Create(ResolvedResource::new(
                Resource::new("s3.Bucket", "my-bucket").with_dependency_phase(()),
            )),
            Effect::Read {
                resource: ResolvedDataSource::new(
                    DataSource::new("s3.Bucket", "existing").with_dependency_phase(()),
                ),
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
                to: ResolvedResource::new(
                    Resource::new("s3.Bucket", "my-bucket")
                        .with_attribute(
                            "versioning",
                            Value::Concrete(ConcreteValue::String("Enabled".into())),
                        )
                        .with_dependency_phase(()),
                ),
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
                dependencies: Default::default(),
                explicit_dependencies: Default::default(),
                blocked_by_updates: Default::default(),
            },
        ];

        for effect in effects {
            let json = serde_json::to_string(&effect).unwrap();
            let deserialized = deserialize_effect(&json);
            assert_eq!(
                SavedEffect::from(effect.clone()),
                deserialized,
                "Round-trip failed for {:?}",
                effect
            );
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
    fn explicit_dependencies_are_carried_by_resolved_resource() {
        let bucket: Resource = Resource::new("s3.Bucket", "b");
        let create = Effect::Create(ResolvedResource::from_resolved_parts(
            bucket.into_value_snapshot(),
            Default::default(),
            test_dependencies(&["role", "kms"]),
        ));
        let got = create.explicit_dependencies();
        assert!(
            got.contains(&test_id("role")) && got.contains(&test_id("kms")),
            "got {got:?}"
        );
    }

    #[test]
    fn explicit_dependencies_for_delete_uses_stored_set() {
        let effect: Effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "s3.Bucket",
                "b",
            )),
            identifier: fixture_applied_identifier("x"),
            generation: EffectGeneration::Current,
            directives: Directives::default(),
            binding: Some(absolute_binding("bucket")),
            dependencies: crate::test_support::persisted([test_id("role"), test_id("kms")]),
            explicit_dependencies: test_dependencies(&["role"]),
            blocked_by_updates: Default::default(),
        };
        let got = effect.explicit_dependencies();
        assert!(got.contains(&test_id("role")));
        assert!(
            !got.contains(&test_id("kms")),
            "kms is value-ref-only; got {:?}",
            got
        );
    }

    #[test]
    fn explicit_dependencies_empty_for_state_only_effects() {
        let imp: Effect = Effect::Import {
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
            origin: None,
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
    fn delete_without_optional_typed_dependency_metadata_deserialises_to_empty() {
        // Saved plans may omit `explicit_dependencies` and
        // `blocked_by_updates`. `#[serde(default)]` must populate both while
        // the dependency values that are present remain complete addresses.
        let legacy = serde_json::json!({
            "Delete": {
                "id": {"provider": "aws", "resource_type": "s3.Bucket", "name": "b"},
                "identifier": "x",
                "directives": {},
                "binding": null,
                "dependencies": [
                    {"provider": "aws", "resource_type": "iam.Role", "name": "role"}
                ],
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

        let _: Effect = Effect::Wait {
            id: ResolvedResourceId::with_identity("__wait", "cert_issued"),
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
            explicit_dependencies: Default::default(),
        };
    }

    #[test]
    fn wait_binding_name_returns_wait_binding() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let e: Effect = Effect::Wait {
            id: ResolvedResourceId::with_identity("__wait", "cert_issued"),
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
            explicit_dependencies: Default::default(),
        };
        assert_eq!(
            e.binding_address().as_deref(),
            Some(&absolute_binding("cert_issued"))
        );
    }

    #[test]
    fn wait_is_not_mutating() {
        use crate::resource::{ConcreteValue, Value};
        use crate::wait::predicate::{AttrPath, WaitPredicate};
        use std::time::Duration;

        let e: Effect = Effect::Wait {
            id: ResolvedResourceId::with_identity("__wait", "cert_issued"),
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
            explicit_dependencies: Default::default(),
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

        let target = ResolvedResource::new(
            Resource::new("acm.Certificate", "cert").with_dependency_phase(()),
        );
        let original = Effect::Wait {
            id: ResolvedResourceId::with_identity("__wait", "cert_issued"),
            target_id: ManagedWaitTarget::new(&target),
            until: WaitPredicate::equals(
                AttrPath::single("status"),
                Value::Concrete(ConcreteValue::String("ISSUED".into())),
            )
            .unwrap(),
            until_surface: "cert.status == aws.acm.Certificate.Status.Issued".to_string(),
            timeout: Duration::from_secs(4500),
            interval: Duration::from_secs(5),
            explicit_dependencies: Default::default(),
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
        let create: Effect = Effect::Create(ResolvedResource::new(
            Resource::new("test", "x").into_value_snapshot(),
        ));
        assert!(matches!(
            create.as_basic(),
            Some(BasicEffect::Create { .. })
        ));

        let update: Effect = Effect::Update {
            from: Box::new(
                ResState::existing(rid.clone(), Default::default()).with_identifier("x-1"),
            ),
            identifier: fixture_applied_identifier("x-1"),
            to: ResolvedResource::new(Resource::new("test", "x").with_dependency_phase(())),
            changed_attributes: vec![],
        };
        assert!(matches!(
            update.as_basic(),
            Some(BasicEffect::Update { .. })
        ));

        let delete: Effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(rid.clone()),
            identifier: fixture_applied_identifier("x-1"),
            generation: EffectGeneration::Current,
            directives: Directives::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        assert!(matches!(
            delete.as_basic(),
            Some(BasicEffect::Delete { .. })
        ));

        // Non-basic variants must not. If a new variant is added and
        // someone forgets to extend `as_basic`, the exhaustive match
        // inside `as_basic` is what catches it at compile time; this
        // test catches misclassification of an existing variant.
        let read: Effect = Effect::Read {
            resource: ResolvedDataSource::new(
                DataSource::new("test", "x").with_dependency_phase(()),
            ),
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
            origin: None,
        };
        let wait = Effect::Wait {
            id: ResolvedResourceId::with_identity("__wait", "w"),
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
            explicit_dependencies: Default::default(),
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
