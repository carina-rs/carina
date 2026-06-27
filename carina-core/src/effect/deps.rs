use std::collections::{HashMap, HashSet};

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

pub struct DependencyAnalysis {
    deps_of: HashMap<usize, HashSet<usize>>,
    dependents_of: HashMap<usize, HashSet<usize>>,
}

impl DependencyAnalysis {
    fn new(effect_count: usize) -> Self {
        let deps_of = (0..effect_count).map(|idx| (idx, HashSet::new())).collect();
        let dependents_of = (0..effect_count).map(|idx| (idx, HashSet::new())).collect();
        Self {
            deps_of,
            dependents_of,
        }
    }

    fn add_edge(&mut self, child: usize, parent: usize) {
        self.deps_of.entry(child).or_default().insert(parent);
        self.dependents_of.entry(parent).or_default().insert(child);
    }

    pub fn deps_of(&self, child: usize) -> Option<&HashSet<usize>> {
        self.deps_of.get(&child)
    }

    pub fn dependents_of(&self, parent: usize) -> Option<&HashSet<usize>> {
        self.dependents_of.get(&parent)
    }

    pub fn into_deps_of(self) -> HashMap<usize, HashSet<usize>> {
        self.deps_of
    }
}

struct DependencyAnalyzer {
    binding_to_idx: HashMap<String, usize>,
    name_to_delete_idx: HashMap<String, usize>,
    compositions_by_binding: HashMap<String, crate::resource::Composition>,
}

fn resource_synthetic_key(id: &ResourceId) -> String {
    format!("{}:{}", id.resource_type, id.name_str())
}

impl DependencyAnalyzer {
    fn new(
        binding_to_idx: HashMap<String, usize>,
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
            name_to_delete_idx,
            compositions_by_binding,
        }
    }

    fn lookup_delete_idx(&self, binding: &str) -> Option<usize> {
        self.name_to_delete_idx.get(binding).copied()
    }

    fn lookup_idxs(&self, binding: &str) -> Vec<usize> {
        self.binding_to_idx
            .get(binding)
            .copied()
            .or_else(|| self.name_to_delete_idx.get(binding).copied())
            .into_iter()
            .collect()
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
                    self.record_binding_edge(&binding, analysis, self_idx);
                }
                ScheduleEdge::BlockedBy(binding) => {
                    for blocked_idx in self.lookup_idxs(&binding) {
                        analysis.add_edge(blocked_idx, self_idx);
                    }
                }
                ScheduleEdge::BlockedByIfDelete(binding) => {
                    if let Some(blocked_idx) = self.lookup_delete_idx(&binding) {
                        analysis.add_edge(blocked_idx, self_idx);
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
                self.record_binding_edge(path.binding(), analysis, child);
            });
            value.visit_binding_refs(&mut |binding| {
                bindings_seen_in_values.insert(binding.to_string());
                self.record_binding_edge(binding, analysis, child);
            });
        }
        for binding in resource.dependency_bindings() {
            if !bindings_seen_in_values.contains(binding) {
                self.record_binding_edge(binding, analysis, child);
            }
        }
        if let Some(directives) = resource.directives() {
            for binding in &directives.depends_on {
                self.record_binding_edge(binding, analysis, child);
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

    fn record_binding_edge(&self, binding: &str, analysis: &mut DependencyAnalysis, child: usize) {
        let mut visited = HashSet::new();
        self.record_binding_edge_inner(binding, analysis, child, &mut visited);
    }

    fn record_binding_edge_inner<'a>(
        &'a self,
        binding: &'a str,
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
                analysis.add_edge(child, parent);
            }
            return;
        }
        let Some(composition) = self.compositions_by_binding.get(binding) else {
            return;
        };
        for inner in crate::deps::get_composition_dependencies(composition) {
            let key: &'a str =
                if let Some((k, _)) = self.compositions_by_binding.get_key_value(inner.as_str()) {
                    k.as_str()
                } else if let Some((k, _)) = self.binding_to_idx.get_key_value(inner.as_str()) {
                    k.as_str()
                } else {
                    continue;
                };
            self.record_binding_edge_inner(key, analysis, child, visited);
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
            && !matches!(effect, Effect::Delete { .. })
            && effect.as_resource_ref().is_some()
        {
            binding_to_idx
                .entry(resource_synthetic_key(effect.resource_id()))
                .or_insert(idx);
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

    let analyzer = DependencyAnalyzer::new(binding_to_idx, name_to_delete_idx, compositions);
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
    }

    analysis
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
                    analysis.add_edge(idx, *create_idx);
                }
            }
            _ => {}
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
                from: Box::new(state_for(&consumer.id)),
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
    fn anonymous_consumer_blocked_by_updates_edge_is_recorded() {
        let producer_id = ResourceId::new("test.Producer", "main");
        let consumer_id = ResourceId::new("test.Consumer", "anonymous");
        let mut producer = Resource::new("test.Producer", "main");
        producer.binding = Some("producer".to_string());
        let mut consumer = Resource::new("test.Consumer", "anonymous");
        consumer.set_attr(
            "producer_id",
            Value::resource_ref("producer".to_string(), "id".to_string(), vec![]),
        );
        let effects = vec![
            Effect::Create(producer),
            Effect::Update {
                id: consumer_id.clone(),
                from: Box::new(state_for(&consumer_id)),
                to: consumer.clone(),
                changed_attributes: vec!["producer_id".to_string()],
            },
            Effect::Delete {
                id: producer_id,
                identifier: "old-producer".to_string(),
                directives: Default::default(),
                binding: Some("producer".to_string()),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::from(["test.Consumer:anonymous".to_string()]),
            },
        ];
        let unresolved = HashMap::from([(
            consumer_id.clone(),
            UnresolvedResource::from_pre_resolve(consumer),
        )]);

        let deps =
            build_effect_dependency_analysis(&effects, &unresolved, &[], ScheduleInputs::Apply)
                .into_deps_of();

        assert!(
            deps[&2].contains(&1),
            "CBD old delete must wait for anonymous consumer update via synthetic key"
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
}
