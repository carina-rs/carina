//! Dependency graph utilities for resource ordering and failure propagation

use std::collections::{HashMap, HashSet};

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
    topological_sort(resources)
}

/// Internal topological sort for creation ordering.
fn topological_sort(resources: &[Resource]) -> Result<Vec<Resource>, String> {
    // Build binding name to resource mapping
    let mut binding_to_resource: HashMap<String, &Resource> = HashMap::new();
    for resource in resources {
        if let Some(ref binding_name) = resource.binding {
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
        let binding_name = resource.binding.clone().unwrap_or_else(|| {
            format!(
                "{}:{}",
                resource.id.resource_type,
                resource.id.identity_or_empty()
            )
        });

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::Effect;
    use crate::resource::{DeferredValue, Resource, ResourceId, Value};
    use crate::wait::predicate::{AttrPath, WaitPredicate};

    fn wait_effect_with_explicit_dependency(binding: Option<&str>) -> Effect {
        Effect::Wait {
            binding: "cert_issued".to_string(),
            target_id: ResourceId::with_identity("acm.Certificate", "cert"),
            until: WaitPredicate::Equals {
                attr: AttrPath::single("status"),
                value: Value::Concrete(crate::resource::ConcreteValue::String(
                    "ISSUED".to_string(),
                )),
            },
            until_surface: "cert.status == ISSUED".to_string(),
            timeout: std::time::Duration::from_secs(60),
            interval: std::time::Duration::from_millis(1),
            explicit_dependencies: binding.into_iter().map(String::from).collect(),
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
            id: ResourceId::with_identity("_virtual.module", "v"),
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
    fn wait_blocking_bindings_include_target_before_explicit_dependencies() {
        let effect = wait_effect_with_explicit_dependency(Some("other_failed"));
        let blocking_bindings = effect.blocking_bindings();

        assert_eq!(blocking_bindings.first(), Some(&"cert".to_string()));
        assert_eq!(
            blocking_bindings.into_iter().collect::<HashSet<_>>(),
            HashSet::from(["cert".to_string(), "other_failed".to_string()])
        );
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
                r.binding.clone().unwrap_or_else(|| {
                    format!("{}:{}", r.id.resource_type, r.id.identity_or_empty())
                })
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
                r.binding.clone().unwrap_or_else(|| {
                    format!("{}:{}", r.id.resource_type, r.id.identity_or_empty())
                })
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
