//! Dependency graph utilities for resource ordering and failure propagation

use std::collections::{HashMap, HashSet};

use crate::effect::Effect;
use crate::resource::{Resource, Value};

/// Extract binding names that a resource depends on.
///
/// Collects dependencies from two sources and merges them:
/// 1. `ResourceRef` values found in attributes
/// 2. `_dependency_bindings` metadata (saved by `resolve_refs_with_state`
///    before ResourceRef values were resolved to plain strings)
///
/// Both sources are always merged because partial resolution can cause
/// ResourceRef bindings to differ from the original direct dependencies.
pub fn get_resource_dependencies(resource: &Resource) -> HashSet<String> {
    let mut deps = HashSet::new();
    for value in resource.attributes.values() {
        collect_dependencies(value, &mut deps);
    }
    // Always merge pre-computed dependency bindings.
    // After resolve_refs_with_state(), some ResourceRef values may have been
    // partially resolved: e.g., `tgw_attach.transit_gateway_id` becomes
    // `ResourceRef { binding: "tgw", attr: "id" }` because tgw_attach's
    // transit_gateway_id is itself `tgw.id`. In this case, collect_dependencies
    // finds "tgw" but misses "tgw_attach". The _dependency_bindings metadata
    // (saved before resolution) correctly records the original direct dependency.
    for name in &resource.dependency_bindings {
        deps.insert(name.clone());
    }
    for name in &resource.directives.depends_on {
        deps.insert(name.clone());
    }
    deps
}

/// Dependency-binding collection for a [`Composition`](crate::resource::Composition).
///
/// `Composition` has no `directives` field — synthetic IR nodes
/// never carry `depends_on`. The collection therefore reduces to
/// "attributes' ResourceRefs ∪ `dependency_bindings`", which is the
/// same first-two-thirds of [`get_resource_dependencies`].
pub fn get_composition_dependencies(virt: &crate::resource::Composition) -> HashSet<String> {
    let mut deps = HashSet::new();
    for attr in virt.signature.attributes.values() {
        // The Forwarded/Derived split (#3294) is a typed
        // classification; dependency collection still walks the
        // underlying `Value` shape, so reify and recurse.
        collect_dependencies(&attr.to_value(), &mut deps);
    }
    for name in &virt.dependency_bindings {
        deps.insert(name.clone());
    }
    deps
}

/// Dependency-binding collection for a [`DataSource`](crate::resource::DataSource).
///
/// A `DataSource` carries `directives` (so `depends_on` applies), but
/// no `prefixes`. The collection is "attributes' `ResourceRef`s ∪
/// `dependency_bindings` ∪ `directives.depends_on`" — structurally the
/// same union [`get_resource_dependencies`] computes for a managed
/// resource.
pub fn get_data_source_dependencies(data_source: &crate::resource::DataSource) -> HashSet<String> {
    let mut deps = HashSet::new();
    for value in data_source.attributes.values() {
        collect_dependencies(value, &mut deps);
    }
    for name in &data_source.dependency_bindings {
        deps.insert(name.clone());
    }
    for name in &data_source.directives.depends_on {
        deps.insert(name.clone());
    }
    deps
}

/// Recursively collect resource reference dependencies from a value.
///
/// Both attribute references (`vpc.vpc_id`) and bare-binding refs
/// (`vpc_id` standing alone) record the same dependency edge: the
/// resource depends on the named binding. The two parse to different
/// `Value` variants since #2847 (`ResourceRef` vs. `BindingRef`), so
/// the walker visits both forms here.
pub(crate) fn collect_dependencies(value: &Value, deps: &mut HashSet<String>) {
    value.visit_resource_refs(&mut |path| {
        deps.insert(path.binding().to_string());
    });
    value.visit_binding_refs(&mut |binding| {
        deps.insert(binding.to_string());
    });
}

/// Like [`get_resource_dependencies`], but excludes `directives.depends_on`.
///
/// The parser/resolver snapshots this into `Resource.dependency_bindings`
/// before reference resolution. Keeping the depends_on edges out of that
/// snapshot is what lets the validation pass tell a redundant edge apart
/// from a depends_on-only edge.
///
/// Takes a [`ResourceRef`](crate::parser::ResourceRef) so the resolver
/// can compute the snapshot for any node kind ([`Resource`],
/// [`DataSource`], [`Composition`](crate::resource::Composition))
/// uniformly (carina#3181 / #3308).
pub fn get_resource_value_ref_dependencies(
    resource: crate::parser::ResourceRef<'_>,
) -> HashSet<String> {
    let mut deps = HashSet::new();
    let attrs = resource.attributes();
    for value in attrs.values() {
        collect_dependencies(value, &mut deps);
    }
    for name in resource.dependency_bindings() {
        deps.insert(name.clone());
    }
    deps
}

/// Sort resources topologically based on dependencies.
///
/// Resources are ordered so that dependencies come before dependents (creation order).
/// The DFS traversal respects the input order for resources at the same level,
/// preserving the declaration order from the .crn file.
///
/// Returns an error if a circular dependency is detected, with a message
/// showing the cycle path (e.g., "Circular dependency detected: a -> b -> c -> a").
pub fn sort_resources_by_dependencies(resources: &[Resource]) -> Result<Vec<Resource>, String> {
    topological_sort(resources, false)
}

/// Sort resources for destroy ordering.
///
/// Like `sort_resources_by_dependencies`, but pre-sorts resources by dependency
/// depth (ascending) before DFS traversal, then reverses the result. This ensures
/// that shallower independent resources (like an internet gateway at depth 1)
/// appear late in destroy order -- after deeper chains have been deleted.
///
/// The depth-based pre-sorting is only needed for destroy ordering. For apply
/// (creation) ordering, the plain topological sort preserves the declaration
/// order from the .crn file, which is the expected behavior (#1071).
pub fn sort_resources_for_destroy(resources: &[Resource]) -> Result<Vec<Resource>, String> {
    let sorted = topological_sort(resources, true)?;
    Ok(sorted.into_iter().rev().collect())
}

/// Internal topological sort with optional depth-based pre-sorting.
///
/// When `depth_presort` is true, resources are pre-sorted by dependency depth
/// (ascending) before DFS traversal. This makes the sort input-order-independent,
/// ensuring stable results for independent branches.
fn topological_sort(resources: &[Resource], depth_presort: bool) -> Result<Vec<Resource>, String> {
    // Build binding name to resource mapping
    let mut binding_to_resource: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(ref binding_name) = resource.binding {
            binding_to_resource.insert(binding_name.clone(), resource);
        }
    }

    let mut presorted: Vec<&Resource> = resources.iter().collect();

    if depth_presort {
        // Compute the dependency depth for each resource: the length of the longest
        // chain from a root (resource with no dependencies) to this resource.
        let mut depth_cache: HashMap<String, usize> = HashMap::new();
        fn compute_depth(
            binding: &str,
            binding_to_resource: &HashMap<String, &Resource>,
            cache: &mut HashMap<String, usize>,
            visiting: &mut HashSet<String>,
        ) -> usize {
            if let Some(&cached) = cache.get(binding) {
                return cached;
            }
            // Guard against circular dependencies
            if visiting.contains(binding) {
                return 0;
            }
            visiting.insert(binding.to_string());
            let depth = if let Some(resource) = binding_to_resource.get(binding) {
                let deps = get_resource_dependencies(resource);
                deps.iter()
                    .map(|d| 1 + compute_depth(d, binding_to_resource, cache, visiting))
                    .max()
                    .unwrap_or(0)
            } else {
                0
            };
            visiting.remove(binding);
            cache.insert(binding.to_string(), depth);
            depth
        }

        let mut depth_visiting: HashSet<String> = HashSet::new();
        for resource in resources {
            let binding = resource.binding.clone().unwrap_or_else(|| {
                format!("{}:{}", resource.id.resource_type, resource.id.name_str())
            });
            compute_depth(
                &binding,
                &binding_to_resource,
                &mut depth_cache,
                &mut depth_visiting,
            );
        }

        // Pre-sort resources by dependency depth (ascending) so DFS visits
        // shallower resources first. This ensures that "leaf" resources like
        // an internet gateway (depth 1, no dependents) are emitted early in
        // creation order, placing them late in destroy order -- after deeper
        // chains (like nat_gw -> route) have been destroyed.
        presorted.sort_by(|a, b| {
            let a_binding = a
                .binding
                .clone()
                .unwrap_or_else(|| format!("{}:{}", a.id.resource_type, a.id.name_str()));
            let b_binding = b
                .binding
                .clone()
                .unwrap_or_else(|| format!("{}:{}", b.id.resource_type, b.id.name_str()));
            let a_depth = depth_cache.get(&a_binding).copied().unwrap_or(0);
            let b_depth = depth_cache.get(&b_binding).copied().unwrap_or(0);
            a_depth.cmp(&b_depth) // Ascending: shallower first
        });
    }

    // Build dependency graph
    let mut sorted = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut visiting: Vec<String> = Vec::new();

    fn visit<'a>(
        resource: &'a Resource,
        binding_to_resource: &HashMap<String, &'a Resource>,
        visited: &mut HashSet<String>,
        visiting: &mut Vec<String>,
        sorted: &mut Vec<Resource>,
    ) -> Result<(), String> {
        let binding_name = resource
            .binding
            .clone()
            .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name_str()));

        if visited.contains(&binding_name) {
            return Ok(());
        }
        if let Some(pos) = visiting.iter().position(|n| n == &binding_name) {
            let cycle: Vec<&str> = visiting[pos..]
                .iter()
                .map(|s| s.as_str())
                .chain(std::iter::once(binding_name.as_str()))
                .collect();
            return Err(format!(
                "Circular dependency detected: {}",
                cycle.join(" -> ")
            ));
        }

        visiting.push(binding_name.clone());

        // Visit dependencies first
        let deps = get_resource_dependencies(resource);
        for dep in &deps {
            if let Some(dep_resource) = binding_to_resource.get(dep) {
                visit(dep_resource, binding_to_resource, visited, visiting, sorted)?;
            }
        }

        visiting.pop();
        visited.insert(binding_name);
        sorted.push(resource.clone());
        Ok(())
    }

    for resource in &presorted {
        visit(
            resource,
            &binding_to_resource,
            &mut visited,
            &mut visiting,
            &mut sorted,
        )?;
    }

    Ok(sorted)
}

/// Build a reverse dependency map: for each binding, which bindings depend on it.
/// If resource A depends on resource B, then `dependents_map["b"]` contains "a".
pub fn build_dependents_map(resources: &[&Resource]) -> HashMap<String, HashSet<String>> {
    let mut dependents_map: HashMap<String, HashSet<String>> = HashMap::new();
    for resource in resources {
        let binding = resource
            .binding
            .clone()
            .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name_str()));

        let deps = get_resource_dependencies(resource);
        for dep in deps {
            dependents_map
                .entry(dep)
                .or_default()
                .insert(binding.clone());
        }
    }
    dependents_map
}

/// Check if an effect has any dependency on failed bindings.
/// Returns the name of the first failed dependency found, or None.
pub fn find_failed_dependency(
    effect: &Effect,
    failed_bindings: &HashSet<String>,
) -> Option<String> {
    if let Effect::Wait {
        explicit_dependencies,
        ..
    } = effect
    {
        return explicit_dependencies
            .iter()
            .find(|dep| failed_bindings.contains(*dep))
            .cloned();
    }

    // carina#3181 PR D: `Effect` payloads are typestate structs, so the
    // dependency set is assembled from the `ResourceLike` view (value
    // refs + `dependency_bindings`) plus the effect's explicit
    // `depends_on` edges — the same union `get_resource_dependencies`
    // computes for a legacy `Resource`.
    let resource = effect.as_resource_ref()?;
    let mut deps = get_resource_value_ref_dependencies(resource);
    deps.extend(effect.explicit_dependencies());
    deps.into_iter().find(|dep| failed_bindings.contains(dep))
}

/// Check if any dependent of the given binding has failed (is in failed_bindings).
/// Returns the first failed dependent found, if any.
pub fn find_failed_dependent<'a>(
    binding: &str,
    dependents_map: &'a HashMap<String, HashSet<String>>,
    failed_bindings: &'a HashSet<String>,
) -> Option<&'a String> {
    // Check direct dependents
    if let Some(dependents) = dependents_map.get(binding) {
        for dep in dependents {
            if failed_bindings.contains(dep) {
                return Some(dep);
            }
            // Check transitive: if a dependent of this binding has a dependent that failed
            if let Some(failed) = find_failed_dependent(dep, dependents_map, failed_bindings) {
                return Some(failed);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::WaitTarget;
    use crate::resource::{DeferredValue, Directives, Resource, ResourceId, Value};
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    fn wait_effect_with_explicit_dependency(binding: &str) -> Effect {
        Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::new("acm.Certificate", "cert"),
            target: WaitTarget::Known("cert-id".to_string()),
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(crate::resource::ConcreteValue::String(
                    "ISSUED".to_string(),
                )),
            },
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: std::time::Duration::from_secs(60),
            interval: std::time::Duration::from_millis(1),
            explicit_dependencies: HashSet::from([binding.to_string()]),
        }
    }

    fn make_resource(binding: &str, deps: &[&str]) -> Resource {
        let mut r = Resource::new("test", binding);
        r.binding = Some(binding.to_string());
        for dep in deps {
            r.set_attr(
                format!("ref_{}", dep),
                Value::resource_ref(dep.to_string(), "id", vec![]),
            );
        }
        r
    }

    #[test]
    fn test_get_resource_dependencies() {
        let resource = make_resource("a", &["b", "c"]);
        let deps = get_resource_dependencies(&resource);
        assert!(deps.contains("b"));
        assert!(deps.contains("c"));
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn test_get_composition_dependencies_collects_attrs_and_deps() {
        use crate::resource::{Composition, Signature};
        use indexmap::IndexMap;
        use std::collections::BTreeSet;

        // Build a composition whose attributes carry a ResourceRef and
        // whose `dependency_bindings` carries a separate entry.
        // Both must end up in the merged set.
        let mut attributes: IndexMap<String, crate::resource::CompositionAttribute> =
            IndexMap::new();
        attributes.insert(
            "role_arn".to_string(),
            crate::resource::CompositionAttribute::from_value(Value::resource_ref(
                "role".to_string(),
                "arn",
                vec![],
            )),
        );
        let mut dep_bindings = BTreeSet::new();
        dep_bindings.insert("explicit_dep".to_string());
        let virt = Composition {
            id: ResourceId::new("_virtual.module", "v"),
            signature: Signature {
                arguments: IndexMap::new(),
                attributes,
            },
            binding: Some("v".to_string()),
            dependency_bindings: dep_bindings,
            module_name: "m".to_string(),
            instance: "v".to_string(),
            quoted_string_attrs: Default::default(),
        };

        let deps = get_composition_dependencies(&virt);
        assert!(
            deps.contains("role"),
            "expected attribute ResourceRef binding `role`, got {deps:?}",
        );
        assert!(
            deps.contains("explicit_dep"),
            "expected pre-recorded dependency `explicit_dep`, got {deps:?}",
        );
        assert_eq!(deps.len(), 2);
    }

    #[test]
    fn collect_dependencies_recurses_into_secret_inner_value() {
        // `Deferred(Secret(_))` is reachable after resolution wraps a value
        // (for example in parser/resolve.rs), so pin this walker arm directly.
        let value = Value::Deferred(DeferredValue::Secret(Box::new(Value::resource_ref(
            "role",
            "arn",
            vec![],
        ))));
        let mut deps = HashSet::new();

        collect_dependencies(&value, &mut deps);

        assert_eq!(deps, HashSet::from(["role".to_string()]));
    }

    // Closure-in-attribute regression test deleted: `Value::Closure` no
    // longer exists, so a closure can never reach `Resource.attributes`.
    // The original concern (refs inside captured args getting silently
    // dropped from dependency walks) is now type-impossible.

    /// Regression test for #1078: when resolve_refs_with_state partially resolves
    /// a transitive reference (e.g., `tgw_attach.transit_gateway_id` resolves to
    /// `ResourceRef { binding: "tgw", attr: "id" }` because tgw_attach's
    /// transit_gateway_id is itself `tgw.id`), the resolved resource has a
    /// ResourceRef pointing to "tgw" instead of "tgw_attach".
    ///
    /// `_dependency_bindings` correctly records the original dependency ("tgw_attach"),
    /// but the fallback only triggers when deps is empty. Since the ResourceRef to
    /// "tgw" makes deps non-empty, the fallback is skipped, and "tgw_attach" is lost.
    #[test]
    fn test_dependency_bindings_merged_when_resourceref_partially_resolved() {
        let mut resource = Resource::new("ec2.route", "my-route");
        // After partial resolution: transit_gateway_id resolved to a ResourceRef
        // pointing at "tgw" (the transitive target), not "tgw_attach" (the direct dep)
        resource.set_attr(
            "transit_gateway_id".to_string(),
            Value::resource_ref("tgw", "id", vec![]),
        );
        // route_table_id resolved to a ResourceRef pointing at "rt"
        resource.set_attr(
            "route_table_id".to_string(),
            Value::resource_ref("rt", "route_table_id", vec![]),
        );
        // dependency_bindings was saved before resolution with the CORRECT deps
        resource.dependency_bindings =
            std::collections::BTreeSet::from(["rt".to_string(), "tgw_attach".to_string()]);

        let deps = get_resource_dependencies(&resource);
        // Must include "tgw_attach" from dependency_bindings
        assert!(
            deps.contains("tgw_attach"),
            "Expected deps to contain 'tgw_attach' but got: {:?}",
            deps
        );
        // Must also include "rt" and "tgw"
        assert!(deps.contains("rt"));
        assert!(deps.contains("tgw"));
    }

    #[test]
    fn test_get_resource_dependencies_no_deps() {
        let resource = make_resource("a", &[]);
        let deps = get_resource_dependencies(&resource);
        assert!(deps.is_empty());
    }

    #[test]
    fn test_sort_resources_by_dependencies() {
        // b depends on a
        let a = make_resource("a", &[]);
        let b = make_resource("b", &["a"]);

        // Even if b comes first in the input, a should come first in the output
        let sorted = sort_resources_by_dependencies(&[b, a]).unwrap();
        let binding_order: Vec<_> = sorted.iter().filter_map(|r| r.binding.as_deref()).collect();
        assert_eq!(binding_order, vec!["a", "b"]);
    }

    #[test]
    fn test_build_dependents_map() {
        // A depends on B
        let a = make_resource("a", &["b"]);
        let b = make_resource("b", &[]);
        let resources: Vec<&Resource> = vec![&a, &b];

        let map = build_dependents_map(&resources);

        // b's dependents should contain "a"
        assert!(map.get("b").unwrap().contains("a"));
        // a should have no dependents
        assert!(!map.contains_key("a"));
    }

    #[test]
    fn test_find_failed_dependency_direct() {
        let resource = make_resource("b", &["a"]);
        let effect = Effect::Create(resource.clone());

        let mut failed = HashSet::new();
        failed.insert("a".to_string());

        let result = find_failed_dependency(&effect, &failed);
        assert_eq!(result, Some("a".to_string()));
    }

    #[test]
    fn test_find_failed_dependency_none() {
        let resource = make_resource("b", &["a"]);
        let effect = Effect::Create(resource.clone());

        let failed: HashSet<String> = HashSet::new();

        let result = find_failed_dependency(&effect, &failed);
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_failed_dependency_no_deps() {
        let resource = make_resource("a", &[]);
        let effect = Effect::Create(resource.clone());

        let mut failed = HashSet::new();
        failed.insert("x".to_string());

        let result = find_failed_dependency(&effect, &failed);
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_failed_dependency_transitive_propagation() {
        let resource_c = make_resource("c", &["b"]);
        let effect_c = Effect::Create(resource_c.clone());

        let mut failed = HashSet::new();
        failed.insert("a".to_string());
        failed.insert("b".to_string());

        let result = find_failed_dependency(&effect_c, &failed);
        assert_eq!(result, Some("b".to_string()));
    }

    #[test]
    fn test_find_failed_dependency_delete_effect() {
        let effect = Effect::Delete {
            id: ResourceId::new("test", "a"),
            identifier: "id-123".to_string(),
            directives: Directives::default(),
            binding: None,
            dependencies: HashSet::new(),
            explicit_dependencies: std::collections::HashSet::new(),
        };

        let mut failed = HashSet::new();
        failed.insert("some_binding".to_string());

        let result = find_failed_dependency(&effect, &failed);
        assert_eq!(result, None);
    }

    #[test]
    fn find_failed_dependency_returns_wait_explicit_dependency_when_failed() {
        let effect = wait_effect_with_explicit_dependency("failed_binding");
        let failed_bindings: HashSet<String> =
            ["failed_binding"].into_iter().map(String::from).collect();

        assert_eq!(
            find_failed_dependency(&effect, &failed_bindings),
            Some("failed_binding".to_string())
        );
    }

    #[test]
    fn find_failed_dependency_returns_none_when_wait_explicit_dependency_did_not_fail() {
        let effect = wait_effect_with_explicit_dependency("failed_binding");
        let failed_bindings: HashSet<String> = HashSet::new();

        assert_eq!(find_failed_dependency(&effect, &failed_bindings), None);
    }

    #[test]
    fn test_find_failed_dependent() {
        let mut dependents_map: HashMap<String, HashSet<String>> = HashMap::new();
        dependents_map
            .entry("b".to_string())
            .or_default()
            .insert("a".to_string());

        let mut failed_bindings = HashSet::new();
        failed_bindings.insert("a".to_string());

        let result = find_failed_dependent("b", &dependents_map, &failed_bindings);
        assert_eq!(result, Some(&"a".to_string()));
    }

    #[test]
    fn test_find_failed_dependent_none() {
        let mut dependents_map: HashMap<String, HashSet<String>> = HashMap::new();
        dependents_map
            .entry("b".to_string())
            .or_default()
            .insert("a".to_string());

        let failed_bindings: HashSet<String> = HashSet::new();

        let result = find_failed_dependent("b", &dependents_map, &failed_bindings);
        assert_eq!(result, None);
    }

    #[test]
    fn test_sort_resources_direct_circular_dependency() {
        // A depends on itself
        let a = make_resource("a", &["a"]);
        let result = sort_resources_by_dependencies(&[a]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err, "Circular dependency detected: a -> a");
    }

    #[test]
    fn test_sort_resources_transitive_circular_dependency() {
        // A depends on C, B depends on A, C depends on B
        let a = make_resource("a", &["c"]);
        let b = make_resource("b", &["a"]);
        let c = make_resource("c", &["b"]);
        let result = sort_resources_by_dependencies(&[a, b, c]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.starts_with("Circular dependency detected:"),
            "Expected circular dependency error, got: {}",
            err
        );
    }

    /// Reproduces the destroy ordering issue from #1067:
    /// When IGW and NAT gateway both depend on VPC but are independent of each other,
    /// the destroy order must delete NAT gateway (and its route) before IGW.
    ///
    /// Dependency graph (from nat_gateway.crn):
    ///   vpc (root)
    ///   igw -> vpc
    ///   subnet -> vpc
    ///   eip (no deps)
    ///   nat_gw -> eip, subnet
    ///   rt -> vpc
    ///   route -> rt, nat_gw
    ///
    /// In destroy order (reversed creation), IGW must come after route and nat_gw
    /// because AWS requires the NAT gateway to be deleted before the IGW can be detached.
    #[test]
    fn test_destroy_order_igw_after_nat_gateway() {
        let vpc = make_resource("vpc", &[]);
        let igw = make_resource("igw", &["vpc"]);
        let subnet = make_resource("subnet", &["vpc"]);
        let eip = make_resource("eip", &[]);
        let nat_gw = make_resource("nat_gw", &["eip", "subnet"]);
        let rt = make_resource("rt", &["vpc"]);
        let route = make_resource("route", &["rt", "nat_gw"]);

        // Single sort + reverse (the correct approach).
        // The previous double-sort-reverse pattern (sort → reverse → sort → reverse)
        // could produce incorrect ordering for independent branches (#1067).
        //
        // Test with multiple input orderings to ensure the result is correct
        // regardless of declaration order in the .crn file.
        let orderings: Vec<Vec<Resource>> = vec![
            // Original .crn order
            vec![
                vpc.clone(),
                igw.clone(),
                subnet.clone(),
                eip.clone(),
                nat_gw.clone(),
                rt.clone(),
                route.clone(),
            ],
            // nat_gw before igw
            vec![
                vpc.clone(),
                subnet.clone(),
                eip.clone(),
                nat_gw.clone(),
                rt.clone(),
                route.clone(),
                igw.clone(),
            ],
            // igw last
            vec![
                eip.clone(),
                vpc.clone(),
                subnet.clone(),
                nat_gw.clone(),
                rt.clone(),
                route.clone(),
                igw.clone(),
            ],
        ];

        for (i, input) in orderings.iter().enumerate() {
            let destroy_order_resources = sort_resources_for_destroy(input).unwrap();
            let destroy_order: Vec<&str> = destroy_order_resources
                .iter()
                .filter_map(|r| r.binding.as_deref())
                .collect();

            // IGW must come after route and nat_gw in destroy order
            let igw_pos = destroy_order.iter().position(|&b| b == "igw").unwrap();
            let route_pos = destroy_order.iter().position(|&b| b == "route").unwrap();
            let nat_gw_pos = destroy_order.iter().position(|&b| b == "nat_gw").unwrap();

            assert!(
                igw_pos > route_pos,
                "Ordering {}: IGW (pos {}) must be destroyed after route (pos {}). Destroy order: {:?}",
                i,
                igw_pos,
                route_pos,
                destroy_order
            );
            assert!(
                igw_pos > nat_gw_pos,
                "Ordering {}: IGW (pos {}) must be destroyed after nat_gw (pos {}). Destroy order: {:?}",
                i,
                igw_pos,
                nat_gw_pos,
                destroy_order
            );
        }
    }

    /// Regression test: the double-sort-reverse pattern that previously
    /// caused IGW to be destroyed before NAT gateway (#1067).
    /// This test verifies that even with orphans appended after the initial sort,
    /// a single sort+reverse produces correct destroy ordering.
    #[test]
    fn test_destroy_order_with_orphans_appended() {
        let vpc = make_resource("vpc", &[]);
        let igw = make_resource("igw", &["vpc"]);
        let subnet = make_resource("subnet", &["vpc"]);
        let eip = make_resource("eip", &[]);
        let nat_gw = make_resource("nat_gw", &["eip", "subnet"]);
        let rt = make_resource("rt", &["vpc"]);
        let route = make_resource("route", &["rt", "nat_gw"]);
        // Simulate an orphan resource that depends on vpc
        let orphan = make_resource("orphan", &["vpc"]);

        // Append orphan after initial resources (simulating orphan discovery)
        let all = vec![vpc, igw, subnet, eip, nat_gw, rt, route, orphan];

        let destroy_order_resources = sort_resources_for_destroy(&all).unwrap();
        let destroy_order: Vec<&str> = destroy_order_resources
            .iter()
            .filter_map(|r| r.binding.as_deref())
            .collect();

        let igw_pos = destroy_order.iter().position(|&b| b == "igw").unwrap();
        let route_pos = destroy_order.iter().position(|&b| b == "route").unwrap();
        let nat_gw_pos = destroy_order.iter().position(|&b| b == "nat_gw").unwrap();
        let vpc_pos = destroy_order.iter().position(|&b| b == "vpc").unwrap();

        assert!(
            igw_pos > route_pos,
            "IGW must be destroyed after route. Destroy order: {:?}",
            destroy_order
        );
        assert!(
            igw_pos > nat_gw_pos,
            "IGW must be destroyed after nat_gw. Destroy order: {:?}",
            destroy_order
        );
        assert!(
            igw_pos < vpc_pos,
            "IGW must be destroyed before vpc. Destroy order: {:?}",
            destroy_order
        );
    }

    /// Regression test for #1071: depth-based pre-sorting must not change
    /// creation (apply) order for resources with explicit dependencies.
    ///
    /// Models igw.crn (parsed from DSL, including anonymous resource):
    ///   vpc (no deps)
    ///   igw (no deps)
    ///   igw_attachment -> vpc, igw
    ///   rt -> vpc
    ///   route (anonymous) -> rt, igw_attachment
    ///
    /// The route must always come AFTER igw_attachment in creation order.
    #[test]
    fn test_apply_order_route_after_gateway_attachment() {
        use crate::parser::{ProviderContext, parse};

        let input = r#"
            provider awscc {
              region = awscc.Region.ap_northeast_1
            }

            let vpc = awscc.ec2.Vpc {
              cidr_block = "10.0.0.0/16"
            }

            let igw = awscc.ec2.internet_gateway {}

            let igw_attachment = awscc.ec2.vpc_gateway_attachment {
              vpc_id              = vpc.vpc_id
              internet_gateway_id = igw.internet_gateway_id
            }

            let rt = awscc.ec2.RouteTable {
              vpc_id = vpc.vpc_id
            }

            awscc.ec2.route {
              route_table_id         = rt.route_table_id
              destination_cidr_block = "0.0.0.0/0"
              gateway_id             = igw_attachment.internet_gateway_id
            }
        "#;

        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let creation_order: Vec<String> = sorted
            .iter()
            .map(|r| {
                r.binding
                    .clone()
                    .unwrap_or_else(|| format!("{}:{}", r.id.resource_type, r.id.name_str()))
            })
            .collect();

        let route_pos = creation_order
            .iter()
            .position(|b| b.contains("route") && !b.contains("route_table"))
            .unwrap();
        let attachment_pos = creation_order
            .iter()
            .position(|b| b == "igw_attachment")
            .unwrap();
        let rt_pos = creation_order.iter().position(|b| b == "rt").unwrap();

        assert!(
            route_pos > attachment_pos,
            "route (pos {}) must come AFTER igw_attachment (pos {}) in creation order. Order: {:?}",
            route_pos,
            attachment_pos,
            creation_order
        );
        assert!(
            route_pos > rt_pos,
            "route (pos {}) must come AFTER rt (pos {}) in creation order. Order: {:?}",
            route_pos,
            rt_pos,
            creation_order
        );
    }

    /// Regression test for #1071: models transit_gateway.crn
    ///   vpc (no deps)
    ///   subnet -> vpc
    ///   tgw (no deps)
    ///   tgw_attach -> tgw, vpc, subnet
    ///   rt -> vpc
    ///   route (anonymous) -> rt, tgw_attach
    #[test]
    fn test_apply_order_route_after_tgw_attachment() {
        use crate::parser::{ProviderContext, parse};

        let input = r#"
            provider awscc {
              region = awscc.Region.ap_northeast_1
            }

            let vpc = awscc.ec2.Vpc {
              cidr_block = "10.0.0.0/16"
            }

            let subnet = awscc.ec2.Subnet {
              vpc_id            = vpc.vpc_id
              cidr_block        = "10.0.1.0/24"
              availability_zone = "ap-northeast-1a"
            }

            let tgw = awscc.ec2.transit_gateway {
              description = "Transit Gateway for route test"
            }

            let tgw_attach = awscc.ec2.transit_gateway_attachment {
              transit_gateway_id = tgw.id
              vpc_id             = vpc.vpc_id
              subnet_ids         = [subnet.subnet_id]
            }

            let rt = awscc.ec2.RouteTable {
              vpc_id = vpc.vpc_id
            }

            awscc.ec2.route {
              route_table_id         = rt.route_table_id
              destination_cidr_block = "10.1.0.0/16"
              transit_gateway_id     = tgw_attach.transit_gateway_id
            }
        "#;

        let parsed = parse(input, &ProviderContext::default()).unwrap();
        let sorted = sort_resources_by_dependencies(&parsed.resources).unwrap();
        let creation_order: Vec<String> = sorted
            .iter()
            .map(|r| {
                r.binding
                    .clone()
                    .unwrap_or_else(|| format!("{}:{}", r.id.resource_type, r.id.name_str()))
            })
            .collect();

        let route_pos = creation_order
            .iter()
            .position(|b| b.contains("route") && !b.contains("route_table"))
            .unwrap();
        let attach_pos = creation_order
            .iter()
            .position(|b| b == "tgw_attach")
            .unwrap();

        assert!(
            route_pos > attach_pos,
            "route (pos {}) must come AFTER tgw_attach (pos {}) in creation order. Order: {:?}",
            route_pos,
            attach_pos,
            creation_order
        );
    }

    #[test]
    fn test_transitive_chain() {
        let mut dependents_map: HashMap<String, HashSet<String>> = HashMap::new();
        dependents_map
            .entry("c".to_string())
            .or_default()
            .insert("b".to_string());
        dependents_map
            .entry("b".to_string())
            .or_default()
            .insert("a".to_string());

        let mut failed_bindings = HashSet::new();
        failed_bindings.insert("a".to_string());

        let result = find_failed_dependent("b", &dependents_map, &failed_bindings);
        assert_eq!(result, Some(&"a".to_string()));

        let result = find_failed_dependent("c", &dependents_map, &failed_bindings);
        assert_eq!(result, Some(&"a".to_string()));
    }

    #[test]
    fn directives_depends_on_is_unioned_into_resource_dependencies() {
        let mut bucket = Resource::new("s3.Bucket", "bucket");
        bucket.directives.depends_on = vec!["role".to_string()];

        let deps = get_resource_dependencies(&bucket);
        assert!(
            deps.contains("role"),
            "expected directives.depends_on entry to surface in get_resource_dependencies, got {:?}",
            deps
        );
    }

    #[test]
    fn directives_depends_on_unions_with_value_refs() {
        let mut bucket = Resource::new("s3.Bucket", "bucket");
        bucket.set_attr(
            "encryption_key".to_string(),
            Value::resource_ref("key", "arn", vec![]),
        );
        bucket.directives.depends_on = vec!["role".to_string()];

        let deps = get_resource_dependencies(&bucket);
        assert!(deps.contains("key"), "value-ref dep missing");
        assert!(deps.contains("role"), "directives.depends_on dep missing");
    }
}
