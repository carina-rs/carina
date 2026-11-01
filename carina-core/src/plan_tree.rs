//! Shared plan tree-building logic for CLI and TUI.
//!
//! Provides functions to build a dependency graph from a `Plan` and then
//! derive a single-parent tree suitable for hierarchical display.
//!
//! Also provides compact display helpers (`extract_compact_hint`,
//! `shorten_attr_name`, `shorten_service_name`) used by both CLI and TUI.

use crate::resource::ResolvedResourceId;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::detail_rows::DetailRow;
use crate::effect::Effect;
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

pub fn deferred_for_verb<S, D>(
    plan: &Plan<S>,
    template: &crate::parser::DeferredForExpression<
        S,
        crate::resource::NoPlaceholder,
        LoopSlot,
        AbsoluteBindingAddress,
        D,
    >,
) -> &'static str {
    let roots = template.root_bindings();
    if plan.effects().iter().any(|effect| {
        effect
            .binding_address()
            .is_some_and(|binding| roots.contains(binding.as_ref()))
    }) {
        "applies"
    } else {
        "resolves"
    }
}

pub fn deferred_for_display_name<S, D>(
    template: &crate::parser::DeferredForExpression<
        S,
        crate::resource::NoPlaceholder,
        LoopSlot,
        AbsoluteBindingAddress,
        D,
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

pub fn deferred_for_detail_rows<S, D>(
    template: &crate::parser::DeferredForExpression<
        S,
        crate::resource::NoPlaceholder,
        LoopSlot,
        AbsoluteBindingAddress,
        D,
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
    pub binding_to_effect: HashMap<crate::resource::AbsoluteBindingAddress, usize>,
    pub id_to_effect: HashMap<ResolvedResourceId, usize>,
    pub effect_deps: HashMap<usize, crate::resource::ResolvedDependencies>,
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
                    self.id_to_effect
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
    let replacement_display = replacement_display.into_iter().collect::<Vec<_>>();
    let suppressed_deletes = replacement_display
        .iter()
        .map(|metadata| metadata.delete_idx)
        .collect::<HashSet<_>>();
    let replacement_creates = replacement_display
        .iter()
        .map(|metadata| (metadata.create_idx, metadata.delete_idx))
        .collect::<HashMap<_, _>>();
    let mut graph = DependencyGraph {
        binding_to_effect: HashMap::new(),
        id_to_effect: HashMap::new(),
        effect_deps: HashMap::new(),
        effect_bindings: HashMap::new(),
        effect_types: HashMap::new(),
    };
    for (index, effect) in effects.iter().enumerate() {
        if suppressed_deletes.contains(&index) {
            continue;
        }
        let effect = effect.as_ref();
        let id = effect.scheduler_id();
        let mut dependencies = match effect {
            Effect::Wait { target_id, .. } => std::iter::once(target_id.id().clone()).collect(),
            Effect::Delete { dependencies, .. } => dependencies.to_resolved_dependencies(),
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::DeferredCreate(_)
            | Effect::DeferredReplace(_) => effect.blocking_keys(),
            Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. } => Default::default(),
        };
        let mut visible_binding = effect.binding_address().map(std::borrow::Cow::into_owned);
        if let Some(delete_index) = replacement_creates.get(&index)
            && let Some(Effect::Delete {
                id: old_id,
                binding,
                dependencies: prior,
                ..
            }) = effects.get(*delete_index).map(AsRef::as_ref)
        {
            dependencies.extend(prior.iter().cloned());
            graph.id_to_effect.insert(old_id.clone(), index);
            if let Some(binding) = binding {
                visible_binding = Some(binding.clone());
                graph.binding_to_effect.insert(binding.clone(), index);
            }
        }
        let owns_current_address = !matches!(
            effect,
            Effect::Delete {
                generation: crate::effect::EffectGeneration::Deposed(_),
                ..
            } | Effect::ReleaseListKeys { .. }
        );
        if owns_current_address {
            if matches!(effect, Effect::Move { .. }) {
                graph.id_to_effect.entry(id.clone()).or_insert(index);
            } else {
                graph.id_to_effect.insert(id.clone(), index);
            }
            graph.binding_to_effect.insert(id.binding_address(), index);
            if let Some(binding) = effect.binding_address() {
                graph.binding_to_effect.insert(binding.into_owned(), index);
            }
        }
        match effect {
            Effect::DeferredCreate(payload) => {
                if let Some(binding) = payload.template.target().bound_binding_address() {
                    graph.binding_to_effect.insert(binding.clone(), index);
                }
            }
            Effect::DeferredReplace(payload) => {
                if let Some(binding) = payload.template.target().bound_binding_address() {
                    graph.binding_to_effect.insert(binding.clone(), index);
                }
                for delete in payload.deletes.iter() {
                    graph.id_to_effect.insert(delete.id.clone(), index);
                    if let Some(binding) = &delete.binding {
                        graph.binding_to_effect.insert(binding.clone(), index);
                    }
                }
            }
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. }
            | Effect::Wait { .. } => {}
        }
        let (binding, resource_type) = match effect {
            Effect::Wait { .. } => (id.identity_str().to_owned(), "wait".to_owned()),
            Effect::DeferredCreate(_) | Effect::DeferredReplace(_) => {
                (id.to_string(), "deferred_for".to_owned())
            }
            Effect::Read { .. }
            | Effect::Create(_)
            | Effect::ResumeCreate { .. }
            | Effect::Update { .. }
            | Effect::Delete { .. }
            | Effect::Import { .. }
            | Effect::Remove { .. }
            | Effect::Move { .. }
            | Effect::ReleaseListKeys { .. } => (
                visible_binding
                    .map(|binding| binding.to_string())
                    .unwrap_or_else(|| id.to_string()),
                id.resource_type().to_owned(),
            ),
        };
        graph.effect_bindings.insert(index, binding);
        graph.effect_types.insert(index, resource_type);
        graph.effect_deps.insert(index, dependencies);
    }
    graph
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
    let id_to_effect = &graph.id_to_effect;
    let effect_deps = &graph.effect_deps;
    let effect_bindings = &graph.effect_bindings;
    let effect_types = &graph.effect_types;

    // Step 1: Build the full reverse dependency map (all parents)
    let mut all_dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..plan.effects().len() {
        all_dependents.insert(idx, Vec::new());
    }
    for (idx, deps) in effect_deps {
        for dep in deps.iter() {
            if let Some(&dep_idx) = id_to_effect.get(dep) {
                all_dependents.entry(dep_idx).or_default().push(*idx);
            }
        }
    }

    // Step 2: Identify initial roots (no deps in plan)
    let mut initial_roots: Vec<usize> = Vec::new();
    for (idx, deps) in effect_deps {
        let has_dep_in_plan = deps.iter().any(|d| id_to_effect.contains_key(d));
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
                    child_deps
                        .iter()
                        .filter_map(|dependency| id_to_effect.get(dependency))
                        .any(|dependency_idx| *dependency_idx != idx)
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
            .filter_map(|d| id_to_effect.get(d).cloned())
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
pub fn extract_compact_hint<
    S,
    P: crate::resource::PlaceholderKind,
    T: crate::resource::PlaceholderKind,
    D,
    B: crate::resource::BindingAddress,
    R,
>(
    resource: crate::parser::ResourceRef<'_, S, P, T, D, B, R>,
    parent_binding: Option<&str>,
) -> Option<String>
where
    D: crate::parser::DeferredForPhaseTraversal<P, T, B>,
{
    match resource {
        crate::parser::ResourceRef::Resource(resource) => {
            extract_compact_hint_from_attributes(&resource.attributes, parent_binding)
        }
        crate::parser::ResourceRef::DataSource(resource) => {
            extract_compact_hint_from_attributes(&resource.attributes, parent_binding)
        }
        crate::parser::ResourceRef::LiveComposition(composition) => {
            let attributes = composition
                .signature
                .attributes
                .iter()
                .filter_map(|(name, attribute)| {
                    attribute.value().map(|value| (name.clone(), value))
                })
                .collect();
            extract_compact_hint_from_attributes(&attributes, parent_binding)
        }
        crate::parser::ResourceRef::Deferred { resource, .. } => {
            extract_compact_hint_from_attributes(resource.attributes(), parent_binding)
        }
    }
}

fn extract_compact_hint_from_attributes<
    P: crate::resource::PlaceholderKind,
    B: crate::resource::BindingAddress,
>(
    attrs: &indexmap::IndexMap<String, crate::resource::Value<P, B>>,
    parent_binding: Option<&str>,
) -> Option<String> {
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
    use crate::effect::SchedulerKey;
    use crate::effect::{ChangedCreateOnly, DeferredReplaceDelete, NonEmptyDeletes};
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
        ResolvedResource::new(resource.with_dependency_phase(()))
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
        let mut resources = ResolvedResource::collect_resolved([actual, alias, consumer])
            .expect("fixture binding addresses resolve independently of identities");
        let actual = resources.remove(0);
        let alias = resources.remove(0);
        let consumer = resources.remove(0);
        let target_id = crate::wait::ManagedWaitTarget::new(&actual);
        let mut plan = Plan::new();
        plan.add(Effect::Create(actual));
        plan.add(Effect::Create(alias));
        plan.add(Effect::Wait {
            id: ResolvedResourceId::with_identity("__wait", "ready"),
            target_id,
            until: WaitPredicate::equals(
                AttrPath::single("identifier"),
                Value::Concrete(ConcreteValue::String("ready".into())),
            )
            .expect("valid wait predicate"),
            until_surface: "actual.identifier == 'ready'".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(1),
            explicit_dependencies: Default::default(),
        });
        plan.add(Effect::Create(consumer));

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
            extract_compact_hint(
                crate::parser::ResourceRef::<
                    _,
                    crate::resource::NoPlaceholder,
                    crate::resource::LoopSlot,
                >::Resource(&resource),
                None,
            ),
            Some("name: A.json".to_string())
        );
    }

    #[test]
    fn explicit_only_depends_on_edge_is_in_dependency_graph() {
        let role = Resource::new("iam.Role", "role").with_root_binding("role".to_string());
        let mut bucket =
            Resource::new("s3.Bucket", "bucket").with_root_binding("bucket".to_string());
        bucket.dependencies.depends_on = vec![absolute("role")].into();

        let mut plan = Plan::new();
        let mut resources =
            ResolvedResource::collect_resolved([role, bucket]).expect("explicit fixture graph");
        plan.add(Effect::Create(resources.remove(0)));
        plan.add(Effect::Create(resources.remove(0)));
        let graph = build_dependency_graph(&plan);

        let bucket_idx = *graph
            .binding_to_effect
            .get(&absolute("bucket"))
            .expect("bucket effect indexed by binding");
        let bucket_deps = graph
            .effect_deps
            .get(&bucket_idx)
            .expect("bucket effect has deps map entry");
        assert!(
            bucket_deps.contains(&SchedulerKey::with_identity("iam.Role", "role")),
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
                dependencies: crate::test_support::persisted([SchedulerKey::with_identity(
                    "ec2.InternetGateway",
                    "igw",
                )]),
                explicit_dependencies: std::iter::once(SchedulerKey::with_identity(
                    "ec2.InternetGateway",
                    "igw",
                ))
                .collect(),
            },
            create_before_destroy: true,
            changed_create_only: ChangedCreateOnly::new(vec!["cidr_block".to_string()]).unwrap(),
            cascade_ref_hints: Vec::new(),
            temporary_name: None,
            permanent_name_override: None,
            consumer_updates: Default::default(),
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
        assert_eq!(graph.binding_to_effect.get(&absolute("vpc_old")), Some(&1));
        assert_eq!(graph.binding_to_effect.get(&absolute("vpc_new")), Some(&1));
        assert_eq!(
            graph.effect_bindings.get(&1).map(String::as_str),
            Some("vpc_old")
        );
        assert!(
            graph
                .effect_deps
                .get(&1)
                .is_some_and(|deps| deps
                    .contains(&SchedulerKey::with_identity("ec2.InternetGateway", "igw"))),
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
            to: ResolvedResource::from_resolved_parts(
                subnet.with_dependency_phase(()),
                std::iter::once(crate::resource::ResolvedResourceId::with_identity(
                    "ec2.Vpc", "main",
                ))
                .collect(),
                Default::default(),
            ),
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
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
        });

        let graph = build_dependency_graph(&plan);
        assert_eq!(
            graph.binding_to_effect.get(&absolute("main")),
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
        plan.add(Effect::DeferredReplace(Box::new(
            crate::test_support::compiled_replace(
                NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                    id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                        "route53.Record",
                        "validation_records[0]",
                    )),
                    identifier: crate::effect::fixture_applied_identifier("record-0"),
                    directives: Directives::default(),
                    binding: Some(absolute("validation_records[0]")),
                    dependencies: Default::default(),
                    explicit_dependencies: Default::default(),
                    blocked_by_updates: Default::default(),
                }])
                .expect("fixture has one delete"),
                Box::new(deferred),
            ),
        )));
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.Record",
                "old-record-0",
            )),
            identifier: crate::effect::fixture_applied_identifier("record-0"),
            generation: crate::effect::EffectGeneration::Current,
            directives: Directives::default(),
            binding: Some(absolute("validation_records[0]")),
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
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
            dependencies: Default::default(),
            explicit_dependencies: Default::default(),
            blocked_by_updates: Default::default(),
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
        plan.add(Effect::DeferredReplace(Box::new(
            crate::test_support::compiled_replace(
                NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                    id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                        "route53.Record",
                        "validation_records[0]",
                    )),
                    identifier: crate::effect::fixture_applied_identifier("record-0"),
                    directives: Directives::default(),
                    binding: Some(absolute("validation_records[0]")),
                    dependencies: Default::default(),
                    explicit_dependencies: Default::default(),
                    blocked_by_updates: Default::default(),
                }])
                .expect("fixture has one delete"),
                Box::new(deferred),
            ),
        )));
        plan.add(Effect::Wait {
            id: crate::resource::ResolvedResourceId::with_identity(
                "__wait",
                "wait_validation_record_0",
            ),
            target_id: crate::wait::ManagedWaitTarget::new(
                &crate::resource::ResolvedResource::new(
                    Resource::new("route53.Record", "validation_records[0]")
                        .with_dependency_phase(()),
                ),
            ),
            until: WaitPredicate::equals(
                AttrPath::single("status"),
                Value::Concrete(ConcreteValue::String("ready".into())),
            )
            .unwrap(),
            until_surface: "status == 'ready'".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(1),
            explicit_dependencies: Default::default(),
        });

        let graph = build_dependency_graph(&plan);
        assert_eq!(
            graph
                .binding_to_effect
                .get(&absolute("validation_records[0]")),
            Some(&0)
        );

        let wait_idx = *graph
            .binding_to_effect
            .get(&absolute("wait_validation_record_0"))
            .expect("wait effect indexed by binding");
        let wait_deps = graph
            .effect_deps
            .get(&wait_idx)
            .expect("wait effect has deps map entry");
        assert!(
            wait_deps.contains(&SchedulerKey::with_identity(
                "route53.Record",
                "validation_records[0]"
            )),
            "wait should depend on the deferred replace's absorbed resource identity; got {wait_deps:?}"
        );

        let (_roots, dependents) = build_single_parent_tree(&plan, &graph);
        assert_eq!(dependents.get(&0), Some(&vec![1]));
    }
}
