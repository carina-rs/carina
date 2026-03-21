//! Dependency graph utilities for resource ordering and failure propagation

use std::collections::{HashMap, HashSet};

use crate::effect::Effect;
use crate::resource::{Resource, Value};

/// Extract binding names that a resource depends on
pub fn get_resource_dependencies(resource: &Resource) -> HashSet<String> {
    let mut deps = HashSet::new();
    for value in resource.attributes.values() {
        collect_dependencies(value, &mut deps);
    }
    deps
}

/// Recursively collect resource reference dependencies from a value
fn collect_dependencies(value: &Value, deps: &mut HashSet<String>) {
    match value {
        Value::ResourceRef { binding_name, .. } => {
            deps.insert(binding_name.clone());
        }
        Value::List(items) => {
            for item in items {
                collect_dependencies(item, deps);
            }
        }
        Value::Map(map) => {
            for v in map.values() {
                collect_dependencies(v, deps);
            }
        }
        _ => {}
    }
}

/// Sort resources topologically based on dependencies.
///
/// Returns an error if a circular dependency is detected, with a message
/// showing the cycle path (e.g., "Circular dependency detected: a -> b -> c -> a").
pub fn sort_resources_by_dependencies(resources: &[Resource]) -> Result<Vec<Resource>, String> {
    // Build binding name to resource mapping
    let mut binding_to_resource: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(Value::String(binding_name)) = resource.attributes.get("_binding") {
            binding_to_resource.insert(binding_name.clone(), resource);
        }
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
            .attributes
            .get("_binding")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name));

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

    for resource in resources {
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
            .attributes
            .get("_binding")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| format!("{}:{}", resource.id.resource_type, resource.id.name));

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
    let resource = effect.resource()?;
    let deps = get_resource_dependencies(resource);
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
    use crate::resource::{LifecycleConfig, Resource, ResourceId, Value};

    fn make_resource(binding: &str, deps: &[&str]) -> Resource {
        let mut r = Resource::new("test", binding);
        r.attributes
            .insert("_binding".to_string(), Value::String(binding.to_string()));
        for dep in deps {
            r.attributes.insert(
                format!("ref_{}", dep),
                Value::ResourceRef {
                    binding_name: dep.to_string(),
                    attribute_name: "id".to_string(),
                },
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
        let binding_order: Vec<_> = sorted
            .iter()
            .filter_map(|r| match r.attributes.get("_binding") {
                Some(Value::String(s)) => Some(s.as_str()),
                _ => None,
            })
            .collect();
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
        let effect = Effect::Create(resource);

        let mut failed = HashSet::new();
        failed.insert("a".to_string());

        let result = find_failed_dependency(&effect, &failed);
        assert_eq!(result, Some("a".to_string()));
    }

    #[test]
    fn test_find_failed_dependency_none() {
        let resource = make_resource("b", &["a"]);
        let effect = Effect::Create(resource);

        let failed: HashSet<String> = HashSet::new();

        let result = find_failed_dependency(&effect, &failed);
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_failed_dependency_no_deps() {
        let resource = make_resource("a", &[]);
        let effect = Effect::Create(resource);

        let mut failed = HashSet::new();
        failed.insert("x".to_string());

        let result = find_failed_dependency(&effect, &failed);
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_failed_dependency_transitive_propagation() {
        let resource_c = make_resource("c", &["b"]);
        let effect_c = Effect::Create(resource_c);

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
            lifecycle: LifecycleConfig::default(),
            binding: None,
            dependencies: HashSet::new(),
        };

        let mut failed = HashSet::new();
        failed.insert("some_binding".to_string());

        let result = find_failed_dependency(&effect, &failed);
        assert_eq!(result, None);
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
        // Traversal: a -> c -> b -> a (cycle)
        let a = make_resource("a", &["c"]);
        let b = make_resource("b", &["a"]);
        let c = make_resource("c", &["b"]);
        let result = sort_resources_by_dependencies(&[a, b, c]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err, "Circular dependency detected: a -> c -> b -> a");
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
}
