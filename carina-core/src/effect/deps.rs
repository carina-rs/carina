use std::collections::{BTreeSet, HashMap, HashSet};

use crate::effect::{Effect, EffectInstanceKey, ScheduleEdge};
use crate::non_empty::NonEmptyVec;
#[cfg(test)]
use crate::resource::ResourceId;
use crate::resource::{ResolvedDependencies, ResolvedResourceId, Resource};

#[derive(Debug, Clone)]
pub struct UnresolvedResource<S = crate::resource::ModuleSource>(Resource<S>);

impl<S> UnresolvedResource<S> {
    /// Pre-resolution snapshot used by dependency analysis and apply-time
    /// reference re-resolution.
    pub fn from_pre_resolve(resource: Resource<S>) -> Self {
        Self(resource)
    }

    pub fn as_resource(&self) -> &Resource<S> {
        &self.0
    }
}

/// Selects which scheduling contract [`build_effect_dependency_analysis`]
/// applies.
///
/// `Apply` follows the apply scheduler's rules (resource refs become edges
/// and meta effects contribute `DependsOn` edges). `Destroy` ignores
/// resource-ref edges and instead lets each effect's [`Effect::destroy_edges`] drive the
/// graph. The typed `aliases` slot carries `wait`-binding bridges that
/// would otherwise be dropped from the destroy plan. `kept_targets` names
/// graph nodes that intentionally have no destroy effect; distinguishing them
/// from an absent address keeps missing targets fallible without inventing
/// no-op effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleInputs<'a> {
    Apply,
    Destroy {
        aliases: &'a [DestroyWaitAlias],
        kept_targets: &'a KeptDestroyTargets,
    },
}

/// Resolved dependency targets that exist in the expanded graph but are not
/// being operated on by this destroy run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeptDestroyTargets(HashSet<ResolvedResourceId>);

impl FromIterator<ResolvedResourceId> for KeptDestroyTargets {
    fn from_iter<T: IntoIterator<Item = ResolvedResourceId>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl KeptDestroyTargets {
    fn iter(&self) -> impl Iterator<Item = &ResolvedResourceId> {
        self.0.iter()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestroyWaitAlias {
    pub id: ResolvedResourceId,
    pub target_id: ResolvedResourceId,
    pub explicit_dependencies: ResolvedDependencies,
    pub consumers: NonEmptyVec<ResolvedResourceId>,
}

impl DestroyWaitAlias {
    /// Construct a destroy-wait alias node.
    ///
    /// Returns `None` when `consumers` is empty: an alias with no
    /// consumers carries no scheduling information, so callers should
    /// drop it rather than treat the `None` as an error.
    pub fn new(
        id: ResolvedResourceId,
        target_id: ResolvedResourceId,
        explicit_dependencies: ResolvedDependencies,
        consumers: Vec<ResolvedResourceId>,
    ) -> Option<Self> {
        Some(Self {
            id,
            target_id,
            explicit_dependencies,
            consumers: NonEmptyVec::from_vec(consumers)?,
        })
    }

    fn destroy_edges(&self) -> Vec<ScheduleEdge> {
        let mut edges = vec![ScheduleEdge::BlockedBy(self.target_id.clone())];
        edges.extend(
            self.explicit_dependencies
                .iter()
                .filter(|dependency| *dependency != &self.target_id)
                .cloned()
                .map(ScheduleEdge::BlockedBy),
        );
        edges.extend(self.consumers.iter().cloned().map(ScheduleEdge::DependsOn));
        edges
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WritesSet {
    attrs: BTreeSet<String>,
}

impl WritesSet {
    pub(crate) fn from_update<S>(effect: &Effect<S>) -> Option<Self> {
        let Effect::Update {
            changed_attributes, ..
        } = effect
        else {
            return None;
        };
        Some(Self {
            attrs: changed_attributes.iter().cloned().collect(),
        })
    }
}

pub(crate) mod reads {
    use std::collections::BTreeSet;

    use super::WritesSet;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct KnownReads {
        attrs: BTreeSet<String>,
    }

    impl KnownReads {
        pub(crate) fn from_attributes(attrs: &BTreeSet<String>) -> Self {
            Self {
                attrs: attrs.clone(),
            }
        }

        #[cfg(test)]
        pub(crate) fn from_attrs(attrs: &[&str]) -> Self {
            Self {
                attrs: attrs.iter().map(|attr| (*attr).to_string()).collect(),
            }
        }

        pub(crate) fn attrs(&self) -> &BTreeSet<String> {
            &self.attrs
        }

        fn union(mut self, other: KnownReads) -> Self {
            self.attrs.extend(other.attrs);
            self
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum ReadsSet {
        Known(KnownReads),
        Unknown,
    }

    impl ReadsSet {
        pub(crate) fn from_walker(walker_result: KnownReads) -> Self {
            Self::Known(walker_result)
        }

        pub(crate) fn unknown() -> Self {
            Self::Unknown
        }

        pub(crate) fn merge(self, other: ReadsSet) -> ReadsSet {
            match (self, other) {
                (ReadsSet::Known(a), ReadsSet::Known(b)) => ReadsSet::Known(a.union(b)),
                _ => ReadsSet::Unknown,
            }
        }

        pub(crate) fn disjoint(&self, writes: &WritesSet) -> bool {
            match self {
                ReadsSet::Known(set) => set.attrs().is_disjoint(&writes.attrs),
                ReadsSet::Unknown => false,
            }
        }

        #[cfg(test)]
        pub(crate) fn is_unknown(&self) -> bool {
            matches!(self, ReadsSet::Unknown)
        }
    }
}

use reads::{KnownReads, ReadsSet};

#[derive(Debug)]
pub struct DependencyAnalysis {
    deps_of: HashMap<usize, HashSet<usize>>,
    dependents_of: HashMap<usize, HashSet<usize>>,
    reads_by_edge: HashMap<usize, HashMap<usize, ReadsSet>>,
    unresolved_required: Vec<UnresolvedRequiredDependency>,
    unresolved_delete_ordering: Vec<UnresolvedDeleteOrderingDependency>,
}

impl DependencyAnalysis {
    fn new(effect_count: usize) -> Self {
        let deps_of = (0..effect_count).map(|idx| (idx, HashSet::new())).collect();
        let dependents_of = (0..effect_count).map(|idx| (idx, HashSet::new())).collect();
        Self {
            deps_of,
            dependents_of,
            reads_by_edge: HashMap::new(),
            unresolved_required: Vec::new(),
            unresolved_delete_ordering: Vec::new(),
        }
    }

    fn add_edge(&mut self, child: usize, parent: usize, reads: ReadsSet) {
        self.deps_of.entry(child).or_default().insert(parent);
        self.dependents_of.entry(parent).or_default().insert(child);
        self.reads_by_edge
            .entry(child)
            .or_default()
            .entry(parent)
            .and_modify(|existing| {
                let previous = std::mem::replace(existing, ReadsSet::unknown());
                *existing = previous.merge(reads.clone());
            })
            .or_insert(reads);
    }

    fn remove_edge(&mut self, child: usize, parent: usize) {
        if let Some(deps) = self.deps_of.get_mut(&child) {
            deps.remove(&parent);
        }
        if let Some(dependents) = self.dependents_of.get_mut(&parent) {
            dependents.remove(&child);
        }
    }

    pub fn deps_of(&self, child: usize) -> Option<&HashSet<usize>> {
        self.deps_of.get(&child)
    }

    pub fn dependents_of(&self, parent: usize) -> Option<&HashSet<usize>> {
        self.dependents_of.get(&parent)
    }

    pub(crate) fn reads_for_edge(&self, child: usize, parent: usize) -> Option<&ReadsSet> {
        self.reads_by_edge
            .get(&child)
            .and_then(|by_parent| by_parent.get(&parent))
    }

    pub fn into_deps_of(self) -> HashMap<usize, HashSet<usize>> {
        self.deps_of
    }

    pub fn unresolved_delete_ordering(&self) -> &[UnresolvedDeleteOrderingDependency] {
        &self.unresolved_delete_ordering
    }
}

/// A persisted ordering edge attached to a Delete whose target no longer has
/// either a configured effect or a state row. The Delete remains schedulable
/// using its known edges; callers surface this typed notice to the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedDeleteOrderingDependency {
    delete_id: ResolvedResourceId,
    generation: crate::effect::EffectGeneration,
    dependency: ResolvedResourceId,
}

impl UnresolvedDeleteOrderingDependency {
    pub fn delete_id(&self) -> &ResolvedResourceId {
        &self.delete_id
    }

    pub fn generation(&self) -> &crate::effect::EffectGeneration {
        &self.generation
    }

    pub fn dependency(&self) -> &ResolvedResourceId {
        &self.dependency
    }
}

/// A scheduling edge whose producer is required by construction but absent
/// from the effect set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedRequiredDependency {
    effect_index: usize,
    id: ResolvedResourceId,
}

impl UnresolvedRequiredDependency {
    pub fn effect_index(&self) -> usize {
        self.effect_index
    }

    pub fn id(&self) -> &ResolvedResourceId {
        &self.id
    }
}

impl std::fmt::Display for UnresolvedRequiredDependency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "effect {} requires missing scheduling target `{}`",
            self.effect_index, self.id
        )
    }
}

/// Dependency analysis failed because one or more required scheduling targets
/// could not be resolved. The partial analysis is retained so the executor can
/// fail only the affected effects without discarding unrelated ordering.
#[derive(Debug)]
pub struct DependencyAnalysisFailure(Box<DependencyAnalysis>);

impl DependencyAnalysisFailure {
    pub fn unresolved(&self) -> &[UnresolvedRequiredDependency] {
        &self.0.unresolved_required
    }

    pub fn into_parts(mut self) -> (DependencyAnalysis, Vec<UnresolvedRequiredDependency>) {
        let unresolved = std::mem::take(&mut self.0.unresolved_required);
        (*self.0, unresolved)
    }
}

impl std::fmt::Display for DependencyAnalysisFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let unresolved = self
            .unresolved()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        write!(f, "invalid effect dependency graph: {unresolved}")
    }
}

impl std::error::Error for DependencyAnalysisFailure {}

#[derive(Debug, Clone, Default)]
struct IdentityTargets {
    preferred: Option<usize>,
    alias_preferred: Option<usize>,
    delete: Option<usize>,
}

impl IdentityTargets {
    fn insert_preferred(&mut self, idx: usize) {
        self.preferred.get_or_insert(idx);
    }

    fn insert_alias_preferred(&mut self, idx: usize) {
        self.alias_preferred.get_or_insert(idx);
    }

    fn insert_delete(&mut self, idx: usize) {
        self.delete = Some(idx);
    }

    fn lookup(&self) -> Option<usize> {
        self.preferred.or(self.alias_preferred).or(self.delete)
    }

    fn lookup_delete_blockers(&self) -> Vec<usize> {
        let mut indices = Vec::new();
        if let Some(delete) = self.delete {
            indices.push(delete);
        }
        if let Some(alias) = self.alias_preferred {
            indices.push(alias);
        }
        indices
    }
}

struct DependencyAnalyzer {
    instance_to_idx: HashMap<EffectInstanceKey, IdentityTargets>,
    delete_blockers_by_id: HashMap<ResolvedResourceId, Vec<usize>>,
    known_graph_ids: HashSet<ResolvedResourceId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DependencyTargetOutcome {
    Scheduled(usize),
    Kept,
    Missing,
}

impl DependencyAnalyzer {
    fn new(
        instance_to_idx: HashMap<EffectInstanceKey, IdentityTargets>,
        delete_blockers_by_id: HashMap<ResolvedResourceId, Vec<usize>>,
        known_graph_ids: HashSet<ResolvedResourceId>,
    ) -> Self {
        Self {
            instance_to_idx,
            delete_blockers_by_id,
            known_graph_ids,
        }
    }

    fn lookup_by_id(&self, id: &ResolvedResourceId) -> Option<usize> {
        self.instance_to_idx
            .get(&EffectInstanceKey::current(id.clone()))
            .and_then(IdentityTargets::lookup)
    }

    fn target_outcome(&self, id: &ResolvedResourceId) -> DependencyTargetOutcome {
        if let Some(index) = self.lookup_by_id(id) {
            DependencyTargetOutcome::Scheduled(index)
        } else if self.known_graph_ids.contains(id) {
            DependencyTargetOutcome::Kept
        } else {
            DependencyTargetOutcome::Missing
        }
    }

    fn lookup_deletes_by_id(&self, id: &ResolvedResourceId) -> Vec<usize> {
        self.delete_blockers_by_id
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    fn record_missing(
        &self,
        analysis: &mut DependencyAnalysis,
        effect_index: usize,
        id: ResolvedResourceId,
    ) {
        analysis
            .unresolved_required
            .push(UnresolvedRequiredDependency { effect_index, id });
    }

    fn record_unresolved_delete_ordering<S>(
        &self,
        analysis: &mut DependencyAnalysis,
        effect: &Effect<S>,
        dependency: ResolvedResourceId,
    ) {
        let Effect::Delete { id, generation, .. } = effect else {
            unreachable!("only Delete edges can be non-fatal destroy-ordering warnings")
        };
        analysis
            .unresolved_delete_ordering
            .push(UnresolvedDeleteOrderingDependency {
                delete_id: id.clone(),
                generation: generation.clone(),
                dependency,
            });
    }

    fn collect_from_schedule_edges<S, E: AsRef<Effect<S>>>(
        &self,
        effects: &[E],
        edges: Vec<ScheduleEdge>,
        analysis: &mut DependencyAnalysis,
        self_idx: usize,
    ) {
        for edge in edges {
            match edge {
                ScheduleEdge::DependsOn(id) => {
                    let delete_parents = if self_idx >= effects.len() {
                        self.lookup_deletes_by_id(&id)
                    } else {
                        Vec::new()
                    };
                    if !delete_parents.is_empty() {
                        for parent in delete_parents {
                            analysis.add_edge(self_idx, parent, ReadsSet::unknown());
                        }
                    } else {
                        match self.target_outcome(&id) {
                            DependencyTargetOutcome::Scheduled(parent) => {
                                let reads = effects
                                    .get(self_idx)
                                    .and_then(|effect| effect.as_ref().known_dependency_reads(&id))
                                    .map(KnownReads::from_attributes)
                                    .map(ReadsSet::from_walker)
                                    .unwrap_or_else(ReadsSet::unknown);
                                analysis.add_edge(self_idx, parent, reads);
                            }
                            DependencyTargetOutcome::Kept => {}
                            DependencyTargetOutcome::Missing => {
                                self.record_missing(analysis, self_idx, id);
                            }
                        }
                    }
                }
                ScheduleEdge::RequiredDependsOn(id) => match self.target_outcome(&id) {
                    DependencyTargetOutcome::Scheduled(parent) => {
                        analysis.add_edge(self_idx, parent, ReadsSet::unknown());
                    }
                    DependencyTargetOutcome::Kept => {}
                    DependencyTargetOutcome::Missing => {
                        self.record_missing(analysis, self_idx, id);
                    }
                },
                ScheduleEdge::BlockedBy(id) => {
                    let is_delete_fanout_source = self_idx >= effects.len()
                        || matches!(effects[self_idx].as_ref(), Effect::Delete { .. });
                    if is_delete_fanout_source {
                        let blocked = self.lookup_deletes_by_id(&id);
                        for blocked_idx in &blocked {
                            if *blocked_idx != self_idx {
                                analysis.add_edge(*blocked_idx, self_idx, ReadsSet::unknown());
                            }
                        }
                        if blocked.is_empty()
                            && matches!(self.target_outcome(&id), DependencyTargetOutcome::Missing)
                        {
                            if self_idx < effects.len()
                                && matches!(effects[self_idx].as_ref(), Effect::Delete { .. })
                            {
                                self.record_unresolved_delete_ordering(
                                    analysis,
                                    effects[self_idx].as_ref(),
                                    id,
                                );
                            } else {
                                self.record_missing(analysis, self_idx, id);
                            }
                        }
                    } else {
                        match self.target_outcome(&id) {
                            DependencyTargetOutcome::Scheduled(blocked_idx) => {
                                analysis.add_edge(blocked_idx, self_idx, ReadsSet::unknown());
                            }
                            DependencyTargetOutcome::Kept => {}
                            DependencyTargetOutcome::Missing => {
                                self.record_missing(analysis, self_idx, id);
                            }
                        }
                    }
                }
                ScheduleEdge::BlockedByIfDelete(id) => {
                    let blocked = self.lookup_deletes_by_id(&id);
                    for blocked_idx in &blocked {
                        if *blocked_idx != self_idx
                            && *blocked_idx < effects.len()
                            && matches!(effects[*blocked_idx].as_ref(), Effect::Delete { .. })
                        {
                            analysis.add_edge(*blocked_idx, self_idx, ReadsSet::unknown());
                        }
                    }
                    if blocked.is_empty()
                        && matches!(self.target_outcome(&id), DependencyTargetOutcome::Missing)
                    {
                        self.record_unresolved_delete_ordering(
                            analysis,
                            effects[self_idx].as_ref(),
                            id,
                        );
                    }
                }
            }
        }
    }
}

/// Build the scheduling dependency graph for a slice of effects.
///
/// Single entry point shared by the parallel scheduler, the phased
/// scheduler, and the destroy CLI driver. `inputs` chooses between the
/// apply contract and the destroy contract (see [`ScheduleInputs`]); the
/// resulting [`DependencyAnalysis`] is indexed by effect position, with
/// any destroy `aliases` appearing at indices `effects.len()..` so callers
/// can distinguish real effects from wait-binding bridges.
pub fn build_effect_dependency_analysis<'a, S, E>(
    effects: &[E],
    unresolved_resource_ids: impl IntoIterator<Item = &'a ResolvedResourceId>,
    inputs: ScheduleInputs<'_>,
) -> Result<DependencyAnalysis, DependencyAnalysisFailure>
where
    E: AsRef<Effect<S>>,
{
    build_effect_dependency_analysis_resolved(
        effects,
        unresolved_resource_ids.into_iter().cloned().collect(),
        inputs,
    )
}

fn build_effect_dependency_analysis_resolved<E, S>(
    effects: &[E],
    mut known_graph_ids: HashSet<ResolvedResourceId>,
    inputs: ScheduleInputs<'_>,
) -> Result<DependencyAnalysis, DependencyAnalysisFailure>
where
    E: AsRef<Effect<S>>,
{
    let mut instance_to_idx: HashMap<EffectInstanceKey, IdentityTargets> = HashMap::new();
    let aliases = match inputs {
        ScheduleInputs::Apply => &[][..],
        ScheduleInputs::Destroy {
            aliases,
            kept_targets,
        } => {
            known_graph_ids.extend(kept_targets.iter().cloned());
            aliases
        }
    };
    for (idx, effect) in effects.iter().enumerate() {
        let effect = effect.as_ref();
        match effect {
            Effect::Delete { .. } => instance_to_idx
                .entry(effect.instance_key())
                .or_default()
                .insert_delete(idx),
            // A release changes only the authoring record in locked state. It
            // neither produces provider-visible values nor represents a
            // provider operation that consumers should wait for. When the
            // same resource also updates, that Update remains the scheduling
            // target regardless of effect order.
            Effect::ReleaseListKeys { .. } => {}
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::Wait { .. }
            | Effect::DeferredCreate { .. }
            | Effect::DeferredReplace(_) => instance_to_idx
                .entry(effect.instance_key())
                .or_default()
                .insert_preferred(idx),
        }
    }
    let alias_offset = effects.len();
    for (alias_idx, alias) in aliases.iter().enumerate() {
        instance_to_idx
            .entry(EffectInstanceKey::current(alias.id.clone()))
            .or_default()
            .insert_alias_preferred(alias_offset + alias_idx);
    }

    let mut delete_blockers_by_id: HashMap<ResolvedResourceId, Vec<usize>> = HashMap::new();
    for (key, targets) in &instance_to_idx {
        let blockers = targets.lookup_delete_blockers();
        if !blockers.is_empty() {
            delete_blockers_by_id
                .entry(key.id().clone())
                .or_default()
                .extend(blockers);
        }
    }
    for blockers in delete_blockers_by_id.values_mut() {
        blockers.sort_unstable();
        blockers.dedup();
    }

    let analyzer = DependencyAnalyzer::new(instance_to_idx, delete_blockers_by_id, known_graph_ids);
    let mut analysis = DependencyAnalysis::new(effects.len() + aliases.len());
    add_same_identity_replacement_order_edges(effects, &mut analysis);

    for (idx, effect) in effects.iter().enumerate() {
        let effect = effect.as_ref();
        match inputs {
            ScheduleInputs::Apply => {
                analyzer.collect_from_schedule_edges(
                    effects,
                    effect.apply_edges(),
                    &mut analysis,
                    idx,
                );
            }
            ScheduleInputs::Destroy { .. } => {
                analyzer.collect_from_schedule_edges(
                    effects,
                    effect.destroy_edges(),
                    &mut analysis,
                    idx,
                );
            }
        }
    }
    if let ScheduleInputs::Destroy { .. } = inputs {
        for (alias_idx, alias) in aliases.iter().enumerate() {
            analyzer.collect_from_schedule_edges(
                effects,
                alias.destroy_edges(),
                &mut analysis,
                alias_offset + alias_idx,
            );
        }
    }

    if analysis.unresolved_required.is_empty() {
        Ok(analysis)
    } else {
        Err(DependencyAnalysisFailure(Box::new(analysis)))
    }
}

fn add_same_identity_replacement_order_edges<S, E: AsRef<Effect<S>>>(
    effects: &[E],
    analysis: &mut DependencyAnalysis,
) {
    let mut previous_by_instance: HashMap<EffectInstanceKey, usize> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        let effect = effect.as_ref();
        if !matches!(effect, Effect::Create(_) | Effect::Delete { .. }) {
            continue;
        }

        let instance = effect.instance_key();
        if let Some(previous) = previous_by_instance.insert(instance, idx) {
            analysis.add_edge(idx, previous, ReadsSet::unknown());
        }
    }
}

pub fn relax_update_update_edges<S, E: AsRef<Effect<S>>>(
    effects: &[E],
    analysis: &mut DependencyAnalysis,
) {
    for child in 0..effects.len() {
        if !matches!(effects[child].as_ref(), Effect::Update { .. }) {
            continue;
        }
        let Some(parents) = analysis.deps_of(child).cloned() else {
            continue;
        };
        for parent in parents {
            let Some(writes) = WritesSet::from_update(effects[parent].as_ref()) else {
                continue;
            };
            let Some(reads) = analysis.reads_for_edge(child, parent) else {
                continue;
            };
            if reads.disjoint(&writes) {
                analysis.remove_edge(child, parent);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{
        ResolvedDependencyReads, ResolvedResource, ResolvedResourceId, ResourceIdentity, State,
        Value,
    };

    fn test_id(identity: &str) -> ResolvedResourceId {
        ResolvedResourceId::with_identity(
            "test",
            ResourceIdentity::try_from(identity.to_string())
                .expect("test identity must be non-empty"),
        )
    }

    fn resolved_dependencies(names: &[&str]) -> ResolvedDependencies {
        names.iter().map(|name| test_id(name)).collect()
    }

    fn state_for(id: &ResourceId) -> State {
        State::existing(id.clone(), Default::default())
            .with_identifier(id.identity_display().to_string())
    }

    fn update_effect(binding: &'static str, refs: &[(&str, &str)], changed: &[&str]) -> Effect {
        let mut resource = Resource::new("test", binding);
        resource.binding = Some(binding.to_string());
        let mut dependencies = ResolvedDependencies::default();
        let mut reads = ResolvedDependencyReads::default();
        for (dep, attr) in refs {
            resource.set_attr(
                format!("{}_{}", dep, attr),
                Value::resource_ref((*dep).to_string(), (*attr).to_string(), vec![]),
            );
            let id = test_id(dep);
            dependencies.insert(id.clone());
            reads.insert_known(id, *attr);
        }
        Effect::Update {
            identifier: crate::effect::fixture_applied_identifier(
                resource.id.identity_display().to_string(),
            ),
            from: Box::new(state_for(&resource.id)),
            to: ResolvedResource::from_resolved_parts_with_reads(
                resource,
                dependencies,
                ResolvedDependencies::default(),
                reads,
            ),
            changed_attributes: changed.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn create_effect(binding: &'static str) -> Effect {
        let mut resource = Resource::new("test", binding);
        resource.binding = Some(binding.to_string());
        Effect::Create(ResolvedResource::new(resource))
    }

    fn delete_effect(
        identity: &'static str,
        dependencies: &[&str],
        blocked_by_updates: &[&str],
    ) -> Effect {
        Effect::Delete {
            id: test_id(identity),
            identifier: crate::effect::fixture_applied_identifier(format!("{identity}-id")),
            generation: crate::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: Some(identity.to_string()),
            dependencies: resolved_dependencies(dependencies),
            explicit_dependencies: Default::default(),
            blocked_by_updates: resolved_dependencies(blocked_by_updates),
        }
    }

    fn deposed_delete_effect(identity: &'static str, dependencies: &[&str]) -> Effect {
        let mut effect = delete_effect(identity, dependencies, &[]);
        if let Effect::Delete { generation, .. } = &mut effect {
            *generation =
                crate::effect::EffectGeneration::Deposed(crate::resource::DeposedKey::new_unique());
        }
        effect
    }

    fn analyzer_from_instance_to_idx(
        instance_to_idx: HashMap<EffectInstanceKey, IdentityTargets>,
    ) -> DependencyAnalyzer {
        let mut delete_blockers_by_id: HashMap<ResolvedResourceId, Vec<usize>> = HashMap::new();
        for (key, targets) in &instance_to_idx {
            let blockers = targets.lookup_delete_blockers();
            if !blockers.is_empty() {
                delete_blockers_by_id
                    .entry(key.id().clone())
                    .or_default()
                    .extend(blockers);
            }
        }
        for blockers in delete_blockers_by_id.values_mut() {
            blockers.sort_unstable();
            blockers.dedup();
        }
        DependencyAnalyzer::new(instance_to_idx, delete_blockers_by_id, HashSet::new())
    }

    fn total_edges(deps: &HashMap<usize, HashSet<usize>>) -> usize {
        deps.values().map(HashSet::len).sum()
    }

    #[test]
    fn effect_identity_returns_let_binding_for_named_effect() {
        let mut resource = Resource::new("test", "named");
        resource.binding = Some("named".to_string());
        let effect = Effect::Create(ResolvedResource::new(resource));

        assert_eq!(effect.identity(), ResourceIdentity::new("named"));
    }

    #[test]
    fn effect_identity_returns_system_hash_for_anonymous_effect() {
        let anonymous: Effect = Effect::Delete {
            id: ResolvedResourceId::new(ResourceId::with_identity("test", "anon_1234")),
            identifier: crate::effect::fixture_applied_identifier("anonymous-id"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        let mut named_resource = Resource::new("test", "named");
        named_resource.binding = Some("named".to_string());
        let named = Effect::Create(ResolvedResource::new(named_resource));

        assert_eq!(anonymous.identity(), ResourceIdentity::new("anon_1234"));
        assert_ne!(anonymous.identity(), named.identity());
    }

    #[test]
    fn scheduler_index_keys_on_full_resolved_id() {
        let first = test_id("first");
        let second = test_id("second");
        let mut first_targets = IdentityTargets::default();
        first_targets.insert_preferred(0);
        let mut second_targets = IdentityTargets::default();
        second_targets.insert_preferred(1);
        let analyzer = analyzer_from_instance_to_idx(HashMap::from([
            (
                crate::effect::EffectInstanceKey::current(first.clone()),
                first_targets,
            ),
            (
                crate::effect::EffectInstanceKey::current(second.clone()),
                second_targets,
            ),
        ]));

        assert_eq!(analyzer.lookup_by_id(&first), Some(0));
        assert_eq!(analyzer.lookup_by_id(&second), Some(1));
    }

    #[test]
    fn delete_apply_edge_for_dependency_uses_blocked_by_if_delete() {
        let delete_target = delete_effect("foo", &[], &[]);
        let dependent_delete = delete_effect("dependent", &["foo"], &[]);
        let deps = build_effect_dependency_analysis(
            &[delete_target, dependent_delete],
            std::iter::empty(),
            ScheduleInputs::Apply,
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert_eq!(total_edges(&deps), 1);
        assert!(
            deps[&0].contains(&1),
            "dependency delete must wait for the dependent delete"
        );

        let create_target = create_effect("foo");
        let dependent_delete = delete_effect("dependent", &["foo"], &[]);
        let deps = build_effect_dependency_analysis(
            &[create_target, dependent_delete],
            std::iter::empty(),
            ScheduleInputs::Apply,
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();
        assert_eq!(
            total_edges(&deps),
            0,
            "create targets must not be blocked by delete dependencies on apply"
        );

        let update_target = update_effect("foo", &[], &["tags"]);
        let dependent_delete = delete_effect("dependent", &["foo"], &[]);
        let deps = build_effect_dependency_analysis(
            &[update_target, dependent_delete],
            std::iter::empty(),
            ScheduleInputs::Apply,
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();
        assert_eq!(
            total_edges(&deps),
            0,
            "update targets must not be blocked by delete dependencies on apply"
        );
    }

    #[test]
    fn apply_orphan_delete_with_missing_persisted_dependency_is_a_typed_warning() {
        let delete = delete_effect("orphan", &["vanished"], &[]);

        let analysis =
            build_effect_dependency_analysis(&[delete], std::iter::empty(), ScheduleInputs::Apply)
                .expect("a vanished persisted destroy dependency must not abort apply");

        assert!(analysis.deps_of(0).expect("delete analysis").is_empty());
        let [warning] = analysis.unresolved_delete_ordering() else {
            panic!("expected one typed destroy-ordering warning");
        };
        assert_eq!(warning.delete_id(), &test_id("orphan"));
        assert_eq!(
            warning.generation(),
            &crate::effect::EffectGeneration::Current
        );
        assert_eq!(warning.dependency(), &test_id("vanished"));
    }

    #[test]
    fn dependency_analysis_api_does_not_require_compositions() {
        let effects = [create_effect("standalone")];
        let analysis =
            build_effect_dependency_analysis(&effects, std::iter::empty(), ScheduleInputs::Apply)
                .expect("composition-independent dependency analysis must resolve");

        assert_eq!(analysis.deps_of(0), Some(&HashSet::new()));
    }

    #[test]
    fn dependency_analysis_origin_keys_are_resolved_resource_ids() {
        let effect = create_effect("resolved");
        let Effect::Create(resource) = &effect else {
            unreachable!("fixture is a create effect")
        };
        let origins = HashMap::from([(
            resource.resolved_id(),
            UnresolvedResource::from_pre_resolve(resource.as_inner().clone()),
        )]);

        build_effect_dependency_analysis(&[effect], origins.keys(), ScheduleInputs::Apply)
            .expect("resolved origin keys must be accepted without a runtime conversion");
    }

    #[test]
    fn deposed_delete_dependency_edges_resolve_target_deposed_generation() {
        let parent_delete = deposed_delete_effect("parent", &[]);
        let child_delete = deposed_delete_effect("child", &["parent"]);

        let deps = build_effect_dependency_analysis(
            &[parent_delete, child_delete],
            std::iter::empty(),
            ScheduleInputs::Apply,
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert!(
            deps[&0].contains(&1),
            "deposed parent delete must wait for deposed child delete"
        );
    }

    #[test]
    fn delete_apply_edge_for_blocked_by_updates_uses_depends_on() {
        let consumer = update_effect("consumer", &[], &["acl_id"]);
        let delete = delete_effect("old_web_acl", &[], &["consumer"]);

        let deps = build_effect_dependency_analysis(
            &[consumer, delete],
            std::iter::empty(),
            ScheduleInputs::Apply,
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert_eq!(total_edges(&deps), 1);
        assert!(
            deps[&1].contains(&0),
            "delete must wait for the consumer update"
        );
    }

    #[test]
    fn delete_apply_edge_for_blocked_by_updates_rejects_missing_target() {
        let delete = delete_effect("old_web_acl", &[], &["missing_consumer"]);

        let failure =
            build_effect_dependency_analysis(&[delete], std::iter::empty(), ScheduleInputs::Apply)
                .expect_err("a required delete gate must not be silently dropped");

        assert_eq!(failure.unresolved().len(), 1);
        assert_eq!(failure.unresolved()[0].effect_index(), 0);
        assert_eq!(failure.unresolved()[0].id(), &test_id("missing_consumer"));
    }

    #[test]
    fn delete_destroy_edge_ignores_blocked_by_updates() {
        let parent = delete_effect("parent", &[], &[]);
        let consumer = update_effect("consumer", &[], &["acl_id"]);
        let child = delete_effect("child", &["parent"], &["consumer"]);

        let deps = build_effect_dependency_analysis(
            &[parent, consumer, child],
            std::iter::empty(),
            ScheduleInputs::Destroy {
                aliases: &[],
                kept_targets: &KeptDestroyTargets::default(),
            },
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert_eq!(total_edges(&deps), 1);
        assert!(
            deps[&0].contains(&2),
            "destroy dependencies must remain unconditional"
        );
        assert!(
            deps[&2].is_empty(),
            "blocked_by_updates must not contribute destroy edges"
        );
    }

    #[test]
    fn cbd_ordering_unit_test_via_scheduler_edges() {
        let create = create_effect("new_web_acl");
        let consumer = update_effect("consumer", &[], &["web_acl_id"]);
        let delete = delete_effect("old_web_acl", &[], &["consumer"]);

        let deps = build_effect_dependency_analysis(
            &[create, consumer, delete],
            std::iter::empty(),
            ScheduleInputs::Apply,
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert!(
            deps[&1].is_empty(),
            "consumer update should have no create/delete dependency in this fixture"
        );
        assert!(
            deps[&2].contains(&1),
            "old web ACL delete must wait for the consumer update"
        );
        assert_eq!(total_edges(&deps), 1);
    }

    #[test]
    fn blocked_by_if_delete_resolves_anonymous_delete_through_identity_index() {
        let delete = Effect::Delete {
            id: ResolvedResourceId::new(ResourceId::with_identity("test", "foo")),
            identifier: crate::effect::fixture_applied_identifier("foo-id"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        let mut blocker_resource = Resource::new("test", "blocker");
        blocker_resource.binding = Some("blocker".to_string());
        let blocker = Effect::Create(ResolvedResource::new(blocker_resource));
        let effects = vec![delete, blocker];
        let mut foo_targets = IdentityTargets::default();
        foo_targets.insert_delete(0);
        let analyzer = analyzer_from_instance_to_idx(HashMap::from([(
            crate::effect::EffectInstanceKey::current(test_id("foo")),
            foo_targets,
        )]));
        let mut analysis = DependencyAnalysis::new(effects.len());

        analyzer.collect_from_schedule_edges(
            &effects,
            vec![ScheduleEdge::BlockedByIfDelete(test_id("foo"))],
            &mut analysis,
            1,
        );

        assert!(
            analysis.into_deps_of()[&0].contains(&1),
            "anonymous delete must wait for the effect that blocks its identity"
        );
    }

    #[test]
    fn typed_dependency_resolves_to_anonymous_delete_by_full_address() {
        let delete = Effect::Delete {
            id: ResolvedResourceId::new(ResourceId::with_identity("test", "foo")),
            identifier: crate::effect::fixture_applied_identifier("foo-id"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: None,
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        let mut consumer_resource = Resource::new("test", "consumer");
        consumer_resource.binding = Some("consumer".to_string());
        let consumer = Effect::Create(ResolvedResource::from_resolved_parts(
            consumer_resource,
            resolved_dependencies(&["foo"]),
            ResolvedDependencies::default(),
        ));

        let deps = build_effect_dependency_analysis(
            &[delete, consumer],
            std::iter::empty(),
            ScheduleInputs::Apply,
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert!(
            deps[&1].contains(&0),
            "typed dependencies resolve by exact ResolvedResourceId match"
        );
    }

    #[test]
    fn destroy_delete_edges_block_dependencies_by_consumers() {
        let parent: Effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test", "parent",
            )),
            identifier: crate::effect::fixture_applied_identifier("parent-id"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: Some("parent".to_string()),
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        let child = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test", "child",
            )),
            identifier: crate::effect::fixture_applied_identifier("child-id"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: Some("child".to_string()),
            dependencies: resolved_dependencies(&["parent"]),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        let effects = vec![parent, child];

        let deps = build_effect_dependency_analysis(
            &effects,
            std::iter::empty(),
            ScheduleInputs::Destroy {
                aliases: &[],
                kept_targets: &KeptDestroyTargets::default(),
            },
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert!(deps[&0].contains(&1), "parent delete must wait for child");
    }

    #[test]
    fn destroy_wait_alias_bridges_target_to_consumers() {
        let cert: Effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity("test", "cert")),
            identifier: crate::effect::fixture_applied_identifier("cert-id"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: Some("cert".to_string()),
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        let listener = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test", "listener",
            )),
            identifier: crate::effect::fixture_applied_identifier("listener-id"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: Some("listener".to_string()),
            dependencies: resolved_dependencies(&["cert_issued"]),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        let wait = DestroyWaitAlias::new(
            test_id("cert_issued"),
            test_id("cert"),
            ResolvedDependencies::default(),
            vec![test_id("listener")],
        )
        .expect("test alias has a consumer");
        let effects = vec![cert, listener];

        let deps = build_effect_dependency_analysis(
            &effects,
            std::iter::empty(),
            ScheduleInputs::Destroy {
                aliases: &[wait],
                kept_targets: &KeptDestroyTargets::default(),
            },
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert!(deps[&2].contains(&1), "wait must wait for listener");
        assert!(deps[&0].contains(&2), "cert must wait for wait alias");
    }

    #[test]
    fn destroy_wait_alias_orders_deposed_delete_before_wait_target_delete() {
        let cert = delete_effect("cert", &[], &[]);
        let listener = deposed_delete_effect("listener", &["cert_issued"]);
        let wait = DestroyWaitAlias::new(
            test_id("cert_issued"),
            test_id("cert"),
            ResolvedDependencies::default(),
            vec![test_id("listener")],
        )
        .expect("test alias has a deposed consumer");
        let effects = vec![cert, listener];

        let deps = build_effect_dependency_analysis(
            &effects,
            std::iter::empty(),
            ScheduleInputs::Destroy {
                aliases: &[wait],
                kept_targets: &KeptDestroyTargets::default(),
            },
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert!(
            deps[&2].contains(&1),
            "wait alias must wait for the deposed listener delete"
        );
        assert!(
            deps[&0].contains(&2),
            "wait target delete must wait for the alias after the deposed listener"
        );
    }

    #[test]
    fn destroy_wait_alias_orders_deposed_wait_target_after_consumers() {
        let cert = deposed_delete_effect("cert", &[]);
        let listener = delete_effect("listener", &["cert_issued"], &[]);
        let wait = DestroyWaitAlias::new(
            test_id("cert_issued"),
            test_id("cert"),
            ResolvedDependencies::default(),
            vec![test_id("listener")],
        )
        .expect("test alias has a consumer");
        let effects = vec![cert, listener];

        let deps = build_effect_dependency_analysis(
            &effects,
            std::iter::empty(),
            ScheduleInputs::Destroy {
                aliases: &[wait],
                kept_targets: &KeptDestroyTargets::default(),
            },
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert!(deps[&2].contains(&1), "wait must wait for listener");
        assert!(
            deps[&0].contains(&2),
            "deposed wait target delete must wait for the alias after its consumers"
        );
    }

    #[test]
    fn destroy_wait_alias_rejects_empty_consumers() {
        assert!(
            DestroyWaitAlias::new(
                test_id("w"),
                test_id("t"),
                ResolvedDependencies::default(),
                vec![],
            )
            .is_none()
        );
    }

    #[test]
    fn unknown_destroy_dependency_address_is_a_typed_warning() {
        let orphan: Effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test", "listener",
            )),
            identifier: crate::effect::fixture_applied_identifier("listener-id"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: Some("listener".to_string()),
            dependencies: resolved_dependencies(&["missing_wait"]),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        let analysis = build_effect_dependency_analysis(
            &[orphan],
            std::iter::empty(),
            ScheduleInputs::Destroy {
                aliases: &[],
                kept_targets: &KeptDestroyTargets::default(),
            },
        )
        .expect("an orphan Delete remains schedulable with its known edges");

        assert_eq!(analysis.unresolved_delete_ordering().len(), 1);
        let warning = &analysis.unresolved_delete_ordering()[0];
        assert_eq!(warning.delete_id(), &test_id("listener"));
        assert_eq!(
            warning.generation(),
            &crate::effect::EffectGeneration::Current
        );
        assert_eq!(warning.dependency(), &test_id("missing_wait"));
    }

    #[test]
    fn destroy_accepts_resolved_non_effect_dependency_target() {
        let query = test_id("query");
        let consumer: Effect = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test", "consumer",
            )),
            identifier: crate::effect::fixture_applied_identifier("consumer-id"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Default::default(),
            binding: Some("consumer".to_string()),
            dependencies: std::iter::once(query.clone()).collect(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        let kept_targets = std::iter::once(query).collect();

        let analysis = build_effect_dependency_analysis(
            &[consumer],
            std::iter::empty(),
            ScheduleInputs::Destroy {
                aliases: &[],
                kept_targets: &kept_targets,
            },
        )
        .expect("a known data-source-like graph node is not a missing delete target");

        assert!(analysis.into_deps_of()[&0].is_empty());
    }

    #[test]
    fn deposed_delete_and_current_create_same_identity_have_no_replacement_edge() {
        let delete = Effect::Delete {
            id: ResolvedResourceId::new(ResourceId::with_identity("test", "thing")),
            identifier: crate::effect::fixture_applied_identifier("thing-old"),
            generation: crate::effect::EffectGeneration::Deposed(
                crate::resource::DeposedKey::new_unique(),
            ),
            directives: Default::default(),
            binding: Some("thing".to_string()),
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        };
        let create = create_effect("thing");

        let deps = build_effect_dependency_analysis(
            &[delete, create],
            std::iter::empty(),
            ScheduleInputs::Apply,
        )
        .expect("dependency analysis must resolve")
        .into_deps_of();

        assert!(
            !deps[&1].contains(&0),
            "current create must not be ordered after deposed delete as a replacement"
        );
    }

    #[test]
    fn release_is_never_the_dependency_target_for_a_same_resource_update() {
        let release = Effect::ReleaseListKeys {
            id: ResolvedResourceId::new(ResourceId::with_identity("test", "parent")),
            attribute: "settings".to_string(),
            key_field: crate::schema::FieldName::declared("key"),
            keys: crate::effect::ReleasedListKeys::try_new(vec![
                crate::effect::ReleasedListKey::new(
                    "b",
                    Value::Concrete(crate::resource::ConcreteValue::String(
                        "2".to_string().into(),
                    )),
                ),
            ])
            .expect("test release contains one key"),
        };
        let effects = vec![
            release,
            update_effect("parent", &[], &["settings"]),
            update_effect("child", &[("parent", "id")], &["name"]),
        ];
        let unresolved: HashMap<ResolvedResourceId, UnresolvedResource> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Update { to, .. } => Some((
                    to.resolved_id(),
                    UnresolvedResource::from_pre_resolve(to.as_inner().clone()),
                )),
                Effect::Read { .. }
                | Effect::Create(_)
                | Effect::ResumeCreate { .. }
                | Effect::Delete { .. }
                | Effect::Import { .. }
                | Effect::Remove { .. }
                | Effect::Move { .. }
                | Effect::ReleaseListKeys { .. }
                | Effect::Wait { .. }
                | Effect::DeferredCreate { .. }
                | Effect::DeferredReplace(_) => None,
            })
            .collect();

        let deps =
            build_effect_dependency_analysis(&effects, unresolved.keys(), ScheduleInputs::Apply)
                .expect("dependency analysis must resolve")
                .into_deps_of();

        assert!(
            deps[&2].contains(&1),
            "consumer must wait for the provider Update"
        );
        assert!(
            !deps[&2].contains(&0),
            "consumer must never wait for the state-only release"
        );
    }

    #[test]
    fn relax_update_update_edge_when_child_reads_disjoint_attribute() {
        let effects = vec![
            update_effect("parent", &[], &["tags"]),
            update_effect("child", &[("parent", "id")], &["tags"]),
        ];
        let unresolved: HashMap<ResolvedResourceId, UnresolvedResource> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Update { to, .. } => Some((
                    to.resolved_id(),
                    UnresolvedResource::from_pre_resolve(to.as_inner().clone()),
                )),
                _ => None,
            })
            .collect();
        let mut analysis =
            build_effect_dependency_analysis(&effects, unresolved.keys(), ScheduleInputs::Apply)
                .expect("dependency analysis must resolve");
        relax_update_update_edges(&effects, &mut analysis);

        assert!(!analysis.into_deps_of()[&1].contains(&0));
    }
}
