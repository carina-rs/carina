use std::collections::{BTreeSet, HashMap, HashSet};

use crate::effect::{BindingKey, Effect, ScheduleEdge};
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
        let mut edges = vec![ScheduleEdge::BlockedBy(BindingKey::Binding(
            self.target_binding.clone(),
        ))];
        edges.extend(
            self.explicit_dependencies
                .iter()
                .filter(|dependency| dependency.as_str() != self.target_binding)
                .cloned()
                .map(|binding| ScheduleEdge::BlockedBy(BindingKey::Binding(binding))),
        );
        edges.extend(
            self.consumers
                .iter()
                .cloned()
                .map(|binding| ScheduleEdge::DependsOn(BindingKey::Binding(binding))),
        );
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
    binding_to_idx: HashMap<BindingKey, usize>,
    compositions_by_binding: HashMap<String, crate::resource::Composition>,
}

impl DependencyAnalyzer {
    fn new(
        binding_to_idx: HashMap<BindingKey, usize>,
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
            compositions_by_binding,
        }
    }

    fn lookup_by_key(&self, key: &BindingKey) -> Option<usize> {
        self.binding_to_idx.get(key).copied()
    }

    fn lookup_by_string_ref(&self, name: &str) -> Option<usize> {
        self.lookup_by_key(&BindingKey::Binding(name.to_string()))
            .or_else(|| {
                self.binding_to_idx.iter().find_map(|(key, idx)| match key {
                    BindingKey::Anonymous { name: n, .. } if n == name => Some(*idx),
                    _ => None,
                })
            })
    }

    fn collect_from_schedule_edges(
        &self,
        effects: &[Effect],
        edges: Vec<ScheduleEdge>,
        analysis: &mut DependencyAnalysis,
        self_idx: usize,
    ) {
        for edge in edges {
            match edge {
                ScheduleEdge::DependsOn(key) => {
                    if let Some(parent) = self.lookup_by_key(&key) {
                        analysis.add_edge(self_idx, parent, ReadsSet::unknown());
                    }
                }
                ScheduleEdge::BlockedBy(key) => {
                    if let Some(blocked_idx) = self.lookup_by_key(&key) {
                        analysis.add_edge(blocked_idx, self_idx, ReadsSet::unknown());
                    }
                }
                ScheduleEdge::BlockedByIfDelete(key) => {
                    if let Some(blocked_idx) = self.lookup_by_key(&key)
                        && blocked_idx < effects.len()
                        && matches!(effects[blocked_idx], Effect::Delete { .. })
                    {
                        analysis.add_edge(blocked_idx, self_idx, ReadsSet::unknown());
                    }
                }
            }
        }
    }

    fn collect_blocked_by_if_delete_string_ref(
        &self,
        effects: &[Effect],
        binding: &str,
        analysis: &mut DependencyAnalysis,
        self_idx: usize,
    ) {
        if let Some(blocked_idx) = self.lookup_by_string_ref(binding)
            && blocked_idx < effects.len()
            && matches!(effects[blocked_idx], Effect::Delete { .. })
        {
            analysis.add_edge(blocked_idx, self_idx, ReadsSet::unknown());
        }
    }

    fn collect_blocked_by_string_ref(
        &self,
        binding: &str,
        analysis: &mut DependencyAnalysis,
        self_idx: usize,
    ) {
        if let Some(blocked_idx) = self.lookup_by_string_ref(binding) {
            analysis.add_edge(blocked_idx, self_idx, ReadsSet::unknown());
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

    fn record_binding_edge_inner(
        &self,
        binding: &str,
        reads: ReadsSet,
        analysis: &mut DependencyAnalysis,
        child: usize,
        visited: &mut HashSet<String>,
    ) {
        if !visited.insert(binding.to_string()) {
            return;
        }
        if let Some(parent) = self.lookup_by_string_ref(binding) {
            analysis.add_edge(child, parent, reads);
            return;
        }
        let Some(composition) = self.compositions_by_binding.get(binding) else {
            return;
        };
        for inner in crate::deps::get_composition_dependencies(composition) {
            if self.compositions_by_binding.contains_key(inner.as_str())
                || self.lookup_by_string_ref(&inner).is_some()
            {
                self.record_binding_edge_inner(
                    &inner,
                    ReadsSet::unknown(),
                    analysis,
                    child,
                    visited,
                );
            }
        }
    }
}

fn build_binding_index(
    effects: &[Effect],
    aliases: &[DestroyWaitAlias],
) -> HashMap<BindingKey, usize> {
    let mut binding_to_idx: HashMap<BindingKey, usize> = HashMap::new();
    for (idx, effect) in effects.iter().enumerate() {
        binding_to_idx.insert(effect.binding_key(), idx);
    }
    let alias_offset = effects.len();
    for (alias_idx, alias) in aliases.iter().enumerate() {
        binding_to_idx.insert(
            BindingKey::Binding(alias.binding.clone()),
            alias_offset + alias_idx,
        );
    }
    binding_to_idx
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
    let aliases = match inputs {
        ScheduleInputs::Apply => &[][..],
        ScheduleInputs::Destroy { aliases } => aliases,
    };
    let binding_to_idx = build_binding_index(effects, aliases);
    let alias_offset = effects.len();

    let analyzer = DependencyAnalyzer::new(binding_to_idx, compositions);
    let mut analysis = DependencyAnalysis::new(effects.len() + aliases.len());

    for (idx, effect) in effects.iter().enumerate() {
        match inputs {
            ScheduleInputs::Apply => {
                if effect.is_scheduler_meta() {
                    analyzer.collect_from_schedule_edges(
                        effects,
                        effect.apply_edges(),
                        &mut analysis,
                        idx,
                    );
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
                analyzer.collect_from_schedule_edges(
                    effects,
                    effect.apply_edges(),
                    &mut analysis,
                    idx,
                );
                if let Effect::Replace { from, .. } = effect {
                    for binding in &from.dependency_bindings {
                        analyzer.collect_blocked_by_if_delete_string_ref(
                            effects,
                            binding,
                            &mut analysis,
                            idx,
                        );
                    }
                }
            }
            ScheduleInputs::Destroy { .. } => {
                analyzer.collect_from_schedule_edges(
                    effects,
                    effect.destroy_edges(),
                    &mut analysis,
                    idx,
                );
                if let Effect::Replace { from, .. } = effect {
                    for binding in &from.dependency_bindings {
                        analyzer.collect_blocked_by_string_ref(binding, &mut analysis, idx);
                    }
                }
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

    analysis
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
            if reads.disjoint(&writes) {
                analysis.remove_edge(child, parent);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::{BindingKey, ChangedCreateOnly};
    use crate::resource::{State, Value};

    fn state_for(id: &ResourceId) -> State {
        State::not_found(id.clone())
    }

    fn analyzer_for_effects(effects: &[Effect]) -> DependencyAnalyzer {
        DependencyAnalyzer::new(build_binding_index(effects, &[]), &[])
    }

    fn delete_effect(resource_type: &str, name: &str, binding: Option<&str>) -> Effect {
        Effect::Delete {
            id: ResourceId::new(resource_type, name),
            identifier: format!("{name}-id"),
            directives: Default::default(),
            binding: binding.map(str::to_string),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
        }
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
            from: Box::new(state_for(&resource.id)),
            to: resource,
            changed_attributes: changed.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn blocked_by_if_delete_resolves_anonymous_delete_through_binding_key_index() {
        let delete = delete_effect("test.Resource", "anonymous_old", None);
        let update = update_effect("consumer", &[], &["ref"]);
        let effects = vec![delete, update];
        let mut analysis = DependencyAnalysis::new(effects.len());
        let analyzer = analyzer_for_effects(&effects);

        analyzer.collect_from_schedule_edges(
            &effects,
            vec![ScheduleEdge::BlockedByIfDelete(BindingKey::Anonymous {
                resource_type: "test.Resource".to_string(),
                name: "anonymous_old".to_string(),
            })],
            &mut analysis,
            1,
        );

        let deps = analysis.into_deps_of();
        assert!(
            deps[&0].contains(&1),
            "anonymous delete must wait for the consumer update"
        );
    }

    #[test]
    fn blocked_by_if_delete_resolves_let_bound_delete_through_binding_key_index() {
        let delete = delete_effect("test.Resource", "old", Some("old_binding"));
        let update = update_effect("consumer", &[], &["ref"]);
        let effects = vec![delete, update];
        let mut analysis = DependencyAnalysis::new(effects.len());
        let analyzer = analyzer_for_effects(&effects);

        analyzer.collect_from_schedule_edges(
            &effects,
            vec![ScheduleEdge::BlockedByIfDelete(BindingKey::Binding(
                "old_binding".to_string(),
            ))],
            &mut analysis,
            1,
        );

        let deps = analysis.into_deps_of();
        assert!(
            deps[&0].contains(&1),
            "let-bound delete must still wait for the consumer update"
        );
    }

    #[test]
    fn effect_binding_key_distinguishes_named_and_anonymous_resources() {
        let named = Effect::Create(Resource::new("test.Resource", "named").with_binding("r"));
        assert_eq!(named.binding_key(), BindingKey::Binding("r".to_string()));

        let anonymous = delete_effect("test.Resource", "anonymous", None);
        assert_eq!(
            anonymous.binding_key(),
            BindingKey::Anonymous {
                resource_type: "test.Resource".to_string(),
                name: "anonymous".to_string(),
            }
        );
    }

    #[test]
    fn string_ref_resolves_to_anonymous_delete_when_no_binding_match() {
        let create = Effect::Create(Resource::new("test.Resource", "foo").with_binding("foo"));
        let delete = delete_effect("other.Resource", "foo", None);
        let mut consumer = Resource::new("test.Resource", "consumer").with_binding("consumer");
        consumer.dependency_bindings = std::collections::BTreeSet::from(["foo".to_string()]);
        let consumer = Effect::Create(consumer);
        let effects = vec![create, delete.clone(), consumer.clone()];
        let analyzer = analyzer_for_effects(&effects);

        assert_eq!(
            analyzer.lookup_by_key(&BindingKey::Binding("foo".to_string())),
            Some(0),
            "strict BindingKey lookup must resolve the let-bound create"
        );
        assert_eq!(
            analyzer.lookup_by_string_ref("foo"),
            Some(0),
            "string refs must prefer an exact binding match over an anonymous resource with the same name"
        );

        let mut schedule_analysis = DependencyAnalysis::new(effects.len());
        analyzer.collect_from_schedule_edges(
            &effects,
            vec![ScheduleEdge::DependsOn(BindingKey::Binding(
                "foo".to_string(),
            ))],
            &mut schedule_analysis,
            2,
        );
        assert!(
            schedule_analysis.into_deps_of()[&2].contains(&0),
            "strict schedule edge should use the let-bound create"
        );

        let mut string_ref_analysis = DependencyAnalysis::new(effects.len());
        analyzer.collect_from_resource_ref(
            consumer.as_resource_ref().unwrap(),
            &mut string_ref_analysis,
            2,
        );
        assert!(
            string_ref_analysis.into_deps_of()[&2].contains(&0),
            "dependency_bindings string ref should use the let-bound create while it exists"
        );

        let effects = vec![delete, consumer];
        let analyzer = analyzer_for_effects(&effects);
        assert_eq!(
            analyzer.lookup_by_key(&BindingKey::Binding("foo".to_string())),
            None,
            "strict BindingKey lookup must not resolve anonymous resources through Binding"
        );
        assert_eq!(
            analyzer.lookup_by_string_ref("foo"),
            Some(0),
            "string refs should fall back to the anonymous resource name when no binding matches"
        );

        let mut string_ref_analysis = DependencyAnalysis::new(effects.len());
        analyzer.collect_from_resource_ref(
            effects[1].as_resource_ref().unwrap(),
            &mut string_ref_analysis,
            1,
        );
        assert!(
            string_ref_analysis.into_deps_of()[&1].contains(&0),
            "dependency_bindings string ref should fall back to the anonymous delete"
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
        };
        let child = Effect::Delete {
            id: ResourceId::new("test", "child"),
            identifier: "child-id".to_string(),
            directives: Default::default(),
            binding: Some("child".to_string()),
            dependencies: HashSet::from(["parent".to_string()]),
            explicit_dependencies: HashSet::new(),
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
        };
        let listener = Effect::Delete {
            id: ResourceId::new("test", "listener"),
            identifier: "listener-id".to_string(),
            directives: Default::default(),
            binding: Some("listener".to_string()),
            dependencies: HashSet::from(["cert_issued".to_string()]),
            explicit_dependencies: HashSet::new(),
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
    fn replace_from_dependency_only_blocks_delete_targets() {
        let mut x = Resource::new("test", "x");
        x.binding = Some("x".to_string());

        let from = State::existing(ResourceId::new("test", "replace_me"), HashMap::new())
            .with_dependency_bindings(std::collections::BTreeSet::from(["x".to_string()]));
        let mut to = Resource::new("test", "replace_me");
        to.binding = Some("replace_me".to_string());

        let effects = vec![
            Effect::Create(x),
            Effect::Replace {
                id: ResourceId::new("test", "replace_me"),
                from: Box::new(from),
                to,
                directives: Default::default(),
                changed_create_only: ChangedCreateOnly::new(vec!["name".to_string()]).unwrap(),
                cascading_updates: Vec::new(),
                temporary_name: None,
                cascade_ref_hints: Vec::new(),
            },
        ];

        let deps =
            build_effect_dependency_analysis(&effects, &HashMap::new(), &[], ScheduleInputs::Apply)
                .into_deps_of();

        assert!(
            deps[&0].is_empty(),
            "create target must not be blocked by replace"
        );
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
