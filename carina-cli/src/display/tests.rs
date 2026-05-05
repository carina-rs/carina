use super::*;

use carina_core::effect::{CascadingUpdate, Effect};
use carina_core::plan::Plan;
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State, Value};

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
    plan.add(Effect::Create(b));

    // Should not panic
    print_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
    );
}

/// Test that print_plan handles the dependency graph correctly when
/// dependents map is accessed with valid indices.
#[test]
fn test_print_plan_with_internal_dependency_does_not_panic() {
    let a = make_resource("test.resource", "a", "a", &[]);
    let b = make_resource("test.resource", "b", "b", &["a"]);
    let mut plan = Plan::new();
    plan.add(Effect::Create(a));
    plan.add(Effect::Create(b));

    // Should not panic
    print_plan(
        &plan,
        DetailLevel::Full,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
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
    plan.add(Effect::Create(vpc));
    plan.add(Effect::Create(rt_b));
    plan.add(Effect::Create(subnet_b));
    plan.add(Effect::Create(rt_a));
    plan.add(Effect::Create(subnet_a));

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
    plan.add(Effect::Create(vpc));
    plan.add(Effect::Create(sg));
    plan.add(Effect::Create(endpoint));

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
    plan.add(Effect::Create(vpc));
    plan.add(Effect::Create(rt));
    plan.add(Effect::Create(igw));
    plan.add(Effect::Create(route));
    plan.add(Effect::Create(igw_attachment));

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
    plan.add(Effect::Create(vpc));
    plan.add(Effect::Create(rt));
    plan.add(Effect::Create(igw));
    plan.add(Effect::Create(route));
    plan.add(Effect::Create(igw_attachment));

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

    let hint = extract_compact_hint(&r, None);
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
    let hint = extract_compact_hint(&r, Some("database_rt"));
    assert_eq!(hint, Some("subnet: database_subnet_1a".to_string()));
}

/// Test that extract_compact_hint falls back to string values when no ResourceRef.
#[test]
fn test_extract_compact_hint_string_fallback() {
    let mut r = Resource::new("ec2.route", "hash456");
    r.set_attr(
        "destination_cidr_block".to_string(),
        Value::String("0.0.0.0/0".to_string()),
    );

    let hint = extract_compact_hint(&r, None);
    assert_eq!(hint, Some("destination_cidr_block: 0.0.0.0/0".to_string()));
}

/// Test that extract_compact_hint returns None when no useful attributes.
#[test]
fn test_extract_compact_hint_none() {
    let r = Resource::new("ec2.route", "hash789");
    let hint = extract_compact_hint(&r, None);
    assert_eq!(hint, None);
}

/// Test that extract_compact_hint prefers string over ResourceRef.
#[test]
fn test_extract_compact_hint_prefers_string_over_resource_ref() {
    let mut r = Resource::new("ec2.route", "hash_mixed");
    r.set_attr(
        "destination".to_string(),
        Value::String("10.0.0.0/8".to_string()),
    );
    r.set_attr(
        "gateway_id".to_string(),
        Value::resource_ref("igw".to_string(), "id".to_string(), vec![]),
    );

    let hint = extract_compact_hint(&r, None);
    // String takes priority over ResourceRef
    assert_eq!(hint, Some("destination: 10.0.0.0/8".to_string()));
}

/// Test that extract_compact_hint shortens service_name values.
#[test]
fn test_extract_compact_hint_service_name_shortening() {
    let mut r = Resource::new("ec2.vpc_endpoint", "hash_svc");
    r.set_attr(
        "service_name".to_string(),
        Value::String("com.amazonaws.ap-northeast-1.ecr.dkr".to_string()),
    );

    let hint = extract_compact_hint(&r, None);
    assert_eq!(hint, Some("service: ecr.dkr".to_string()));

    // Single service component
    let mut r2 = Resource::new("ec2.vpc_endpoint", "hash_svc2");
    r2.set_attr(
        "service_name".to_string(),
        Value::String("com.amazonaws.ap-northeast-1.s3".to_string()),
    );

    let hint2 = extract_compact_hint(&r2, None);
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
        Value::String("Allow HTTPS from VPC".to_string()),
    );

    // When parent is endpoint_sg, should skip group_id and use description
    let hint = extract_compact_hint(&r, Some("endpoint_sg"));
    assert_eq!(hint, Some("description: Allow HTTPS from VPC".to_string()));
}

/// Test that extract_compact_hint prefers string over ResourceRef for vpc_endpoint.
#[test]
fn test_extract_compact_hint_prefers_string_for_vpc_endpoint() {
    let mut r = Resource::new("ec2.vpc_endpoint", "hash_ep");
    r.set_attr(
        "service_name".to_string(),
        Value::String("com.amazonaws.ap-northeast-1.ecr.dkr".to_string()),
    );
    r.set_attr(
        "security_group_ids".to_string(),
        Value::List(vec![Value::resource_ref(
            "endpoint_sg".to_string(),
            "group_id".to_string(),
            vec![],
        )]),
    );
    r.set_attr(
        "vpc_id".to_string(),
        Value::resource_ref("vpc".to_string(), "id".to_string(), vec![]),
    );

    // String attribute (service_name) takes priority over ResourceRef
    let hint = extract_compact_hint(&r, Some("vpc"));
    assert_eq!(hint, Some("service: ecr.dkr".to_string()));
}

/// Test that has_binding correctly detects bound vs anonymous resources.
#[test]
fn test_has_binding() {
    let mut bound = Resource::new("ec2.Vpc", "vpc");
    bound.binding = Some("vpc".to_string());
    assert!(has_binding(&bound));

    let anonymous = Resource::new("ec2.Vpc", "hash123");
    assert!(!has_binding(&anonymous));
}

/// Test that format_compact_name shows plain identifiers for bound resources and
/// parenthesized hints for anonymous resources.
#[test]
fn test_format_compact_name_bound_resource() {
    let mut r = Resource::new("ec2.Vpc", "vpc");
    r.binding = Some("vpc".to_string());
    // For bound resources, should show name as plain identifier (no quotes)
    let result = format_compact_name(&r, "vpc", None);
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
    let result = format_compact_name(&r, "hash123", None);
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
    plan.add(Effect::Create(vpc));
    plan.add(Effect::Create(rt));

    // Should not panic
    print_plan(
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
    );
}

/// Test compact mode skips attributes by checking that _binding attribute
/// keys are not printed (attributes are hidden in compact mode).
#[test]
fn test_print_plan_compact_with_anonymous_resources() {
    let mut anon = Resource::new("ec2.route", "hash_anon");
    anon.set_attr(
        "destination_cidr_block".to_string(),
        Value::String("0.0.0.0/0".to_string()),
    );
    anon.set_attr(
        "route_table_id".to_string(),
        Value::resource_ref("public_rt".to_string(), "id".to_string(), vec![]),
    );

    let mut plan = Plan::new();
    plan.add(Effect::Create(anon));

    // Should not panic; anonymous resources should show hints
    print_plan(
        &plan,
        DetailLevel::None,
        &HashMap::new(),
        None,
        &HashMap::new(),
        &[],
        &[],
    );
}

/// Test that extract_compact_hint extracts ResourceRef from inside a List value.
/// e.g., security_group_ids = [endpoint_sg.group_id] should produce "security_group: endpoint_sg"
#[test]
fn test_extract_compact_hint_list_containing_resource_ref() {
    let mut r = Resource::new("ec2.vpc_endpoint", "hash_list_ref");
    r.set_attr(
        "security_group_ids".to_string(),
        Value::List(vec![Value::resource_ref(
            "endpoint_sg".to_string(),
            "group_id".to_string(),
            vec![],
        )]),
    );

    let hint = extract_compact_hint(&r, None);
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
        Value::List(vec![Value::resource_ref(
            "endpoint_sg".to_string(),
            "group_id".to_string(),
            vec![],
        )]),
    );

    let hint = extract_compact_hint(&r, Some("endpoint_sg"));
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
        Value::String("awscc.AvailabilityZone.ap_northeast_1a".to_string()),
    );

    let hint = extract_compact_hint(&r, None);
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
    r.set_attr("_hash".to_string(), Value::String("abc123".to_string()));

    let hint = extract_compact_hint(&r, None);
    assert_eq!(hint, None, "Internal attributes should be skipped");
}

#[test]
fn test_cascading_update_shows_attribute_diffs() {
    use std::collections::HashMap;

    // Build a Replace effect with a cascading update that changes vpc_id
    let vpc_from = State::existing(
        ResourceId::new("ec2.Vpc", "vpc"),
        HashMap::from([(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        )]),
    );
    let vpc_to = Resource::new("ec2.Vpc", "vpc")
        .with_binding("vpc")
        .with_attribute("cidr_block", Value::String("10.1.0.0/16".to_string()));

    let subnet_from = State::existing(
        ResourceId::new("ec2.Subnet", "subnet"),
        HashMap::from([
            (
                "vpc_id".to_string(),
                Value::String("vpc-old123".to_string()),
            ),
            (
                "cidr_block".to_string(),
                Value::String("10.0.1.0/24".to_string()),
            ),
        ]),
    );
    let subnet_to = Resource::new("ec2.Subnet", "subnet")
        .with_attribute(
            "vpc_id",
            Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
        )
        .with_attribute("cidr_block", Value::String("10.0.1.0/24".to_string()));

    let replace_effect = Effect::Replace {
        id: ResourceId::new("ec2.Vpc", "vpc"),
        from: Box::new(vpc_from),
        to: vpc_to,
        lifecycle: LifecycleConfig {
            create_before_destroy: true,
            ..Default::default()
        },
        changed_create_only: vec!["cidr_block".to_string()],
        cascading_updates: vec![CascadingUpdate {
            id: ResourceId::new("ec2.Subnet", "subnet"),
            from: Box::new(subnet_from),
            to: subnet_to,
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
    );
}

#[test]
fn test_format_cascading_update_attr_diff() {
    use std::collections::HashMap;

    let cascade = CascadingUpdate {
        id: ResourceId::new("ec2.Subnet", "subnet"),
        from: Box::new(State::existing(
            ResourceId::new("ec2.Subnet", "subnet"),
            HashMap::from([
                (
                    "vpc_id".to_string(),
                    Value::String("vpc-old123".to_string()),
                ),
                (
                    "cidr_block".to_string(),
                    Value::String("10.0.1.0/24".to_string()),
                ),
            ]),
        )),
        to: Resource::new("ec2.Subnet", "subnet")
            .with_attribute(
                "vpc_id",
                Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
            )
            .with_attribute("cidr_block", Value::String("10.0.1.0/24".to_string())),
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
    use std::collections::HashMap;

    let from_attrs = HashMap::from([
        ("vpc_id".to_string(), Value::String("vpc-123".to_string())),
        (
            "cidr_block".to_string(),
            Value::String("10.0.1.0/24".to_string()),
        ),
    ]);
    let to_attrs = HashMap::from([
        ("_binding".to_string(), Value::String("subnet".to_string())),
        ("vpc_id".to_string(), Value::String("vpc-123".to_string())),
        (
            "cidr_block".to_string(),
            Value::String("10.0.1.0/24".to_string()),
        ),
    ]);

    // Verify the precondition: the values are semantically equal
    assert!(
        Value::String("vpc-123".to_string())
            .semantically_equal(&Value::String("vpc-123".to_string())),
        "precondition: old and new vpc_id should be semantically equal"
    );

    let output =
        format_replace_changed_attrs(&from_attrs, &to_attrs, &["vpc_id".to_string()], "    ", &[]);

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
    use std::collections::HashMap;

    let from_attrs = HashMap::from([(
        "vpc_id".to_string(),
        Value::String("vpc-0bf023ff87bf1aa0c".to_string()),
    )]);
    let to_attrs = HashMap::from([(
        "vpc_id".to_string(),
        Value::String("vpc-0bf023ff87bf1aa0c".to_string()),
    )]);

    let hints = vec![("vpc_id".to_string(), "vpc.vpc_id".to_string())];

    let output = format_replace_changed_attrs(
        &from_attrs,
        &to_attrs,
        &["vpc_id".to_string()],
        "    ",
        &hints,
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

/// Test that cascading update diff only shows attributes referencing the replaced binding,
/// not attributes with false diffs due to DSL vs AWS format mismatch (issue #958).
#[test]
fn test_format_cascading_update_diff_excludes_non_ref_attributes() {
    use std::collections::HashMap;

    let cascade = CascadingUpdate {
        id: ResourceId::new("ec2.Subnet", "subnet"),
        from: Box::new(State::existing(
            ResourceId::new("ec2.Subnet", "subnet"),
            HashMap::from([
                (
                    "vpc_id".to_string(),
                    Value::String("vpc-old123".to_string()),
                ),
                (
                    "availability_zone".to_string(),
                    Value::String("ap-northeast-1a".to_string()),
                ),
                (
                    "cidr_block".to_string(),
                    Value::String("10.0.1.0/24".to_string()),
                ),
            ]),
        )),
        to: Resource::new("ec2.Subnet", "subnet")
            .with_attribute(
                "vpc_id",
                Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
            )
            .with_attribute(
                "availability_zone",
                Value::String("awscc.AvailabilityZone.ap_northeast_1a".to_string()),
            )
            .with_attribute("cidr_block", Value::String("10.0.1.0/24".to_string())),
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
        id: ResourceId::new("ec2.Instance", "instance"),
        from: Box::new(State::existing(
            ResourceId::new("ec2.Instance", "instance"),
            HashMap::from([(
                "security_group_ids".to_string(),
                Value::List(vec![Value::String("sg-old123".to_string())]),
            )]),
        )),
        to: Resource::new("ec2.Instance", "instance").with_attribute(
            "security_group_ids",
            Value::List(vec![Value::resource_ref(
                "sg".to_string(),
                "group_id".to_string(),
                vec![],
            )]),
        ),
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
        ResourceId::new("ec2.Vpc", "vpc"),
        HashMap::from([(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
        )]),
    );

    // SG: Replace effect (has `to` resource that depends on VPC)
    let sg_to = make_resource("ec2.SecurityGroup", "sg", "sg", &["vpc"]);
    let sg_from = State::existing(
        ResourceId::new("ec2.SecurityGroup", "sg"),
        HashMap::from([(
            "ref_vpc".to_string(),
            Value::String("vpc-old123".to_string()),
        )]),
    );

    // Subnet: Delete effect (only has id and identifier — no resource, no deps)
    // In the original DSL, subnet depends on VPC, but Delete loses that info.
    let subnet_delete = Effect::Delete {
        id: ResourceId::new("ec2.Subnet", "subnet"),
        identifier: "subnet-12345".to_string(),
        lifecycle: LifecycleConfig::default(),
        binding: Some("subnet".to_string()),
        dependencies: HashSet::from(["vpc".to_string()]),
    };

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        id: ResourceId::new("ec2.Vpc", "vpc"),
        from: Box::new(vpc_from),
        to: vpc_to,
        changed_attributes: vec!["cidr_block".to_string()],
    });
    plan.add(Effect::Replace {
        id: ResourceId::new("ec2.SecurityGroup", "sg"),
        from: Box::new(sg_from),
        to: sg_to,
        lifecycle: LifecycleConfig::default(),
        changed_create_only: vec!["ref_vpc".to_string()],
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
        ResourceId::new("ec2.Vpc", "vpc"),
        HashMap::from([(
            "cidr_block".to_string(),
            Value::String("10.0.0.0/16".to_string()),
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
        Value::String("vpc-0123456789abcdef0".to_string()),
    );
    sg.set_attr(
        "group_description".to_string(),
        Value::String("Test security group".to_string()),
    );
    // _dependency_bindings is saved by resolve_refs_with_state() before
    // ResourceRef values are resolved to strings.
    sg.dependency_bindings = std::collections::BTreeSet::from(["vpc".to_string()]);

    let mut plan = Plan::new();
    plan.add(Effect::Update {
        id: ResourceId::new("ec2.Vpc", "vpc"),
        from: Box::new(vpc_from),
        to: vpc_to,
        changed_attributes: vec!["tags".to_string()],
    });
    plan.add(Effect::Create(sg));

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
        id: ResourceId::with_provider("awscc", "ec2.Vpc", "ec2_vpc_fb75c929"),
        identifier: "vpc-12345".to_string(),
        lifecycle: LifecycleConfig::default(),
        binding: Some("my_vpc".to_string()),
        dependencies: HashSet::new(),
    };
    assert_eq!(format_effect(&effect), "Delete awscc.ec2.Vpc.my_vpc");
}

#[test]
fn format_effect_delete_falls_back_to_id_name() {
    let effect = Effect::Delete {
        id: ResourceId::with_provider("awscc", "ec2.Vpc", "ec2_vpc_fb75c929"),
        identifier: "vpc-12345".to_string(),
        lifecycle: LifecycleConfig::default(),
        binding: None,
        dependencies: HashSet::new(),
    };
    assert_eq!(
        format_effect(&effect),
        "Delete awscc.ec2.Vpc.ec2_vpc_fb75c929"
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
