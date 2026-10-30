use std::collections::{HashMap, HashSet, VecDeque};

use carina_core::effect::Effect;
use carina_core::executor::{
    PlanSchedulerPhase, UnresolvedResource, build_plan_scheduler_dependency_edges,
};
use carina_core::identifier::anonymous_identity_discriminator;
use carina_core::plan::{Plan, ReplaceDisplayInfo};
use carina_core::plan_tree::build_dependency_graph_from_effects;
use carina_core::resource::{
    EphemeralId, ExpansionTrace, LiveComposition, ModuleSource, PersistentId, ResolvedResource,
    ResolvedResourceId, ResourceId, ResourceIdentity,
};

use crate::plan_summary::{PlanChangeSummary, PlanEffectSummaryPart};

#[derive(Clone, Copy)]
pub struct StateMetadata<'a> {
    pub lineage: &'a str,
    pub serial: u64,
}

#[derive(Clone, Copy)]
pub struct PlanImageInput<'a> {
    pub plan: &'a Plan,
    pub desired_resources: &'a [ResolvedResource],
    /// Identity-resolved snapshots from before value/reference resolution.
    /// The executor scheduler uses these to reconstruct apply-order edges.
    pub unresolved_resources: &'a [UnresolvedResource],
    pub compositions: &'a [LiveComposition],
    pub expansion_trace: Option<&'a ExpansionTrace>,
    pub state: Option<StateMetadata<'a>>,
    pub summary: &'a PlanChangeSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum GraphNodeId {
    Effect(usize),
    Unchanged(ResolvedResourceId),
    CollapsedModule(EphemeralId),
}

impl GraphNodeId {
    pub(crate) fn stable_key(&self) -> String {
        match self {
            Self::Effect(index) => format!("effect:{index:020}"),
            Self::Unchanged(id) => format!("unchanged:{id}"),
            Self::CollapsedModule(id) => format!("module:{}", id.inner()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum EffectKind {
    Read,
    Create,
    ResumeCreate,
    Update,
    Replace,
    Delete,
    Import,
    Remove,
    Move,
    Release,
    Wait,
    DeferredCreate,
    DeferredReplace,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum BlastKind {
    Replace,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlastSource {
    pub id: GraphNodeId,
    pub label: String,
    pub kind: BlastKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphNode {
    pub id: GraphNodeId,
    pub label: String,
    pub resource_type: String,
    pub anonymous_discriminator: Option<String>,
    pub kind: EffectKind,
    pub attribute_names: Vec<String>,
    pub note: Option<String>,
    pub module: Option<EphemeralId>,
    pub affected_by: Vec<BlastSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphEdge {
    /// Edge source. For ordering edges this effect runs first; for reference
    /// edges this resource is the referenced dependency.
    pub from: GraphNodeId,
    /// Edge target. For ordering edges this effect runs later; for reference
    /// edges this resource contains the reference.
    pub to: GraphNodeId,
    pub highlight: Option<BlastKind>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LogicalEdge {
    /// Referenced dependency.
    pub dependency: GraphNodeId,
    /// Resource that references `dependency`.
    pub dependent: GraphNodeId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModuleGroup {
    pub id: EphemeralId,
    pub label: String,
    pub source_path: Option<String>,
    pub collapsed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateSummary {
    pub lineage: String,
    pub serial: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphChangeSummary {
    effect_parts: Vec<PlanEffectSummaryPart>,
    export_change_count: usize,
    residual_deferred_for_count: usize,
    has_changes: bool,
}

impl GraphChangeSummary {
    fn from_plan_summary(summary: &PlanChangeSummary) -> Self {
        Self {
            effect_parts: summary.effect_parts().to_vec(),
            export_change_count: summary.export_change_count(),
            residual_deferred_for_count: summary.residual_deferred_for_count(),
            has_changes: summary.has_changes(),
        }
    }

    pub(crate) fn has_changes(&self) -> bool {
        self.has_changes
    }

    pub(crate) fn effect_parts(&self) -> &[PlanEffectSummaryPart] {
        &self.effect_parts
    }

    pub(crate) fn export_change_count(&self) -> usize {
        self.export_change_count
    }

    pub(crate) fn residual_deferred_for_count(&self) -> usize {
        self.residual_deferred_for_count
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphModel {
    pub nodes: Vec<GraphNode>,
    pub ordering_edges: Vec<GraphEdge>,
    pub reference_edges: Vec<GraphEdge>,
    pub logical_edges: Vec<LogicalEdge>,
    pub modules: Vec<ModuleGroup>,
    pub state: Option<StateSummary>,
    pub summary: GraphChangeSummary,
    node_indices: HashMap<GraphNodeId, usize>,
}

impl GraphModel {
    pub(crate) fn node(&self, id: &GraphNodeId) -> Option<&GraphNode> {
        self.node_indices.get(id).map(|index| &self.nodes[*index])
    }
}

#[derive(Clone)]
struct ModuleInfo {
    id: EphemeralId,
    label: String,
    source_path: Option<String>,
}

pub(crate) fn build_model(input: PlanImageInput<'_>) -> GraphModel {
    let replacements: HashMap<usize, ReplaceDisplayInfo<'_>> = input
        .plan
        .replace_display_info()
        .map(|replacement| (replacement.create_idx, replacement))
        .collect();
    let replacement_deletes: HashMap<usize, usize> = replacements
        .iter()
        .map(|(create_idx, replacement)| (replacement.delete_idx, *create_idx))
        .collect();

    let changed_resource_ids: HashSet<ResourceId> = input
        .plan
        .effects()
        .iter()
        .filter_map(effect_owned_resource_id)
        .cloned()
        .collect();

    // Add unchanged resources as value-free synthetic Create effects only for
    // dependency analysis. Real effects stay borrowed from the plan, while
    // the shells retain just identity, binding, dependency metadata, and
    // directives. Attribute values are never cloned into this analysis view.
    let original_effect_count = input.plan.effects().len();
    let mut synthetic_effects = Vec::new();
    let mut synthetic_nodes = HashMap::new();
    for (resource_idx, resource) in input.desired_resources.iter().enumerate() {
        if changed_resource_ids.contains(&resource.id) {
            continue;
        }
        let id = resource.resolved_id();
        let effect_idx = original_effect_count + synthetic_effects.len();
        synthetic_effects.push(dependency_only_create(resource));
        synthetic_nodes.insert(effect_idx, (GraphNodeId::Unchanged(id), resource_idx));
    }
    let analysis_effects: Vec<&Effect> = input
        .plan
        .effects()
        .iter()
        .chain(synthetic_effects.iter())
        .collect();

    let mut effect_nodes = vec![None; analysis_effects.len()];
    for (idx, node) in effect_nodes
        .iter_mut()
        .enumerate()
        .take(original_effect_count)
    {
        *node = Some(if let Some(create_idx) = replacement_deletes.get(&idx) {
            GraphNodeId::Effect(*create_idx)
        } else {
            GraphNodeId::Effect(idx)
        });
    }
    for (idx, (id, _)) in &synthetic_nodes {
        effect_nodes[*idx] = Some(id.clone());
    }
    let mut module_catalog = build_module_catalog(input.compositions, input.expansion_trace);
    for resource in input.desired_resources {
        if let Some(info) = module_from_source(resource.module_source.as_ref(), &module_catalog) {
            module_catalog.entry(info.id.clone()).or_insert(info);
        }
    }
    let mut nodes = Vec::new();
    for (idx, effect) in input.plan.effects().iter().enumerate() {
        if replacement_deletes.contains_key(&idx) {
            continue;
        }
        let replacement = replacements.get(&idx).copied();
        let module = outer_module(
            effect.resource_id(),
            effect_module_source(effect),
            input.expansion_trace,
            &module_catalog,
        );
        if let Some(info) = &module {
            module_catalog
                .entry(info.id.clone())
                .or_insert_with(|| info.clone());
        }
        nodes.push(node_from_effect(idx, effect, replacement, module.as_ref()));
    }

    for (id, resource_idx) in synthetic_nodes.values() {
        let resource = &input.desired_resources[*resource_idx];
        let module = outer_module(
            &resource.id,
            resource.module_source.as_ref(),
            input.expansion_trace,
            &module_catalog,
        );
        if let Some(info) = &module {
            module_catalog
                .entry(info.id.clone())
                .or_insert_with(|| info.clone());
        }
        let label = resource_label(
            resource.binding.as_deref(),
            &resource.id,
            module.as_ref().map(|info| info.label.as_str()),
        );
        let anonymous_discriminator =
            resource_anonymous_discriminator(resource.binding.as_deref(), &resource.id);
        nodes.push(GraphNode {
            id: id.clone(),
            label,
            resource_type: resource.id.display_type(),
            anonymous_discriminator,
            kind: EffectKind::Unchanged,
            attribute_names: Vec::new(),
            note: None,
            module: module.map(|info| info.id),
            affected_by: Vec::new(),
        });
    }

    let dependency_graph =
        build_dependency_graph_from_effects(&analysis_effects, input.plan.replace_display_info());
    let mut logical_edge_set = HashSet::new();
    let mut effect_dependencies: Vec<_> = dependency_graph.effect_deps.iter().collect();
    effect_dependencies.sort_by_key(|(idx, _)| **idx);
    for (dependent_idx, bindings) in effect_dependencies {
        let mut bindings: Vec<_> = bindings.iter().collect();
        bindings.sort();
        for binding in bindings {
            let Some(dependency_idx) = dependency_graph.binding_to_effect.get(binding) else {
                continue;
            };
            let (Some(dependency), Some(dependent)) = (
                effect_nodes.get(*dependency_idx).and_then(Clone::clone),
                effect_nodes.get(*dependent_idx).and_then(Clone::clone),
            ) else {
                continue;
            };
            if dependency != dependent {
                logical_edge_set.insert((dependency, dependent));
            }
        }
    }
    let mut logical_edges: Vec<_> = logical_edge_set
        .into_iter()
        .map(|(dependency, dependent)| LogicalEdge {
            dependency,
            dependent,
        })
        .collect();
    sort_logical_edges(&mut logical_edges);

    let unresolved_resources: HashMap<_, _> = input
        .unresolved_resources
        .iter()
        .map(|resource| (resource.as_resource().id.clone(), resource))
        .collect();
    let executor_edges = build_plan_scheduler_dependency_edges(
        input.plan.effects(),
        &unresolved_resources,
        input.compositions,
    );
    let mut ordering_edge_set = HashSet::new();
    for (parent, child) in executor_edges {
        if parent.phase == PlanSchedulerPhase::DeferredReplaceTeardown
            || child.phase == PlanSchedulerPhase::DeferredReplaceTeardown
            || replacement_deletes.contains_key(&parent.effect_index)
            || replacement_deletes.contains_key(&child.effect_index)
        {
            continue;
        }
        let (Some(from), Some(to)) = (
            effect_nodes.get(parent.effect_index).and_then(Clone::clone),
            effect_nodes.get(child.effect_index).and_then(Clone::clone),
        ) else {
            continue;
        };
        if from != to {
            ordering_edge_set.insert((from, to));
        }
    }
    let mut ordering_edges: Vec<_> = ordering_edge_set
        .into_iter()
        .map(|(from, to)| GraphEdge {
            from,
            to,
            highlight: None,
        })
        .collect();
    sort_ordering_edges(&mut ordering_edges);

    let logical_adjacency = adjacency(
        logical_edges
            .iter()
            .map(|edge| (&edge.dependency, &edge.dependent)),
    );
    let reverse_logical_adjacency = adjacency(
        logical_edges
            .iter()
            .map(|edge| (&edge.dependent, &edge.dependency)),
    );
    attach_blast_radius(&mut nodes, &logical_adjacency);

    let changed_ids: HashSet<_> = nodes
        .iter()
        .filter(|node| node.kind != EffectKind::Unchanged)
        .map(|node| node.id.clone())
        .collect();
    let downstream_of_changes = reachable(changed_ids.iter().cloned(), &logical_adjacency);
    let upstream_of_changes = reachable(changed_ids.iter().cloned(), &reverse_logical_adjacency);
    let kept_ids: HashSet<_> = nodes
        .iter()
        .filter(|node| {
            node.kind != EffectKind::Unchanged
                || (downstream_of_changes.contains(&node.id)
                    && upstream_of_changes.contains(&node.id))
        })
        .map(|node| node.id.clone())
        .collect();

    let changed_modules: HashSet<_> = nodes
        .iter()
        .filter(|node| node.kind != EffectKind::Unchanged)
        .filter_map(|node| node.module.clone())
        .collect();
    let mut module_counts: HashMap<EphemeralId, usize> = HashMap::new();
    for resource in input.desired_resources {
        if let Some(info) = outer_module(
            &resource.id,
            resource.module_source.as_ref(),
            input.expansion_trace,
            &module_catalog,
        ) {
            module_catalog
                .entry(info.id.clone())
                .or_insert_with(|| info.clone());
            *module_counts.entry(info.id).or_default() += 1;
        }
    }

    let mut module_infos: Vec<_> = module_catalog.values().cloned().collect();
    module_infos.sort_by_key(module_info_sort_key);
    let mut collapse_map: HashMap<GraphNodeId, GraphNodeId> = HashMap::new();
    let mut collapsed_nodes = Vec::new();
    let mut collapsed_module_ids = HashSet::new();
    for module in &module_infos {
        if changed_modules.contains(&module.id) {
            continue;
        }
        let members: Vec<_> = nodes
            .iter()
            .filter(|node| {
                node.kind == EffectKind::Unchanged
                    && node.module.as_ref() == Some(&module.id)
                    && kept_ids.contains(&node.id)
            })
            .collect();
        if members.is_empty() {
            continue;
        }

        let collapsed_id = GraphNodeId::CollapsedModule(module.id.clone());
        let mut affected_by = members
            .iter()
            .flat_map(|node| node.affected_by.iter().cloned())
            .collect::<Vec<_>>();
        sort_and_dedup_blast_sources(&mut affected_by);
        for member in members {
            collapse_map.insert(member.id.clone(), collapsed_id.clone());
        }
        let count = module_counts[&module.id];
        collapsed_nodes.push(GraphNode {
            id: collapsed_id,
            label: format!("{} — {count} unchanged", module.label),
            resource_type: module
                .source_path
                .as_deref()
                .map(|path| format!("module {path}"))
                .unwrap_or_else(|| "module".to_string()),
            anonymous_discriminator: None,
            kind: EffectKind::Unchanged,
            attribute_names: Vec::new(),
            note: None,
            module: Some(module.id.clone()),
            affected_by,
        });
        collapsed_module_ids.insert(module.id.clone());
    }

    nodes.retain(|node| kept_ids.contains(&node.id) && !collapse_map.contains_key(&node.id));
    nodes.extend(collapsed_nodes);

    logical_edges = remap_logical_edges(logical_edges, &kept_ids, &collapse_map);
    ordering_edges = remap_ordering_edges(ordering_edges, &kept_ids, &collapse_map);

    let affected_by_id: HashMap<_, _> = nodes
        .iter()
        .map(|node| (node.id.clone(), node.affected_by.clone()))
        .collect();
    // An ordering edge already explains the visible relationship, even when
    // delete semantics make it run opposite to the logical reference.
    let ordering_pairs: HashSet<_> = ordering_edges
        .iter()
        .flat_map(|edge| {
            [
                (edge.from.clone(), edge.to.clone()),
                (edge.to.clone(), edge.from.clone()),
            ]
        })
        .collect();
    let mut reference_edges: Vec<_> = logical_edges
        .iter()
        .filter(|edge| !ordering_pairs.contains(&(edge.dependency.clone(), edge.dependent.clone())))
        .map(|edge| GraphEdge {
            from: edge.dependency.clone(),
            to: edge.dependent.clone(),
            highlight: None,
        })
        .collect();
    for edge in &mut ordering_edges {
        edge.highlight = edge_highlight(
            affected_by_id.get(&edge.from).map(Vec::as_slice),
            affected_by_id.get(&edge.to).map(Vec::as_slice),
        );
    }
    for edge in &mut reference_edges {
        edge.highlight = edge_highlight(
            affected_by_id.get(&edge.from).map(Vec::as_slice),
            affected_by_id.get(&edge.to).map(Vec::as_slice),
        );
    }

    nodes.sort_by_key(node_sort_key);
    sort_logical_edges(&mut logical_edges);
    sort_ordering_edges(&mut ordering_edges);
    sort_ordering_edges(&mut reference_edges);

    let visible_modules: HashSet<_> = nodes
        .iter()
        .filter_map(|node| node.module.clone())
        .collect();
    let modules: Vec<_> = module_infos
        .into_iter()
        .filter(|module| visible_modules.contains(&module.id))
        .map(|module| ModuleGroup {
            collapsed: collapsed_module_ids.contains(&module.id),
            id: module.id,
            label: module.label,
            source_path: module.source_path,
        })
        .collect();
    let node_indices = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.clone(), index))
        .collect();

    GraphModel {
        nodes,
        ordering_edges,
        reference_edges,
        logical_edges,
        modules,
        state: input.state.map(|state| StateSummary {
            lineage: state.lineage.to_string(),
            serial: state.serial,
        }),
        summary: GraphChangeSummary::from_plan_summary(input.summary),
        node_indices,
    }
}

fn effect_owned_resource_id(effect: &Effect) -> Option<&ResourceId> {
    match effect {
        Effect::Read { resource } => Some(&resource.id),
        Effect::Create(resource) => Some(&resource.id),
        Effect::ResumeCreate { resource, .. } => Some(&resource.id),
        Effect::Update { to, .. } => Some(&to.id),
        Effect::Delete { id, .. } => Some(id.as_inner()),
        Effect::Import { id, .. } => Some(id.as_inner()),
        Effect::Remove { id } => Some(id.as_inner()),
        Effect::Move { to, .. } => Some(to.as_inner()),
        Effect::ReleaseListKeys { id, .. } => Some(id.as_inner()),
        Effect::Wait { .. } => None,
        Effect::DeferredCreate { .. } => None,
        Effect::DeferredReplace(_) => None,
    }
}

fn dependency_only_create(resource: &ResolvedResource) -> Effect {
    // Project from the resolved resource so host-owned schema witnesses stay
    // attached even though this graph-only shell deliberately drops values.
    let mut shell = resource.as_inner().clone();
    shell.attributes.clear();
    shell.prefixes.clear();
    shell.module_source = None;
    shell.default_tag_keys = Default::default();
    shell.quoted_string_attrs.clear();
    shell.dependency_bindings = carina_core::deps::get_resource_dependencies(resource.as_inner())
        .into_iter()
        .collect();
    Effect::Create(ResolvedResource::new(shell))
}

fn effect_module_source(effect: &Effect) -> Option<&ModuleSource> {
    match effect {
        Effect::Read { resource } => resource.module_source.as_ref(),
        Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => {
            resource.module_source.as_ref()
        }
        Effect::Update { to, .. } => to.module_source.as_ref(),
        Effect::DeferredCreate { template, .. } => {
            template.template_resource.module_source.as_ref()
        }
        Effect::DeferredReplace(payload) => {
            payload.template.template_resource.module_source.as_ref()
        }
        Effect::Delete { .. }
        | Effect::Import { .. }
        | Effect::Remove { .. }
        | Effect::Move { .. }
        | Effect::ReleaseListKeys { .. }
        | Effect::Wait { .. } => None,
    }
}

fn node_from_effect(
    idx: usize,
    effect: &Effect,
    replacement: Option<ReplaceDisplayInfo<'_>>,
    module: Option<&ModuleInfo>,
) -> GraphNode {
    let module_label = module.map(|info| info.label.as_str());
    let anonymous_discriminator = effect_anonymous_discriminator(effect);
    let (mut label, resource_type, kind, mut attribute_names, note) = match effect {
        Effect::Read { resource } => (
            resource_label(resource.binding.as_deref(), &resource.id, module_label),
            resource.id.display_type(),
            EffectKind::Read,
            Vec::new(),
            None,
        ),
        Effect::Create(resource) => {
            if let Some(replacement) = replacement {
                (
                    resource_label(resource.binding.as_deref(), &resource.id, module_label),
                    resource.id.display_type(),
                    EffectKind::Replace,
                    replacement.changed_create_only.to_vec(),
                    replacement
                        .create_before_destroy
                        .then(|| "create before destroy".to_string()),
                )
            } else {
                (
                    resource_label(resource.binding.as_deref(), &resource.id, module_label),
                    resource.id.display_type(),
                    EffectKind::Create,
                    Vec::new(),
                    None,
                )
            }
        }
        Effect::ResumeCreate {
            resource,
            started_at: _,
        } => (
            resource_label(resource.binding.as_deref(), &resource.id, module_label),
            resource.id.display_type(),
            EffectKind::ResumeCreate,
            Vec::new(),
            Some("resume create".to_string()),
        ),
        Effect::Update {
            from: _,
            identifier: _,
            to,
            changed_attributes,
        } => (
            resource_label(to.binding.as_deref(), &to.id, module_label),
            to.id.display_type(),
            EffectKind::Update,
            changed_attributes.clone(),
            None,
        ),
        Effect::Delete {
            id,
            identifier: _,
            generation,
            directives: _,
            binding,
            dependencies: _,
            explicit_dependencies: _,
            blocked_by_updates: _,
        } => (
            resource_label(binding.as_deref(), id.as_inner(), module_label),
            id.display_type(),
            EffectKind::Delete,
            Vec::new(),
            (!generation.is_current()).then(|| "deposed generation".to_string()),
        ),
        Effect::Import { id, identifier: _ } => (
            id.identity_str().to_string(),
            id.display_type(),
            EffectKind::Import,
            Vec::new(),
            None,
        ),
        Effect::Remove { id } => (
            id.identity_str().to_string(),
            id.display_type(),
            EffectKind::Remove,
            Vec::new(),
            Some("remove from state".to_string()),
        ),
        Effect::Move { from, to, .. } => (
            to.identity_str().to_string(),
            to.display_type(),
            EffectKind::Move,
            Vec::new(),
            Some(format!("from {from}")),
        ),
        Effect::ReleaseListKeys {
            id,
            attribute,
            key_field: _,
            keys,
        } => (
            id.identity_str().to_string(),
            id.display_type(),
            EffectKind::Release,
            vec![attribute.clone()],
            Some(format!(
                "release {} key{} from authored state",
                keys.len(),
                if keys.len() == 1 { "" } else { "s" }
            )),
        ),
        Effect::Wait {
            identity,
            target_id,
            until: _,
            until_surface: _,
            timeout: _,
            interval: _,
            explicit_dependencies: _,
        } => (
            identity.to_string(),
            format!("wait on {}", target_id.id().display_type()),
            EffectKind::Wait,
            Vec::new(),
            None,
        ),
        Effect::DeferredCreate { id, template } => (
            deferred_label(template, id),
            id.display_type(),
            EffectKind::DeferredCreate,
            Vec::new(),
            deferred_after_note(template),
        ),
        Effect::DeferredReplace(payload) => (
            deferred_label(&payload.template, &payload.id),
            payload.id.display_type(),
            EffectKind::DeferredReplace,
            Vec::new(),
            deferred_after_note(&payload.template),
        ),
    };

    if let Some(origin) = effect.loop_origin() {
        label.push_str("  ");
        origin.append_display_suffix(&mut label);
    }

    attribute_names.sort();
    attribute_names.dedup();
    GraphNode {
        id: GraphNodeId::Effect(idx),
        label,
        resource_type,
        anonymous_discriminator,
        kind,
        attribute_names,
        note,
        module: module.map(|info| info.id.clone()),
        affected_by: Vec::new(),
    }
}

fn deferred_after_note(template: &carina_core::parser::DeferredForExpression) -> Option<String> {
    let root_bindings = template.root_bindings();
    (!root_bindings.is_empty()).then(|| {
        format!(
            "after {}",
            root_bindings.into_iter().collect::<Vec<_>>().join(", ")
        )
    })
}

fn effect_anonymous_discriminator(effect: &Effect) -> Option<String> {
    let (binding, id) = match effect {
        Effect::Read { resource } => (resource.binding.as_deref(), &resource.id),
        Effect::Create(resource) | Effect::ResumeCreate { resource, .. } => {
            (resource.binding.as_deref(), &resource.id)
        }
        Effect::Update { to, .. } => (to.binding.as_deref(), &to.id),
        Effect::Delete { binding, id, .. } => (binding.as_deref(), id.as_inner()),
        Effect::ReleaseListKeys { id, .. } => (None, id.as_inner()),
        Effect::Import { .. }
        | Effect::Remove { .. }
        | Effect::Move { .. }
        | Effect::Wait { .. }
        | Effect::DeferredCreate { .. }
        | Effect::DeferredReplace(_) => return None,
    };
    resource_anonymous_discriminator(binding, id)
}

fn resource_anonymous_discriminator(binding: Option<&str>, id: &ResourceId) -> Option<String> {
    if binding.is_some() {
        None
    } else {
        id.identity().and_then(anonymous_identity_discriminator)
    }
}

fn resource_label(binding: Option<&str>, id: &ResourceId, module_label: Option<&str>) -> String {
    if let Some(binding) = binding {
        return binding.to_string();
    }

    let short_type = id
        .resource_type
        .rsplit('.')
        .next()
        .unwrap_or(&id.resource_type);
    module_label
        .map(|module| format!("{module}.{short_type}"))
        .unwrap_or_else(|| short_type.to_string())
}

fn deferred_label(
    template: &carina_core::parser::DeferredForExpression,
    id: &ResolvedResourceId,
) -> String {
    match &template.target {
        carina_core::parser::DeferredForTarget::Bound { binding } => binding.clone(),
        carina_core::parser::DeferredForTarget::Unbound { .. } => {
            format!(
                "{}.<pending> (deferred for at {})",
                id.display_type(),
                template.target.display_name(),
            )
        }
    }
}

fn build_module_catalog(
    compositions: &[LiveComposition],
    trace: Option<&ExpansionTrace>,
) -> HashMap<EphemeralId, ModuleInfo> {
    let mut catalog = HashMap::new();
    if let Some(trace) = trace {
        let mut traced: Vec<_> = trace
            .iter()
            .filter_map(|(_, sites)| sites.first())
            .map(|site| ModuleInfo {
                id: site.id.clone(),
                label: site.binding().to_string(),
                source_path: site.source_path.clone(),
            })
            .collect();
        traced.sort_by_key(module_info_sort_key);
        for info in traced {
            catalog.entry(info.id.clone()).or_insert(info);
        }
    }
    for composition in compositions {
        let info = {
            let call = composition.call_path().root();
            let Ok(identity) = ResourceIdentity::try_from(call.instance.clone()) else {
                continue;
            };
            ModuleInfo {
                id: EphemeralId::new(ResolvedResourceId::with_identity("_virtual", identity)),
                label: call.display_label(),
                source_path: call.module_source.clone(),
            }
        };
        catalog.entry(info.id.clone()).or_insert(info);
    }
    catalog
}

fn module_from_source(
    source: Option<&ModuleSource>,
    catalog: &HashMap<EphemeralId, ModuleInfo>,
) -> Option<ModuleInfo> {
    let source = source?;
    let identity = source.outer_instance_identity()?;
    let mut exact: Vec<_> = catalog
        .values()
        .filter(|info| info.id.inner().identity() == &identity)
        .cloned()
        .collect();
    exact.sort_by_key(module_info_sort_key);
    if let Some(info) = exact.into_iter().next() {
        return Some(info);
    }
    let label = identity.to_string();
    Some(ModuleInfo {
        id: EphemeralId::new(ResolvedResourceId::with_identity("_virtual", identity)),
        label,
        source_path: None,
    })
}

fn outer_module(
    id: &ResourceId,
    source: Option<&ModuleSource>,
    trace: Option<&ExpansionTrace>,
    catalog: &HashMap<EphemeralId, ModuleInfo>,
) -> Option<ModuleInfo> {
    if let Some(site) =
        trace.and_then(|trace| trace.call_sites_of(&PersistentId::new(id.clone())).first())
    {
        return Some(ModuleInfo {
            id: site.id.clone(),
            label: site.binding().to_string(),
            source_path: site.source_path.clone(),
        });
    }
    if let Some(info) = module_from_source(source, catalog) {
        return Some(info);
    }

    // State-only effects (notably Delete) no longer carry the desired
    // Resource's ModuleSource. Resolve those against known module-call ids via
    // the typed, delimiter-aware address ancestry relation. No binding label
    // or user-supplied resource attribute participates in identity.
    let identity = id.identity()?;
    let mut candidates: Vec<_> = catalog
        .values()
        .filter(|info| identity.is_address_descendant_of(info.id.inner().identity()))
        .cloned()
        .collect();
    candidates.sort_by_key(|info| {
        (
            info.id.inner().identity_str().split('.').count(),
            info.id.inner().identity_str().to_string(),
        )
    });
    candidates.into_iter().next()
}

fn module_info_sort_key(module: &ModuleInfo) -> (String, String, String) {
    (
        module.label.clone(),
        module.source_path.clone().unwrap_or_default(),
        module.id.inner().to_string(),
    )
}

fn node_sort_key(node: &GraphNode) -> (String, String, String, String) {
    (
        node.module
            .as_ref()
            .map(|id| id.inner().to_string())
            .unwrap_or_default(),
        node.resource_type.clone(),
        node.label.clone(),
        node.id.stable_key(),
    )
}

fn sort_logical_edges(edges: &mut [LogicalEdge]) {
    edges.sort_by_key(|edge| (edge.dependency.stable_key(), edge.dependent.stable_key()));
}

fn sort_ordering_edges(edges: &mut [GraphEdge]) {
    edges.sort_by_key(|edge| (edge.from.stable_key(), edge.to.stable_key()));
}

fn adjacency<'a>(
    edges: impl IntoIterator<Item = (&'a GraphNodeId, &'a GraphNodeId)>,
) -> HashMap<GraphNodeId, Vec<GraphNodeId>> {
    let mut adjacency: HashMap<GraphNodeId, Vec<GraphNodeId>> = HashMap::new();
    for (from, to) in edges {
        adjacency.entry(from.clone()).or_default().push(to.clone());
    }
    for next in adjacency.values_mut() {
        next.sort_by_key(GraphNodeId::stable_key);
        next.dedup();
    }
    adjacency
}

fn reachable(
    starts: impl IntoIterator<Item = GraphNodeId>,
    adjacency: &HashMap<GraphNodeId, Vec<GraphNodeId>>,
) -> HashSet<GraphNodeId> {
    let mut reached = HashSet::new();
    let mut queue: VecDeque<_> = starts.into_iter().collect();
    while let Some(id) = queue.pop_front() {
        if !reached.insert(id.clone()) {
            continue;
        }
        if let Some(next) = adjacency.get(&id) {
            queue.extend(next.iter().cloned());
        }
    }
    reached
}

fn attach_blast_radius(
    nodes: &mut [GraphNode],
    logical_adjacency: &HashMap<GraphNodeId, Vec<GraphNodeId>>,
) {
    let sources: Vec<_> = nodes
        .iter()
        .filter_map(|node| {
            let kind = match node.kind {
                EffectKind::Replace | EffectKind::DeferredReplace => BlastKind::Replace,
                EffectKind::Delete => BlastKind::Delete,
                EffectKind::Read
                | EffectKind::Create
                | EffectKind::ResumeCreate
                | EffectKind::Update
                | EffectKind::Import
                | EffectKind::Remove
                | EffectKind::Move
                | EffectKind::Release
                | EffectKind::Wait
                | EffectKind::DeferredCreate
                | EffectKind::Unchanged => return None,
            };
            Some(BlastSource {
                id: node.id.clone(),
                label: node.label.clone(),
                kind,
            })
        })
        .collect();

    let mut affected: HashMap<GraphNodeId, Vec<BlastSource>> = HashMap::new();
    for source in sources {
        let mut descendants = reachable([source.id.clone()], logical_adjacency);
        descendants.remove(&source.id);
        for descendant in descendants {
            affected.entry(descendant).or_default().push(source.clone());
        }
    }
    for node in nodes {
        node.affected_by = affected.remove(&node.id).unwrap_or_default();
        sort_and_dedup_blast_sources(&mut node.affected_by);
    }
}

fn sort_and_dedup_blast_sources(sources: &mut Vec<BlastSource>) {
    sources.sort_by_key(|source| (source.kind, source.label.clone(), source.id.stable_key()));
    sources.dedup_by(|left, right| left.id == right.id);
}

fn remap_logical_edges(
    edges: Vec<LogicalEdge>,
    kept: &HashSet<GraphNodeId>,
    collapsed: &HashMap<GraphNodeId, GraphNodeId>,
) -> Vec<LogicalEdge> {
    let mut unique = HashSet::new();
    for edge in edges {
        if !kept.contains(&edge.dependency) || !kept.contains(&edge.dependent) {
            continue;
        }
        let dependency = collapsed
            .get(&edge.dependency)
            .cloned()
            .unwrap_or(edge.dependency);
        let dependent = collapsed
            .get(&edge.dependent)
            .cloned()
            .unwrap_or(edge.dependent);
        if dependency != dependent {
            unique.insert((dependency, dependent));
        }
    }
    unique
        .into_iter()
        .map(|(dependency, dependent)| LogicalEdge {
            dependency,
            dependent,
        })
        .collect()
}

fn remap_ordering_edges(
    edges: Vec<GraphEdge>,
    kept: &HashSet<GraphNodeId>,
    collapsed: &HashMap<GraphNodeId, GraphNodeId>,
) -> Vec<GraphEdge> {
    let mut unique = HashSet::new();
    for edge in edges {
        if !kept.contains(&edge.from) || !kept.contains(&edge.to) {
            continue;
        }
        let from = collapsed.get(&edge.from).cloned().unwrap_or(edge.from);
        let to = collapsed.get(&edge.to).cloned().unwrap_or(edge.to);
        if from != to {
            unique.insert((from, to));
        }
    }
    unique
        .into_iter()
        .map(|(from, to)| GraphEdge {
            from,
            to,
            highlight: None,
        })
        .collect()
}

fn edge_highlight(from: Option<&[BlastSource]>, to: Option<&[BlastSource]>) -> Option<BlastKind> {
    let kinds = from
        .into_iter()
        .flatten()
        .chain(to.into_iter().flatten())
        .map(|source| source.kind);
    if kinds.clone().any(|kind| kind == BlastKind::Delete) {
        Some(BlastKind::Delete)
    } else if kinds.into_iter().any(|kind| kind == BlastKind::Replace) {
        Some(BlastKind::Replace)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use carina_core::effect::{Effect, EffectGeneration};
    use carina_core::parser::{DeferredForExpression, ForBinding};
    use carina_core::plan::Plan;
    use carina_core::provider::AppliedIdentifier;
    use carina_core::resource::{
        CallSite, ConcreteValue, EphemeralId, ExpansionTrace, PersistentId, ResolvedResource,
        ResolvedResourceId, Resource, ResourceId, State, Value,
    };

    use super::*;

    fn input<'a>(
        plan: &'a Plan,
        desired_resources: &'a [ResolvedResource],
        unresolved_resources: &'a [UnresolvedResource],
        summary: &'a PlanChangeSummary,
    ) -> PlanImageInput<'a> {
        PlanImageInput {
            plan,
            desired_resources,
            unresolved_resources,
            compositions: &[],
            expansion_trace: None,
            state: None,
            summary,
        }
    }

    fn unresolved(resources: &[ResolvedResource]) -> Vec<UnresolvedResource> {
        resources
            .iter()
            .map(|resource| UnresolvedResource::from_pre_resolve(resource.as_inner().clone()))
            .collect()
    }

    fn resolved(resources: impl IntoIterator<Item = Resource>) -> Vec<ResolvedResource> {
        ResolvedResource::collect_resolved(resources).expect("test resources have resolved ids")
    }

    #[test]
    fn dependency_only_create_preserves_resolved_ignore_changes() {
        use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};

        let mut resource = Resource::new("test.Widget", "example").with_attribute(
            "comment",
            Value::Concrete(ConcreteValue::String("configured".into())),
        );
        resource.directives.ignore_changes = vec![
            serde_json::from_value(serde_json::json!({
                "segments": [{ "kind": "attribute", "value": "comment" }]
            }))
            .expect("valid ignore_changes path"),
        ];
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "",
            ResourceSchema::new("test.Widget")
                .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
                .validate()
                .expect("valid graph-model schema"),
        );
        let resource = ResolvedResource::resolve_with_schemas(resource, &schemas)
            .expect("ignore_changes path resolves");

        let effect = dependency_only_create(&resource);
        let Effect::Create(shell) = effect else {
            panic!("dependency shell must be a create effect");
        };

        assert_eq!(shell.ignore_changes(), resource.ignore_changes());
        assert!(shell.attributes.is_empty());
    }

    fn named_resource(binding: &str, dependencies: &[&str]) -> Resource {
        let identity = carina_core::resource::ResourceIdentity::try_from(binding.to_string())
            .expect("test binding is a valid identity");
        let mut resource = Resource::new("test.Widget", identity);
        resource.binding = Some(binding.to_string());
        for dependency in dependencies {
            resource.set_attr(
                format!("ref_{dependency}"),
                Value::resource_ref((*dependency).to_string(), "id".to_string(), vec![]),
            );
        }
        resource
    }

    fn delete(binding: &str, dependencies: &[&str]) -> Effect {
        let identity = carina_core::resource::ResourceIdentity::try_from(binding.to_string())
            .expect("test binding is a valid identity");
        Effect::Delete {
            id: ResolvedResourceId::new(ResourceId::with_identity("test.Widget", identity)),
            identifier: AppliedIdentifier::new(format!("{binding}-provider-id")).unwrap(),
            generation: EffectGeneration::Current,
            directives: Default::default(),
            binding: Some(binding.to_string()),
            dependencies: dependencies
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        }
    }

    #[test]
    fn anonymous_same_type_resources_with_unresolved_names_remain_distinct() {
        let mut first = Resource::new("test.Widget", "anonymous-system-id-1");
        first.set_attr(
            "name",
            Value::resource_ref("future".to_string(), "name".to_string(), vec![]),
        );
        let mut second = Resource::new("test.Widget", "anonymous-system-id-2");
        second.set_attr(
            "name",
            Value::resource_ref("future".to_string(), "name".to_string(), vec![]),
        );

        let mut plan = Plan::new();
        plan.add(Effect::Create(ResolvedResource::new(first.clone())));
        plan.add(Effect::Create(ResolvedResource::new(second.clone())));
        let desired = resolved([first, second]);
        let unresolved = unresolved(&desired);

        let model = build_model(input(
            &plan,
            &desired,
            &unresolved,
            &PlanChangeSummary::new(&plan, &[], &[]),
        ));

        assert_eq!(model.nodes.len(), 2);
        assert!(model.node(&GraphNodeId::Effect(0)).is_some());
        assert!(model.node(&GraphNodeId::Effect(1)).is_some());
    }

    #[test]
    fn anonymous_resource_label_uses_short_type_instead_of_system_identity() {
        let resource = Resource::with_provider(
            "awscc",
            "ec2.SecurityGroup",
            "awscc_ec2_security_group_166e04b3",
            None,
        );
        let mut plan = Plan::new();
        plan.add(Effect::Create(ResolvedResource::new(resource.clone())));

        let desired = resolved([resource]);
        let unresolved = unresolved(&desired);
        let model = build_model(input(
            &plan,
            &desired,
            &unresolved,
            &PlanChangeSummary::new(&plan, &[], &[]),
        ));
        let node = model.node(&GraphNodeId::Effect(0)).unwrap();

        assert_eq!(node.label, "SecurityGroup");
        assert_eq!(node.resource_type, "awscc.ec2.SecurityGroup");
        assert_eq!(node.anonymous_discriminator.as_deref(), Some("166e04b3"));
    }

    #[test]
    fn hash_suffixed_named_resource_stays_named() {
        let mut resource = Resource::with_provider(
            "awscc",
            "ec2.SecurityGroup",
            "named_but_hash_suffixed_166e04b3",
            None,
        );
        resource.binding = Some("production_security_group".to_string());
        let mut plan = Plan::new();
        plan.add(Effect::Create(ResolvedResource::new(resource.clone())));

        let desired = resolved([resource]);
        let unresolved = unresolved(&desired);
        let model = build_model(input(
            &plan,
            &desired,
            &unresolved,
            &PlanChangeSummary::new(&plan, &[], &[]),
        ));
        let node = model.node(&GraphNodeId::Effect(0)).unwrap();

        assert_eq!(node.label, "production_security_group");
        assert_eq!(node.anonymous_discriminator, None);
    }

    #[test]
    fn deferred_effect_with_literal_iterable_has_no_value_detail() {
        const ITERABLE_VALUE: &str = "value-that-must-not-appear";
        let template = DeferredForExpression {
            file: None,
            line: 1,
            header: "for item in literal".to_string(),
            resource_type: "test.Widget".to_string(),
            attributes: Vec::new(),
            target: carina_core::parser::DeferredForTarget::Bound {
                binding: "widgets".to_string(),
            },
            iterable: Value::Concrete(ConcreteValue::String(ITERABLE_VALUE.into())),
            binding: ForBinding::Simple("item".to_string()),
            template_resource: Resource::new("test.Widget", "widgets"),
        };
        let effect = Effect::DeferredCreate {
            id: ResolvedResourceId::new(ResourceId::with_identity("__deferred_for", "widgets")),
            template: Box::new(template),
        };

        let node = node_from_effect(0, &effect, None, None);

        assert_eq!(node.note, None);
        assert!(!format!("{node:?}").contains(ITERABLE_VALUE));
    }

    #[test]
    fn deferred_unbound_label_survives_saved_plan_serde_round_trip() {
        let fixture = tempfile::tempdir().expect("deferred label fixture");
        std::fs::write(
            fixture.path().join("main.crn"),
            "let upstream = upstream_state { source = '../upstream' }\n\
             for value in upstream.accounts {\n\
               mock.test.Child { identity_input = value }\n\
             }\n",
        )
        .expect("write deferred loop fixture");
        let mut parsed = carina_core::config_loader::parse_directory(
            fixture.path(),
            &carina_core::parser::ProviderContext::default(),
        )
        .expect("parse deferred loop fixture");
        let template = parsed
            .deferred_for_expressions
            .pop()
            .expect("fixture contains one deferred loop");
        assert_eq!(template.target.display_name(), "main.crn:2");
        assert!(
            template
                .file
                .as_deref()
                .is_some_and(|file| file.contains(fixture.path().to_string_lossy().as_ref())),
            "the live diagnostic path should be machine-local before serialization"
        );

        let saved = carina_core::parser::SavedDeferredForExpression::from(template);
        let json = serde_json::to_string(&saved).expect("serialize saved deferred template");
        let saved: carina_core::parser::SavedDeferredForExpression =
            serde_json::from_str(&json).expect("deserialize saved deferred template");
        let DeferredForExpression {
            file,
            line,
            header,
            resource_type,
            attributes,
            target,
            iterable,
            binding,
            template_resource: _,
        } = saved;
        let round_tripped = DeferredForExpression {
            file,
            line,
            header,
            resource_type,
            attributes,
            target,
            iterable,
            binding,
            template_resource: Resource::pending_with_provider("mock", "test.Child", None),
        };

        let mut plan = Plan::new();
        plan.add(Effect::DeferredCreate {
            id: ResolvedResourceId::with_provider_identity(
                "mock",
                "__deferred_for",
                "pending_accounts",
                None,
            ),
            template: Box::new(round_tripped),
        });
        let node = node_from_effect(0, &plan.effects()[0], None, None);

        assert!(
            node.label.contains("deferred for at main.crn:2"),
            "saved-plan label lost its display-relative origin: {}",
            node.label,
        );
    }

    #[test]
    fn update_contains_only_changed_attribute_names() {
        let mut resource = named_resource("service", &[]);
        resource.set_attr(
            "password",
            Value::Concrete(ConcreteValue::String("TOP-SECRET".into())),
        );
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            from: Box::new(State::existing(resource.id.clone(), HashMap::new())),
            identifier: AppliedIdentifier::new("service-id").unwrap(),
            to: ResolvedResource::new(resource.clone()),
            changed_attributes: vec!["password".to_string(), "description".to_string()],
        });

        let desired = resolved([resource]);
        let unresolved = unresolved(&desired);
        let model = build_model(input(
            &plan,
            &desired,
            &unresolved,
            &PlanChangeSummary::new(&plan, &[], &[]),
        ));
        let node = model.node(&GraphNodeId::Effect(0)).unwrap();

        assert_eq!(node.kind, EffectKind::Update);
        assert_eq!(node.attribute_names, ["description", "password"]);
        assert!(!format!("{node:?}").contains("TOP-SECRET"));
    }

    #[test]
    fn delete_blast_radius_contains_all_transitive_dependents() {
        let mut plan = Plan::new();
        plan.add(delete("base", &[]));
        plan.add(delete("middle", &["base"]));
        plan.add(delete("leaf", &["middle"]));

        let model = build_model(input(
            &plan,
            &[],
            &[],
            &PlanChangeSummary::new(&plan, &[], &[]),
        ));
        let leaf = model.node(&GraphNodeId::Effect(2)).unwrap();
        let affected_by: HashSet<_> = leaf
            .affected_by
            .iter()
            .map(|source| source.id.clone())
            .collect();

        assert!(affected_by.contains(&GraphNodeId::Effect(0)));
        assert!(affected_by.contains(&GraphNodeId::Effect(1)));
    }

    #[test]
    fn unchanged_nodes_are_kept_only_between_changed_nodes() {
        let source = named_resource("source", &[]);
        let connector = named_resource("connector", &["source"]);
        let sink = named_resource("sink", &["connector"]);
        let unrelated = named_resource("unrelated", &[]);

        let mut plan = Plan::new();
        plan.add(Effect::Create(ResolvedResource::new(source.clone())));
        plan.add(Effect::Create(ResolvedResource::new(sink.clone())));
        let desired = resolved([source, connector.clone(), sink, unrelated.clone()]);
        let unresolved = unresolved(&desired);

        let model = build_model(input(
            &plan,
            &desired,
            &unresolved,
            &PlanChangeSummary::new(&plan, &[], &[]),
        ));
        let connector_id = GraphNodeId::Unchanged(ResolvedResourceId::new(connector.id));
        let unrelated_id = GraphNodeId::Unchanged(ResolvedResourceId::new(unrelated.id));

        assert!(model.node(&connector_id).is_some());
        assert!(model.node(&unrelated_id).is_none());
    }

    #[test]
    fn replace_pair_is_one_node_with_forcing_attributes() {
        let fixture = crate::fixture_plan::build_plan_from_fixture_name("replace_create_only");
        let model = build_model(PlanImageInput {
            plan: &fixture.plan,
            desired_resources: &[],
            unresolved_resources: &[],
            compositions: &[],
            expansion_trace: None,
            state: None,
            summary: &PlanChangeSummary::new(&fixture.plan, &[], &[]),
        });

        let replacements: Vec<_> = model
            .nodes
            .iter()
            .filter(|node| node.kind == EffectKind::Replace)
            .collect();
        assert_eq!(replacements.len(), 2);
        assert!(
            replacements
                .iter()
                .any(|node| node.attribute_names == ["external_name"])
        );
        assert_eq!(model.nodes.len(), 2, "delete halves must not become nodes");
    }

    #[test]
    fn replacement_blast_radius_reaches_transitive_dependents() {
        let fixture = crate::fixture_plan::build_plan_from_fixture_name("replace_create_only");
        let mut plan = fixture.plan;
        let consumer = named_resource("consumer", &["alpha"]);
        plan.add(Effect::Create(ResolvedResource::new(consumer)));

        let model = build_model(input(
            &plan,
            &[],
            &[],
            &PlanChangeSummary::new(&plan, &[], &[]),
        ));
        let source = model
            .nodes
            .iter()
            .find(|node| node.kind == EffectKind::Replace && node.label == "alpha")
            .expect("alpha replacement node");
        let consumer = model
            .nodes
            .iter()
            .find(|node| node.label == "consumer")
            .expect("consumer node");

        assert!(
            consumer
                .affected_by
                .iter()
                .any(|blast| blast.id == source.id && blast.kind == BlastKind::Replace)
        );
    }

    #[test]
    fn unchanged_module_on_changed_path_collapses_to_one_node() {
        let source = named_resource("source", &[]);
        let connector = named_resource("module/connector", &["source"]);
        let sink = named_resource("sink", &["module/connector"]);
        let mut plan = Plan::new();
        plan.add(Effect::Create(ResolvedResource::new(source.clone())));
        plan.add(Effect::Create(ResolvedResource::new(sink.clone())));
        let desired = resolved([source, connector.clone(), sink]);

        let site = CallSite::new(
            EphemeralId::new(ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual.module",
                "network",
            ))),
            "./modules/network",
        );
        let mut trace = ExpansionTrace::new();
        trace.record(PersistentId::new(connector.id.clone()), vec![site.clone()]);
        let unresolved = unresolved(&desired);
        let model = build_model(PlanImageInput {
            expansion_trace: Some(&trace),
            ..input(
                &plan,
                &desired,
                &unresolved,
                &PlanChangeSummary::new(&plan, &[], &[]),
            )
        });

        assert!(
            model
                .node(&GraphNodeId::Unchanged(ResolvedResourceId::new(
                    connector.id
                )))
                .is_none()
        );
        let collapsed = model
            .node(&GraphNodeId::CollapsedModule(site.id))
            .expect("collapsed module node");
        assert_eq!(collapsed.label, "network — 1 unchanged");
        assert!(model.modules.iter().any(|module| module.collapsed));
    }

    #[test]
    fn deleted_resource_uses_exact_module_address_ancestry() {
        let net_leaf = named_resource("net.remaining", &[]);
        let network_leaf = named_resource("network.remaining", &[]);
        let desired = resolved([net_leaf.clone(), network_leaf.clone()]);
        let mut plan = Plan::new();
        plan.add(delete("network.removed", &[]));

        let net_site = CallSite::new(
            EphemeralId::new(ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual", "net",
            ))),
            "./modules/net",
        );
        let network_site = CallSite::new(
            EphemeralId::new(ResolvedResourceId::new(ResourceId::with_identity(
                "_virtual", "network",
            ))),
            "./modules/network",
        );
        let mut trace = ExpansionTrace::new();
        trace.record(PersistentId::new(net_leaf.id), vec![net_site.clone()]);
        trace.record(
            PersistentId::new(network_leaf.id),
            vec![network_site.clone()],
        );
        let unresolved = unresolved(&desired);

        let model = build_model(PlanImageInput {
            expansion_trace: Some(&trace),
            ..input(
                &plan,
                &desired,
                &unresolved,
                &PlanChangeSummary::new(&plan, &[], &[]),
            )
        });
        let deleted = model.node(&GraphNodeId::Effect(0)).expect("delete node");

        assert_eq!(deleted.module.as_ref(), Some(&network_site.id));
        assert_ne!(deleted.module.as_ref(), Some(&net_site.id));
        let module = model
            .modules
            .iter()
            .find(|module| module.id == network_site.id)
            .expect("deleted resource keeps its module band");
        assert!(!module.collapsed, "a module containing a delete is changed");
    }

    #[test]
    fn complex_fixture_preserves_cascade_connector_delete_blast_and_module() {
        let fixture =
            crate::fixture_plan::build_plan_from_fixture_name("plan_image_module_cascade");
        let model = build_model(PlanImageInput {
            plan: &fixture.plan,
            desired_resources: &fixture.desired_resources,
            unresolved_resources: &fixture.unresolved_resources,
            compositions: &fixture.compositions,
            expansion_trace: Some(&fixture.expansion_trace),
            state: None,
            summary: &PlanChangeSummary::new(&fixture.plan, &[], &[]),
        });

        let replacements: Vec<_> = model
            .nodes
            .iter()
            .filter(|node| node.kind == EffectKind::Replace)
            .collect();
        assert_eq!(replacements.len(), 2);
        assert!(replacements.iter().all(|node| {
            node.attribute_names
                .iter()
                .any(|attribute| attribute == "external_name")
        }));
        assert!(
            model
                .nodes
                .iter()
                .any(|node| { node.kind == EffectKind::Unchanged && node.label == "connector" })
        );

        let obsolete = model
            .nodes
            .iter()
            .find(|node| node.label == "obsolete")
            .expect("obsolete delete");
        let child = model
            .nodes
            .iter()
            .find(|node| node.label == "obsolete_child")
            .expect("obsolete child delete");
        assert!(
            child
                .affected_by
                .iter()
                .any(|source| { source.id == obsolete.id && source.kind == BlastKind::Delete })
        );
        assert!(
            model
                .nodes
                .iter()
                .any(|node| { node.label == "review.module_node" && node.module.is_some() })
        );
    }
}
