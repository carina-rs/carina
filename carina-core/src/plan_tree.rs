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
use crate::effect::Effect;
use crate::plan::{DeferredSummaryAction, DeferredSummaryEntry, Plan};
use crate::resource::{ConcreteValue, DeferredValue, Value};
use crate::utils::enum_display_value;
use crate::value::format_value_with_key;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildRenderItem {
    Normal(usize),
}

pub fn child_render_items(effects: &[Effect], child_indices: &[usize]) -> Vec<ChildRenderItem> {
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

pub fn deferred_summary_for_plan(plan: &Plan) -> DeferredSummaryForPlan {
    let entries = plan
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            Effect::DeferredCreate {
                upstream_binding, ..
            } => Some(DeferredSummaryEntry {
                upstream_binding: upstream_binding.clone(),
                verb: deferred_for_verb(plan, upstream_binding).to_string(),
                action: DeferredSummaryAction::Add,
            }),
            Effect::DeferredReplace {
                upstream_binding, ..
            } => Some(DeferredSummaryEntry {
                upstream_binding: upstream_binding.clone(),
                verb: deferred_for_verb(plan, upstream_binding).to_string(),
                action: DeferredSummaryAction::Replace,
            }),
            _ => None,
        })
        .collect();

    DeferredSummaryForPlan { entries }
}

pub fn is_synthetic_deferred_binding(binding_name: &str) -> bool {
    binding_name
        .rsplit('.')
        .next()
        .is_none_or(|segment| segment.is_empty() || segment.starts_with('_'))
}

pub fn deferred_for_source(template: &crate::parser::DeferredForExpression) -> String {
    if template.iterable_attr.is_empty() {
        template.iterable_binding.clone()
    } else {
        format!("{}.{}", template.iterable_binding, template.iterable_attr)
    }
}

pub fn deferred_for_verb(plan: &Plan, upstream_binding: &str) -> &'static str {
    if plan
        .effects()
        .iter()
        .any(|effect| effect.binding_name().as_deref() == Some(upstream_binding))
    {
        "applies"
    } else {
        "resolves"
    }
}

pub fn deferred_for_display_name(
    template: &crate::parser::DeferredForExpression,
    upstream_binding: &str,
    verb: &str,
) -> String {
    let note = format!("(N records after {upstream_binding} {verb})");
    if is_synthetic_deferred_binding(&template.binding_name) {
        note
    } else {
        format!("{}[*] {note}", template.binding_name)
    }
}

pub fn deferred_for_detail_rows(
    template: &crate::parser::DeferredForExpression,
    upstream_binding: &str,
    verb: &str,
) -> Vec<DetailRow> {
    let mut rows = vec![DetailRow::Text {
        text: format!("<- {}", template.header),
    }];
    let mut attrs: Vec<_> = template.attributes.iter().collect();
    attrs.sort_by_key(|(key, _)| key.clone());
    rows.extend(attrs.into_iter().map(|(key, value)| DetailRow::Attribute {
        key: key.clone(),
        value: format_deferred_for_template_value(value, key, upstream_binding, verb),
        ref_binding: None,
        annotation: None,
    }));
    rows
}

pub fn format_deferred_for_template_value(
    value: &Value,
    key: &str,
    upstream_binding: &str,
    verb: &str,
) -> String {
    let formatted = format_value_with_key(value, Some(key));
    let replaced = replace_known_after_upstream(&formatted, upstream_binding, verb);
    if let Some(inner) = replaced
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .filter(|value| value.starts_with("(known after ") && !value.contains(','))
    {
        inner.to_string()
    } else {
        replaced
    }
}

pub fn replace_known_after_upstream(input: &str, upstream_binding: &str, verb: &str) -> String {
    const PREFIX: &str = "(known after upstream apply";
    let replacement = format!("(known after {upstream_binding} {verb})");
    let mut output = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find(PREFIX) {
        output.push_str(&rest[..start]);
        let after_start = &rest[start..];
        if let Some(end) = after_start.find(')') {
            output.push_str(&replacement);
            rest = &after_start[end + 1..];
        } else {
            output.push_str(after_start);
            rest = "";
        }
    }

    output.push_str(rest);
    output
}

/// Intermediate data for tree-building: maps from effect indices to their
/// bindings, dependency sets, and resource types.
pub struct DependencyGraph {
    pub binding_to_effect: HashMap<String, usize>,
    pub effect_deps: HashMap<usize, HashSet<String>>,
    pub effect_bindings: HashMap<usize, String>,
    pub effect_types: HashMap<usize, String>,
}

/// Build the dependency graph maps from a plan's effects.
pub fn build_dependency_graph(plan: &Plan) -> DependencyGraph {
    let mut binding_to_effect: HashMap<String, usize> = HashMap::new();
    let mut effect_deps: HashMap<usize, HashSet<String>> = HashMap::new();
    let mut effect_bindings: HashMap<usize, String> = HashMap::new();
    let mut effect_types: HashMap<usize, String> = HashMap::new();
    let replacement_display: Vec<_> = plan.replace_display_info().collect();
    let replacement_delete_indices: HashSet<_> = replacement_display
        .iter()
        .map(|metadata| metadata.delete_idx)
        .collect();
    let replacement_create_info: HashMap<_, _> = replacement_display
        .iter()
        .map(|metadata| (metadata.create_idx, *metadata))
        .collect();

    for (idx, effect) in plan.effects().iter().enumerate() {
        if replacement_delete_indices.contains(&idx) {
            continue;
        }

        let (resource, mut deps): (Option<crate::parser::ResourceRef<'_>>, HashSet<String>) =
            match effect {
                // carina#3181 PR D / #3308: `Create`/`Update`/`Read`
                // all carry a typestate struct — reach them through the
                // shared `ResourceRef` view, and assemble the dependency
                // set from value refs + the effect's explicit depends_on.
                Effect::Create(_) | Effect::Update { .. } | Effect::Read { .. } => {
                    let rl = effect
                        .as_resource_ref()
                        .expect("variant carries a resource");
                    let deps = effect.blocking_bindings().into_iter().collect();
                    (Some(rl), deps)
                }
                Effect::Delete {
                    id,
                    binding,
                    dependencies,
                    ..
                } => {
                    let deps = dependencies.clone();
                    if let Some(b) = binding {
                        binding_to_effect.insert(b.clone(), idx);
                        effect_bindings.insert(idx, b.clone());
                    } else {
                        let fallback = id.to_string();
                        binding_to_effect.insert(fallback.clone(), idx);
                        effect_bindings.insert(idx, fallback);
                    }
                    effect_types.insert(idx, id.resource_type.clone());
                    effect_deps.insert(idx, deps);
                    continue;
                }
                Effect::Import { id, .. } | Effect::Remove { id, .. } => {
                    let fallback = id.to_string();
                    binding_to_effect.insert(fallback.clone(), idx);
                    effect_bindings.insert(idx, fallback);
                    effect_types.insert(idx, id.resource_type.clone());
                    effect_deps.insert(idx, HashSet::new());
                    continue;
                }
                Effect::Move { to, .. } => {
                    let fallback = to.to_string();
                    binding_to_effect.insert(fallback.clone(), idx);
                    effect_bindings.insert(idx, fallback);
                    effect_types.insert(idx, to.resource_type.clone());
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
                    deps.insert(target_id.identity_or_empty().to_string());
                    let binding = identity.to_string();
                    binding_to_effect.insert(binding.clone(), idx);
                    effect_bindings.insert(idx, binding.clone());
                    effect_types.insert(idx, "wait".to_string());
                    effect_deps.insert(idx, deps);
                    continue;
                }
                Effect::DeferredCreate {
                    id,
                    upstream_binding,
                    template,
                    ..
                } => {
                    let fallback = id.to_string();
                    binding_to_effect.insert(fallback.clone(), idx);
                    binding_to_effect.insert(template.binding_name.clone(), idx);
                    effect_bindings.insert(idx, fallback);
                    effect_types.insert(idx, "deferred_for".to_string());
                    effect_deps.insert(idx, HashSet::from([upstream_binding.clone()]));
                    continue;
                }
                Effect::DeferredReplace {
                    deletes,
                    id,
                    upstream_binding,
                    template,
                    ..
                } => {
                    let fallback = id.to_string();
                    binding_to_effect.insert(fallback.clone(), idx);
                    binding_to_effect.insert(template.binding_name.clone(), idx);
                    for delete in deletes {
                        if let Some(binding) = &delete.binding {
                            binding_to_effect.insert(binding.clone(), idx);
                        }
                    }
                    effect_bindings.insert(idx, fallback);
                    effect_types.insert(idx, "deferred_for".to_string());
                    effect_deps.insert(idx, HashSet::from([upstream_binding.clone()]));
                    continue;
                }
            };

        if let Some(r) = resource {
            let mut replacement_binding = None;
            let mut replacement_binding_aliases = Vec::new();
            if let Some(metadata) = replacement_create_info.get(&idx)
                && let Some(Effect::Delete {
                    id,
                    binding,
                    dependencies,
                    ..
                }) = plan.effects().get(metadata.delete_idx)
            {
                deps.extend(dependencies.iter().cloned());
                if let Some(binding) = binding {
                    replacement_binding = Some(binding.clone());
                    replacement_binding_aliases.push(binding.clone());
                } else {
                    replacement_binding_aliases.push(id.to_string());
                }
            }

            let resource_binding = r.binding().map(str::to_string);
            let binding = replacement_binding
                .or_else(|| resource_binding.clone())
                .unwrap_or_else(|| r.id().to_string());
            for alias in replacement_binding_aliases {
                binding_to_effect.insert(alias, idx);
            }
            if let Some(resource_binding) = resource_binding
                && resource_binding != binding
            {
                binding_to_effect.insert(resource_binding, idx);
            }
            binding_to_effect.insert(binding.clone(), idx);
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
pub fn build_single_parent_tree(
    plan: &Plan,
    graph: &DependencyGraph,
) -> (Vec<usize>, HashMap<usize, Vec<usize>>) {
    let binding_to_effect = &graph.binding_to_effect;
    let effect_deps = &graph.effect_deps;
    let effect_bindings = &graph.effect_bindings;
    let effect_types = &graph.effect_types;

    let effect_binding_set: HashSet<&str> = binding_to_effect.keys().map(|s| s.as_str()).collect();

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
            let binding_of_idx = effect_bindings.get(&idx).map(|s| s.as_str());
            let all_dependents_have_other_deps = children.iter().all(|&child_idx| {
                effect_deps.get(&child_idx).is_some_and(|child_deps| {
                    child_deps.iter().any(|d| {
                        effect_binding_set.contains(d.as_str())
                            && Some(d.as_str()) != binding_of_idx
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
pub fn extract_compact_hint(
    resource: crate::parser::ResourceRef<'_>,
    parent_binding: Option<&str>,
) -> Option<String> {
    let attrs = resource.attributes();
    let mut keys: Vec<&String> = attrs.keys().filter(|k| !k.starts_with('_')).collect();
    keys.sort();

    // Priority 1: First distinguishing string attribute (most identifying)
    for key in &keys {
        if let Some(Value::Concrete(ConcreteValue::String(s))) = attrs.get(*key)
            && !s.is_empty()
        {
            let short_key = shorten_attr_name(key);
            // Strip namespace from DSL enum identifiers (e.g., awscc.AvailabilityZone.ap_northeast_1a -> "ap_northeast_1a")
            let resolved = enum_display_value(s).unwrap_or(s);
            let display_value = shorten_service_name(key, resolved);
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
    use crate::effect::{ChangedCreateOnly, DeferredReplaceDelete, NonEmptyDeletes};
    use crate::parser::{DeferredForExpression, ForBinding};
    use crate::plan::{Plan, ReplacementDelete, ReplacementGroup};
    use crate::resource::{
        ConcreteValue, Directives, ResolvedResource, Resource, ResourceId, ResourceIdentity, Value,
    };
    use crate::wait::predicate::{AttrPath, WaitPredicate};
    use std::time::Duration;

    fn resolved(resource: Resource) -> ResolvedResource {
        ResolvedResource::new(resource)
    }

    #[test]
    fn explicit_only_depends_on_edge_is_in_dependency_graph() {
        let role = Resource::new("iam.Role", "role").with_binding("role".to_string());
        let mut bucket = Resource::new("s3.Bucket", "bucket").with_binding("bucket".to_string());
        bucket.directives.depends_on = vec!["role".to_string()];

        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(role)));
        plan.add(Effect::Create(resolved(bucket)));
        let graph = build_dependency_graph(&plan);

        let bucket_idx = *graph
            .binding_to_effect
            .get("bucket")
            .expect("bucket effect indexed by binding");
        let bucket_deps = graph
            .effect_deps
            .get(&bucket_idx)
            .expect("bucket effect has deps map entry");
        assert!(
            bucket_deps.contains("role"),
            "explicit depends_on edge should be in dependency graph; got {:?}",
            bucket_deps
        );
    }

    #[test]
    fn replacement_delete_dependencies_and_binding_merge_into_create_node() {
        let igw = Resource::new("ec2.InternetGateway", "igw").with_binding("internet_gateway");
        let create = Resource::new("ec2.Vpc", "vpc").with_binding("vpc_new");
        let mut plan = Plan::new();
        plan.add(Effect::Create(resolved(igw)));
        plan.add_replacement(ReplacementGroup {
            create: resolved(create),
            delete: ReplacementDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "ec2.Vpc", "vpc-old",
                )),
                identifier: "vpc-123".to_string(),
                directives: Directives::default(),
                binding: Some("vpc_old".to_string()),
                dependencies: HashSet::from(["internet_gateway".to_string()]),
                explicit_dependencies: HashSet::from(["internet_gateway".to_string()]),
            },
            create_before_destroy: true,
            changed_create_only: ChangedCreateOnly::new(vec!["cidr_block".to_string()]).unwrap(),
            cascade_ref_hints: Vec::new(),
            temporary_name: None,
            permanent_name_override: None,
            consumer_updates: HashSet::new(),
            previous_attributes: HashMap::from([(
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
            )]),
        });

        let graph = build_dependency_graph(&plan);

        assert!(
            !graph.effect_deps.contains_key(&2),
            "replacement delete should not be a graph entry"
        );
        assert_eq!(graph.binding_to_effect.get("vpc_old"), Some(&1));
        assert_eq!(graph.binding_to_effect.get("vpc_new"), Some(&1));
        assert_eq!(
            graph.effect_bindings.get(&1).map(String::as_str),
            Some("vpc_old")
        );
        assert!(
            graph
                .effect_deps
                .get(&1)
                .is_some_and(|deps| deps.contains("internet_gateway")),
            "delete-side dependencies should be merged into create deps"
        );

        let (roots, dependents) = build_single_parent_tree(&plan, &graph);
        assert_eq!(roots, vec![0]);
        assert_eq!(dependents.get(&0), Some(&vec![1]));
    }

    #[test]
    fn child_render_items_renders_deferred_replace_normally() {
        let template_resource = Resource::new("route53.Record", "validation_records")
            .with_binding("validation_records");
        let deferred = DeferredForExpression {
            file: None,
            line: 1,
            header: "for opt in cert.domain_validation_options".to_string(),
            resource_type: "aws.route53.Record".to_string(),
            attributes: Vec::new(),
            binding_name: "validation_records".to_string(),
            iterable_binding: "cert".to_string(),
            iterable_attr: "domain_validation_options".to_string(),
            binding: ForBinding::Simple("opt".to_string()),
            template_resource,
        };

        let mut plan = Plan::new();
        plan.add(Effect::DeferredReplace {
            deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.Record",
                    "validation_records[0]",
                )),
                identifier: "record-0".to_string(),
                directives: Directives::default(),
                binding: Some("validation_records[0]".to_string()),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            }])
            .expect("fixture has one delete"),
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "validation_records",
            )),
            upstream_binding: "cert".to_string(),
            template: Box::new(deferred),
        });
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.Record",
                "old-record-0",
            )),
            identifier: "record-0".to_string(),
            directives: Directives::default(),
            binding: Some("validation_records[0]".to_string()),
            dependencies: HashSet::new(),
            explicit_dependencies: HashSet::new(),
            blocked_by_updates: HashSet::new(),
        });
        plan.add(Effect::Delete {
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.Record",
                "old-record-abc",
            )),
            identifier: "record-abc".to_string(),
            directives: Directives::default(),
            binding: Some("validation_records[abc]".to_string()),
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
        let template_resource = Resource::new("route53.Record", "validation_records")
            .with_binding("validation_records");
        let deferred = DeferredForExpression {
            file: None,
            line: 1,
            header: "for opt in cert.domain_validation_options".to_string(),
            resource_type: "aws.route53.Record".to_string(),
            attributes: Vec::new(),
            binding_name: "validation_records".to_string(),
            iterable_binding: "cert".to_string(),
            iterable_attr: "domain_validation_options".to_string(),
            binding: ForBinding::Simple("opt".to_string()),
            template_resource,
        };

        let mut plan = Plan::new();
        plan.add(Effect::DeferredReplace {
            deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
                id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                    "route53.Record",
                    "validation_records[0]",
                )),
                identifier: "record-0".to_string(),
                directives: Directives::default(),
                binding: Some("validation_records[0]".to_string()),
                dependencies: HashSet::new(),
                explicit_dependencies: HashSet::new(),
                blocked_by_updates: HashSet::new(),
            }])
            .expect("fixture has one delete"),
            id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "__deferred_for",
                "validation_records",
            )),
            upstream_binding: "cert".to_string(),
            template: Box::new(deferred),
        });
        plan.add(Effect::Wait {
            identity: ResourceIdentity::new("wait_validation_record_0"),
            target_id: crate::resource::ResolvedResourceId::new(ResourceId::with_identity(
                "route53.Record",
                "validation_records[0]",
            )),
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(ConcreteValue::String("ready".to_string())),
            },
            until_surface: "status == 'ready'".to_string(),
            timeout: Duration::from_secs(60),
            interval: Duration::from_secs(1),
            explicit_dependencies: HashSet::new(),
        });

        let graph = build_dependency_graph(&plan);
        assert_eq!(
            graph.binding_to_effect.get("validation_records[0]"),
            Some(&0)
        );

        let wait_idx = *graph
            .binding_to_effect
            .get("wait_validation_record_0")
            .expect("wait effect indexed by binding");
        let wait_deps = graph
            .effect_deps
            .get(&wait_idx)
            .expect("wait effect has deps map entry");
        assert!(
            wait_deps.contains("validation_records[0]"),
            "wait should depend on the deferred replace's absorbed delete binding; got {wait_deps:?}"
        );

        let (_roots, dependents) = build_single_parent_tree(&plan, &graph);
        assert_eq!(dependents.get(&0), Some(&vec![1]));
    }

    #[test]
    fn empty_deferred_binding_is_synthetic() {
        assert!(is_synthetic_deferred_binding(""));
        assert!(is_synthetic_deferred_binding("module._records"));
        assert!(!is_synthetic_deferred_binding("validation_records"));
    }
}
