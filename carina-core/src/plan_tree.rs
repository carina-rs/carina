//! Shared plan tree-building logic for CLI and TUI.
//!
//! Provides functions to build a dependency graph from a `Plan` and then
//! derive a single-parent tree suitable for hierarchical display.
//!
//! Also provides compact display helpers (`extract_compact_hint`,
//! `shorten_attr_name`, `shorten_service_name`) used by both CLI and TUI.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::deps::get_resource_dependencies;
use crate::effect::Effect;
use crate::plan::Plan;
use crate::resource::Value;
use crate::utils::{convert_enum_value, is_dsl_enum_format};

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

    for (idx, effect) in plan.effects().iter().enumerate() {
        let (resource, deps) = match effect {
            Effect::Create(r) => (Some(r), get_resource_dependencies(r)),
            Effect::Update { to, .. } => (Some(to), get_resource_dependencies(to)),
            Effect::Replace { to, .. } => (Some(to), get_resource_dependencies(to)),
            Effect::Read { resource } => (Some(resource), get_resource_dependencies(resource)),
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
        };

        if let Some(r) = resource {
            let binding = r.binding.clone().unwrap_or_else(|| r.id.to_string());
            binding_to_effect.insert(binding.clone(), idx);
            effect_bindings.insert(idx, binding);
            effect_types.insert(idx, r.id.resource_type.clone());
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
/// non-parent `Value::ResourceRef` attributes, then combines them into a comma-separated
/// hint. String hints appear first (most identifying), followed by ResourceRef hints.
///
/// `parent_binding` is the binding name of the parent resource in the tree, used to
/// skip ResourceRef attributes that redundantly reference the parent.
pub fn extract_compact_hint(
    resource: &crate::resource::Resource,
    parent_binding: Option<&str>,
) -> Option<String> {
    let mut keys: Vec<_> = resource
        .attributes
        .keys()
        .filter(|k| !k.starts_with('_'))
        .collect();
    keys.sort();

    // Priority 1: First distinguishing string attribute (most identifying)
    for key in &keys {
        if let Some(Value::String(s)) = resource.get_attr(key)
            && !s.is_empty()
        {
            let short_key = shorten_attr_name(key);
            // Resolve DSL enum identifiers (e.g., awscc.AvailabilityZone.ap_northeast_1a -> "ap-northeast-1a")
            let resolved = if is_dsl_enum_format(s) {
                Cow::Owned(convert_enum_value(s))
            } else {
                Cow::Borrowed(s.as_str())
            };
            let display_value = shorten_service_name(key, &resolved);
            return Some(format!("{}: {}", short_key, display_value));
        }
    }

    // Priority 2: First non-parent ResourceRef attribute (direct or inside a List)
    for key in &keys {
        match resource.get_attr(key) {
            Some(Value::ResourceRef { path }) => {
                if parent_binding == Some(path.binding()) {
                    continue;
                }
                let short_key = shorten_attr_name(key);
                return Some(format!("{}: {}", short_key, path.binding()));
            }
            Some(Value::List(items)) => {
                for item in items {
                    if let Value::ResourceRef { path } = item {
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
