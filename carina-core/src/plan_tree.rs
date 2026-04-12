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
/// The tree shows dependencies as children: a resource's children are the
/// resources it depends on (which must be created first). Roots are resources
/// that no other resource depends on (the "outermost" consumers).
///
/// Each resource is assigned to exactly one parent (or is a root). When
/// multiple resources depend on the same resource, it is placed under the
/// shallowest (most ancestral) dependent. Ties are broken by
/// (resource_type, binding_name) for determinism.
///
/// Returns (roots, children) where roots are sorted and each parent's
/// children are sorted by (resource_type, binding_name).
pub fn build_single_parent_tree(
    plan: &Plan,
    graph: &DependencyGraph,
) -> (Vec<usize>, HashMap<usize, Vec<usize>>) {
    let binding_to_effect = &graph.binding_to_effect;
    let effect_deps = &graph.effect_deps;
    let effect_bindings = &graph.effect_bindings;
    let effect_types = &graph.effect_types;

    // Step 1: Build forward and reverse dependency maps by resolving binding
    // names to effect indices in a single pass.
    let mut all_dependencies: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut all_dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for (idx, deps) in effect_deps {
        for dep in deps {
            if let Some(&dep_idx) = binding_to_effect.get(dep) {
                all_dependencies.entry(*idx).or_default().push(dep_idx);
                all_dependents.entry(dep_idx).or_default().push(*idx);
            }
        }
    }

    // Step 2: Identify roots — effects that no other effect depends on
    let mut roots: Vec<usize> = Vec::new();
    for idx in 0..plan.effects().len() {
        if all_dependents.get(&idx).is_none_or(|v| v.is_empty()) {
            roots.push(idx);
        }
    }

    let sort_key = |idx: &usize| -> (String, String) {
        let rtype = effect_types.get(idx).cloned().unwrap_or_default();
        let binding = effect_bindings.get(idx).cloned().unwrap_or_default();
        (rtype, binding)
    };

    // Step 4: Compute depth via BFS from roots through dependencies
    let mut depth: HashMap<usize, usize> = HashMap::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    for &root in &roots {
        depth.insert(root, 0);
        queue.push_back(root);
    }
    while let Some(node) = queue.pop_front() {
        let d = depth[&node];
        if let Some(deps) = all_dependencies.get(&node) {
            for &dep in deps {
                if let std::collections::hash_map::Entry::Vacant(e) = depth.entry(dep) {
                    e.insert(d + 1);
                    queue.push_back(dep);
                }
            }
        }
    }

    // Step 5: For each non-root effect, select a single parent:
    // the shallowest effect that depends on it (from all_dependents).
    let mut children_map: HashMap<usize, Vec<usize>> = HashMap::new();
    for idx in 0..plan.effects().len() {
        if roots.contains(&idx) {
            continue;
        }
        let Some(dependents) = all_dependents.get(&idx) else {
            continue;
        };
        let parent = dependents.iter().copied().min_by(|a, b| {
            let da = depth.get(a).copied().unwrap_or(usize::MAX);
            let db = depth.get(b).copied().unwrap_or(usize::MAX);
            da.cmp(&db).then_with(|| sort_key(a).cmp(&sort_key(b)))
        });
        if let Some(parent) = parent {
            children_map.entry(parent).or_default().push(idx);
        }
    }

    // Step 6: Sort each parent's children by (resource_type, binding_name)
    for children in children_map.values_mut() {
        children.sort_by_key(|a| sort_key(a));
    }

    // Also sort roots by (resource_type, binding_name)
    roots.sort_by_key(|a| sort_key(a));

    (roots, children_map)
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
            // Strip namespace from DSL enum identifiers (e.g., awscc.AvailabilityZone.ap_northeast_1a -> "ap_northeast_1a")
            let resolved = if is_dsl_enum_format(s) {
                Cow::Borrowed(convert_enum_value(s))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{Resource, Value};

    /// Helper: build plan, graph, and tree, then return (roots, children_map).
    fn tree_from_plan(plan: &Plan) -> (Vec<usize>, HashMap<usize, Vec<usize>>) {
        let graph = build_dependency_graph(plan);
        build_single_parent_tree(plan, &graph)
    }

    #[test]
    fn dependencies_are_children() {
        // vpc has no deps; route_table depends on vpc; subnet depends on vpc.
        // After flip: route_table and subnet are roots (nothing depends on them),
        // vpc is a child.
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "my-vpc")
                .with_binding("vpc")
                .with_attribute("cidr_block", Value::String("10.0.0.0/16".to_string())),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.route_table", "my-rt")
                .with_binding("rt")
                .with_attribute(
                    "vpc_id",
                    Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
                ),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "my-subnet")
                .with_binding("subnet")
                .with_attribute(
                    "vpc_id",
                    Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
                ),
        ));

        let (roots, children) = tree_from_plan(&plan);

        // Roots should be route_table and subnet (nothing depends on them)
        let root_bindings: Vec<String> = roots
            .iter()
            .map(|&i| plan.effects()[i].binding_name().unwrap())
            .collect();
        assert!(root_bindings.contains(&"rt".to_string()));
        assert!(root_bindings.contains(&"subnet".to_string()));
        assert!(!root_bindings.contains(&"vpc".to_string()));

        // vpc should be a child of one of the roots
        let vpc_idx = plan
            .effects()
            .iter()
            .position(|e| e.binding_name() == Some("vpc".to_string()))
            .unwrap();
        let vpc_is_child_of_a_root = roots
            .iter()
            .any(|&r| children.get(&r).is_some_and(|c| c.contains(&vpc_idx)));
        assert!(vpc_is_child_of_a_root);
    }

    #[test]
    fn chain_dependency_becomes_nested_children() {
        // A depends on B, B depends on C.
        // After flip: A is root, B is child of A, C is child of B.
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("ec2.vpc", "c").with_binding("c"),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.subnet", "b")
                .with_binding("b")
                .with_attribute(
                    "ref_c",
                    Value::resource_ref("c".to_string(), "id".to_string(), vec![]),
                ),
        ));
        plan.add(Effect::Create(
            Resource::new("ec2.instance", "a")
                .with_binding("a")
                .with_attribute(
                    "ref_b",
                    Value::resource_ref("b".to_string(), "id".to_string(), vec![]),
                ),
        ));

        let (roots, children) = tree_from_plan(&plan);

        // A is the only root (nothing depends on A)
        assert_eq!(roots.len(), 1);
        let a_idx = roots[0];
        assert_eq!(plan.effects()[a_idx].binding_name().unwrap(), "a");

        // B is child of A
        let a_children = children.get(&a_idx).unwrap();
        assert_eq!(a_children.len(), 1);
        let b_idx = a_children[0];
        assert_eq!(plan.effects()[b_idx].binding_name().unwrap(), "b");

        // C is child of B
        let b_children = children.get(&b_idx).unwrap();
        assert_eq!(b_children.len(), 1);
        let c_idx = b_children[0];
        assert_eq!(plan.effects()[c_idx].binding_name().unwrap(), "c");
    }

    #[test]
    fn no_deps_resources_are_roots() {
        // Two independent resources with no deps → both are roots.
        let mut plan = Plan::new();
        plan.add(Effect::Create(
            Resource::new("s3.bucket", "a").with_binding("a"),
        ));
        plan.add(Effect::Create(
            Resource::new("s3.bucket", "b").with_binding("b"),
        ));

        let (roots, _children) = tree_from_plan(&plan);
        assert_eq!(roots.len(), 2);
    }
}
