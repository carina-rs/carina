use std::collections::{BTreeSet, HashMap, HashSet};

use crate::effect::{Effect, ScheduleEdge};
use crate::non_empty::NonEmptyVec;
use crate::parser::ResourceRef;
use crate::resource::{Resource, ResourceId};

#[derive(Debug, Clone)]
pub struct UnresolvedResource(Resource);

impl UnresolvedResource {
    /// Pre-resolution snapshot used by dependency analysis and apply-time
    /// reference re-resolution.
    pub fn from_pre_resolve(resource: Resource) -> Self {
        Self(resource)
    }

    pub fn as_resource(&self) -> &Resource {
        &self.0
    }
}

/// Selects which scheduling contract [`build_effect_dependency_analysis`]
/// applies.
///
/// `Apply` follows the apply scheduler's rules (resource refs become edges,
/// `Replace` from-bindings block only when they resolve to deletes, meta
/// effects contribute `DependsOn` edges). `Destroy` ignores resource-ref
/// edges and instead lets each effect's [`Effect::destroy_edges`] drive the
/// graph; the typed `aliases` slot carries `wait`-binding bridges that
/// would otherwise be dropped from the destroy plan, and the variant shape
/// makes it impossible to pass aliases into an apply run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleInputs<'a> {
    Apply,
    Destroy { aliases: &'a [DestroyWaitAlias] },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestroyWaitAlias {
    pub binding: String,
    pub target_binding: String,
    pub explicit_dependencies: HashSet<String>,
    pub consumers: NonEmptyVec<String>,
}

impl DestroyWaitAlias {
    /// Construct a destroy-wait alias node.
    ///
    /// Returns `None` when `consumers` is empty: an alias with no
    /// consumers carries no scheduling information, so callers should
    /// drop it rather than treat the `None` as an error.
    pub fn new(
        binding: String,
        target_binding: String,
        explicit_dependencies: HashSet<String>,
        consumers: Vec<String>,
    ) -> Option<Self> {
        Some(Self {
            binding,
            target_binding,
            explicit_dependencies,
            consumers: NonEmptyVec::from_vec(consumers)?,
        })
    }

    fn destroy_edges(&self) -> Vec<ScheduleEdge> {
        let mut edges = vec![ScheduleEdge::BlockedBy(self.target_binding.clone())];
        edges.extend(
            self.explicit_dependencies
                .iter()
                .filter(|dependency| dependency.as_str() != self.target_binding)
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
    pub(crate) fn from_update(effect: &Effect) -> Option<Self> {
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

    use crate::resource::AccessPath;

    use super::WritesSet;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct KnownReads {
        attrs: BTreeSet<String>,
    }

    impl KnownReads {
        pub(crate) fn from_walker(path: &AccessPath) -> Self {
            let mut attrs = BTreeSet::new();
            attrs.insert(path.attribute().to_string());
            Self { attrs }
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

pub struct DependencyAnalysis {
    deps_of: HashMap<usize, HashSet<usize>>,
    dependents_of: HashMap<usize, HashSet<usize>>,
    reads_by_edge: HashMap<usize, HashMap<usize, ReadsSet>>,
}

impl DependencyAnalysis {
    fn new(effect_count: usize) -> Self {
        let deps_of = (0..effect_count).map(|idx| (idx, HashSet::new())).collect();
        let dependents_of = (0..effect_count).map(|idx| (idx, HashSet::new())).collect();
        Self {
            deps_of,
            dependents_of,
            reads_by_edge: HashMap::new(),
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
}

struct DependencyAnalyzer {
    binding_to_idx: HashMap<String, usize>,
    binding_to_final_idx: HashMap<String, usize>,
    name_to_delete_idx: HashMap<String, usize>,
    compositions_by_binding: HashMap<String, crate::resource::Composition>,
}

impl DependencyAnalyzer {
    fn new(
        binding_to_idx: HashMap<String, usize>,
        binding_to_final_idx: HashMap<String, usize>,
        name_to_delete_idx: HashMap<String, usize>,
        compositions: &[crate::resource::Composition],
    ) -> Self {
        let compositions_by_binding = compositions
            .iter()
            .filter_map(|composition| {
                composition
                    .binding
                    .clone()
                    .map(|binding| (binding, composition.clone()))
            })
            .collect();
        Self {
            binding_to_idx,
            binding_to_final_idx,
            name_to_delete_idx,
            compositions_by_binding,
        }
    }

    fn lookup_initial_idx(&self, binding: &str) -> Option<usize> {
        self.binding_to_idx.get(binding).copied()
    }

    fn lookup_delete_idx(&self, binding: &str) -> Option<usize> {
        self.name_to_delete_idx.get(binding).copied()
    }

    fn lookup_idxs(&self, binding: &str) -> Vec<usize> {
        let mut out = Vec::with_capacity(2);
        if let Some(initial) = self.binding_to_idx.get(binding).copied() {
            out.push(initial);
        }
        if let Some(final_idx) = self.binding_to_final_idx.get(binding).copied()
            && !out.contains(&final_idx)
        {
            out.push(final_idx);
        }
        if out.is_empty()
            && let Some(delete_idx) = self.name_to_delete_idx.get(binding).copied()
        {
            out.push(delete_idx);
        }
        out
    }

    fn collect_from_schedule_edges(
        &self,
        edges: Vec<ScheduleEdge>,
        analysis: &mut DependencyAnalysis,
        self_idx: usize,
    ) {
        for edge in edges {
            match edge {
                ScheduleEdge::DependsOn(binding) => {
                    self.record_binding_edge(&binding, ReadsSet::unknown(), analysis, self_idx);
                }
                ScheduleEdge::DependsOnCreated(binding) => {
                    if let Some(parent) = self.lookup_initial_idx(&binding) {
                        analysis.add_edge(self_idx, parent, ReadsSet::unknown());
                    }
                }
                ScheduleEdge::BlockedBy(binding) => {
                    for blocked_idx in self.lookup_idxs(&binding) {
                        analysis.add_edge(blocked_idx, self_idx, ReadsSet::unknown());
                    }
                }
                ScheduleEdge::BlockedByIfDelete(binding) => {
                    if let Some(blocked_idx) = self.lookup_delete_idx(&binding) {
                        analysis.add_edge(blocked_idx, self_idx, ReadsSet::unknown());
                    } else {
                        tracing::warn!(
                            binding = %binding,
                            effect_idx = self_idx,
                            "BlockedByIfDelete target did not resolve to a Delete effect; dropping scheduler edge"
                        );
                    }
                }
            }
        }
    }

    fn collect_from_resource_ref(
        &self,
        resource: ResourceRef<'_>,
        analysis: &mut DependencyAnalysis,
        child: usize,
    ) {
        let attrs = resource.attributes();
        let mut bindings_seen_in_values = HashSet::new();
        for value in attrs.values() {
            value.visit_resource_refs(&mut |path| {
                bindings_seen_in_values.insert(path.binding().to_string());
                self.record_binding_edge(
                    path.binding(),
                    ReadsSet::from_walker(KnownReads::from_walker(path)),
                    analysis,
                    child,
                );
            });
            value.visit_binding_refs(&mut |binding| {
                bindings_seen_in_values.insert(binding.to_string());
                self.record_binding_edge(binding, ReadsSet::unknown(), analysis, child);
            });
        }
        for binding in resource.dependency_bindings() {
            if !bindings_seen_in_values.contains(binding) {
                self.record_binding_edge(binding, ReadsSet::unknown(), analysis, child);
            }
        }
        if let Some(directives) = resource.directives() {
            for binding in &directives.depends_on {
                self.record_binding_edge(binding, ReadsSet::unknown(), analysis, child);
            }
        }
    }

    fn collect_from_resource(
        &self,
        resource: &Resource,
        analysis: &mut DependencyAnalysis,
        child: usize,
    ) {
        self.collect_from_resource_ref(ResourceRef::Resource(resource), analysis, child);
    }

    fn record_binding_edge(
        &self,
        binding: &str,
        reads: ReadsSet,
        analysis: &mut DependencyAnalysis,
        child: usize,
    ) {
        let mut visited = HashSet::new();
        self.record_binding_edge_inner(binding, reads, analysis, child, &mut visited);
    }

    fn record_binding_edge_inner<'a>(
        &'a self,
        binding: &'a str,
        reads: ReadsSet,
        analysis: &mut DependencyAnalysis,
        child: usize,
        visited: &mut HashSet<&'a str>,
    ) {
        if !visited.insert(binding) {
            return;
        }
        let parents = self.lookup_idxs(binding);
        if !parents.is_empty() {
            for parent in parents {
                analysis.add_edge(child, parent, reads.clone());
            }
            return;
        }
        let Some(composition) = self.compositions_by_binding.get(binding) else {
            return;
        };
        for inner in crate::deps::get_composition_dependencies(composition) {
            let key: &'a str = if let Some((k, _)) =
                self.compositions_by_binding.get_key_value(inner.as_str())
            {
                k.as_str()
            } else if let Some((k, _)) = self.binding_to_idx.get_key_value(inner.as_str()) {
                k.as_str()
            } else if let Some((k, _)) = self.binding_to_final_idx.get_key_value(inner.as_str()) {
                k.as_str()
            } else {
                continue;
            };
            self.record_binding_edge_inner(key, ReadsSet::unknown(), analysis, child, visited);
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
pub fn build_effect_dependency_analysis(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    compositions: &[crate::resource::Composition],
    inputs: ScheduleInputs<'_>,
) -> DependencyAnalysis {
    let mut binding_to_idx: HashMap<String, usize> = HashMap::new();
    let mut binding_to_final_idx: HashMap<String, usize> = HashMap::new();
    let mut name_to_delete_idx: HashMap<String, usize> = HashMap::new();
    let aliases = match inputs {
        ScheduleInputs::Apply => &[][..],
        ScheduleInputs::Destroy { aliases } => aliases,
    };
    for (idx, effect) in effects.iter().enumerate() {
        if !(matches!(inputs, ScheduleInputs::Apply) && matches!(effect, Effect::Delete { .. }))
            && let Some(binding) = effect.binding_name()
        {
            binding_to_idx.entry(binding).or_insert(idx);
        }
        if matches!(inputs, ScheduleInputs::Apply)
            && let Effect::Update {
                from: crate::effect::UpdateBase::CreatedBy { binding, .. },
                ..
            } = effect
        {
            binding_to_final_idx.insert(binding.clone(), idx);
        }
        if let Effect::Delete { id, binding, .. } = effect {
            if let Some(binding) = binding {
                name_to_delete_idx.insert(binding.clone(), idx);
            } else {
                name_to_delete_idx.insert(id.name_str().to_string(), idx);
            }
        }
    }
    let alias_offset = effects.len();
    for (alias_idx, alias) in aliases.iter().enumerate() {
        binding_to_idx.insert(alias.binding.clone(), alias_offset + alias_idx);
    }

    let analyzer = DependencyAnalyzer::new(
        binding_to_idx,
        binding_to_final_idx,
        name_to_delete_idx,
        compositions,
    );
    let mut analysis = DependencyAnalysis::new(effects.len() + aliases.len());

    for (idx, effect) in effects.iter().enumerate() {
        match inputs {
            ScheduleInputs::Apply => {
                if effect.is_scheduler_meta() {
                    analyzer.collect_from_schedule_edges(effect.apply_edges(), &mut analysis, idx);
                    continue;
                }
                if effect.as_resource_ref().is_some() {
                    if let Some(unresolved) = unresolved_resources.get(effect.resource_id()) {
                        analyzer.collect_from_resource(
                            unresolved.as_resource(),
                            &mut analysis,
                            idx,
                        );
                    } else if let Some(resource) = effect.as_resource_ref() {
                        analyzer.collect_from_resource_ref(resource, &mut analysis, idx);
                    }
                }
                analyzer.collect_from_schedule_edges(effect.apply_edges(), &mut analysis, idx);
            }
            ScheduleInputs::Destroy { .. } => {
                analyzer.collect_from_schedule_edges(effect.destroy_edges(), &mut analysis, idx);
            }
        }
    }
    if let ScheduleInputs::Destroy { .. } = inputs {
        for (alias_idx, alias) in aliases.iter().enumerate() {
            analyzer.collect_from_schedule_edges(
                alias.destroy_edges(),
                &mut analysis,
                alias_offset + alias_idx,
            );
        }
    }
    if matches!(inputs, ScheduleInputs::Apply) {
        collect_cbd_create_delete_edges(effects, &mut analysis);
        collect_created_by_update_delete_edges(effects, &mut analysis);
    }

    analysis
}

fn collect_created_by_update_delete_edges(effects: &[Effect], analysis: &mut DependencyAnalysis) {
    let mut delete_by_id: HashMap<&ResourceId, usize> = HashMap::new();
    let mut created_by_update_by_id: HashMap<&ResourceId, usize> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        match effect {
            Effect::Update {
                id,
                from: crate::effect::UpdateBase::CreatedBy { .. },
                ..
            } => {
                created_by_update_by_id.insert(id, idx);
            }
            Effect::Delete { id, .. } => {
                delete_by_id.insert(id, idx);
            }
            _ => {}
        }
    }
    for (id, update_idx) in created_by_update_by_id {
        if let Some(delete_idx) = delete_by_id.get(id) {
            analysis.add_edge(update_idx, *delete_idx, ReadsSet::unknown());
        }
    }
}

fn collect_cbd_create_delete_edges(effects: &[Effect], analysis: &mut DependencyAnalysis) {
    let mut create_by_id: HashMap<&ResourceId, usize> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        match effect {
            Effect::Create(resource) => {
                create_by_id.entry(&resource.id).or_insert(idx);
            }
            Effect::Delete { id, .. } => {
                if let Some(create_idx) = create_by_id.get(id) {
                    analysis.add_edge(idx, *create_idx, ReadsSet::unknown());
                }
            }
            _ => {}
        }
    }
}

pub fn relax_update_update_edges(effects: &[Effect], analysis: &mut DependencyAnalysis) {
    for child in 0..effects.len() {
        if !matches!(&effects[child], Effect::Update { .. }) {
            continue;
        }
        let Some(parents) = analysis.deps_of(child).cloned() else {
            continue;
        };
        for parent in parents {
            let Some(writes) = WritesSet::from_update(&effects[parent]) else {
                continue;
            };
            let Some(reads) = analysis.reads_for_edge(child, parent) else {
                continue;
            };
            if let Effect::Update {
                id,
                from: crate::effect::UpdateBase::CreatedBy { .. },
                ..
            } = &effects[parent]
            {
                if !reads.disjoint(&writes) {
                    remove_cbd_delete_wait_for_final_state_consumer(effects, analysis, id, child);
                } else {
                    analysis.remove_edge(child, parent);
                }
                continue;
            }
            if reads.disjoint(&writes) {
                analysis.remove_edge(child, parent);
            }
        }
    }
}

/// Remove the CBD Delete -> consumer edge only when that edge came from the
/// Delete's captured `blocked_by_updates` set. Other future Delete -> consumer
/// edges must not be silently removed by the rename cycle breaker.
fn remove_cbd_delete_wait_for_final_state_consumer(
    effects: &[Effect],
    analysis: &mut DependencyAnalysis,
    replacement_id: &ResourceId,
    consumer_idx: usize,
) {
    let Some(consumer_binding) = effects[consumer_idx].binding_name() else {
        return;
    };
    for (delete_idx, effect) in effects.iter().enumerate() {
        if matches!(
            effect,
            Effect::Delete {
                id,
                blocked_by_updates,
                ..
            } if id == replacement_id && blocked_by_updates.contains(&consumer_binding)
        ) {
            analysis.remove_edge(delete_idx, consumer_idx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{State, Value};

    fn state_for(id: &ResourceId) -> State {
        State::not_found(id.clone())
    }

    fn update_effect(binding: &str, refs: &[(&str, &str)], changed: &[&str]) -> Effect {
        let mut resource = Resource::new("test", binding);
        resource.binding = Some(binding.to_string());
        for (dep, attr) in refs {
            resource.set_attr(
                format!("{}_{}", dep, attr),
                Value::resource_ref((*dep).to_string(), (*attr).to_string(), vec![]),
            );
        }
        Effect::Update {
            id: resource.id.clone(),
            from: crate::effect::UpdateBase::Existing(Box::new(state_for(&resource.id))),
            to: resource,
            changed_attributes: changed.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn delete_effect(
        binding: &str,
        blocked_by_updates: HashSet<String>,
        dependencies: HashSet<String>,
    ) -> Effect {
        Effect::Delete {
            id: ResourceId::new("test", binding),
            identifier: format!("{binding}-old-id"),
            directives: Default::default(),
            binding: Some(binding.to_string()),
            dependencies,
            explicit_dependencies: HashSet::new(),
            blocked_by_updates,
        }
    }

    fn cbd_rename_update(binding: &str, changed_attr: &str) -> Effect {
        let id = ResourceId::new("test", binding);
        let mut renamed = Resource::new("test", binding);
        renamed.binding = Some(binding.to_string());
        renamed.set_attr(
            changed_attr,
            Value::Concrete(crate::resource::ConcreteValue::String("final".to_string())),
        );
        Effect::Update {
            id: id.clone(),
            from: crate::effect::UpdateBase::CreatedBy {
                binding: binding.to_string(),
                id,
            },
            to: renamed,
            changed_attributes: vec![changed_attr.to_string()],
        }
    }

    fn create_effect(binding: &str) -> Effect {
        let mut created = Resource::new("test", binding);
        created.binding = Some(binding.to_string());
        Effect::Create(created)
    }

    fn deps_after_relax(
        effects: &[Effect],
        unresolved: &HashMap<ResourceId, UnresolvedResource>,
    ) -> HashMap<usize, HashSet<usize>> {
        let mut analysis =
            build_effect_dependency_analysis(effects, unresolved, &[], ScheduleInputs::Apply);
        relax_update_update_edges(effects, &mut analysis);
        analysis.into_deps_of()
    }

    fn assert_cycle_free(deps: &HashMap<usize, HashSet<usize>>, effect_count: usize) {
        let mut remaining: HashMap<usize, HashSet<usize>> = (0..effect_count)
            .map(|idx| (idx, deps.get(&idx).cloned().unwrap_or_default()))
            .collect();
        let mut ready: Vec<usize> = remaining
            .iter()
            .filter_map(|(idx, deps)| deps.is_empty().then_some(*idx))
            .collect();
        let mut visited = 0;

        while let Some(idx) = ready.pop() {
            let Some(_) = remaining.remove(&idx) else {
                continue;
            };
            visited += 1;
            let newly_ready: Vec<usize> = remaining
                .iter_mut()
                .filter_map(|(candidate, deps)| {
                    deps.remove(&idx);
                    deps.is_empty().then_some(*candidate)
                })
                .collect();
            ready.extend(newly_ready);
        }

        assert_eq!(
            visited, effect_count,
            "dependency graph must be cycle-free, remaining={remaining:?}, deps={deps:?}"
        );
    }

    struct CapturingSubscriber {
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for CapturingSubscriber {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::WARN
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut visitor = WarningVisitor::default();
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.rendered);
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    #[derive(Default)]
    struct WarningVisitor {
        rendered: String,
    }

    impl tracing::field::Visit for WarningVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;

            let _ = write!(self.rendered, "{}={value:?};", field.name());
        }
    }

    #[test]
    fn created_by_update_depends_on_create_with_same_binding() {
        let id = ResourceId::new("test", "renamed");
        let mut created = Resource::new("test", "renamed");
        created.binding = Some("renamed".to_string());
        let mut renamed = created.clone();
        renamed.set_attr(
            "name",
            Value::Concrete(crate::resource::ConcreteValue::String("final".to_string())),
        );
        let effects = vec![
            Effect::Create(created),
            Effect::Update {
                id: id.clone(),
                from: crate::effect::UpdateBase::CreatedBy {
                    binding: "renamed".to_string(),
                    id,
                },
                to: renamed,
                changed_attributes: vec!["name".to_string()],
            },
        ];

        let deps =
            build_effect_dependency_analysis(&effects, &HashMap::new(), &[], ScheduleInputs::Apply)
                .into_deps_of();

        assert!(
            deps[&1].contains(&0),
            "rename update must wait for the replacement create"
        );
    }

    #[test]
    fn created_by_rename_update_waits_for_delete_with_same_id() {
        let id = ResourceId::new("test", "renamed");
        let mut created = Resource::new("test", "renamed");
        created.binding = Some("renamed".to_string());
        let mut renamed = created.clone();
        renamed.set_attr(
            "name",
            Value::Concrete(crate::resource::ConcreteValue::String("final".to_string())),
        );
        let effects = vec![
            Effect::Create(created),
            Effect::Delete {
                id: id.clone(),
                identifier: "old-id".to_string(),
                directives: Default::default(),
                binding: Some("renamed".to_string()),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            },
            Effect::Update {
                id: id.clone(),
                from: crate::effect::UpdateBase::CreatedBy {
                    binding: "renamed".to_string(),
                    id: id.clone(),
                },
                to: renamed,
                changed_attributes: vec!["name".to_string()],
            },
        ];

        let deps =
            build_effect_dependency_analysis(&effects, &HashMap::new(), &[], ScheduleInputs::Apply)
                .into_deps_of();

        assert!(
            deps[&2].contains(&1),
            "post-create rename update must wait for the old resource delete"
        );
    }

    #[test]
    fn consumer_update_reading_renamed_attribute_does_not_cycle() {
        let consumer = match update_effect("consumer", &[("renamed", "name")], &["target_name"]) {
            Effect::Update { to, .. } => to,
            _ => unreachable!(),
        };
        let effects = vec![
            create_effect("renamed"),
            Effect::Update {
                id: consumer.id.clone(),
                from: crate::effect::UpdateBase::Existing(Box::new(state_for(&consumer.id))),
                to: consumer.clone(),
                changed_attributes: vec!["target_name".to_string()],
            },
            delete_effect(
                "renamed",
                HashSet::from(["consumer".to_string()]),
                HashSet::new(),
            ),
            cbd_rename_update("renamed", "name"),
        ];
        let unresolved = HashMap::from([(
            consumer.id.clone(),
            UnresolvedResource::from_pre_resolve(consumer),
        )]);

        let deps = deps_after_relax(&effects, &unresolved);

        assert!(
            deps[&1].contains(&3),
            "consumer reading the renamed attribute must use the rename final state"
        );
        assert!(
            !deps[&2].contains(&1),
            "delete cannot also wait on that final-state consumer without forming a CBD cycle"
        );
        assert_cycle_free(&deps, effects.len());
    }

    #[test]
    fn consumer_with_depends_on_directive_waits_for_rename() {
        let mut consumer = Resource::new("test", "consumer");
        consumer.binding = Some("consumer".to_string());
        consumer.directives.depends_on.push("renamed".to_string());
        let effects = vec![
            create_effect("renamed"),
            Effect::Update {
                id: consumer.id.clone(),
                from: crate::effect::UpdateBase::Existing(Box::new(state_for(&consumer.id))),
                to: consumer.clone(),
                changed_attributes: vec!["target".to_string()],
            },
            delete_effect(
                "renamed",
                HashSet::from(["consumer".to_string()]),
                HashSet::new(),
            ),
            cbd_rename_update("renamed", "name"),
        ];
        let unresolved = HashMap::from([(
            consumer.id.clone(),
            UnresolvedResource::from_pre_resolve(consumer),
        )]);

        let deps = deps_after_relax(&effects, &unresolved);

        assert!(
            deps[&1].contains(&3),
            "Unknown reads from depends_on must conservatively wait for the rename final state"
        );
        assert!(
            !deps[&2].contains(&1),
            "Delete must not also wait on the final-state consumer in the CBD cycle"
        );
        assert_cycle_free(&deps, effects.len());
    }

    #[test]
    fn consumer_update_reading_renamed_identifier_uses_final_state() {
        let consumer = match update_effect("consumer", &[("renamed", "name")], &["target_name"]) {
            Effect::Update { to, .. } => to,
            _ => unreachable!(),
        };
        let effects = vec![
            create_effect("renamed"),
            Effect::Update {
                id: consumer.id.clone(),
                from: crate::effect::UpdateBase::Existing(Box::new(state_for(&consumer.id))),
                to: consumer.clone(),
                changed_attributes: vec!["target_name".to_string()],
            },
            delete_effect(
                "renamed",
                HashSet::from(["consumer".to_string()]),
                HashSet::new(),
            ),
            cbd_rename_update("renamed", "name"),
        ];
        let unresolved = HashMap::from([(
            consumer.id.clone(),
            UnresolvedResource::from_pre_resolve(consumer),
        )]);

        let deps = deps_after_relax(&effects, &unresolved);

        assert!(deps[&1].contains(&3));
        assert!(deps[&3].contains(&2));
        assert_cycle_free(&deps, effects.len());
    }

    #[test]
    fn consumer_update_reading_non_renamed_attribute_uses_created_state() {
        let consumer = match update_effect("consumer", &[("renamed", "id")], &["target_id"]) {
            Effect::Update { to, .. } => to,
            _ => unreachable!(),
        };
        let effects = vec![
            create_effect("renamed"),
            Effect::Update {
                id: consumer.id.clone(),
                from: crate::effect::UpdateBase::Existing(Box::new(state_for(&consumer.id))),
                to: consumer.clone(),
                changed_attributes: vec!["target_id".to_string()],
            },
            delete_effect(
                "renamed",
                HashSet::from(["consumer".to_string()]),
                HashSet::new(),
            ),
            cbd_rename_update("renamed", "name"),
        ];
        let unresolved = HashMap::from([(
            consumer.id.clone(),
            UnresolvedResource::from_pre_resolve(consumer),
        )]);

        let deps = deps_after_relax(&effects, &unresolved);

        assert!(deps[&1].contains(&0));
        assert!(
            !deps[&1].contains(&3),
            "consumer reading a non-renamed attribute should not wait for rename final state"
        );
        assert!(
            deps[&2].contains(&1),
            "delete still waits for consumers that can update from created state"
        );
        assert_cycle_free(&deps, effects.len());
    }

    #[test]
    fn cbd_ordering_patterns_are_cycle_free() {
        let no_consumers = vec![
            create_effect("renamed"),
            delete_effect("renamed", HashSet::new(), HashSet::new()),
            cbd_rename_update("renamed", "name"),
        ];
        let deps = deps_after_relax(&no_consumers, &HashMap::new());
        assert_cycle_free(&deps, no_consumers.len());

        let consumer = match update_effect("consumer", &[("renamed", "id")], &["target_id"]) {
            Effect::Update { to, .. } => to,
            _ => unreachable!(),
        };
        let one_consumer = vec![
            create_effect("renamed"),
            Effect::Update {
                id: consumer.id.clone(),
                from: crate::effect::UpdateBase::Existing(Box::new(state_for(&consumer.id))),
                to: consumer.clone(),
                changed_attributes: vec!["target_id".to_string()],
            },
            delete_effect(
                "renamed",
                HashSet::from(["consumer".to_string()]),
                HashSet::new(),
            ),
            cbd_rename_update("renamed", "name"),
        ];
        let unresolved = HashMap::from([(
            consumer.id.clone(),
            UnresolvedResource::from_pre_resolve(consumer),
        )]);
        let deps = deps_after_relax(&one_consumer, &unresolved);
        assert_cycle_free(&deps, one_consumer.len());

        let promoted_consumer = Resource::new("test", "promoted").with_binding("promoted");
        let promoted = vec![
            create_effect("renamed"),
            delete_effect("renamed", HashSet::new(), HashSet::new()),
            cbd_rename_update("renamed", "name"),
            Effect::Create(promoted_consumer.clone()),
            delete_effect("promoted", HashSet::new(), HashSet::new()),
        ];
        let deps = deps_after_relax(&promoted, &HashMap::new());
        assert_cycle_free(&deps, promoted.len());
    }

    #[test]
    fn cbd_delete_waits_for_create_with_same_id() {
        let id = ResourceId::new("test", "renamed");
        let mut created = Resource::new("test", "renamed");
        created.binding = Some("renamed".to_string());
        let effects = vec![
            Effect::Create(created),
            Effect::Delete {
                id,
                identifier: "old-id".to_string(),
                directives: Default::default(),
                binding: Some("renamed".to_string()),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            },
        ];

        let deps =
            build_effect_dependency_analysis(&effects, &HashMap::new(), &[], ScheduleInputs::Apply)
                .into_deps_of();

        assert!(
            deps[&1].contains(&0),
            "CBD old delete must wait for the replacement create"
        );
    }

    #[test]
    fn consumer_ref_waits_for_create_and_rename_final_state() {
        let id = ResourceId::new("test", "renamed");
        let mut created = Resource::new("test", "renamed");
        created.binding = Some("renamed".to_string());
        let mut renamed = created.clone();
        renamed.set_attr(
            "name",
            Value::Concrete(crate::resource::ConcreteValue::String("final".to_string())),
        );
        let mut consumer = Resource::new("test", "consumer");
        consumer.binding = Some("consumer".to_string());
        consumer.set_attr(
            "target_name",
            Value::resource_ref("renamed".to_string(), "name".to_string(), vec![]),
        );
        let effects = vec![
            Effect::Create(created),
            Effect::Update {
                id: id.clone(),
                from: crate::effect::UpdateBase::CreatedBy {
                    binding: "renamed".to_string(),
                    id,
                },
                to: renamed,
                changed_attributes: vec!["name".to_string()],
            },
            Effect::Update {
                id: consumer.id.clone(),
                from: crate::effect::UpdateBase::Existing(Box::new(state_for(&consumer.id))),
                to: consumer.clone(),
                changed_attributes: vec!["target_name".to_string()],
            },
        ];
        let unresolved = HashMap::from([(
            consumer.id.clone(),
            UnresolvedResource::from_pre_resolve(consumer),
        )]);

        let deps =
            build_effect_dependency_analysis(&effects, &unresolved, &[], ScheduleInputs::Apply)
                .into_deps_of();

        assert!(deps[&2].contains(&0), "consumer must wait for create");
        assert!(
            deps[&2].contains(&1),
            "consumer must also wait for the rename final state when it reads the renamed attribute"
        );
    }

    #[test]
    fn delete_dependencies_only_block_delete_targets_during_apply() {
        let mut created = Resource::new("test", "a");
        created.binding = Some("a".to_string());
        let mut consumer = Resource::new("test", "b");
        consumer.binding = Some("b".to_string());
        consumer.set_attr(
            "a_id",
            Value::resource_ref("a".to_string(), "id".to_string(), vec![]),
        );
        let effects = vec![
            Effect::Create(created),
            Effect::Update {
                id: consumer.id.clone(),
                from: crate::effect::UpdateBase::Existing(Box::new(state_for(&consumer.id))),
                to: consumer.clone(),
                changed_attributes: vec!["a_id".to_string()],
            },
            Effect::Delete {
                id: ResourceId::new("test", "a"),
                identifier: "old-a".to_string(),
                directives: Default::default(),
                binding: Some("a".to_string()),
                dependencies: HashSet::from(["b".to_string()]),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::from(["b".to_string()]),
            },
        ];
        let unresolved = HashMap::from([(
            consumer.id.clone(),
            UnresolvedResource::from_pre_resolve(consumer),
        )]);

        let deps =
            build_effect_dependency_analysis(&effects, &unresolved, &[], ScheduleInputs::Apply)
                .into_deps_of();

        assert!(
            deps[&2].contains(&1),
            "CBD delete still waits for explicit blocked_by_updates"
        );
        assert!(
            !deps[&1].contains(&2),
            "old dependency bindings must not make an Update wait for the Delete"
        );
    }

    #[test]
    fn blocked_by_if_delete_warns_when_target_not_a_delete() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = CapturingSubscriber {
            events: std::sync::Arc::clone(&events),
        };
        let dispatch = tracing::Dispatch::new(subscriber);
        let mut created = Resource::new("test", "target");
        created.binding = Some("target".to_string());
        let effects = vec![
            Effect::Create(created),
            Effect::Delete {
                id: ResourceId::new("test", "dependent"),
                identifier: "dependent-old-id".to_string(),
                directives: Default::default(),
                binding: Some("dependent".to_string()),
                dependencies: HashSet::from(["target".to_string()]),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            },
        ];

        tracing::dispatcher::with_default(&dispatch, || {
            let _ = build_effect_dependency_analysis(
                &effects,
                &HashMap::new(),
                &[],
                ScheduleInputs::Apply,
            );
        });

        let events = events.lock().unwrap();
        assert!(
            events.iter().any(|event| event
                .contains("BlockedByIfDelete target did not resolve to a Delete effect")
                && event.contains("target")),
            "expected BlockedByIfDelete warning, got {events:?}"
        );
    }

    #[test]
    fn destroy_delete_edges_block_dependencies_by_consumers() {
        let parent = Effect::Delete {
            id: ResourceId::new("test", "parent"),
            identifier: "parent-id".to_string(),
            directives: Default::default(),
            binding: Some("parent".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: std::collections::HashSet::new(),
        };
        let child = Effect::Delete {
            id: ResourceId::new("test", "child"),
            identifier: "child-id".to_string(),
            directives: Default::default(),
            binding: Some("child".to_string()),
            dependencies: HashSet::from(["parent".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: std::collections::HashSet::new(),
        };
        let effects = vec![parent, child];

        let deps = build_effect_dependency_analysis(
            &effects,
            &HashMap::new(),
            &[],
            ScheduleInputs::Destroy { aliases: &[] },
        )
        .into_deps_of();

        assert!(deps[&0].contains(&1), "parent delete must wait for child");
    }

    #[test]
    fn destroy_wait_alias_bridges_target_to_consumers() {
        let cert = Effect::Delete {
            id: ResourceId::new("test", "cert"),
            identifier: "cert-id".to_string(),
            directives: Default::default(),
            binding: Some("cert".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: std::collections::HashSet::new(),
        };
        let listener = Effect::Delete {
            id: ResourceId::new("test", "listener"),
            identifier: "listener-id".to_string(),
            directives: Default::default(),
            binding: Some("listener".to_string()),
            dependencies: HashSet::from(["cert_issued".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: std::collections::HashSet::new(),
        };
        let wait = DestroyWaitAlias::new(
            "cert_issued".to_string(),
            "cert".to_string(),
            HashSet::new(),
            vec!["listener".to_string()],
        )
        .expect("test alias has a consumer");
        let effects = vec![cert, listener];

        let deps = build_effect_dependency_analysis(
            &effects,
            &HashMap::new(),
            &[],
            ScheduleInputs::Destroy { aliases: &[wait] },
        )
        .into_deps_of();

        assert!(deps[&2].contains(&1), "wait must wait for listener");
        assert!(deps[&0].contains(&2), "cert must wait for wait alias");
    }

    #[test]
    fn destroy_wait_alias_rejects_empty_consumers() {
        assert!(DestroyWaitAlias::new("w".into(), "t".into(), HashSet::new(), vec![]).is_none());
    }

    #[test]
    fn unknown_destroy_dependency_names_are_dropped() {
        let orphan = Effect::Delete {
            id: ResourceId::new("test", "listener"),
            identifier: "listener-id".to_string(),
            directives: Default::default(),
            binding: Some("listener".to_string()),
            dependencies: HashSet::from(["missing_wait".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: std::collections::HashSet::new(),
        };
        let deps = build_effect_dependency_analysis(
            &[orphan],
            &HashMap::new(),
            &[],
            ScheduleInputs::Destroy { aliases: &[] },
        )
        .into_deps_of();

        assert!(deps[&0].is_empty());
    }

    #[test]
    fn relax_update_update_edge_when_child_reads_disjoint_attribute() {
        let effects = vec![
            update_effect("parent", &[], &["tags"]),
            update_effect("child", &[("parent", "id")], &["tags"]),
        ];
        let unresolved: HashMap<ResourceId, UnresolvedResource> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Update { to, .. } => Some((
                    effect.resource_id().clone(),
                    UnresolvedResource::from_pre_resolve(to.clone()),
                )),
                _ => None,
            })
            .collect();
        let mut analysis =
            build_effect_dependency_analysis(&effects, &unresolved, &[], ScheduleInputs::Apply);
        relax_update_update_edges(&effects, &mut analysis);

        assert!(!analysis.into_deps_of()[&1].contains(&0));
    }
}
