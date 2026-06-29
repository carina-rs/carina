use std::collections::{BTreeSet, HashMap, HashSet};

use crate::effect::{Effect, ScheduleEdge};
use crate::non_empty::NonEmptyVec;
use crate::parser::ResourceRef;
use crate::resource::{Resource, ResourceId, ResourceIdentity};

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
        let mut edges = vec![ScheduleEdge::BlockedBy(ResourceIdentity::new(
            self.target_binding.clone(),
        ))];
        edges.extend(
            self.explicit_dependencies
                .iter()
                .filter(|dependency| dependency.as_str() != self.target_binding)
                .cloned()
                .map(ResourceIdentity::new)
                .map(ScheduleEdge::BlockedBy),
        );
        edges.extend(
            self.consumers
                .iter()
                .cloned()
                .map(ResourceIdentity::new)
                .map(ScheduleEdge::DependsOn),
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
    identity_to_idx: HashMap<ResourceIdentity, usize>,
    compositions_by_binding: HashMap<String, crate::resource::Composition>,
}

impl DependencyAnalyzer {
    fn new(
        identity_to_idx: HashMap<ResourceIdentity, usize>,
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
            identity_to_idx,
            compositions_by_binding,
        }
    }

    fn lookup_by_identity(&self, identity: &ResourceIdentity) -> Option<usize> {
        self.identity_to_idx.get(identity).copied()
    }

    fn lookup_by_string_ref(&self, binding: &str) -> Option<usize> {
        self.lookup_by_identity(&ResourceIdentity::new(binding))
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
                ScheduleEdge::DependsOn(identity) => {
                    if let Some(parent) = self.lookup_by_identity(&identity) {
                        analysis.add_edge(self_idx, parent, ReadsSet::unknown());
                    }
                }
                ScheduleEdge::BlockedBy(identity) => {
                    if let Some(blocked_idx) = self.lookup_by_identity(&identity) {
                        analysis.add_edge(blocked_idx, self_idx, ReadsSet::unknown());
                    }
                }
                ScheduleEdge::BlockedByIfDelete(identity) => {
                    if let Some(blocked_idx) = self.lookup_by_identity(&identity)
                        && blocked_idx < effects.len()
                        && matches!(effects[blocked_idx], Effect::Delete { .. })
                    {
                        analysis.add_edge(blocked_idx, self_idx, ReadsSet::unknown());
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
                || self.lookup_by_string_ref(inner.as_str()).is_some()
            {
                self.record_binding_edge_inner(
                    inner.as_str(),
                    ReadsSet::unknown(),
                    analysis,
                    child,
                    visited,
                );
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
pub fn build_effect_dependency_analysis(
    effects: &[Effect],
    unresolved_resources: &HashMap<ResourceId, UnresolvedResource>,
    compositions: &[crate::resource::Composition],
    inputs: ScheduleInputs<'_>,
) -> DependencyAnalysis {
    let mut identity_to_idx: HashMap<ResourceIdentity, usize> = HashMap::new();
    let aliases = match inputs {
        ScheduleInputs::Apply => &[][..],
        ScheduleInputs::Destroy { aliases } => aliases,
    };
    for (idx, effect) in effects.iter().enumerate() {
        identity_to_idx.insert(effect.identity(), idx);
    }
    let alias_offset = effects.len();
    for (alias_idx, alias) in aliases.iter().enumerate() {
        identity_to_idx.insert(
            ResourceIdentity::new(alias.binding.clone()),
            alias_offset + alias_idx,
        );
    }

    let analyzer = DependencyAnalyzer::new(identity_to_idx, compositions);
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
    use crate::effect::ChangedCreateOnly;
    use crate::resource::{ResolvedResource, ResolvedResourceId, ResourceIdentity, State, Value};

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
            from: Box::new(state_for(&resource.id)),
            to: ResolvedResource::new(resource),
            changed_attributes: changed.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn create_effect(binding: &str) -> Effect {
        let mut resource = Resource::new("test", binding);
        resource.binding = Some(binding.to_string());
        Effect::Create(ResolvedResource::new(resource))
    }

    fn delete_effect(identity: &str, dependencies: &[&str], blocked_by_updates: &[&str]) -> Effect {
        Effect::Delete {
            id: ResolvedResourceId::new(ResourceId::with_identity("test", identity)),
            identifier: format!("{identity}-id"),
            directives: Default::default(),
            binding: Some(identity.to_string()),
            dependencies: dependencies
                .iter()
                .map(|dependency| (*dependency).to_string())
                .collect(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: blocked_by_updates
                .iter()
                .cloned()
                .map(ResourceIdentity::new)
                .collect(),
        }
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
        let anonymous = Effect::Delete {
            id: ResolvedResourceId::new(ResourceId::with_identity("test", "anon_1234")),
            identifier: "anonymous-id".to_string(),
            directives: Default::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };
        let mut named_resource = Resource::new("test", "named");
        named_resource.binding = Some("named".to_string());
        let named = Effect::Create(ResolvedResource::new(named_resource));

        assert_eq!(anonymous.identity(), ResourceIdentity::new("anon_1234"));
        assert_ne!(anonymous.identity(), named.identity());
    }

    #[test]
    fn scheduler_index_keys_on_identity_not_string() {
        let first = ResourceIdentity::new("first");
        let second = ResourceIdentity::new("second");
        let analyzer = DependencyAnalyzer::new(
            HashMap::from([(first.clone(), 0), (second.clone(), 1)]),
            &[],
        );

        assert_eq!(analyzer.lookup_by_identity(&first), Some(0));
        assert_eq!(analyzer.lookup_by_identity(&second), Some(1));
        assert_eq!(analyzer.lookup_by_string_ref("first"), Some(0));
        assert_eq!(analyzer.lookup_by_string_ref("second"), Some(1));
    }

    #[test]
    fn delete_apply_edge_for_dependency_uses_blocked_by_if_delete() {
        let delete_target = delete_effect("foo", &[], &[]);
        let dependent_delete = delete_effect("dependent", &["foo"], &[]);
        let deps = build_effect_dependency_analysis(
            &[delete_target, dependent_delete],
            &HashMap::new(),
            &[],
            ScheduleInputs::Apply,
        )
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
            &HashMap::new(),
            &[],
            ScheduleInputs::Apply,
        )
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
            &HashMap::new(),
            &[],
            ScheduleInputs::Apply,
        )
        .into_deps_of();
        assert_eq!(
            total_edges(&deps),
            0,
            "update targets must not be blocked by delete dependencies on apply"
        );
    }

    #[test]
    fn delete_apply_edge_for_blocked_by_updates_uses_depends_on() {
        let consumer = update_effect("consumer", &[], &["acl_id"]);
        let delete = delete_effect("old_web_acl", &[], &["consumer"]);

        let deps = build_effect_dependency_analysis(
            &[consumer, delete],
            &HashMap::new(),
            &[],
            ScheduleInputs::Apply,
        )
        .into_deps_of();

        assert_eq!(total_edges(&deps), 1);
        assert!(
            deps[&1].contains(&0),
            "delete must wait for the consumer update"
        );
    }

    #[test]
    fn delete_destroy_edge_ignores_blocked_by_updates() {
        let parent = delete_effect("parent", &[], &[]);
        let consumer = update_effect("consumer", &[], &["acl_id"]);
        let child = delete_effect("child", &["parent"], &["consumer"]);

        let deps = build_effect_dependency_analysis(
            &[parent, consumer, child],
            &HashMap::new(),
            &[],
            ScheduleInputs::Destroy { aliases: &[] },
        )
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
            &HashMap::new(),
            &[],
            ScheduleInputs::Apply,
        )
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
            identifier: "foo-id".to_string(),
            directives: Default::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };
        let mut blocker_resource = Resource::new("test", "blocker");
        blocker_resource.binding = Some("blocker".to_string());
        let blocker = Effect::Create(ResolvedResource::new(blocker_resource));
        let effects = vec![delete, blocker];
        let analyzer =
            DependencyAnalyzer::new(HashMap::from([(ResourceIdentity::new("foo"), 0)]), &[]);
        let mut analysis = DependencyAnalysis::new(effects.len());

        analyzer.collect_from_schedule_edges(
            &effects,
            vec![ScheduleEdge::BlockedByIfDelete(ResourceIdentity::new(
                "foo",
            ))],
            &mut analysis,
            1,
        );

        assert!(
            analysis.into_deps_of()[&0].contains(&1),
            "anonymous delete must wait for the effect that blocks its identity"
        );
    }

    #[test]
    fn string_ref_resolves_to_anonymous_delete_via_identity_match() {
        let delete = Effect::Delete {
            id: ResolvedResourceId::new(ResourceId::with_identity("test", "foo")),
            identifier: "foo-id".to_string(),
            directives: Default::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };
        let mut consumer_resource = Resource::new("test", "consumer");
        consumer_resource.binding = Some("consumer".to_string());
        consumer_resource
            .dependency_bindings
            .insert("foo".to_string());
        let consumer = Effect::Create(ResolvedResource::new(consumer_resource));

        let deps = build_effect_dependency_analysis(
            &[delete, consumer],
            &HashMap::new(),
            &[],
            ScheduleInputs::Apply,
        )
        .into_deps_of();

        assert!(
            deps[&1].contains(&0),
            "plain string refs resolve by exact ResourceIdentity match"
        );
    }

    #[test]
    fn destroy_delete_edges_block_dependencies_by_consumers() {
        let parent = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test", "parent",
            )),
            identifier: "parent-id".to_string(),
            directives: Default::default(),
            binding: Some("parent".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };
        let child = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test", "child",
            )),
            identifier: "child-id".to_string(),
            directives: Default::default(),
            binding: Some("child".to_string()),
            dependencies: HashSet::from(["parent".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
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
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity("test", "cert")),
            identifier: "cert-id".to_string(),
            directives: Default::default(),
            binding: Some("cert".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        };
        let listener = Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test", "listener",
            )),
            identifier: "listener-id".to_string(),
            directives: Default::default(),
            binding: Some("listener".to_string()),
            dependencies: HashSet::from(["cert_issued".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
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

        let from = State::existing(
            ResourceId::with_identity("test", "replace_me"),
            HashMap::new(),
        )
        .with_dependency_bindings(std::collections::BTreeSet::from(["x".to_string()]));
        let mut to = Resource::new("test", "replace_me");
        to.binding = Some("replace_me".to_string());

        let effects = vec![
            Effect::Create(ResolvedResource::new(x)),
            Effect::Replace {
                from: Box::new(from),
                to: ResolvedResource::new(to),
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
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "test", "listener",
            )),
            identifier: "listener-id".to_string(),
            directives: Default::default(),
            binding: Some("listener".to_string()),
            dependencies: HashSet::from(["missing_wait".to_string()]),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
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
                    UnresolvedResource::from_pre_resolve(to.as_inner().clone()),
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
