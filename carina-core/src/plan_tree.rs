//! Shared plan tree-building logic for CLI and TUI.
//!
//! Provides functions to build a dependency graph from a `Plan` and then
//! derive a single-parent tree suitable for hierarchical display.
//!
//! Also provides compact display helpers (`extract_compact_hint`,
//! `shorten_attr_name`, `shorten_service_name`) used by both CLI and TUI.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::detail_rows::DetailRow;
use crate::effect::{Effect, SchedulerKey};
use crate::plan::{DeferredSummaryAction, DeferredSummaryEntry, Plan, ReplaceDisplayInfo};
use crate::resource::{AbsoluteBindingAddress, ConcreteValue, DeferredValue, LoopSlot, Value};
use crate::value::{
    RenderedValue, is_upstream_deferred_marker_value, render_value_with_upstream_deferred_label,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildRenderItem {
    Normal(usize),
}

pub fn child_render_items<S>(
    effects: &[Effect<S>],
    child_indices: &[usize],
) -> Vec<ChildRenderItem> {
    child_indices
        .iter()
        .copied()
        .filter(|idx| effects.get(*idx).is_some())
        .map(ChildRenderItem::Normal)
        .collect()
}

#[derive(Debug, Default)]
pub struct DeferredSummaryForPlan {
    pub entries: Vec<DeferredSummaryEntry>,
}

pub fn deferred_summary_for_plan<S>(plan: &Plan<S>) -> DeferredSummaryForPlan {
    let entries = plan
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            Effect::DeferredCreate(payload) => Some(DeferredSummaryEntry {
                upstream_binding: payload.template.binding_description(),
                verb: deferred_for_verb(plan, &payload.template).to_string(),
                action: DeferredSummaryAction::Add,
            }),
            Effect::DeferredReplace(payload) => Some(DeferredSummaryEntry {
                upstream_binding: payload.template.binding_description(),
                verb: deferred_for_verb(plan, &payload.template).to_string(),
                action: DeferredSummaryAction::Replace,
            }),
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. } => None,
        })
        .collect();

    DeferredSummaryForPlan { entries }
}

pub fn deferred_for_source<S>(
    template: &crate::parser::DeferredForExpression<
        S,
        crate::resource::NoPlaceholder,
        LoopSlot,
        AbsoluteBindingAddress,
    >,
) -> String {
    crate::value::format_value(&template.iterable)
}

pub fn deferred_for_verb<S>(
    plan: &Plan<S>,
    template: &crate::parser::DeferredForExpression<
        S,
        crate::resource::NoPlaceholder,
        LoopSlot,
        AbsoluteBindingAddress,
    >,
) -> &'static str {
    let roots = template.root_bindings();
    if plan.effects().iter().any(|effect| {
        effect
            .binding_address()
            .is_some_and(|binding| roots.contains(binding))
    }) {
        "applies"
    } else {
        "resolves"
    }
}

pub fn deferred_for_display_name<S>(
    template: &crate::parser::DeferredForExpression<
        S,
        crate::resource::NoPlaceholder,
        LoopSlot,
        AbsoluteBindingAddress,
    >,
    verb: &str,
) -> String {
    let upstream_binding = template.binding_description();
    let note = format!("(N records after {upstream_binding} {verb})");
    match template.target() {
        crate::parser::DeferredForTarget::Bound(target) => {
            format!("{}[*] {note}", target.binding())
        }
        crate::parser::DeferredForTarget::Unbound { .. } => note,
    }
}

pub fn deferred_for_detail_rows<S>(
    template: &crate::parser::DeferredForExpression<
        S,
        crate::resource::NoPlaceholder,
        LoopSlot,
        AbsoluteBindingAddress,
    >,
    upstream_binding: &str,
    verb: &str,
) -> Vec<DetailRow> {
    let mut rows = vec![DetailRow::Text {
        text: format!("<- {}", template.header),
    }];
    let mut attrs: Vec<_> = template.attributes().iter().collect();
    attrs.sort_by_key(|(name, _)| *name);
    rows.extend(attrs.into_iter().map(|(key, value)| DetailRow::Attribute {
        key: key.clone(),
        value: render_deferred_for_template_value(value, upstream_binding, verb),
        ref_binding: None,
        annotation: None,
    }));
    rows
}

pub fn render_deferred_for_template_value(
    value: &Value<LoopSlot, AbsoluteBindingAddress>,
    upstream_binding: &str,
    verb: &str,
) -> RenderedValue {
    let value = match value {
        Value::Concrete(ConcreteValue::List(items)) if matches!(items.as_slice(), [item] if is_upstream_deferred_marker_value(item)) => {
            &items[0]
        }
        _ => value,
    };
    render_value_with_upstream_deferred_label(value, upstream_binding, verb)
}

/// Intermediate data for tree-building: maps from effect indices to their
/// bindings, dependency sets, and resource types.
pub struct DependencyGraph {
    pub(crate) binding_to_effect: HashMap<SchedulerKey, usize>,
    pub(crate) effect_deps: HashMap<usize, HashSet<SchedulerKey>>,
    pub effect_bindings: HashMap<usize, String>,
    pub effect_types: HashMap<usize, String>,
}

impl DependencyGraph {
    /// Dependency/dependent effect-index pairs, without exposing scheduler keys
    /// as untyped display strings to graph consumers.
    pub fn dependency_edges(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.effect_deps
            .iter()
            .flat_map(move |(dependent, dependencies)| {
                dependencies.iter().filter_map(move |dependency| {
                    self.binding_to_effect
                        .get(dependency)
                        .map(|dependency| (*dependency, *dependent))
                })
            })
    }
}

/// Build the dependency graph maps from a plan's effects.
pub fn build_dependency_graph<S>(plan: &Plan<S>) -> DependencyGraph {
    build_dependency_graph_from_effects(plan.effects(), plan.replace_display_info())
}

/// Build dependency graph maps from borrowed effect-like values.
///
/// This is the same analysis as [`build_dependency_graph`], but permits
/// display-only callers to append lightweight synthetic effects without
/// cloning the plan or the values carried by its real effects.
pub fn build_dependency_graph_from_effects<'a, E, S>(
    effects: &[E],
    replacement_display: impl IntoIterator<Item = ReplaceDisplayInfo<'a>>,
) -> DependencyGraph
where
    E: AsRef<Effect<S>>,
{
    let mut binding_to_effect: HashMap<SchedulerKey, usize> = HashMap::new();
    let mut effect_deps: HashMap<usize, HashSet<SchedulerKey>> = HashMap::new();
    let mut effect_bindings: HashMap<usize, String> = HashMap::new();
    let mut effect_types: HashMap<usize, String> = HashMap::new();
    let replacement_display: Vec<_> = replacement_display.into_iter().collect();
    let replacement_delete_indices: HashSet<_> = replacement_display
        .iter()
        .map(|metadata| metadata.delete_idx)
        .collect();
    let replacement_create_info: HashMap<_, _> = replacement_display
        .iter()
        .map(|metadata| (metadata.create_idx, *metadata))
        .collect();

    for (idx, effect) in effects.iter().enumerate() {
        let effect = effect.as_ref();
        if replacement_delete_indices.contains(&idx) {
            continue;
        }

        let (resource, mut deps) = match effect {
            // carina#3181 PR D / #3308: `Create`/`Update`/`Read`
            // all carry a typestate struct — reach them through the
            // shared `ResourceRef` view, and assemble the dependency
            // set from value refs + the effect's explicit depends_on.
            Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Read { .. } => {
                let rl = effect
                    .as_resource_ref()
                    .expect("variant carries a resource");
                let deps: HashSet<SchedulerKey> = effect.blocking_keys().into_iter().collect();
                (Some(rl), deps)
            }
            Effect::Delete {
                id,
                binding,
                generation,
                dependencies,
                ..
            } => {
                let deps = dependencies
                    .iter()
                    .cloned()
                    .map(SchedulerKey::Binding)
                    .collect();
                let is_current = generation.is_current();
                if is_current {
                    for key in effect.scheduling_keys() {
                        binding_to_effect.insert(key, idx);
                    }
                }
                if let Some(b) = binding {
                    let b = b.to_string();
                    effect_bindings.insert(idx, b.clone());
                } else {
                    let fallback = id.to_string();
                    effect_bindings.insert(idx, fallback);
                }
                effect_types.insert(idx, id.resource_type.clone());
                effect_deps.insert(idx, deps);
                continue;
            }
            Effect::Import { id, .. } | Effect::Remove { id, .. } => {
                let fallback = id.to_string();
                for key in effect.scheduling_keys() {
                    binding_to_effect.insert(key, idx);
                }
                effect_bindings.insert(idx, fallback);
                effect_types.insert(idx, id.resource_type.clone());
                effect_deps.insert(idx, HashSet::new());
                continue;
            }
            Effect::Move { to, .. } => {
                let fallback = to.to_string();
                for key in effect.scheduling_keys() {
                    binding_to_effect.insert(key, idx);
                }
                effect_bindings.insert(idx, fallback);
                effect_types.insert(idx, to.resource_type.clone());
                effect_deps.insert(idx, HashSet::new());
                continue;
            }
            Effect::ReleaseListKeys { id, .. } => {
                let fallback = id.to_string();
                effect_bindings.insert(idx, fallback);
                effect_types.insert(idx, id.resource_type.clone());
                effect_deps.insert(idx, HashSet::new());
                continue;
            }
            Effect::Wait {
                identity,
                target_id,
                ..
            } => {
                // Scheduling uses `Effect::blocking_bindings()` so
                // wait targets and explicit wait-block dependencies
                // both block dispatch. The display tree intentionally
                // uses only the target edge because it visualizes the
                // structural "wait gates this resource" relationship.
                let mut deps = HashSet::new();
                deps.insert(SchedulerKey::Resource(target_id.id().identity().clone()));
                let binding = identity.to_string();
                binding_to_effect.insert(SchedulerKey::Binding(identity.clone()), idx);
                effect_bindings.insert(idx, binding.clone());
                effect_types.insert(idx, "wait".to_string());
                effect_deps.insert(idx, deps);
                continue;
            }
            Effect::DeferredCreate(payload) => {
                let template = &payload.template;
                let fallback = payload.id().to_string();
                for key in effect.scheduling_keys() {
                    binding_to_effect.insert(key, idx);
                }
                effect_bindings.insert(idx, fallback);
                effect_types.insert(idx, "deferred_for".to_string());
                effect_deps.insert(
                    idx,
                    template
                        .root_bindings()
                        .into_iter()
                        .map(SchedulerKey::Binding)
                        .collect(),
                );
                continue;
            }
            Effect::DeferredReplace(payload) => {
                let fallback = payload.id().to_string();
                for key in effect.scheduling_keys() {
                    binding_to_effect.insert(key, idx);
                }
                for delete in payload.deletes.iter() {
                    binding_to_effect
                        .insert(SchedulerKey::Resource(delete.id.identity().clone()), idx);
                    if let Some(binding) = &delete.binding {
                        binding_to_effect.insert(SchedulerKey::Binding(binding.clone()), idx);
                    }
                }
                effect_bindings.insert(idx, fallback);
                effect_types.insert(idx, "deferred_for".to_string());
                effect_deps.insert(
                    idx,
                    payload
                        .template
                        .root_bindings()
                        .into_iter()
                        .map(SchedulerKey::Binding)
                        .collect(),
                );
                continue;
            }
        };

        if let Some(r) = resource {
            let mut replacement_binding = None;
            if let Some(metadata) = replacement_create_info.get(&idx)
                && let Some(Effect::Delete {
                    id,
                    binding,
                    dependencies,
                    ..
                }) = effects.get(metadata.delete_idx).map(AsRef::as_ref)
            {
                deps.extend(dependencies.iter().cloned().map(SchedulerKey::Binding));
                binding_to_effect.insert(SchedulerKey::Resource(id.identity().clone()), idx);
                if let Some(binding) = binding {
                    replacement_binding = Some(binding);
                    binding_to_effect.insert(SchedulerKey::Binding(binding.clone()), idx);
                }
            }

            let binding = replacement_binding
                .or_else(|| r.binding_address())
                .map(ToString::to_string)
                .unwrap_or_else(|| r.id().to_string());
            for key in effect.scheduling_keys() {
                binding_to_effect.insert(key, idx);
            }
            effect_bindings.insert(idx, binding);
            effect_types.insert(idx, r.id().resource_type.clone());
        }
        effect_deps.insert(idx, deps);
    }

    DependencyGraph {
        binding_to_effect,
        effect_deps,
        effect_bindings,
        effect_types,
    }
}

/// Build a single-parent tree from the dependency graph.
///
/// Each resource is assigned to exactly one parent (or is a root). When a
/// resource depends on multiple resources in the plan, it is placed under the
/// shallowest (most ancestral) dependency. Ties are broken by
/// (resource_type, binding_name) for determinism.
///
/// Returns (roots, dependents) where roots are sorted and each parent's
/// children are sorted by (resource_type, binding_name).
pub fn build_single_parent_tree<S>(
    plan: &Plan<S>,
    graph: &DependencyGraph,
) -> (Vec<usize>, HashMap<usize, Vec<usize>>) {
    let binding_to_effect = &graph.binding_to_effect;
    let effect_deps = &graph.effect_deps;
    let effect_bindings = &graph.effect_bindings;
    let effect_types = &graph.effect_types;

    // Step 1: Build the full reverse dependency map (all parents)
    let mut all_dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..plan.effects().len() {
        all_dependents.insert(idx, Vec::new());
    }
    for (idx, deps) in effect_deps {
        for dep in deps {
            if let Some(&dep_idx) = binding_to_effect.get(dep) {
                all_dependents.entry(dep_idx).or_default().push(*idx);
            }
        }
    }

    // Step 2: Identify initial roots (no deps in plan)
    let mut initial_roots: Vec<usize> = Vec::new();
    for (idx, deps) in effect_deps {
        let has_dep_in_plan = deps.iter().any(|d| binding_to_effect.contains_key(d));
        if !has_dep_in_plan {
            initial_roots.push(*idx);
        }
    }
    initial_roots.sort();

    let sort_key = |idx: &usize| -> (String, String) {
        let rtype = effect_types.get(idx).cloned().unwrap_or_default();
        let binding = effect_bindings.get(idx).cloned().unwrap_or_default();
        (rtype, binding)
    };

    // Step 3: Nest no-dep resources under their first dependent (Issue #928)
    // A no-dep resource can be nested only if every dependent has at least one
    // other dependency in the plan (besides this resource).
    let mut nested_under_dependent: HashSet<usize> = HashSet::new();
    for &idx in &initial_roots {
        let mut children = all_dependents.get(&idx).cloned().unwrap_or_default();
        children.sort_by_key(|a| sort_key(a));
        if !children.is_empty() {
            let all_dependents_have_other_deps = children.iter().all(|&child_idx| {
                effect_deps.get(&child_idx).is_some_and(|child_deps| {
                    child_deps.iter().any(|d| {
                        binding_to_effect
                            .get(d)
                            .is_some_and(|dependency_idx| *dependency_idx != idx)
                    })
                })
            });
            if all_dependents_have_other_deps {
                nested_under_dependent.insert(idx);
            }
        }
    }

    // Step 4: Compute final roots
    let roots: Vec<usize> = initial_roots
        .iter()
        .filter(|idx| !nested_under_dependent.contains(idx))
        .cloned()
        .collect();

    // Step 5: Compute depth for each resource via BFS from roots
    let mut depth: HashMap<usize, usize> = HashMap::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    for &root in &roots {
        depth.insert(root, 0);
        queue.push_back(root);
    }
    // Also set depth for nested-under-dependent resources (they will be
    // children of some non-root resource, but we need them reachable)
    while let Some(node) = queue.pop_front() {
        let d = depth[&node];
        if let Some(children) = all_dependents.get(&node) {
            for &child in children {
                if let std::collections::hash_map::Entry::Vacant(e) = depth.entry(child) {
                    e.insert(d + 1);
                    queue.push_back(child);
                }
            }
        }
    }

    // Step 6: For each non-root resource, select a single parent:
    // the dependency with the shallowest depth (most ancestral).
    // For nested-under-dependent resources, their parent is the first
    // dependent (by sort order).
    let mut dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..plan.effects().len() {
        dependents.insert(idx, Vec::new());
    }

    for (idx, deps) in effect_deps {
        if roots.contains(idx) || nested_under_dependent.contains(idx) {
            continue;
        }
        // Find deps that are in the plan
        let mut parent_candidates: Vec<usize> = deps
            .iter()
            .filter_map(|d| binding_to_effect.get(d).cloned())
            .collect();
        if parent_candidates.is_empty() {
            continue;
        }
        // Pick the shallowest parent; break ties by (resource_type, binding_name)
        parent_candidates.sort_by(|a, b| {
            let da = depth.get(a).copied().unwrap_or(usize::MAX);
            let db = depth.get(b).copied().unwrap_or(usize::MAX);
            da.cmp(&db).then_with(|| sort_key(a).cmp(&sort_key(b)))
        });
        let parent = parent_candidates[0];
        dependents.entry(parent).or_default().push(*idx);
    }

    // Add nested-under-dependent resources as children of the shallowest
    // referencing resource. This ensures resources like IGW are nested under
    // igw_attachment (depth 1) rather than route (depth 2+).
    for &idx in &nested_under_dependent {
        let mut children = all_dependents.get(&idx).cloned().unwrap_or_default();
        // Pick the shallowest dependent; break ties by (resource_type, binding_name)
        children.sort_by(|a, b| {
            let da = depth.get(a).copied().unwrap_or(usize::MAX);
            let db = depth.get(b).copied().unwrap_or(usize::MAX);
            da.cmp(&db).then_with(|| sort_key(a).cmp(&sort_key(b)))
        });
        if let Some(&best_dependent) = children.first() {
            dependents.entry(best_dependent).or_default().push(idx);
        }
    }

    // Step 7: Sort each parent's children by (resource_type, binding_name)
    for children in dependents.values_mut() {
        children.sort_by_key(|a| sort_key(a));
    }

    // Also sort roots by (resource_type, binding_name)
    let mut sorted_roots = roots;
    sorted_roots.sort_by_key(|a| sort_key(a));

    (sorted_roots, dependents)
}

/// Extract a compact hint for anonymous resources.
///
/// Collects the first distinguishing string attribute (like `service_name`) and ALL
/// non-parent `Value::Deferred(DeferredValue::ResourceRef)` attributes, then combines them into a comma-separated
/// hint. String hints appear first (most identifying), followed by ResourceRef hints.
///
/// `parent_binding` is the binding name of the parent resource in the tree, used to
/// skip ResourceRef attributes that redundantly reference the parent.
pub fn extract_compact_hint<S>(
    resource: crate::parser::ResourceRef<'_, S>,
    parent_binding: Option<&str>,
) -> Option<String> {
    let attrs = resource.attributes();
    let mut keys: Vec<&String> = attrs.keys().collect();
    keys.sort();

    // Priority 1: First distinguishing string attribute (most identifying)
    for key in &keys {
        if let Some(Value::Concrete(ConcreteValue::String(s))) = attrs.get(*key)
            && !s.expose_plaintext().is_empty()
        {
            let short_key = shorten_attr_name(key);
            let display_value = shorten_service_name(key, s.expose_plaintext());
            return Some(format!("{}: {}", short_key, display_value));
        }
    }

    // Priority 2: First non-parent ResourceRef attribute (direct or inside a List)
    for key in &keys {
        match attrs.get(*key) {
            Some(Value::Deferred(DeferredValue::ResourceRef { path })) => {
                if parent_binding == Some(path.binding()) {
                    continue;
                }
                let short_key = shorten_attr_name(key);
                return Some(format!("{}: {}", short_key, path.binding()));
            }
            Some(Value::Concrete(ConcreteValue::List(items))) => {
                for item in items {
                    if let Value::Deferred(DeferredValue::ResourceRef { path }) = item {
                        if parent_binding == Some(path.binding()) {
                            continue;
                        }
                        let short_key = shorten_attr_name(key);
                        return Some(format!("{}: {}", short_key, path.binding()));
                    }
                }
            }
            _ => {}
        }
    }

    None
}

/// Shorten common attribute name suffixes for compact display.
/// e.g., `subnet_id` -> `subnet`, `route_table_id` -> `route_table`,
///       `service_name` -> `service`, `group_name` -> `group`
pub fn shorten_attr_name(attr: &str) -> &str {
    attr.strip_suffix("_ids")
        .or_else(|| attr.strip_suffix("_id"))
        .or_else(|| attr.strip_suffix("_name"))
        .unwrap_or(attr)
}

/// For `service_name` attributes, extract just the service suffix from AWS endpoint names.
/// e.g., `com.amazonaws.ap-northeast-1.ecr.dkr` -> `ecr.dkr`
///       `com.amazonaws.ap-northeast-1.s3` -> `s3`
pub fn shorten_service_name<'a>(attr_name: &str, value: &'a str) -> Cow<'a, str> {
    if attr_name == "service_name" {
        // Match pattern: com.amazonaws.<region>.<service...>
        if let Some(rest) = value.strip_prefix("com.amazonaws.") {
            // Skip the region part (e.g., "ap-northeast-1") and take the rest
            if let Some(dot_pos) = rest.find('.') {
                let after_region = &rest[dot_pos + 1..];
                if !after_region.is_empty() {
                    return Cow::Borrowed(after_region);
                }
            }
        }
    }
    Cow::Borrowed(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::{
        ChangedCreateOnly, DeferredReplaceDelete, DeferredReplacePayload, NonEmptyDeletes,
    };
    use crate::parser::{ExpandedDeferredForExpression as DeferredForExpression, ForBinding};
    use crate::plan::{Plan, ReplacementDelete, ReplacementGroup};
    use crate::resource::{
        Directives, ExpandedConcreteValue as ConcreteValue, ExpandedDeferredValue as DeferredValue,
        ExpandedValue as Value, LoopSlot, PathSegment, RelativeBindingName, ResolvedResource,
        Resource, ResourceId, RootScope, State, Subscript,
    };
    use crate::wait::predicate::{AttrPath, WaitPredicate};
    use std::time::Duration;

    type LoopValue = crate::resource::Value<LoopSlot>;
    type LoopConcreteValue = crate::resource::ConcreteValue<LoopSlot>;
    type LoopDeferredValue = crate::resource::DeferredValue<LoopSlot>;
    type LoopInterpolationPart = crate::resource::InterpolationPart<LoopSlot>;
    type LoopResource = crate::resource::Resource<crate::resource::ModuleSource, LoopSlot>;

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource)
    }

    fn absolute(binding: &str) -> crate::resource::AbsoluteBindingAddress {
        RootScope::new().resolve(RelativeBindingName::new(binding))
    }

    #[test]
    fn binding_spelling_cannot_steal_a_resource_identity_dependency() {
        let actual = Resource::new("test.Resource", "shared").with_root_binding("actual");
        let alias = Resource::new("test.Resource", "unrelated").with_root_binding("shared");
        let consumer = Resource::new("test.Resource", "consumer")
            .with_root_binding("consumer")
            .with_attribute(
                "comment",
                Value::root_resource_ref("shared", "identifier", vec![]),
            );
        let actual = resolved(actual);
        let target_id = crate::wait::ManagedWaitTarget::new(&actual);
        let mut plan = Plan::new();
        plan.add(Effect::Create(actual));
        plan.add(Effect::Create(resolved(alias)));
        plan.add(Effect::Wait {
            identity: absolute("ready"),
            target_id,
            until: WaitPredicate::equals(
                AttrPath::single("identifier"),
                Value::Concrete(ConcreteValue::String("ready".into())),
            )
            .expect("valid wait predicate"),
            until_surface: "actual.identifier == 'ready'".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(1),
            explicit_dependencies: HashSet::new(),
        });
        plan.add(Effect::Create(resolved(consumer)));

        let graph = build_dependency_graph(&plan);
        let (roots, dependents) = build_single_parent_tree(&plan, &graph);
        assert_eq!(roots, vec![0, 1]);
        assert_eq!(
            dependents.get(&0),
            Some(&vec![2]),
            "wait must use the resource identity"
        );
        assert_eq!(
            dependents.get(&1),
            Some(&vec![3]),
            "value reference must use the binding"
        );
    }

    #[test]
    fn deferred_for_source_renders_the_complete_coalesce_expression() {
        let template_resource = LoopResource::new("test.Child", "children");
        let deferred = DeferredForExpression::new(
            None,
            1,
            "for x in (up.items ?? fallback.items)".to_string(),
            crate::parser::DeferredForTarget::bound(
                crate::resource::absolute_binding_address_for_test("children"),
            )
            .expect("fixture binding is valid"),
            Value::Deferred(DeferredValue::Coalesce {
                value: Box::new(Value::root_resource_ref("up", "items", Vec::new())),
                default: Box::new(Value::root_resource_ref("fallback", "items", Vec::new())),
            }),
            ForBinding::Simple("x".to_string()),
            template_resource,
        );

        assert_eq!(deferred_for_source(&deferred), "up.items ?? fallback.items");
    }

    #[test]
    fn deferred_for_display_hides_direct_placeholder_access_but_keeps_expressions() {
        let placeholder = LoopValue::Deferred(LoopDeferredValue::Placeholder(LoopSlot::Value));
        let direct_access = crate::resource::apply_postfix_access(
            crate::resource::apply_postfix_access(
                placeholder.clone(),
                PathSegment::Field {
                    name: "resource_record".to_string(),
                },
                0,
                crate::resource::AccessMissPolicy::Error,
            )
            .unwrap(),
            PathSegment::Field {
                name: "name".to_string(),
            },
            0,
            crate::resource::AccessMissPolicy::Error,
        )
        .unwrap();
        assert_eq!(
            render_deferred_for_template_value(&direct_access, "cert", "applies").plain(),
            "(known after cert applies)"
        );
        assert_eq!(
            render_deferred_for_template_value(
                &LoopValue::Concrete(LoopConcreteValue::List(vec![direct_access])),
                "cert",
                "applies",
            )
            .plain(),
            "(known after cert applies)"
        );

        let expression = crate::resource::apply_postfix_access(
            LoopValue::Deferred(LoopDeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    LoopValue::Concrete(LoopConcreteValue::String("/".into())).into(),
                    placeholder.into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 1 },
            },
            0,
            crate::resource::AccessMissPolicy::Error,
        )
        .unwrap();
        assert_eq!(
            render_deferred_for_template_value(&expression, "cert", "applies").plain(),
            "split('/', (known after cert applies))[1]"
        );
    }

    #[test]
    fn deferred_for_display_does_not_rewrite_marker_text_inside_strings() {
        let value = LoopValue::Concrete(LoopConcreteValue::String(
            "(known after upstream apply: user text)".into(),
        ));

        assert_eq!(
            render_deferred_for_template_value(&value, "cert", "applies").plain(),
            "'(known after upstream apply: user text)'"
        );
    }

    #[test]
    fn deferred_for_display_does_not_rewrite_marker_text_inside_map_keys() {
        let value = LoopValue::Concrete(LoopConcreteValue::Map(indexmap::IndexMap::from([(
            "(known after upstream apply: x)".to_string(),
            LoopValue::Deferred(LoopDeferredValue::Placeholder(LoopSlot::Value)),
        )])));

        assert_eq!(
            render_deferred_for_template_value(&value, "cert", "applies").plain(),
            "{'(known after upstream apply: x)': (known after cert applies)}"
        );
    }

    #[test]
    fn deferred_for_display_rewrites_access_inside_interpolation() {
        let access = crate::resource::apply_postfix_access(
            LoopValue::Deferred(LoopDeferredValue::Placeholder(LoopSlot::Value)),
            PathSegment::Field {
                name: "resource_record_name".to_string(),
            },
            0,
            crate::resource::AccessMissPolicy::Error,
        )
        .unwrap();
        let value = LoopValue::Deferred(LoopDeferredValue::Interpolation(vec![
            LoopInterpolationPart::Expr(access),
            LoopInterpolationPart::Literal(".example".into()),
        ]));

        assert_eq!(
            render_deferred_for_template_value(&value, "cert", "applies").plain(),
            "\"${(known after cert applies)}.example\""
        );
    }

    #[test]
    fn compact_hint_preserves_dotted_string_value() {
        let resource: Resource = Resource::new("test.resource", "dotted-name").with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("A.json".into())),
        );

        assert_eq!(
            extract_compact_hint(crate::parser::ResourceRef::Resource(&resource), None),
            Some("name: A.json".to_string())
        );
    }

    #[test]
    fn explicit_only_depends_on_edge_is_in_dependency_graph() {
        let role = Resource::new("iam.Role", "role").with_root_binding("role".to_string());
        let mut bucket =
            Resource::new("s3.Bucket", "bucket").with_root_binding("bucket".to_string());
        bucket.directives.depends_on = vec![absolute("role")];

        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(role)));
        plan.add(Effect::Create(resolved(bucket)));
        let graph = build_dependency_graph(&plan);

        let bucket_idx = *graph
            .binding_to_effect
            .get(&SchedulerKey::Binding(absolute("bucket")))
            .expect("bucket effect indexed by binding");
        let bucket_deps = graph
            .effect_deps
            .get(&bucket_idx)
            .expect("bucket effect has deps map entry");
        assert!(
            bucket_deps.contains(&SchedulerKey::Binding(absolute("role"))),
            "explicit depends_on edge should be in dependency graph; got {:?}",
            bucket_deps
        );
    }

    #[test]
    fn replacement_delete_dependencies_and_binding_merge_into_create_node() {
        let igw = Resource::new("ec2.InternetGateway", "igw").with_root_binding("internet_gateway");
        let create = Resource::new("ec2.Vpc", "vpc").with_root_binding("vpc_new");
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(igw)));
        plan.add_replacement(ReplacementGroup {
            create: resolved(create),
            delete: ReplacementDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "ec2.Vpc", "vpc-old",
                )),
                identifier: crate::effect::fixture_applied_identifier("vpc-123"),
                directives: Directives::default(),
                binding: Some(absolute("vpc_old")),
                dependencies: HashSet::from([absolute("internet_gateway")]),
                explicit_dependencies: HashSet::from([absolute("internet_gateway")]),
            },
            create_before_destroy: true,
            changed_create_only: ChangedCreateOnly::new(vec!["cidr_block".to_string()]).unwrap(),
            cascade_ref_hints: Vec::new(),
            temporary_name: None,
            permanent_name_override: None,
            consumer_updates: HashSet::new(),
            previous_attributes: HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
            )]),
        });

        let graph = build_dependency_graph(&plan);

        assert!(
            !graph.effect_deps.contains_key(&2),
            "replacement delete should not be a graph entry"
        );
        assert_eq!(
            graph
                .binding_to_effect
                .get(&SchedulerKey::Binding(absolute("vpc_old"))),
            Some(&1)
        );
        assert_eq!(
            graph
                .binding_to_effect
                .get(&SchedulerKey::Binding(absolute("vpc_new"))),
            Some(&1)
        );
        assert_eq!(
            graph.effect_bindings.get(&1).map(String::as_str),
            Some("vpc_old")
        );
        assert!(
            graph.effect_deps.get(&1).is_some_and(
                |deps| deps.contains(&SchedulerKey::Binding(absolute("internet_gateway")))
            ),
            "delete-side dependencies should be merged into create deps"
        );

        let (roots, dependents) = build_single_parent_tree(&plan, &graph);
        assert_eq!(roots, vec![0]);
        assert_eq!(dependents.get(&0), Some(&vec![1]));
    }

    #[test]
    fn deposed_delete_does_not_steal_current_binding_parentage() {
        let main = Resource::new("ec2.Vpc", "main")
            .with_root_binding("main")
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.1.0.0/16".into())),
            );
        let subnet = Resource::new("ec2.Subnet", "subnet")
            .with_root_binding("subnet")
            .with_attribute(
                "vpc_id",
                Value::root_resource_ref("main".to_string(), "vpc_id".to_string(), vec![]),
            )
            .with_attribute(
                "cidr_block",
                Value::Concrete(ConcreteValue::String("10.1.1.0/24".into())),
            );
        let mut plan = Plan::new();
        plan.add(Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("main"),
            from: Box::new(State::existing(main.id.clone(), HashMap::new())),
            to: resolved(main),
            changed_attributes: vec!["cidr_block".to_string()],
        });
        plan.add(Effect::Update {
            identifier: crate::effect::fixture_applied_identifier("subnet"),
            from: Box::new(State::existing(subnet.id.clone(), HashMap::new())),
            to: resolved(subnet),
            changed_attributes: vec!["cidr_block".to_string()],
        });
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "ec2.Vpc", "main",
            )),
            identifier: crate::effect::fixture_applied_identifier("vpc-old"),
            generation: crate::effect::EffectGeneration::Deposed(
                crate::resource::DeposedKey::new_unique(),
            ),
            directives: Directives::default(),
            binding: Some(absolute("main")),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });

        let graph = build_dependency_graph(&plan);
        assert_eq!(
            graph
                .binding_to_effect
                .get(&SchedulerKey::Binding(absolute("main"))),
            Some(&0),
            "current instance must keep the binding used by dependents"
        );
        assert_eq!(
            graph.effect_bindings.get(&2).map(String::as_str),
            Some("main"),
            "the deposed delete still needs its own display label"
        );

        let (_roots, dependents) = build_single_parent_tree(&plan, &graph);
        assert!(
            dependents
                .get(&0)
                .is_some_and(|children| children.contains(&1)),
            "dependent update should hang under the current main update, not the deposed delete; dependents: {dependents:?}"
        );
        assert!(
            !dependents
                .get(&2)
                .is_some_and(|children| children.contains(&1)),
            "deposed delete must not parent the dependent update"
        );
    }

    #[test]
    fn child_render_items_renders_deferred_replace_normally() {
        let template_resource = LoopResource::new("aws.route53.Record", "ignored")
            .with_absolute_binding(absolute("validation_records"));
        let deferred = DeferredForExpression::new(
            None,
            1,
            "for opt in cert.domain_validation_options".to_string(),
            crate::parser::DeferredForTarget::bound(
                crate::resource::absolute_binding_address_for_test("validation_records"),
            )
            .expect("fixture binding is valid"),
            Value::root_resource_ref("cert", "domain_validation_options", Vec::new()),
            ForBinding::Simple("opt".to_string()),
            template_resource,
        );

        let mut plan = Plan::new();
        plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.Record",
                    "validation_records[0]",
                )),
                identifier: crate::effect::fixture_applied_identifier("record-0"),
                directives: Directives::default(),
                binding: Some(absolute("validation_records[0]")),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            }])
            .expect("fixture has one delete"),
            template: Box::new(deferred),
        })));
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.Record",
                "old-record-0",
            )),
            identifier: crate::effect::fixture_applied_identifier("record-0"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Directives::default(),
            binding: Some(absolute("validation_records[0]")),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.Record",
                "old-record-abc",
            )),
            identifier: crate::effect::fixture_applied_identifier("record-abc"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Directives::default(),
            binding: Some(absolute("validation_records[abc]")),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });

        assert_eq!(
            child_render_items(plan.effects(), &[0, 1, 2]),
            vec![
                ChildRenderItem::Normal(0),
                ChildRenderItem::Normal(1),
                ChildRenderItem::Normal(2),
            ]
        );
    }

    #[test]
    fn dependency_graph_indexes_deferred_replace_delete_bindings() {
        let template_resource = LoopResource::new("aws.route53.Record", "ignored")
            .with_absolute_binding(absolute("validation_records"));
        let deferred = DeferredForExpression::new(
            None,
            1,
            "for opt in cert.domain_validation_options".to_string(),
            crate::parser::DeferredForTarget::bound(
                crate::resource::absolute_binding_address_for_test("validation_records"),
            )
            .expect("fixture binding is valid"),
            Value::root_resource_ref("cert", "domain_validation_options", Vec::new()),
            ForBinding::Simple("opt".to_string()),
            template_resource,
        );

        let mut plan = Plan::new();
        plan.add(Effect::DeferredReplace(Box::new(DeferredReplacePayload {
            deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.Record",
                    "validation_records[0]",
                )),
                identifier: crate::effect::fixture_applied_identifier("record-0"),
                directives: Directives::default(),
                binding: Some(absolute("validation_records[0]")),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            }])
            .expect("fixture has one delete"),
            template: Box::new(deferred),
        })));
        plan.add(Effect::Wait {
            identity: crate::resource::absolute_binding_address_for_test(
                "wait_validation_record_0",
            ),
            target_id: crate::wait::ManagedWaitTarget::new(
                &crate::resource::ResolvedResource::new(Resource::new(
                    "route53.Record",
                    "validation_records[0]",
                )),
            ),
            until: WaitPredicate::equals(
                AttrPath::single("status"),
                Value::Concrete(ConcreteValue::String("ready".into())),
            )
            .unwrap(),
            until_surface: "status == 'ready'".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(1),
            explicit_dependencies: HashSet::new(),
        });

        let graph = build_dependency_graph(&plan);
        assert_eq!(
            graph
                .binding_to_effect
                .get(&SchedulerKey::Binding(absolute("validation_records[0]"))),
            Some(&0)
        );

        let wait_idx = *graph
            .binding_to_effect
            .get(&SchedulerKey::Binding(absolute("wait_validation_record_0")))
            .expect("wait effect indexed by binding");
        let wait_deps = graph
            .effect_deps
            .get(&wait_idx)
            .expect("wait effect has deps map entry");
        assert!(
            wait_deps.contains(&SchedulerKey::Resource(
                crate::resource::ResourceIdentity::new("validation_records[0]")
            )),
            "wait should depend on the deferred replace's absorbed resource identity; got {wait_deps:?}"
        );

        let (_roots, dependents) = build_single_parent_tree(&plan, &graph);
        assert_eq!(dependents.get(&0), Some(&vec![1]));
    }
}
