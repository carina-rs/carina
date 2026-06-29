use super::*;

use colored::Colorize;

use carina_core::effect::{CascadingUpdate, DeferredReplaceDelete, Effect, NonEmptyDeletes};
use carina_core::plan::Plan;
use carina_core::resource::{
    AccessPath, ConcreteValue, DeferredValue, Directives, ResolvedResource, Resource, ResourceId,
    State, UnknownReason, Value,
};

fn resolved(resource: Resource) -> ResolvedResource {
    ResolvedResource::new(resource)
}

fn make_resource(resource_type: &str, name: &str, binding: &str, deps: &[&str]) -> Resource {
    let mut r = Resource::new(resource_type, name);
    r.binding = Some(binding.to_string());
    for dep in deps {
        r.set_attr(
            format!("ref_{}", dep),
            Value::resource_ref(dep.to_string(), "id".to_string(), vec![]),
        );
    }
    r
}

/// Test that print_plan does not panic when a resource has a dependency
/// on a binding that is not present in the plan (external dependency).
/// This exercises the dependency graph construction code path where
/// `.unwrap()` could theoretically panic if `dep_idx` were invalid.
#[test]
fn test_print_plan_with_external_dependency_does_not_panic() {
    // Resource "b" depends on "a", but "a" is NOT in the plan.
    // This simulates an external/unresolved dependency.
    let b = make_resource("test.resource", "b", "b", &["a"]);
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(b)));

    // Should not panic
    print_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    );
}

/// Test that print_plan handles the dependency graph correctly when
/// dependents map is accessed with valid indices.
#[test]
fn test_print_plan_with_internal_dependency_does_not_panic() {
    let a = make_resource("test.resource", "a", "a", &[]);
    let b = make_resource("test.resource", "b", "b", &["a"]);
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(a)));
    plan.add(Effect::Create(resolved(b)));

    // Should not panic
    print_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    );
}

/// Helper: compute root indices using the same algorithm as print_plan.
fn compute_roots(plan: &Plan) -> Vec<usize> {
    let (roots, _, _, _) = build_plan_tree(plan);
    roots
}

/// Helper: build the full plan tree structure using the same algorithm as
/// print_plan. Returns (roots, dependents, effect_bindings, effect_types)
/// so tests can inspect the tree structure.
#[allow(clippy::type_complexity)]
fn build_plan_tree(
    plan: &Plan,
) -> (
    Vec<usize>,
    HashMap<usize, Vec<usize>>,
    HashMap<usize, String>,
    HashMap<usize, String>,
) {
    let graph = build_dependency_graph(plan);
    let (roots, dependents) = build_single_parent_tree(plan, &graph);
    (roots, dependents, graph.effect_bindings, graph.effect_types)
}

/// Helper: walk the tree from roots in print order and collect binding names
/// in the order they would be printed. This replicates the traversal logic
/// from print_effect_tree.
fn collect_print_order(
    roots: &[usize],
    dependents: &HashMap<usize, Vec<usize>>,
    effect_bindings: &HashMap<usize, String>,
) -> Vec<String> {
    let mut printed: HashSet<usize> = HashSet::new();
    let mut result: Vec<String> = Vec::new();

    fn walk(
        idx: usize,
        dependents: &HashMap<usize, Vec<usize>>,
        effect_bindings: &HashMap<usize, String>,
        printed: &mut HashSet<usize>,
        result: &mut Vec<String>,
    ) {
        if printed.contains(&idx) {
            return;
        }
        printed.insert(idx);
        if let Some(binding) = effect_bindings.get(&idx) {
            result.push(binding.clone());
        }
        let children = dependents.get(&idx).cloned().unwrap_or_default();
        let unprinted: Vec<_> = children
            .iter()
            .filter(|c| !printed.contains(c))
            .cloned()
            .collect();
        for child in unprinted {
            walk(child, dependents, effect_bindings, printed, result);
        }
    }

    for &root in roots {
        walk(root, dependents, effect_bindings, &mut printed, &mut result);
    }
    result
}

/// Issue #933 (part 1): Siblings under the same parent should be sorted
/// by (resource_type, binding_name) for deterministic, grouped output.
///
/// Scenario: VPC has children of different types (subnets, route tables).
/// Same-type resources should be grouped together, and within a type,
/// sorted by binding name.
#[test]
fn test_siblings_sorted_by_resource_type_and_binding() {
    // VPC is root. Under it: 2 subnets and 2 route tables, added in
    // interleaved order to expose HashMap non-determinism.
    let vpc = make_resource("ec2.Vpc", "vpc", "vpc", &[]);
    let rt_b = make_resource("ec2.RouteTable", "rt_b", "rt_b", &["vpc"]);
    let subnet_b = make_resource("ec2.Subnet", "subnet_b", "subnet_b", &["vpc"]);
    let rt_a = make_resource("ec2.RouteTable", "rt_a", "rt_a", &["vpc"]);
    let subnet_a = make_resource("ec2.Subnet", "subnet_a", "subnet_a", &["vpc"]);

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(vpc)));
    plan.add(Effect::Create(resolved(rt_b)));
    plan.add(Effect::Create(resolved(subnet_b)));
    plan.add(Effect::Create(resolved(rt_a)));
    plan.add(Effect::Create(resolved(subnet_a)));

    let (roots, dependents, effect_bindings, _effect_types) = build_plan_tree(&plan);

    assert_eq!(roots, vec![0], "VPC should be the only root");

    // Get the children of VPC (index 0)
    let vpc_children = dependents.get(&0).unwrap();
    let child_labels: Vec<(String, String)> = vpc_children
        .iter()
        .map(|&idx| {
            let binding = effect_bindings.get(&idx).unwrap().clone();
            let effect = &plan.effects()[idx];
            let rtype = match effect {
                Effect::Create(r) => r.id.resource_type.clone(),
                _ => String::new(),
            };
            (rtype, binding)
        })
        .collect();

    // Expected: sorted by (resource_type, binding_name)
    // ec2.route_table comes before ec2.subnet alphabetically
    let expected = vec![
        ("ec2.RouteTable".to_string(), "rt_a".to_string()),
        ("ec2.RouteTable".to_string(), "rt_b".to_string()),
        ("ec2.Subnet".to_string(), "subnet_a".to_string()),
        ("ec2.Subnet".to_string(), "subnet_b".to_string()),
    ];

    assert_eq!(
        child_labels, expected,
        "Siblings should be sorted by (resource_type, binding_name). \
         Got: {:?}",
        child_labels
    );
}

/// Issue #933 (part 2): When a resource depends on multiple resources in
/// the tree, it should be placed under the dependency that is closest to
/// the root (most ancestral), not an arbitrary one.
///
/// Scenario:
///   - vpc (root)
///   - sg depends on vpc
///   - vpc_endpoint depends on both vpc and sg
///
/// VPC is an ancestor of SG. The endpoint should be placed under VPC
/// (closer to root), not under SG. Currently, the endpoint is added as
/// a child of BOTH vpc and sg in the dependents map, and whichever is
/// traversed first claims it. This test verifies deterministic placement
/// under the most ancestral dependency.
#[test]
fn test_parent_selection_prefers_most_ancestral_dependency() {
    let vpc = make_resource("ec2.Vpc", "vpc", "vpc", &[]);
    let sg = make_resource("ec2.SecurityGroup", "sg", "sg", &["vpc"]);
    let endpoint = make_resource("ec2.vpc_endpoint", "endpoint", "endpoint", &["vpc", "sg"]);

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(vpc)));
    plan.add(Effect::Create(resolved(sg)));
    plan.add(Effect::Create(resolved(endpoint)));

    let (roots, dependents, effect_bindings, _) = build_plan_tree(&plan);
    let _print_order = collect_print_order(&roots, &dependents, &effect_bindings);

    assert_eq!(roots, vec![0], "VPC should be the only root");

    // The endpoint (idx 2) should be a direct child of VPC (idx 0),
    // NOT a child of SG (idx 1). VPC is the most ancestral dependency.
    //
    // Expected tree:
    //   vpc
    //   ├── sg
    //   └── endpoint
    //
    // NOT:
    //   vpc
    //   └── sg
    //       └── endpoint
    //
    // Check via print order: vpc -> sg -> endpoint (sg has no children
    // because endpoint is under vpc). If endpoint were under sg, we'd
    // get vpc -> sg -> endpoint too, but we verify via direct children.
    let vpc_children: Vec<String> = dependents
        .get(&0)
        .unwrap()
        .iter()
        .filter_map(|&idx| effect_bindings.get(&idx).cloned())
        .collect();
    assert!(
        vpc_children.contains(&"endpoint".to_string()),
        "endpoint should be a direct child of vpc (most ancestral), \
         not nested under sg. VPC children: {:?}",
        vpc_children
    );

    // sg should NOT have endpoint as a child (it should only be under vpc)
    let sg_children: Vec<String> = dependents
        .get(&1)
        .unwrap()
        .iter()
        .filter_map(|&idx| effect_bindings.get(&idx).cloned())
        .collect();
    assert!(
        !sg_children.contains(&"endpoint".to_string()),
        "endpoint should NOT be a child of sg. SG children: {:?}. \
         When a resource depends on multiple resources, it should only \
         be placed under the most ancestral one (vpc), not all of them.",
        sg_children
    );
}

/// Issue #928: A resource that has no dependencies but IS referenced by
/// other resources should NOT appear as a disconnected root-level item.
/// It should be nested under the resource that references it.
///
/// Scenario (from the issue):
///   - vpc: no deps
///   - rt: depends on vpc
///   - route: depends on rt, igw
///   - igw_attachment: depends on vpc, igw
///   - igw: no deps (but referenced by route and igw_attachment)
///
/// Current (buggy): igw appears as a separate root alongside vpc.
/// Expected: igw should be nested under igw_attachment (or route),
///           so only vpc is a root.
#[test]
fn test_referenced_resource_without_deps_should_not_be_root() {
    let vpc = make_resource("ec2.Vpc", "vpc", "vpc", &[]);
    let rt = make_resource("ec2.RouteTable", "rt", "rt", &["vpc"]);
    let igw = make_resource("ec2.internet_gateway", "igw", "igw", &[]);
    let route = make_resource("ec2.route", "route", "route", &["rt", "igw"]);
    let igw_attachment = make_resource(
        "ec2.vpc_gateway_attachment",
        "igw_attachment",
        "igw_attachment",
        &["vpc", "igw"],
    );

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(vpc)));
    plan.add(Effect::Create(resolved(rt)));
    plan.add(Effect::Create(resolved(igw)));
    plan.add(Effect::Create(resolved(route)));
    plan.add(Effect::Create(resolved(igw_attachment)));

    let roots = compute_roots(&plan);

    // IGW (index 2) should NOT be a root because it is referenced by
    // other resources in the plan (route and igw_attachment).
    // Only VPC (index 0) should be a root.
    assert_eq!(
        roots,
        vec![0],
        "Only vpc should be a root. igw (index 2) is referenced by other resources \
         and should be nested, not a disconnected root. Got roots: {:?}",
        roots
    );
}

/// Issue #933: A dependency-free resource that is referenced by multiple
/// resources should be nested under the shallowest referencing resource.
///
/// Scenario:
///   - vpc (root, depth 0)
///   - rt depends on vpc (depth 1)
///   - igw_attachment depends on vpc, igw (depth 1)
///   - route depends on rt, igw (depth 2, under rt)
///   - igw: no deps (referenced by route at depth 2, igw_attachment at depth 1)
///
/// IGW should be nested under igw_attachment (depth 1), not route (depth 2).
#[test]
fn test_dependency_free_resource_nested_under_shallowest_referencing_resource() {
    let vpc = make_resource("ec2.Vpc", "vpc", "vpc", &[]);
    let rt = make_resource("ec2.RouteTable", "rt", "rt", &["vpc"]);
    let igw = make_resource("ec2.internet_gateway", "igw", "igw", &[]);
    let route = make_resource("ec2.route", "route", "route", &["rt", "igw"]);
    let igw_attachment = make_resource(
        "ec2.vpc_gateway_attachment",
        "igw_attachment",
        "igw_attachment",
        &["vpc", "igw"],
    );

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(vpc)));
    plan.add(Effect::Create(resolved(rt)));
    plan.add(Effect::Create(resolved(igw)));
    plan.add(Effect::Create(resolved(route)));
    plan.add(Effect::Create(resolved(igw_attachment)));

    let (roots, dependents, effect_bindings, _) = build_plan_tree(&plan);

    assert_eq!(roots, vec![0], "Only vpc should be root");

    // igw_attachment is index 4, route is index 3, igw is index 2
    // igw should be a child of igw_attachment (depth 1), NOT route (depth 2)
    let igw_attachment_children: Vec<String> = dependents
        .get(&4)
        .unwrap()
        .iter()
        .filter_map(|&idx| effect_bindings.get(&idx).cloned())
        .collect();
    assert!(
        igw_attachment_children.contains(&"igw".to_string()),
        "igw should be nested under igw_attachment (shallowest referencing resource at depth 1). \
         igw_attachment children: {:?}",
        igw_attachment_children
    );

    let route_children: Vec<String> = dependents
        .get(&3)
        .unwrap()
        .iter()
        .filter_map(|&idx| effect_bindings.get(&idx).cloned())
        .collect();
    assert!(
        !route_children.contains(&"igw".to_string()),
        "igw should NOT be nested under route (deeper at depth 2). \
         route children: {:?}",
        route_children
    );
}

// --- Compact mode tests ---

/// Test that extract_compact_hint returns only the first non-parent ResourceRef hint.
#[test]
fn test_extract_compact_hint_resource_ref() {
    let mut r = Resource::new("ec2.subnet_route_table_association", "hash123");
    r.set_attr(
        "route_table_id".to_string(),
        Value::resource_ref("public_rt".to_string(), "id".to_string(), vec![]),
    );
    r.set_attr(
        "subnet_id".to_string(),
        Value::resource_ref("public_subnet_1a".to_string(), "id".to_string(), vec![]),
    );

    let hint = extract_compact_hint(carina_core::parser::ResourceRef::Resource(&r), None);
    // Should return only the first ResourceRef alphabetically, with _id suffix stripped
    assert_eq!(hint, Some("route_table: public_rt".to_string()));
}

/// Test that extract_compact_hint skips ResourceRef that matches parent binding.
#[test]
fn test_extract_compact_hint_skips_parent_ref() {
    let mut r = Resource::new("ec2.subnet_route_table_association", "hash123");
    r.set_attr(
        "route_table_id".to_string(),
        Value::resource_ref("database_rt".to_string(), "id".to_string(), vec![]),
    );
    r.set_attr(
        "subnet_id".to_string(),
        Value::resource_ref("database_subnet_1a".to_string(), "id".to_string(), vec![]),
    );

    // When parent is database_rt, should skip route_table_id and show only subnet_id
    let hint = extract_compact_hint(
        carina_core::parser::ResourceRef::Resource(&r),
        Some("database_rt"),
    );
    assert_eq!(hint, Some("subnet: database_subnet_1a".to_string()));
}

/// Test that extract_compact_hint falls back to string values when no ResourceRef.
#[test]
fn test_extract_compact_hint_string_fallback() {
    let mut r = Resource::new("ec2.route", "hash456");
    r.set_attr(
        "destination_cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("0.0.0.0/0".to_string())),
    );

    let hint = extract_compact_hint(carina_core::parser::ResourceRef::Resource(&r), None);
    assert_eq!(hint, Some("destination_cidr_block: 0.0.0.0/0".to_string()));
}

/// Test that extract_compact_hint returns None when no useful attributes.
#[test]
fn test_extract_compact_hint_none() {
    let r = Resource::new("ec2.route", "hash789");
    let hint = extract_compact_hint(carina_core::parser::ResourceRef::Resource(&r), None);
    assert_eq!(hint, None);
}

/// Test that extract_compact_hint prefers string over ResourceRef.
#[test]
fn test_extract_compact_hint_prefers_string_over_resource_ref() {
    let mut r = Resource::new("ec2.route", "hash_mixed");
    r.set_attr(
        "destination".to_string(),
        Value::Concrete(ConcreteValue::String("10.0.0.0/8".to_string())),
    );
    r.set_attr(
        "gateway_id".to_string(),
        Value::resource_ref("igw".to_string(), "id".to_string(), vec![]),
    );

    let hint = extract_compact_hint(carina_core::parser::ResourceRef::Resource(&r), None);
    // String takes priority over ResourceRef
    assert_eq!(hint, Some("destination: 10.0.0.0/8".to_string()));
}

/// Test that extract_compact_hint shortens service_name values.
#[test]
fn test_extract_compact_hint_service_name_shortening() {
    let mut r = Resource::new("ec2.vpc_endpoint", "hash_svc");
    r.set_attr(
        "service_name".to_string(),
        Value::Concrete(ConcreteValue::String(
            "com.amazonaws.ap-northeast-1.ecr.dkr".to_string(),
        )),
    );

    let hint = extract_compact_hint(carina_core::parser::ResourceRef::Resource(&r), None);
    assert_eq!(hint, Some("service: ecr.dkr".to_string()));

    // Single service component
    let mut r2 = Resource::new("ec2.vpc_endpoint", "hash_svc2");
    r2.set_attr(
        "service_name".to_string(),
        Value::Concrete(ConcreteValue::String(
            "com.amazonaws.ap-northeast-1.s3".to_string(),
        )),
    );

    let hint2 = extract_compact_hint(carina_core::parser::ResourceRef::Resource(&r2), None);
    assert_eq!(hint2, Some("service: s3".to_string()));
}

/// Test that extract_compact_hint falls back to string when all ResourceRefs match parent.
#[test]
fn test_extract_compact_hint_all_refs_match_parent() {
    let mut r = Resource::new("ec2.security_group_ingress", "hash_sg");
    r.set_attr(
        "group_id".to_string(),
        Value::resource_ref("endpoint_sg".to_string(), "id".to_string(), vec![]),
    );
    r.set_attr(
        "description".to_string(),
        Value::Concrete(ConcreteValue::String("Allow HTTPS from VPC".to_string())),
    );

    // When parent is endpoint_sg, should skip group_id and use description
    let hint = extract_compact_hint(
        carina_core::parser::ResourceRef::Resource(&r),
        Some("endpoint_sg"),
    );
    assert_eq!(hint, Some("description: Allow HTTPS from VPC".to_string()));
}

/// Test that extract_compact_hint prefers string over ResourceRef for vpc_endpoint.
#[test]
fn test_extract_compact_hint_prefers_string_for_vpc_endpoint() {
    let mut r = Resource::new("ec2.vpc_endpoint", "hash_ep");
    r.set_attr(
        "service_name".to_string(),
        Value::Concrete(ConcreteValue::String(
            "com.amazonaws.ap-northeast-1.ecr.dkr".to_string(),
        )),
    );
    r.set_attr(
        "security_group_ids".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::resource_ref(
            "endpoint_sg".to_string(),
            "group_id".to_string(),
            vec![],
        )])),
    );
    r.set_attr(
        "vpc_id".to_string(),
        Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
    );

    // String attribute (service_name) takes priority over ResourceRef
    let hint = extract_compact_hint(carina_core::parser::ResourceRef::Resource(&r), Some("vpc"));
    assert_eq!(hint, Some("service: ecr.dkr".to_string()));
}

/// Test that has_binding correctly detects bound vs anonymous resources.
#[test]
fn test_has_binding() {
    let mut bound = Resource::new("ec2.Vpc", "vpc");
    bound.binding = Some("vpc".to_string());
    assert!(has_binding(carina_core::parser::ResourceRef::Resource(
        &bound
    )));

    let anonymous = Resource::new("ec2.Vpc", "hash123");
    assert!(!has_binding(carina_core::parser::ResourceRef::Resource(
        &anonymous
    )));
}

/// Test that format_compact_name shows plain identifiers for bound resources and
/// parenthesized hints for anonymous resources.
#[test]
fn test_format_compact_name_bound_resource() {
    let mut r = Resource::new("ec2.Vpc", "vpc");
    r.binding = Some("vpc".to_string());
    // For bound resources, should show name as plain identifier (no quotes)
    let result = format_compact_name(carina_core::parser::ResourceRef::Resource(&r), "vpc", None);
    assert!(
        !result.contains('"'),
        "Bound resource name should not be quoted"
    );
    assert_eq!(result, "vpc", "Should be the plain binding name");
}

#[test]
fn test_format_compact_name_anonymous_with_hint() {
    let mut r = Resource::new("ec2.subnet_route_table_association", "hash123");
    r.set_attr(
        "subnet_id".to_string(),
        Value::resource_ref("database_subnet_1a".to_string(), "id".to_string(), vec![]),
    );
    let result = format_compact_name(
        carina_core::parser::ResourceRef::Resource(&r),
        "hash123",
        None,
    );
    assert!(
        result.contains('(') && result.contains(')'),
        "Anonymous resource should show hint in parentheses, got: {}",
        result
    );
    assert!(
        result.contains("subnet: database_subnet_1a"),
        "Should contain the ResourceRef hint, got: {}",
        result
    );
}

/// Test that print_plan with compact=true does not panic and does not
/// print attribute lines.
#[test]
fn test_print_plan_compact_does_not_panic() {
    let vpc = make_resource("ec2.Vpc", "vpc", "vpc", &[]);
    let rt = make_resource("ec2.RouteTable", "rt", "rt", &["vpc"]);
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(vpc)));
    plan.add(Effect::Create(resolved(rt)));

    // Should not panic
    print_plan(
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    );
}

/// Test compact mode skips attributes by checking that _binding attribute
/// keys are not printed (attributes are hidden in compact mode).
#[test]
fn test_print_plan_compact_with_anonymous_resources() {
    let mut anon = Resource::new("ec2.route", "hash_anon");
    anon.set_attr(
        "destination_cidr_block".to_string(),
        Value::Concrete(ConcreteValue::String("0.0.0.0/0".to_string())),
    );
    anon.set_attr(
        "route_table_id".to_string(),
        Value::resource_ref("public_rt".to_string(), "id".to_string(), vec![]),
    );

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(anon)));

    // Should not panic; anonymous resources should show hints
    print_plan(
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    );
}

/// Test that extract_compact_hint extracts ResourceRef from inside a List value.
/// e.g., security_group_ids = [endpoint_sg.group_id] should produce "security_group: endpoint_sg"
#[test]
fn test_extract_compact_hint_list_containing_resource_ref() {
    let mut r = Resource::new("ec2.vpc_endpoint", "hash_list_ref");
    r.set_attr(
        "security_group_ids".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::resource_ref(
            "endpoint_sg".to_string(),
            "group_id".to_string(),
            vec![],
        )])),
    );

    let hint = extract_compact_hint(carina_core::parser::ResourceRef::Resource(&r), None);
    assert_eq!(
        hint,
        Some("security_group: endpoint_sg".to_string()),
        "Should extract ResourceRef from inside List and strip _ids suffix"
    );
}

/// Test that extract_compact_hint skips List<ResourceRef> when ref matches parent.
#[test]
fn test_extract_compact_hint_list_ref_skips_parent() {
    let mut r = Resource::new("ec2.vpc_endpoint", "hash_list_parent");
    r.set_attr(
        "security_group_ids".to_string(),
        Value::Concrete(ConcreteValue::List(vec![Value::resource_ref(
            "endpoint_sg".to_string(),
            "group_id".to_string(),
            vec![],
        )])),
    );

    let hint = extract_compact_hint(
        carina_core::parser::ResourceRef::Resource(&r),
        Some("endpoint_sg"),
    );
    assert_eq!(
        hint, None,
        "Should skip List<ResourceRef> when ref matches parent"
    );
}

/// Test that shorten_attr_name handles _ids suffix (plural).
#[test]
fn test_shorten_attr_name_ids_suffix() {
    assert_eq!(shorten_attr_name("security_group_ids"), "security_group");
    assert_eq!(shorten_attr_name("subnet_ids"), "subnet");
    // _id still works
    assert_eq!(shorten_attr_name("subnet_id"), "subnet");
    // _name still works
    assert_eq!(shorten_attr_name("service_name"), "service");
}

/// Test that extract_compact_hint resolves DSL enum identifiers.
#[test]
fn test_extract_compact_hint_resolves_dsl_enum() {
    let mut r = Resource::new("ec2.Subnet", "hash_enum");
    r.set_attr(
        "availability_zone".to_string(),
        Value::Concrete(ConcreteValue::String(
            "awscc.AvailabilityZone.ap_northeast_1a".to_string(),
        )),
    );

    let hint = extract_compact_hint(carina_core::parser::ResourceRef::Resource(&r), None);
    // Displays DSL form (underscored) until provider alias tables include
    // to_dsl reverse mappings (see issue #1675).
    assert_eq!(
        hint,
        Some("availability_zone: ap_northeast_1a".to_string()),
        "DSL enum identifiers should be stripped of namespace prefix"
    );
}

/// Test that extract_compact_hint skips _-prefixed attributes.
#[test]
fn test_extract_compact_hint_skips_internal_attributes() {
    let mut r = Resource::new("ec2.Vpc", "hash_internal");
    r.binding = Some("vpc".to_string());
    r.set_attr(
        "_hash".to_string(),
        Value::Concrete(ConcreteValue::String("abc123".to_string())),
    );

    let hint = extract_compact_hint(carina_core::parser::ResourceRef::Resource(&r), None);
    assert_eq!(hint, None, "Internal attributes should be skipped");
}

#[test]
fn test_cascading_update_shows_attribute_diffs() {
    use std::collections::HashMap;

    // Build a Replace effect with a cascading update that changes vpc_id
    let vpc_from = State::existing(
        ResourceId::with_identity("ec2.Vpc", "vpc"),
        HashMap::from([(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        )]),
    );
    let vpc_to = Resource::new("ec2.Vpc", "vpc")
        .with_binding("vpc")
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.1.0.0/16".to_string())),
        );

    let subnet_from = State::existing(
        ResourceId::with_identity("ec2.Subnet", "subnet"),
        HashMap::from([
            (
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-old123".to_string())),
            ),
            (
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
            ),
        ]),
    );
    let subnet_to = Resource::new("ec2.Subnet", "subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        )
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
        );

    let replace_effect = Effect::Replace {
        from: Box::new(vpc_from),
        to: resolved(vpc_to),
        directives: Directives {
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: carina_core::effect::ChangedCreateOnly::new(vec![
            "cidr_block".to_string(),
        ])
        .unwrap(),
        cascading_updates: vec![CascadingUpdate {
            from: Box::new(subnet_from),
            to: resolved(subnet_to),
        }],
        temporary_name: None,
        cascade_ref_hints: vec![],
    };

    let mut plan = Plan::new();
    plan.add(replace_effect);

    // Should not panic and should display attribute diffs for cascading updates
    print_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    );
}

#[test]
fn test_format_cascading_update_attr_diff() {
    use std::collections::HashMap;

    let cascade = CascadingUpdate {
        from: Box::new(State::existing(
            ResourceId::with_identity("ec2.Subnet", "subnet"),
            HashMap::from([
                (
                    "vpc_id".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc-old123".to_string())),
                ),
                (
                    "cidr_block".to_string(),
                    Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
                ),
            ]),
        )),
        to: resolved(
            Resource::new("ec2.Subnet", "subnet")
                .with_attribute(
                    "vpc_id",
                    Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
                )
                .with_attribute(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
                ),
        ),
    };

    let output = format_cascading_update_diff(&cascade, "    ", "vpc");
    // vpc_id references the replaced binding "vpc", so it should appear
    assert!(
        output.contains("vpc_id"),
        "Expected vpc_id in diff output, got: {}",
        output
    );
    // cidr_block does not reference the replaced binding, so it should NOT appear
    assert!(
        !output.contains("cidr_block"),
        "cidr_block should not appear in diff output, got: {}",
        output
    );
    // Should show the old value and new value
    assert!(
        output.contains("vpc-old123"),
        "Expected old value in diff, got: {}",
        output
    );
    assert!(
        output.contains("vpc.vpc_id"),
        "Expected new value in diff, got: {}",
        output
    );
}

/// Test that a cascade-triggered changed_create_only attribute with the same old and new
/// value is still displayed with "(forces replacement, known after apply)" annotation.
///
/// This reproduces the real-world scenario where a cascade merge adds `vpc_id` to
/// `changed_create_only`, but the ResourceRef in `to` resolves to the current VPC ID
/// (same as `from`) because the new VPC hasn't been created yet. The attribute must
/// NOT be hidden even though `semantically_equal` returns true.
#[test]
fn test_replace_changed_create_only_same_value_shown_as_known_after_apply() {
    let id = ResourceId::with_provider_identity("awscc", "ec2.Subnet", "subnet", None);
    let from = State::existing(
        id.clone(),
        [
            (
                "vpc_id".to_string(),
                Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
            ),
            (
                "cidr_block".to_string(),
                Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
            ),
        ]
        .into_iter()
        .collect(),
    );
    let mut to = Resource::new("ec2.Subnet", "subnet")
        .with_attribute(
            "vpc_id",
            Value::Concrete(ConcreteValue::String("vpc-123".to_string())),
        )
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
        );
    to.id = id.clone();

    // Verify the precondition: the values are semantically equal
    assert!(
        Value::Concrete(ConcreteValue::String("vpc-123".to_string())).semantically_equal(
            &Value::Concrete(ConcreteValue::String("vpc-123".to_string()))
        ),
        "precondition: old and new vpc_id should be semantically equal"
    );

    let effect = Effect::Replace {
        from: Box::new(from),
        to: resolved(to),
        directives: Directives::default(),
        changed_create_only: carina_core::effect::ChangedCreateOnly::new(vec![
            "vpc_id".to_string(),
        ])
        .unwrap(),
        cascading_updates: Vec::<CascadingUpdate>::new(),
        temporary_name: None,
        cascade_ref_hints: Vec::new(),
    };
    let mut plan = Plan::new();
    plan.add(effect);
    let output = format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    );

    // vpc_id must appear in output, not be hidden
    assert!(
        output.contains("vpc_id"),
        "Expected vpc_id in output, got: {}",
        output
    );
    // Must show "known after apply" annotation
    assert!(
        output.contains("known after apply"),
        "Expected 'known after apply' in output, got: {}",
        output
    );
    // Must show "forces replacement" annotation
    assert!(
        output.contains("forces replacement"),
        "Expected 'forces replacement' in output, got: {}",
        output
    );
    // Must show the current value
    assert!(
        output.contains("vpc-123"),
        "Expected current value 'vpc-123' in output, got: {}",
        output
    );
}

/// Test that cascade_ref_hints causes the display to show the original ResourceRef
/// binding instead of the resolved value for cascade-triggered same-value attributes.
#[test]
fn test_replace_cascade_ref_hints_show_binding() {
    let id = ResourceId::with_provider_identity("awscc", "ec2.Subnet", "subnet", None);
    let from = State::existing(
        id.clone(),
        [(
            "vpc_id".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-0bf023ff87bf1aa0c".to_string())),
        )]
        .into_iter()
        .collect(),
    );
    let mut to = Resource::new("ec2.Subnet", "subnet").with_attribute(
        "vpc_id",
        Value::Concrete(ConcreteValue::String("vpc-0bf023ff87bf1aa0c".to_string())),
    );
    to.id = id.clone();

    let effect = Effect::Replace {
        from: Box::new(from),
        to: resolved(to),
        directives: Directives::default(),
        changed_create_only: carina_core::effect::ChangedCreateOnly::new(vec![
            "vpc_id".to_string(),
        ])
        .unwrap(),
        cascading_updates: Vec::<CascadingUpdate>::new(),
        temporary_name: None,
        cascade_ref_hints: vec![("vpc_id".to_string(), "vpc.vpc_id".to_string())],
    };
    let mut plan = Plan::new();
    plan.add(effect);
    let output = format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    );

    // Must show the ResourceRef hint instead of the resolved value on the right side
    assert!(
        output.contains("vpc.vpc_id"),
        "Expected 'vpc.vpc_id' hint in output, got: {}",
        output
    );
    // Must still show the old resolved value
    assert!(
        output.contains("vpc-0bf023ff87bf1aa0c"),
        "Expected old value in output, got: {}",
        output
    );
    // Must show forces replacement annotation
    assert!(
        output.contains("forces replacement, known after apply"),
        "Expected 'forces replacement, known after apply' in output, got: {}",
        output
    );
}

#[test]
fn test_deferred_create_renders_deferred_until_apply_marker() {
    let mut template_resource = Resource::new("route53.Record", "validation_records");
    template_resource.binding = Some("validation_records".to_string());
    template_resource.set_attr(
        "name",
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValuePath {
            path: AccessPath::with_fields("opt", "resource_record", vec!["name".to_string()]),
        })),
    );

    let deferred = carina_core::parser::DeferredForExpression {
        file: None,
        line: 1,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "aws.route53.Record".to_string(),
        attributes: template_resource
            .attributes
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        binding_name: "validation_records".to_string(),
        iterable_binding: "cert".to_string(),
        iterable_attr: "domain_validation_options".to_string(),
        binding: carina_core::parser::ForBinding::Simple("opt".to_string()),
        template_resource,
    };

    let mut plan = Plan::new();
    plan.add(Effect::DeferredCreate {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        upstream_binding: "cert".to_string(),
        template: Box::new(deferred),
    });

    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));

    insta::assert_snapshot!(output, @"
    Execution Plan:

      + aws.route53.Record validation_records[*] (N records after cert resolves)
          <- for opt in cert.domain_validation_options
          name: (known after cert resolves)

    Plan: 0 to add, 0 to change, 0 to destroy.
           N to add after cert resolves.
    ");
}

fn deferred_validation_records_effect() -> Effect {
    let mut template_resource = Resource::new("route53.Record", "validation_records");
    template_resource.binding = Some("validation_records".to_string());
    template_resource.set_attr(
        "name",
        Value::Deferred(DeferredValue::Unknown(UnknownReason::ForValuePath {
            path: AccessPath::with_fields("opt", "resource_record", vec!["name".to_string()]),
        })),
    );

    let deferred = carina_core::parser::DeferredForExpression {
        file: None,
        line: 1,
        header: "for opt in cert.domain_validation_options".to_string(),
        resource_type: "aws.route53.Record".to_string(),
        attributes: template_resource
            .attributes
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        binding_name: "validation_records".to_string(),
        iterable_binding: "cert".to_string(),
        iterable_attr: "domain_validation_options".to_string(),
        binding: carina_core::parser::ForBinding::Simple("opt".to_string()),
        template_resource,
    };

    Effect::DeferredCreate {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "__deferred_for",
            "validation_records",
        )),
        upstream_binding: "cert".to_string(),
        template: Box::new(deferred),
    }
}

fn delete_record_effect(binding: &str) -> Effect {
    Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "route53.Record",
            binding,
        )),
        identifier: format!("{binding}-id"),
        directives: Directives::default(),
        binding: Some(binding.to_string()),
        dependencies: HashSet::from(["cert".to_string()]),
        explicit_dependencies: HashSet::new(),
        blocked_by_updates: HashSet::new(),
    }
}

fn deferred_replace_validation_records_effect() -> Effect {
    let Effect::DeferredCreate {
        id,
        upstream_binding,
        template,
    } = deferred_validation_records_effect()
    else {
        unreachable!("helper constructs DeferredCreate")
    };
    let Effect::Delete {
        id: delete_id,
        identifier,
        directives,
        binding,
        dependencies,
        explicit_dependencies,
        blocked_by_updates,
    } = delete_record_effect("validation_records[0]")
    else {
        unreachable!("helper constructs Delete")
    };

    Effect::DeferredReplace {
        deletes: NonEmptyDeletes::try_new(vec![DeferredReplaceDelete {
            id: delete_id,
            identifier,
            directives,
            binding,
            dependencies,
            explicit_dependencies,
            blocked_by_updates,
        }])
        .expect("fixture has one delete"),
        id,
        upstream_binding,
        template,
    }
}

#[test]
fn test_deferred_replace_renders_top_level() {
    let mut plan = Plan::new();
    plan.add(deferred_replace_validation_records_effect());

    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));

    insta::assert_snapshot!(output, @"
    Execution Plan:

      +/- aws.route53.Record validation_records[*] (N records after cert resolves)
          <- for opt in cert.domain_validation_options
          name: (known after cert resolves)

    Plan: 0 to add, 0 to change, 0 to destroy.
           N to replace after cert resolves.
    ");
}

#[test]
fn test_deferred_replace_renders_dependent_children() {
    let mut dependent = Resource::new("acm.CertificateValidation", "cert-validation")
        .with_binding("cert_validation");
    dependent.set_attr(
        "validation_record_id",
        Value::resource_ref(
            "__deferred_for.validation_records".to_string(),
            "id".to_string(),
            vec![],
        ),
    );

    let mut plan = Plan::new();
    plan.add(deferred_replace_validation_records_effect());
    plan.add(Effect::Create(resolved(dependent)));

    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));

    insta::assert_snapshot!(output, @"
    Execution Plan:

      +/- aws.route53.Record validation_records[*] (N records after cert resolves)
          <- for opt in cert.domain_validation_options
          name: (known after cert resolves)
            │
            └─ + acm.CertificateValidation cert-validation
                  validation_record_id: __deferred_for.validation_records.id

    Plan: 1 to add, 0 to change, 0 to destroy.
           N to replace after cert resolves.
    ");
}

#[test]
fn test_deferred_replace_keeps_unrelated_same_type_delete() {
    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(
        Resource::new("acm.Certificate", "cert").with_binding("cert"),
    )));
    plan.add(delete_record_effect("old_record"));
    plan.add(deferred_replace_validation_records_effect());

    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    ));

    insta::assert_snapshot!(output, @"
    Execution Plan:

      + acm.Certificate cert
            ├─ +/- aws.route53.Record validation_records[*] (N records after cert applies)
            │     <- for opt in cert.domain_validation_options
            │     name: (known after cert applies)
            │
            └─ - route53.Record old_record

    Plan: 1 to add, 0 to change, 1 to destroy.
           N to replace after cert applies.
    ");
}

/// Test that cascading update diff only shows attributes referencing the replaced binding,
/// not attributes with false diffs due to DSL vs AWS format mismatch (issue #958).
#[test]
fn test_format_cascading_update_diff_excludes_non_ref_attributes() {
    use std::collections::HashMap;

    let cascade = CascadingUpdate {
        from: Box::new(State::existing(
            ResourceId::with_identity("ec2.Subnet", "subnet"),
            HashMap::from([
                (
                    "vpc_id".to_string(),
                    Value::Concrete(ConcreteValue::String("vpc-old123".to_string())),
                ),
                (
                    "availability_zone".to_string(),
                    Value::Concrete(ConcreteValue::String("ap-northeast-1a".to_string())),
                ),
                (
                    "cidr_block".to_string(),
                    Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
                ),
            ]),
        )),
        to: resolved(
            Resource::new("ec2.Subnet", "subnet")
                .with_attribute(
                    "vpc_id",
                    Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
                )
                .with_attribute(
                    "availability_zone",
                    Value::Concrete(ConcreteValue::String(
                        "awscc.AvailabilityZone.ap_northeast_1a".to_string(),
                    )),
                )
                .with_attribute(
                    "cidr_block",
                    Value::Concrete(ConcreteValue::String("10.0.1.0/24".to_string())),
                ),
        ),
    };

    let replaced_binding = "vpc";
    let output = format_cascading_update_diff(&cascade, "    ", replaced_binding);

    // vpc_id references the replaced binding "vpc", so it SHOULD appear
    assert!(
        output.contains("vpc_id"),
        "Expected vpc_id in diff output, got: {}",
        output
    );
    // availability_zone does NOT reference the replaced binding, so it should NOT appear
    // (this was the false diff in issue #958)
    assert!(
        !output.contains("availability_zone"),
        "availability_zone should not appear in diff (false diff), got: {}",
        output
    );
    // cidr_block does NOT reference the replaced binding, so it should NOT appear
    assert!(
        !output.contains("cidr_block"),
        "cidr_block should not appear in diff, got: {}",
        output
    );
}

/// Test that cascading update diff shows List attributes containing ResourceRef
/// to the replaced binding (e.g., security_group_ids = [sg.group_id]).
#[test]
fn test_format_cascading_update_diff_includes_list_with_ref() {
    use std::collections::HashMap;

    let cascade = CascadingUpdate {
        from: Box::new(State::existing(
            ResourceId::with_identity("ec2.Instance", "instance"),
            HashMap::from([(
                "security_group_ids".to_string(),
                Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                    ConcreteValue::String("sg-old123".to_string()),
                )])),
            )]),
        )),
        to: resolved(Resource::new("ec2.Instance", "instance").with_attribute(
            "security_group_ids",
            Value::Concrete(ConcreteValue::List(vec![Value::resource_ref(
                "sg".to_string(),
                "group_id".to_string(),
                vec![],
            )])),
        )),
    };

    let replaced_binding = "sg";
    let output = format_cascading_update_diff(&cascade, "    ", replaced_binding);

    assert!(
        output.contains("security_group_ids"),
        "Expected security_group_ids in diff output, got: {}",
        output
    );
}

/// Issue #949: Plan tree structure is lost for mixed effect plans.
///
/// When a plan contains Delete effects, they have no resource (only `id`
/// and `identifier`), so:
/// - No binding is registered in `binding_to_effect`
/// - No dependencies are extracted
/// - No `effect_bindings` or `effect_types` entry is created
///
/// This means Delete effects are invisible to the tree algorithm and
/// cannot participate as roots or children. The tree degrades to a flat
/// list for any plan containing Delete effects.
///
/// Scenario:
///   - VPC (Update) — root, no dependencies
///   - SG (Replace) — depends on VPC via ResourceRef
///   - Subnet (Delete) — should be child of VPC, but Delete has no resource
///
/// Expected tree:
///   vpc (update)
///   ├── sg (replace)
///   └── subnet (delete)
///
/// Actual: Delete effect for subnet is either missing from the tree or
/// appears as a disconnected root because it has no binding or dependency info.
#[test]
fn test_mixed_plan_tree_with_delete_effect() {
    // VPC: Update effect (has `to` resource with binding)
    let vpc_to = make_resource("ec2.Vpc", "vpc", "vpc", &[]);
    let vpc_from = State::existing(
        ResourceId::with_identity("ec2.Vpc", "vpc"),
        HashMap::from([(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        )]),
    );

    // SG: Replace effect (has `to` resource that depends on VPC)
    let sg_to = make_resource("ec2.SecurityGroup", "sg", "sg", &["vpc"]);
    let sg_from = State::existing(
        ResourceId::with_identity("ec2.SecurityGroup", "sg"),
        HashMap::from([(
            "ref_vpc".to_string(),
            Value::Concrete(ConcreteValue::String("vpc-old123".to_string())),
        )]),
    );

    // Subnet: Delete effect (only has id and identifier — no resource, no deps)
    // In the original DSL, subnet depends on VPC, but Delete loses that info.
    let subnet_delete = Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_identity(
            "ec2.Subnet",
            "subnet",
        )),
        identifier: "subnet-12345".to_string(),
        directives: Directives::default(),
        binding: Some("subnet".to_string()),
        dependencies: HashSet::from(["vpc".to_string()]),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    };

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(vpc_from),
        to: resolved(vpc_to),
        changed_attributes: vec!["cidr_block".to_string()],
    });
    plan.add(Effect::Replace {
        from: Box::new(sg_from),
        to: resolved(sg_to),
        directives: Directives::default(),
        changed_create_only: carina_core::effect::ChangedCreateOnly::new(vec![
            "ref_vpc".to_string(),
        ])
        .unwrap(),
        cascading_updates: vec![],
        temporary_name: None,
        cascade_ref_hints: vec![],
    });
    plan.add(subnet_delete);

    let (roots, dependents, effect_bindings, _effect_types) = build_plan_tree(&plan);

    // VPC (idx 0) should be the only root
    assert_eq!(
        roots,
        vec![0],
        "VPC should be the only root. Got roots: {:?} (bindings: {:?})",
        roots,
        roots
            .iter()
            .filter_map(|i| effect_bindings.get(i))
            .collect::<Vec<_>>()
    );

    // SG (idx 1) should be a child of VPC
    let vpc_children: Vec<usize> = dependents.get(&0).cloned().unwrap_or_default();
    assert!(
        vpc_children.contains(&1),
        "SG (idx 1) should be a child of VPC. VPC children: {:?}",
        vpc_children
    );

    // Subnet Delete (idx 2) should also be a child of VPC.
    // Currently fails because Delete effects have no binding/dependency info.
    assert!(
        vpc_children.contains(&2),
        "Subnet Delete (idx 2) should be a child of VPC, but Delete effects \
         have no resource/binding/dependency info so the tree cannot place them. \
         VPC children: {:?}, all roots: {:?}",
        vpc_children,
        roots
    );
}

/// Issue #972: When ResourceRef values are resolved to literal strings
/// (as happens with --refresh=false), the tree algorithm loses dependency
/// information and places the resource as a top-level root instead of
/// nesting it under its parent.
///
/// Scenario (mixed_operations fixture):
///   - VPC: Update effect (existing, tags changed)
///   - SG: Create effect with vpc_id = "vpc-0123456789abcdef0"
///     (resolved from vpc.vpc_id by resolve_refs_with_state)
///
/// The SG should be nested under VPC because in the DSL it has
/// `vpc_id = vpc.vpc_id`, but after resolve_refs_with_state() the
/// ResourceRef is replaced with a plain string, so
/// get_resource_dependencies() finds no dependencies.
#[test]
fn test_resolved_ref_loses_dependency_for_tree_nesting() {
    // VPC: Update effect (tags changed)
    let vpc_to = make_resource("ec2.Vpc", "vpc", "vpc", &[]);
    let vpc_from = State::existing(
        ResourceId::with_identity("ec2.Vpc", "vpc"),
        HashMap::from([(
            "cidr_block".to_string(),
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".to_string())),
        )]),
    );

    // SG: Create effect with RESOLVED ref (string instead of ResourceRef).
    // This is what happens after resolve_refs_with_state() runs:
    // vpc_id = vpc.vpc_id becomes vpc_id = "vpc-0123456789abcdef0"
    let mut sg = Resource::new("ec2.SecurityGroup", "sg");
    sg.binding = Some("sg".to_string());
    // This is the resolved value — a plain string, NOT a ResourceRef
    sg.set_attr(
        "vpc_id".to_string(),
        Value::Concrete(ConcreteValue::String("vpc-0123456789abcdef0".to_string())),
    );
    sg.set_attr(
        "group_description".to_string(),
        Value::Concrete(ConcreteValue::String("Test security group".to_string())),
    );
    // _dependency_bindings is saved by resolve_refs_with_state() before
    // ResourceRef values are resolved to strings.
    sg.dependency_bindings = std::collections::BTreeSet::from(["vpc".to_string()]);

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        from: Box::new(vpc_from),
        to: resolved(vpc_to),
        changed_attributes: vec!["tags".to_string()],
    });
    plan.add(Effect::Create(resolved(sg)));

    let (roots, dependents, effect_bindings, _effect_types) = build_plan_tree(&plan);

    // VPC (idx 0) should be the only root.
    // SG has _dependency_bindings metadata that preserves the dependency
    // on "vpc", so get_resource_dependencies() recovers it.
    assert_eq!(
        roots,
        vec![0],
        "VPC should be the only root. SG should be nested under VPC \
         because it references vpc.vpc_id in the DSL. Got roots: {:?} \
         (bindings: {:?})",
        roots,
        roots
            .iter()
            .filter_map(|i| effect_bindings.get(i))
            .collect::<Vec<_>>()
    );

    // SG (idx 1) should be a child of VPC (idx 0)
    let vpc_children: Vec<usize> = dependents.get(&0).cloned().unwrap_or_default();
    assert!(
        vpc_children.contains(&1),
        "SG (idx 1) should be a child of VPC. VPC children: {:?}",
        vpc_children
    );
}

#[test]
fn format_effect_delete_uses_binding_name() {
    let effect = Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "awscc",
            "ec2.Vpc",
            "ec2_vpc_fb75c929",
            None,
        )),
        identifier: "vpc-12345".to_string(),
        directives: Directives::default(),
        binding: Some("my_vpc".to_string()),
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    };
    assert_eq!(format_effect(&effect), "Delete awscc.ec2.Vpc my_vpc");
}

#[test]
fn format_effect_delete_falls_back_to_id_name() {
    let effect = Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "awscc",
            "ec2.Vpc",
            "ec2_vpc_fb75c929",
            None,
        )),
        identifier: "vpc-12345".to_string(),
        directives: Directives::default(),
        binding: None,
        dependencies: HashSet::new(),
        explicit_dependencies: std::collections::HashSet::new(),
        blocked_by_updates: HashSet::new(),
    };
    assert_eq!(
        format_effect(&effect),
        "Delete awscc.ec2.Vpc ec2_vpc_fb75c929"
    );
}

/// Each `format_effect` variant must separate `provider.resource_type`
/// from the resource name with a single space (carina-rs/carina#2572)
/// so the type/address boundary is visible in plan/apply output.
#[test]
fn format_effect_create_separates_type_and_name_with_space() {
    let mut r = Resource::new("s3.Bucket", "state_bucket");
    r.id = ResourceId::with_provider_identity("aws", "s3.Bucket", "state_bucket", None);
    let effect = Effect::Create(resolved(r));
    assert_eq!(format_effect(&effect), "Create aws.s3.Bucket state_bucket");
}

#[test]
fn format_effect_update_separates_type_and_name_with_space() {
    let id = ResourceId::with_provider_identity("awscc", "iam.Role", "bs.bootstrap.role", None);
    let mut to = Resource::new("iam.Role", "bs.bootstrap.role");
    to.id = id.clone();
    let effect = Effect::Update {
        from: Box::new(State::not_found(ResourceId::with_provider_identity(
            "awscc",
            "iam.Role",
            "bs.bootstrap.role",
            None,
        ))),
        to: resolved(to),
        changed_attributes: Vec::new(),
    };
    assert_eq!(
        format_effect(&effect),
        "Update awscc.iam.Role bs.bootstrap.role"
    );
}

#[test]
fn format_effect_replace_separates_type_and_name_with_space() {
    let id = ResourceId::with_provider_identity("awscc", "ec2.Vpc", "vpc", None);
    let mut to = Resource::new("ec2.Vpc", "vpc");
    to.id = id.clone();
    let effect = Effect::Replace {
        from: Box::new(State::not_found(ResourceId::with_provider_identity(
            "awscc", "ec2.Vpc", "vpc", None,
        ))),
        to: resolved(to),
        directives: Directives::default(),
        changed_create_only: carina_core::effect::ChangedCreateOnly::new(vec!["attr".to_string()])
            .unwrap(),
        cascading_updates: Vec::<CascadingUpdate>::new(),
        temporary_name: None,
        cascade_ref_hints: Vec::new(),
    };
    assert_eq!(format_effect(&effect), "Replace awscc.ec2.Vpc vpc");
}

#[test]
fn format_replace_with_removed_create_only_attribute_shows_forcing_detail() {
    let id = ResourceId::with_provider_identity("test", "Widget", "beta", None);
    let from = State::existing(
        id.clone(),
        [(
            "legacy_token".to_string(),
            Value::Concrete(ConcreteValue::String("tok".to_string())),
        )]
        .into_iter()
        .collect(),
    );
    let mut to = Resource::new("Widget", "beta");
    to.id = id.clone();
    let effect = Effect::Replace {
        from: Box::new(from),
        to: resolved(to),
        directives: Directives::default(),
        changed_create_only: carina_core::effect::ChangedCreateOnly::new(vec![
            "legacy_token".to_string(),
        ])
        .unwrap(),
        cascading_updates: Vec::<CascadingUpdate>::new(),
        temporary_name: None,
        cascade_ref_hints: Vec::new(),
    };
    let mut plan = Plan::new();
    plan.add(effect);

    let output = format_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
        None,
        None,
    );

    assert!(
        output.contains("legacy_token"),
        "expected removed create-only key in replace details, got:\n{output}"
    );
    assert!(
        output.contains("(forces replacement)"),
        "expected forcing annotation in replace details, got:\n{output}"
    );
}

#[test]
fn format_effect_import_separates_type_and_name_with_space() {
    let effect = Effect::Import {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "aws",
            "s3.Bucket",
            "logs",
            None,
        )),
        identifier: carina_core::resource::Value::Concrete(
            carina_core::resource::ConcreteValue::String("my-logs-bucket".to_string()),
        ),
    };
    assert_eq!(
        format_effect(&effect),
        "Import aws.s3.Bucket logs (id: my-logs-bucket)"
    );
}

#[test]
fn format_effect_remove_separates_type_and_name_with_space() {
    let effect = Effect::Remove {
        id: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "awscc", "ec2.Vpc", "old_vpc", None,
        )),
    };
    assert_eq!(
        format_effect(&effect),
        "Remove awscc.ec2.Vpc old_vpc from state"
    );
}

#[test]
fn format_effect_move_separates_type_and_name_with_space() {
    let effect = Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "awscc", "ec2.Vpc", "vpc_a", None,
        )),
        to: carina_core::resource::ResolvedResourceId::new(ResourceId::with_provider_identity(
            "awscc", "ec2.Vpc", "vpc_b", None,
        )),
    };
    assert_eq!(
        format_effect(&effect),
        "Move awscc.ec2.Vpc vpc_a -> awscc.ec2.Vpc vpc_b"
    );
}

#[test]
fn split_top_level_simple_elements() {
    assert_eq!(
        split_top_level(r#""a", "b", "c""#),
        vec![r#""a""#, r#""b""#, r#""c""#]
    );
}

#[test]
fn split_top_level_nested_brackets() {
    assert_eq!(split_top_level("[1, 2], [3, 4]"), vec!["[1, 2]", "[3, 4]"]);
}

#[test]
fn split_top_level_nested_braces() {
    assert_eq!(
        split_top_level("{a: 1, b: 2}, {c: 3}"),
        vec!["{a: 1, b: 2}", "{c: 3}"]
    );
}

#[test]
fn split_top_level_quoted_commas() {
    assert_eq!(
        split_top_level(r#""a, b", "c""#),
        vec![r#""a, b""#, r#""c""#]
    );
}

#[test]
fn colored_value_list_elements_colored_individually() {
    // Each element in a list should be colored by its type
    let result = colored_value(r#"["hello", "world"]"#, false);
    // Each quoted string should be green, not the whole list
    assert!(result.contains('['));
    assert!(result.contains(']'));
    // The individual strings should have color codes
    let green_hello = "\"hello\"".green().to_string();
    assert!(
        result.contains(&green_hello),
        "Expected green-colored hello in: {}",
        result
    );
}

#[test]
fn colored_value_list_ref_binding_cyan() {
    let result = colored_value("[binding.attr]", true);
    let cyan_elem = "binding.attr".cyan().to_string();
    assert!(
        result.contains(&cyan_elem),
        "Expected cyan ref in: {}",
        result
    );
}

#[test]
fn colored_value_map_values_colored() {
    let result = colored_value(r#"{Name: "test-vpc", count: 42}"#, false);
    let green_val = "\"test-vpc\"".green().to_string();
    let white_val = "42".white().to_string();
    assert!(
        result.contains(&green_val),
        "Expected green string value in: {}",
        result
    );
    assert!(
        result.contains(&white_val),
        "Expected white number in: {}",
        result
    );
}

#[test]
fn colored_value_empty_list() {
    assert_eq!(colored_value("[]", false), "[]");
}

#[test]
fn colored_value_empty_map() {
    assert_eq!(colored_value("{}", false), "{}");
}

/// Strip ANSI SGR (color) escape sequences for assertion purposes.
/// Matches the helper used by `plan_snapshot_tests.rs` and
/// `module_info_snapshot_tests.rs` (both call sites use this same regex).
fn strip_ansi(s: &str) -> String {
    let re = regex_lite::Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    re.replace_all(s, "").to_string()
}

#[test]
fn colored_value_preserves_vertical_list_layout() {
    // `format_value_pretty` emits a multi-line bracketed form for lists that
    // exceed the 80-col threshold. `colored_value` must NOT collapse the
    // layout back to inline — it should color atoms in place and keep
    // newlines / leading indentation intact.
    let input = "[\n  \"a\",\n  \"b\",\n  \"c\",\n]";
    let result = colored_value(input, false);
    let stripped = strip_ansi(&result);
    assert_eq!(
        stripped, input,
        "vertical layout (newlines, indent, trailing comma, closing bracket) must be preserved verbatim",
    );
    let green_a = "\"a\"".green().to_string();
    assert!(
        result.contains(&green_a),
        "string atoms inside the vertical list should still be colored: {result:?}",
    );
}

#[test]
fn colored_value_preserves_vertical_list_closing_bracket_alone() {
    // The closing `]` sits on its own line, indented to the parent. This is
    // the most layout-fragile token — earlier impls collapsed it into the
    // previous line as `,]`. Pin its standalone-line preservation explicitly.
    let input = "[\n  \"x\",\n]";
    let result = colored_value(input, false);
    let stripped = strip_ansi(&result);
    assert_eq!(stripped, input);
    assert!(
        stripped.ends_with("\n]"),
        "closing bracket must remain on its own line, got: {stripped:?}",
    );
}

#[test]
fn colored_value_preserves_vertical_map_layout() {
    // Same constraint for `format_map_vertical` output.
    let input = "\n  k1: \"v1\"\n  k2: 42";
    let result = colored_value(input, false);
    let stripped = strip_ansi(&result);
    assert_eq!(
        stripped, input,
        "vertical map layout must be preserved verbatim, got: {result:?}",
    );
}

#[test]
fn format_export_value_duration_renders_canonical() {
    // Regression: simplify + Round 1 added the Value::Concrete(ConcreteValue::Duration) arm so
    // exports of a Duration attribute don't fall through to the
    // wildcard "(known after apply)" placeholder. Pin the canonical
    // form here so a future refactor cannot silently regress to either
    // the wildcard or the {:?} Debug shape.
    let v = Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(60)));
    assert_eq!(format_export_value(&v), "1min");
}

#[test]
fn format_deferred_value_duration_renders_canonical() {
    // Same shape as format_export_value but on the deferred-for path.
    let v = Value::Concrete(ConcreteValue::Duration(std::time::Duration::from_secs(60)));
    let result =
        carina_core::plan_tree::format_deferred_for_template_value(&v, "ttl", "cert", "applies");
    // The deferred path may inject ANSI dimming for Unknown variants,
    // but a resolved Duration must render verbatim.
    assert_eq!(result, "1min");
}

/// Regression for #3115: a deleted-effect attribute whose value is a
/// multi-line `format_value_pretty` map (e.g. CloudFront
/// `default_cache_behavior`) must not bleed the red strikethrough
/// across the *leading indentation whitespace* of continuation lines.
///
/// `colored` places the ANSI style once at the start and the reset
/// once at the end of the whole string. Styling the entire multi-line
/// pretty payload in one shot makes the strike span the newline-leading
/// indent spaces of every continuation line, so the strike appears to
/// start at the left edge instead of at the content column.
///
/// Invariant asserted: on every rendered line, the first non-whitespace
/// character must appear before any ANSI escape (`\x1b`). Equivalently,
/// the leading-whitespace prefix of each line carries no escape byte.
#[test]
fn delete_pretty_attribute_does_not_strike_indentation() {
    use indexmap::IndexMap;

    // A nested map forces `format_value_pretty` into a multi-line
    // vertical layout with indented continuation lines, mirroring the
    // real CloudFront `default_cache_behavior` shape from the bug
    // report.
    let mut forwarded_values = IndexMap::new();
    forwarded_values.insert(
        "query_string".to_string(),
        Value::Concrete(ConcreteValue::Bool(false)),
    );
    let mut cache_behavior = IndexMap::new();
    cache_behavior.insert(
        "viewer_protocol_policy".to_string(),
        Value::Concrete(ConcreteValue::String("redirect-to-https".to_string())),
    );
    cache_behavior.insert(
        "forwarded_values".to_string(),
        Value::Concrete(ConcreteValue::Map(forwarded_values)),
    );

    let row = DetailRow::PrettyAttribute {
        key: "default_cache_behavior".to_string(),
        value: Value::Concrete(ConcreteValue::Map(cache_behavior)),
    };
    let effect = Effect::Delete {
        id: carina_core::resource::ResolvedResourceId::new(
            carina_core::resource::ResourceId::with_identity("cloudfront.Distribution", "dist"),
        ),
        identifier: "E123".to_string(),
        directives: Default::default(),
        binding: None,
        dependencies: Default::default(),
        explicit_dependencies: Default::default(),
        blocked_by_updates: Default::default(),
    };

    // Force ANSI styling on: the test harness' stdout is not a TTY, so
    // `colored` auto-disables escapes and the bug (which is *in* the
    // escape placement) would be invisible without this override.
    colored::control::set_override(true);

    let mut out = String::new();
    // Non-empty `attr_prefix` so continuation lines have a real indent.
    render_detail_row(&mut out, &row, &effect, "    ");

    colored::control::unset_override();

    // A style that opens on one line and resets on a later line keeps
    // strikethrough/red active across the intervening newline(s), so
    // the terminal paints the strike over the leading indentation of
    // every continuation line. The correct shape (matching the
    // non-delete `color_lines` path) opens and closes the style
    // *within* each line, after that line's indentation.
    //
    // Invariant: at every `\n`, the ANSI style depth must be zero —
    // no styling span may cross a line boundary.
    let mut depth: i32 = 0;
    let mut saw_open = false;
    let bytes: Vec<char> = out.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '\u{1b}' && i + 1 < bytes.len() && bytes[i + 1] == '[' {
            // Parse a CSI `\x1b[...m` sequence.
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != 'm' {
                j += 1;
            }
            let params: String = bytes[i + 2..j].iter().collect();
            if params == "0" {
                depth = (depth - 1).max(0);
            } else {
                depth += 1;
                saw_open = true;
            }
            i = j + 1;
            continue;
        }
        if c == '\n' {
            assert_eq!(
                depth, 0,
                "an ANSI style span crossed a newline: strikethrough/red \
                 bleeds across the leading indentation of the next line. \
                 rendered: {:?}",
                out,
            );
        }
        i += 1;
    }
    assert!(
        saw_open,
        "no ANSI styling was emitted; the test must run with colored \
         override on and exercise the Delete styling path",
    );
}

#[test]
fn forcing_changed_uses_plain_green_cli_value() {
    let row = DetailRow::ChangedForcesReplacement {
        key: "name".to_string(),
        old: "\"old\"".to_string(),
        new: "\"new\"".to_string(),
    };
    let effect = Effect::Update {
        from: Box::new(State::existing(
            ResourceId::with_identity("test.Widget", "w"),
            HashMap::new(),
        )),
        to: resolved(Resource::new("test.Widget", "w")),
        changed_attributes: vec!["name".to_string()],
    };

    colored::control::set_override(true);
    let mut out = String::new();
    render_detail_row(&mut out, &row, &effect, "  ");
    colored::control::unset_override();

    assert!(
        out.contains("\u{1b}[32m\"new\"\u{1b}[0m"),
        "forcing Changed rows must render the new value with plain green: {out:?}"
    );
}

#[test]
fn forcing_changed_multiline_green_style_does_not_cross_newline() {
    let row = DetailRow::ChangedForcesReplacement {
        key: "rules".to_string(),
        old: "[]".to_string(),
        new: "[\n  \"new\"\n]".to_string(),
    };
    let effect = Effect::Update {
        from: Box::new(State::existing(
            ResourceId::with_identity("test.Widget", "w"),
            HashMap::new(),
        )),
        to: resolved(Resource::new("test.Widget", "w")),
        changed_attributes: vec!["rules".to_string()],
    };

    colored::control::set_override(true);
    let mut out = String::new();
    render_detail_row(&mut out, &row, &effect, "  ");
    colored::control::unset_override();

    assert!(
        out.contains("\u{1b}[32m[\u{1b}[0m\n\u{1b}[32m  \"new\"\u{1b}[0m\n\u{1b}[32m]\u{1b}[0m"),
        "forcing Changed multiline values must reset green styling before each newline: {out:?}"
    );
}

/// When a state refresh has been printed above the plan output, a single
/// blank line must separate the refresh-progress block from the plan's
/// terminal section (`Execution Plan:` or `No changes.`). See issue #3148.
#[test]
fn test_refresh_plan_separator_emits_blank_line_after_refresh() {
    assert_eq!(refresh_plan_separator(true), "\n");
}

/// Without a refresh (e.g. `--refresh=false`, fixture/snapshot path) there
/// is no progress block above the plan, so no extra blank line is emitted.
#[test]
fn test_refresh_plan_separator_no_blank_line_without_refresh() {
    assert_eq!(refresh_plan_separator(false), "");
}

/// carina#3322: the composition group header reads
/// `▾ module "<binding>" (<source_path>)`. The keyword is `module`,
/// not the internal `Composition`; the binding name is the user's
/// `let` LHS, and the parenthesized suffix surfaces the DSL `use`
/// path so the operator can trace the group back to a real `.crn`
/// file.
#[test]
fn test_composition_header_renders_module_with_source_path() {
    let header = strip_ansi(&format_composition_header("r", Some("./modules/infra")));
    assert_eq!(header, r#"  ▾ module "r" (./modules/infra)"#);
}

/// Fallback shape when no `use` path was recorded for the call site
/// (test fixtures, hand-built traces). The parenthesized suffix is
/// dropped so the header still reads as a clean `▾ module "<binding>"`
/// line — never as a literal "None" or an empty `()`.
#[test]
fn test_composition_header_drops_parens_for_none_source_path() {
    let header = strip_ansi(&format_composition_header("r", None));
    assert_eq!(header, r#"  ▾ module "r""#);
}

#[test]
fn module_header_sigil_is_module_specific_not_create() {
    let module_sigil = Sigil::module_header();
    let create_sigil =
        Effect::Create(resolved(Resource::new("aws.s3.Bucket", "x"))).display_glyph();

    assert_eq!(module_sigil.raw, "▾");
    assert_ne!(
        module_sigil.raw, create_sigil,
        "module header sigil must differ from Effect::Create's display glyph"
    );
}

#[test]
fn every_top_level_sigil_starts_at_left_margin_column() {
    fn raw_sigil(raw: &'static str) -> Sigil {
        Sigil {
            raw,
            rendered: raw.normal(),
        }
    }

    let cases = vec![
        ("effect_create", raw_sigil("+")),
        ("effect_delete", raw_sigil("-")),
        ("effect_update_or_remove", raw_sigil("~")),
        ("effect_wait", raw_sigil(">")),
        ("effect_read", raw_sigil("<=")),
        ("effect_import", raw_sigil("<-")),
        ("effect_move", raw_sigil("->")),
        ("effect_replace_create_before_destroy", raw_sigil("+/-")),
        ("effect_replace_delete_before_create", raw_sigil("-/+")),
        ("module_header", Sigil::module_header()),
        ("deferred_for", Sigil::deferred_for_expression()),
        ("export_added", Sigil::export_added()),
        ("export_modified", Sigil::export_modified()),
        ("export_removed", Sigil::export_removed()),
    ];

    for (label, sigil) in cases {
        let prefix = top_level_sigil_prefix(&sigil);
        let plain = strip_ansi(&prefix);
        assert_eq!(
            plain.find(sigil.raw),
            Some(LEFT_MARGIN.len()),
            "sigil {label} did not start at the left-margin column: {plain:?}",
        );
    }
}

#[test]
fn module_children_render_with_tree_connectors() {
    use carina_core::resource::{CallSite, EphemeralId, ExpansionTrace, PersistentId};
    use carina_core::schema::SchemaRegistry;

    let inner = make_resource("aws.eks.Cluster", "cluster/inner", "inner", &[]);
    let role = make_resource("aws.iam.Role", "cluster/inner-role", "inner_role", &[]);
    let inner_id = inner.id.clone();
    let role_id = role.id.clone();

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(inner)));
    plan.add(Effect::Create(resolved(role)));

    let cluster_site = CallSite::new(
        EphemeralId::new(ResourceId::with_identity("_virtual", "cluster")),
        "./modules/cluster",
    );
    let mut trace = ExpansionTrace::new();
    trace.record(PersistentId::new(inner_id), vec![cluster_site.clone()]);
    trace.record(PersistentId::new(role_id), vec![cluster_site]);

    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        Some(&SchemaRegistry::new()),
        &HashMap::new(),
        &[],
        &[],
        None,
        Some(&trace),
    ));

    assert!(
        output.contains(
            "  ▾ module \"cluster\" (./modules/cluster)\n     ├─ + aws.eks.Cluster cluster/inner\n     └─ + aws.iam.Role cluster/inner-role"
        ),
        "module leaves must render as connector children:\n{output}",
    );
}

#[test]
fn module_child_connector_gutter_extends_through_nested_dependents() {
    use carina_core::resource::{CallSite, EphemeralId, ExpansionTrace, PersistentId};
    use carina_core::schema::SchemaRegistry;

    let cluster = make_resource("aws.eks.Cluster", "cluster/inner", "inner", &[]);
    let node_role = make_resource("aws.iam.Role", "cluster/node-role", "node_role", &["inner"]);
    let bucket = make_resource("aws.s3.Bucket", "cluster/logs", "logs", &[]);
    let cluster_id = cluster.id.clone();
    let bucket_id = bucket.id.clone();

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(cluster)));
    plan.add(Effect::Create(resolved(node_role)));
    plan.add(Effect::Create(resolved(bucket)));

    let cluster_site = CallSite::new(
        EphemeralId::new(ResourceId::with_identity("_virtual", "cluster")),
        "./modules/cluster",
    );
    let mut trace = ExpansionTrace::new();
    trace.record(PersistentId::new(cluster_id), vec![cluster_site.clone()]);
    trace.record(PersistentId::new(bucket_id), vec![cluster_site]);

    let output = strip_ansi(&format_plan(
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        Some(&SchemaRegistry::new()),
        &HashMap::new(),
        &[],
        &[],
        None,
        Some(&trace),
    ));

    assert!(
        output.contains(
            "     ├─ + aws.eks.Cluster cluster/inner\n     │     └─ + aws.iam.Role cluster/node-role\n     └─ + aws.s3.Bucket cluster/logs"
        ),
        "outer module gutter must continue through the child subtree:\n{output}",
    );
}

#[test]
fn module_group_with_only_suppressed_move_does_not_emit_orphan_gutter() {
    let from_id = ResourceId::with_identity("aws.s3.Bucket", "cluster/old_logs");
    let to_id = ResourceId::with_identity("aws.s3.Bucket", "cluster/logs");
    let mut updated = Resource::new("aws.s3.Bucket", "cluster/logs");
    updated.id = to_id.clone();

    let mut plan = Plan::new();
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(from_id),
        to: carina_core::resource::ResolvedResourceId::new(to_id.clone()),
    });
    plan.add(Effect::Update {
        from: Box::new(State::not_found(to_id.clone())),
        to: resolved(updated),
        changed_attributes: Vec::new(),
    });

    let moved_origins = HashMap::new();
    let mut ctx = TreeRenderContext {
        out: format!(
            "{}\n",
            strip_ansi(&format_composition_header(
                "cluster",
                Some("./modules/cluster")
            ))
        ),
        printed: HashSet::new(),
        plan: &plan,
        dependents: HashMap::new(),
        detail: DetailLevel::None,
        delete_attributes: None,
        schemas: None,
        moved_origins: &moved_origins,
        prev_explicit: None,
        update_or_replace_targets: [to_id].into_iter().collect(),
    };

    let rendered_attrs = ctx.render_children(
        &[ChildRenderItem::Normal(0)],
        ChildRenderOptions {
            parent_indent: 0,
            parent_is_last: true,
            parent_prefix: "",
            parent_binding: None,
            parent_displayed_attrs: false,
            child_prefix_override: Some(module_child_prefix()),
        },
    );

    assert!(
        !rendered_attrs,
        "suppressed children should not report displayed attributes"
    );
    assert!(
        !ctx.out
            .contains("  ▾ module \"cluster\" (./modules/cluster)\n     │\n"),
        "module group with only suppressed children must not emit an orphan gutter:\n{output}",
        output = ctx.out,
    );
    assert_eq!(ctx.out, "  ▾ module \"cluster\" (./modules/cluster)\n");
    assert!(
        ctx.printed.contains(&0),
        "suppressed move should be consumed"
    );
}

#[test]
fn resource_with_only_suppressed_move_dependent_does_not_emit_orphan_gutter() {
    let parent = make_resource("aws.s3.Bucket", "parent", "parent", &[]).with_attribute(
        "bucket",
        Value::Concrete(ConcreteValue::String("parent".to_string())),
    );
    let from_id = ResourceId::with_identity("aws.s3.Bucket", "old_child");
    let to_id = ResourceId::with_identity("aws.s3.Bucket", "new_child");
    let mut updated = Resource::new("aws.s3.Bucket", "new_child");
    updated.id = to_id.clone();

    let mut plan = Plan::new();
    plan.add(Effect::Create(resolved(parent)));
    plan.add(Effect::Move {
        from: carina_core::resource::ResolvedResourceId::new(from_id),
        to: carina_core::resource::ResolvedResourceId::new(to_id.clone()),
    });
    plan.add(Effect::Update {
        from: Box::new(State::not_found(to_id.clone())),
        to: resolved(updated),
        changed_attributes: Vec::new(),
    });

    let moved_origins = HashMap::new();
    let mut ctx = TreeRenderContext {
        out: String::new(),
        printed: HashSet::new(),
        plan: &plan,
        dependents: HashMap::from([(0, vec![1])]),
        detail: DetailLevel::Full,
        delete_attributes: None,
        schemas: None,
        moved_origins: &moved_origins,
        prev_explicit: None,
        update_or_replace_targets: [to_id].into_iter().collect(),
    };

    let rendered_attrs = ctx.format_render_item(&ChildRenderItem::Normal(0), 0, true, "", None);
    let output = strip_ansi(&ctx.out);

    assert!(rendered_attrs, "parent create should display attributes");
    assert!(
        !output.lines().any(|line| line == "        │"),
        "resource with only suppressed move dependent must not emit an orphan gutter:\n{output}",
    );
    assert!(
        ctx.printed.contains(&1),
        "suppressed move dependent should be consumed"
    );
}

/// carina#3356: `reindent_with_gutter` restores the tree gutter on every
/// continuation line of a multi-line value block. The caller emits
/// `attr_prefix` ahead of line 0, so line 0 is returned verbatim; each
/// later line's leading `attr_prefix`-width spaces are swapped for the
/// gutter string, preserving the value-relative indent past the gutter.
#[test]
fn test_reindent_with_gutter_restores_gutter_on_continuation_lines() {
    // attr_prefix is 8 columns wide: 5 spaces, `│`, 2 spaces.
    let attr_prefix = "     │  ";
    // `format_value_pretty`-style block: line 0 bare (caller prepends the
    // prefix), continuation lines indented with plain spaces sized to the
    // gutter width + value indent. The `*`-marked element key sits 2 cols
    // past the gutter; the sibling continuation key aligns 2 further in.
    let block = "\n          * action: \"x\"\n            effect: \"y\"";
    let out = reindent_with_gutter(block, attr_prefix);
    assert_eq!(
        out,
        "\n     │    * action: \"x\"\n     │      effect: \"y\"",
    );
}

/// A blank inter-element separator (empty line) collapses to the bare
/// gutter with no trailing padding, matching the tree's own `│`-only
/// continuation lines (and avoiding trailing-whitespace noise).
#[test]
fn test_reindent_with_gutter_blank_line_becomes_bare_gutter() {
    let attr_prefix = "     │  ";
    let block = "\n          * a: \"x\"\n\n          * b: \"y\"";
    let out = reindent_with_gutter(block, attr_prefix);
    assert_eq!(out, "\n     │    * a: \"x\"\n     │\n     │    * b: \"y\"",);
}

/// A single-line value has no continuation rows, so it is returned
/// unchanged — the caller's `attr_prefix` on line 0 already suffices.
#[test]
fn test_reindent_with_gutter_single_line_unchanged() {
    assert_eq!(reindent_with_gutter("\"scalar\"", "     │  "), "\"scalar\"");
}

/// For a root resource the gutter is pure spaces, so reindenting is a
/// no-op (leading spaces swapped for an equal-width run of spaces) and a
/// blank separator trims back to empty — existing root-level snapshots
/// must stay byte-identical.
#[test]
fn test_reindent_with_gutter_no_gutter_is_noop() {
    let attr_prefix = "      "; // 6 spaces, no `│`
    let block = "\n        * a: \"x\"\n\n        * b: \"y\"";
    let out = reindent_with_gutter(block, attr_prefix);
    assert_eq!(out, block);
}
